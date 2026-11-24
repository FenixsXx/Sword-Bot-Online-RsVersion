//! Gestione dell'overlay web: la pagina per OBS e il WebSocket per lo stato degli eventi.
//!
//! Questo modulo si occupa di due cose principali:
//! 1. Servire la pagina HTML (`/overlay/{guild_id}`) che viene usata come "overlay" (sovraimpressione)
//!    per le dirette streaming (ad esempio, su OBS, Twitch, YouTube). Un overlay è un elemento grafico
//!    che appare sopra il video in diretta, mostrando informazioni in tempo reale.
//! 2. Gestire le connessioni WebSocket (`/ws/{guild_id}`) che permettono a queste pagine overlay
//!    di ricevere aggiornamenti in tempo reale sullo stato dell'evento (es. chi sta parlando, i volumi, i partecipanti).
//!
//! ### Cos'è un WebSocket?
//!
//! Immagina una connessione HTTP (quella normale, usata per caricare le pagine web) come una conversazione
//! "domanda-risposta": il tuo browser chiede una pagina, il server la manda e la conversazione finisce.
//!
//! Un WebSocket è come una "conversazione continua" o un filo diretto: una volta stabilita la connessione,
//! sia il browser (la pagina overlay) che il server (il bot) possono inviare messaggi in qualsiasi momento,
//! senza dover fare nuove richieste. Questo è fondamentale per gli aggiornamenti in tempo reale.
//!
//! ### Autenticazione dei client WebSocket
//!
//! Quando una pagina overlay si collega tramite WebSocket, deve dimostrare di essere autorizzata.
//! Questo avviene tramite una "chiave" (un token segreto) che si trova nel link della pagina overlay (es. `/overlay/12345#MIA_CHIAVE_SEGRETA`).
//!
//! Il primo messaggio che il client WebSocket deve inviare al bot è `{"token": "MIA_CHIAVE_SEGRETA"}`.
//!
//! - **Controllo di sicurezza**: Se il client non invia questo messaggio entro 5 secondi, o se la chiave è sbagliata,
//!   la connessione viene chiusa con un errore (codice 1008). La pagina mostrerà un messaggio come "LINK NON VALIDO"
//!   per informare l'utente del problema. Questo impedisce a persone non autorizzate di collegarsi e leggere lo stato dell'evento.
//!
//! In sintesi: questo modulo permette agli streamer di avere overlay interattivi e aggiornati in tempo reale
//! durante le loro dirette, con un sistema di sicurezza per garantire che solo gli utenti autorizzati possano accedere ai dati.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval_at, timeout};
use tracing::{error, warn};

use crate::database::{Database, DbResult};
use crate::formatting::casefold;
use crate::live_event::{overlay_token_with, python_int};
use crate::overlay::{LiveSessionInfo, LiveState, VoiceMember};
use crate::stats::{UserStats, contributor_level, contributor_points};

/// Quante pagine overlay possono essere collegate contemporaneamente.
/// Questo limita il numero di connessioni attive per evitare di sovraccaricare il server.
pub const MAX_OVERLAY_CLIENTS: usize = 25;
/// Quanto tempo ha un client WebSocket per inviare la chiave di autenticazione prima di essere disconnesso.
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// Ogni quanto il server invia un "ping" (un piccolo messaggio di controllo) ai client WebSocket
/// per verificare che la connessione sia ancora viva e che il client sia responsivo.
/// Questo è simile all'"heartbeat" di altri sistemi.
const HEARTBEAT: Duration = Duration::from_secs(20);
/// Il tempo massimo che il server attende per inviare un messaggio a un client WebSocket.
/// Se un client è troppo lento a ricevere, la connessione viene chiusa.
const SEND_TIMEOUT: Duration = Duration::from_secs(1);
/// La dimensione massima consentita per un messaggio ricevuto da un client WebSocket.
/// Questo previene attacchi o errori dovuti a messaggi troppo grandi.
const MAX_MESSAGE_SIZE: usize = 1024;

