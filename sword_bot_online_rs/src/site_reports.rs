//! I report degli eventi del sito, `/api/site/reports` (`cogs/site_reports.py`).
//!
//! I report vivono in una tabella di Supabase (REST); il bot li inoltra, li valida e, su richiesta,
//! li pubblica come embed nel canale dei report eventi, ricordando in `site_report_notifications` che
//! lo ha già fatto. Ogni modifica passa dal registro audit e avvisa i collegamenti live del sito.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};
use tracing::{error, warn};

use crate::cards::EmbedSpec;
use crate::database::Database;
use crate::discord_api::{ApiError, ChannelKind};
use crate::hooks::publish_site_change;
use crate::live_event::python_int;
use crate::notify::AuditEntry;
use crate::pyiso::{parse_date, parse_time};
use crate::schedule::isoformat;
use crate::site_auth::{SitePrincipal, SiteAuth};
use crate::site_backend::SiteBackend;
use crate::site_common::{json_error, json_response, plain, query_params, read_json_body, BodyError};
use crate::voice::BoxFuture;

pub const REPORT_CATEGORIES: [&str; 6] = ["Minigiochi", "Anime", "Film", "Quiz", "Disegno", "Musica"];
pub const REPORT_DURATIONS: [i64; 11] = [15, 30, 45, 60, 75, 90, 105, 120, 150, 180, 240];
pub const REPORT_COLUMNS: &str = "id,name,category,event_date,event_time,duration,attendees,new_members,new_member_names,responsible,notes,clip_recorded,clip_registrar,flop,created_at";
const REPORT_COLOR: u32 = 0xE67E22;

// --- Supabase ----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum StoreReply {
    /// La risposta (JSON, di norma un elenco; `[]` se il corpo era vuoto).
    Rows(Value),
    /// Supabase ha risposto con un errore HTTP (400 o più).
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// Rete, timeout o risposta che non è JSON.
    Unreachable,
}

pub struct StoreCall<'a> {
    pub method: &'a str,
    pub params: Vec<(&'a str, String)>,
    pub payload: Option<&'a Value>,
    pub prefer: Option<&'a str>,
}

/// La tabella dei report (`/rest/v1/reports`).
pub trait ReportStore: Send + Sync + 'static {
    fn request<'a>(&'a self, call: StoreCall<'a>) -> BoxFuture<'a, Result<StoreReply, StoreError>>;
}

/// Supabase vero, con 10 secondi per richiesta.
pub struct SupabaseStore {
    client: reqwest::Client,
    url: String,
    key: String,
}

impl SupabaseStore {
    pub fn new(url: &str, key: &str) -> Self {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default();
        Self { client, url: url.trim_end_matches('/').to_string(), key: key.to_string() }
    }
}

impl ReportStore for SupabaseStore {
    fn request<'a>(&'a self, call: StoreCall<'a>) -> BoxFuture<'a, Result<StoreReply, StoreError>> {
        Box::pin(async move {
            let endpoint = format!("{}/rest/v1/reports", self.url);
            let url = url::Url::parse_with_params(&endpoint, call.params.iter().map(|(key, value)| (*key, value.as_str()))).map_err(|_| StoreError::Unreachable)?;
            let method = reqwest::Method::from_bytes(call.method.as_bytes()).map_err(|_| StoreError::Unreachable)?;
            let mut request = self
                .client
                .request(method, url)
                .header("apikey", &self.key)
                .header("Authorization", format!("Bearer {}", self.key))
                .header("Content-Type", "application/json");
            if let Some(prefer) = call.prefer {
                request = request.header("Prefer", prefer);
            }
            if let Some(payload) = call.payload {
                request = request.body(payload.to_string());
            }
            let response = request.send().await.map_err(|_| StoreError::Unreachable)?;
            let status = response.status().as_u16();
            let body = response.text().await.map_err(|_| StoreError::Unreachable)?;
            if status >= 400 {
                error!("Richiesta report Supabase {} fallita (HTTP {status}): {}", call.method, body.chars().take(500).collect::<String>());
                return Ok(StoreReply::Failed);
            }
            if body.is_empty() {
                return Ok(StoreReply::Rows(json!([])));
            }
            serde_json::from_str(&body).map(StoreReply::Rows).map_err(|_| StoreError::Unreachable)
        })
    }
}

