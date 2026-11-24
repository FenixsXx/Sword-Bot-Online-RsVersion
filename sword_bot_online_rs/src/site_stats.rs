//! Le statistiche staff del sito, `/api/site/stats/…` (`cogs/site_stats.py`): sei letture riservate
//! allo staff con la sessione Discord del sito, mai memorizzate dal browser.
//!
//! I dati vengono da [`PlatformData`]; qui ci sono i parametri (con i loro limiti), gli errori e i
//! piccoli adattamenti dei dati per il browser (gli ID Discord sono stringhe, perché superano il più
//! grande intero che JavaScript rappresenta senza perdere cifre).

use std::sync::Arc;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use serde_json::{Value, json};
use tracing::error;

use crate::live_event::python_int;
use crate::platform_data::{DataError, PlatformData};
use crate::site_auth::SiteAuth;
use crate::site_common::{json_error, json_response, plain, query_params};

const DAYS_RANGE: (i64, i64) = (7, 365);
const EVENTS_LIMIT_RANGE: (i64, i64) = (1, 100);
const HISTORY_LIMIT_RANGE: (i64, i64) = (1, 100);
const HISTORY_QUERY_LENGTH: (usize, usize) = (2, 100);

/// Stato condiviso per le route del modulo delle statistiche staff.
/// Contiene riferimenti a Auth e PlatformData per elaborare le richieste.
#[derive(Clone)]
pub struct StatsState {
    pub auth: Arc<SiteAuth>,
    pub data: Arc<PlatformData>,
}

/// Registra le route dell'API statistiche staff.
/// Fornisce endpoint per overview, health, heatmap, accuracy, events e history.
pub fn routes(state: StatsState) -> Router {
    Router::new()
        .route("/api/site/stats/overview", get(overview))
        .route("/api/site/stats/health", get(health))
        .route("/api/site/stats/heatmap", get(heatmap))
        .route("/api/site/stats/accuracy", get(accuracy))
        .route("/api/site/stats/events", get(events))
        .route("/api/site/stats/history", get(history))
        .with_state(state)
}

/// Un parametro non valido: il messaggio che il browser mostra (risposta 400).
struct InvalidParameter(String);

/// Estrae un parametro intero dalla query con validazione e limiti.
///
/// - `query`: mappa dei parametri della query
/// - `name`: nome del parametro da estrarre
/// - `default`: valore di default se il parametro manca o è vuoto
/// - `bounds`: coppia (min, max) per il range accettabile
/// - `label`: nome leggibile per messaggi di errore
///
/// Ritorna `Err(InvalidParameter)` se il valore è fuori range.
fn int_param(query: &std::collections::HashMap<String, String>, name: &str, default: i64, bounds: (i64, i64), label: &str) -> Result<i64, InvalidParameter> {
    let Some(raw) = query.get(name).filter(|raw| !raw.is_empty()) else {
        return Ok(default);
    };
    let value = python_int(raw).ok_or_else(|| InvalidParameter(format!("{label} non valido")))?;
    let (low, high) = bounds;
    if !(low..=high).contains(&value) {
        return Err(InvalidParameter(format!("{label} deve essere compreso tra {low} e {high}")));
    }
    Ok(value)
}

// Converte gli ID Discord in stringhe per compatibilità con JavaScript.
// I numeri interi grandi possono perdere precisione in JS, quindi vengono
// serializzati come stringhe quando non sono null.
fn user_id_text(value: &Value) -> Value {
    if value.is_null() { Value::Null } else { Value::String(value.as_i64().map_or_else(|| value.to_string(), |id| id.to_string())) }
}

// Trasforma i dati dell'evento aggiungendo gli ID utente come stringhe per JS.
fn events_payload(mut data: Value) -> Value {
    if let Some(reports) = data.get_mut("reports").and_then(Value::as_array_mut) {
        for report in reports {
            if let Some(speakers) = report.get_mut("top_speakers").and_then(Value::as_array_mut) {
                for speaker in speakers {
                    let id = user_id_text(&speaker["user_id"]);
                    speaker["user_id"] = id;
                }
            }
        }
    }
    // Come in Python il testo sui limiti precede i reports.
    json!({"limitations": data["limitations"], "reports": data["reports"]})
}

// Costruisce il payload per l'endpoint di accuratezza dello staff.
// Include la definizione metrica e l'elenco degli staff con le loro percentuali.
fn accuracy_payload(rows: &Value) -> Value {
    let rows = rows.as_array().cloned().unwrap_or_default();
    let definition = rows.first().map_or_else(
        || "Percentuale di proposte selezionate che hanno pareggiato il massimo dei voti nel sondaggio community archiviato.".to_string(),
        |row| row["definition"].as_str().unwrap_or_default().to_string(),
    );
    json!({
        "definition": definition,
        "staff": rows.iter().map(|row| json!({
            "user_id": user_id_text(&row["user_id"]),
            "name": row["name"],
            "selected": row["selected"],
            "won": row["won"],
            "accuracy_percent": row["accuracy_percent"],
        })).collect::<Vec<_>>(),
    })
}

