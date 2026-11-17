//! L'API pubblica `/api/v1/…` (`cogs/public_api.py`) e la pagina pubblica `/community`.
//!
//! L'API serve a chi legge i dati della community da fuori (un altro bot, un sito): classifica,
//! stagioni, albo d'oro, eventi, record e profilo di una persona. Richiede `PUBLIC_API_TOKEN` nell'
//! intestazione `Authorization: Bearer …`; senza un token valido (almeno 32 caratteri, senza spazi ai
//! bordi) risponde 503. La pagina `/community` è aperta a tutti.
//!
//! Differenze da Python:
//! * la pagina `/community` è il modello incorporato nel programma, non il file letto a ogni richiesta;
//! * l'ora degli eventi è letta come `datetime.fromisoformat` per le forme che il bot scrive
//!   (`AAAA-MM-GG` con orario e fuso facoltativi, separatore `T` o spazio); le altre danno 500;
//! * `NaN` e `Infinity` nei dati dell'albo d'oro non sono accettati.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use chrono::{DateTime, NaiveDate, NaiveDateTime};
use chrono_tz::Tz;
use rusqlite::OptionalExtension;
use serde_json::{Map, Value, json};
use tracing::error;

use crate::database::Database;
use crate::live_event::python_int;
use crate::platform_data::{PlatformData, round1};
use crate::pyiso::{parse_date, parse_time};
use crate::seasonal::{Season, completed_season_champions, season_champion_titles, season_for_date, season_leaderboard_rows, season_points_for_user};
use crate::site_admin::{IntArg, int_arg_text};
use crate::site_common::{NO_STORE, constant_time_equals, json_response, plain, python_json_ascii, query_params};
use crate::site_reports::py_strip;
use crate::site_tools::generic_500;
use crate::stats::{UserStats, contributor_leaderboard_rows, contributor_level, contributor_points};

const COMMUNITY_HTML: &str = include_str!("../templates/community_site.html");
const DATA_PLACEHOLDER: &str = "__SWORD_PUBLIC_DATA__";
const COMMUNITY_CSP: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; img-src 'self' https://cdn.discordapp.com https://media.discordapp.net; base-uri 'none'; frame-ancestors 'none'";

#[derive(Clone)]
pub struct PublicApiState {
    pub db: Database,
    pub data: Arc<PlatformData>,
    /// Il token valido, o vuoto se l'API non è abilitata.
    token: Arc<str>,
    pub clock: fn() -> DateTime<Tz>,
}

impl PublicApiState {
    /// `raw_token` è `PUBLIC_API_TOKEN`: se è troppo corto o ha spazi ai bordi l'API resta spenta.
    pub fn new(db: Database, data: Arc<PlatformData>, raw_token: &str, clock: fn() -> DateTime<Tz>) -> Self {
        let valid = raw_token.is_empty() || (raw_token.chars().count() >= 32 && py_strip(raw_token) == raw_token);
        if !valid {
            error!("API non abilitata: PUBLIC_API_TOKEN deve contenere almeno 32 caratteri senza spazi esterni.");
        }
        Self { db, data, token: if valid { raw_token.into() } else { "".into() }, clock }
    }
}

pub fn routes(state: PublicApiState) -> Router {
    Router::new()
        .route("/api/v1/leaderboard", get(leaderboard))
        .route("/api/v1/season", get(season))
        .route("/api/v1/seasons", get(seasons))
        .route("/api/v1/hall-of-fame", get(hall_of_fame))
        .route("/api/v1/events", get(events))
        .route("/api/v1/community-records", get(community_records))
        .route("/api/v1/profile/{user_id}", get(profile))
        .route("/community", get(community_site))
        .with_state(state)
}

type Reply = Result<Response, Response>;

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    response
}

/// `authenticate`: il token deve essere configurato e coincidere con quello dell'intestazione.
fn authenticate(state: &PublicApiState, headers: &HeaderMap) -> Result<(), Response> {
    if state.token.is_empty() {
        return Err(no_store(plain(StatusCode::SERVICE_UNAVAILABLE, "API non configurata")));
    }
    let supplied = headers.get(header::AUTHORIZATION).map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned()).unwrap_or_default();
    if constant_time_equals(&supplied, &format!("Bearer {}", state.token)) {
        return Ok(());
    }
    let mut response = no_store(plain(StatusCode::UNAUTHORIZED, "Autenticazione richiesta"));
    response.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    Err(response)
}