/// Il contenuto HTML della pagina overlay, incluso direttamente nel codice del bot.
/// "include_str!" legge il file `event_overlay.html` dalla cartella `templates`.
const OVERLAY_HTML: &str = include_str!("../templates/event_overlay.html");

// --- Gestione dei collegamenti WebSocket --------------------------------------------------------

/// Cosa chiedere a un collegamento di inviare.
/// Questo `enum` (tipo enumerato) definisce i possibili tipi di messaggi in uscita dal server al client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    /// Invia un messaggio di testo (JSON) al client.
    Text(String),
    /// Chiude la connessione WebSocket con un codice e un motivo specifici.
    Close(u16, &'static str),
}

/// `OverlayHub` gestisce tutti i client (le pagine overlay) attualmente connessi.
/// Immagina un "hub" come un centro di smistamento dove tutte le connessioni arrivano e vengono gestite.
/// (In Python, questo corrisponderebbe alle variabili `clients` e `open_connections`.)
#[derive(Debug, Default)]
pub struct OverlayHub {
    /// Una mappa (dizionario) che associa un ID unico a ogni client connesso e al suo "canale di invio".
    /// Un `Mutex` (Mutual Exclusion) serve a proteggere l'accesso a `clients` per evitare che più parti
    /// del programma cerchino di modificarlo contemporaneamente, causando errori.
    clients: Mutex<HashMap<u64, mpsc::Sender<Outgoing>>>,
    /// Un contatore atomico per assegnare un ID unico a ogni nuova connessione.
    /// `AtomicU64` significa che il conteggio può essere aggiornato in modo sicuro da più "thread" (parti del programma che girano in parallelo).
    next_id: AtomicU64,
    /// Un contatore atomico per tenere traccia del numero totale di connessioni WebSocket aperte.
    open_connections: AtomicUsize,
}

/// `ConnectionSlot` rappresenta un "posto" occupato tra le connessioni aperte.
/// Quando un `ConnectionSlot` viene creato, incrementa il contatore delle connessioni aperte.
/// Quando esce di scena (cioè non è più usato), rilascia il posto e decrementa il contatore.
#[derive(Debug)]
pub struct ConnectionSlot(Arc<OverlayHub>);

/// `impl Drop for ConnectionSlot` definisce cosa succede quando un `ConnectionSlot` viene "droppato" (distrutto).
/// Questo meccanismo garantisce che il conteggio delle connessioni aperte sia sempre accurato.
impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        // Decrementa il contatore delle connessioni aperte in modo sicuro (atomico).
        self.0.open_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `Registration` è una struttura temporanea che serve per registrare un nuovo client nell'hub.
/// Quando viene "droppata", si assicura che il client venga rimosso dalla lista.
pub(crate) struct Registration {
    hub: Arc<OverlayHub>,
    id: u64,
}

/// `impl Drop for Registration` definisce il comportamento alla distruzione di `Registration`.
/// Rimuove il client dall'elenco `clients` dell'hub.
impl Drop for Registration {
    fn drop(&mut self) {
        self.hub.clients().remove(&self.id);
    }
}

impl OverlayHub {
    /// Ottiene un "blocco" (lock) sul dizionario dei client.
    /// Questo permette di accedere e modificare la lista dei client in modo sicuro.
    fn clients(&self) -> std::sync::MutexGuard<'_, HashMap<u64, mpsc::Sender<Outgoing>>> {
        // `lock()` cerca di ottenere il blocco. Se fallisce (perché un altro thread ha "avvelenato" il Mutex),
        // recupera comunque i dati interni per evitare che il programma crashi.
        self.clients.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Restituisce il numero di client WebSocket che hanno completato l'autenticazione.
    pub fn client_count(&self) -> usize {
        self.clients().len()
    }

    /// Restituisce il numero totale di connessioni WebSocket aperte, inclusi quelli non ancora autenticati.
    pub fn open_connections(&self) -> usize {
        self.open_connections.load(Ordering::SeqCst)
    }

