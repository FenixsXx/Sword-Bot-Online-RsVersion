//! Dominio pubblico dell'overlay e stato dell'evento live, condivisi tra `cogs/live_event.py` e
//! `cogs/staff_tools.py`.
//!
//! In Python `LiveEventCog` legge `EVENT_OVERLAY_PUBLIC_URL`, ne ricava `origin` e tiene la sessione
//! live in `session`; `StaffToolsCog` li guarda da fuori con `bot.get_cog("LiveEventCog")`. Qui le due
//! cose stanno in [`LiveState`], che il futuro modulo dell'evento live riempirà.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use unicode_normalization::UnicodeNormalization;

/// Il messaggio quando `EVENT_OVERLAY_PUBLIC_URL` non è un'origine HTTPS.
pub const PUBLIC_URL_ERROR: &str = "EVENT_OVERLAY_PUBLIC_URL deve essere un'origine HTTPS, ad esempio https://overlay.example.it.";

// --- urlsplit --------------------------------------------------------------------------------

/// Le parti di un indirizzo come le restituisce `urllib.parse.urlsplit` di Python 3.12.
#[derive(Debug, PartialEq, Eq)]
struct SplitUrl {
    scheme: String,
    netloc: String,
    path: String,
    query: String,
    fragment: String,
}

/// `urlsplit` con le sue regole (anche le stranezze: spazi iniziali e `\t\r\n` ignorati, schema in
/// minuscolo, `[...]` solo per gli IPv6). `Err` dove Python solleva `ValueError`.
fn urlsplit(raw: &str) -> Result<SplitUrl, ()> {
    // Solo l'inizio: gli spazi in coda restano (Python: `lstrip`).
    let trimmed = raw.trim_start_matches(|c: char| c <= ' ');
    let mut rest: String = trimmed.chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();

    let mut scheme = String::new();
    if let Some(colon) = rest.find(':') {
        let first = rest.as_bytes()[0];
        if colon > 0 && first.is_ascii_alphabetic() && rest[..colon].bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
            scheme = rest[..colon].to_ascii_lowercase();
            rest = rest[colon + 1..].to_string();
        }
    }

    let mut netloc = String::new();
    if rest.starts_with("//") {
        let after = &rest[2..];
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = after[..end].to_string();
        rest = after[end..].to_string();
        let (open, close) = (netloc.contains('['), netloc.contains(']'));
        if open != close {
            return Err(());
        }
        if open {
            let inside = netloc.split_once('[').map_or("", |(_, tail)| tail);
            let inside = inside.split_once(']').map_or(inside, |(head, _)| head);
            check_bracketed_host(inside)?;
        }
    }

    let mut fragment = String::new();
    if let Some((head, tail)) = rest.split_once('#') {
        fragment = tail.to_string();
        rest = head.to_string();
    }
    let mut query = String::new();
    if let Some((head, tail)) = rest.split_once('?') {
        query = tail.to_string();
        rest = head.to_string();
    }
    check_netloc(&netloc)?;
    Ok(SplitUrl { scheme, netloc, path: rest, query, fragment })
}

/// `_check_bracketed_host`: tra parentesi quadre ci può stare un IPv6 (anche con zona) o un IPvFuture.
fn check_bracketed_host(host: &str) -> Result<(), ()> {
    if let Some(future) = host.strip_prefix('v') {
        // `\Av[a-fA-F0-9]+\..+\Z`
        let digits = future.bytes().take_while(u8::is_ascii_hexdigit).count();
        let after = &future[digits..];
        return if digits > 0 && after.starts_with('.') && after.len() > 1 { Ok(()) } else { Err(()) };
    }
    // `fe80::1%eth0`: la zona non può essere vuota né contenere un altro `%`.
    let address = match host.split_once('%') {
        Some((_, zone)) if zone.is_empty() || zone.contains('%') => return Err(()),
        Some((address, _)) => address,
        None => host,
    };
    address.parse::<std::net::Ipv6Addr>().map(|_| ()).map_err(|_| ())
}

