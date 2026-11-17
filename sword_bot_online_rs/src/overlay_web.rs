//! L'overlay web di `cogs/live_event.py`: la pagina per OBS (`/overlay/{guild_id}`), il websocket
//! (`/ws/{guild_id}`) con cui la pagina riceve lo stato dell'evento e il ciclo che lo pubblica.
//!
//! La pagina si collega con la chiave che sta nel link (`#<chiave>`): il primo messaggio del
//! websocket è `{"token": "<chiave>"}`, controllato in tempo costante. Chi non lo manda entro 5
//! secondi, o manda una chiave sbagliata, viene chiuso con il codice 1008 che la pagina mostra come
//! «LINK NON VALIDO».

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

/// Quante pagine overlay possono essere collegate insieme.
pub const MAX_OVERLAY_CLIENTS: usize = 25;
/// Dopo quanto un collegamento che non manda la chiave viene chiuso.
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// Ogni quanto si manda un ping ai collegamenti (`heartbeat=20` di aiohttp).
const HEARTBEAT: Duration = Duration::from_secs(20);
/// Quanto può attendere l'invio a un collegamento prima di considerarlo troppo lento.
const SEND_TIMEOUT: Duration = Duration::from_secs(1);
/// La dimensione massima di un messaggio ricevuto (`max_msg_size=1024`).
const MAX_MESSAGE_SIZE: usize = 1024;

const OVERLAY_HTML: &str = include_str!("../templates/event_overlay.html");

// --- collegamenti ----------------------------------------------------------------------------

/// Cosa chiedere a un collegamento di mandare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    Text(String),
    Close(u16, &'static str),
}

/// Le pagine overlay collegate (in Python `clients` e `open_connections`).
#[derive(Debug, Default)]
pub struct OverlayHub {
    clients: Mutex<HashMap<u64, mpsc::Sender<Outgoing>>>,
    next_id: AtomicU64,
    open_connections: AtomicUsize,
}

/// Un posto occupato tra i collegamenti aperti: si libera quando va fuori ambito.
#[derive(Debug)]
pub struct ConnectionSlot(Arc<OverlayHub>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.open_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Toglie il collegamento dall'elenco quando termina, comunque termini.
pub(crate) struct Registration {
    hub: Arc<OverlayHub>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.hub.clients().remove(&self.id);
    }
}

impl OverlayHub {
    fn clients(&self) -> std::sync::MutexGuard<'_, HashMap<u64, mpsc::Sender<Outgoing>>> {
        self.clients.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Quanti collegamenti hanno mandato la chiave giusta.
    pub fn client_count(&self) -> usize {
        self.clients().len()
    }

    /// Quanti collegamenti sono aperti, anche se non hanno ancora mandato la chiave.
    pub fn open_connections(&self) -> usize {
        self.open_connections.load(Ordering::SeqCst)
    }

    /// Occupa un posto, se ce n'è uno libero.
    fn try_open(self: &Arc<Self>, limit: usize) -> Option<ConnectionSlot> {
        self.open_connections.try_update(Ordering::SeqCst, Ordering::SeqCst, |open| (open < limit).then_some(open + 1)).ok()?;
        Some(ConnectionSlot(self.clone()))
    }

    pub(crate) fn register(self: &Arc<Self>, sender: mpsc::Sender<Outgoing>) -> Registration {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.clients().insert(id, sender);
        Registration { hub: self.clone(), id }
    }

    /// Manda lo stato a tutti. Chi non riesce a stare al passo (coda piena o già chiuso) viene
    /// tolto dall'elenco e il suo collegamento si chiude con «Client troppo lento».
    pub fn broadcast(&self, text: &str) {
        self.clients().retain(|_, sender| sender.try_send(Outgoing::Text(text.to_string())).is_ok());
    }

    /// Chiude tutti i collegamenti con il codice e il motivo indicati (la chiave è stata cambiata).
    pub fn close_all(&self, code: u16, reason: &'static str) {
        let senders: Vec<_> = self.clients().drain().map(|(_, sender)| sender).collect();
        for sender in senders {
            let _ = sender.try_send(Outgoing::Close(code, reason));
        }
    }

