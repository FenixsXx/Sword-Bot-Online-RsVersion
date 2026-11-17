//! L'evento live: avvio, fine, presenze, ciclo di pubblicazione e ascoltatori degli eventi Discord
//! (la parte di `cogs/live_event.py` che tiene insieme voce, database e overlay).
//!
//! La voce sta dietro [`VoiceGateway`] e [`VoiceConnection`]; le scritture sul database sono in
//! [`crate::live_event`]; l'overlay in [`crate::overlay_web`]. Qui c'è l'orchestrazione: chi può
//! avviare, cosa succede se il collegamento cade, come si chiude un evento e a chi si dice.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::DateTime;
use chrono_tz::Tz;
use serde_json::json;
use tokio::time::{Instant, MissedTickBehavior, interval};
use tracing::{error, info, warn};

use crate::database::Database;
use crate::discord_api::{ChannelKind, DiscordApi, EventStatus, MessagePayload, ScheduledEventInfo};
use crate::live_event::{self, NewEvent};
use crate::notify::{AuditEntry, DiscordSender, Notifier};
use crate::overlay::{LiveSessionInfo, LiveState, VoiceSource};
use crate::overlay_web;
use crate::publisher::Publisher;
use crate::schedule::isoformat;
use crate::voice::{BoxFuture, VoiceConnection, VoiceError, VoiceGateway};

/// Quanto può restare scollegato il bot prima che l'evento finisca (discord.py si riconnette da solo
/// dopo brevi interruzioni).
const VOICE_DISCONNECT_GRACE: Duration = Duration::from_secs(10);
/// Ogni quanto si salva il parlato maturato.
const PERSIST_EVERY: Duration = Duration::from_secs(15);
/// Ogni quanto gira il ciclo.
const TICK: Duration = Duration::from_millis(250);
/// Dopo quanti errori di fila nel ciclo l'evento viene chiuso.
const MAX_FAILURES: u32 = 40;
/// Il titolo di un evento avviato a mano.
pub const DEFAULT_TITLE: &str = "🎮 Game Night";

/// Cosa serve per avviare un evento.
#[derive(Debug, Clone)]
pub struct StartRequest {
    pub guild_id: u64,
    pub channel_id: u64,
    pub title: String,
    /// L'evento Discord che lo ha fatto partire (avvio automatico).
    pub scheduled_event_id: Option<u64>,
    /// Chi lo ha avviato a mano; `None` per l'avvio automatico.
    pub actor: Option<u64>,
}

/// Com'è andato un avvio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// Partito: l'ID del report e il link per OBS.
    Started { event_id: i64, link: String },
    /// Lo stesso evento programmato era già in corso: niente da fare.
    AlreadyRunning,
    NotConfigured,
    AlreadyActive,
    NoBotMember,
    NoConnectPermission,
    /// Il collegamento o l'avvio dell'ascolto sono falliti.
    ConnectFailed,
    /// Questa versione del bot non sa ancora collegarsi ai canali vocali.
    VoiceUnavailable,
    ReportFailed,
}

pub const VOICE_UNAVAILABLE: &str = "⚠️ Il collegamento al canale vocale e il rilevamento degli speaker non sono ancora disponibili in questa versione del bot.";

impl StartOutcome {
    /// La risposta da mostrare a chi ha avviato l'evento a mano.
    pub fn message(&self, channel_id: u64) -> String {
        match self {
            StartOutcome::Started { link, .. } => format!(
                "✅ Modalità evento live attiva in <#{channel_id}>.\nOBS Browser Source: {link}\n\
                 Il link è una chiave di accesso: condividilo solo con chi deve vedere l'overlay. \
                 L'audio viene analizzato per stimare gli speaker, ma non viene salvato. \
                 Il report con presenza e parlato stimato è consultabile dallo staff \
                 ed eliminabile con `/evento elimina-rapporto`."
            ),
            StartOutcome::NotConfigured => "❌ Overlay non configurato correttamente. Verifica `EVENT_OVERLAY_PUBLIC_URL`, `EVENT_OVERLAY_HOST` e `EVENT_OVERLAY_PORT`; serve un dominio HTTPS con reverse proxy.".to_string(),
            StartOutcome::AlreadyActive | StartOutcome::AlreadyRunning => "⚠️ È già attivo un evento live. Usa `/evento termina` prima di avviarne un altro.".to_string(),
            StartOutcome::NoBotMember => "❌ Non riesco a recuperare il membro del bot in questo server.".to_string(),
            StartOutcome::NoConnectPermission => "❌ Il bot non ha il permesso di connettersi a quel canale vocale.".to_string(),
            StartOutcome::ConnectFailed => "❌ Non sono riuscito a collegarmi o avviare il rilevamento speaker. Controlla permessi, libreria voice-receive e log.".to_string(),
            StartOutcome::VoiceUnavailable => VOICE_UNAVAILABLE.to_string(),
            StartOutcome::ReportFailed => "❌ Non sono riuscito a creare il report evento.".to_string(),
        }
    }
}