// Trasforma il payload storico convertendo gli ID Discord in stringhe.
fn history_payload(mut data: Value) -> Value {
    if let Some(timeline) = data.get_mut("timeline").and_then(Value::as_array_mut) {
        for item in timeline {
            let id = user_id_text(&item["selected_by"]);
            item["selected_by"] = id;
        }
    }
    data
}

// Costruisce il payload per l'endpoint di community records.
// Converte gli ID utente in stringhe e organizza i record per chiave.
fn community_records_payload(rows: &mut [Value]) -> Value {
    Value::Array(rows.iter().map(|row| {
        let mut obj = row.as_object().cloned().unwrap_or_default();
        if let Some(user_id) = obj.get("user_id") {
            obj.insert("user_id".to_string(), user_id_text(user_id));
        }
        Value::Object(obj)
    }).collect())
}

/// Handler principale per l'autorizzazione e la risposta.
/// Autorizza la richiesta tramite SiteAuth, costruisce il payload con PlatformData
/// e traduce gli errori nei rispettivi endpoint Python.
async fn endpoint(state: &StatsState, headers: &HeaderMap, path: &str, build: impl FnOnce(&PlatformData) -> Result<Result<Value, DataError>, InvalidParameter>) -> Response {
    if let Err(response) = state.auth.authorize(headers, false).await {
        return response;
    }
    match build(&state.data) {
        Err(InvalidParameter(message)) => json_error(StatusCode::BAD_REQUEST, &message),
        Ok(Ok(payload)) => json_response(StatusCode::OK, &payload),
        Ok(Err(DataError::Sql(e))) => {
            error!("Statistiche del sito non disponibili ({path}): {e}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Statistiche non disponibili al momento")
        }
        Ok(Err(DataError::Value(message))) => {
            error!("Statistiche del sito non disponibili ({path}): {message}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "Statistiche non disponibili al momento")
        }
        Ok(Err(DataError::Unexpected(message))) => {
            error!("Errore imprevisto nelle statistiche del sito ({path}): {message}");
            plain(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error\n\nServer got itself in trouble")
        }
    }
}

/// Endpoint: overview statistiche staff.
async fn overview(State(state): State<StatsState>, headers: HeaderMap) -> Response {
    endpoint(&state, &headers, "/api/site/stats/overview", |data| Ok(data.overview())).await
}

/// Endpoint: health della community con parametro opzionale `days`.
async fn health(State(state): State<StatsState>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    let query = query_params(raw);
    endpoint(&state, &headers, "/api/site/stats/health", |data| {
        let days = int_param(&query, "days", 30, DAYS_RANGE, "Il periodo in giorni")?;
        Ok(data.community_health(Some(days)))
    })
    .await
}

/// Endpoint: heatmap attività con parametro `days`.
async fn heatmap(State(state): State<StatsState>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    let query = query_params(raw);
    endpoint(&state, &headers, "/api/site/stats/heatmap", |data| {
        let days = int_param(&query, "days", 30, DAYS_RANGE, "Il periodo in giorni")?;
        Ok(data.activity_heatmap(days))
    })
    .await
}

async fn accuracy(State(state): State<StatsState>, headers: HeaderMap) -> Response {
    endpoint(&state, &headers, "/api/site/stats/accuracy", |data| Ok(data.staff_selection_accuracy().map(|rows| accuracy_payload(&rows)))).await
}

async fn events(State(state): State<StatsState>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    let query = query_params(raw);
    endpoint(&state, &headers, "/api/site/stats/events", |data| {
        let limit = int_param(&query, "limit", 30, EVENTS_LIMIT_RANGE, "Il limite")?;
        Ok(data.event_analytics(limit).map(events_payload))
    })
    .await
}

async fn history(State(state): State<StatsState>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    let query = query_params(raw);
    endpoint(&state, &headers, "/api/site/stats/history", |data| {
        let text = query.get("q").map_or("", |text| text.trim());
        let (low, high) = HISTORY_QUERY_LENGTH;
        if !(low..=high).contains(&text.chars().count()) {
            return Err(InvalidParameter(format!("La ricerca deve contenere tra {low} e {high} caratteri")));
        }
        let limit = int_param(&query, "limit", 50, HISTORY_LIMIT_RANGE, "Il limite")?;
        Ok(data.historical_search(text, limit).map(history_payload))
    })
    .await
}

#[cfg(test)]
mod tests;