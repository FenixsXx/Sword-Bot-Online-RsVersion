//! L'amministrazione del sito, `/api/site/admin/…` (`cogs/site_admin.py`): lo stato per la
//! schermata di configurazione, le impostazioni del bot, l'anteprima del benvenuto e la gestione dei
//! sondaggi (creare, modificare, eliminare, opzioni, pubblicare, anteprima).
//!
//! Ogni modifica passa dal registro audit e avvisa i collegamenti live del sito. Quasi tutto richiede
//! un amministratore; aggiungere o togliere opzioni e l'anteprima di un sondaggio bastano allo staff.
//!
//! Differenze da Python:
//! * il lock del sondaggio (`refresh_locks`) è quello del `Publisher`, preso tramite [`AdminOps`];
//! * eliminando un sondaggio, un messaggio che Discord vieta di togliere dà 502 anche quando Python
//!   (che cerca il canale nella cache) non avrebbe nemmeno provato: qui non c'è la cache;
//! * `NaN` e `Infinity` nel JSON non sono accettati (Python li accetta).

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post, put};
use chrono::DateTime;
use chrono_tz::Tz;
use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};
use tracing::{error, info};
use unicode_general_category::{GeneralCategory, get_general_category};

use crate::database::Database;
use crate::discord_api::{ApiError, ChannelKind, DiscordApi, MessagePayload};
use crate::formatting::casefold;
use crate::hooks::publish_site_change;
use crate::live_event::{decimal_digit, python_int};
use crate::notify::{AuditEntry, DiscordSender};
use crate::polls::find_poll;
use crate::queries::poll_options;
use crate::site_auth::{GuildOverview, SiteAuth, SiteDirectory, SitePrincipal};
use crate::site_backend::{DiscordBackend, SiteBackend};
use crate::site_common::{BodyError, json_error, json_response, plain, read_json_body, staff_role_values, status_text};
use crate::site_reports::{py_str, py_strip};
use crate::site_tools::{digits_param, generic_500};
use crate::stats::revoke_ballot_entry_stats;
use crate::voice::BoxFuture;
use crate::welcome::{DEFAULT_WELCOME_BODY, DEFAULT_WELCOME_GIF, DEFAULT_WELCOME_TITLE, NewMember, WelcomeSettings, build_welcome_embed};

/// Dopo quanto sparisce l'anteprima di un sondaggio.
pub const PREVIEW_SECONDS: u64 = 60;

type Reply = Result<Response, Response>;

// --- operazioni su Discord ---------------------------------------------------------------------

/// Il lock di un sondaggio, tenuto finché il valore esiste.
pub type PollGuard = Box<dyn Send>;

/// Le operazioni su Discord e sui servizi del bot che la sola `SiteBackend` non offre.
pub trait AdminOps: Send + Sync + 'static {
    /// Aggiorna tutti i sondaggi e la dashboard.
    fn refresh_all(&self) -> BoxFuture<'_, Result<(), String>>;
    /// Invia un messaggio (senza menzioni); restituisce l'ID.
    fn send_message(&self, channel_id: u64, payload: MessagePayload) -> BoxFuture<'_, Result<u64, ApiError>>;
    fn delete_message(&self, channel_id: u64, message_id: u64) -> BoxFuture<'_, Result<(), ApiError>>;
    /// Prende il lock del sondaggio.
    fn lock_poll(&self, poll_id: i64) -> BoxFuture<'_, PollGuard>;
    /// Dimentica il lock di un sondaggio eliminato.
    fn forget_poll_lock(&self, poll_id: i64);
}

impl<S: DiscordSender, A: DiscordApi> AdminOps for DiscordBackend<S, A> {
    fn refresh_all(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move { self.publisher.refresh_all().await.map_err(|e| e.to_string()) })
    }

    fn send_message(&self, channel_id: u64, payload: MessagePayload) -> BoxFuture<'_, Result<u64, ApiError>> {
        Box::pin(async move { self.publisher.api().send_message(channel_id, payload).await })
    }

    fn delete_message(&self, channel_id: u64, message_id: u64) -> BoxFuture<'_, Result<(), ApiError>> {
        Box::pin(async move { self.publisher.api().delete_message(channel_id, message_id).await })
    }

    fn lock_poll(&self, poll_id: i64) -> BoxFuture<'_, PollGuard> {
        Box::pin(async move { Box::new(self.publisher.lock_poll(poll_id).await) as PollGuard })
    }

    fn forget_poll_lock(&self, poll_id: i64) {
        self.publisher.forget_poll_lock(poll_id);
    }
}

/// Le operazioni collegate a bot avviato (il server HTTP parte prima del gateway).
#[derive(Default)]
pub struct LateAdminOps {
    /// Amministratore operations (disponibili una volta avviato il bot).
    inner: OnceLock<Arc<dyn AdminOps>>,
}

impl LateAdminOps {
    pub fn set(&self, ops: Arc<dyn AdminOps>) {
        let _ = self.inner.set(ops);
    }
}

fn not_ready() -> ApiError {
    ApiError::Http("il bot non è ancora pronto".into())
}

impl AdminOps for LateAdminOps {
    fn refresh_all(&self) -> BoxFuture<'_, Result<(), String>> {
        match self.inner.get() {
            Some(ops) => ops.refresh_all(),
            None => Box::pin(async { Err("il bot non è ancora pronto".to_string()) }),
        }
    }

    fn send_message(&self, channel_id: u64, payload: MessagePayload) -> BoxFuture<'_, Result<u64, ApiError>> {
        match self.inner.get() {
            Some(ops) => ops.send_message(channel_id, payload),
            None => Box::pin(async { Err(not_ready()) }),
        }
    }

    fn delete_message(&self, channel_id: u64, message_id: u64) -> BoxFuture<'_, Result<(), ApiError>> {
        match self.inner.get() {
            Some(ops) => ops.delete_message(channel_id, message_id),
            None => Box::pin(async { Err(not_ready()) }),
        }
    }

    fn lock_poll(&self, poll_id: i64) -> BoxFuture<'_, PollGuard> {
        match self.inner.get() {
            Some(ops) => ops.lock_poll(poll_id),
            None => Box::pin(async { Box::new(()) as PollGuard }),
        }
    }

    fn forget_poll_lock(&self, poll_id: i64) {
        if let Some(ops) = self.inner.get() {
            ops.forget_poll_lock(poll_id);
        }
    }
}

#[derive(Clone)]
pub struct AdminState {
    /// Stato amministrativo condiviso per le rotte del sito.
    pub auth: Arc<SiteAuth>,
    pub db: Database,
    pub backend: Arc<dyn SiteBackend>,
    pub ops: Arc<dyn AdminOps>,
    pub directory: Arc<dyn SiteDirectory>,
    pub clock: fn() -> DateTime<Tz>,
    /// Dopo quanto sparisce l'anteprima di un sondaggio.
    pub preview_delay: Duration,
}