/// Quello che i comandi `/evento avvia|collega|termina` chiedono all'evento live.
pub trait LiveController: Send + Sync {
    /// Avvia un evento a mano (`start_event` con un'interazione).
    fn start(&self, guild_id: u64, channel_id: u64, actor: u64) -> BoxFuture<'_, StartOutcome>;
    /// Termina l'evento in corso e lo registra nel registro audit; `None` se non ce n'è uno.
    fn stop(&self, actor: u64) -> BoxFuture<'_, Option<i64>>;
    /// Il canale vocale standard (non Stage) in cui si trova una persona.
    fn standard_voice_channel_of(&self, guild_id: u64, user_id: u64) -> BoxFuture<'_, Option<u64>>;
    /// Chiude l'evento in corso con il motivo indicato (senza scrivere nell'audit).
    fn finish(&self, reason: &'static str) -> BoxFuture<'_, Option<i64>>;
    /// Avvia un evento con un titolo, come fa il sito (senza interazione: parte come l'avvio automatico).
    fn start_titled(&self, guild_id: u64, channel_id: u64, title: String) -> BoxFuture<'_, StartOutcome>;
}

/// Il controller collegato a bot avviato: il server HTTP parte prima del gateway.
#[derive(Default)]
pub struct LateLive {
    inner: std::sync::OnceLock<Arc<dyn LiveController>>,
}

impl LateLive {
    pub fn set(&self, controller: Arc<dyn LiveController>) {
        let _ = self.inner.set(controller);
    }

