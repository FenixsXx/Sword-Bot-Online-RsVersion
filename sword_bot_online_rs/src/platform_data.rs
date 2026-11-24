//! Questo modulo fornisce letture del database per le statistiche del sito web.
//!
//! Le funzioni qui disponibili servono per visualizzare sul sito informazioni utili:
//! - **Salute della community**: un punteggio che misura quanto è attiva la community
//! - **Mappa dell'attività**: grafico che mostra quando gli utenti sono più attivi
//! - **Ricerca nello storico**: trova proposte selezionate nel passato
//! - **Precisione dello staff**: verifica quanto lo staff sceglie proposte vincenti
//! - **Report eventi live**: statistiche sugli eventi vocali passati
//! - **Panoramica generale**: riepilogo dello stato attuale del bot
//!
//! **Caching**: ogni lettura viene memorizzata nella cache per 15 secondi per velocizzare le richieste successive.
//! La cache viene svuotata quando i dati cambiano (chiamata a `clear_read_cache`).
//!
//! **Gestione errori in Rust vs Python**:
//! - `DataError::Sql`: errori del database SQLite, mostrati come 500 con messaggio dettagliato
//! - `DataError::Value`: dati o parametri non validi, mostrati come 500 con messaggio
//! - `DataError::Unexpected`: errori imprevisti, mostrati come 500 generico (senza dettagli sensibili)
//!
//! Nota: i nomi delle funzioni in italiano riflettono il nome originale in inglese per facilitare
//! la collaborazione con il codice Python correlato in `cogs/platform_data.py`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Timelike, TimeZone};
use chrono_tz::Tz;
use rusqlite::params;
use serde_json::{Map, Value, json};

use crate::config::TZ;
use crate::database::Database;
use crate::formatting::casefold;
use crate::live_event::python_int;
use crate::schedule::isoformat;
use crate::site_auth::SiteDirectory;

/// Quanto resta in cache una lettura.
pub const READ_CACHE_TTL: Duration = Duration::from_secs(15);

const ACCURACY_DEFINITION: &str = "Percentuale di proposte selezionate che hanno pareggiato il massimo dei voti nel sondaggio community archiviato.";

/// Risultati di errore per le letture del database.
///
/// Per chi non è programmatore: questo tipo di dato rappresenta gli errori che possono
/// verificarsi quando si leggono dati dal database. L'enum (un tipo speciale che può
/// assumere diverse forme) ha tre varianti:
///
/// - `Sql`: problemi tecnici con il database (es. tabella mancante, query errata)
/// - `Value`: dati errati o parametri non validi (es. data non riconosciuta)
/// - `Unexpected`: errori imprevedibili che dovrebbero essere segnalati all'amministratore
#[derive(Debug)]
pub enum DataError {
    /// Un errore del database SQLite (corrisponde a `sqlite3.Error` in Python).
    /// Questo succede quando c'è un problema tecnico con il database:
    /// - tabella mancante o con struttura errata
    /// - query SQL malformata
    /// - database corrotto o chiuso
    Sql(rusqlite::Error),
    /// Errore di validazione dei dati (corrisponde a `ValueError`, `KeyError`, `TypeError` in Python).
    /// Questo succede quando i dati non sono nel formato atteso:
    /// - stringa formato data non riconosciuta
    /// - numero mancante o non convertibile
    /// - chiave esterna non trovata
    Value(String),
    /// Errore imprevisto (corrisponde a errori come `AttributeError` in Python).
    /// Questo succede per condizioni che non dovrebbero mai verificarsi:
    /// - struttura dati imprevista
    /// - bug nel codice logico
    /// - incoerenza dei dati
    /// In Python, questi errori non vengono gestiti esplicitamente e causano un errore 500 generico.
    Unexpected(String),
}

impl From<rusqlite::Error> for DataError {
    fn from(error: rusqlite::Error) -> Self {
        DataError::Sql(error)
    }
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataError::Sql(e) => write!(f, "{e}"),
            DataError::Value(message) | DataError::Unexpected(message) => f.write_str(message),
        }
    }
}

/// Risultato di una lettura del database: `Result<Value, DataError>`.
/// `Value` contiene i dati JSON restituiti, `DataError` rappresenta un errore.
type DataResult = Result<Value, DataError>;