/// `_query_limit`: `limit` da 1 a 100 (25 se manca); un testo che non è un intero dà 400.
fn query_limit(raw_query: Option<String>) -> Result<usize, Response> {
    let params = query_params(raw_query);
    let Some(text) = params.get("limit") else {
        return Ok(25);
    };
    match int_arg_text(text) {
        IntArg::Int(value) => Ok(value.clamp(1, 100) as usize),
        IntArg::Huge => Ok(if text.trim_start().starts_with('-') { 1 } else { 100 }),
        IntArg::Invalid => Err(plain(StatusCode::BAD_REQUEST, "Parametro limit non valido")),
    }
}

fn db_failure<T>(what: &str, result: Result<T, impl std::fmt::Display>) -> Result<T, Response> {
    result.map_err(|e| {
        error!("{what}: {e}");
        generic_500()
    })
}

fn json_ok(value: &Value) -> Reply {
    Ok(json_response(StatusCode::OK, value))
}

// --- le letture --------------------------------------------------------------------------------

fn season_json(season: &Season) -> Value {
    json!({"key": season.key, "label": season.label, "starts_at": season.starts_at, "ends_at": season.ends_at})
}

fn today(state: &PublicApiState) -> NaiveDate {
    (state.clock)().date_naive()
}

fn champions_json(state: &PublicApiState, today: NaiveDate, limit: usize) -> Result<Vec<Value>, Response> {
    let champions = db_failure("Campioni delle stagioni", state.db.with_conn(|conn| completed_season_champions(conn, today, limit)))?;
    Ok(champions
        .iter()
        .map(|row| json!({"season_key": row.season_key, "label": row.label, "user_id": row.user_id, "points": row.points, "name": state.data.member_name(row.user_id)}))
        .collect())
}

/// `datetime.fromisoformat` per le forme che il bot scrive: i microsecondi dall'epoca (con il fuso
/// applicato) e se l'istante ha un fuso.
fn parse_iso_datetime(text: &str) -> Option<(i128, bool)> {
    let bytes = text.as_bytes();
    // Dove finisce la data: `AAAA-MM-GG`, `AAAAMMGG` o le date con la settimana.
    let date_length = match (bytes.get(4), bytes.get(5)) {
        (Some(b'-'), Some(b'W')) => if bytes.get(8) == Some(&b'-') { 10 } else { 8 },
        (Some(b'-'), _) => 10,
        (Some(b'W'), _) => if bytes.get(7).is_some_and(u8::is_ascii_digit) { 8 } else { 7 },
        _ => 8,
    };
    let date_text = text.get(..date_length.min(text.len()))?;
    let date = parse_date(date_text)?;
    let (time, offset) = if text.len() == date_length {
        (None, None)
    } else {
        // Un solo carattere separa data e ora (qualsiasi, come in Python).
        let rest = text.get(date_length..)?;
        let mut chars = rest.chars();
        chars.next()?;
        let time = parse_time(chars.as_str())?;
        (Some(time), time.offset_micros)
    };
    let naive: NaiveDateTime = match time {
        Some(time) => date.and_hms_micro_opt(time.hour, time.minute, time.second, time.microsecond)?,
        None => date.and_hms_opt(0, 0, 0)?,
    };
    let micros = i128::from(naive.and_utc().timestamp_micros()) - i128::from(offset.unwrap_or(0));
    Some((micros, offset.is_some()))
}