    /// Tenta di aprire un nuovo "slot" (posto) per una connessione.
    /// Se il numero di connessioni aperte raggiunge il limite `MAX_OVERLAY_CLIENTS`, restituisce `None`.
    fn try_open(self: &Arc<Self>, limit: usize) -> Option<ConnectionSlot> {
        // `try_update` tenta di aggiornare il contatore solo se la condizione `open < limit` è vera.
        self.open_connections.try_update(Ordering::SeqCst, Ordering::SeqCst, |open| (open < limit).then_some(open + 1)).ok()?;
        Some(ConnectionSlot(self.clone()))
    }

    /// Registra un nuovo "mittente" (sender) per un client WebSocket nell'hub.
    /// Un `sender` è un canale attraverso il quale il server può inviare messaggi al client.
    pub(crate) fn register(self: &Arc<Self>, sender: mpsc::Sender<Outgoing>) -> Registration {
        // Assegna un ID unico al nuovo client.
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        // Inserisce il nuovo client (ID e sender) nella mappa.
        self.clients().insert(id, sender);
        Registration { hub: self.clone(), id }
    }

    /// Invia lo stato dell'evento a tutti i client WebSocket connessi (una "trasmissione" o broadcast).
    /// Se un client non riesce a ricevere il messaggio (perché troppo lento o già chiuso),
    /// viene rimosso dalla lista e la sua connessione viene terminata con un messaggio di errore.
    pub fn broadcast(&self, text: &str) {
        // `retain` itera sui client e mantiene solo quelli per cui `try_send` ha successo.
        self.clients().retain(|_, sender| sender.try_send(Outgoing::Text(text.to_string())).is_ok());
    }

    /// Chiude tutte le connessioni WebSocket attive con un codice e un motivo specifici.
    /// Questo viene usato, ad esempio, quando la chiave dell'overlay viene cambiata e le vecchie connessioni non sono più valide.
    pub fn close_all(&self, code: u16, reason: &'static str) {
        // `drain()` rimuove tutti gli elementi dalla mappa, restituendoli.
        let senders: Vec<_> = self.clients().drain().map(|(_, sender)| sender).collect();
        for sender in senders {
            // Tenta di inviare un messaggio di chiusura a ciascun client.
            let _ = sender.try_send(Outgoing::Close(code, reason));
        }
    }

    /// Notifica a tutti i client che l'evento è terminato e chiude le loro connessioni.
    /// (Corrisponde alla funzione `finish_event` in Python).
    pub fn event_ended(&self) {
        let senders: Vec<_> = self.clients().drain().map(|(_, sender)| sender).collect();
        for sender in senders {
            // Invia prima un messaggio JSON che indica che l'evento non è più attivo.
            let _ = sender.try_send(Outgoing::Text(json!({"type": "status", "active": false}).to_string()));
            // Poi invia un messaggio di chiusura con il motivo "Evento terminato".
            let _ = sender.try_send(Outgoing::Close(1000, "Evento terminato"));
        }
    }
}

// --- Gestione dello stato dell'evento -----------------------------------------------------------

/// Arrotonda un numero decimale a 3 cifre dopo la virgola.
/// Simile alla funzione `round(valore, 3)` di Python, ma assicura che l'arrotondamento sia preciso.
fn round3(value: f64) -> f64 {
    // Formatta il numero come stringa con 3 decimali e poi lo riconverte in `f64`.
    format!("{value:.3}").parse().unwrap_or(value)
}

/// Rappresenta un partecipante a un evento in diretta.
/// Contiene tutte le informazioni necessarie per visualizzare un utente nell'overlay.
struct Participant {
    id: u64,
    name: String,
    folded: String, // Versione "piegata" del nome per confronti (es. per l'ordinamento insensibile alla maiuscola/minuscola).
    avatar: String, // URL dell'immagine avatar dell'utente.
    speaking: bool, // Indica se l'utente sta parlando attivamente.
    volume: f64,    // Il volume rilevato della voce dell'utente.
    level: &'static str, // Il livello di contributore dell'utente (es. "Principiante", "Esperto").
    streak: i64,    // La serie di vittorie o partecipazioni consecutive dell'utente.
    wins: i64,      // Il numero di proposte vincenti dell'utente.
}

