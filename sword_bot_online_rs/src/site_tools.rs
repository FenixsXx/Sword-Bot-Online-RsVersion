//! Gli strumenti staff del sito, `/api/site/tools/…` (`cogs/site_tools.py`): lo stato per la
//! schermata di controllo, la revisione e la scelta delle proposte, i backup, i report degli eventi
//! live e l'avvio e l'arresto dell'evento live dal sito.
//!
//! Ogni modifica passa dal registro audit (con `source: "site"` nei dettagli) e avvisa i collegamenti
//! live del sito.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::DateTime;
use chrono_tz::Tz;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use tracing::error;

use crate::backups::{BackupService, BackupStatus};
use crate::database::Database;
use crate::hooks::publish_site_change;
use crate::live_event::{delete_report, python_int};
use crate::live_service::{LateLive, StartOutcome};
use crate::notify::AuditEntry;
use crate::overlay::LiveState;
use crate::queries::suggestions_list;
use crate::schedule::isoformat;
use crate::site_auth::{SiteAuth, SiteDirectory};
use crate::site_backend::SiteBackend;
use crate::site_common::{BodyError, json_error, json_response, plain, read_json_body, status_text};
use crate::stats::{AuthorStat, increment_suggestion_authors_stat, revoke_ballot_entry_stats};
use crate::voice::BoxFuture;

const REVIEW_ACTIONS: [&str; 3] = ["review", "approve", "reject"];
const GENERIC_500: &str = "500 Internal Server Error\n\nServer got itself in trouble";

/// I backup del database (`backup_status` e `create_backups`).
pub trait SiteBackups: Send + Sync + 'static {
    fn status(&self) -> Value;
    /// Crea i backup; restituisce i nomi dei file.
    fn create(&self) -> BoxFuture<'_, Result<Vec<String>, String>>;
}

fn status_json(status: &BackupStatus) -> Value {
    json!({
        "root": status.root.display().to_string(),
        "daily_count": status.daily_count,
        "latest_daily": status.latest_daily,
        "weekly_count": status.weekly_count,
        "latest_weekly": status.latest_weekly,
        "cloud_upload_error": status.cloud_upload_error,
    })
}

impl SiteBackups for BackupService {
    fn status(&self) -> Value {
        status_json(&BackupService::status(self))
    }

    fn create(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(async move {
            let files = self.create_backups(None).await.map_err(|e| e.to_string())?;
            Ok(files.iter().filter_map(|path| path.file_name().and_then(|name| name.to_str()).map(str::to_string)).collect())
        })
    }
}

#[derive(Clone)]
pub struct ToolsState {
    pub auth: Arc<SiteAuth>,
    pub db: Database,
    pub backend: Arc<dyn SiteBackend>,
    pub directory: Arc<dyn SiteDirectory>,
    pub live_state: Arc<LiveState>,
    pub live: Arc<LateLive>,
    pub backups: Arc<dyn SiteBackups>,
    pub clock: fn() -> DateTime<Tz>,
}

pub fn routes(state: ToolsState) -> Router {
    Router::new()
        .route("/api/site/tools/state", get(tools_state))
        .route("/api/site/tools/proposals/{suggestion_id}/review", post(review_proposal))
        .route("/api/site/tools/proposals/{suggestion_id}/choose", post(choose_proposal))
        .route("/api/site/tools/proposals/{suggestion_id}/selection", axum::routing::delete(remove_selection))
        .route("/api/site/tools/backups", get(backups).post(backups))
        .route("/api/site/tools/events/{target}", axum::routing::delete(delete_event_report).post(live_event_control))
        .with_state(state)
}

/// Un parametro del percorso fatto di sole cifre (`\d+`): `None` se il percorso non corrisponde
/// (404); `Some(None)` se è troppo grande per il database.
///
/// aiohttp confronta il percorso ancora codificato (`%D9%A2`): le cifre di altri alfabeti non
/// corrispondono, quindi qui contano solo quelle ASCII.
pub(crate) fn digits_param(raw: &str) -> Option<Option<i64>> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(python_int(raw))
}

pub(crate) fn generic_500() -> Response {
    plain(StatusCode::INTERNAL_SERVER_ERROR, GENERIC_500)
}

impl ToolsState {
    fn setting_set(&self, key: &str) -> bool {
        self.db.get_setting(key).ok().flatten().is_some_and(|value| !value.is_empty())
    }
}

// --- stato -------------------------------------------------------------------------------------