pub fn routes(state: AdminState) -> Router {
    Router::new()
        .route("/api/site/admin/state", get(admin_state))
        .route("/api/site/admin/settings", put(update_settings))
        .route("/api/site/admin/welcome/test", post(test_welcome))
        .route("/api/site/admin/polls", post(create_poll))
        .route("/api/site/admin/polls/publish", post(publish_polls))
        .route("/api/site/admin/polls/test", post(test_poll))
        .route("/api/site/admin/polls/{poll_id}", put(update_poll).delete(delete_poll))
        .route("/api/site/admin/polls/{poll_id}/options", post(add_option))
        .route("/api/site/admin/polls/{poll_id}/options/{option_id}", axum::routing::delete(delete_option))
        .with_state(state)
}

// --- cose di Python ----------------------------------------------------------------------------

/// Il risultato di `int(valore)` di Python su un dato JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntArg {
    /// Intero normale.
    Int(i64),
    /// Un intero valido ma troppo grande per il database (e per qualsiasi ID).
    Huge,
    /// `TypeError` o `ValueError`.
    Invalid,
}

/// `int(testo)`: come [`python_int`], ma distingue un numero troppo grande da un testo non valido.
pub(crate) fn int_arg_text(text: &str) -> IntArg {
    if let Some(value) = python_int(text) {
        return IntArg::Int(value);
    }
    let text = text.trim();
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    let mut previous_underscore = true;
    let mut any = false;
    for c in digits.chars() {
        if c == '_' {
            if previous_underscore {
                return IntArg::Invalid;
            }
            previous_underscore = true;
        } else if decimal_digit(c).is_some() {
            previous_underscore = false;
            any = true;
        } else {
            return IntArg::Invalid;
        }
    }
    if any && !previous_underscore { IntArg::Huge } else { IntArg::Invalid }
}

fn int_arg(value: &Value) -> IntArg {
    match value {
        Value::String(text) => int_arg_text(text),
        Value::Bool(flag) => IntArg::Int(i64::from(*flag)),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                IntArg::Int(int)
            } else if number.is_u64() {
                IntArg::Huge
            } else {
                match number.as_f64() {
                    Some(float) if float.is_finite() && float.abs() < 9.0e18 => IntArg::Int(float.trunc() as i64),
                    Some(float) if float.is_finite() => IntArg::Huge,
                    _ => IntArg::Invalid,
                }
            }
        }
        _ => IntArg::Invalid,
    }
}

/// `isinstance(valore, int)` senza i booleani.
fn is_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) if !number.is_f64() => number.as_i64(),
        _ => None,
    }
}

/// La prima lettera della categoria Unicode (`unicodedata.category(c)[0]`).
fn category_class(category: GeneralCategory) -> char {
    use GeneralCategory::*;
    match category {
        UppercaseLetter | LowercaseLetter | TitlecaseLetter | ModifierLetter | OtherLetter => 'L',
        NonspacingMark | SpacingMark | EnclosingMark => 'M',
        DecimalNumber | LetterNumber | OtherNumber => 'N',
        ConnectorPunctuation | DashPunctuation | OpenPunctuation | ClosePunctuation | InitialPunctuation | FinalPunctuation | OtherPunctuation => 'P',
        MathSymbol | CurrencySymbol | ModifierSymbol | OtherSymbol => 'S',
        SpaceSeparator | LineSeparator | ParagraphSeparator => 'Z',
        Control | Format | Surrogate | PrivateUse | Unassigned => 'C',
        // La libreria può aggiungere categorie: un codice sconosciuto è come uno non assegnato.
        _ => 'C',
    }
}

/// `re.fullmatch(r"<a?:[A-Za-z0-9_]{2,32}:\d{17,20}>", valore)`.
fn custom_emoji(value: &str) -> bool {
    let Some(inner) = value.strip_prefix('<').and_then(|rest| rest.strip_suffix('>')) else {
        return false;
    };
    let inner = match inner.strip_prefix('a') {
        Some(rest) if rest.starts_with(':') => rest,
        _ => inner,
    };
    let Some(inner) = inner.strip_prefix(':') else {
        return false;
    };
    let Some((name, id)) = inner.split_once(':') else {
        return false;
    };
    let name_length = name.chars().count();
    let id_length = id.chars().count();
    (2..=32).contains(&name_length)
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && (17..=20).contains(&id_length)
        && id.chars().all(|c| decimal_digit(c).is_some())
}

/// `_valid_emoji`: un'emoji personalizzata o da 1 a 32 caratteri fatti di simboli, segni, numeri e
/// controlli, con almeno un simbolo (o il tasto «keycap»).
fn valid_emoji(value: &str) -> bool {
    if custom_emoji(value) {
        return true;
    }
    let categories: Vec<GeneralCategory> = value.chars().map(get_general_category).collect();
    (1..=categories.len().min(32)).contains(&categories.len())
        && (categories.iter().any(|c| matches!(c, GeneralCategory::OtherSymbol | GeneralCategory::ModifierSymbol)) || value.ends_with('\u{20e3}'))
        && categories.iter().all(|c| matches!(category_class(*c), 'S' | 'M' | 'C' | 'N'))
}

/// `re.fullmatch(r"#?[0-9a-fA-F]{6}", valore)`.
fn valid_color(value: &str) -> bool {
    let digits = value.strip_prefix('#').unwrap_or(value);
    digits.len() == 6 && digits.chars().all(|c| c.is_ascii_hexdigit())
}

/// L'esito del controllo di un indirizzo di GIF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GifCheck {
    Ok,
    Rejected,
    /// `urlsplit` solleva `ValueError`: in Python è un errore non gestito (500).
    Crash,
}

/// `_check_bracketed_host` di `urllib.parse`.
fn bracketed_host_valid(host: &str) -> bool {
    if let Some(rest) = host.strip_prefix('v') {
        let Some((hex, tail)) = rest.split_once('.') else { return false };
        return !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()) && !tail.is_empty();
    }
    let address = host.split_once('%').map_or(host, |(address, scope)| if scope.is_empty() { host } else { address });
    address.parse::<std::net::Ipv6Addr>().is_ok()
}