/// `_checknetloc`: dopo la normalizzazione NFKC non devono comparire `/ ? # @ :` nuovi.
fn check_netloc(netloc: &str) -> Result<(), ()> {
    if netloc.is_ascii() {
        return Ok(());
    }
    let cleaned: String = netloc.chars().filter(|c| !matches!(c, '@' | ':' | '#' | '?')).collect();
    let normalized: String = cleaned.nfkc().collect();
    if cleaned == normalized {
        return Ok(());
    }
    if normalized.contains(['/', '?', '#', '@', ':']) { Err(()) } else { Ok(()) }
}

/// Il `netloc` di un indirizzo in minuscolo (`urlsplit(testo).netloc.lower()`), o `None` se Python
/// non riuscirebbe a leggerlo (`ValueError`).
pub(crate) fn origin_netloc(raw: &str) -> Option<String> {
    urlsplit(raw).ok().map(|parts| parts.netloc.to_lowercase())
}

// --- origine pubblica ------------------------------------------------------------------------

/// `EVENT_OVERLAY_PUBLIC_URL` e quello che se ne ricava (come in `LiveEventCog.__init__`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PublicUrl {
    /// Il valore dell'ambiente senza `/` finali.
    pub public_url: String,
    /// `https://dominio[:porta]`, vuota se non è configurato o non è valido.
    pub origin: String,
    pub websocket_origin: String,
    /// Il motivo per cui l'indirizzo non è valido.
    pub configuration_error: Option<String>,
}

impl PublicUrl {
    pub fn from_env() -> Self {
        Self::parse(&std::env::var("EVENT_OVERLAY_PUBLIC_URL").unwrap_or_default())
    }

    pub fn parse(raw: &str) -> Self {
        let public_url = raw.trim_end_matches('/').to_string();
        if public_url.is_empty() {
            return Self { public_url, ..Self::default() };
        }
        // Con un indirizzo che Python non riesce nemmeno a leggere (`ValueError`) il cog non si
        // avvierebbe; qui lo si tratta come un indirizzo non valido.
        let valid = urlsplit(&public_url).ok().filter(|parts| {
            let userinfo = parts.netloc.rsplit_once('@').map(|(info, _)| info);
            let (username, password) = userinfo.map_or(("", ""), |info| info.split_once(':').unwrap_or((info, "")));
            parts.scheme == "https"
                && !parts.netloc.is_empty()
                && parts.path.is_empty()
                && parts.query.is_empty()
                && parts.fragment.is_empty()
                && username.is_empty()
                && password.is_empty()
        });
        match valid {
            Some(parts) => Self {
                websocket_origin: format!("wss://{}", parts.netloc),
                origin: format!("https://{}", parts.netloc),
                public_url,
                configuration_error: None,
            },
            None => Self { public_url, configuration_error: Some(PUBLIC_URL_ERROR.to_string()), ..Self::default() },
        }
    }

    /// L'indirizzo dell'area staff del sito (`<origine>/staff/`), vuoto senza dominio.
    pub fn staff_site_url(&self) -> String {
        if self.origin.is_empty() { String::new() } else { format!("{}/staff/", self.origin) }
    }
}

// --- evento live -----------------------------------------------------------------------------

/// Quello che serve sapere della sessione live in corso (in Python il dizionario `session`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveSessionInfo {
    pub title: String,
    pub guild_id: u64,
    pub channel_id: u64,
    /// Il nome del canale vocale, come lo mostra l'overlay.
    pub channel_name: String,
    /// L'inizio dell'evento (ISO 8601 con fuso), come lo legge l'overlay.
    pub started_at: String,
}

/// Una persona nel canale vocale dell'evento.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceMember {
    pub id: u64,
    /// Il nome mostrato nel server (`display_name`).
    pub name: String,
    pub avatar_url: String,
}

/// Da dove l'overlay prende chi è nel canale e chi sta parlando: la sessione vocale (in Python
/// `session["channel"].members` e `session["sink"].active_levels()`).
pub trait VoiceSource: Send + Sync + std::fmt::Debug {
    /// Le persone collegate (bot esclusi).
    fn members(&self) -> Vec<VoiceMember>;
    /// Il volume di chi sta parlando ora, per utente. Chi non parla non compare.
    fn levels(&self) -> HashMap<u64, f64>;
}