// --- validazione -------------------------------------------------------------------------------

/// Perché un corpo non è un report valido.
/// Enumera i motivi per cui un corpo JSON non è un report valido.
#[derive(Debug, PartialEq, Eq)]
pub enum Invalid {
    /// Il messaggio mostrato all'utente (risposta 400).
    Message(String),
    /// In Python un errore non gestito (una categoria che non è nemmeno un testo semplice).
    Crash,
}

fn invalid(message: impl Into<String>) -> Invalid {
    Invalid::Message(message.into())
}

/// `str.strip()` di Python.
/// Rimuove gli spazi vuoti e i caratteri di controllo dai lati del testo.
pub fn py_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// Converte un valore JSON in intero Python, ritornando `None` se non è un numero intero.
fn is_python_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) if !number.is_f64() => number.as_i64(),
        _ => None,
    }
}

/// `_validate_report`: i valori da salvare, nell'ordine in cui Python li produce.
/// Convalida il corpo JSON di un report e restituisce i valori normalizzati, oppure un errore `Invalid`.
pub fn validate_report(payload: &Value) -> Result<Map<String, Value>, Invalid> {
    let Value::Object(payload) = payload else {
        return Err(invalid("Corpo JSON non valido"));
    };
    let text_fields = [("name", 120usize), ("new_member_names", 1000), ("responsible", 120), ("notes", 3000), ("clip_registrar", 120)];
    let mut texts: Vec<(&str, String)> = Vec::new();
    for (field, limit) in text_fields {
        let stripped = match payload.get(field) {
            None => "",
            Some(Value::String(text)) => py_strip(text),
            Some(_) => return Err(invalid(format!("Campo {field} non valido o troppo lungo"))),
        };
        if stripped.chars().count() > limit {
            return Err(invalid(format!("Campo {field} non valido o troppo lungo")));
        }
        texts.push((field, stripped.to_string()));
    }
    let text = |field: &str| texts.iter().find(|(name, _)| *name == field).map(|(_, value)| value.as_str()).unwrap_or_default();
    if text("name").is_empty() || text("responsible").is_empty() {
        return Err(invalid("Nome evento e responsabile sono obbligatori"));
    }
    let category = match payload.get("category") {
        Some(Value::String(category)) if REPORT_CATEGORIES.contains(&category.as_str()) => category.clone(),
        Some(Value::Array(_) | Value::Object(_)) => return Err(Invalid::Crash),
        _ => return Err(invalid("Categoria non valida")),
    };
    let date_text = payload.get("event_date").and_then(Value::as_str);
    let time_text = payload.get("event_time").and_then(Value::as_str);
    let (Some(date_text), Some(time_text)) = (date_text.filter(|text| parse_date(text).is_some()), time_text) else {
        return Err(invalid("Data o ora non valida"));
    };
    let Some(parsed_time) = parse_time(time_text) else {
        return Err(invalid("Data o ora non valida"));
    };
    if parsed_time.second != 0 || parsed_time.microsecond != 0 || parsed_time.offset_micros.is_some() {
        return Err(invalid("L'ora deve essere nel formato HH:MM"));
    }
    let mut numbers = [0i64; 3];
    for (slot, field) in numbers.iter_mut().zip(["duration", "attendees", "new_members"]) {
        *slot = payload.get(field).and_then(is_python_int).ok_or_else(|| invalid(format!("Campo {field} non valido")))?;
    }
    let [duration, attendees, new_members] = numbers;
    if !REPORT_DURATIONS.contains(&duration) {
        return Err(invalid("Durata non valida"));
    }
    if attendees < 0 || !(0..=attendees).contains(&new_members) {
        return Err(invalid("Numero di presenze o nuovi membri non valido"));
    }
    let mut flags = [false; 2];
    for (slot, field) in flags.iter_mut().zip(["clip_recorded", "flop"]) {
        *slot = payload.get(field).and_then(Value::as_bool).ok_or_else(|| invalid(format!("Campo {field} non valido")))?;
    }
    let [clip_recorded, flop] = flags;
    if clip_recorded && text("clip_registrar").is_empty() {
        return Err(invalid("Indica chi ha registrato la clip"));
    }

    let mut values = Map::new();
    for (field, value) in &texts {
        values.insert((*field).to_string(), json!(value));
    }
    values.insert("category".into(), json!(category));
    values.insert("event_date".into(), json!(date_text));
    values.insert("event_time".into(), json!(format!("{:02}:{:02}:{:02}", parsed_time.hour, parsed_time.minute, parsed_time.second)));
    values.insert("duration".into(), json!(duration));
    values.insert("attendees".into(), json!(attendees));
    values.insert("new_members".into(), json!(new_members));
    values.insert("clip_recorded".into(), json!(clip_recorded));
    values.insert("flop".into(), json!(flop));
    Ok(values)
}