/// Il controllo dell'URL di una GIF: `urlsplit(valore)` con schema `https`, un host e nessuna
/// credenziale.
fn check_gif_url(value: &str) -> GifCheck {
    let url: String = value.trim_start_matches(|c: char| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    let mut scheme = String::new();
    let mut rest = url.as_str();
    if let Some(index) = url.find(':') {
        let head = &url[..index];
        let first = head.chars().next();
        if index > 0 && first.is_some_and(|c| c.is_ascii_alphabetic()) && head.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            scheme = head.to_ascii_lowercase();
            rest = &url[index + 1..];
        }
    }
    let mut netloc = "";
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = &after[..end];
        let (opens, closes) = (netloc.contains('['), netloc.contains(']'));
        if opens != closes {
            return GifCheck::Crash;
        }
        if opens && closes {
            let host = netloc.split_once('[').map_or("", |(_, tail)| tail.split_once(']').map_or(tail, |(host, _)| host));
            if !bracketed_host_valid(host) {
                return GifCheck::Crash;
            }
        }
    }
    let (userinfo, has_userinfo) = match netloc.rsplit_once('@') {
        Some((userinfo, _)) => (userinfo, true),
        None => ("", false),
    };
    let (username, password) = if has_userinfo {
        match userinfo.split_once(':') {
            Some((username, password)) => (username, password),
            None => (userinfo, ""),
        }
    } else {
        ("", "")
    };
    if scheme != "https" || netloc.is_empty() || !username.is_empty() || !password.is_empty() { GifCheck::Rejected } else { GifCheck::Ok }
}

/// Il valore di una colonna come JSON (`sqlite3.Row` -> `json.dumps`).
fn sql_to_json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(number) => json!(number),
        SqlValue::Real(number) => json!(number),
        SqlValue::Text(text) => json!(text),
        SqlValue::Blob(_) => Value::Null,
    }
}

fn sql_truthy(value: &SqlValue) -> bool {
    match value {
        SqlValue::Null => false,
        SqlValue::Integer(number) => *number != 0,
        SqlValue::Real(number) => *number != 0.0,
        SqlValue::Text(text) => !text.is_empty(),
        SqlValue::Blob(bytes) => !bytes.is_empty(),
    }
}

fn is_constraint(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(failure, _) if failure.code == rusqlite::ErrorCode::ConstraintViolation)
}

fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

// --- piccoli aiuti per le route ----------------------------------------------------------------

fn bad(message: &str) -> Response {
    json_error(StatusCode::BAD_REQUEST, message)
}

fn ok_body() -> Response {
    json_response(StatusCode::OK, &json!({"ok": true}))
}

/// Una risposta senza `Cache-Control: no-store`, come le `web.json_response` di Python senza intestazioni.
fn without_cache_control(status: StatusCode, value: &Value) -> Response {
    let mut response = json_response(status, value);
    response.headers_mut().remove(header::CACHE_CONTROL);
    response
}

fn failed<T>(what: &str, result: Result<T, rusqlite::Error>) -> Result<T, Response> {
    result.map_err(|e| {
        error!("{what}: {e}");
        generic_500()
    })
}

async fn read_json(body: Body, invalid: &str) -> Result<Value, Response> {
    match read_json_body(body).await {
        Ok(value) => Ok(value),
        Err(BodyError::TooLarge) => Err(plain(StatusCode::PAYLOAD_TOO_LARGE, "Maximum request body size 65536 exceeded")),
        Err(BodyError::Invalid) => Err(bad(invalid)),
    }
}

impl AdminState {
    async fn principal(&self, headers: &HeaderMap, admin_only: bool) -> Result<SitePrincipal, Response> {
        self.auth.authorize(headers, admin_only).await
    }

    fn setting(&self, key: &str) -> Result<Option<String>, Response> {
        failed("Lettura di un'impostazione", self.db.get_setting(key))
    }

    fn setting_or(&self, key: &str, default: &str) -> Result<String, Response> {
        failed("Lettura di un'impostazione", self.db.get_setting_or(key, default))
    }

    /// Il server configurato (`configured_guild`), se il bot lo conosce.
    fn guild(&self) -> Option<GuildOverview> {
        let (guild_id, _) = self.auth.configured_guild()?;
        self.directory.guild_overview(guild_id)
    }

    fn guild_or_503(&self) -> Result<GuildOverview, Response> {
        self.guild().ok_or_else(|| json_error(StatusCode::SERVICE_UNAVAILABLE, "Server Discord non disponibile o non configurato"))
    }

    fn audit(&self, principal: &SitePrincipal, action: &str, entity: Option<(&str, Option<i64>)>, details: Value) {
        let mut entry = AuditEntry::new(Some(principal.user_id), action);
        entry = match entity {
            Some((kind, Some(id))) => entry.entity(kind, id),
            Some((kind, None)) => entry.entity_type(kind),
            None => entry,
        };
        self.backend.audit(entry.details(details));
    }

    /// `refresh_discord_poll`: aggiorna il sondaggio e la dashboard; `Some` con il 502 se Discord rifiuta.
    async fn refresh_discord_poll(&self, poll_id: i64, failure_message: &str) -> Option<Response> {
        let refreshed = async {
            self.backend.refresh_poll(poll_id).await?;
            self.backend.refresh_dashboard().await
        };
        match refreshed.await {
            Ok(()) => None,
            Err(e) => {
                error!("Sondaggio {poll_id} salvato ma refresh Discord fallito: {e}");
                Some(json_error(StatusCode::BAD_GATEWAY, failure_message))
            }
        }
    }

    /// Il canale dei sondaggi come ID: `int(get_setting("poll_channel"))` solleva se non è un numero.
    fn poll_channel_id(&self) -> Result<Option<Option<u64>>, Response> {
        let Some(text) = self.setting("poll_channel")?.filter(|text| !text.is_empty()) else {
            return Ok(None);
        };
        match int_arg_text(&text) {
            IntArg::Int(id) => Ok(Some(u64::try_from(id).ok())),
            IntArg::Huge => Ok(Some(None)),
            IntArg::Invalid => Err(generic_500()),
        }
    }
}

// --- stato -------------------------------------------------------------------------------------

fn welcome_settings_json(state: &AdminState) -> Result<Value, Response> {
    Ok(json!({
        "enabled": state.setting_or("welcome_enabled", "1")? == "1",
        "channel_id": state.setting_or("welcome_channel", "")?,
        "title": state.setting_or("welcome_title", DEFAULT_WELCOME_TITLE)?,
        "body": state.setting_or("welcome_body", DEFAULT_WELCOME_BODY)?,
        "gif_url": state.setting_or("welcome_gif", DEFAULT_WELCOME_GIF)?,
    }))
}