/// Crea una "istantanea" (snapshot) dello stato attuale dell'evento.
/// Questo include chi è nel canale vocale, chi sta parlando, il loro volume e le loro statistiche.
/// I partecipanti vengono ordinati: chi parla prima, poi per volume decrescente e infine per nome.
pub fn event_snapshot(db: &Database, session: &LiveSessionInfo, members: &[VoiceMember], levels: &HashMap<u64, f64>) -> DbResult<Value> {
    // Estrae gli ID dei membri e li converte in `i64` per la query al database.
    let ids: Vec<i64> = members.iter().filter_map(|member| i64::try_from(member.id).ok()).collect();

    // Recupera le statistiche degli utenti e le serie di contributi dal database.
    let (stats, streaks) = if ids.is_empty() {
        // Se non ci sono partecipanti, restituisce mappe vuote.
        (HashMap::new(), HashMap::new())
    } else {
        // Esegue query al database per ottenere i dati.
        db.with_conn(|conn| -> DbResult<_> {
            let placeholders = vec!["?"; ids.len()].join(","); // Crea stringa di placeholder per la query SQL.
            let mut stats = HashMap::new();
            let mut statement = conn.prepare(&format!(
                "SELECT user_id, proposals_submitted, supports_given, supports_received, ballot_entries, winning_proposals \
                 FROM user_stats WHERE user_id IN ({placeholders})"
            ))?;
            // Mappa i risultati della query sulle statistiche utente.
            let rows = statement.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    UserStats {
                        proposals_submitted: row.get(1)?,
                        supports_given: row.get(2)?,
                        supports_received: row.get(3)?,
                        ballot_entries: row.get(4)?,
                        winning_proposals: row.get(5)?,
                    },
                ))
            })?;
            for row in rows {
                let (id, row_stats) = row?;
                stats.insert(id, row_stats);
            }
            let mut streaks = HashMap::new();
            let mut statement = conn.prepare(&format!("SELECT user_id, current_streak FROM user_streaks WHERE user_id IN ({placeholders})"))?;
            // Mappa i risultati della query sulle serie di contributi.
            let rows = statement.query_map(rusqlite::params_from_iter(ids.iter()), |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
            for row in rows {
                let (id, streak) = row?;
                streaks.insert(id, streak);
            }
            Ok((stats, streaks))
        })?
    };

    // Crea una lista di `Participant` combinando i dati vocali con le statistiche del database.
    let mut participants: Vec<Participant> = members
        .iter()
        .map(|member| {
            let key = i64::try_from(member.id).unwrap_or(-1);
            let user_stats = stats.get(&key).copied().unwrap_or_default();
            let name: String = member.name.chars().take(80).collect(); // Limita il nome a 80 caratteri.
            Participant {
                id: member.id,
                folded: casefold(&name), // Versione per ordinamento.
                name,
                avatar: member.avatar_url.clone(),
                speaking: levels.contains_key(&member.id), // L'utente sta parlando se ha un livello di volume.
                volume: round3(levels.get(&member.id).copied().unwrap_or(0.0).min(1.0)), // Volume, arrotondato e limitato a 1.0.
                level: contributor_level(contributor_points(&user_stats)), // Determina il livello di contributore.
                streak: streaks.get(&key).copied().unwrap_or(0),
                wins: user_stats.winning_proposals,
            }
        })
        .collect();

    // Ordina i partecipanti secondo le regole: chi parla per primo, poi volume, poi nome.
    participants.sort_by(|a, b| {
        (!a.speaking).cmp(&!b.speaking) // Chi parla prima.
            .then_with(|| b.volume.total_cmp(&a.volume)) // Poi per volume decrescente.
            .then_with(|| a.folded.cmp(&b.folded)) // Infine per nome (insensibile a maiuscole/minuscole).
    });

    // Identifica il "main speaker" (il principale oratore), se ce n'è uno.
    let main_speaker = participants.first().filter(|first| first.speaking).map(|first| first.id);
    let count = participants.len();

    // Converte la lista di `Participant` in un formato JSON adatto all'invio via WebSocket.
    let participants: Vec<Value> = participants
        .into_iter()
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.name,
                "avatar": p.avatar,
                "speaking": p.speaking,
                "volume": p.volume,
                "level": p.level,
                "streak": p.streak,
                "wins": p.wins,
            })
        })
        .collect();

    // Costruisce l'oggetto JSON finale che rappresenta l'istantanea dell'evento.
    Ok(json!({
        "type": "event",
        "active": true,
        "title": session.title,
        "channel": session.channel_name,
        "started_at": session.started_at,
        "participant_count": count,
        "participants": participants,
        "main_speaker_id": main_speaker,
    }))
}

