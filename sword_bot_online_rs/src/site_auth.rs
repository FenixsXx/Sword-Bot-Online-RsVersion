//! Gestione dell'accesso al sito staff tramite Discord.
//!
//! Questo modulo gestisce l'autenticazione degli utenti che vogliono accedere al pannello di controllo
//! del bot (il "sito staff"). Invece di usare una classica coppia email/password, il bot utilizza
//! il sistema "Accedi con Discord" (OAuth2).
//!
//! ### Cos'è OAuth2?
//!
//! Immagina di voler entrare in un club privato (il sito staff del bot). Invece di avere una tua chiave,
//! il club accetta che tu mostri la tua tessera di un'organizzazione fidata (Discord).
//! 1. Tu clicchi su "Accedi con Discord".
//! 2. Il sito ti manda su Discord, dove confermi la tua identità.
//! 3. Discord manda un "codice" al bot.
//! 4. Il bot scambia quel codice con Discord per ricevere le tue informazioni (chi sei, in che server sei).
//! 5. Se il bot vede che sei un amministratore o un membro dello staff nel server Discord giusto, ti lascia entrare.
//!
//! ### Sessioni e Cookie
//!
//! Una volta che hai fatto l'accesso, il bot non vuole chiederti di rifarlo per ogni singola operazione.
//! Per questo crea una "sessione":
//! - Salva nel tuo browser un "cookie" (un piccolo frammento di testo) chiamato `sfo_site_session`.
//! - Questo cookie è come un "pass giornaliero": finché è valido (qui dura 12 ore), il bot ti riconosce.
//! - Le sessioni attive sono salvate nel database nella tabella `web_sessions`.

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

/// Nome del cookie che mantiene l'utente collegato al sito.
pub const SESSION_COOKIE: &str = "sfo_site_session";
/// Nome del cookie temporaneo usato durante la procedura di login per sicurezza (anti-contraffazione).
pub const STATE_COOKIE: &str = "sfo_discord_oauth_state";
/// Durata di una sessione di accesso (12 ore).
pub const SESSION_SECONDS: u64 = 12 * 3600;
/// Quanto tempo è valido il tentativo di login (10 minuti). Se non completi l'accesso entro questo tempo, devi ricominciare.
pub const OAUTH_STATE_TTL_SECONDS: f64 = 600.0;

// Indirizzi ufficiali di Discord per gestire l'identità.
const AUTHORIZE_URL: &str = "https://discord.com/oauth2/authorize";
const TOKEN_URL: &str = "https://discord.com/api/oauth2/token";
const PROFILE_URL: &str = "https://discord.com/api/users/@me";

/// Rappresenta l'utente che sta navigando sul sito.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SitePrincipal {
    /// L'ID numerico unico dell'utente su Discord.
    pub user_id: u64,
    /// Il nome che l'utente mostra sul server.
    pub display_name: String,
    /// Vero se l'utente è un amministratore del server Discord.
    pub is_admin: bool,
    /// Vero se l'utente ha un ruolo che lo qualifica come "staff" del bot.
    pub is_staff: bool,
}

// --- Informazioni sul server Discord -----------------------------------------------------------

/// Informazioni base sul server (Guild) Discord, necessarie per il sito.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildFacts {
    /// L'ID dell'utente che ha creato il server (il proprietario).
    pub owner_id: u64,
}

/// Rappresenta un utente all'interno del server Discord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteMember {
    pub display_name: String,
    /// Indica se l'utente ha poteri di amministratore nel server.
    pub administrator: bool,
    /// Elenco dei ruoli (ID numerici) assegnati a questo utente.
    pub role_ids: Vec<u64>,
}

/// Panoramica completa del server mostrata nelle pagine di amministrazione.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuildOverview {
    /// Nome del server (es. "Il mio Server Discord").
    pub name: String,
    /// Numero totale di persone nel server.
    pub member_count: u64,
    /// Elenco dei canali di testo (ID e nome) ordinati per posizione.
    pub text_channels: Vec<(u64, String)>,
    /// Elenco dei ruoli del server che lo staff può gestire.
    pub roles: Vec<(u64, String)>,
    /// L'immagine profilo del bot.
    pub bot_avatar_url: String,
}

/// Possibili errori quando si cercano informazioni nella directory di Discord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryError {
    /// Il bot non ha i permessi per vedere queste informazioni.
    Forbidden,
    /// Errore generico di comunicazione con i server di Discord.
    Http(String),
}