fn polls_json(state: &AdminState) -> Result<Vec<Value>, Response> {
    type PollRowValues = (SqlValue, SqlValue, SqlValue, SqlValue, SqlValue, SqlValue, SqlValue);
    let rows: Result<Vec<PollRowValues>, rusqlite::Error> = state.db.with_conn(|conn| {
        conn.prepare("SELECT id,name,emoji,color,position,active,is_community FROM polls ORDER BY position,id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))?
            .collect()
    });
    let mut polls = Vec::new();
    for (id, name, emoji, color, position, active, community) in failed("Elenco dei sondaggi", rows)? {
        let poll_id = match &id {
            SqlValue::Integer(id) => *id,
            _ => 0,
        };
        let options = failed("Opzioni di un sondaggio", poll_options(&state.db, poll_id))?;
        polls.push(json!({
            "id": sql_to_json(id),
            "name": sql_to_json(name),
            "emoji": sql_to_json(emoji),
            "color": sql_to_json(color),
            "position": sql_to_json(position),
            "active": sql_truthy(&active),
            "is_community": sql_truthy(&community),
            "options": options.iter().map(|option| json!({"id": option.id, "label": option.label, "votes": option.n})).collect::<Vec<_>>(),
        }));
    }
    Ok(polls)
}

async fn admin_state(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    admin_state_inner(&state, &headers).await.unwrap_or_else(|response| response)
}

async fn admin_state_inner(state: &AdminState, headers: &HeaderMap) -> Reply {
    let principal = state.principal(headers, false).await?;
    let guild = state.guild_or_503()?;
    let id_setting = |key: &str| state.setting_or(key, "");
    let mut settings = welcome_settings_json(state)?;
    let extra = json!({
        "poll_channel_id": id_setting("poll_channel")?,
        "staff_channel_id": id_setting("staff_channel")?,
        "event_report_channel_id": id_setting("event_report_channel")?,
        "activity_channel_id": id_setting("activity_channel")?,
        "activity_feed_enabled": state.setting_or("activity_feed_enabled", "0")? == "1",
        "staff_role_ids": staff_role_values(&state.setting_or("staff_roles", "")?),
        "slots": match int_arg_text(&state.setting_or("slots", "5")?) {
            IntArg::Int(slots) => slots,
            _ => return Err(generic_500()),
        },
        "tipi": state.setting_or("tipi", crate::config::DEFAULT_TIPI)?,
    });
    if let (Some(settings), Value::Object(extra)) = (settings.as_object_mut(), extra) {
        settings.extend(extra);
    }
    let result = json!({
        "channels": guild.text_channels.iter().map(|(id, name)| json!({"id": id.to_string(), "name": name})).collect::<Vec<_>>(),
        "roles": guild.roles.iter().map(|(id, name)| json!({"id": id.to_string(), "name": name})).collect::<Vec<_>>(),
        "server_name": guild.name,
        "member_count": guild.member_count,
        "principal": {"user_id": principal.user_id.to_string(), "display_name": principal.display_name, "is_admin": principal.is_admin},
        "settings": settings,
        "polls": polls_json(state)?,
    });
    Ok(json_response(StatusCode::OK, &result))
}

// --- impostazioni ------------------------------------------------------------------------------

/// `parse_channel`: l'ID di un canale testuale del server, o il messaggio di errore.
fn parse_channel(guild: &GuildOverview, raw: &Value, label: &str) -> Result<Option<String>, String> {
    if raw.is_null() || raw.as_str() == Some("") {
        return Ok(None);
    }
    match int_arg(raw) {
        IntArg::Invalid => Err(format!("{label}: ID non valido")),
        IntArg::Huge => Err(format!("{label}: seleziona un canale testuale del server")),
        IntArg::Int(id) => match u64::try_from(id).ok().filter(|id| guild.text_channels.iter().any(|(channel, _)| channel == id)) {
            Some(id) => Ok(Some(id.to_string())),
            None => Err(format!("{label}: seleziona un canale testuale del server")),
        },
    }
}

const CHANNEL_FIELDS: [(&str, &str, &str); 5] = [
    ("welcome_channel_id", "welcome_channel", "Canale benvenuto"),
    ("poll_channel_id", "poll_channel", "Canale sondaggi"),
    ("staff_channel_id", "staff_channel", "Canale staff"),
    ("event_report_channel_id", "event_report_channel", "Canale report eventi"),
    ("activity_channel_id", "activity_channel", "Canale feed attività"),
];

const WELCOME_FIELDS: [(&str, &str, usize); 3] = [("welcome_title", "welcome_title", 256), ("welcome_body", "welcome_body", 4000), ("welcome_gif", "welcome_gif", 1000)];

/// Le impostazioni da salvare, in ordine, o l'errore da restituire.
fn collect_settings(state: &AdminState, guild: &GuildOverview, payload: &Map<String, Value>) -> Result<Vec<(String, String)>, Response> {
    let mut values: Vec<(String, String)> = Vec::new();
    let find = |values: &[(String, String)], key: &str| values.iter().find(|(name, _)| name == key).map(|(_, value)| value.clone());

    for (field, key, label) in CHANNEL_FIELDS {
        if let Some(raw) = payload.get(field) {
            let channel = parse_channel(guild, raw, label).map_err(|message| bad(&message))?;
            values.push((key.to_string(), channel.unwrap_or_default()));
        }
    }

    if let Some(enabled) = payload.get("welcome_enabled") {
        let Value::Bool(enabled) = enabled else {
            return Err(bad("Stato benvenuto non valido"));
        };
        values.push(("welcome_enabled".into(), if *enabled { "1" } else { "0" }.into()));
    }

    if let Some(slots) = payload.get("slots") {
        match is_int(slots) {
            Some(slots @ 3..=5) => values.push(("slots".into(), slots.to_string())),
            _ => return Err(bad("Gli slot devono essere 3, 4 o 5")),
        }
    }

    if let Some(enabled) = payload.get("activity_feed_enabled") {
        let Value::Bool(enabled) = enabled else {
            return Err(bad("Stato del feed attività non valido"));
        };
        let activity_channel = match find(&values, "activity_channel") {
            Some(channel) => channel,
            None => state.setting_or("activity_channel", "")?,
        };
        if *enabled && activity_channel.is_empty() {
            return Err(bad("Seleziona un canale per il feed attività"));
        }
        values.push(("activity_feed_enabled".into(), if *enabled { "1" } else { "0" }.into()));
    }

    if let Some(role_ids) = payload.get("staff_role_ids") {
        let invalid = || bad("Elenco ruoli staff non valido");
        let Value::Array(items) = role_ids else { return Err(invalid()) };
        let ids: Option<Vec<&str>> = items.iter().map(Value::as_str).collect();
        let Some(ids) = ids.filter(|ids| ids.len() <= 50) else { return Err(invalid()) };
        if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err(invalid());
        }
        let valid: HashSet<String> = guild.roles.iter().map(|(id, _)| id.to_string()).collect();
        if !ids.iter().all(|id| valid.contains(*id)) {
            return Err(bad("Uno o più ruoli non appartengono al server"));
        }
        let mut sorted: Vec<u64> = ids.iter().filter_map(|id| id.parse().ok()).collect();
        sorted.sort_unstable();
        values.push(("staff_roles".into(), sorted.iter().map(u64::to_string).collect::<Vec<_>>().join(",")));
    }

    if let Some(tipi) = payload.get("tipi") {
        let invalid = || bad("Elenco dei tipi proposta non valido");
        let Value::String(tipi) = tipi else { return Err(invalid()) };
        let items: Vec<&str> = tipi.split(',').collect();
        if tipi.chars().count() > 500 || py_strip(tipi).is_empty() || items.iter().any(|item| py_strip(item).is_empty()) || items.len() > 20 {
            return Err(invalid());
        }
        let mut seen = HashSet::new();
        let unique: Vec<&str> = items.iter().map(|item| py_strip(item)).filter(|item| seen.insert(*item)).collect();
        values.push(("tipi".into(), unique.join(",")));
    }

    for (field, key, limit) in WELCOME_FIELDS {
        let Some(value) = payload.get(field) else { continue };
        let Some(text) = value.as_str().filter(|text| text.chars().count() <= limit) else {
            return Err(bad(&format!("Campo {field} non valido o troppo lungo")));
        };
        if field == "welcome_gif" && !text.is_empty() {
            match check_gif_url(text) {
                GifCheck::Ok => {}
                GifCheck::Rejected => return Err(bad("La GIF deve usare un URL HTTPS valido")),
                GifCheck::Crash => return Err(generic_500()),
            }
        }
        values.push((key.to_string(), text.to_string()));
    }
    Ok(values)
}