/// Restituisce l'istantanea dello stato attuale dell'evento.
/// Se un evento è in corso, restituisce i dettagli; altrimenti, indica che non ci sono eventi attivi.
pub fn current_snapshot(state: &OverlayState) -> DbResult<Value> {
    match state.live.live_view() {
        None => Ok(json!({"type": "status", "active": false})), // Nessun evento attivo.
        Some((session, members, levels)) => event_snapshot(&state.db, &session, &members, &levels), // Evento attivo.
    }
}

// --- Gestione delle route web -------------------------------------------------------------------

/// Contiene tutte le informazioni necessarie per le route web e per il ciclo di pubblicazione.
/// Questa è lo "stato" condiviso tra le diverse parti del server web relative all'overlay.
#[derive(Clone)]
pub struct OverlayState {
    /// Riferimento allo stato generale degli eventi live. `Arc` (Atomic Reference Counted) permette
    /// di condividere in modo sicuro lo stesso stato tra più parti del programma.
    pub live: Arc<LiveState>,
    /// Il database per recuperare le informazioni necessarie (es. statistiche utenti).
    pub db: Database,
    /// Il timeout per l'autenticazione delle connessioni WebSocket.
    pub auth_timeout: Duration,
}

impl OverlayState {
    /// Crea una nuova istanza di `OverlayState`.
    pub fn new(live: Arc<LiveState>, db: Database) -> Self {
        Self { live, db, auth_timeout: AUTH_TIMEOUT }
    }
}

/// Definisce le "route" (i percorsi URL) gestiti da questo modulo.
/// Un `Router` è come una mappa che indica al server quali funzioni chiamare per ogni URL specifico.
pub fn routes(state: OverlayState) -> Router {
    Router::new()
        // La route per la pagina HTML dell'overlay: `/overlay/{guild_id}`
        .route("/overlay/{guild_id}", get(overlay_page))
        // La route per la connessione WebSocket: `/ws/{guild_id}`
        .route("/ws/{guild_id}", get(websocket))
        // Associa lo stato dell'overlay a tutte queste route, rendendolo disponibile alle funzioni che le gestiscono.
        .with_state(state)
}

/// Verifica se l'ID del "guild" (server Discord) richiesto corrisponde a quello configurato.
/// Questo è un controllo di sicurezza per assicurarsi che l'overlay venga richiesto per il server giusto.
/// `python_int()` è una funzione che converte stringhe in numeri interi in modo flessibile,
/// simile a come fa `int()` in Python (accetta spazi, segni e cifre non latine).
fn guild_matches(db: &Database, requested: &str) -> bool {
    // Recupera l'ID del server configurato dal database.
    let configured = match db.get_setting("guild_id") {
        Ok(value) => value.unwrap_or_else(|| "0".to_string()),
        Err(e) => {
            error!("Impossibile leggere l'ID del server per l'overlay: {e}");
            return false;
        }
    };
    // Confronta gli ID convertiti in interi.
    matches!((python_int(requested), python_int(&configured)), (Some(a), Some(b)) if a == b)
}

/// Crea una risposta HTTP standard per un errore "404 Not Found" (risorsa non trovata).
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "404: Not Found").into_response()
}

