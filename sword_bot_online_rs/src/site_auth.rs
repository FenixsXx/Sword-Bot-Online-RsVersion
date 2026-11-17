//! Accesso al sito staff con Discord (`cogs/site_auth.py`): sessioni in cookie, controllo dei
//! permessi del membro nel server e login OAuth2 («Accedi con Discord»).
//!
//! Il sito non ha password: chi entra lo fa con il proprio account Discord, e per ogni richiesta si
//! rilegge dal server se è ancora amministratore o staff. Le sessioni durano 12 ore e stanno nella
//! tabella `web_sessions`; gli stati del login nella tabella `site_oauth_states`.

use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use poise::serenity_prelude::{self as serenity, GuildId, UserId};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use tracing::{error, warn};

use crate::database::{Database, DbResult};
use crate::live_event::python_int;
use crate::site_common::{
    add_cookie, constant_time_equals, cookie_value, delete_cookie, json_error, json_response, plain, query_params, redirect, set_cookie, staff_role_ids,
};
use crate::voice::BoxFuture;

pub const SESSION_COOKIE: &str = "sfo_site_session";
pub const STATE_COOKIE: &str = "sfo_discord_oauth_state";
/// Quanto dura una sessione.
pub const SESSION_SECONDS: u64 = 12 * 3600;
/// Quanto vale uno stato OAuth.
pub const OAUTH_STATE_TTL_SECONDS: f64 = 600.0;

const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";
const TOKEN_URL: &str = "https://discord.com/api/oauth2/token";
const PROFILE_URL: &str = "https://discord.com/api/users/@me";

/// Chi sta usando il sito.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitePrincipal {
    pub user_id: u64,
    pub display_name: String,
    pub is_admin: bool,
    pub is_staff: bool,
}

// --- server Discord ----------------------------------------------------------------------------

/// Il server configurato, come lo conosce il bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildFacts {
    pub owner_id: u64,
}

/// Un membro del server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteMember {
    pub display_name: String,
    /// Ha il permesso di amministratore (anche tramite i ruoli).
    pub administrator: bool,
    pub role_ids: Vec<u64>,
}

/// Ciò che le pagine di amministrazione mostrano del server: nome, membri, canali testuali e ruoli
/// che lo staff può assegnare.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuildOverview {
    pub name: String,
    pub member_count: u64,
    /// Canali testuali e di annunci (`guild.text_channels`), per posizione.
    pub text_channels: Vec<(u64, String)>,
    /// Ruoli esclusi `@everyone` e quelli gestiti da un'integrazione, nell'ordine di `guild.roles`.
    pub roles: Vec<(u64, String)>,
    /// L'avatar del bot (`bot.user.display_avatar.url`); vuoto se il bot non è ancora pronto.
    pub bot_avatar_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryError {
    /// Discord ha risposto 403 (`discord.Forbidden`).
    Forbidden,
    /// Qualsiasi altro errore di rete o di Discord (`discord.HTTPException`).
    Http(String),
}

/// Dove si guardano il server e i suoi membri (`bot.get_guild`, `guild.get_member`,
/// `guild.fetch_member`).
pub trait SiteDirectory: Send + Sync + 'static {
    /// Il server, se il bot lo conosce.
    fn guild(&self, guild_id: u64) -> Option<GuildFacts>;
    /// Il membro, dalla cache o da Discord. `Ok(None)` se non appartiene al server.
    fn member(&self, guild_id: u64, user_id: u64) -> BoxFuture<'_, Result<Option<SiteMember>, DirectoryError>>;
    /// Il nome mostrato di un membro, solo dalla cache (`guild.get_member`).
    fn cached_member_name(&self, _guild_id: u64, _user_id: u64) -> Option<String> {
        None
    }
    /// I canali vocali del server (non gli Stage), per posizione (`guild.voice_channels`).
    fn voice_channels(&self, _guild_id: u64) -> Vec<(u64, String)> {
        Vec::new()
    }
    /// Nome, canali testuali e ruoli del server, per le pagine di amministrazione.
    fn guild_overview(&self, _guild_id: u64) -> Option<GuildOverview> {
        None
    }
}

/// L'ordine di `guild.roles` in discord.py: per posizione e, a parità, dall'ID più alto al più basso.
pub(crate) fn role_order(position: u16, id: u64) -> (u16, std::cmp::Reverse<u64>) {
    (position, std::cmp::Reverse(id))
}