// --- embed del report --------------------------------------------------------------------------

/// `repr(float)` di Python: le cifre più brevi che riconvertono allo stesso numero, in notazione
/// decimale tra `1e-4` e `1e16` (esclusi) e in esponente altrove.
/// Restituisce la rappresentazione a precisione variabile di un numero in virgola mobile.
pub(crate) fn py_float_repr(float: f64) -> String {
    if float == 0.0 {
        return if float.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    let scientific = format!("{float:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let sign = if mantissa.starts_with('-') { "-" } else { "" };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let decimal_point = exponent + 1;
    if -4 < decimal_point && decimal_point <= 16 {
        if decimal_point <= 0 {
            format!("{sign}0.{}{digits}", "0".repeat(decimal_point.unsigned_abs() as usize))
        } else if digits.len() as i32 <= decimal_point {
            format!("{sign}{digits}{}.0", "0".repeat(decimal_point as usize - digits.len()))
        } else {
            format!("{sign}{}.{}", &digits[..decimal_point as usize], &digits[decimal_point as usize..])
        }
    } else {
        let mut mantissa = digits[..1].to_string();
        if digits.len() > 1 {
            mantissa.push('.');
            mantissa.push_str(&digits[1..]);
        }
        let exponent = decimal_point - 1;
        format!("{sign}{mantissa}e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs())
    }
}

/// `repr()` di Python per i valori JSON.
/// Restituisce la rappresentazione letterale Python di un valore (es. `True`, `None`, numeri, stringhe).
fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(flag) => if *flag { "True" } else { "False" }.to_string(),
        Value::Number(number) => match number.as_f64().filter(|_| number.is_f64()) {
            Some(float) => py_float_repr(float),
            None => number.to_string(),
        },
        Value::String(text) => {
            let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
            let mut repr = String::from(quote);
            for c in text.chars() {
                match c {
                    '\\' => repr.push_str("\\\\"),
                    '\n' => repr.push_str("\\n"),
                    '\r' => repr.push_str("\\r"),
                    '\t' => repr.push_str("\\t"),
                    c if c == quote => {
                        repr.push('\\');
                        repr.push(c);
                    }
                    c => repr.push(c),
                }
            }
            repr.push(quote);
            repr
        }
        Value::Array(items) => format!("[{}]", items.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => format!("{{{}}}", map.iter().map(|(key, value)| format!("{}: {}", py_repr(&Value::String(key.clone())), py_repr(value))).collect::<Vec<_>>().join(", ")),
    }
}

/// `str(valore)` di Python.
/// Restituisce la rappresentazione stringa di un valore: per `String` restituisce il testo,
/// per gli altri tipi usa `py_repr` (es. `True`, `None`, numeri).
pub(crate) fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => py_repr(other),
    }
}