async fn tools_state(State(state): State<ToolsState>, headers: HeaderMap) -> Response {
    let principal = match state.auth.authorize(&headers, false).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let session = state.live_state.session();
    let proposals = match suggestions_list(&state.db, None) {
        Ok(rows) => rows,
        Err(e) => {
            error!("Elenco delle proposte per il sito non riuscito: {e}");
            return generic_500();
        }
    };
    let event_reports: Result<Vec<Value>, rusqlite::Error> = state.db.with_conn(|conn| {
        let mut statement = conn.prepare(
            "SELECT id,title,started_at,ended_at,participant_count,peak_participants FROM live_events WHERE ended_at IS NOT NULL ORDER BY id DESC LIMIT 20",
        )?;
        statement
            .query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "title": row.get::<_, String>(1)?,
                    "started_at": row.get::<_, String>(2)?,
                    "ended_at": row.get::<_, Option<String>>(3)?,
                    "participant_count": row.get::<_, i64>(4)?,
                    "peak_participants": row.get::<_, i64>(5)?,
                }))
            })?
            .collect()
    });
    let event_reports = match event_reports {
        Ok(rows) => rows,
        Err(e) => {
            error!("Report degli eventi per il sito non riusciti: {e}");
            return generic_500();
        }
    };
    let voice_channels: Vec<Value> = state
        .auth
        .configured_guild()
        .map(|(guild_id, _)| state.directory.voice_channels(guild_id))
        .unwrap_or_default()
        .into_iter()
        .map(|(id, name)| json!({"id": id.to_string(), "name": name}))
        .collect();
    let mut result = json!({
        "principal": {"user_id": principal.user_id.to_string(), "display_name": principal.display_name, "is_admin": principal.is_admin},
        "proposals": proposals.iter().map(|row| json!({
            "id": row.id,
            "name": row.name,
            "type": row.tipo,
            "link": row.link,
            "status": row.status,
            "review_status": row.review_status,
            "supporters": row.n,
        })).collect::<Vec<_>>(),
        "events": {
            "active": session.is_some(),
            "title": session.as_ref().map_or(String::new(), |session| session.title.clone()),
            "channel_id": session.as_ref().map_or(String::new(), |session| session.channel_id.to_string()),
            "channel_name": session.as_ref().map_or(String::new(), |session| session.channel_name.clone()),
            "reports": event_reports,
        },
        "voice_channels": voice_channels,
    });
    if principal.is_admin {
        result["backups"] = state.backups.status();
    }
    json_response(StatusCode::OK, &result)
}

// --- revisione ---------------------------------------------------------------------------------

async fn review_proposal(State(state): State<ToolsState>, headers: HeaderMap, Path(raw_id): Path<String>, body: Body) -> Response {
    let Some(suggestion_id) = digits_param(&raw_id) else {
        return status_text(StatusCode::NOT_FOUND);
    };
    let principal = match state.auth.authorize(&headers, false).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let payload = match read_json_body(body).await {
        Ok(payload) => payload,
        Err(BodyError::TooLarge) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "Maximum request body size 65536 exceeded"),
        Err(BodyError::Invalid) => return json_error(StatusCode::BAD_REQUEST, "Richiesta non valida"),
    };
    let Value::Object(payload) = payload else {
        return json_error(StatusCode::BAD_REQUEST, "Corpo JSON non valido");
    };
    let Some(action) = payload.get("action").and_then(Value::as_str).filter(|action| REVIEW_ACTIONS.contains(action)) else {
        return json_error(StatusCode::BAD_REQUEST, "Azione di revisione non valida");
    };
    let reason = match payload.get("reason") {
        None => "",
        Some(Value::String(reason)) if reason.chars().count() <= 200 => reason,
        Some(_) => return json_error(StatusCode::BAD_REQUEST, "Motivazione non valida"),
    };
    // Un ID che il database non può rappresentare fa fallire Python con un errore non gestito.
    let Some(suggestion_id) = suggestion_id else {
        return generic_500();
    };
    let outcome = state.backend.apply_review(principal.user_id as i64, suggestion_id, action.to_string(), reason.to_string()).await;
    if !outcome.ok {
        return json_error(StatusCode::from_u16(outcome.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), &outcome.message);
    }
    json_response(StatusCode::OK, &json!({"ok": true, "review_status": outcome.review_status}))
}

// --- ballottaggio ------------------------------------------------------------------------------

/// Cosa è successo nella transazione che sceglie una proposta.
enum Chosen {
    NotFound,
    Conflict(String),
    Picked { name: String, kind: String, week: i64, poll_id: i64, authors: Vec<u64> },
}