/// La directory vera: cache del gateway e, se il membro non c'è, una richiesta a Discord.
pub struct DiscordDirectory {
    cache: Arc<serenity::Cache>,
    http: Arc<serenity::Http>,
}

impl DiscordDirectory {
    pub fn new(cache: Arc<serenity::Cache>, http: Arc<serenity::Http>) -> Self {
        Self { cache, http }
    }
}

impl SiteDirectory for DiscordDirectory {
    fn guild(&self, guild_id: u64) -> Option<GuildFacts> {
        self.cache.guild(GuildId::new(guild_id)).map(|guild| GuildFacts { owner_id: guild.owner_id.get() })
    }

    fn cached_member_name(&self, guild_id: u64, user_id: u64) -> Option<String> {
        let guild = self.cache.guild(GuildId::new(guild_id))?;
        guild.members.get(&UserId::new(user_id)).map(|member| member.display_name().to_string())
    }

    fn voice_channels(&self, guild_id: u64) -> Vec<(u64, String)> {
        let Some(guild) = self.cache.guild(GuildId::new(guild_id)) else { return Vec::new() };
        let mut channels: Vec<(u16, u64, String)> = guild
            .channels
            .values()
            .filter(|channel| channel.kind == serenity::ChannelType::Voice)
            .map(|channel| (channel.position, channel.id.get(), channel.name.clone()))
            .collect();
        channels.sort_by_key(|(position, id, _)| (*position, *id));
        channels.into_iter().map(|(_, id, name)| (id, name)).collect()
    }

    fn guild_overview(&self, guild_id: u64) -> Option<GuildOverview> {
        let guild = self.cache.guild(GuildId::new(guild_id))?;
        let mut channels: Vec<(u16, u64, String)> = guild
            .channels
            .values()
            .filter(|channel| matches!(channel.kind, serenity::ChannelType::Text | serenity::ChannelType::News))
            .map(|channel| (channel.position, channel.id.get(), channel.name.clone()))
            .collect();
        channels.sort_by_key(|(position, id, _)| (*position, *id));
        let mut roles: Vec<(u16, u64, String)> = guild
            .roles
            .values()
            .filter(|role| role.id.get() != guild_id && !role.managed)
            .map(|role| (role.position, role.id.get(), role.name.clone()))
            .collect();
        roles.sort_by_key(|(position, id, _)| role_order(*position, *id));
        Some(GuildOverview {
            name: guild.name.clone(),
            member_count: guild.member_count,
            text_channels: channels.into_iter().map(|(_, id, name)| (id, name)).collect(),
            roles: roles.into_iter().map(|(_, id, name)| (id, name)).collect(),
            bot_avatar_url: self.cache.current_user().face(),
        })
    }

    fn member(&self, guild_id: u64, user_id: u64) -> BoxFuture<'_, Result<Option<SiteMember>, DirectoryError>> {
        Box::pin(async move {
            let guild_id = GuildId::new(guild_id);
            let user_id = UserId::new(user_id);
            let describe = |member: &serenity::Member, guild: &serenity::Guild| SiteMember {
                display_name: member.display_name().to_string(),
                administrator: guild.member_permissions(member).administrator(),
                role_ids: member.roles.iter().map(|role| role.get()).collect(),
            };
            if let Some(guild) = self.cache.guild(guild_id) {
                if let Some(member) = guild.members.get(&user_id) {
                    return Ok(Some(describe(member, &guild)));
                }
            }
            match guild_id.member(&self.http, user_id).await {
                Ok(member) => {
                    let Some(guild) = self.cache.guild(guild_id) else { return Err(DirectoryError::Http("server non in cache".into())) };
                    Ok(Some(describe(&member, &guild)))
                }
                Err(serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(response))) => match response.status_code.as_u16() {
                    404 => Ok(None),
                    403 => Err(DirectoryError::Forbidden),
                    _ => Err(DirectoryError::Http(format!("HTTP {}", response.status_code))),
                },
                Err(e) => Err(DirectoryError::Http(e.to_string())),
            }
        })
    }
}