/// Converte una stringa in un `HeaderValue` per le intestazioni HTTP.
/// Le intestazioni HTTP sono informazioni aggiuntive inviate insieme alla risposta o alla richiesta.
fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_bytes(value.as_bytes()).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Gestisce la richiesta GET per la pagina HTML dell'overlay (`/overlay/{guild_id}`).
///
/// **Axum**: È un framework web per Rust che aiuta a costruire server web in modo efficiente.
/// `State(state)`: Inietta lo stato dell'overlay (OverlayState) nella funzione.
/// `Path(guild_id)`: Estrae l'ID del server Discord dall'URL (es. se l'URL è `/overlay/123`, `guild_id` sarà "123").
async fn overlay_page(State(state): State<OverlayState>, Path(guild_id): Path<String>) -> Response {
    // Verifica che l'ID del server richiesto sia valido.
    if !guild_matches(&state.db, &guild_id) {
        return not_found(); // Se non valido, restituisce 404.
    }

    // `Content-Security-Policy` (CSP): è una misura di sicurezza che dice al browser
    // da quali fonti può caricare risorse (script, immagini, stili, ecc.) per quella pagina.
    // Questo previene attacchi come lo "script injection".
    let policy = format!(
        "default-src 'self'; img-src 'self' https://cdn.discordapp.com https://media.discordapp.net; \
         style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; connect-src 'self' {}; \
         base-uri 'none'; frame-ancestors *",
        state.live.public_url.websocket_origin // Permette connessioni WebSocket dall'origine definita.
    );

    // Crea la risposta HTTP con il contenuto HTML dell'overlay.
    let mut response = (StatusCode::OK, OVERLAY_HTML).into_response();
    let headers = response.headers_mut();
    // Imposta varie intestazioni HTTP per la sicurezza e il controllo della cache.
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store")); // Non memorizzare la pagina in cache.
    headers.insert(header::CONTENT_SECURITY_POLICY, header_value(&policy));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer")); // Non inviare l'URL di riferimento.
    headers.insert(HeaderName::from_static("x-content-type-options"), HeaderValue::from_static("nosniff")); // Previene l'"indovinamento" del tipo di contenuto da parte del browser.
    response
}

/// Gestisce la richiesta di connessione WebSocket (`/ws/{guild_id}`).
///
/// `WebSocketUpgrade`: il meccanismo di Axum per "promuovere" una normale richiesta HTTP
/// a una connessione WebSocket persistente.
async fn websocket(
    State(state): State<OverlayState>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    // Controlla l'ID del server e se un evento è attivo.
    if !guild_matches(&state.db, &guild_id) || state.live.session().is_none() {
        return not_found();
    }

    // Verifica l'intestazione `Origin` per ragioni di sicurezza.
    // L'Origin è l'indirizzo del sito da cui proviene la richiesta. Permettiamo solo connessioni
    // dalla nostra stessa origine pubblica (`state.live.public_url.origin`).
    // Questo impedisce ad altri siti di collegarsi al nostro WebSocket.
    let origin = headers.get(header::ORIGIN).and_then(|value| value.to_str().ok());
    if origin != Some(state.live.public_url.origin.as_str()) {
        return (StatusCode::FORBIDDEN, "Origine non autorizzata").into_response();
    }

    // Tenta di occupare uno slot per la nuova connessione. Se troppe connessioni sono già aperte, rifiuta.
    let Some(slot) = state.live.hub.try_open(MAX_OVERLAY_CLIENTS) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Troppi overlay collegati").into_response(); // Errore 503 Service Unavailable.
    };

    // Gestisce l'errore se la richiesta di upgrade a WebSocket fallisce.
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };

    // Una volta che l'upgrade è riuscito, passa la connessione alla funzione `serve_socket`
    // che gestirà la comunicazione WebSocket effettiva.
    upgrade.max_message_size(MAX_MESSAGE_SIZE) // Imposta la dimensione massima dei messaggi.
           .on_upgrade(move |socket| serve_socket(socket, state, slot))
}

/// Rappresenta il primo messaggio ricevuto da una connessione WebSocket.
enum First {
    Text(String), // Un messaggio di testo.
    NotText,      // Un messaggio non di testo (es. binario).
    TooBig,       // Il messaggio è troppo grande.
    Gone,         // La connessione è stata chiusa o non ha inviato nulla.
}