/// Una "Directory" è un sistema per trovare informazioni sugli utenti e sul server.
/// Questo "Trait" definisce quali azioni possiamo fare (astrazione).
pub trait SiteDirectory: Send + Sync + 'static {
    /// Trova informazioni sul server dato il suo ID.
    fn guild(&self, guild_id: u64) -> Option<GuildFacts>;
    /// Trova informazioni su un membro specifico del server.
    fn member(&self, guild_id: u64, user_id: u64) -> BoxFuture<'_, Result<Option<SiteMember>, DirectoryError>>;
    /// Recupera il nome di un membro dalla memoria veloce (cache), senza chiedere a Discord.
    fn cached_member_name(&self, _guild_id: u64, _user_id: u64) -> Option<String> {
        None
    }
    /// Elenca i canali vocali del server.
    fn voice_channels(&self, _guild_id: u64) -> Vec<(u64, String)> {
        Vec::new()
    }
    /// Crea una panoramica completa del server per il pannello di amministrazione.
    fn guild_overview(&self, _guild_id: u64) -> Option<GuildOverview> {
        None
    }
}

/// Definisce l'ordine in cui mostrare i ruoli (stesso ordine usato dall'app di Discord).
pub(crate) fn role_order(position: u16, id: u64) -> (u16, std::cmp::Reverse<u64>) {
    (position, std::cmp::Reverse(id))
}

/// Implementazione reale della Directory che interroga la cache e le API di Discord.
pub struct DiscordDirectory {
    /// La cache locale del bot (informazioni salvate in memoria).
    cache: Arc<serenity::Cache>,
    /// Il sistema per fare richieste dirette a Discord.
    http: Arc<serenity::Http>,
}

impl DiscordDirectory {
    pub fn new(cache: Arc<serenity::Cache>, http: Arc<serenity::Http>) -> Self {
        Self { cache, http }
    }
}

impl SiteDirectory for DiscordDirectory {
    fn guild(&self, guild_id: u64) -> Option<GuildFacts> {
        // Cerca il server nella cache.
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
        // Prende canali di testo e annunci.
        let mut channels: Vec<(u16, u64, String)> = guild
            .channels
            .values()
            .filter(|channel| matches!(channel.kind, serenity::ChannelType::Text | serenity::ChannelType::News))
            .map(|channel| (channel.position, channel.id.get(), channel.name.clone()))
            .collect();
        channels.sort_by_key(|(position, id, _)| (*position, *id));

        // Prende i ruoli gestibili (esclude quelli automatici delle app).
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

            // Prova prima a guardare in cache.
            if let Some(guild) = self.cache.guild(guild_id) {
                if let Some(member) = guild.members.get(&user_id) {
                    return Ok(Some(describe(member, &guild)));
                }
            }

            // Se non c'è in cache, chiede direttamente a Discord via internet.
            match guild_id.member(&self.http, user_id).await {
                Ok(member) => {
                    let Some(guild) = self.cache.guild(guild_id) else { return Err(DirectoryError::Http("server non in cache".into())) };
                    Ok(Some(describe(&member, &guild)))
                }
                Err(serenity::Error::Http(serenity::HttpError::UnsuccessfulRequest(response))) => match response.status_code.as_u16() {
                    404 => Ok(None), // L'utente non è nel server.
                    403 => Err(DirectoryError::Forbidden),
                    _ => Err(DirectoryError::Http(format!("HTTP {}", response.status_code))),
                },
                Err(e) => Err(DirectoryError::Http(e.to_string())),
            }
        })
    }
}

/// Una directory "pigra" che aspetta che il bot sia pronto prima di fornire dati.
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

// --- OAuth2: Il sistema di login "Accedi con Discord" ------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthError {
    /// Discord non risponde o la connessione è caduta.
    Unavailable,
}

/// Risposta tecnica da Discord: codice di stato (es. 200 OK) e i dati JSON ricevuti.
pub type OAuthReply = (u16, Option<Value>);

/// I dati necessari per scambiare il "codice" temporaneo di Discord con un "token" di accesso.
pub struct TokenRequest<'a> {
    pub client_id: &'a str,
    pub client_secret: &'a str,
    pub code: &'a str,
    pub redirect_uri: &'a str,
}

