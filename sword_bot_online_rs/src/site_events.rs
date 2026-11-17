//! Aggiornamenti in tempo reale per il sito staff (WebSocket `/api/site/events`,
//! `cogs/site_events.py`).
//!
//! Il bot chiama [`SiteEventHub::publish`] ogni volta che cambiano dei dati (vedi `hooks`). L'hub non
//! blocca mai chi pubblica: raggruppa le raffiche (al massimo un messaggio per argomento ogni
//! secondo) e affida l'invio a un task per ogni collegamento, che ha la sua coda e viene chiuso se è
//! lento o se la sessione non è più valida.
//!
//! Messaggi inviati al browser:
//! - `{"type": "ready"}` subito dopo il collegamento;
//! - `{"type": "change", "topic": "<argomento>"}` quando cambiano i dati di quell'argomento.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout};
use tracing::{error, warn};

use crate::overlay::origin_netloc;
use crate::site_auth::SiteAuth;
use crate::site_common::json_error;

/// Gli argomenti su cui il bot può segnalare un cambiamento.
pub const TOPICS: [&str; 5] = ["polls", "proposals", "events", "reports", "settings"];
/// Il codice con cui si chiude un collegamento la cui sessione non è più valida.
pub const SESSION_EXPIRED_CLOSE_CODE: u16 = 4401;
const TOO_SLOW_CLOSE_CODE: u16 = 1013;
const GOING_AWAY: u16 = 1001;
const MESSAGE_TOO_BIG: u16 = 1009;
/// La dimensione massima di un messaggio ricevuto (`max_msg_size=1024`).
const MAX_MESSAGE_SIZE: usize = 1024;

#[derive(Debug, Clone, Copy)]
pub struct HubConfig {
    pub max_clients: usize,
    /// Intervallo minimo tra due messaggi dello stesso argomento.
    pub coalesce: Duration,
    pub send_timeout: Duration,
    pub heartbeat: Duration,
    pub queue_size: usize,
    /// Dopo quanto senza messaggi si controlla che la sessione sia ancora valida.
    pub revalidate: Duration,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            max_clients: 50,
            coalesce: Duration::from_secs(1),
            send_timeout: Duration::from_secs(5),
            heartbeat: Duration::from_secs(25),
            queue_size: 16,
            revalidate: Duration::from_secs(300),
        }
    }
}

struct Client {
    queue: mpsc::Sender<String>,
    close: watch::Sender<Option<(u16, &'static str)>>,
}

#[derive(Default)]
struct HubState {
    clients: HashMap<u64, Client>,
    next_id: u64,
    scheduled: HashSet<String>,
    last_sent: HashMap<String, Instant>,
}

pub struct SiteEventHub {
    config: HubConfig,
    auth: Arc<SiteAuth>,
    state: Mutex<HubState>,
    /// Posti occupati da collegamenti già autenticati (anche durante l'handshake).
    reserved: AtomicUsize,
}

/// Un posto occupato: si libera quando va fuori ambito.
struct Slot(Arc<SiteEventHub>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.reserved.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Toglie il collegamento dall'elenco quando termina.
struct Registration {
    hub: Arc<SiteEventHub>,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.hub.state().clients.remove(&self.id);
    }
}

/// `json.dumps({"type": "change", "topic": topic})` di Python.
fn change_message(topic: &str) -> String {
    format!("{{\"type\": \"change\", \"topic\": \"{topic}\"}}")
}

impl SiteEventHub {
    pub fn new(auth: Arc<SiteAuth>, config: HubConfig) -> Arc<Self> {
        Arc::new(Self { config, auth, state: Mutex::new(HubState::default()), reserved: AtomicUsize::new(0) })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Quanti collegamenti sono aperti.
    #[cfg(test)]
    pub fn client_count(&self) -> usize {
        self.reserved.load(Ordering::SeqCst)
    }

    pub fn routes(self: &Arc<Self>) -> Router {
        Router::new().route("/api/site/events", get(handle)).with_state(self.clone())
    }

    // --- pubblicazione (non bloccante) ---------------------------------------------------------

    /// Segnala che i dati di un argomento sono cambiati. Non aspetta niente: se non c'è nessuno
    /// collegato, o l'argomento è già in coda, non fa nulla.
    pub fn publish(self: &Arc<Self>, topic: &str) {
        if !TOPICS.contains(&topic) {
            warn!("Argomento di aggiornamento del sito sconosciuto: {topic:?}");
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // Nessun runtime attivo (es. uno script fuori dal bot): nessuno da avvisare.
            return;
        };
        let delay = {
            let mut state = self.state();
            if state.clients.is_empty() || state.scheduled.contains(topic) {
                return;
            }
            let now = Instant::now();
            let delay = state.last_sent.get(topic).map_or(Duration::ZERO, |last| (*last + self.config.coalesce).saturating_duration_since(now));
            state.scheduled.insert(topic.to_string());
            delay
        };
        let hub = self.clone();
        let topic = topic.to_string();
        runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            hub.flush(&topic);
        });
    }

    fn flush(&self, topic: &str) {
        let mut state = self.state();
        // Se nel frattempo l'hub si è fermato, l'argomento non è più in coda.
        if !state.scheduled.remove(topic) {
            return;
        }
        state.last_sent.insert(topic.to_string(), Instant::now());
        let message = change_message(topic);
        let mut slow = Vec::new();
        for (id, client) in &state.clients {
            if client.queue.try_send(message.clone()).is_err() {
                slow.push(*id);
            }
        }
        for id in slow {
            if let Some(client) = state.clients.remove(&id) {
                warn!("Collegamento live del sito troppo lento: chiuso.");
                let _ = client.close.send(Some((TOO_SLOW_CLOSE_CODE, "Client troppo lento")));
            }
        }
    }