    /// Dice a tutti che l'evento è finito e chiude i collegamenti (`finish_event`).
    pub fn event_ended(&self) {
        let senders: Vec<_> = self.clients().drain().map(|(_, sender)| sender).collect();
        for sender in senders {
            let _ = sender.try_send(Outgoing::Text(json!({"type": "status", "active": false}).to_string()));
            let _ = sender.try_send(Outgoing::Close(1000, "Evento terminato"));
        }
    }
}

// --- stato dell'evento -----------------------------------------------------------------------

/// `round(valore, 3)` di Python: arrotonda sul valore esatto del numero, non su `valore * 1000`.
fn round3(value: f64) -> f64 {
    format!("{value:.3}").parse().unwrap_or(value)
}

struct Participant {
    id: u64,
    name: String,
    folded: String,
    avatar: String,
    speaking: bool,
    volume: f64,
    level: &'static str,
    streak: i64,
    wins: i64,
}

/// `event_snapshot`: chi c'è nel canale, chi parla e con che volume, con livello e serie di ognuno.
/// Chi parla viene per primo, poi per volume decrescente e per nome.
pub fn event_snapshot(db: &Database, session: &LiveSessionInfo, members: &[VoiceMember], levels: &HashMap<u64, f64>) -> DbResult<Value> {
    let ids: Vec<i64> = members.iter().filter_map(|member| i64::try_from(member.id).ok()).collect();
    let (stats, streaks) = if ids.is_empty() {
        (HashMap::new(), HashMap::new())
    } else {
        db.with_conn(|conn| -> DbResult<_> {
            let placeholders = vec!["?"; ids.len()].join(",");
            let mut stats = HashMap::new();
            let mut statement = conn.prepare(&format!(
                "SELECT user_id, proposals_submitted, supports_given, supports_received, ballot_entries, winning_proposals \
                 FROM user_stats WHERE user_id IN ({placeholders})"
            ))?;
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
            let rows = statement.query_map(rusqlite::params_from_iter(ids.iter()), |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
            for row in rows {
                let (id, streak) = row?;
                streaks.insert(id, streak);
            }
            Ok((stats, streaks))
        })?
    };

    let mut participants: Vec<Participant> = members
        .iter()
        .map(|member| {
            let key = i64::try_from(member.id).unwrap_or(-1);
            let user_stats = stats.get(&key).copied().unwrap_or_default();
            let name: String = member.name.chars().take(80).collect();
            Participant {
                id: member.id,
                folded: casefold(&name),
                name,
                avatar: member.avatar_url.clone(),
                speaking: levels.contains_key(&member.id),
                volume: round3(levels.get(&member.id).copied().unwrap_or(0.0).min(1.0)),
                level: contributor_level(contributor_points(&user_stats)),
                streak: streaks.get(&key).copied().unwrap_or(0),
                wins: user_stats.winning_proposals,
            }
        })
        .collect();
    participants.sort_by(|a, b| {
        (!a.speaking).cmp(&!b.speaking).then_with(|| b.volume.total_cmp(&a.volume)).then_with(|| a.folded.cmp(&b.folded))
    });
    let main_speaker = participants.first().filter(|first| first.speaking).map(|first| first.id);
    let count = participants.len();
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

/// Lo stato da mandare ora: l'evento in corso, oppure «nessun evento attivo».
pub fn current_snapshot(state: &OverlayState) -> DbResult<Value> {
    match state.live.live_view() {
        None => Ok(json!({"type": "status", "active": false})),
        Some((session, members, levels)) => event_snapshot(&state.db, &session, &members, &levels),
    }
}

// --- route -----------------------------------------------------------------------------------

/// Tutto ciò che le route e il ciclo di pubblicazione devono conoscere.
#[derive(Clone)]
pub struct OverlayState {
    pub live: Arc<LiveState>,
    pub db: Database,
    /// Dopo quanto un collegamento senza chiave viene chiuso (5 secondi; più breve nei test).
    pub auth_timeout: Duration,
}

impl OverlayState {
    pub fn new(live: Arc<LiveState>, db: Database) -> Self {
        Self { live, db, auth_timeout: AUTH_TIMEOUT }
    }
}

pub fn routes(state: OverlayState) -> Router {
    Router::new().route("/overlay/{guild_id}", get(overlay_page)).route("/ws/{guild_id}", get(websocket)).with_state(state)
}

/// `_guild_matches`: l'ID nell'indirizzo è quello del server dell'evento. Come `int()` di Python,
/// accetta spazi, segno e cifre non latine.
fn guild_matches(db: &Database, requested: &str) -> bool {
    let configured = match db.get_setting("guild_id") {
        Ok(value) => value.unwrap_or_else(|| "0".to_string()),
        Err(e) => {
            error!("Impossibile leggere l'ID del server per l'overlay: {e}");
            return false;
        }
    };
    matches!((python_int(requested), python_int(&configured)), (Some(a), Some(b)) if a == b)
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "404: Not Found").into_response()
}

fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_bytes(value.as_bytes()).unwrap_or_else(|_| HeaderValue::from_static(""))
}