/// Definisce come il bot parla con Discord per il login.
pub trait OAuthProvider: Send + Sync + 'static {
    /// Chiede a Discord un "token" segreto usando il codice ricevuto dopo il login dell'utente.
    fn exchange_code<'a>(&'a self, request: TokenRequest<'a>) -> BoxFuture<'a, Result<OAuthReply, OAuthError>>;
    /// Chiede a Discord le informazioni personali dell'utente (nome, avatar, ID) usando il token.
    fn fetch_profile<'a>(&'a self, access_token: &'a str) -> BoxFuture<'a, Result<OAuthReply, OAuthError>>;
}

/// Implementazione reale che fa richieste HTTP ai server di Discord.
pub struct DiscordOAuth {
    client: reqwest::Client,
}

impl DiscordOAuth {
    pub fn new() -> Self {
        // Crea un "cliente" HTTP che aspetta al massimo 10 secondi per ogni risposta.
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().unwrap_or_default();
        Self { client }
    }

    /// Analizza la risposta ricevuta da Discord.
    async fn reply(response: reqwest::Response) -> Result<OAuthReply, OAuthError> {
        let status = response.status().as_u16();
        if status != 200 {
            return Ok((status, None));
        }
        // Prova a leggere il contenuto come JSON.
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
            // Invia i dati come se fosse un modulo compilato su una pagina web.
            let response = self.client.post(TOKEN_URL).form(&form).send().await.map_err(|_| OAuthError::Unavailable)?;
            Self::reply(response).await
        })
    }

    fn fetch_profile<'a>(&'a self, access_token: &'a str) -> BoxFuture<'a, Result<OAuthReply, OAuthError>> {
        Box::pin(async move {
            // Chiede il profilo usando il token di accesso per dimostrare chi è.
            let response = self.client.get(PROFILE_URL).bearer_auth(access_token).send().await.map_err(|_| OAuthError::Unavailable)?;
            Self::reply(response).await
        })
    }
}

// --- Configurazioni ----------------------------------------------------------------------------

/// Le chiavi e gli indirizzi necessari per far funzionare il login con Discord.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthConfig {
    /// ID dell'applicazione creata sul portale sviluppatori di Discord.
    pub client_id: String,
    /// Chiave segreta dell'applicazione (da non rivelare mai!).
    pub client_secret: String,
    /// L'indirizzo dove Discord deve rimandare l'utente dopo che ha accettato il login.
    pub redirect_uri: String,
    /// Se vero, i cookie verranno inviati solo su connessioni sicure (HTTPS).
    pub secure_cookie: bool,
}

impl AuthConfig {
    /// Legge le impostazioni dalle variabili d'ambiente.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Self::from_values(&var("DISCORD_OAUTH_CLIENT_ID"), &var("DISCORD_OAUTH_CLIENT_SECRET"), std::env::var("DISCORD_OAUTH_REDIRECT_URI").ok(), &var("EVENT_OVERLAY_PUBLIC_URL"))
    }

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

    /// Controlla se abbiamo tutte le informazioni per far funzionare il login.
    pub fn oauth_configured(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty() && self.redirect_uri.starts_with("https://")
    }
}

/// Codifica un testo in modo che sia sicuro da inserire in un indirizzo web (URL).
/// Esempio: uno spazio diventa '+'.
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