async fn update_settings(State(state): State<AdminState>, headers: HeaderMap, body: Body) -> Response {
    update_settings_inner(&state, &headers, body).await.unwrap_or_else(|response| response)
}

async fn update_settings_inner(state: &AdminState, headers: &HeaderMap, body: Body) -> Reply {
    let principal = state.principal(headers, true).await?;
    let Value::Object(payload) = read_json(body, "Corpo JSON non valido").await? else {
        return Err(bad("Corpo JSON non valido"));
    };
    let guild = state.guild_or_503()?;
    let values = collect_settings(state, &guild, &payload)?;
    if values.is_empty() {
        return Err(bad("Nessuna impostazione da salvare"));
    }
    for (key, value) in &values {
        failed("Salvataggio di un'impostazione", state.db.set_setting(key, value))?;
    }
    let mut keys: Vec<&str> = values.iter().map(|(key, _)| key.as_str()).collect();
    keys.sort_unstable();
    state.audit(&principal, "site_configuration_updated", Some(("settings", None)), json!({"keys": keys}));
    publish_site_change("settings");
    if values.iter().any(|(key, _)| key == "poll_channel") {
        match state.ops.refresh_all().await {
            Ok(()) => publish_site_change("polls"),
            Err(e) => error!("Aggiornamento sondaggi dopo la configurazione fallito: {e}"),
        }
    }
    Ok(json_response(StatusCode::OK, &json!({"ok": true, "settings": welcome_settings_json(state)?})))
}

// --- anteprima del benvenuto -------------------------------------------------------------------

async fn test_welcome(State(state): State<AdminState>, headers: HeaderMap, body: Body) -> Response {
    test_welcome_inner(&state, &headers, body).await.unwrap_or_else(|response| response)
}

async fn test_welcome_inner(state: &AdminState, headers: &HeaderMap, body: Body) -> Reply {
    let principal = state.principal(headers, true).await?;
    let guild = state.guild_or_503()?;
    let Value::Object(payload) = read_json(body, "Corpo JSON non valido").await? else {
        return Err(bad("Corpo JSON non valido"));
    };
    let from_payload = payload.get("welcome_channel_id").filter(|value| match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    });
    let raw = match from_payload {
        Some(value) => value.clone(),
        None => state.setting("welcome_channel")?.map_or(Value::Null, Value::String),
    };
    let channel_id = match parse_channel(&guild, &raw, "Canale benvenuto") {
        Ok(Some(channel_id)) => channel_id,
        Ok(None) => return Err(bad("Seleziona un canale di benvenuto")),
        Err(message) => return Err(bad(&message)),
    };
    let mut settings = WelcomeSettings {
        title_template: state.setting_or("welcome_title", DEFAULT_WELCOME_TITLE)?,
        body_template: state.setting_or("welcome_body", DEFAULT_WELCOME_BODY)?,
        gif_url: state.setting_or("welcome_gif", DEFAULT_WELCOME_GIF)?,
    };
    for (field, limit) in [("welcome_title", 256), ("welcome_body", 4000), ("welcome_gif", 1000)] {
        if let Some(value) = payload.get(field) {
            let Some(text) = value.as_str().filter(|text| text.chars().count() <= limit) else {
                return Err(bad(&format!("Campo {field} non valido")));
            };
            match field {
                "welcome_title" => settings.title_template = text.to_string(),
                "welcome_body" => settings.body_template = text.to_string(),
                _ => settings.gif_url = text.to_string(),
            }
        }
    }
    if !settings.gif_url.is_empty() {
        match check_gif_url(&settings.gif_url) {
            GifCheck::Ok => {}
            GifCheck::Rejected => return Err(bad("La GIF deve usare un URL HTTPS valido")),
            GifCheck::Crash => return Err(generic_500()),
        }
    }
    let Ok(channel_id) = channel_id.parse::<u64>() else {
        return Err(generic_500());
    };
    let member = NewMember { username: "Utente di prova", server_name: &guild.name, member_count: guild.member_count + 1, avatar_url: &guild.bot_avatar_url };
    let payload = MessagePayload::embed(build_welcome_embed(&settings, &member)).with_content("🧪 Anteprima del benvenuto (nessun membro è stato menzionato).");
    let message_id = match state.ops.send_message(channel_id, payload).await {
        Ok(message_id) => message_id,
        Err(e) => {
            error!("Invio del test benvenuto fallito nel canale {channel_id}: {e}");
            return Err(json_error(StatusCode::BAD_GATEWAY, "Discord non ha accettato il messaggio di prova"));
        }
    };
    state.audit(&principal, "welcome_test_sent", Some(("channel", Some(channel_id as i64))), json!({"message_id": message_id}));
    Ok(ok_body())
}

// --- creare un sondaggio -----------------------------------------------------------------------

enum Created {
    Conflict,
    Done(i64),
}

async fn create_poll(State(state): State<AdminState>, headers: HeaderMap, body: Body) -> Response {
    create_poll_inner(&state, &headers, body).await.unwrap_or_else(|response| response)
}