/// La directory collegata a bot avviato: il server HTTP parte prima del gateway, e finché la directory
/// non c'è il server Discord risulta «non disponibile».
#[derive(Default)]
pub struct LateDirectory {
    inner: OnceLock<Arc<dyn SiteDirectory>>,
}

impl LateDirectory {
    pub fn set(&self, directory: Arc<dyn SiteDirectory>) {
        let _ = self.inner.set(directory);
    }
}

impl SiteDirectory for LateDirectory {
    fn guild(&self, guild_id: u64) -> Option<GuildFacts> {
        self.inner.get()?.guild(guild_id)
    }

    fn cached_member_name(&self, guild_id: u64, user_id: u64) -> Option<String> {
        self.inner.get()?.cached_member_name(guild_id, user_id)
    }

    fn voice_channels(&self, guild_id: u64) -> Vec<(u64, String)> {
        self.inner.get().map_or_else(Vec::new, |directory| directory.voice_channels(guild_id))
    }

    fn guild_overview(&self, guild_id: u64) -> Option<GuildOverview> {
        self.inner.get()?.guild_overview(guild_id)
    }

    fn member(&self, guild_id: u64, user_id: u64) -> BoxFuture<'_, Result<Option<SiteMember>, DirectoryError>> {
        match self.inner.get() {
            Some(directory) => directory.member(guild_id, user_id),
            None => Box::pin(async { Ok(None) }),
        }
    }
}

// --- OAuth2 ------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthError {
    /// Rete, timeout o risposta che non è JSON.
    Unavailable,
}

/// La risposta di Discord: lo stato HTTP e, solo se è 200, il JSON.
pub type OAuthReply = (u16, Option<Value>);

pub struct TokenRequest<'a> {
    pub client_id: &'a str,
    pub client_secret: &'a str,
    pub code: &'a str,
    pub redirect_uri: &'a str,
}

/// Le due chiamate a Discord del login.
pub trait OAuthProvider: Send + Sync + 'static {
    /// Scambia il codice con un access token.
    fn exchange_code<'a>(&'a self, request: TokenRequest<'a>) -> BoxFuture<'a, Result<OAuthReply, OAuthError>>;
    /// Legge il profilo di chi ha fatto il login.
    fn fetch_profile<'a>(&'a self, access_token: &'a str) -> BoxFuture<'a, Result<OAuthReply, OAuthError>>;
}

/// Il provider vero: Discord via HTTPS, con 10 secondi di tempo per chiamata.
pub struct DiscordOAuth {
    client: reqwest::Client,
}

impl DiscordOAuth {
    pub fn new() -> Self {
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap_or_default();
        Self { client }
    }

    async fn reply(response: reqwest::Response) -> Result<OAuthReply, OAuthError> {
        let status = response.status().as_u16();
        if status != 200 {
            return Ok((status, None));
        }
        response.json::<Value>().await.map(|body| (status, Some(body))).map_err(|_| OAuthError::Unavailable)
    }
}

impl OAuthProvider for DiscordOAuth {
    fn exchange_code<'a>(&'a self, request: TokenRequest<'a>) -> BoxFuture<'a, Result<OAuthReply, OAuthError>> {
        Box::pin(async move {
            let form = [
                ("client_id", request.client_id),
                ("client_secret", request.client_secret),
                ("grant_type", "authorization_code"),
                ("code", request.code),
                ("redirect_uri", request.redirect_uri),
            ];
            let response = self.client.post(TOKEN_URL).form(&form).send().await.map_err(|_| OAuthError::Unavailable)?;
            Self::reply(response).await
        })
    }

    fn fetch_profile<'a>(&'a self, access_token: &'a str) -> BoxFuture<'a, Result<OAuthReply, OAuthError>> {
        Box::pin(async move {
            let response = self.client.get(PROFILE_URL).bearer_auth(access_token).send().await.map_err(|_| OAuthError::Unavailable)?;
            Self::reply(response).await
        })
    }
}

// --- configurazione ----------------------------------------------------------------------------

/// Le variabili d'ambiente del login (`DISCORD_OAUTH_*` ed `EVENT_OVERLAY_PUBLIC_URL`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub secure_cookie: bool,
}