/// `round(x, 1)` di Python: arrotonda sul valore esatto del numero.
/// Converte il numero in stringa con 1 cifra decimale, poi torna indietro a f64.
/// Usa `unwrap_or` come fallback se la conversione fallisce.
pub(crate) fn round1(value: f64) -> f64 {
    format!("{value:.1}").parse().unwrap_or(value)
}

/// `round(x)` di Python: all'intero pari più vicino.
fn round_int(value: f64) -> i64 {
    value.round_ties_even() as i64
}

/// `int(valore)` di Python su un dato JSON.
fn python_int_of(value: &Value) -> Option<i64> {
    match value {
        Value::String(text) => python_int(text),
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

/// La verità di un valore JSON, come `if valore:` in Python.
// Valori falsi: null, false, 0, stringhe vuote, array oggetti vuoti.
// Tutti gli altri valori sono truthy.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|f| f != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

// Converte un timestamp Unix in DateTime locale.
// Usa il fuso orario configurato (TZ) e restituisce un valore predefinito se il timestamp è invalido.
fn local(timestamp: i64) -> DateTime<Tz> {
    TZ.timestamp_opt(timestamp, 0).single().unwrap_or_else(|| TZ.timestamp_opt(0, 0).unwrap())
}

// Calcola l'intervallo di settimane per un periodo di giorni.
/// Restituisce una coppia (prima_settimana, ultima_settimana) con il conteggio delle settimane
/// nel periodo specificato, considerando un range di settimane indietro.
/// Calcola l'intervallo di settimane per un periodo di giorni.
/// Restituisce una coppia (prima_settimana, ultima_settimana) con il conteggio delle settimane
/// nel periodo specificato, considerando un range di settimane indietro.
pub fn period_weeks(current_week: i64, days: i64, periods_back: i64) -> (i64, i64) {
    let span = ((days + 6).div_euclid(7)).max(1);
    let last = current_week - span * periods_back;
    (last - span + 1, last)
}

/// Struttura che fornisce l'accesso ai dati della piattaforma per le operazioni sul sito.
/// Include il database, la directory del sito e un sistema di cache per le letture frequenti.
pub struct PlatformData {
    db: Database,
    directory: Arc<dyn SiteDirectory>,
    /// `GUILD_ID` dell'ambiente, se `guild_id` non è nelle impostazioni.
    fallback_guild_id: Option<u64>,
    now: fn() -> i64,
    monotonic: fn() -> Instant,
    cache: Mutex<HashMap<String, (Instant, Value)>>,
}

impl PlatformData {
    // Crea una nuova instance di PlatformData con i dati forniti.
    // Il fallback_guild_id è usato quando guild_id non è nelle impostazioni del bot.
    pub fn new(db: Database, directory: Arc<dyn SiteDirectory>, fallback_guild_id: Option<u64>) -> Self {
        Self { db, directory, fallback_guild_id, now: || chrono::Utc::now().timestamp(), monotonic: Instant::now, cache: Mutex::new(HashMap::new()) }
    }

    #[cfg(test)]
    pub(crate) fn with_clocks(mut self, now: fn() -> i64, monotonic: fn() -> Instant) -> Self {
        self.now = now;
        self.monotonic = monotonic;
        self
    }

    /// Svuota la cache delle letture a causa di aggiornamenti dati.
    // Chiamato quando il bot riceve aggiornamenti che invalidano i dati in cache.
    pub fn clear_read_cache(&self) {
        self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
    }

    // Una lettura con cache: stesso nome e stessi argomenti, stesso risultato per 15 secondi.
    // Usa chiave unica per ogni combinazione di parametri.
    // Rimuove le voci scadute prima di ogni accesso alla cache.
    fn cached(&self, key: String, compute: impl FnOnce() -> DataResult) -> DataResult {
        let now = (self.monotonic)();
        {
            let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match cache.get(&key) {
                Some((expires, value)) if *expires > now => return Ok(value.clone()),
                Some(_) => {
                    cache.remove(&key);
                }
                None => {}
            }
        }
        let result = compute()?;
        let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.retain(|_, (expires, _)| *expires > now);
        cache.insert(key, (now + READ_CACHE_TTL, result.clone()));
        Ok(result)
    }

    /// Il nome di un membro dalla cache del server, o `Utente <id>`.
    pub(crate) fn member_name(&self, user_id: i64) -> String {
        let setting = self.db.get_setting("guild_id").ok().flatten().filter(|value| !value.is_empty());
        let guild_id = match setting {
            Some(value) => python_int(&value).and_then(|id| u64::try_from(id).ok()),
            None => self.fallback_guild_id,
        };
        let name = guild_id.zip(u64::try_from(user_id).ok()).and_then(|(guild, user)| self.directory.cached_member_name(guild, user));
        match name {
            Some(name) => name.chars().take(100).collect(),
            None => format!("Utente {user_id}"),
        }
    }

    // --- salute della community ---------------------------------------------------------------

    /// Il punteggio di salute della community negli ultimi `days` giorni (30 se non indicato) e nel
    /// periodo precedente.
    pub fn community_health(&self, days: Option<i64>) -> DataResult {
        let key = format!("community_health|{}", days.map_or(String::new(), |d| d.to_string()));
        self.cached(key, || {
            let days = days.unwrap_or(30);
            let now = (self.now)() + 1;
            let window = days * 86400;
            let current_week = self.db.current_week()?;
            let current = self.health_metrics(now - window, now, period_weeks(current_week, days, 0), days)?;
            let previous = self.health_metrics(now - window * 2, now - window, period_weeks(current_week, days, 1), days)?;
            let score = current["score"].as_i64().unwrap_or(0);
            let previous_score = previous["score"].as_i64().unwrap_or(0);
            Ok(json!({
                "period_days": days,
                "score": score,
                "previous_score": previous_score,
                "trend": score - previous_score,
                "metrics": current,
                "methodology": "Punteggio euristico 0-100: utenti attivi (35), continuità dei giorni (25), varietà di 4 azioni (20), supporti e voti aggregati (15), eventi conclusi (10). I componenti sono normalizzati da 105 a 100. È un indicatore descrittivo, non una misura scientifica.",
            }))
        })
    }

    // Calcola le metriche di salute della community per un periodo specifico.
    // Raccoglie azioni della community, utenti attivi, timestamp e eventi conclusi,
    // poi calcola un punteggio combinando partecipazione, continuità, varietà, coinvolgimento e eventi.
    fn health_metrics(&self, start: i64, end: i64, weeks: (i64, i64), days: i64) -> Result<Value, DataError> {
        let (start_text, end_text) = (isoformat(local(start)), isoformat(local(end)));
        let (counts, active_users, timestamps, events) = self.db.with_conn(|conn| -> rusqlite::Result<_> {
            let mut statement =
                conn.prepare("SELECT action, COUNT(*) AS count FROM community_activity WHERE occurred_at>=? AND occurred_at<? GROUP BY action")?;
            let counts: HashMap<String, i64> = statement.query_map(params![start, end], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<Result<_, _>>()?;
            let active_users: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT user_id) FROM (SELECT user_id FROM weekly_activity WHERE week BETWEEN ? AND ? UNION SELECT p.user_id FROM live_event_participants p JOIN live_events e ON e.id=p.event_id WHERE e.started_at>=? AND e.started_at<?)",
                params![weeks.0, weeks.1, start_text, end_text],
                |row| row.get(0),
            )?;
            let mut statement = conn.prepare("SELECT occurred_at FROM community_activity WHERE occurred_at>=? AND occurred_at<?")?;
            let timestamps: Vec<i64> = statement.query_map(params![start, end], |row| row.get(0))?.collect::<Result<_, _>>()?;
            let events: i64 = conn.query_row(
                "SELECT COUNT(*) FROM live_events WHERE ended_at IS NOT NULL AND started_at>=? AND started_at<?",
                params![start_text, end_text],
                |row| row.get(0),
            )?;
            Ok((counts, active_users, timestamps, events))
        })?;
        let count = |action: &str| counts.get(action).copied().unwrap_or(0);
        let (proposals, supports, votes, attendance) = (count("proposal"), count("support"), count("vote"), count("event_join"));
        let local_activity: Vec<DateTime<Tz>> = timestamps.iter().map(|timestamp| local(*timestamp)).collect();
        let active_days = local_activity.iter().map(|item| item.date_naive()).collect::<BTreeSet<_>>().len() as i64;
        let active_weeks = local_activity.iter().map(|item| (item.iso_week().year(), item.iso_week().week())).collect::<BTreeSet<_>>().len() as i64;
        let diversity = [proposals, supports, votes, attendance].iter().filter(|count| **count > 0).count() as i64;
        let participation = 35.min((active_users as f64 * 3.5) as i64);
        let continuity = 25.min((active_days as f64 / days.max(1) as f64 * 100.0) as i64);
        let variety = diversity * 5;
        let engagement = 15.min(((supports + votes) as f64 / 5.0) as i64);
        let event_points = 10.min(events * 2);
        let total = participation + continuity + variety + engagement + event_points;
        Ok(json!({
            "score": round_int(total as f64 * 100.0 / 105.0),
            "active_users": active_users,
            "active_days": active_days,
            "active_weeks": active_weeks,
            "proposals": proposals,
            "supports": supports,
            "votes": votes,
            "event_attendance": attendance,
            "events": events,
            "components": {
                "partecipazione": participation,
                "continuità": continuity,
                "varietà": variety,
                "coinvolgimento": engagement,
                "eventi": event_points,
            },
        }))
    }

    // --- mappa dell'attività -------------------------------------------------------------------

    /// Quante azioni per giorno della settimana e ora (ora italiana) negli ultimi `days` giorni.
    pub fn activity_heatmap(&self, days: i64) -> DataResult {
        self.cached(format!("activity_heatmap|{days}"), || {
            let since = (self.now)() - days * 86400;
            let records: Vec<(String, i64)> = self.db.with_conn(|conn| -> rusqlite::Result<_> {
                let mut statement = conn.prepare("SELECT action, occurred_at FROM community_activity WHERE occurred_at>=?")?;
                statement.query_map([since], |row| Ok((row.get(0)?, row.get(1)?)))?.collect()
            })?;
            let mut matrix: BTreeMap<(u32, u32), i64> = BTreeMap::new();
            let mut actions: Map<String, Value> = Map::new();
            for (action, timestamp) in records {
                let time = local(timestamp);
                *matrix.entry((time.weekday().num_days_from_monday(), time.hour())).or_insert(0) += 1;
                let total = actions.get(&action).and_then(Value::as_i64).unwrap_or(0) + 1;
                actions.insert(action, json!(total));
            }
            Ok(json!({
                "period_days": days,
                "timezone": "Europe/Rome",
                "weekday_order": ["Lun", "Mar", "Mer", "Gio", "Ven", "Sab", "Dom"],
                "cells": matrix.iter().map(|((weekday, hour), count)| json!({"weekday": weekday, "hour": hour, "count": count})).collect::<Vec<_>>(),
                "actions": actions,
                "limitations": "I voti sono conteggiati senza utente né opzione per preservarne l'anonimato. I dati orari sono disponibili solo da quando è attiva la registrazione degli eventi; non sono ricostruibili dagli aggregati settimanali precedenti.",
            }))
        })
    }

    // --- ricerca nello storico -----------------------------------------------------------------

    /// Le apparizioni di una proposta nello storico (proposte selezionate e proposte ancora attive).
    pub fn historical_search(&self, query: &str, limit: i64) -> DataResult {
        self.cached(format!("historical_search|{query:?}|{limit}"), || {
            let query: String = query.trim().chars().take(100).collect();
            if query.chars().count() < 2 {
                return Err(DataError::Value("La ricerca deve contenere almeno 2 caratteri.".into()));
            }
            let escaped = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
            let pattern = format!("%{escaped}%");
            // Ogni voce ha la sua chiave (ID della proposta); l'ordine di inserimento conta.
            let mut order: Vec<i64> = Vec::new();
            let mut items: HashMap<i64, Map<String, Value>> = HashMap::new();
            let (picked, current) = self.db.with_conn(|conn| -> rusqlite::Result<_> {
                let mut statement = conn.prepare(
                    "SELECT original_suggestion_id, week, name, tipo, picked_by, picked_at FROM picked_suggestions WHERE name LIKE ? ESCAPE '\\' OR norm_name LIKE ? ESCAPE '\\' ORDER BY week DESC LIMIT ?",
                )?;
                let picked: Vec<(i64, i64, String, String, Option<i64>, Option<String>)> = statement
                    .query_map(params![pattern, pattern, limit], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?
                    .collect::<Result<_, _>>()?;
                let mut won_counts = Vec::new();
                for (id, week, ..) in &picked {
                    let won: i64 = conn.query_row("SELECT COUNT(*) FROM user_winning_proposals WHERE suggestion_id=? AND week=?", params![id, week], |row| row.get(0))?;
                    won_counts.push(won);
                }
                let mut statement = conn.prepare(
                    "SELECT s.id, s.week, s.name, s.tipo, s.status, s.picked_by, s.picked_at FROM suggestions s WHERE s.name LIKE ? ESCAPE '\\' OR s.norm_name LIKE ? ESCAPE '\\' ORDER BY s.week DESC LIMIT ?",
                )?;
                let current: Vec<(i64, i64, String, String, Option<String>, Option<i64>, Option<String>)> = statement
                    .query_map(params![pattern, pattern, limit], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))?
                    .collect::<Result<_, _>>()?;
                Ok((picked.into_iter().zip(won_counts).collect::<Vec<_>>(), current))
            })?;
            for ((id, week, name, kind, picked_by, picked_at), won) in picked {
                if !items.contains_key(&id) {
                    order.push(id);
                }
                items.insert(
                    id,
                    json!({"name": name, "type": kind, "week": week, "selected_by": picked_by, "selected_at": picked_at, "won": won > 0}).as_object().cloned().unwrap_or_default(),
                );
            }
            for (id, week, name, kind, status, picked_by, picked_at) in current {
                if items.contains_key(&id) {
                    continue;
                }
                order.push(id);
                items.insert(
                    id,
                    json!({"name": name, "type": kind, "week": week, "selected_by": picked_by, "selected_at": picked_at, "won": false, "status": status}).as_object().cloned().unwrap_or_default(),
                );
            }
            let mut ordered: Vec<Map<String, Value>> = order.into_iter().filter_map(|id| items.remove(&id)).collect();
            ordered.sort_by(|a, b| b["week"].as_i64().cmp(&a["week"].as_i64()));
            ordered.truncate(usize::try_from(limit.max(0)).unwrap_or(0));
            let ballot_entries = ordered
                .iter()
                .filter(|item| item.get("status").and_then(Value::as_str) == Some("picked") || !item["selected_by"].is_null() || !item["selected_at"].is_null())
                .count();
            let wins = ordered.iter().filter(|item| item["won"] == json!(true)).count();
            Ok(json!({
                "query": query,
                "appearances": ordered.len(),
                "ballot_entries": ballot_entries,
                "wins": wins,
                "timeline": ordered,
                "limitations": "Le apparizioni conteggiano le proposte selezionate conservate e quelle ancora attive; le proposte non selezionate delle settimane già chiuse non sono archiviate come record interrogabili.",
            }))
        })
    }

    // --- precisione dello staff ----------------------------------------------------------------

    /// Per ogni persona dello staff, quante proposte ha selezionato e quante hanno pareggiato il
    /// massimo dei voti nel sondaggio community archiviato.
    pub fn staff_selection_accuracy(&self) -> DataResult {
        self.cached("staff_selection_accuracy|".to_string(), || {
            let rows: Vec<Option<String>> = self.db.with_conn(|conn| -> rusqlite::Result<_> {
                let mut statement = conn.prepare("SELECT week, closed_at, data FROM history ORDER BY id DESC LIMIT ?")?;
                statement.query_map([200], |row| row.get::<_, Option<String>>(2))?.collect()
            })?;
            let mut totals: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
            for data in rows {
                let Some(data) = data else { continue };
                let Ok(snapshot) = serde_json::from_str::<Value>(&data) else { continue };
                let Value::Object(snapshot) = snapshot else {
                    return Err(DataError::Unexpected("l'istantanea non è un oggetto".into()));
                };
                let (top_labels, top_ids) = top_options(snapshot.get("polls"))?;
                let suggestions = match snapshot.get("suggestions") {
                    None => Vec::new(),
                    Some(Value::Array(items)) => items.clone(),
                    Some(_) => return Err(DataError::Value("suggestions non è un elenco".into())),
                };
                for suggestion in &suggestions {
                    let Value::Object(suggestion) = suggestion else {
                        return Err(DataError::Unexpected("una proposta non è un oggetto".into()));
                    };
                    let staff = suggestion.get("picked_by").filter(|value| !value.is_null());
                    if suggestion.get("status").and_then(Value::as_str) != Some("picked") {
                        continue;
                    }
                    let Some(staff) = staff else { continue };
                    let staff_id = python_int_of(staff).ok_or_else(|| DataError::Value("picked_by non valido".into()))?;
                    let metric = totals.entry(staff_id).or_insert((0, 0));
                    metric.0 += 1;
                    let won = match suggestion.get("id").filter(|value| !value.is_null()) {
                        Some(id) => top_ids.iter().any(|candidate| candidate == id),
                        None => {
                            let name = match suggestion.get("name") {
                                None => String::new(),
                                Some(Value::String(name)) => name.clone(),
                                Some(_) => return Err(DataError::Unexpected("il nome non è un testo".into())),
                            };
                            top_labels.contains(&casefold(&name))
                        }
                    };
                    if won {
                        metric.1 += 1;
                    }
                }
            }
            let mut sorted: Vec<(i64, (i64, i64))> = totals.into_iter().collect();
            sorted.sort_by_key(|(user_id, (selected, _))| (-*selected, *user_id));
            Ok(Value::Array(
                sorted
                    .into_iter()
                    .map(|(user_id, (selected, won))| {
                        json!({
                            "user_id": user_id,
                            "name": self.member_name(user_id),
                            "selected": selected,
                            "won": won,
                            "accuracy_percent": round1(won as f64 * 100.0 / selected as f64),
                            "definition": ACCURACY_DEFINITION,
                        })
                    })
                    .collect(),
            ))
        })
    }

    // --- report degli eventi live --------------------------------------------------------------

    /// Gli ultimi `limit` eventi live conclusi, con partecipazione e chi ha parlato di più.
    pub fn event_analytics(&self, limit: i64) -> DataResult {
        self.cached(format!("event_analytics|{limit}"), || {
            type EventRow = (i64, String, String, String, i64, i64, String);
            let events: Vec<EventRow> = self.db.with_conn(|conn| -> rusqlite::Result<_> {
                let mut statement = conn.prepare(
                    "SELECT id, title, started_at, ended_at, peak_participants, participant_count, end_reason FROM live_events WHERE ended_at IS NOT NULL ORDER BY id DESC LIMIT ?",
                )?;
                statement.query_map([limit], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))?.collect()
            })?;
            let mut reports = Vec::new();
            for (id, title, started_at, ended_at, peak, count, reason) in events {
                let participants: Vec<(i64, i64, f64)> = self.db.with_conn(|conn| -> rusqlite::Result<_> {
                    let mut statement = conn.prepare(
                        "SELECT user_id, participation_seconds, speaking_seconds FROM live_event_participants WHERE event_id=? ORDER BY speaking_seconds DESC",
                    )?;
                    statement.query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?.collect()
                })?;
                let started = DateTime::parse_from_rfc3339(&started_at).map_err(|e| DataError::Value(format!("istante non valido: {e}")))?;
                let ended = DateTime::parse_from_rfc3339(&ended_at).map_err(|e| DataError::Value(format!("istante non valido: {e}")))?;
                let micros = ended.signed_duration_since(started).num_microseconds().unwrap_or(i64::MAX);
                let duration = ((micros as f64 / 1e6) as i64).max(0);
                let total_presence: i64 = participants.iter().map(|(_, presence, _)| presence).sum();
                let total_speaking: f64 = participants.iter().map(|(_, _, speaking)| speaking).sum();
                let top_speakers: Vec<Value> = participants
                    .iter()
                    .take(5)
                    .filter(|(_, _, speaking)| *speaking > 0.0)
                    .map(|(user_id, _, speaking)| {
                        json!({
                            "user_id": user_id,
                            "name": self.member_name(*user_id),
                            "speaking_seconds_estimated": round1(*speaking),
                            // Senza parlato totale Python dà l'intero 0, non 0.0.
                            "share_percent": if total_speaking != 0.0 { json!(round1(speaking * 100.0 / total_speaking)) } else { json!(0) },
                        })
                    })
                    .collect();
                reports.push(json!({
                    "id": id,
                    "title": title,
                    "started_at": started_at,
                    "duration_seconds": duration,
                    "peak_participants": peak,
                    "participant_count": count,
                    "average_presence_seconds": if participants.is_empty() { 0 } else { (total_presence as f64 / participants.len() as f64) as i64 },
                    // Con nessun partecipante la somma è l'intero 0 (non 0.0).
                    "speaking_seconds_estimated": if participants.is_empty() { json!(0) } else { json!(round1(total_speaking)) },
                    "top_speakers": top_speakers,
                    "end_reason": reason,
                }));
            }
            Ok(json!({
                "reports": reports,
                "limitations": "Durate di parlato stimate da pacchetti vocali; possono includere sovrapposizioni e non sono una trascrizione.",
            }))
        })
    }

    // --- panoramica ----------------------------------------------------------------------------

    pub fn overview(&self) -> DataResult {
        self.cached("overview|".to_string(), || {
            let week = self.db.current_week()?;
            let counts = self.db.with_conn(|conn| -> rusqlite::Result<_> {
                let pending: i64 = conn.query_row("SELECT COUNT(*) FROM suggestions WHERE week=? AND status='pending' AND review_status='approved'", [week], |row| row.get(0))?;
                let selected: i64 = conn.query_row("SELECT COUNT(*) FROM suggestions WHERE week=? AND status='picked'", [week], |row| row.get(0))?;
                let completed: i64 = conn.query_row("SELECT COUNT(*) FROM live_events WHERE ended_at IS NOT NULL", [], |row| row.get(0))?;
                let active: i64 = conn.query_row("SELECT COUNT(*) FROM live_events WHERE ended_at IS NULL", [], |row| row.get(0))?;
                let audit: i64 = conn.query_row("SELECT COUNT(*) FROM staff_audit", [], |row| row.get(0))?;
                Ok((pending, selected, completed, active, audit))
            })?;
            Ok(json!({
                "current_week": week,
                "pending_proposals": counts.0,
                "selected_proposals": counts.1,
                "completed_events": counts.2,
                "active_events": counts.3,
                "audit_entries": counts.4,
                "community_health": self.community_health(None)?,
            }))
        })
    }

    /// I record della community, in ordine di chiave (li userà l'API pubblica).
    #[allow(dead_code)]
    pub fn records(&self) -> DataResult {
        let rows: Vec<Value> = self.db.with_conn(|conn| -> rusqlite::Result<_> {
            let mut statement = conn.prepare("SELECT record_key, metric, label, week, user_id FROM community_records ORDER BY record_key")?;
            statement
                .query_map([], |row| {
                    Ok(json!({
                        "key": row.get::<_, String>(0)?,
                        "metric": row.get::<_, i64>(1)?,
                        "label": row.get::<_, String>(2)?,
                        "week": row.get::<_, i64>(3)?,
                        "user_id": row.get::<_, Option<i64>>(4)?,
                    }))
                })?
                .collect()
        })?;
        Ok(Value::Array(rows))
    }
}