async fn create_poll_inner(state: &AdminState, headers: &HeaderMap, body: Body) -> Reply {
    let principal = state.principal(headers, true).await?;
    if state.setting("poll_channel")?.is_none_or(|channel| channel.is_empty()) {
        return Err(bad("Seleziona prima il canale sondaggi"));
    }
    let Value::Object(payload) = read_json(body, "Corpo JSON non valido").await? else {
        return Err(bad("Corpo JSON non valido"));
    };
    let name = payload.get("name");
    let default_emoji = Value::String("🎮".into());
    let default_color = Value::String("#5865F2".into());
    let emoji = payload.get("emoji").unwrap_or(&default_emoji);
    let color = payload.get("color").unwrap_or(&default_color);
    let community = payload.get("is_community").unwrap_or(&Value::Bool(false)).clone();
    let option_labels = payload.get("options").cloned().unwrap_or_else(|| json!([]));

    let name = match name.and_then(Value::as_str).map(py_strip) {
        Some(name) if (1..=100).contains(&name.chars().count()) => name.to_string(),
        _ => return Err(bad("Il nome sondaggio deve contenere da 1 a 100 caratteri")),
    };
    let Some(emoji) = emoji.as_str().filter(|emoji| valid_emoji(emoji)) else {
        return Err(bad("Emoji non valida"));
    };
    let Some(color) = color.as_str().filter(|color| valid_color(color)) else {
        return Err(bad("Colore non valido: usa #RRGGBB"));
    };
    let Value::Bool(community) = community else {
        return Err(bad("Tipo sondaggio non valido"));
    };
    let Value::Array(option_labels) = option_labels else {
        return Err(bad("Un sondaggio può avere al massimo 25 opzioni"));
    };
    if option_labels.len() > 25 {
        return Err(bad("Un sondaggio può avere al massimo 25 opzioni"));
    }
    let labels: Vec<String> = option_labels.iter().map(|label| py_strip(&py_str(label)).to_string()).collect();
    if labels.iter().any(|label| label.is_empty() || label.chars().count() > 80) {
        return Err(bad("Ogni opzione deve contenere da 1 a 80 caratteri"));
    }
    if labels.iter().map(|label| casefold(label)).collect::<HashSet<_>>().len() != labels.len() {
        return Err(bad("Le opzioni non possono essere duplicate"));
    }
    let color = format!("#{}", color.trim_start_matches('#'));

    let created = state.db.transaction(|tx| -> rusqlite::Result<Created> {
        if community && tx.query_row("SELECT 1 FROM polls WHERE is_community=1 AND active=1", [], |_| Ok(())).optional()?.is_some() {
            return Ok(Created::Conflict);
        }
        let position: SqlValue = tx.query_row("SELECT COALESCE(MAX(position),0)+1 FROM polls", [], |row| row.get(0))?;
        tx.execute(
            "INSERT INTO polls(name,emoji,color,position,active,is_community) VALUES(?,?,?,?,1,?)",
            params![name, emoji, color, position, i64::from(community)],
        )?;
        let poll_id = tx.last_insert_rowid();
        for label in &labels {
            tx.execute("INSERT INTO options(poll_id,label) VALUES(?,?)", params![poll_id, label])?;
        }
        Ok(Created::Done(poll_id))
    });
    let poll_id = match created {
        Ok(Created::Conflict) => return Err(json_error(StatusCode::CONFLICT, "Esiste già un sondaggio community attivo")),
        Ok(Created::Done(poll_id)) => poll_id,
        Err(e) if is_constraint(&e) => return Err(json_error(StatusCode::CONFLICT, "Esiste già un sondaggio con questo nome")),
        Err(e) => {
            error!("Creazione sondaggio dal sito non riuscita: {e}");
            return Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Creazione sondaggio non riuscita"));
        }
    };
    state.audit(&principal, "poll_created_from_site", Some(("poll", Some(poll_id))), json!({"name": name, "community": community}));
    publish_site_change("polls");
    if let Some(failure) = state.refresh_discord_poll(poll_id, "Sondaggio salvato ma non pubblicato").await {
        return Err(failure);
    }
    Ok(without_cache_control(StatusCode::CREATED, &json!({"ok": true, "id": poll_id})))
}

// --- modificare un sondaggio -------------------------------------------------------------------

async fn update_poll(State(state): State<AdminState>, headers: HeaderMap, Path(raw_id): Path<String>, body: Body) -> Response {
    update_poll_inner(&state, &headers, &raw_id, body).await.unwrap_or_else(|response| response)
}

async fn update_poll_inner(state: &AdminState, headers: &HeaderMap, raw_id: &str, body: Body) -> Reply {
    let Some(poll_id) = digits_param(raw_id) else {
        return Err(status_text(StatusCode::NOT_FOUND));
    };
    let principal = state.principal(headers, true).await?;
    let Value::Object(payload) = read_json(body, "Richiesta non valida").await? else {
        return Err(bad("Corpo JSON non valido"));
    };
    let name = match payload.get("name").and_then(Value::as_str).map(py_strip) {
        Some(name) if (1..=100).contains(&name.chars().count()) => name.to_string(),
        _ => return Err(bad("Nome sondaggio non valido")),
    };
    let Some(emoji) = payload.get("emoji").and_then(Value::as_str).filter(|emoji| valid_emoji(emoji)) else {
        return Err(bad("Emoji non valida"));
    };
    let Some(color) = payload.get("color").and_then(Value::as_str).filter(|color| valid_color(color)) else {
        return Err(bad("Colore non valido"));
    };
    let Some(position) = payload.get("position").and_then(is_int).filter(|position| (0..=999).contains(position)) else {
        return Err(bad("Posizione non valida"));
    };
    let (Some(Value::Bool(community)), Some(Value::Bool(active))) = (payload.get("is_community"), payload.get("active")) else {
        return Err(bad("Stato sondaggio non valido"));
    };
    let (community, active) = (*community, *active);
    // Un ID che il database non può rappresentare fa fallire Python con un errore non gestito.
    let Some(poll_id) = poll_id else {
        return Err(generic_500());
    };
    let Some(poll) = failed("Lettura del sondaggio", find_poll(&state.db, poll_id))? else {
        return Err(json_error(StatusCode::NOT_FOUND, "Sondaggio non trovato"));
    };
    if community && active {
        let other = failed(
            "Controllo del sondaggio community",
            state.db.with_conn(|conn| conn.query_row("SELECT 1 FROM polls WHERE is_community=1 AND active=1 AND id<>?", [poll_id], |_| Ok(())).optional()),
        )?;
        if other.is_some() {
            return Err(json_error(StatusCode::CONFLICT, "Esiste già un sondaggio community attivo"));
        }
    }
    let guard = state.ops.lock_poll(poll_id).await;
    let closes_at: Option<f64> = if !active {
        None
    } else if !poll.is_active() {
        Some(failed("Calcolo della scadenza", crate::schedule::poll_closing_time(&state.db, (state.clock)()))?)
    } else {
        poll.closes_at
    };
    let color = format!("#{}", color.trim_start_matches('#'));
    let updated = state.db.transaction(|tx| -> rusqlite::Result<()> {
        tx.execute(
            "UPDATE polls SET name=?,emoji=?,color=?,position=?,is_community=?,active=?,closes_at=? WHERE id=?",
            params![name, emoji, color, position, i64::from(community), i64::from(active), closes_at, poll_id],
        )?;
        Ok(())
    });
    match updated {
        Ok(()) => {}
        Err(e) if is_constraint(&e) => return Err(json_error(StatusCode::CONFLICT, "Esiste già un sondaggio con questo nome")),
        Err(e) => {
            error!("Aggiornamento sondaggio {poll_id} non riuscito: {e}");
            return Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Aggiornamento sondaggio non riuscito"));
        }
    }
    if poll.is_active() && !active {
        if let Some(message_id) = poll.message_id.as_deref().filter(|id| !id.is_empty()) {
            if let Some(channel) = state.poll_channel_id()?.flatten() {
                let Some(message) = int_arg_text(message_id).as_message() else {
                    return Err(generic_500());
                };
                match state.ops.delete_message(channel, message).await {
                    Ok(()) | Err(ApiError::NotFound) => {}
                    Err(e) => error!("Impossibile rimuovere il messaggio sondaggio {poll_id}: {e}"),
                }
            }
        }
    }
    // L'aggiornamento prende lo stesso lock (non rientrante): va chiamato dopo averlo rilasciato.
    drop(guard);
    state.audit(&principal, "poll_updated_from_site", Some(("poll", Some(poll_id))), json!({"name": name, "active": active}));
    publish_site_change("polls");
    if active {
        if let Err(e) = state.backend.refresh_poll(poll_id).await {
            error!("Sondaggio aggiornato ma refresh Discord fallito: {e}");
            return Err(json_error(StatusCode::BAD_GATEWAY, "Sondaggio aggiornato ma Discord non è stato aggiornato"));
        }
    }
    if let Err(e) = state.backend.refresh_dashboard().await {
        error!("Aggiornamento della dashboard non riuscito: {e}");
        return Err(generic_500());
    }
    Ok(ok_body())
}