/// Attende e legge il primo messaggio da una connessione WebSocket.
/// Questa funzione è fondamentale per l'autenticazione iniziale.
async fn first_message(socket: &mut WebSocket) -> First {
    loop {
        match socket.recv().await {
            None | Some(Ok(Message::Close(_))) => return First::Gone, // Connessione chiusa.
            Some(Err(_)) => return First::TooBig, // Errore di ricezione (probabilmente messaggio troppo grande).
            Some(Ok(Message::Text(text))) => return First::Text(text.to_string()), // Messaggio di testo.
            Some(Ok(Message::Binary(_))) => return First::NotText, // Messaggio binario non previsto.
            // I messaggi di Ping e Pong sono gestiti automaticamente dal protocollo WebSocket e non
            // contano come messaggi di contenuto, quindi li ignoriamo e continuiamo ad aspettare.
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
        }
    }
}

/// Invia un messaggio di chiusura a un client WebSocket.
async fn send_close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let frame = Message::Close(Some(CloseFrame { code, reason: reason.into() }));
    // Tenta di inviare il messaggio di chiusura, ma con un timeout breve.
    // Se il client non risponde in tempo, non blocchiamo il server indefinitamente.
    let _ = timeout(SEND_TIMEOUT, socket.send(frame)).await;
}

/// Estrae il token di autenticazione da un messaggio JSON.
/// Il messaggio deve essere nel formato `{"token": "<chiave>"}`.
///
/// - Se il messaggio non è un oggetto JSON valido o non contiene la chiave "token",
///   viene trattato come una chiave vuota.
/// - Se la chiave "token" esiste ma non è una stringa, il token non è valido (`None`).
fn supplied_token(text: &str) -> Option<String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(object)) => match object.get("token") {
            None => Some(String::new()), // Token mancante, trattato come vuoto.
            Some(Value::String(token)) => Some(token.clone()), // Token trovato e valido.
            Some(_) => None, // Token presente ma non è una stringa.
        },
        _ => Some(String::new()), // Il messaggio non è un oggetto JSON, trattato come token vuoto.
    }
}

/// Confronta il token fornito dal client con il token atteso dal server.
/// Il confronto avviene in "tempo costante" per prevenire attacchi di "timing attack" (dove
/// un attaccante può dedurre informazioni misurando il tempo di risposta).
/// I token devono essere ASCII (caratteri inglesi) perché sono generati in `base64url`.
fn token_matches(supplied: &str, expected: &str) -> bool {
    // Controlla lunghezza e tipo di caratteri prima del confronto byte per byte.
    if !supplied.is_ascii() || !expected.is_ascii() || supplied.len() != expected.len() {
        return false;
    }
    // Confronto in tempo costante usando XOR (OR esclusivo) per ogni byte.
    // Se la somma delle differenze è 0, i token sono uguali.
    supplied.bytes().zip(expected.bytes()).fold(0u8, |difference, (a, b)| difference | (a ^ b)) == 0
}

