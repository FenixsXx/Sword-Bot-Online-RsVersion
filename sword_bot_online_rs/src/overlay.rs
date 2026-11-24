//! Dominio pubblico dell'overlay e stato dell'evento live, condivisi tra `cogs/live_event.py` e
//! `cogs/staff_tools.py`.
//!
//! In Python `LiveEventCog` legge `EVENT_OVERLAY_PUBLIC_URL`, ne ricava `origin` e tiene la sessione
//! live in `session`; `StaffToolsCog` li guarda da fuori con `bot.get_cog("LiveEventCog")`. Qui le due
//! cose stanno in [`LiveState`], che il futuro modulo dell'evento live riempirà.
//!
//! --- SPIEGAZIONE PER NON PROGRAMMATORI ---
//!
//! **Cosa è l'Overlay:**
//! L'overlay è una pagina web che mostra in tempo reale chi sta parlando in un canale vocale Discord
//! durante un evento live. Si apre nel browser e si aggiorna via WebSocket.
//!
//! **Architettura (semplificata):**
//! - `PublicUrl`: Parsea e valida l'URL pubblico dell'overlay (es. `https://overlay.example.it`)
//! - `LiveSessionInfo`: Info sulla sessione live in corso (titolo, server, canale, ora inizio)
//! - `VoiceMember`: Una persona nel canale vocale (id, nome, avatar)
//! - `VoiceSource`: Trait (interfaccia) per ottenere membri e livelli audio da qualsiasi backend
//! - `LiveState`: Stato centrale che tiene la sessione attuale e serve i dati all'overlay via WebSocket
//!
//! **Compatibilità Python ↔ Rust:**
//! Il parsing dell'URL (`urlsplit`, `check_bracketed_host`, `check_netloc`) replica **esattamente**
//! il comportamento di `urllib.parse.urlsplit` di Python 3.12, incluse le sue "stranezze".
//! I test confrontano i risultati con un file JSON "golden" generato dal codice Python.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use unicode_normalization::UnicodeNormalization;

/// Il messaggio quando `EVENT_OVERLAY_PUBLIC_URL` non è un'origine HTTPS.
pub const PUBLIC_URL_ERROR: &str = "EVENT_OVERLAY_PUBLIC_URL deve essere un'origine HTTPS, ad esempio https://overlay.example.it.";

// --- urlsplit --------------------------------------------------------------------------------