#[derive(Debug, Clone)]
struct ActiveSession {
    info: LiveSessionInfo,
    source: Option<Arc<dyn VoiceSource>>,
}

/// Dominio pubblico e sessione live: il "cog" `LiveEventCog` visto da fuori.
#[derive(Debug)]
pub struct LiveState {
    pub public_url: PublicUrl,
    /// Genera la chiave dell'overlay (`secrets.token_urlsafe(32)`); nei test è una chiave fissa.
    pub token_generator: fn() -> String,
    /// I collegamenti aperti dalle pagine overlay.
    pub hub: Arc<crate::overlay_web::OverlayHub>,
    session: Mutex<Option<ActiveSession>>,
}

impl Default for LiveState {
    fn default() -> Self {
        Self::new(PublicUrl::default())
    }
}

impl LiveState {
    pub fn new(public_url: PublicUrl) -> Self {
        Self { public_url, token_generator: crate::live_event::new_token, hub: Arc::default(), session: Mutex::new(None) }
    }

    #[cfg(test)]
    pub fn with_token_generator(mut self, generator: fn() -> String) -> Self {
        self.token_generator = generator;
        self
    }

    pub fn session(&self) -> Option<LiveSessionInfo> {
        self.session.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().map(|active| active.info.clone())
    }

    /// Imposta (o toglie) la sessione, senza una sorgente vocale: l'overlay mostra una stanza vuota.
    pub fn set_session(&self, session: Option<LiveSessionInfo>) {
        *self.session.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = session.map(|info| ActiveSession { info, source: None });
    }

    /// Imposta la sessione con la sorgente di chi è nel canale e chi parla.
    pub fn start_session(&self, info: LiveSessionInfo, source: Arc<dyn VoiceSource>) {
        *self.session.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ActiveSession { info, source: Some(source) });
    }

    /// La sessione con chi è nel canale e chi sta parlando in questo istante.
    pub fn live_view(&self) -> Option<(LiveSessionInfo, Vec<VoiceMember>, HashMap<u64, f64>)> {
        let active = self.session.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()?;
        let (members, levels) = active.source.as_ref().map_or_else(|| (Vec::new(), HashMap::new()), |source| (source.members(), source.levels()));
        Some((active.info, members, levels))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn public_url_parsing_matches_python() {
        let golden: Value = serde_json::from_str(include_str!("golden/staff_tools_expected.json")).unwrap();
        let cases = golden["origin_cases"].as_array().unwrap();
        assert!(cases.len() > 40);
        for case in cases {
            let url = case["url"].as_str().unwrap();
            let parsed = PublicUrl::parse(url);
            if case.get("value_error").is_some() {
                // Python non riesce a leggere l'indirizzo: qui diventa un indirizzo non valido.
                assert_eq!(parsed.origin, "", "{url:?}");
                assert_eq!(parsed.configuration_error.as_deref(), Some(PUBLIC_URL_ERROR), "{url:?}");
                continue;
            }
            assert_eq!(parsed.public_url, case["public_url"].as_str().unwrap(), "{url:?}: indirizzo");
            assert_eq!(parsed.origin, case["origin"].as_str().unwrap(), "{url:?}: origine");
            assert_eq!(parsed.websocket_origin, case["websocket_origin"].as_str().unwrap(), "{url:?}: origine websocket");
            assert_eq!(parsed.configuration_error.as_deref(), case["configuration_error"].as_str(), "{url:?}: errore");
            assert_eq!(parsed.staff_site_url(), case["site_url"].as_str().unwrap(), "{url:?}: area staff");
        }
    }

    #[test]
    fn live_state_remembers_the_session() {
        let state = LiveState::new(PublicUrl::parse("https://x.it"));
        assert_eq!(state.session(), None);
        let info = LiveSessionInfo { title: "Serata".into(), guild_id: 1, channel_id: 2, ..Default::default() };
        state.set_session(Some(info.clone()));
        assert_eq!(state.session(), Some(info));
        state.set_session(None);
        assert_eq!(state.session(), None);
    }
}