/// Le etichette (in minuscolo «casefold») e gli ID delle opzioni più votate dei sondaggi community.
fn top_options(polls: Option<&Value>) -> Result<(HashSet<String>, Vec<Value>), DataError> {
    let polls = match polls {
        None => return Ok((HashSet::new(), Vec::new())),
        Some(Value::Array(polls)) => polls,
        Some(_) => return Err(DataError::Value("polls non è un elenco".into())),
    };
    let mut labels = HashSet::new();
    let mut ids: Vec<Value> = Vec::new();
    for poll in polls {
        let Value::Object(poll) = poll else {
            return Err(DataError::Unexpected("un sondaggio non è un oggetto".into()));
        };
        if !poll.get("is_community").is_some_and(truthy) {
            continue;
        }
        let options = match poll.get("options") {
            None => Vec::new(),
            Some(Value::Array(options)) => options.clone(),
            Some(_) => return Err(DataError::Value("options non è un elenco".into())),
        };
        let mut votes_of = Vec::new();
        for option in &options {
            let Value::Object(option) = option else {
                return Err(DataError::Unexpected("un'opzione non è un oggetto".into()));
            };
            let votes = match option.get("votes") {
                None => 0.0,
                Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
                Some(Value::Bool(flag)) => f64::from(u8::from(*flag)),
                Some(_) => return Err(DataError::Value("voti non numerici".into())),
            };
            votes_of.push((option, votes));
        }
        // `max(..., default=0)`: con voti negativi il massimo è negativo, e vale comunque.
        let highest = votes_of.iter().map(|(_, votes)| *votes).reduce(f64::max).unwrap_or(0.0);
        if highest == 0.0 {
            continue;
        }
        for (option, votes) in &votes_of {
            if *votes != highest {
                continue;
            }
            let label = match option.get("label") {
                None => String::new(),
                Some(Value::String(label)) => label.clone(),
                Some(_) => return Err(DataError::Unexpected("l'etichetta non è un testo".into())),
            };
            labels.insert(casefold(&label));
            if let Some(id) = option.get("suggestion_id").filter(|id| !id.is_null()) {
                ids.push(id.clone());
            }
        }
    }
    Ok((labels, ids))
}