async fn overlay_page(State(state): State<OverlayState>, Path(guild_id): Path<String>) -> Response {
    if !guild_matches(&state.db, &guild_id) {
        return not_found();
    }
    let policy = format!(
        "default-src 'self'; img-src 'self' https://cdn.discordapp.com https://media.discordapp.net; \
         style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; connect-src 'self' {}; \
         base-uri 'none'; frame-ancestors *",
        state.live.public_url.websocket_origin
    );
    let mut response = (StatusCode::OK, OVERLAY_HTML).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::CONTENT_SECURITY_POLICY, header_value(&policy));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(HeaderName::from_static("x-content-type-options"), HeaderValue::from_static("nosniff"));
    response
}

async fn websocket(
    State(state): State<OverlayState>,
    Path(guild_id): Path<String>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if !guild_matches(&state.db, &guild_id) || state.live.session().is_none() {
        return not_found();
    }
    // Senza l'intestazione `Origin` (o con un'altra origine) non si entra: lo stato dell'evento non
    // si legge da pagine di altri siti.
    let origin = headers.get(header::ORIGIN).and_then(|value| value.to_str().ok());
    if origin != Some(state.live.public_url.origin.as_str()) {
        return (StatusCode::FORBIDDEN, "Origine non autorizzata").into_response();
    }
    let Some(slot) = state.live.hub.try_open(MAX_OVERLAY_CLIENTS) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Troppi overlay collegati").into_response();
    };
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    upgrade.max_message_size(MAX_MESSAGE_SIZE).on_upgrade(move |socket| serve_socket(socket, state, slot))
}

/// Il primo messaggio di un collegamento.
enum First {
    Text(String),
    NotText,
    TooBig,
    Gone,
}

async fn first_message(socket: &mut WebSocket) -> First {
    loop {
        match socket.recv().await {
            None | Some(Ok(Message::Close(_))) => return First::Gone,
            Some(Err(_)) => return First::TooBig,
            Some(Ok(Message::Text(text))) => return First::Text(text.to_string()),
            Some(Ok(Message::Binary(_))) => return First::NotText,
            // I ping e i pong li gestisce il protocollo: non contano come messaggio.
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
        }
    }
}

async fn send_close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let frame = Message::Close(Some(CloseFrame { code, reason: reason.into() }));
    let _ = timeout(SEND_TIMEOUT, socket.send(frame)).await;
}

/// La chiave mandata dalla pagina: `{"token": "<chiave>"}`. Un messaggio che non è un oggetto JSON
/// vale come una chiave vuota; una chiave che non è un testo non è valida.
fn supplied_token(text: &str) -> Option<String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(object)) => match object.get("token") {
            None => Some(String::new()),
            Some(Value::String(token)) => Some(token.clone()),
            Some(_) => None,
        },
        _ => Some(String::new()),
    }
}