fn choose_in_db(db: &Database, suggestion_id: i64, actor: i64, now: DateTime<Tz>) -> Result<Chosen, String> {
    db.transaction(|tx| -> rusqlite::Result<Result<Chosen, String>> {
        let week = crate::database::current_week(tx)?;
        let suggestion: Option<(String, String, String, Option<String>)> = tx
            .query_row("SELECT name, tipo, review_status, status FROM suggestions WHERE id=? AND week=?", params![suggestion_id, week], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .optional()?;
        let poll: Option<i64> = tx.query_row("SELECT id FROM polls WHERE is_community=1 AND active=1", [], |row| row.get(0)).optional()?;
        let (Some((name, kind, review_status, status)), Some(poll_id)) = (suggestion, poll) else {
            return Ok(Ok(Chosen::NotFound));
        };
        if review_status != "approved" {
            return Ok(Ok(Chosen::Conflict("La proposta deve essere approvata prima del ballottaggio".into())));
        }
        if status.as_deref() == Some("picked") {
            return Ok(Ok(Chosen::Conflict("La proposta è già nel ballottaggio".into())));
        }
        let slots_setting = crate::database::setting_or(tx, "slots", "5")?;
        let Some(slots) = python_int(&slots_setting) else {
            return Ok(Err("slot non validi".into()));
        };
        let taken: i64 = tx.query_row("SELECT COUNT(*) FROM options WHERE poll_id=? AND suggestion_id IS NOT NULL", [poll_id], |row| row.get(0))?;
        if taken >= slots {
            return Ok(Ok(Chosen::Conflict(format!("Gli slot del ballottaggio ({slots}) sono pieni"))));
        }
        tx.execute("INSERT INTO options(poll_id,label,suggestion_id) VALUES(?,?,?)", params![poll_id, name, suggestion_id])?;
        tx.execute(
            "UPDATE suggestions SET status='picked',picked_by=?,picked_at=? WHERE id=? AND week=? AND status='pending'",
            params![actor, isoformat(now), suggestion_id, week],
        )?;
        increment_suggestion_authors_stat(tx, suggestion_id, week, AuthorStat::BallotEntries, now)?;
        let authors: Vec<i64> = tx
            .prepare("SELECT DISTINCT user_id FROM proposals WHERE suggestion_id=? AND week=?")?
            .query_map(params![suggestion_id, week], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(Ok(Chosen::Picked { name, kind, week, poll_id, authors: authors.into_iter().filter_map(|id| u64::try_from(id).ok()).collect() }))
    })
    .map_err(|e| e.to_string())?
}

async fn choose_proposal(State(state): State<ToolsState>, headers: HeaderMap, Path(raw_id): Path<String>) -> Response {
    let Some(suggestion_id) = digits_param(&raw_id) else {
        return status_text(StatusCode::NOT_FOUND);
    };
    let principal = match state.auth.authorize(&headers, false).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !state.setting_set("poll_channel") || !state.setting_set("staff_channel") {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Configura prima i canali sondaggi e staff");
    }
    let Some(suggestion_id) = suggestion_id else {
        return generic_500();
    };
    let chosen = match choose_in_db(&state.db, suggestion_id, principal.user_id as i64, (state.clock)()) {
        Ok(chosen) => chosen,
        Err(e) => {
            error!("Scelta dal sito della proposta {suggestion_id} non riuscita: {e}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Impossibile aggiungere la proposta al ballottaggio");
        }
    };
    let (name, kind, week, poll_id, authors) = match chosen {
        Chosen::NotFound => return json_error(StatusCode::NOT_FOUND, "Proposta o sondaggio community non disponibile"),
        Chosen::Conflict(message) => return json_error(StatusCode::CONFLICT, &message),
        Chosen::Picked { name, kind, week, poll_id, authors } => (name, kind, week, poll_id, authors),
    };
    state.backend.audit(
        AuditEntry::new(Some(principal.user_id), "proposal_selected")
            .entity("suggestion", suggestion_id)
            .details(json!({"name": name, "week": week, "source": "site"})),
    );
    publish_site_change("proposals");
    publish_site_change("polls");
    let refreshed = async {
        state.backend.refresh_poll(poll_id).await?;
        state.backend.refresh_dashboard().await
    };
    if let Err(e) = refreshed.await {
        error!("Proposta salvata ma refresh del sondaggio fallito: {e}");
        return json_error(StatusCode::BAD_GATEWAY, "Proposta salvata ma sondaggio Discord non aggiornato");
    }
    state.backend.notify_users(authors, format!("🏆 La tua proposta **{name}** è entrata nel ballottaggio.")).await;
    state.backend.publish_activity(format!("🏆 **{name}** è entrata nel ballottaggio community!")).await;
    state.backend.staff_log(format!("✅ <@{}> ha scelto «{name}» ({kind}) dal sito.", principal.user_id));
    json_response(StatusCode::OK, &json!({"ok": true}))
}

enum Removed {
    NotFound,
    Removed { name: String, poll_id: Option<i64> },
}

fn remove_in_db(db: &Database, suggestion_id: i64, now: DateTime<Tz>) -> rusqlite::Result<Removed> {
    db.transaction(|tx| {
        let week = crate::database::current_week(tx)?;
        let suggestion: Option<String> = tx
            .query_row("SELECT name FROM suggestions WHERE id=? AND week=? AND status='picked'", params![suggestion_id, week], |row| row.get(0))
            .optional()?;
        let Some(name) = suggestion else {
            return Ok(Removed::NotFound);
        };
        let poll_id: Option<i64> = tx.query_row("SELECT poll_id FROM options WHERE suggestion_id=?", [suggestion_id], |row| row.get(0)).optional()?;
        tx.execute("DELETE FROM options WHERE suggestion_id=?", [suggestion_id])?;
        tx.execute("UPDATE suggestions SET status='pending',picked_by=NULL,picked_at=NULL WHERE id=?", [suggestion_id])?;
        revoke_ballot_entry_stats(tx, suggestion_id, week, now)?;
        Ok(Removed::Removed { name, poll_id })
    })
}

async fn remove_selection(State(state): State<ToolsState>, headers: HeaderMap, Path(raw_id): Path<String>) -> Response {
    let Some(suggestion_id) = digits_param(&raw_id) else {
        return status_text(StatusCode::NOT_FOUND);
    };
    let principal = match state.auth.authorize(&headers, false).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !state.setting_set("poll_channel") || !state.setting_set("staff_channel") {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Configura prima i canali sondaggi e staff");
    }
    let Some(suggestion_id) = suggestion_id else {
        return generic_500();
    };
    let removed = match remove_in_db(&state.db, suggestion_id, (state.clock)()) {
        Ok(removed) => removed,
        Err(e) => {
            error!("Rimozione dal ballottaggio dal sito non riuscita: {e}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Impossibile rimuovere la proposta");
        }
    };
    let (name, poll_id) = match removed {
        Removed::NotFound => return json_error(StatusCode::NOT_FOUND, "Proposta non trovata nel ballottaggio"),
        Removed::Removed { name, poll_id } => (name, poll_id),
    };
    // L'audit prima dell'aggiornamento di Discord: la proposta è già stata tolta.
    state.backend.audit(
        AuditEntry::new(Some(principal.user_id), "proposal_unselected").entity("suggestion", suggestion_id).details(json!({"name": name, "source": "site"})),
    );
    publish_site_change("proposals");
    publish_site_change("polls");
    let refreshed = async {
        if let Some(poll_id) = poll_id {
            state.backend.refresh_poll(poll_id).await?;
        }
        state.backend.refresh_dashboard().await
    };
    if let Err(e) = refreshed.await {
        error!("Proposta rimossa ma refresh del sondaggio fallito: {e}");
        return json_error(StatusCode::BAD_GATEWAY, "Proposta rimossa ma sondaggio Discord non aggiornato");
    }
    state.backend.staff_log(format!("❌ <@{}> ha tolto «{name}» dal sondaggio finale dal sito.", principal.user_id));
    json_response(StatusCode::OK, &json!({"ok": true}))
}

// --- backup ------------------------------------------------------------------------------------

async fn backups(State(state): State<ToolsState>, headers: HeaderMap, request_method: axum::http::Method) -> Response {
    let principal = match state.auth.authorize(&headers, true).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if request_method == axum::http::Method::GET {
        return json_response(StatusCode::OK, &state.backups.status());
    }
    let files = match state.backups.create().await {
        Ok(files) => files,
        Err(e) => {
            error!("Backup richiesto dal sito non riuscito: {e}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Backup non riuscito; controlla i log del bot");
        }
    };
    state.backend.audit(AuditEntry::new(Some(principal.user_id), "database_backup_created").entity_type("database").details(json!({"files": files, "source": "site"})));
    json_response(StatusCode::OK, &json!({"ok": true, "files": files, "status": state.backups.status()}))
}

// --- report degli eventi live ------------------------------------------------------------------

async fn delete_event_report(State(state): State<ToolsState>, headers: HeaderMap, Path(target): Path<String>) -> Response {
    let Some(event_id) = digits_param(&target) else {
        return status_text(StatusCode::NOT_FOUND);
    };
    let principal = match state.auth.authorize(&headers, true).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(event_id) = event_id else {
        return generic_500();
    };
    let deleted = match delete_report(&state.db, event_id) {
        Ok(deleted) => deleted,
        Err(e) => {
            error!("Eliminazione report evento {event_id} dal sito non riuscita: {e}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Impossibile eliminare il report");
        }
    };
    if deleted == 0 {
        return json_error(StatusCode::NOT_FOUND, "Report evento non trovato");
    }
    state.backend.audit(AuditEntry::new(Some(principal.user_id), "live_event_report_deleted").entity("live_event", event_id).details(json!({"source": "site"})));
    publish_site_change("events");
    json_response(StatusCode::OK, &json!({"ok": true}))
}

// --- avvio e arresto dell'evento live ----------------------------------------------------------

/// `int(valore)` di Python su un dato JSON.
fn int_of(value: &Value) -> Option<i64> {
    match value {
        Value::String(text) => python_int(text),
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

async fn live_event_control(State(state): State<ToolsState>, headers: HeaderMap, Path(action): Path<String>, body: Body) -> Response {
    if !matches!(action.as_str(), "start" | "stop") {
        return status_text(StatusCode::NOT_FOUND);
    }
    let principal = match state.auth.authorize(&headers, false).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(controller) = state.live.get() else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Gestione eventi live non disponibile");
    };
    if action == "stop" {
        let Some(event_id) = controller.finish("manuale").await else {
            return json_error(StatusCode::CONFLICT, "Non c'è un evento live attivo");
        };
        state.backend.audit(AuditEntry::new(Some(principal.user_id), "live_event_stopped").entity("live_event", event_id).details(json!({"source": "site"})));
        publish_site_change("events");
        // Questa risposta, in Python, non porta `Cache-Control: no-store`.
        let mut response = json_response(StatusCode::OK, &json!({"ok": true, "event_id": event_id}));
        response.headers_mut().remove(header::CACHE_CONTROL);
        return response;
    }
    let payload = match read_json_body(body).await {
        Ok(payload) => payload,
        Err(BodyError::TooLarge) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "Maximum request body size 65536 exceeded"),
        Err(BodyError::Invalid) => return json_error(StatusCode::BAD_REQUEST, "Canale vocale non valido"),
    };
    let Some(channel_id) = payload.as_object().and_then(|payload| payload.get("channel_id")).and_then(int_of) else {
        return json_error(StatusCode::BAD_REQUEST, "Canale vocale non valido");
    };
    let guild = state.auth.configured_guild().map(|(guild_id, _)| guild_id);
    let voice_channel = guild.zip(u64::try_from(channel_id).ok()).filter(|(guild_id, channel)| state.directory.voice_channels(*guild_id).iter().any(|(id, _)| id == channel));
    let Some((guild_id, channel_id)) = voice_channel else {
        return json_error(StatusCode::BAD_REQUEST, "Seleziona un canale vocale del server");
    };
    let title = match payload.get("title") {
        None => "🎮 Game Night".to_string(),
        Some(Value::String(title)) => crate::site_reports::py_strip(title).to_string(),
        Some(_) => return json_error(StatusCode::BAD_REQUEST, "Titolo evento non valido"),
    };
    if !(1..=100).contains(&title.chars().count()) {
        return json_error(StatusCode::BAD_REQUEST, "Titolo evento non valido");
    }
    if state.live_state.session().is_some() {
        return json_error(StatusCode::CONFLICT, "È già attivo un evento live");
    }
    let outcome = controller.start_titled(guild_id, channel_id, title.clone()).await;
    if !matches!(outcome, StartOutcome::Started { .. }) {
        return json_error(StatusCode::BAD_GATEWAY, "Il bot non è riuscito ad avviare l'evento");
    }
    state.backend.audit(
        AuditEntry::new(Some(principal.user_id), "live_event_started_from_site").entity_type("live_event").details(json!({"channel_id": channel_id, "title": title})),
    );
    publish_site_change("events");
    json_response(StatusCode::OK, &json!({"ok": true}))
}

#[cfg(test)]
mod tests;