/// Costruisce la "query string" (la parte dopo il '?' in un URL) per Discord.
fn authorize_query(pairs: &[(&str, &str)]) -> String {
    let encoded = pairs.iter().map(|(key, value)| format!("{}={}", quote_plus(key), quote_plus(value))).collect::<Vec<_>>().join("&");
    let bytes = encoded.as_bytes();
    let mut out = String::new();
    let mut index = 0;
    while index < bytes.len() {
        // Ripristina alcuni caratteri per renderli leggibili pur rimanendo sicuri.
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

/// Formatta una data nel formato standard internazionale (ISO 8601).
pub fn isoformat_utc(value: DateTime<Utc>) -> String {
    let micros = value.timestamp_subsec_micros();
    let base = value.format("%Y-%m-%dT%H:%M:%S");
    if micros == 0 { format!("{base}+00:00") } else { format!("{base}.{micros:06}+00:00") }
}

/// Converte un testo in formato ISO in una data utilizzabile dal bot.
fn parse_expiry(text: &str) -> Option<DateTime<Utc>> {
    let normalized = if text.len() > 10 && text.as_bytes()[10] == b' ' { format!("{}T{}", &text[..10], &text[11..]) } else { text.to_string() };
    DateTime::parse_from_rfc3339(&normalized).ok().map(|instant| instant.with_timezone(&Utc))
}

/// Estrae l'ID utente dal profilo Discord, gestendo vari formati possibili (stringa, numero, ecc.).
fn profile_user_id(profile: &Value) -> Option<i64> {
    match profile.get("id")? {
        Value::String(text) => python_int(text),
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().map(|float| float.trunc() as i64)),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

// --- Logica principale di Autorizzazione ------------------------------------------------------

/// Il gestore centrale dell'autenticazione.
pub struct SiteAuth {
    db: Database,
    directory: Arc<dyn SiteDirectory>,
    oauth: Arc<dyn OAuthProvider>,
    config: AuthConfig,
    /// L'ID del server Discord da usare se non ne è stato impostato uno specifico.
    fallback_guild_id: Option<u64>,
    clock: fn() -> DateTime<Utc>,
    token: fn() -> String,
}

/// Genera un testo casuale e sicuro da usare come identificativo di sessione.
fn random_token() -> String {
    crate::live_event::new_token()
}

impl SiteAuth {
    pub fn new(db: Database, directory: Arc<dyn SiteDirectory>, oauth: Arc<dyn OAuthProvider>, config: AuthConfig, fallback_guild_id: Option<u64>) -> Self {
        // Prepara la tabella per gestire i tentativi di login.
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

    /// Definisce i percorsi (URL) relativi all'autenticazione.
    pub fn routes(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/staff/auth/login", get(login))      // Inizia il login.
            .route("/staff/auth/callback", get(callback))   // Ritorno da Discord.
            .route("/api/site/auth/session", get(session_info)) // Info sulla sessione attuale.
            .route("/api/site/auth/logout", post(logout))     // Esce dal sito.
            .with_state(self.clone())
    }

    /// Identifica qual è il server Discord di riferimento per il sito.
    pub(crate) fn configured_guild(&self) -> Option<(u64, GuildFacts)> {
        let setting = self.db.get_setting("guild_id").ok().flatten().filter(|value| !value.is_empty());
        let guild_id = match setting {
            Some(value) => u64::try_from(python_int(&value)?).ok()?,
            None => self.fallback_guild_id?,
        };
        Some((guild_id, self.directory.guild(guild_id)?))
    }

    /// Rimuove una sessione dal database (es. al logout o se scaduta).
    fn delete_session(&self, session_id: &str) {
        if let Err(e) = self.db.transaction(|tx| tx.execute("DELETE FROM web_sessions WHERE session_id=?", [session_id])) {
            error!("Eliminazione della sessione web non riuscita: {e}");
        }
    }

    /// Controlla se un utente ha i permessi per stare nel sito staff.
    /// Deve essere nel server configurato ed essere o amministratore o avere un ruolo staff.
    pub async fn member_permissions(&self, user_id: i64) -> Result<SitePrincipal, Response> {
        let Some((guild_id, guild)) = self.configured_guild() else {
            return Err(json_error(StatusCode::SERVICE_UNAVAILABLE, "Server Discord non disponibile o non configurato"));
        };
        let Ok(user) = u64::try_from(user_id) else {
            return Err(json_error(StatusCode::FORBIDDEN, "Devi appartenere al server Discord configurato"));
        };

        // Chiede informazioni sul membro alla directory.
        let member = match self.directory.member(guild_id, user).await {
            Ok(Some(member)) => member,
            Ok(None) => return Err(json_error(StatusCode::FORBIDDEN, "Devi appartenere al server Discord configurato")),
            Err(e) => {
                error!("Impossibile verificare i permessi Discord dell'utente {user}: {e:?}");
                return Err(json_error(StatusCode::BAD_GATEWAY, "Impossibile verificare i permessi Discord"));
            }
        };

        // Verifica se è proprietario o ha il permesso "Amministratore".
        let is_admin = user == guild.owner_id || member.administrator;

        // Verifica se ha uno dei ruoli considerati "Staff".
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

    /// Verifica chi sta facendo una richiesta web controllando il suo cookie di sessione.
    /// Se `admin_only` è vero, l'accesso è permesso solo ai pieni amministratori.
    pub async fn authorize(&self, headers: &HeaderMap, admin_only: bool) -> Result<SitePrincipal, Response> {
        // Cerca il cookie di sessione.
        let session_id = cookie_value(headers, SESSION_COOKIE).unwrap_or_default();
        if session_id.is_empty() {
            return Err(json_error(StatusCode::UNAUTHORIZED, "Accesso Discord richiesto"));
        }

        // Cerca la sessione nel database.
        let now = (self.clock)();
        let session = self
            .db
            .with_conn(|conn| conn.query_row("SELECT user_id, expires_at FROM web_sessions WHERE session_id=?", [&session_id], |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<String>>(1)?))).optional());

        let session = match session {
            Ok(session) => session,
            Err(e) => {
                error!("Lettura della sessione web non riuscita: {e}");
                return Err(plain(StatusCode::INTERNAL_SERVER_ERROR, "Errore interno del server"));
            }
        };

        let Some((user_id, expires_at)) = session else {
            return Err(json_error(StatusCode::UNAUTHORIZED, "Sessione scaduta, accedi di nuovo"));
        };

        // Verifica se la sessione è ancora valida cronologicamente.
        let Some(expires_at) = expires_at.as_deref().and_then(parse_expiry) else {
            warn!("Scadenza sessione web non valida.");
            self.delete_session(&session_id);
            return Err(json_error(StatusCode::UNAUTHORIZED, "Sessione non valida, accedi di nuovo"));
        };
        if expires_at <= now {
            self.delete_session(&session_id);
            return Err(json_error(StatusCode::UNAUTHORIZED, "Sessione scaduta, accedi di nuovo"));
        }

        // Ricarica i permessi da Discord (per sicurezza, se fossero cambiati nel frattempo).
        let principal = self.member_permissions(user_id.unwrap_or(-1)).await?;

        if admin_only && !principal.is_admin {
            return Err(json_error(StatusCode::FORBIDDEN, "Questa operazione è riservata agli amministratori Discord"));
        }

        Ok(principal)
    }

    /// Reindirizza l'utente alla pagina principale con un messaggio di errore.
    fn login_error(reason: &str) -> Response {
        redirect(StatusCode::FOUND, &format!("/staff/?login_error={reason}"))
    }
}

// --- Gestione effettiva delle richieste (Handlers) --------------------------------------------

/// Avvia la procedura di login mandando l'utente su Discord.
async fn login(State(auth): State<Arc<SiteAuth>>) -> Response {
    if !auth.config.oauth_configured() {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "Sistema di login non configurato correttamente.");
    }

    // Crea una chiave di sicurezza ("state") per questo tentativo di login.
    let state = (auth.token)();
    let now = created_at((auth.clock)());

    // Salva lo "state" nel database per verificarlo al ritorno.
    let saved: DbResult<()> = auth.db.transaction(|tx| {
        // Pulisce i vecchi tentativi scaduti.
        tx.execute("DELETE FROM site_oauth_states WHERE created_at<?", [now - OAUTH_STATE_TTL_SECONDS])?;
        tx.execute("INSERT INTO site_oauth_states(state,created_at) VALUES(?,?)", params![state, now])?;
        Ok(())
    });

    if let Err(e) = saved {
        error!("Salvataggio dello stato OAuth non riuscito: {e}");
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "Login Discord non disponibile, riprova.");
    }

    // Costruisce l'indirizzo di Discord dove mandare l'utente.
    let query = authorize_query(&[
        ("client_id", &auth.config.client_id),
        ("redirect_uri", &auth.config.redirect_uri),
        ("response_type", "code"),
        ("scope", "identify"), // Chiediamo solo l'identità base.
        ("state", &state),
    ]);

    let mut response = redirect(StatusCode::FOUND, &format!("{AUTHORIZE_URL}?{query}"));
    // Salva lo state anche in un cookie per il confronto finale.
    add_cookie(&mut response, &set_cookie(STATE_COOKIE, &state, OAUTH_STATE_TTL_SECONDS as u64, auth.config.secure_cookie, "/staff/auth"));
    response
}

/// Converte una data in un numero (secondi trascorsi dal 1970).
fn created_at(instant: DateTime<Utc>) -> f64 {
    instant.timestamp_micros() as f64 / 1e6
}

/// Gestisce il ritorno dell'utente da Discord.
async fn callback(State(auth): State<Arc<SiteAuth>>, headers: HeaderMap, RawQuery(raw_query): RawQuery) -> Response {
    if !auth.config.oauth_configured() {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "Login Discord non configurato");
    }

    let query = query_params(raw_query);
    let state = query.get("state").cloned().unwrap_or_default();
    let cookie_state = cookie_value(&headers, STATE_COOKIE).unwrap_or_default();

    // Verifica che lo "state" ricevuto sia lo stesso che avevamo inviato (protezione CSRF).
    if state.is_empty() || cookie_state.is_empty() || !constant_time_equals(&state, &cookie_state) {
        return plain(StatusCode::BAD_REQUEST, "Richiesta di login non valida o contraffatta");
    }

    // Verifica che lo "state" sia presente nel database e lo cancella (si può usare una volta sola).
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
        return plain(StatusCode::BAD_REQUEST, "Tempo per il login scaduto, riprova.");
    }

    if query.get("error").is_some_and(|reason| !reason.is_empty()) {
        return SiteAuth::login_error("discord");
    }

    let code = query.get("code").cloned().unwrap_or_default();
    if code.is_empty() {
        return plain(StatusCode::BAD_REQUEST, "Codice di autenticazione mancante");
    }

    // Scambia il codice con Discord per ottenere il token.
    let request = TokenRequest { client_id: &auth.config.client_id, client_secret: &auth.config.client_secret, code: &code, redirect_uri: &auth.config.redirect_uri };
    let (status, body) = match auth.oauth.exchange_code(request).await {
        Ok(reply) => reply,
        Err(OAuthError::Unavailable) => return SiteAuth::login_error("unavailable"),
    };

    if status != 200 {
        error!("Scambio del codice Discord fallito (Errore HTTP {status}).");
        return SiteAuth::login_error("discord");
    }

    let Some(access_token) = body.as_ref().and_then(|payload| payload.get("access_token")).and_then(Value::as_str).filter(|token| !token.is_empty()) else {
        error!("Discord non ha fornito un token di accesso.");
        return SiteAuth::login_error("discord");
    };

    // Chiede il profilo a Discord.
    let (status, profile) = match auth.oauth.fetch_profile(access_token).await {
        Ok(reply) => reply,
        Err(OAuthError::Unavailable) => return SiteAuth::login_error("unavailable"),
    };

    if status != 200 {
        error!("Lettura del profilo Discord fallita (Errore HTTP {status}).");
        return SiteAuth::login_error("discord");
    }

    let Some(user_id) = profile.as_ref().and_then(profile_user_id) else {
        error!("Impossibile trovare l'ID utente nel profilo Discord.");
        return SiteAuth::login_error("discord");
    };

    // Verifica se l'utente ha i permessi per entrare.
    let Ok(principal) = auth.member_permissions(user_id).await else {
        return SiteAuth::login_error("forbidden");
    };

    // Crea una sessione ufficiale.
    let session_id = (auth.token)();
    let now = (auth.clock)();
    let expires_at = now + chrono::Duration::seconds(SESSION_SECONDS as i64);

    let saved: DbResult<()> = auth.db.transaction(|tx| {
        // Pulisce vecchie sessioni scadute.
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

    // Manda l'utente nel pannello staff e gli dà il cookie di sessione.
    let mut response = redirect(StatusCode::FOUND, "/staff/");
    add_cookie(&mut response, &set_cookie(SESSION_COOKIE, &session_id, SESSION_SECONDS, auth.config.secure_cookie, "/"));
    // Cancella il cookie temporaneo usato per il login.
    add_cookie(&mut response, &delete_cookie(STATE_COOKIE, "/staff/auth"));
    response
}

/// Restituisce informazioni sulla sessione attuale dell'utente in formato JSON.
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

/// Chiude la sessione dell'utente (Logout).
async fn logout(State(auth): State<Arc<SiteAuth>>, headers: HeaderMap) -> Response {
    if let Some(session_id) = cookie_value(&headers, SESSION_COOKIE).filter(|id| !id.is_empty()) {
        auth.delete_session(&session_id);
    }
    let mut response = json_response(StatusCode::OK, &json!({"ok": true}));
    // Dice al browser di cancellare il cookie di sessione.
    add_cookie(&mut response, &delete_cookie(SESSION_COOKIE, "/"));
    response.into_response()
}

#[cfg(test)]
pub(crate) mod tests;