impl IntArg {
    /// L'ID di un messaggio, se è un numero valido per Discord.
    fn as_message(self) -> Option<u64> {
        match self {
            IntArg::Int(id) => u64::try_from(id).ok(),
            _ => None,
        }
    }
}

// --- eliminare un sondaggio --------------------------------------------------------------------

async fn delete_poll(State(state): State<AdminState>, headers: HeaderMap, Path(raw_id): Path<String>) -> Response {
    delete_poll_inner(&state, &headers, &raw_id).await.unwrap_or_else(|response| response)
}

async fn delete_poll_inner(state: &AdminState, headers: &HeaderMap, raw_id: &str) -> Reply {
    let Some(poll_id) = digits_param(raw_id) else {
        return Err(status_text(StatusCode::NOT_FOUND));
    };
    let principal = state.principal(headers, true).await?;
    let Some(poll_id) = poll_id else {
        return Err(generic_500());
    };
    let Some(poll) = failed("Lettura del sondaggio", find_poll(&state.db, poll_id))? else {
        return Err(json_error(StatusCode::NOT_FOUND, "Sondaggio non trovato"));
    };
    let guard = state.ops.lock_poll(poll_id).await;
    if let Some(channel) = state.poll_channel_id()?.flatten() {
        if let Some(message_id) = poll.message_id.as_deref().filter(|id| !id.is_empty()) {
            let Some(message) = int_arg_text(message_id).as_message() else {
                return Err(generic_500());
            };
            match state.ops.delete_message(channel, message).await {
                Ok(()) | Err(ApiError::NotFound) => {}
                Err(e) => {
                    error!("Impossibile rimuovere il messaggio del sondaggio {poll_id}: {e}");
                    return Err(json_error(StatusCode::BAD_GATEWAY, "Non è stato possibile rimuovere il messaggio Discord"));
                }
            }
        }
    }
    let now = (state.clock)();
    let deleted = state.db.transaction(|tx| -> rusqlite::Result<()> {
        crate::suggestions::release_poll_suggestions(tx, poll_id, now)?;
        tx.execute("DELETE FROM polls WHERE id=?", [poll_id])?;
        tx.execute("DELETE FROM poll_message_claims WHERE poll_id=?", [poll_id])?;
        Ok(())
    });
    if let Err(e) = deleted {
        error!("Eliminazione sondaggio {poll_id} non riuscita: {e}");
        return Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Eliminazione sondaggio non riuscita"));
    }
    state.ops.forget_poll_lock(poll_id);
    if let Err(e) = state.backend.refresh_dashboard().await {
        error!("Aggiornamento della dashboard non riuscito: {e}");
        return Err(generic_500());
    }
    drop(guard);
    state.audit(&principal, "poll_deleted_from_site", Some(("poll", Some(poll_id))), json!({"name": poll.name}));
    publish_site_change("polls");
    publish_site_change("proposals");
    Ok(ok_body())
}

// --- opzioni -----------------------------------------------------------------------------------

async fn add_option(State(state): State<AdminState>, headers: HeaderMap, Path(raw_id): Path<String>, body: Body) -> Response {
    add_option_inner(&state, &headers, &raw_id, body).await.unwrap_or_else(|response| response)
}

async fn add_option_inner(state: &AdminState, headers: &HeaderMap, raw_id: &str, body: Body) -> Reply {
    let Some(poll_id) = digits_param(raw_id) else {
        return Err(status_text(StatusCode::NOT_FOUND));
    };
    let principal = state.principal(headers, false).await?;
    let payload = read_json(body, "Richiesta non valida").await?;
    let label = match payload.as_object().and_then(|payload| payload.get("label")).and_then(Value::as_str).map(py_strip) {
        Some(label) if (1..=80).contains(&label.chars().count()) => label.to_string(),
        _ => return Err(bad("L'opzione deve contenere da 1 a 80 caratteri")),
    };
    let Some(poll_id) = poll_id else {
        return Err(generic_500());
    };
    let exists = failed("Lettura del sondaggio", state.db.with_conn(|conn| conn.query_row("SELECT id FROM polls WHERE id=?", [poll_id], |_| Ok(())).optional()))?;
    if exists.is_none() {
        return Err(json_error(StatusCode::NOT_FOUND, "Sondaggio non trovato"));
    }
    if failed("Opzioni del sondaggio", poll_options(&state.db, poll_id))?.len() >= 25 {
        return Err(json_error(StatusCode::CONFLICT, "Il sondaggio ha già 25 opzioni"));
    }
    let duplicate = failed(
        "Controllo dell'opzione",
        state.db.with_conn(|conn| conn.query_row("SELECT 1 FROM options WHERE poll_id=? AND LOWER(label)=LOWER(?)", params![poll_id, label], |_| Ok(())).optional()),
    )?;
    if duplicate.is_some() {
        return Err(json_error(StatusCode::CONFLICT, "Opzione già presente"));
    }
    let inserted = state.db.transaction(|tx| -> rusqlite::Result<()> {
        tx.execute("INSERT INTO options(poll_id,label) VALUES(?,?)", params![poll_id, label])?;
        Ok(())
    });
    if let Err(e) = inserted {
        error!("Aggiunta opzione al sondaggio {poll_id} non riuscita: {e}");
        return Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Aggiunta opzione non riuscita"));
    }
    // L'audit prima dell'aggiornamento di Discord: l'opzione è già salvata.
    state.audit(&principal, "poll_option_added_from_site", Some(("poll", Some(poll_id))), json!({"label": label}));
    publish_site_change("polls");
    if let Some(failure) = state.refresh_discord_poll(poll_id, "Opzione salvata ma sondaggio Discord non aggiornato").await {
        return Err(failure);
    }
    Ok(without_cache_control(StatusCode::CREATED, &json!({"ok": true})))
}