impl AuthConfig {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Self::from_values(&var("DISCORD_OAUTH_CLIENT_ID"), &var("DISCORD_OAUTH_CLIENT_SECRET"), std::env::var("DISCORD_OAUTH_REDIRECT_URI").ok(), &var("EVENT_OVERLAY_PUBLIC_URL"))
    }

    /// Dai valori delle variabili: l'indirizzo di ritorno predefinito è `<dominio>/staff/auth/callback`.
    pub fn from_values(client_id: &str, client_secret: &str, redirect_uri: Option<String>, public_url: &str) -> Self {
        let public_url = public_url.trim_end_matches('/');
        let redirect_uri = match redirect_uri {
            Some(value) => value,
            None if public_url.is_empty() => String::new(),
            None => format!("{public_url}/staff/auth/callback"),
        };
        Self::new(client_id.trim(), client_secret.trim(), redirect_uri.trim())
    }

    pub fn new(client_id: &str, client_secret: &str, redirect_uri: &str) -> Self {
        Self {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            redirect_uri: redirect_uri.to_string(),
            secure_cookie: redirect_uri.starts_with("https://"),
        }
    }

    /// Il login funziona solo con id, segreto e un indirizzo di ritorno HTTPS.
    pub fn oauth_configured(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty() && self.redirect_uri.starts_with("https://")
    }
}

/// `urllib.parse.quote_plus(testo, safe="")`.
fn quote_plus(text: &str) -> String {
    let mut quoted = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => quoted.push(char::from(byte)),
            b' ' => quoted.push('+'),
            other => quoted.push_str(&format!("%{other:02X}")),
        }
    }
    quoted
}

/// La query come la lascia `HTTPFound(url)` di aiohttp: `yarl` normalizza gli indirizzi e toglie la
/// codifica a `/ : ? @ , ! * ' ( ) $`, che nella query sono ammessi; `& = + # ; %` restano codificati.
fn authorize_query(pairs: &[(&str, &str)]) -> String {
    let encoded = pairs.iter().map(|(key, value)| format!("{}={}", quote_plus(key), quote_plus(value))).collect::<Vec<_>>().join("&");
    let bytes = encoded.as_bytes();
    let mut out = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let decoded = std::str::from_utf8(&bytes[index + 1..index + 3]).ok().and_then(|hex| u8::from_str_radix(hex, 16).ok());
            if let Some(byte) = decoded.filter(|byte| b"/:?@,!*'()$".contains(byte)) {
                out.push(char::from(byte));
                index += 3;
                continue;
            }
        }
        out.push(char::from(bytes[index]));
        index += 1;
    }
    out
}

/// `datetime.isoformat()` di un istante in UTC: `2027-03-05T12:00:00+00:00`, con i microsecondi solo
/// se non sono zero.
pub fn isoformat_utc(value: DateTime<Utc>) -> String {
    let micros = value.timestamp_subsec_micros();
    let base = value.format("%Y-%m-%dT%H:%M:%S");
    if micros == 0 { format!("{base}+00:00") } else { format!("{base}.{micros:06}+00:00") }
}

/// `datetime.fromisoformat(testo)` per le scadenze salvate. Senza fuso orario Python non riuscirebbe
/// nemmeno a confrontarle: qui sono scadenze non valide.
fn parse_expiry(text: &str) -> Option<DateTime<Utc>> {
    let normalized = if text.len() > 10 && text.as_bytes()[10] == b' ' { format!("{}T{}", &text[..10], &text[11..]) } else { text.to_string() };
    DateTime::parse_from_rfc3339(&normalized).ok().map(|instant| instant.with_timezone(&Utc))
}

/// `int(profilo["id"])` di Python.
fn profile_user_id(profile: &Value) -> Option<i64> {
    match profile.get("id")? {
        Value::String(text) => python_int(text),
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|float| float.trunc() as i64)),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

// --- autorizzazione ----------------------------------------------------------------------------

pub struct SiteAuth {
    db: Database,
    directory: Arc<dyn SiteDirectory>,
    oauth: Arc<dyn OAuthProvider>,
    config: AuthConfig,
    /// `GUILD_ID` dell'ambiente, se `guild_id` non è nelle impostazioni.
    fallback_guild_id: Option<u64>,
    clock: fn() -> DateTime<Utc>,
    token: fn() -> String,
}

fn random_token() -> String {
    crate::live_event::new_token()
}