/// `str(valore)` di Python (verità).
/// Restituisce `true` per tutti i valori eccetto `null`, `false`, `0` e stringhe vuote.
/// `truthy(value: Option<&Value>) -> bool` di Python.
/// Restituisce `true` per tutti i valori eccetto `null`, `false`, `0` e stringhe vuote.
/// Questo riflette il comportamento di `if valore:` in Python.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|float| float != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// Trunca un testo al limite di caratteri specificato.
/// Trunca un testo al limite di caratteri specificato.
/// Restituisce il testo truncato se eccede il limite, altrimenti il testo originale.
fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Aggiunge un campo a un embed Discord.
/// Trunca il valore se eccede i 1024 caratteri, altrimenti usa il testo completo.
fn add_field(embed: EmbedSpec, name: &str, value: &Value) -> EmbedSpec {
    let text = py_strip(&py_str(value)).to_string();
    let text = if text.is_empty() { "—".to_string() } else { text };
    embed.field(name, truncate(&text, 1024), true)
}

/// `report.get(chiave)` con `None` se manca.
/// Restituisce il valore associato a una chiave in un report, oppure `Value::Null` se manca.
/// Restituisce il valore associato a una chiave in un report, oppure `Value::Null` se manca.
fn field_of<'a>(report: &'a Value, key: &str) -> &'a Value {
    report.get(key).unwrap_or(&Value::Null)
}

/// L'embed di un report (`_report_embed`).
/// Costruisce un embed Discord a partire dai dati di un report, includendo campi come categoria,
/// data/ora, presenze, responsabile, clip e note.
pub fn report_embed(report: &Value) -> EmbedSpec {
    let name = if truthy(report.get("name")) { py_str(field_of(report, "name")) } else { "Senza titolo".to_string() };
    let mut embed = EmbedSpec { title: Some(format!("📅 Nuovo evento · {}", truncate(&name, 220))), description: None, color: REPORT_COLOR, fields: Vec::new(), footer: None, thumbnail: None, image: None, timestamp: None };
    embed = add_field(embed, "Categoria", field_of(report, "category"));
    let event_date = report.get("event_date").map_or_else(|| "—".to_string(), py_str);
    let event_time = if truthy(report.get("event_time")) { py_str(field_of(report, "event_time")) } else { "—".to_string() };
    embed = add_field(embed, "Data e ora", &json!(format!("{event_date} · {}", truncate(&event_time, 5))));
    embed = add_field(embed, "Presenze", report.get("attendees").unwrap_or(&json!(0)));
    embed = add_field(embed, "Nuovi membri", report.get("new_members").unwrap_or(&json!(0)));
    embed = add_field(embed, "Durata", &json!(format!("{} minuti", py_str(report.get("duration").unwrap_or(&json!(0))))));
    embed = add_field(embed, "Responsabile", field_of(report, "responsible"));
    if truthy(report.get("new_member_names")) {
        embed = add_field(embed, "Nomi nuovi membri", field_of(report, "new_member_names"));
    }
    let clip = if truthy(report.get("clip_recorded")) {
        let registrar = if truthy(report.get("clip_registrar")) { py_str(field_of(report, "clip_registrar")) } else { "registratore non indicato".to_string() };
        format!("Sì · {registrar}")
    } else {
        "No".to_string()
    };
    embed = add_field(embed, "Clip social", &json!(clip));
    embed = add_field(embed, "Esito", &json!(if truthy(report.get("flop")) { "Flop" } else { "Regolare" }));
    if truthy(report.get("notes")) {
        embed = embed.field("Note", truncate(&py_str(field_of(report, "notes")), 1024), false);
    }
    embed.footer("Registrato dal sito SFO")
}

// --- UUID --------------------------------------------------------------------------------------

/// `str(UUID(testo))` di Python: accetta anche maiuscole, graffe, `urn:uuid:` e senza trattini.
/// Restituisce l'UUID formattato con trattini, in minuscolo, oppure `None` se il formato non è valido.
pub fn parse_uuid(raw: &str) -> Option<String> {
    let text = raw.replace("urn:", "").replace("uuid:", "");
    let text: String = text.trim_matches(|c| c == '{' || c == '}').chars().filter(|c| *c != '-').collect();
    if text.len() != 32 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let text = text.to_ascii_lowercase();
    Some(format!("{}-{}-{}-{}-{}", &text[..8], &text[8..12], &text[12..16], &text[16..20], &text[20..]))
}