/// `_event_summaries`: gli ultimi eventi conclusi con durata, presenza e parlato.
fn event_summaries(state: &PublicApiState, limit: usize) -> Result<Vec<Value>, Response> {
    type EventRow = (i64, String, String, String, i64, i64, String);
    let rows: Vec<EventRow> = db_failure(
        "Eventi live",
        state.db.with_conn(|conn| -> rusqlite::Result<_> {
            conn.prepare(
                "SELECT e.id, e.title, e.started_at, e.ended_at, e.peak_participants, e.participant_count, e.end_reason
                 FROM live_events e WHERE e.ended_at IS NOT NULL ORDER BY e.id DESC LIMIT ?",
            )?
            .query_map([limit as i64], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))?
            .collect()
        }),
    )?;
    let mut result = Vec::new();
    for (id, title, started_at, ended_at, peak, count, reason) in rows {
        // Un'ora che Python non legge, o un'ora con fuso confrontata con una senza, fa fallire la richiesta.
        let (Some((started, started_aware)), Some((ended, ended_aware))) = (parse_iso_datetime(&started_at), parse_iso_datetime(&ended_at)) else {
            error!("Evento {id}: ora non valida.");
            return Err(generic_500());
        };
        if started_aware != ended_aware {
            error!("Evento {id}: ore con e senza fuso.");
            return Err(generic_500());
        }
        let duration = ((ended - started) / 1_000_000).max(0) as i64;
        let (participants, presence, speaking): (i64, i64, Option<f64>) = db_failure(
            "Partecipanti all'evento",
            state.db.with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(participation_seconds), 0), SUM(speaking_seconds) FROM live_event_participants WHERE event_id=?",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
            }),
        )?;
        result.push(json!({
            "id": id,
            "title": title,
            "started_at": started_at,
            "duration_seconds": duration,
            "participants": count,
            "peak_participants": peak,
            "end_reason": reason,
            "average_presence_seconds": if participants > 0 { presence / participants } else { 0 },
            // Senza partecipanti la somma è l'intero 0 (non 0.0).
            "estimated_speaking_seconds": match speaking {
                Some(total) if participants > 0 => json!(round1(total)),
                _ => json!(0),
            },
        }));
    }
    Ok(result)
}

/// Gli elementi su cui Python itera con `for x in valore`; `None` se il valore non si può percorrere.
fn iterate(value: &Value) -> Option<Vec<Value>> {
    match value {
        Value::Array(items) => Some(items.clone()),
        Value::Object(map) => Some(map.keys().map(|key| Value::String(key.clone())).collect()),
        Value::String(text) => Some(text.chars().map(|c| Value::String(c.to_string())).collect()),
        _ => None,
    }
}

/// I voti di un'opzione come numero (`votes > 0` e il confronto); `None` dove Python solleva `TypeError`.
fn votes_of(option: &Value) -> Option<f64> {
    match option.as_object()?.get("votes") {
        None => Some(0.0),
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::Bool(flag)) => Some(f64::from(u8::from(*flag))),
        Some(_) => None,
    }
}

/// Le etichette più votate di ogni sondaggio; `None` dove Python solleva un'eccezione.
fn winners_of(data: &Map<String, Value>) -> Option<Vec<Value>> {
    let polls = match data.get("polls") {
        None => Vec::new(),
        Some(polls) => iterate(polls)?,
    };
    let mut winners = Vec::new();
    for poll in &polls {
        let poll = poll.as_object()?;
        let options = match poll.get("options") {
            None => Vec::new(),
            Some(options) => iterate(options)?,
        };
        for option in &options {
            let votes = votes_of(option)?;
            if votes <= 0.0 {
                continue;
            }
            let mut top = f64::NEG_INFINITY;
            for item in &options {
                top = top.max(votes_of(item)?);
            }
            if votes == top {
                winners.push(option.as_object()?.get("label")?.clone());
            }
        }
    }
    Some(winners)
}

fn sql_value(value: rusqlite::types::Value) -> Value {
    match value {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(number) => json!(number),
        rusqlite::types::Value::Real(number) => json!(number),
        rusqlite::types::Value::Text(text) => json!(text),
        rusqlite::types::Value::Blob(_) => Value::Null,
    }
}