impl SiteAuth {
    pub fn new(db: Database, directory: Arc<dyn SiteDirectory>, oauth: Arc<dyn OAuthProvider>, config: AuthConfig, fallback_guild_id: Option<u64>) -> Self {
        if let Err(e) = db.execute_batch("CREATE TABLE IF NOT EXISTS site_oauth_states (state TEXT PRIMARY KEY, created_at REAL NOT NULL)") {
            error!("Impossibile preparare la tabella degli stati OAuth: {e}");
        }
        Self { db, directory, oauth, config, fallback_guild_id, clock: Utc::now, token: random_token }
    }

    #[cfg(test)]
    pub(crate) fn with_sources(mut self, clock: fn() -> DateTime<Utc>, token: fn() -> String) -> Self {
        self.clock = clock;
        self.token = token;
        self
    }

    pub fn routes(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/staff/auth/login", get(login))
            .route("/staff/auth/callback", get(callback))
            .route("/api/site/auth/session", get(session_info))
            .route("/api/site/auth/logout", post(logout))
            .with_state(self.clone())
    }

    /// Il server configurato (`configured_guild`): `guild_id` nelle impostazioni, o `GUILD_ID`.
    pub(crate) fn configured_guild(&self) -> Option<(u64, GuildFacts)> {
        let setting = self.db.get_setting("guild_id").ok().flatten().filter(|value| !value.is_empty());
        let guild_id = match setting {
            Some(value) => u64::try_from(python_int(&value)?).ok()?,
            None => self.fallback_guild_id?,
        };
        Some((guild_id, self.directory.guild(guild_id)?))
    }

    fn delete_session(&self, session_id: &str) {
        if let Err(e) = self.db.transaction(|tx| tx.execute("DELETE FROM web_sessions WHERE session_id=?", [session_id])) {
            error!("Eliminazione della sessione web non riuscita: {e}");
        }
    }

    /// I permessi di un membro: deve essere nel server ed essere amministratore o staff.
    pub async fn member_permissions(&self, user_id: i64) -> Result<SitePrincipal, Response> {
        let Some((guild_id, guild)) = self.configured_guild() else {
            return Err(json_error(StatusCode::SERVICE_UNAVAILABLE, "Server Discord non disponibile o non configurato"));
        };
        let Ok(user) = u64::try_from(user_id) else {
            return Err(json_error(StatusCode::FORBIDDEN, "Devi appartenere al server Discord configurato"));
        };
        let member = match self.directory.member(guild_id, user).await {
            Ok(Some(member)) => member,
            Ok(None) => return Err(json_error(StatusCode::FORBIDDEN, "Devi appartenere al server Discord configurato")),
            Err(e) => {
                error!("Impossibile verificare i permessi Discord dell'utente {user}: {e:?}");
                return Err(json_error(StatusCode::BAD_GATEWAY, "Impossibile verificare i permessi Discord"));
            }
        };
        let is_admin = user == guild.owner_id || member.administrator;
        let staff_setting = self.db.get_setting("staff_roles").ok().flatten().unwrap_or_default();
        let Some(staff_roles) = staff_role_ids(&staff_setting) else {
            error!("Configurazione ruoli staff non valida.");
            return Err(json_error(StatusCode::SERVICE_UNAVAILABLE, "Configurazione dei ruoli staff non valida"));
        };
        let is_staff = is_admin || member.role_ids.iter().any(|role| i64::try_from(*role).is_ok_and(|role| staff_roles.contains(&role)));
        if !is_staff {
            return Err(json_error(StatusCode::FORBIDDEN, "Accesso riservato agli amministratori o allo staff"));
        }
        Ok(SitePrincipal { user_id: user, display_name: member.display_name, is_admin, is_staff })
    }