    /// Ferma l'hub: nessun messaggio in sospeso, tutti i collegamenti chiusi (`shutdown`).
    pub fn shutdown(&self) {
        let clients: Vec<Client> = {
            let mut state = self.state();
            state.scheduled.clear();
            state.clients.drain().map(|(_, client)| client).collect()
        };
        for client in clients {
            let _ = client.close.send(Some((GOING_AWAY, "Server in arresto")));
        }
    }

    // --- collegamenti --------------------------------------------------------------------------

    fn try_reserve(self: &Arc<Self>) -> Option<Slot> {
        self.reserved.try_update(Ordering::SeqCst, Ordering::SeqCst, |open| (open < self.config.max_clients).then_some(open + 1)).ok()?;
        Some(Slot(self.clone()))
    }

    /// La sessione è ancora valida? Un errore del server Discord non toglie l'accesso.
    async fn still_authorized(&self, headers: &HeaderMap) -> bool {
        match self.auth.authorize(headers, false).await {
            Ok(_) => true,
            Err(response) => !matches!(response.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN),
        }
    }
}

/// `_same_origin`: l'origine è il sito stesso (l'intestazione `Host`, o `X-Forwarded-Host` dietro un
/// proxy). Senza `Origin` (client che non sono browser) il cookie di sessione basta.
fn same_origin(headers: &HeaderMap) -> bool {
    let origin = headers.get(header::ORIGIN).and_then(|value| value.to_str().ok()).unwrap_or("");
    if origin.is_empty() {
        return true;
    }
    let Some(netloc) = origin_netloc(origin) else { return false };
    let mut allowed = HashSet::new();
    if let Some(host) = headers.get(header::HOST).and_then(|value| value.to_str().ok()) {
        allowed.insert(host.to_lowercase());
    }
    if let Some(forwarded) = headers.get("x-forwarded-host").and_then(|value| value.to_str().ok()).filter(|value| !value.is_empty()) {
        allowed.insert(forwarded.split(',').next().unwrap_or("").trim().to_lowercase());
    }
    allowed.contains(&netloc)
}

async fn handle(State(hub): State<Arc<SiteEventHub>>, headers: HeaderMap, upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>) -> Response {
    if let Err(response) = hub.auth.authorize(&headers, false).await {
        return response;
    }
    if !same_origin(&headers) {
        return json_error(StatusCode::FORBIDDEN, "Origine non autorizzata");
    }
    let Some(slot) = hub.try_reserve() else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Troppe connessioni live attive, riprova più tardi");
    };
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    upgrade.max_message_size(MAX_MESSAGE_SIZE).on_upgrade(move |socket| serve_client(socket, hub, headers, slot))
}

async fn send_close(socket: &mut WebSocket, code: u16, reason: &'static str, send_timeout: Duration) {
    let _ = timeout(send_timeout, socket.send(Message::Close(Some(CloseFrame { code, reason: reason.into() })))).await;
}

async fn serve_client(mut socket: WebSocket, hub: Arc<SiteEventHub>, headers: HeaderMap, _slot: Slot) {
    let config = hub.config;
    let (queue, mut messages) = mpsc::channel::<String>(config.queue_size);
    let (close, mut close_requests) = watch::channel(None);
    let id = {
        let mut state = hub.state();
        let id = state.next_id;
        state.next_id += 1;
        state.clients.insert(id, Client { queue: queue.clone(), close });
        id
    };
    let _registration = Registration { hub: hub.clone(), id };
    let _ = queue.try_send("{\"type\": \"ready\"}".to_string());

    let mut heartbeat = interval_at(Instant::now() + config.heartbeat, config.heartbeat);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut heard_since_ping = true;
    let mut revalidate_at = Instant::now() + config.revalidate;
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                None | Some(Ok(Message::Close(_))) => return,
                // Un messaggio troppo grande (o una connessione rotta): si chiude.
                Some(Err(_)) => return send_close(&mut socket, MESSAGE_TOO_BIG, "", config.send_timeout).await,
                // Il browser non deve inviare nulla: si ignora.
                Some(Ok(_)) => heard_since_ping = true,
            },
            changed = close_requests.changed() => {
                if changed.is_err() {
                    return;
                }
                let request = *close_requests.borrow();
                if let Some((code, reason)) = request {
                    return send_close(&mut socket, code, reason, config.send_timeout).await;
                }
            },
            message = messages.recv() => {
                let Some(text) = message else { return };
                revalidate_at = Instant::now() + config.revalidate;
                if !matches!(timeout(config.send_timeout, socket.send(Message::text(text))).await, Ok(Ok(()))) {
                    hub.state().clients.remove(&id);
                    return send_close(&mut socket, GOING_AWAY, "Invio non riuscito", config.send_timeout).await;
                }
            },
            _ = sleep_until(revalidate_at) => {
                if !hub.still_authorized(&headers).await {
                    hub.state().clients.remove(&id);
                    return send_close(&mut socket, SESSION_EXPIRED_CLOSE_CODE, "Sessione scaduta", config.send_timeout).await;
                }
                revalidate_at = Instant::now() + config.revalidate;
            },
            _ = heartbeat.tick() => {
                if !heard_since_ping {
                    error!("Un collegamento live del sito non risponde ai ping: chiuso.");
                    return;
                }
                heard_since_ping = false;
                if !matches!(timeout(config.send_timeout, socket.send(Message::Ping(Bytes::new()))).await, Ok(Ok(()))) {
                    return;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests;