/// `_history_payload`: le ultime settimane chiuse con partecipanti e opzioni vincenti.
fn history_payload(state: &PublicApiState, limit: usize) -> Result<Vec<Value>, Response> {
    use rusqlite::types::Value as Sql;
    type HistoryRow = (Sql, Sql, Sql);
    let rows: Vec<HistoryRow> = db_failure(
        "Storico",
        state.db.with_conn(|conn| -> rusqlite::Result<_> {
            conn.prepare("SELECT week, closed_at, data FROM history ORDER BY id DESC LIMIT ?")?
                .query_map([limit as i64], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect()
        }),
    )?;
    let mut result = Vec::new();
    for (week, closed_at, data) in rows {
        let Sql::Text(text) = data else { continue };
        let Ok(Value::Object(data)) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(winners) = winners_of(&data) else {
            error!("Riga di storico con dati non validi.");
            return Err(generic_500());
        };
        result.push(json!({
            "week": sql_value(week),
            "closed_at": sql_value(closed_at),
            "participants": data.get("participants").cloned().unwrap_or(Value::Null),
            "winners": winners,
        }));
    }
    Ok(result)
}

async fn leaderboard(State(state): State<PublicApiState>, headers: HeaderMap, RawQuery(query): RawQuery) -> Response {
    leaderboard_inner(&state, &headers, query).unwrap_or_else(|response| response)
}

fn leaderboard_inner(state: &PublicApiState, headers: &HeaderMap, query: Option<String>) -> Reply {
    authenticate(state, headers)?;
    let limit = query_limit(query)?;
    let rows = db_failure("Classifica", state.db.with_conn(|conn| contributor_leaderboard_rows(conn, limit)))?;
    json_ok(&Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    "user_id": row.user_id,
                    "name": state.data.member_name(row.user_id),
                    "points": row.points,
                    "level": row.level,
                    "wins": row.winning_proposals,
                    "supports_received": row.supports_received,
                })
            })
            .collect(),
    ))
}

async fn season(State(state): State<PublicApiState>, headers: HeaderMap, RawQuery(query): RawQuery) -> Response {
    season_inner(&state, &headers, query).unwrap_or_else(|response| response)
}

fn season_inner(state: &PublicApiState, headers: &HeaderMap, query: Option<String>) -> Reply {
    authenticate(state, headers)?;
    let current = season_for_date(today(state));
    let limit = query_limit(query)?;
    let rows = db_failure("Classifica della stagione", state.db.with_conn(|conn| season_leaderboard_rows(conn, &current.key, limit as i64)))?;
    let mut result = season_json(&current);
    result["leaderboard"] = Value::Array(
        rows.iter().map(|row| json!({"user_id": row.user_id, "name": state.data.member_name(row.user_id), "points": row.points, "activities": row.activities})).collect(),
    );
    json_ok(&result)
}

async fn seasons(State(state): State<PublicApiState>, headers: HeaderMap, RawQuery(query): RawQuery) -> Response {
    seasons_inner(&state, &headers, query).unwrap_or_else(|response| response)
}

fn seasons_inner(state: &PublicApiState, headers: &HeaderMap, query: Option<String>) -> Reply {
    authenticate(state, headers)?;
    let today = today(state);
    let current = season_for_date(today);
    let champions = champions_json(state, today, query_limit(query)?)?;
    json_ok(&json!({"current": season_json(&current), "champions": champions}))
}

async fn hall_of_fame(State(state): State<PublicApiState>, headers: HeaderMap, RawQuery(query): RawQuery) -> Response {
    hall_of_fame_inner(&state, &headers, query).unwrap_or_else(|response| response)
}

fn hall_of_fame_inner(state: &PublicApiState, headers: &HeaderMap, query: Option<String>) -> Reply {
    authenticate(state, headers)?;
    json_ok(&Value::Array(history_payload(state, query_limit(query)?)?))
}

async fn events(State(state): State<PublicApiState>, headers: HeaderMap, RawQuery(query): RawQuery) -> Response {
    events_inner(&state, &headers, query).unwrap_or_else(|response| response)
}

fn events_inner(state: &PublicApiState, headers: &HeaderMap, query: Option<String>) -> Reply {
    authenticate(state, headers)?;
    json_ok(&Value::Array(event_summaries(state, query_limit(query)?)?))
}

async fn community_records(State(state): State<PublicApiState>, headers: HeaderMap, RawQuery(query): RawQuery) -> Response {
    community_records_inner(&state, &headers, query).unwrap_or_else(|response| response)
}

fn community_records_inner(state: &PublicApiState, headers: &HeaderMap, query: Option<String>) -> Reply {
    authenticate(state, headers)?;
    let limit = query_limit(query)?;
    let records = db_failure("Record della community", state.data.records())?;
    let rows: Vec<Value> = records.as_array().cloned().unwrap_or_default().into_iter().take(limit).collect();
    json_ok(&Value::Array(rows))
}

async fn profile(State(state): State<PublicApiState>, headers: HeaderMap, Path(raw_id): Path<String>) -> Response {
    profile_inner(&state, &headers, &raw_id).unwrap_or_else(|response| response)
}