    /// Il controller, se il bot è già pronto.
    pub fn get(&self) -> Option<&Arc<dyn LiveController>> {
        self.inner.get()
    }
}

/// L'evento in corso (in Python il dizionario `session`).
struct Session {
    event_id: i64,
    guild_id: u64,
    channel_id: u64,
    connection: Arc<dyn VoiceConnection>,
    title: String,
    scheduled_event_id: Option<u64>,
    /// Secondi di parlato per utente, nell'ordine in cui sono comparsi.
    speaking: Vec<(u64, f64)>,
    /// Quanto di `speaking` è già nel database.
    persisted: Vec<(u64, f64)>,
    audio_recovery_attempts: u32,
    last_audio_packet_at: Option<Instant>,
    last_sample: Instant,
    last_persist: Instant,
    peak: i64,
    disconnected_since: Option<Instant>,
}

/// Cosa fare dopo aver aggiornato i contatori di un passo del ciclo.
enum Next {
    Idle,
    Finish(&'static str),
    Publish { connection: Arc<dyn VoiceConnection>, levels: HashMap<u64, f64> },
}

pub struct LiveService<S: DiscordSender, A: DiscordApi, G: VoiceGateway> {
    db: Database,
    notifier: Notifier<S>,
    publisher: Arc<Publisher<A>>,
    gateway: G,
    live: Arc<LiveState>,
    /// Il server HTTP dell'overlay è in ascolto (`self.runner` in Python).
    overlay_running: bool,
    clock: fn() -> DateTime<Tz>,
    monotonic: fn() -> Instant,
    /// Serializza avvio e fine (`_session_lock`).
    session_lock: tokio::sync::Mutex<()>,
    session: Mutex<Option<Session>>,
    /// Eventi programmati in corso di avvio (`_auto_starting_events`).
    auto_starting: Mutex<HashSet<u64>>,
    /// L'ultimo stato noto di ogni evento programmato: gli aggiornamenti di Discord portano solo il
    /// nuovo stato, mentre discord.py passa anche quello precedente.
    event_status: Mutex<HashMap<u64, EventStatus>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl<S: DiscordSender, A: DiscordApi, G: VoiceGateway> LiveService<S, A, G> {
    pub fn new(
        db: Database,
        notifier: Notifier<S>,
        publisher: Arc<Publisher<A>>,
        gateway: G,
        live: Arc<LiveState>,
        overlay_running: bool,
    ) -> Self {
        Self {
            db,
            notifier,
            publisher,
            gateway,
            live,
            overlay_running,
            clock: crate::schedule::now,
            monotonic: Instant::now,
            session_lock: tokio::sync::Mutex::new(()),
            session: Mutex::new(None),
            auto_starting: Mutex::new(HashSet::new()),
            event_status: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_clocks(mut self, clock: fn() -> DateTime<Tz>, monotonic: fn() -> Instant) -> Self {
        self.clock = clock;
        self.monotonic = monotonic;
        self
    }

    fn api(&self) -> &A {
        self.publisher.api()
    }

    /// C'è un evento in corso.
    pub fn is_active(&self) -> bool {
        lock(&self.session).is_some()
    }

    fn overlay_ready(&self) -> bool {
        let url = &self.live.public_url;
        self.overlay_running && !url.public_url.is_empty() && url.configuration_error.is_none()
    }

    /// Il link dell'overlay (tema classico) con la chiave attuale.
    fn overlay_link(&self, guild_id: u64) -> String {
        let link = live_event::overlay_token_with(&self.db, self.live.token_generator)
            .map_err(|e| e.to_string())
            .and_then(|token| live_event::overlay_link(&self.live.public_url.public_url, guild_id, "classic", &token).map_err(|e| e.to_string()));
        link.unwrap_or_else(|e| {
            error!("Impossibile preparare il link dell'overlay: {e}");
            String::new()
        })
    }

    // --- avvio ---------------------------------------------------------------------------------

    pub async fn start_event(&self, request: StartRequest) -> StartOutcome {
        let StartRequest { guild_id, channel_id, title, scheduled_event_id, actor } = request;
        if !self.overlay_ready() {
            error!("Overlay non configurato correttamente: l'evento live non può partire.");
            return StartOutcome::NotConfigured;
        }
        let _guard = self.session_lock.lock().await;
        if let Some(active) = lock(&self.session).as_ref() {
            // Lo stesso evento programmato già in corso non è un errore.
            if scheduled_event_id.is_some() && active.scheduled_event_id == scheduled_event_id {
                return StartOutcome::AlreadyRunning;
            }
            warn!("Evento {title} ignorato: è già attivo un altro evento.");
            return StartOutcome::AlreadyActive;
        }
        if !self.gateway.bot_in_guild(guild_id) {
            error!("Non riesco a recuperare il membro del bot nel server {guild_id}.");
            return StartOutcome::NoBotMember;
        }
        let Some(channel) = self.gateway.channel_info(guild_id, channel_id).filter(|channel| channel.can_connect) else {
            error!("Avvio evento negato: nessun permesso connect nel canale {channel_id}.");
            return StartOutcome::NoConnectPermission;
        };
        let connection = match self.gateway.connect(guild_id, channel_id).await {
            Ok(connection) => connection,
            Err(VoiceError::Unavailable) => return StartOutcome::VoiceUnavailable,
            Err(VoiceError::Failed(reason)) => {
                error!("Impossibile avviare la modalità evento nel canale vocale {channel_id}: {reason}");
                return StartOutcome::ConnectFailed;
            }
        };
        let started_at = (self.clock)();
        let members: Vec<u64> = connection.members().iter().map(|member| member.id).collect();
        let new_event = NewEvent { guild_id, scheduled_event_id, title: &title, channel_id, channel_name: &channel.name, started_at };
        let event_id = match live_event::create_event_rows(&self.db, &new_event, &members) {
            Ok(event_id) => event_id,
            Err(e) => {
                connection.stop_listening();
                if connection.is_connected() {
                    if let Err(e) = connection.disconnect().await {
                        error!("Impossibile disconnettere il bot dopo l'errore del report: {e}");
                    }
                }
                error!("Impossibile creare il report dell'evento per il canale {channel_id}: {e}");
                return StartOutcome::ReportFailed;
            }
        };
        if let Err(e) = self.db.set_setting("guild_id", guild_id) {
            error!("Impossibile salvare l'ID del server dell'evento: {e}");
        }
        let now = (self.monotonic)();
        *lock(&self.session) = Some(Session {
            event_id,
            guild_id,
            channel_id,
            connection: connection.clone(),
            title: title.clone(),
            scheduled_event_id,
            speaking: Vec::new(),
            persisted: Vec::new(),
            audio_recovery_attempts: 0,
            last_audio_packet_at: connection.last_packet_at(),
            last_sample: now,
            last_persist: now,
            peak: members.len() as i64,
            disconnected_since: None,
        });
        self.live.start_session(
            LiveSessionInfo { title: title.clone(), guild_id, channel_id, channel_name: channel.name.clone(), started_at: isoformat(started_at) },
            connection as Arc<dyn VoiceSource>,
        );
        let link = self.overlay_link(guild_id);
        match actor {
            None => {
                info!("Evento programmato {scheduled_event_id:?} avviato nel canale vocale {channel_id}.");
                // Un evento avviato dal sito non ha un evento programmato: l'ID resta vuoto.
                let entry = AuditEntry::new(None, "scheduled_live_event_started").details(json!({"title": title, "channel_id": channel_id}));
                let entry = match scheduled_event_id {
                    Some(id) => entry.entity("scheduled_event", id),
                    None => entry.entity_type("scheduled_event"),
                };
                self.notifier.record_staff_audit(entry);
                self.notify_staff_event_started(&title, channel_id, guild_id, &link).await;
            }
            Some(actor) => {
                self.notifier.record_staff_audit(
                    AuditEntry::new(Some(actor), "live_event_started")
                        .entity("live_event", event_id)
                        .details(json!({"title": title, "channel_id": channel_id})),
                );
            }
        }
        StartOutcome::Started { event_id, link }
    }

    /// Il canale staff, se è configurato e testuale. I motivi per cui non lo è si registrano.
    async fn staff_text_channel(&self, guild_id: u64, what: &str) -> Option<u64> {
        let raw = match self.db.get_setting("staff_channel") {
            Ok(raw) => raw.filter(|value| !value.is_empty()),
            Err(e) => {
                error!("Impossibile leggere il canale staff per {what}: {e}");
                return None;
            }
        };
        let Some(raw) = raw else {
            error!("Canale staff non configurato per {what}.");
            return None;
        };
        let Some(channel_id) = live_event::python_int(&raw).and_then(|id| u64::try_from(id).ok()) else {
            error!("ID del canale staff non valido: {raw}.");
            return None;
        };
        match self.api().channel_kind(guild_id, channel_id).await {
            Ok(ChannelKind::Text) => Some(channel_id),
            Ok(_) => {
                error!("Il canale configurato per lo staff {channel_id} non è testuale.");
                None
            }
            Err(e) => {
                error!("Impossibile recuperare il canale staff per {what}: {e}");
                None
            }
        }
    }

    async fn notify_staff_event_started(&self, title: &str, channel_id: u64, guild_id: u64, link: &str) {
        let Some(staff) = self.staff_text_channel(guild_id, &format!("l'evento {title}")).await else {
            return;
        };
        let message = format!("🎮 Evento programmato **{title}** avviato in <#{channel_id}>.\nLink OBS: {link}");
        if let Err(e) = self.api().send_message(staff, MessagePayload::default().with_content(message)).await {
            error!("Impossibile inviare il link OBS nel canale staff {staff}: {e}");
        }
    }

    async fn publish_event_report(&self, guild_id: u64, event_id: i64) {
        let Some(staff) = self.staff_text_channel(guild_id, &format!("il report evento {event_id}")).await else {
            return;
        };
        let report = match live_event::build_event_report(&self.db, event_id) {
            Ok(Some(report)) => report,
            Ok(None) => {
                error!("Report evento {event_id} non trovato dopo la chiusura.");
                return;
            }
            Err(e) => {
                error!("Impossibile preparare il report evento {event_id}: {e}");
                return;
            }
        };
        if let Err(e) = self.api().send_message(staff, MessagePayload::embed(report.build())).await {
            error!("Impossibile pubblicare il report evento {event_id} nel canale staff: {e}");
        }
    }

    // --- fine ----------------------------------------------------------------------------------

    /// Chiude l'evento in corso con il motivo indicato. Restituisce l'ID del report, o `None` se non
    /// c'era un evento.
    pub async fn finish_event(&self, reason: &str) -> Option<i64> {
        let guard = self.session_lock.lock().await;
        let session = lock(&self.session).take()?;
        self.live.set_session(None);
        let ended_at = (self.clock)();
        let present_now = session.connection.members().len() as i64;
        if let Err(e) = live_event::finish_event_rows(
            &self.db,
            session.event_id,
            &session.speaking,
            &session.persisted,
            session.peak,
            present_now,
            ended_at,
            reason,
        ) {
            // Un errore del database non deve lasciare l'evento a metà: la pulizia (voce, overlay) e
            // il report proseguono comunque.
            error!("Impossibile salvare la chiusura dell'evento {}: {e}", session.event_id);
        }
        session.connection.stop_listening();
        if let Err(e) = session.connection.disconnect().await {
            error!("Impossibile disconnettere il bot dall'evento {}: {e}", session.event_id);
        }
        self.live.hub.event_ended();
        drop(guard);
        self.publish_event_report(session.guild_id, session.event_id).await;
        self.notifier.record_staff_audit(
            AuditEntry::new(None, "live_event_finished")
                .entity("live_event", session.event_id)
                .details(json!({"reason": reason, "title": session.title})),
        );
        Some(session.event_id)
    }

    /// `/evento termina`: chiude l'evento a mano e lo scrive nel registro audit.
    pub async fn stop_event(&self, actor: u64) -> Option<i64> {
        let event_id = self.finish_event("manuale").await?;
        self.notifier.record_staff_audit(AuditEntry::new(Some(actor), "live_event_stopped").entity("live_event", event_id));
        Some(event_id)
    }

    // --- presenze ------------------------------------------------------------------------------

    /// Una persona entra o esce dal canale dell'evento (`on_voice_state_update`).
    pub fn on_voice_state_update(&self, guild_id: u64, user_id: u64, is_bot: bool, before: Option<u64>, after: Option<u64>) {
        let mut guard = lock(&self.session);
        let Some(session) = guard.as_mut() else { return };
        if is_bot || guild_id != session.guild_id {
            return;
        }
        let was_present = before == Some(session.channel_id);
        let is_present = after == Some(session.channel_id);
        if was_present == is_present {
            return;
        }
        let present_now = session.connection.members().len() as i64;
        match live_event::record_presence_change(&self.db, session.event_id, user_id, was_present, is_present, (self.clock)(), present_now, session.peak) {
            Ok(peak) => session.peak = peak,
            Err(e) => error!("Impossibile aggiornare la presenza dell'utente {user_id} nell'evento {}: {e}", session.event_id),
        }
    }

    // --- ciclo ---------------------------------------------------------------------------------

    /// Aggiorna i contatori della sessione e dice cosa fare dopo.
    fn advance(&self, now: Instant, wall: DateTime<Tz>) -> Next {
        let mut guard = lock(&self.session);
        let Some(session) = guard.as_mut() else { return Next::Idle };
        let elapsed = now.saturating_duration_since(session.last_sample).as_secs_f64().min(1.0);
        session.last_sample = now;
        let levels = session.connection.levels();
        let last_packet = session.connection.last_packet_at();
        if last_packet > session.last_audio_packet_at {
            session.last_audio_packet_at = last_packet;
            session.audio_recovery_attempts = 0;
        }
        let mut speaking: Vec<u64> = levels.keys().copied().collect();
        speaking.sort_unstable();
        for user_id in speaking {
            match session.speaking.iter_mut().find(|(id, _)| *id == user_id) {
                Some((_, seconds)) => *seconds += elapsed,
                None => session.speaking.push((user_id, elapsed)),
            }
        }
        if now.saturating_duration_since(session.last_persist) >= PERSIST_EVERY {
            match live_event::persist_progress(&self.db, session.event_id, &session.speaking, &session.persisted, wall) {
                Ok(()) => session.persisted = session.speaking.clone(),
                Err(e) => error!("Impossibile salvare il progresso dell'evento {}: {e}", session.event_id),
            }
            session.last_persist = now;
        }
        if !session.connection.is_connected() {
            let since = *session.disconnected_since.get_or_insert(now);
            if now.saturating_duration_since(since) < VOICE_DISCONNECT_GRACE {
                return Next::Idle;
            }
            warn!("Connessione vocale persa durante l'evento {}.", session.event_id);
            return Next::Finish("disconnessione");
        }
        session.disconnected_since = None;
        Next::Publish { connection: session.connection.clone(), levels }
    }

    /// Un passo del ciclo: conteggio del parlato, salvataggio, controllo della connessione e
    /// pubblicazione dello stato alle pagine overlay.
    pub async fn tick(&self, now: Instant, wall: DateTime<Tz>) -> Result<(), String> {
        match self.advance(now, wall) {
            Next::Idle => Ok(()),
            Next::Finish(reason) => {
                self.finish_event(reason).await;
                Ok(())
            }
            Next::Publish { connection, levels } => {
                if self.live.hub.client_count() == 0 {
                    return Ok(());
                }
                let Some(info) = self.live.session() else { return Ok(()) };
                let snapshot = overlay_web::event_snapshot(&self.db, &info, &connection.members(), &levels).map_err(|e| e.to_string())?;
                self.live.hub.broadcast(&snapshot.to_string());
                Ok(())
            }
        }
    }

    /// Un passo con il conteggio degli errori di fila: dopo 40 l'evento viene chiuso.
    pub async fn loop_step(&self, failures: &mut u32, now: Instant, wall: DateTime<Tz>) {
        match self.tick(now, wall).await {
            Ok(()) => *failures = 0,
            Err(e) => {
                *failures += 1;
                if matches!(*failures, 1 | 10 | 40) || *failures % 100 == 0 {
                    error!("Errore nel ciclo di pubblicazione dell'evento ({} consecutivi): {e}", *failures);
                }
                if *failures >= MAX_FAILURES {
                    self.finish_event("errore").await;
                    *failures = 0;
                }
            }
        }
    }

    /// Il ciclo di pubblicazione (`publish_loop`): quattro passi al secondo.
    pub async fn run_loop(self: Arc<Self>) {
        let mut ticker = interval(TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut failures = 0u32;
        loop {
            ticker.tick().await;
            self.loop_step(&mut failures, (self.monotonic)(), (self.clock)()).await;
        }
    }

    // --- ascolto audio -------------------------------------------------------------------------

    /// L'ascolto audio si è interrotto con un errore: tra poco si riprova (`_audio_listener_after`).
    pub fn listener_failed(self: &Arc<Self>, connection: Arc<dyn VoiceConnection>, reason: &str) {
        let event = lock(&self.session).as_ref().map_or_else(|| "in avvio".to_string(), |session| session.event_id.to_string());
        error!("Listener audio dell'evento {event} interrotto ({reason}); tenterò il riavvio.");
        let service = self.clone();
        tokio::spawn(async move { service.recover_audio_listener(connection).await });
    }

    /// Riavvia l'ascolto dopo 1, 2, 4… fino a 30 secondi, a seconda dei tentativi già fatti senza
    /// che arrivasse audio.
    pub async fn recover_audio_listener(&self, connection: Arc<dyn VoiceConnection>) {
        let delay = {
            let mut guard = lock(&self.session);
            let Some(session) = guard.as_mut().filter(|session| Arc::ptr_eq(&session.connection, &connection)) else {
                return;
            };
            let attempt = session.audio_recovery_attempts;
            session.audio_recovery_attempts += 1;
            Duration::from_secs(30.min(1u64 << attempt.min(5)))
        };
        tokio::time::sleep(delay).await;
        let _guard = self.session_lock.lock().await;
        let event_id = {
            let guard = lock(&self.session);
            let Some(session) = guard.as_ref().filter(|session| Arc::ptr_eq(&session.connection, &connection)) else {
                return;
            };
            if !connection.is_connected() || connection.is_listening() {
                return;
            }
            session.event_id
        };
        if let Err(e) = connection.restart_listening() {
            error!("Riavvio del listener audio per l'evento {event_id} non riuscito: {e}");
            return;
        }
        info!("Listener audio dell'evento {event_id} riavviato.");
    }

    // --- eventi programmati di Discord ---------------------------------------------------------

    /// Avvia l'evento per un evento programmato abilitato che è diventato attivo
    /// (`_start_scheduled_event`).
    pub async fn start_scheduled_event(&self, guild_id: u64, event: &ScheduledEventInfo) {
        if !live_event::scheduled_event_optins(&self.db).contains(&(event.id as i64))
            || event.status != EventStatus::Active
            || lock(&self.auto_starting).contains(&event.id)
        {
            return;
        }
        let is_voice = match event.channel_id {
            Some(channel) => self.api().channel_kind(guild_id, channel).await.is_ok_and(|kind| kind == ChannelKind::Voice),
            None => false,
        };
        let Some(channel_id) = event.channel_id.filter(|_| is_voice) else {
            warn!("Evento programmato {} abilitato ma non collegato a un canale vocale standard.", event.id);
            return;
        };
        lock(&self.auto_starting).insert(event.id);
        let outcome = self
            .start_event(StartRequest {
                guild_id,
                channel_id,
                title: event.name.chars().take(100).collect(),
                scheduled_event_id: Some(event.id),
                actor: None,
            })
            .await;
        lock(&self.auto_starting).remove(&event.id);
        if !matches!(outcome, StartOutcome::Started { .. } | StartOutcome::AlreadyRunning) {
            error!("Avvio automatico dell'evento programmato {} non riuscito: {outcome:?}.", event.id);
        }
    }

    /// Ricorda lo stato di un evento programmato; restituisce quello precedente.
    pub fn note_event_status(&self, event_id: u64, status: EventStatus) -> Option<EventStatus> {
        lock(&self.event_status).insert(event_id, status)
    }

    /// Un evento programmato è cambiato (`on_scheduled_event_update`). Parte quando diventa attivo,
    /// e finisce quando un evento attivo si conclude o viene annullato.
    pub async fn on_scheduled_event_update(&self, guild_id: u64, event: &ScheduledEventInfo) {
        let before = self.note_event_status(event.id, event.status);
        if before != Some(EventStatus::Active) && event.status == EventStatus::Active {
            self.start_scheduled_event(guild_id, event).await;
        } else if before.is_none_or(|status| status == EventStatus::Active)
            && matches!(event.status, EventStatus::Completed | EventStatus::Canceled)
            && lock(&self.session).as_ref().is_some_and(|session| session.scheduled_event_id == Some(event.id))
        {
            self.finish_event(if event.status == EventStatus::Completed { "completato" } else { "annullato" }).await;
        }
    }

    /// Un evento programmato è stato eliminato (`on_scheduled_event_delete`).
    pub async fn on_scheduled_event_delete(&self, event_id: u64) {
        lock(&self.event_status).remove(&event_id);
        if lock(&self.session).as_ref().is_some_and(|session| session.scheduled_event_id == Some(event_id)) {
            self.finish_event("eliminato").await;
        }
    }

    /// Il bot è pronto: ricorda gli eventi programmati e avvia quelli abilitati già attivi
    /// (`on_ready`).
    pub async fn on_ready(&self, guilds: &[u64]) {
        for guild_id in guilds {
            let events = match self.api().scheduled_events(*guild_id).await {
                Ok(events) => events,
                Err(e) => {
                    error!("Impossibile leggere gli eventi programmati del server {guild_id}: {e}");
                    continue;
                }
            };
            for event in &events {
                self.note_event_status(event.id, event.status);
            }
            // Si leggono qui, come in Python, una volta per ogni server.
            let opted_in = live_event::scheduled_event_optins(&self.db);
            for event in events.iter().filter(|event| opted_in.contains(&(event.id as i64)) && event.status == EventStatus::Active) {
                self.start_scheduled_event(*guild_id, event).await;
            }
        }
    }
}

impl<S: DiscordSender, A: DiscordApi, G: VoiceGateway> LiveController for LiveService<S, A, G> {
    fn start(&self, guild_id: u64, channel_id: u64, actor: u64) -> BoxFuture<'_, StartOutcome> {
        Box::pin(async move {
            self.start_event(StartRequest { guild_id, channel_id, title: DEFAULT_TITLE.to_string(), scheduled_event_id: None, actor: Some(actor) }).await
        })
    }

    fn stop(&self, actor: u64) -> BoxFuture<'_, Option<i64>> {
        Box::pin(self.stop_event(actor))
    }

    fn standard_voice_channel_of(&self, guild_id: u64, user_id: u64) -> BoxFuture<'_, Option<u64>> {
        Box::pin(async move {
            let channel = self.gateway.user_voice_channel(guild_id, user_id)?;
            (self.api().channel_kind(guild_id, channel).await.ok()? == ChannelKind::Voice).then_some(channel)
        })
    }

    fn finish(&self, reason: &'static str) -> BoxFuture<'_, Option<i64>> {
        Box::pin(self.finish_event(reason))
    }

    fn start_titled(&self, guild_id: u64, channel_id: u64, title: String) -> BoxFuture<'_, StartOutcome> {
        Box::pin(async move { self.start_event(StartRequest { guild_id, channel_id, title, scheduled_event_id: None, actor: None }).await })
    }
}

#[cfg(test)]
mod tests;