// --- route -------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct ReportsState {
    pub auth: Arc<SiteAuth>,
    pub db: Database,
    pub backend: Arc<dyn SiteBackend>,
    pub store: Arc<dyn ReportStore>,
    /// `SUPABASE_URL` (senza `/` finale) e `SUPABASE_SERVICE_ROLE_KEY`.
    pub supabase_url: String,
    pub service_key: String,
    /// `GUILD_ID` dell'ambiente, se `guild_id` non è nelle impostazioni.
    pub fallback_guild_id: Option<u64>,
    /// Serializza le pubblicazioni, così un report non esce due volte.
    pub send_lock: Arc<tokio::sync::Mutex<()>>,
    pub clock: fn() -> chrono::DateTime<chrono_tz::Tz>,
}

/// Definisce le route API per i report degli eventi.
/// Espone endpoint per elencare, creare, aggiornare, eliminare e pubblicare report.
pub fn routes(state: ReportsState) -> Router {
    Router::new()
        .route("/api/site/reports", get(list_reports).post(create_report))
        .route("/api/site/reports/{report_id}", axum::routing::put(update_report).delete(delete_report))
        .route("/api/site/reports/{report_id}/publish", post(publish_report))
        .with_state(state)
}

impl ReportsState {
    /// Chi fa la richiesta, e solo se Supabase è configurato.
    async fn authorize(&self, headers: &HeaderMap) -> Result<SitePrincipal, Response> {
        let principal = self.auth.authorize(headers, false).await?;
        if !self.supabase_url.starts_with("https://") || self.service_key.is_empty() {
            return Err(json_error(StatusCode::SERVICE_UNAVAILABLE, "Report non configurati: imposta URL Supabase e chiave service role nell'hosting"));
        }
        Ok(principal)
    }

    /// Una richiesta a Supabase: `Ok(None)` se ha risposto con un errore HTTP.
    async fn call(&self, method: &str, params: Vec<(&str, String)>, payload: Option<&Value>, prefer: Option<&str>) -> Result<Option<Value>, Response> {
        match self.store.request(StoreCall { method, params, payload, prefer }).await {
            Ok(StoreReply::Rows(rows)) => Ok(Some(rows)),
            Ok(StoreReply::Failed) => Ok(None),
            Err(StoreError::Unreachable) => {
                error!("Richiesta a Supabase ({method}) non riuscita: non raggiungibile.");
                Err(json_error(StatusCode::BAD_GATEWAY, "Supabase non raggiungibile"))
            }
        }
    }

    fn audit(&self, principal: &SitePrincipal, action: &str, report_id: &str) {
        self.backend.audit(AuditEntry::new(Some(principal.user_id), action).entity("report", report_id));
    }
}

/// `not rows` di Python.
fn is_empty(rows: &Option<Value>) -> bool {
    !truthy(rows.as_ref())
}

async fn list_reports(State(state): State<ReportsState>, headers: HeaderMap, RawQuery(raw): RawQuery) -> Response {
    if let Err(response) = state.authorize(&headers).await {
        return response;
    }
    let query = query_params(raw);
    let offset = query.get("offset").map_or(Some(0), |raw| python_int(raw));
    let limit = query.get("limit").map_or(Some(1000), |raw| python_int(raw));
    let (Some(offset), Some(limit)) = (offset, limit) else {
        return json_error(StatusCode::BAD_REQUEST, "Paginazione non valida");
    };
    if offset < 0 || !(1..=1000).contains(&limit) {
        return json_error(StatusCode::BAD_REQUEST, "Paginazione non valida");
    }
    let params = vec![
        ("select", REPORT_COLUMNS.to_string()),
        ("order", "event_date.desc,created_at.desc,id.desc".to_string()),
        ("limit", limit.to_string()),
        ("offset", offset.to_string()),
    ];
    match state.call("GET", params, None, None).await {
        Err(response) => response,
        Ok(None) => json_error(StatusCode::BAD_GATEWAY, "Impossibile leggere i report"),
        Ok(Some(rows)) => json_response(StatusCode::OK, &rows),
    }
}