    /// Chi fa la richiesta, dal cookie di sessione (`authorize`); con `admin_only` serve un
    /// amministratore.
    pub async fn authorize(&self, headers: &HeaderMap, admin_only: bool) -> Result<SitePrincipal, Response> {
        let session_id = cookie_value(headers, SESSION_COOKIE).unwrap_or_default();
        if session_id.is_empty() {
            return Err(json_error(StatusCode::UNAUTHORIZED, "Accesso Discord richiesto"));
        }
        let now = (self.clock)();
        let session = self
            .db
            .with_conn(|conn| conn.query_row("SELECT user_id, expires_at FROM web_sessions WHERE session_id=?", [&session_id], |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<String>>(1)?))).optional());
        let session = match session {
            Ok(session) => session,
            Err(e) => {
                error!("Lettura della sessione web non riuscita: {e}");
                return Err(plain(StatusCode::INTERNAL_SERVER_ERROR, "500 Internal Server Error\n\nServer got itself in trouble"));
            }
        };
        let Some((user_id, expires_at)) = session else {
            return Err(json_error(StatusCode::UNAUTHORIZED, "Sessione scaduta, accedi di nuovo"));
        };
        let Some(expires_at) = expires_at.as_deref().and_then(parse_expiry) else {
            warn!("Scadenza sessione web non valida.");
            self.delete_session(&session_id);
            return Err(json_error(StatusCode::UNAUTHORIZED, "Sessione non valida, accedi di nuovo"));
        };
        if expires_at <= now {
            self.delete_session(&session_id);
            return Err(json_error(StatusCode::UNAUTHORIZED, "Sessione scaduta, accedi di nuovo"));
        }
        let principal = self.member_permissions(user_id.unwrap_or(-1)).await?;
        if admin_only && !principal.is_admin {
            return Err(json_error(StatusCode::FORBIDDEN, "Questa operazione è riservata agli amministratori Discord"));
        }
        Ok(principal)
    }

    fn login_error(reason: &str) -> Response {
        redirect(StatusCode::FOUND, &format!("/staff/?login_error={reason}"))
    }
}

// --- route -------------------------------------------------------------------------------------

async fn login(State(auth): State<Arc<SiteAuth>>) -> Response {
    if !auth.config.oauth_configured() {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "Configura DISCORD_OAUTH_CLIENT_ID e DISCORD_OAUTH_CLIENT_SECRET nell'hosting.");
    }
    let state = (auth.token)();
    let now = created_at((auth.clock)());
    let saved: DbResult<()> = auth.db.transaction(|tx| {
        tx.execute("DELETE FROM site_oauth_states WHERE created_at<?", [now - OAUTH_STATE_TTL_SECONDS])?;
        tx.execute("INSERT INTO site_oauth_states(state,created_at) VALUES(?,?)", params![state, now])?;
        Ok(())
    });
    if let Err(e) = saved {
        error!("Salvataggio dello stato OAuth non riuscito: {e}");
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "Login Discord non disponibile, riprova.");
    }
    let query = authorize_query(&[
        ("client_id", &auth.config.client_id),
        ("redirect_uri", &auth.config.redirect_uri),
        ("response_type", "code"),
        ("scope", "identify"),
        ("state", &state),
    ]);
    let mut response = redirect(StatusCode::FOUND, &format!("{AUTHORIZE_URL}?{query}"));
    add_cookie(&mut response, &set_cookie(STATE_COOKIE, &state, OAUTH_STATE_TTL_SECONDS as u64, auth.config.secure_cookie, "/staff/auth"));
    response
}

/// `datetime.timestamp()`: secondi con i microsecondi.
fn created_at(instant: DateTime<Utc>) -> f64 {
    instant.timestamp_micros() as f64 / 1e6
}