/// Confronto in tempo costante tra la chiave mandata e quella vera. I caratteri non ASCII non
/// possono essere una chiave valida (sono tutte `base64url`).
fn token_matches(supplied: &str, expected: &str) -> bool {
    if !supplied.is_ascii() || !expected.is_ascii() || supplied.len() != expected.len() {
        return false;
    }
    supplied.bytes().zip(expected.bytes()).fold(0u8, |difference, (a, b)| difference | (a ^ b)) == 0
}

async fn serve_socket(mut socket: WebSocket, state: OverlayState, _slot: ConnectionSlot) {
    let first = match timeout(state.auth_timeout, first_message(&mut socket)).await {
        Err(_) => return send_close(&mut socket, 1008, "Autenticazione scaduta").await,
        Ok(first) => first,
    };
    let text = match first {
        First::Gone => return,
        First::TooBig => return send_close(&mut socket, 1009, "").await,
        First::NotText => return send_close(&mut socket, 1008, "Autenticazione richiesta").await,
        First::Text(text) => text,
    };
    let expected = match overlay_token_with(&state.db, state.live.token_generator) {
        Ok(token) => token,
        Err(e) => {
            error!("Impossibile leggere la chiave dell'overlay: {e}");
            return;
        }
    };
    if !supplied_token(&text).is_some_and(|supplied| token_matches(&supplied, &expected)) {
        return send_close(&mut socket, 1008, "Token non valido").await;
    }

    let (sender, mut receiver) = mpsc::channel(32);
    let _registration = state.live.hub.register(sender);
    match current_snapshot(&state) {
        Ok(snapshot) => {
            if !matches!(timeout(SEND_TIMEOUT, socket.send(Message::text(snapshot.to_string()))).await, Ok(Ok(()))) {
                return;
            }
        }
        Err(e) => {
            error!("Impossibile preparare lo stato iniziale dell'overlay: {e}");
            return;
        }
    }

    let mut heartbeat = interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut heard_since_ping = true;
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                None | Some(Ok(Message::Close(_))) => return,
                Some(Err(_)) => return send_close(&mut socket, 1009, "").await,
                Some(Ok(_)) => heard_since_ping = true,
            },
            outgoing = receiver.recv() => match outgoing {
                Some(Outgoing::Text(text)) => {
                    // Un collegamento che non riceve entro un secondo è troppo lento: si chiude.
                    if !matches!(timeout(SEND_TIMEOUT, socket.send(Message::text(text))).await, Ok(Ok(()))) {
                        return send_close(&mut socket, 1011, "Client troppo lento").await;
                    }
                }
                Some(Outgoing::Close(code, reason)) => return send_close(&mut socket, code, reason).await,
                None => return send_close(&mut socket, 1011, "Client troppo lento").await,
            },
            _ = heartbeat.tick() => {
                if !heard_since_ping {
                    warn!("Un overlay non risponde ai ping: chiudo il collegamento.");
                    return;
                }
                heard_since_ping = false;
                if !matches!(timeout(SEND_TIMEOUT, socket.send(Message::Ping(Bytes::new()))).await, Ok(Ok(()))) {
                    return;
                }
            }
        }
    }
}

// --- pubblicazione ---------------------------------------------------------------------------

/// Manda lo stato dell'evento alle pagine collegate (se ce ne sono). Il ciclo vero, con il conteggio
/// del parlato e il controllo della connessione, è in `live_service`: qui basta per le prove.
#[cfg(test)]
pub fn publish_once(state: &OverlayState) -> DbResult<()> {
    if state.live.hub.client_count() == 0 || state.live.session().is_none() {
        return Ok(());
    }
    let snapshot = current_snapshot(state)?;
    state.live.hub.broadcast(&snapshot.to_string());
    Ok(())
}

#[cfg(test)]
mod tests;