async fn delete_option(State(state): State<AdminState>, headers: HeaderMap, Path((raw_poll, raw_option)): Path<(String, String)>) -> Response {
    delete_option_inner(&state, &headers, &raw_poll, &raw_option).await.unwrap_or_else(|response| response)
}

async fn delete_option_inner(state: &AdminState, headers: &HeaderMap, raw_poll: &str, raw_option: &str) -> Reply {
    let (Some(poll_id), Some(option_id)) = (digits_param(raw_poll), digits_param(raw_option)) else {
        return Err(status_text(StatusCode::NOT_FOUND));
    };
    let principal = state.principal(headers, false).await?;
    let (Some(poll_id), Some(option_id)) = (poll_id, option_id) else {
        return Err(generic_500());
    };
    let option: Option<(String, Option<i64>)> = failed(
        "Lettura dell'opzione",
        state.db.with_conn(|conn| {
            conn.query_row("SELECT label,suggestion_id FROM options WHERE id=? AND poll_id=?", params![option_id, poll_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()
        }),
    )?;
    let Some((label, suggestion_id)) = option else {
        return Err(json_error(StatusCode::NOT_FOUND, "Opzione non trovata"));
    };
    let suggestion_id = suggestion_id.filter(|id| *id != 0);
    let now = (state.clock)();
    let removed = state.db.transaction(|tx| -> rusqlite::Result<()> {
        if let Some(suggestion_id) = suggestion_id {
            let unpicked = tx.execute(
                "UPDATE suggestions SET status='pending',picked_by=NULL,picked_at=NULL WHERE id=? AND status='picked'",
                [suggestion_id],
            )?;
            if unpicked > 0 {
                revoke_ballot_entry_stats(tx, suggestion_id, crate::database::current_week(tx)?, now)?;
            }
        }
        tx.execute("DELETE FROM options WHERE id=?", [option_id])?;
        Ok(())
    });
    if let Err(e) = removed {
        error!("Rimozione opzione {option_id} non riuscita: {e}");
        return Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "Rimozione opzione non riuscita"));
    }
    // L'audit prima dell'aggiornamento di Discord: l'opzione è già rimossa.
    state.audit(&principal, "poll_option_removed_from_site", Some(("poll", Some(poll_id))), json!({"label": label}));
    publish_site_change("polls");
    if suggestion_id.is_some() {
        publish_site_change("proposals");
    }
    if let Some(failure) = state.refresh_discord_poll(poll_id, "Opzione rimossa ma sondaggio Discord non aggiornato").await {
        return Err(failure);
    }
    Ok(ok_body())
}

// --- pubblicare e provare i sondaggi -----------------------------------------------------------

async fn publish_polls(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    publish_polls_inner(&state, &headers).await.unwrap_or_else(|response| response)
}

async fn publish_polls_inner(state: &AdminState, headers: &HeaderMap) -> Reply {
    let principal = state.principal(headers, true).await?;
    if state.setting("poll_channel")?.is_none_or(|channel| channel.is_empty()) {
        return Err(bad("Configura prima il canale sondaggi"));
    }
    if let Err(e) = state.ops.refresh_all().await {
        error!("Pubblicazione sondaggi dal sito fallita: {e}");
        return Err(json_error(StatusCode::BAD_GATEWAY, "Discord non ha accettato la pubblicazione"));
    }
    state.audit(&principal, "polls_published_from_site", Some(("polls", None)), json!({}));
    publish_site_change("polls");
    Ok(ok_body())
}

async fn test_poll(State(state): State<AdminState>, headers: HeaderMap, body: Body) -> Response {
    test_poll_inner(&state, &headers, body).await.unwrap_or_else(|response| response)
}

async fn test_poll_inner(state: &AdminState, headers: &HeaderMap, body: Body) -> Reply {
    state.principal(headers, false).await?;
    let payload = read_json(body, "ID sondaggio non valido").await?;
    let poll_id = match payload.as_object().and_then(|payload| payload.get("poll_id")).map(int_arg) {
        Some(IntArg::Int(id)) => Some(id),
        Some(IntArg::Huge) => None,
        _ => return Err(bad("ID sondaggio non valido")),
    };
    let Some(channel_setting) = state.setting("poll_channel")?.filter(|channel| !channel.is_empty()) else {
        return Err(bad("Configura prima il canale sondaggi"));
    };
    let Some(poll_id) = poll_id else {
        return Err(generic_500());
    };
    let Some(poll) = failed("Lettura del sondaggio", find_poll(&state.db, poll_id))? else {
        return Err(json_error(StatusCode::NOT_FOUND, "Sondaggio non trovato"));
    };
    let unavailable = || json_error(StatusCode::SERVICE_UNAVAILABLE, "Canale sondaggi non disponibile");
    let channel_id = match int_arg_text(&channel_setting) {
        IntArg::Int(id) => u64::try_from(id).ok(),
        IntArg::Huge => None,
        IntArg::Invalid => return Err(generic_500()),
    };
    let Some(channel_id) = channel_id else {
        return Err(unavailable());
    };
    let guild_id = state.auth.configured_guild().map_or(0, |(guild_id, _)| guild_id);
    match state.backend.channel_kind(guild_id, channel_id).await {
        Ok(ChannelKind::Text) => {}
        Ok(_) => return Err(bad("Il canale sondaggi deve essere testuale")),
        Err(_) => return Err(unavailable()),
    }
    let mut spec = failed("Costruzione dell'anteprima", crate::cards::build_embed(&state.db, &poll, (state.clock)()))?;
    spec.title = Some(truncate_chars(&format!("🧪 Anteprima · {}", spec.title.as_deref().unwrap_or("")), 256));
    spec.footer = Some("Anteprima temporanea dal sito staff; non raccoglie voti.".into());
    let message_id = match state.ops.send_message(channel_id, MessagePayload::embed(spec.build())).await {
        Ok(message_id) => message_id,
        Err(e) => {
            error!("Anteprima sondaggio {poll_id} non inviata: {e}");
            return Err(json_error(StatusCode::BAD_GATEWAY, "Impossibile inviare l'anteprima Discord"));
        }
    };
    let ops = state.ops.clone();
    let delay = state.preview_delay;
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        if let Err(e) = ops.delete_message(channel_id, message_id).await {
            info!("Anteprima sondaggio {poll_id} non eliminata automaticamente: {e}");
        }
    });
    Ok(ok_body())
}

#[cfg(test)]
mod tests;