/// Il corpo JSON di una richiesta, o la risposta d'errore.
async fn json_payload(body: Body, message: &str) -> Result<Value, Response> {
    match read_json_body(body).await {
        Ok(value) => Ok(value),
        Err(BodyError::TooLarge) => Err(plain(StatusCode::PAYLOAD_TOO_LARGE, "Maximum request body size 65536 exceeded")),
        Err(BodyError::Invalid) => Err(json_error(StatusCode::BAD_REQUEST, message)),
    }
}

fn validation_response(invalid: Invalid) -> Response {
    match invalid {
        Invalid::Message(message) => json_error(StatusCode::BAD_REQUEST, &message),
        Invalid::Crash => plain(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error\n\nServer got itself in trouble"),
    }
}

async fn create_report(State(state): State<ReportsState>, headers: HeaderMap, body: Body) -> Response {
    let principal = match state.authorize(&headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let payload = match json_payload(body, "Corpo JSON non valido").await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let values = match validate_report(&payload) {
        Ok(values) => Value::Object(values),
        Err(invalid) => return validation_response(invalid),
    };
    let rows = match state.call("POST", vec![("select", REPORT_COLUMNS.to_string())], Some(&values), Some("return=representation")).await {
        Ok(rows) => rows,
        Err(response) => return response,
    };
    if is_empty(&rows) {
        return json_error(StatusCode::BAD_GATEWAY, "Supabase non ha confermato il report");
    }
    let Some(report) = rows.as_ref().and_then(|rows| rows.as_array()).and_then(|rows| rows.first()).cloned() else {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error\n\nServer got itself in trouble");
    };
    let Some(id) = report.get("id").map(py_str) else {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error\n\nServer got itself in trouble");
    };
    state.audit(&principal, "site_report_created", &id);
    publish_site_change("reports");
    json_response(StatusCode::CREATED, &report)
}

async fn update_report(State(state): State<ReportsState>, headers: HeaderMap, Path(raw_id): Path<String>, body: Body) -> Response {
    let principal = match state.authorize(&headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(report_id) = parse_uuid(&raw_id) else {
        return json_error(StatusCode::BAD_REQUEST, "Richiesta non valida");
    };
    let payload = match json_payload(body, "Richiesta non valida").await {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let values = match validate_report(&payload) {
        Ok(values) => Value::Object(values),
        Err(invalid) => return validation_response(invalid),
    };
    let params = vec![("id", format!("eq.{report_id}")), ("select", "id".to_string())];
    let rows = match state.call("PATCH", params, Some(&values), Some("return=representation")).await {
        Ok(rows) => rows,
        Err(response) => return response,
    };
    if is_empty(&rows) {
        return json_error(StatusCode::NOT_FOUND, "Report non trovato");
    }
    state.audit(&principal, "site_report_updated", &report_id);
    publish_site_change("reports");
    json_response(StatusCode::OK, &json!({"ok": true}))
}

async fn delete_report(State(state): State<ReportsState>, headers: HeaderMap, Path(raw_id): Path<String>) -> Response {
    let principal = match state.authorize(&headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(report_id) = parse_uuid(&raw_id) else {
        return json_error(StatusCode::BAD_REQUEST, "ID report non valido");
    };
    let params = vec![("id", format!("eq.{report_id}")), ("select", "id".to_string())];
    let rows = match state.call("DELETE", params, None, Some("return=representation")).await {
        Ok(rows) => rows,
        Err(response) => return response,
    };
    if is_empty(&rows) {
        return json_error(StatusCode::NOT_FOUND, "Report non trovato");
    }
    // Il report è già sparito da Supabase: la riga locale rimasta dice solo che era stato pubblicato.
    if let Err(e) = state.db.transaction(|tx| tx.execute("DELETE FROM site_report_notifications WHERE report_id=?", [&report_id])) {
        error!("Pulizia della notifica locale del report {report_id} non riuscita: {e}");
    }
    state.audit(&principal, "site_report_deleted", &report_id);
    publish_site_change("reports");
    json_response(StatusCode::OK, &json!({"ok": true}))
}

async fn publish_report(State(state): State<ReportsState>, headers: HeaderMap, Path(raw_id): Path<String>) -> Response {
    let principal = match state.authorize(&headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(report_id) = parse_uuid(&raw_id) else {
        return json_error(StatusCode::BAD_REQUEST, "ID report non valido");
    };
    let channel_setting = state.db.get_setting("event_report_channel").ok().flatten().filter(|value| !value.is_empty());
    let Some(channel_setting) = channel_setting else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Canale report eventi non configurato");
    };
    let params = vec![("id", format!("eq.{report_id}")), ("select", REPORT_COLUMNS.to_string()), ("limit", "1".to_string())];
    let reports = match state.call("GET", params, None, None).await {
        Ok(reports) => reports,
        Err(response) => return response,
    };
    let Some(report) = reports.as_ref().and_then(|reports| reports.as_array()).and_then(|reports| reports.first()).cloned() else {
        return json_error(StatusCode::NOT_FOUND, "Report non trovato");
    };

    let _guard = state.send_lock.lock().await;
    let existing: Option<String> = match state
        .db
        .with_conn(|conn| conn.query_row("SELECT message_id FROM site_report_notifications WHERE report_id=?", [&report_id], |row| row.get(0)).optional())
    {
        Ok(existing) => existing,
        Err(e) => {
            error!("Lettura delle notifiche dei report non riuscita: {e}");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error\n\nServer got itself in trouble");
        }
    };
    if existing.is_some() {
        return json_response(StatusCode::OK, &json!({"ok": true, "already_sent": true}));
    }
    let unavailable = || json_error(StatusCode::SERVICE_UNAVAILABLE, "Canale report eventi non disponibile");
    let Some(channel_id) = python_int(&channel_setting).and_then(|id| u64::try_from(id).ok()) else {
        error!("Canale report eventi {channel_setting} non valido.");
        return unavailable();
    };
    let guild_id = state
        .db
        .get_setting("guild_id")
        .ok()
        .flatten()
        .filter(|value| !value.is_empty())
        .and_then(|value| python_int(&value))
        .and_then(|id| u64::try_from(id).ok())
        .or(state.fallback_guild_id)
        .unwrap_or(0);
    match state.backend.channel_kind(guild_id, channel_id).await {
        Ok(ChannelKind::Text) => {}
        Ok(_) => return json_error(StatusCode::SERVICE_UNAVAILABLE, "Il canale report deve essere testuale"),
        Err(e) => {
            error!("Canale report eventi {channel_id} non disponibile: {e}");
            return unavailable();
        }
    }
    let message_id = match state.backend.send_embed(channel_id, report_embed(&report)).await {
        Ok(message_id) => message_id,
        Err(e @ (ApiError::Forbidden | ApiError::Http(_) | ApiError::NotFound)) => {
            error!("Impossibile pubblicare il report SFO {report_id}: {e}");
            return json_error(StatusCode::BAD_GATEWAY, "Discord non ha accettato il report");
        }
    };
    let recorded = state.db.transaction(|tx| {
        tx.execute(
            "INSERT INTO site_report_notifications(report_id,message_id,sent_at) VALUES(?,?,?)",
            params![report_id, message_id.to_string(), isoformat((state.clock)())],
        )
    });
    if let Err(e) = recorded {
        warn!("Report SFO {report_id} inviato ma non registrato: {e}");
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Invio del report non confermato");
    }
    state.audit(&principal, "site_report_published", &report_id);
    publish_site_change("reports");
    json_response(StatusCode::OK, &json!({"ok": true, "already_sent": false}))
}

/// Non usato in produzione: l'orario di Roma, per i test con orologio finto.
/// Restituisce l'ora corrente nel fuso di Roma (Europe/Rome), usato nei test con orologio finto.
pub fn rome_now() -> chrono::DateTime<chrono_tz::Tz> {
    Utc::now().with_timezone(&crate::config::TZ)
}

#[cfg(test)]
mod tests;