async fn callback(State(auth): State<Arc<SiteAuth>>, headers: HeaderMap, RawQuery(raw_query): RawQuery) -> Response {
    if !auth.config.oauth_configured() {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "Login Discord non configurato");
    }
    let query = query_params(raw_query);
    let state = query.get("state").cloned().unwrap_or_default();
    let cookie_state = cookie_value(&headers, STATE_COOKIE).unwrap_or_default();
    if state.is_empty() || cookie_state.is_empty() || !constant_time_equals(&state, &cookie_state) {
        return plain(StatusCode::BAD_REQUEST, "Richiesta OAuth non valida");
    }
    // Lettura e consumo dello stato nella stessa transazione: ogni stato si usa una volta sola.
    let stored: DbResult<Option<f64>> = auth.db.transaction(|tx| {
        let created: Option<f64> = tx.query_row("SELECT created_at FROM site_oauth_states WHERE state=?", [&state], |row| row.get(0)).optional()?;
        tx.execute("DELETE FROM site_oauth_states WHERE state=?", [&state])?;
        Ok(created)
    });
    let stored = match stored {
        Ok(stored) => stored,
        Err(e) => {
            error!("Verifica dello stato OAuth non riuscita: {e}");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "Login Discord non disponibile, riprova.");
        }
    };
    let now = (auth.clock)();
    if stored.is_none_or(|created| created_at(now) - created > OAUTH_STATE_TTL_SECONDS) {
        return plain(StatusCode::BAD_REQUEST, "Login scaduto, riprova.");
    }
    if query.get("error").is_some_and(|reason| !reason.is_empty()) {
        return SiteAuth::login_error("discord");
    }
    let code = query.get("code").cloned().unwrap_or_default();
    if code.is_empty() {
        return plain(StatusCode::BAD_REQUEST, "Codice OAuth mancante");
    }

    let request = TokenRequest { client_id: &auth.config.client_id, client_secret: &auth.config.client_secret, code: &code, redirect_uri: &auth.config.redirect_uri };
    let (status, body) = match auth.oauth.exchange_code(request).await {
        Ok(reply) => reply,
        Err(OAuthError::Unavailable) => return SiteAuth::login_error("unavailable"),
    };
    if status != 200 {
        error!("Scambio del codice OAuth Discord fallito (HTTP {status}).");
        return SiteAuth::login_error("discord");
    }
    let Some(access_token) = body.as_ref().and_then(|payload| payload.get("access_token")).and_then(Value::as_str).filter(|token| !token.is_empty()) else {
        error!("Discord OAuth non ha restituito un access token.");
        return SiteAuth::login_error("discord");
    };
    let (status, profile) = match auth.oauth.fetch_profile(access_token).await {
        Ok(reply) => reply,
        Err(OAuthError::Unavailable) => return SiteAuth::login_error("unavailable"),
    };
    if status != 200 {
        error!("Lettura del profilo OAuth Discord fallita (HTTP {status}).");
        return SiteAuth::login_error("discord");
    }
    let Some(user_id) = profile.as_ref().and_then(profile_user_id) else {
        error!("Discord OAuth ha restituito un profilo senza ID valido.");
        return SiteAuth::login_error("discord");
    };

    let Ok(principal) = auth.member_permissions(user_id).await else {
        return SiteAuth::login_error("forbidden");
    };
    let session_id = (auth.token)();
    let now = (auth.clock)();
    let expires_at = now + chrono::Duration::seconds(SESSION_SECONDS as i64);
    let saved: DbResult<()> = auth.db.transaction(|tx| {
        tx.execute("DELETE FROM web_sessions WHERE expires_at<?", [isoformat_utc(now)])?;
        tx.execute(
            "INSERT INTO web_sessions(session_id,user_id,display_name,created_at,expires_at) VALUES(?,?,?,?,?)",
            params![session_id, principal.user_id as i64, principal.display_name, isoformat_utc(now), isoformat_utc(expires_at)],
        )?;
        Ok(())
    });
    if let Err(e) = saved {
        error!("Creazione della sessione web non riuscita: {e}");
        return SiteAuth::login_error("unavailable");
    }
    let mut response = redirect(StatusCode::FOUND, "/staff/");
    add_cookie(&mut response, &set_cookie(SESSION_COOKIE, &session_id, SESSION_SECONDS, auth.config.secure_cookie, "/"));
    add_cookie(&mut response, &delete_cookie(STATE_COOKIE, "/staff/auth"));
    response
}

async fn session_info(State(auth): State<Arc<SiteAuth>>, headers: HeaderMap) -> Response {
    match auth.authorize(&headers, false).await {
        Err(response) => response,
        Ok(principal) => json_response(
            StatusCode::OK,
            &json!({
                "authenticated": true,
                "user_id": principal.user_id.to_string(),
                "display_name": principal.display_name,
                "is_admin": principal.is_admin,
                "is_staff": principal.is_staff,
            }),
        ),
    }
}

async fn logout(State(auth): State<Arc<SiteAuth>>, headers: HeaderMap) -> Response {
    if let Some(session_id) = cookie_value(&headers, SESSION_COOKIE).filter(|id| !id.is_empty()) {
        auth.delete_session(&session_id);
    }
    let mut response = json_response(StatusCode::OK, &json!({"ok": true}));
    add_cookie(&mut response, &delete_cookie(SESSION_COOKIE, "/"));
    response.into_response()
}

#[cfg(test)]
pub(crate) mod tests;