/// Gestisce la comunicazione effettiva su una singola connessione WebSocket.
/// Questo è il "loop" principale che riceve e invia messaggi al client.
async fn serve_socket(mut socket: WebSocket, state: OverlayState, _slot: ConnectionSlot) {
    // Attende il primo messaggio di autenticazione con un timeout.
    let first = match timeout(state.auth_timeout, first_message(&mut socket)).await {
        Err(_) => return send_close(&mut socket, 1008, "Autenticazione scaduta").await, // Timeout di autenticazione.
        Ok(first) => first,
    };

    // Controlla il tipo del primo messaggio.
    let text = match first {
        First::Gone => return, // Connessione chiusa prima dell'autenticazione.
        First::TooBig => return send_close(&mut socket, 1009, "").await, // Messaggio troppo grande.
        First::NotText => return send_close(&mut socket, 1008, "Autenticazione richiesta").await, // Messaggio non di testo.
        First::Text(text) => text, // Messaggio di testo, procediamo.
    };

    // Recupera il token atteso dal database.
    let expected = match overlay_token_with(&state.db, state.live.token_generator) {
        Ok(token) => token,
        Err(e) => {
            error!("Impossibile leggere la chiave dell'overlay: {e}");
            return;
        }
    };

    // Verifica che il token fornito dal client corrisponda a quello atteso.
    if !supplied_token(&text).is_some_and(|supplied| token_matches(&supplied, &expected)) {
        return send_close(&mut socket, 1008, "Token non valido").await; // Token non valido.
    }

    // Crea un canale per inviare messaggi a questo client specifico.
    let (sender, mut receiver) = mpsc::channel(32);
    // Registra il client nell'hub.
    let _registration = state.live.hub.register(sender);

    // Invia lo stato iniziale dell'evento al client appena connesso.
    match current_snapshot(&state) {
        Ok(snapshot) => {
            if !matches!(timeout(SEND_TIMEOUT, socket.send(Message::text(snapshot.to_string()))).await, Ok(Ok(()))) {
                return; // Errore nell'invio del primo messaggio.
            }
        }
        Err(e) => {
            error!("Impossibile preparare lo stato iniziale dell'overlay: {e}");
            return;
        }
    }

    // Configura un "heartbeat" (ping) periodico per mantenere viva la connessione.
    let mut heartbeat = interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay); // Ritarda il tick se il programma è occupato.
    let mut heard_since_ping = true; // Flag per sapere se abbiamo ricevuto qualcosa dal client dopo l'ultimo ping.

    // Loop principale di comunicazione WebSocket.
    loop {
        tokio::select! {
            // Ascolta i messaggi in arrivo dal client.
            incoming = socket.recv() => match incoming {
                None | Some(Ok(Message::Close(_))) => return, // Connessione chiusa dal client.
                Some(Err(_)) => return send_close(&mut socket, 1009, "").await, // Errore di ricezione.
                Some(Ok(_)) => heard_since_ping = true, // Ricevuto messaggio, il client è attivo.
            },
            // Ascolta i messaggi in uscita da inviare al client (dal broadcast dell'hub).
            outgoing = receiver.recv() => match outgoing {
                Some(Outgoing::Text(text)) => {
                    // Se l'invio fallisce o impiega troppo tempo, chiudiamo la connessione.
                    if !matches!(timeout(SEND_TIMEOUT, socket.send(Message::text(text))).await, Ok(Ok(()))) {
                        return send_close(&mut socket, 1011, "Client troppo lento").await; // Client non al passo.
                    }
                }
                Some(Outgoing::Close(code, reason)) => return send_close(&mut socket, code, reason).await, // Richiesta di chiusura.
                None => return send_close(&mut socket, 1011, "Client troppo lento").await, // Canale chiuso, client troppo lento.
            },
            // Gestisce i tick dell'heartbeat.
            _ = heartbeat.tick() => {
                if !heard_since_ping {
                    // Se non abbiamo sentito nulla dal client dopo l'ultimo ping, lo consideriamo disconnesso.
                    warn!("Un overlay non risponde ai ping: chiudo il collegamento.");
                    return;
                }
                heard_since_ping = false; // Reset del flag.
                // Invia un messaggio di Ping al client.
                if !matches!(timeout(SEND_TIMEOUT, socket.send(Message::Ping(Bytes::new()))).await, Ok(Ok(()))) {
                    return; // Errore nell'invio del ping.
                }
            }
        }
    }
}

// --- Funzioni di pubblicazione (per test) -------------------------------------------------------

/// Manda lo stato dell'evento alle pagine overlay collegate (se ce ne sono).
/// Questa funzione è usata principalmente nei test o per una pubblicazione "una tantum".
/// Il vero ciclo di aggiornamento con il conteggio di chi parla e il controllo delle connessioni
/// è gestito nel modulo `live_service`.
#[cfg(test)] // Questa sezione del codice è compilata solo quando si eseguono i test.
pub fn publish_once(state: &OverlayState) -> DbResult<()> {
    // Se non ci sono client connessi o nessun evento attivo, non c'è nulla da pubblicare.
    if state.live.hub.client_count() == 0 || state.live.session().is_none() {
        return Ok(());
    }
    let snapshot = current_snapshot(state)?; // Prende l'istantanea attuale dell'evento.
    state.live.hub.broadcast(&snapshot.to_string()); // La trasmette a tutti i client.
    Ok(())
}

#[cfg(test)]
mod tests;