fn profile_inner(state: &PublicApiState, headers: &HeaderMap, raw_id: &str) -> Reply {
    authenticate(state, headers)?;
    let user_id = match python_int(raw_id) {
        Some(id) if id > 0 => id,
        _ => return Err(plain(StatusCode::BAD_REQUEST, "ID utente non valido")),
    };
    let today = today(state);
    let (stats, streak) = db_failure(
        "Profilo",
        state.db.with_conn(|conn| -> rusqlite::Result<_> {
            let stats = conn
                .query_row(
                    "SELECT proposals_submitted, supports_given, supports_received, ballot_entries, winning_proposals FROM user_stats WHERE user_id=?",
                    [user_id],
                    |row| {
                        Ok(UserStats {
                            proposals_submitted: row.get(0)?,
                            supports_given: row.get(1)?,
                            supports_received: row.get(2)?,
                            ballot_entries: row.get(3)?,
                            winning_proposals: row.get(4)?,
                        })
                    },
                )
                .optional()?
                .unwrap_or(UserStats { proposals_submitted: 0, supports_given: 0, supports_received: 0, ballot_entries: 0, winning_proposals: 0 });
            let streak: (i64, i64) = conn
                .query_row("SELECT current_streak, best_streak FROM user_streaks WHERE user_id=?", [user_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?
                .unwrap_or((0, 0));
            Ok((stats, streak))
        }),
    )?;
    let points = contributor_points(&stats);
    let season = season_for_date(today);
    let (season_points, titles) = db_failure(
        "Stagione del profilo",
        state.db.with_conn(|conn| -> rusqlite::Result<_> { Ok((season_points_for_user(conn, &season.key, user_id)?, season_champion_titles(conn, user_id, today, 3)?)) }),
    )?;
    json_ok(&json!({
        "user_id": user_id,
        "name": state.data.member_name(user_id),
        "points": points,
        "level": contributor_level(points),
        "current_streak": streak.0,
        "best_streak": streak.1,
        "proposals_submitted": stats.proposals_submitted,
        "supports_received": stats.supports_received,
        "ballot_entries": stats.ballot_entries,
        "winning_proposals": stats.winning_proposals,
        "season": {"label": season.label, "points": season_points},
        "seasonal_titles": titles,
    }))
}

// --- la pagina pubblica ------------------------------------------------------------------------

/// `_public_site_payload`: i dati incorporati nella pagina.
fn public_site_payload(state: &PublicApiState) -> Result<Value, Response> {
    let today = today(state);
    let season = season_for_date(today);
    let lifetime = db_failure("Classifica", state.db.with_conn(|conn| contributor_leaderboard_rows(conn, 10)))?;
    let seasonal = db_failure("Classifica della stagione", state.db.with_conn(|conn| season_leaderboard_rows(conn, &season.key, 10)))?;
    Ok(json!({
        "season": season.label,
        "lifetime": lifetime.iter().map(|row| json!({
            "user_id": row.user_id,
            "name": state.data.member_name(row.user_id),
            "points": row.points,
            "level": row.level,
            "wins": row.winning_proposals,
        })).collect::<Vec<_>>(),
        "seasonal": seasonal.iter().map(|row| json!({"user_id": row.user_id, "name": state.data.member_name(row.user_id), "points": row.points})).collect::<Vec<_>>(),
        "season_champions": champions_json(state, today, 20)?,
        "hall_of_fame": history_payload(state, 10)?,
        "events": event_summaries(state, 10)?,
    }))
}

/// `_safe_script_json`: JSON adatto a stare dentro un `<script>`.
fn safe_script_json(payload: &Value) -> String {
    python_json_ascii(payload).replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026")
}

async fn community_site(State(state): State<PublicApiState>) -> Response {
    community_site_inner(&state).unwrap_or_else(|response| response)
}

fn community_site_inner(state: &PublicApiState) -> Reply {
    let rendered = COMMUNITY_HTML.replace(DATA_PLACEHOLDER, &safe_script_json(&public_site_payload(state)?));
    let mut response = Response::new(axum::body::Body::from(rendered));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(COMMUNITY_CSP));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    Ok(response)
}

#[cfg(test)]
mod tests;