/// Le parti di un indirizzo come le restituisce `urllib.parse.urlsplit` di Python 3.12.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa è `urlsplit`:**
/// Prende un URL come `"https://example.com:8080/path?query=1#frag"` e lo spezza in 5 parti:
/// - `scheme`: `"https"` (il protocollo)
/// - `netloc`: `"example.com:8080"` (network location: dominio + porta opzionale)
/// - `path`: `"/path"` (il percorso)
/// - `query`: `"query=1"` (i parametri dopo `?`)
/// - `fragment`: `"frag"` (l'ancora dopo `#`)
///
/// **Perché replicare Python:**
/// Il bot Python originale usa `urllib.parse.urlsplit`. Per avere lo stesso comportamento
/// (stessi URL accettati/rifiutati), dobbiamo copiare la logica **esatta**, inclusi edge case
/// e "bug" storici di Python.
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
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Algoritmo (semplificato):**
/// 1. Rimuove spazi/iniziali (`lstrip` in Python: caratteri <= spazio, include tab, newline)
/// 2. Rimuove `\t`, `\r`, `\n` **ovunque** nella stringa (Python lo fa!)
/// 3. Estrae lo schema: tutto prima di `:` se inizia con lettera e contiene solo alfanumerici/+/-/.
///    Lo schema diventa minuscolo.
/// 4. Se c'è `//` dopo lo schema, estrae `netloc` fino a `/`, `?`, `#` o fine stringa.
/// 5. Valida `netloc`: se ha `[...]`, deve essere IPv6 valido (vedi `check_bracketed_host`).
/// 6. Estrae `fragment` (dopo `#`), poi `query` (dopo `?`), il resto è `path`.
/// 7. Valida `netloc` con `check_netloc` (normalizzazione NFKC, caratteri proibiti).
///
/// **`Result<SplitUrl, ()>`:**
/// Ritorna `Ok(SplitUrl)` se tutto bene, `Err(())` (vuoto) se Python solleverebbe `ValueError`.
/// Usiamo `()` (tuple vuota) come tipo errore perché non serve dettaglio: o parse o no.
fn urlsplit(raw: &str) -> Result<SplitUrl, ()> {
    // Solo l'inizio: gli spazi in coda restano (Python: `lstrip`).
    let trimmed = raw.trim_start_matches(|c: char| c <= ' ');
    // Python rimuove \t \r \n ovunque nella stringa prima di parsare.
    let mut rest: String = trimmed.chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();

    let mut scheme = String::new();
    if let Some(colon) = rest.find(':') {
        let first = rest.as_bytes()[0];
        // Schema valido: inizia con lettera, poi solo alfanumerici + - .
        if colon > 0 && first.is_ascii_alphabetic() && rest[..colon].bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
            scheme = rest[..colon].to_ascii_lowercase();
            rest = rest[colon + 1..].to_string();
        }
    }

    let mut netloc = String::new();
    if rest.starts_with("//") {
        let after = &rest[2..];
        // netloc finisce a / ? # o fine stringa
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = after[..end].to_string();
        rest = after[end..].to_string();
        // Se ci sono parentesi quadre, devono essere bilanciate (IPv6)
        let (open, close) = (netloc.contains('['), netloc.contains(']'));
        if open != close {
            return Err(());
        }
        if open {
            // Estrae il contenuto tra [ e ]
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
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa controlla:**
/// Dentro `[...]` nell'URL (es. `[::1]`, `[fe80::1%eth0]`, `[v1.2.3]`) ci deve essere:
/// - Un indirizzo IPv6 standard (es. `::1`, `2001:db8::1`)
/// - O un IPv6 **con zona** (interfaccia di rete): `indirizzo%zona` (es. `fe80::1%eth0`)
///   - La zona non può essere vuota (`%` finale) né contenere un altro `%`
/// - O un **IPvFuture**: `v` + esadecimali + `.` + almeno un carattere (es. `v1a.foo`)
///   - Formato: `v[a-fA-F0-9]+\..+`
///
/// **Perché è importante:**
/// Gli URL con IPv6 usano le parentesi per separare l'indirizzo dalla porta:
/// `http://[::1]:8080/` vs `http://example.com:8080/`
fn check_bracketed_host(host: &str) -> Result<(), ()> {
    // Caso IPvFuture: inizia con 'v'
    if let Some(future) = host.strip_prefix('v') {
        // `\Av[a-fA-F0-9]+\..+\Z`: v + esadecimali + . + resto non vuoto
        let digits = future.bytes().take_while(u8::is_ascii_hexdigit).count();
        let after = &future[digits..];
        return if digits > 0 && after.starts_with('.') && after.len() > 1 { Ok(()) } else { Err(()) };
    }
    // Caso IPv6 (con o senza zona): `fe80::1%eth0`
    let address = match host.split_once('%') {
        Some((_, zone)) if zone.is_empty() || zone.contains('%') => return Err(()),
        Some((address, _)) => address,
        None => host,
    };
    // Prova a parsare come IPv6 standard
    address.parse::<std::net::Ipv6Addr>().map(|_| ()).map_err(|_| ())
}

/// `_checknetloc`: dopo la normalizzazione NFKC non devono comparire `/ ? # @ :` nuovi.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa è NFKC:**
/// Normalizzazione Unicode "Compatibility Decomposition followed by Canonical Composition".
/// Trasforma caratteri "compatibili" nella forma canonica. Esempi:
/// - `ﬁ` (ligatura fi, U+FB01) → `fi` (due caratteri normali)
/// - `①` (circlato, U+2460) → `1`
/// - `Ｈｅｌｌｏ` (fullwidth, U+FF28+) → `Hello`
///
/// **Perché serve qui:**
/// Un attaccante potrebbe usare caratteri Unicode "confondibili" per bypassare controlli.
/// Esempio: `example.com@evil.com` → se `@` viene normalizzato da qualcos'altro...
/// Python fa questo controllo dopo `urlsplit`. Noi replichiamo la logica esatta.
///
/// **Logica:**
/// 1. Se `netloc` è puro ASCII → OK (nessuna normalizzazione cambia ASCII)
/// 2. Altrimenti: rimuove `@`, `:`, `#`, `?` (caratteri che non dovrebbero esserci in netloc)
/// 3. Normalizza NFKC
/// 4. Se il risultato contiene `/`, `?`, `#`, `@`, `:` → ERRORE (normalizzazione ha creato pericoli)
/// 5. Se pulito == normalizzato → OK
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
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Funzione di comodo:**
/// Prende un URL grezzo, lo parsia con `urlsplit`, e ritorna solo il `netloc` in minuscolo.
/// Usato per confrontare origini (same-origin policy) in modo case-insensitive.
/// Ritorna `None` se l'URL è invalido (Python solleva ValueError).
pub(crate) fn origin_netloc(raw: &str) -> Option<String> {
    urlsplit(raw).ok().map(|parts| parts.netloc.to_lowercase())
}

// --- origine pubblica ------------------------------------------------------------------------

/// `EVENT_OVERLAY_PUBLIC_URL` e quello che se ne ricava (come in `LiveEventCog.__init__`).
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Campi:**
/// - `public_url`: Il valore della variabile d'ambiente, senza slash finali
/// - `origin`: `https://dominio[:porta]` (es. `https://overlay.example.it:8443`), vuoto se invalido
/// - `websocket_origin`: `wss://dominio[:porta]` (stesso host, protocollo WebSocket secure), vuoto se invalido
/// - `configuration_error`: `Some(messaggio)` se l'URL non è valido, `None` se OK
///
/// **Validazione (in `parse`):**
/// L'URL deve essere:
/// - Schema `https` (obbligatorio, niente `http`)
/// - Avere un `netloc` non vuoto (dominio obbligatorio)
/// - **Niente** path, query, fragment (deve essere solo l'origine: `https://dominio/`)
/// - **Niente** userinfo (`user:pass@dominio` non permitido)
///
/// **Derivazione:**
/// Da `https://dominio:porta` → `origin = https://dominio:porta`, `websocket_origin = wss://dominio:porta`
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
    /// Crea `PublicUrl` leggendo la variabile d'ambiente `EVENT_OVERLAY_PUBLIC_URL`.
    pub fn from_env() -> Self {
        Self::parse(&std::env::var("EVENT_OVERLAY_PUBLIC_URL").unwrap_or_default())
    }

    /// Parsea e valida un URL grezzo, producendo `PublicUrl`.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Passaggi:**
    /// 1. Rimuove slash finali (`trim_end_matches('/')`)
    /// 2. Se vuoto → ritorna default (tutto vuoto, nessun errore: opzionale)
    /// 3. Chiama `urlsplit` (replica Python). Se fallisce → invalido
    /// 4. Controlla: schema=https, netloc non vuoto, path/query/fragment vuoti, niente user:pass@
    /// 5. Se tutto OK → costruisce `origin` e `websocket_origin`
    /// 6. Altrimenti → ritorna struct con `configuration_error = Some(PUBLIC_URL_ERROR)`
    pub fn parse(raw: &str) -> Self {
        let public_url = raw.trim_end_matches('/').to_string();
        if public_url.is_empty() {
            return Self { public_url, ..Self::default() };
        }
        // Con un indirizzo che Python non riesce nemmeno a leggere (`ValueError`) il cog non si
        // avvierebbe; qui lo si tratta come un indirizzo non valido.
        let valid = urlsplit(&public_url).ok().filter(|parts| {
            // Estrae userinfo (parte prima di @ in netloc)
            let userinfo = parts.netloc.rsplit_once('@').map(|(info, _)| info);
            // Separa username:password se c'è
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
    ///
    /// Esempio: se `origin = "https://overlay.example.it"`, ritorna `"https://overlay.example.it/staff/"`
    pub fn staff_site_url(&self) -> String {
        if self.origin.is_empty() { String::new() } else { format!("{}/staff/", self.origin) }
    }
}

// --- evento live -----------------------------------------------------------------------------

/// Quello che serve sapere della sessione live in corso (in Python il dizionario `session`).
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Campi:**
/// - `title`: Titolo dell'evento (es. "Serata Community")
/// - `guild_id`: ID numerico del server Discord (guild)
/// - `channel_id`: ID numerico del canale vocale
/// - `channel_name`: Nome del canale vocale (per mostrarlo nell'overlay)
/// - `started_at`: Timestamp ISO 8601 con fuso orario (es. "2024-01-15T21:00:00+01:00")
///
/// **Deriva `Default`:** Permette `..Default::default()` per riempire solo i campi necessari nei test.
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
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Campi:**
/// - `id`: ID numerico Discord dell'utente (univoco globale)
/// - `name`: Nome mostrato nel server (`display_name`, può cambiare per server)
/// - `avatar_url`: URL completo dell'avatar (CDN Discord, es. `https://cdn.discordapp.com/avatars/...`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceMember {
    pub id: u64,
    /// Il nome mostrato nel server (`display_name`).
    pub name: String,
    pub avatar_url: String,
}

/// Da dove l'overlay prende chi è nel canale e chi sta parlando: la sessione vocale (in Python
/// `session["channel"].members` e `session["sink"].active_levels()`).
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Trait `VoiceSource` (interfaccia):**
/// Definisce il contratto che qualsiasi "sorgente vocale" deve implementare.
/// Così l'overlay non dipende da songbird, fake, o altro: usa solo questo trait.
///
/// **`Send + Sync + Debug`:**
/// - `Send`: Si può trasferire tra thread (i dati non hanno riferimenti non-thread-safe)
/// - `Sync`: Si può condividere tra thread (riferimenti &T sono thread-safe)
/// - `Debug`: Si può stampare con `{:?}` per log/debug
///
/// **Metodi:**
/// - `members()`: Lista di `VoiceMember` (chi è connesso al canale, bot esclusi)
/// - `levels()`: Mappa `user_id` → `volume` (solo chi sta parlando ora, volume 0.0-1.0 circa)
pub trait VoiceSource: Send + Sync + std::fmt::Debug {
    /// Le persone collegate (bot esclusi).
    fn members(&self) -> Vec<VoiceMember>;
    /// Il volume di chi sta parlando ora, per utente. Chi non parla non compare.
    fn levels(&self) -> HashMap<u64, f64>;
}

/// Sessione attiva interna: info + sorgente vocale opzionale.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Perché `Option<Arc<dyn VoiceSource>>`:**
/// - `Option`: La sessione può esistere senza sorgente vocale (es. appena creata, prima che il bot entri in vocale)
/// - `Arc`: Condivisione thread-safe (l'overlay web legge da thread diversi)
/// - `dyn VoiceSource`: Type erasure - nasconde il tipo concreto (SongbirdConnection, FakeConnection, ecc.)
#[derive(Debug, Clone)]
struct ActiveSession {
    info: LiveSessionInfo,
    source: Option<Arc<dyn VoiceSource>>,
}

/// Dominio pubblico e sessione live: il "cog" `LiveEventCog` visto da fuori.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **`LiveState` è il cuore dello stato live:**
/// Espone metodi per:
/// - Leggere l'URL pubblico (`public_url`)
/// - Generare token sicuri per l'overlay WebSocket (`token_generator`)
/// - Gestire le connessioni WebSocket aperte (`hub`)
/// - Impostare/leggere la sessione live corrente (`session`, `start_session`, `live_view`)
///
/// **Campi:**
/// - `public_url`: Configurazione URL overlay (parsata all'avvio)
/// - `token_generator`: Funzione che genera token URL-safe (32 byte). In produzione usa `secrets.token_urlsafe(32)`,
///   nei test una funzione fissa per determinismo.
/// - `hub`: Gestisce i WebSocket aperti dalle pagine overlay (broadcast aggiornamenti)
/// - `session`: `Mutex<Option<ActiveSession>>` - stato protetto da lock per accesso concorrente
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
    /// Crea un nuovo stato live con l'URL pubblico dato.
    ///
    /// Usa `crate::live_event::new_token` come generatore di token (produzione)
    /// e crea un `OverlayHub` vuoto.
    pub fn new(public_url: PublicUrl) -> Self {
        Self { public_url, token_generator: crate::live_event::new_token, hub: Arc::default(), session: Mutex::new(None) }
    }

    /// (Solo test) Sostituisce il generatore di token con uno personalizzato.
    #[cfg(test)]
    pub fn with_token_generator(mut self, generator: fn() -> String) -> Self {
        self.token_generator = generator;
        self
    }

    /// Ritorna le info della sessione corrente, se c'è.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **`lock().unwrap_or_else(|poisoned| poisoned.into_inner())`:**
    /// Pattern standard per `Mutex`: se il lock è "avvelenato" (un thread ha fatto panic tenendolo),
    /// recupera comunque i dati invece di bloccarsi. Nei test è sicuro.
    ///
    /// **`as_ref().map(|active| active.info.clone())`:**
    /// - `as_ref()`: Trasforma `&Option<T>` in `Option<&T>`
    /// - `map`: Se c'è, clona le info (owned)
    pub fn session(&self) -> Option<LiveSessionInfo> {
        self.session.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().map(|active| active.info.clone())
    }

    /// Imposta (o toglie) la sessione, senza una sorgente vocale: l'overlay mostra una stanza vuota.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Quando si usa:**
    /// - All'inizio evento: bot non ancora in vocale → stanza vuota
    /// - Fine evento: `set_session(None)` → pulisce tutto
    ///
    /// **`session.map(|info| ActiveSession { info, source: None })`:**
    /// Se `session` è `Some(info)`, crea `ActiveSession` con `source: None`.
    /// Se `None`, ritorna `None` (pulisce la sessione).
    pub fn set_session(&self, session: Option<LiveSessionInfo>) {
        *self.session.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = session.map(|info| ActiveSession { info, source: None });
    }

    /// Imposta la sessione con la sorgente di chi è nel canale e chi parla.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Quando si usa:**
    /// Il bot è entrato nel canale vocale, ha la connessione audio attiva.
    /// `source` è tipicamente `Arc<SongbirdConnection>` che implementa `VoiceSource`.
    pub fn start_session(&self, info: LiveSessionInfo, source: Arc<dyn VoiceSource>) {
        *self.session.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ActiveSession { info, source: Some(source) });
    }

    /// La sessione con chi è nel canale e chi sta parlando in questo istante.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Ritorna `Option<(LiveSessionInfo, Vec<VoiceMember>, HashMap<u64, f64>)>`:**
    /// - `LiveSessionInfo`: Info evento (titolo, server, canale, ora)
    /// - `Vec<VoiceMember>`: Lista persone nel canale
    /// - `HashMap<u64, f64>`: user_id → volume attuale (solo chi parla)
    ///
    /// **Logica:**
    /// 1. Locka la sessione, clona l'`ActiveSession` (economico: `Arc` + struct piccoli)
    /// 2. Se c'è `source`, chiama `members()` e `levels()` per dati freschi
    /// 3. Se non c'è `source`, ritorna vettori/vuoti (stanza vuota)
    /// 4. Il tutto in `Some(...)`, o `None` se non c'è sessione attiva
    ///
    /// **Perché `clone()` su `ActiveSession`:**
    /// `ActiveSession` contiene `Arc<dyn VoiceSource>` (puntatore) e `LiveSessionInfo` (String, u64).
    /// Clonare è economico (incrementa refcount dell'Arc, clona stringhe/numeri).
    /// Poi rilascia il lock **prima** di chiamare `source.members()` e `source.levels()`
    /// (che potrebbero lockare altri mutex: evita deadlock).
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
        // Carica i casi di test "golden" generati dal codice Python
        let golden: Value = serde_json::from_str(include_str!("golden/staff_tools_expected.json")).unwrap();
        let cases = golden["origin_cases"].as_array().unwrap();
        assert!(cases.len() > 40); // Almeno 40 casi di test
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
