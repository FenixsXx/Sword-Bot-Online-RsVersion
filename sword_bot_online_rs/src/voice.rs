//! Il collegamento ai canali vocali, dietro dei trait (in Python `discord.VoiceChannel`,
//! `VoiceRecvClient` e `SpeakerTracker`).
//!
//! L'evento live (`live_service`) non sa come ci si collega a un canale e come si ascoltano i
//! pacchetti audio: chiede a un [`VoiceGateway`] di collegarsi e riceve una [`VoiceConnection`]. In
//! produzione c'è [`SongbirdGateway`]: legge dalla cache canali, permessi e presenze e si collega
//! con songbird, che riceve l'audio (anche con la cifratura end-to-end DAVE) e lo decodifica in PCM.
//! [`SpeakerTracker`] ne ricava chi sta parlando senza conservare audio.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::serenity_prelude::{self as serenity, ChannelId, GuildId, UserId};
use songbird::Songbird;
use songbird::driver::{DecodeConfig, DecodeMode};
use songbird::events::context_data::VoiceTick;
use songbird::events::{CoreEvent, EventContext};
use tokio::time::Instant;
use tracing::warn;

use crate::overlay::{VoiceMember, VoiceSource};

/// Un futuro con tipo cancellato, per i metodi `async` dei trait usati come `dyn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceError {
    /// Questa versione del bot non sa collegarsi ai canali vocali.
    Unavailable,
    /// Il collegamento o l'avvio dell'ascolto sono falliti (permessi, libreria, rete).
    Failed(String),
}

impl std::fmt::Display for VoiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VoiceError::Unavailable => f.write_str("collegamento vocale non disponibile"),
            VoiceError::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for VoiceError {}

/// Un collegamento attivo a un canale vocale, con l'ascolto già avviato.
pub trait VoiceConnection: VoiceSource {
    /// Il bot è ancora nel canale (`voice_client.is_connected()`).
    fn is_connected(&self) -> bool;
    /// L'ascolto dei pacchetti è attivo (`voice_client.is_listening()`).
    fn is_listening(&self) -> bool;
    /// Quando è arrivato l'ultimo pacchetto audio (`sink.last_packet_at`), se è mai arrivato.
    fn last_packet_at(&self) -> Option<Instant>;
    /// Riavvia l'ascolto dopo un errore.
    fn restart_listening(&self) -> Result<(), VoiceError>;
    fn stop_listening(&self);
    /// Lascia il canale (`disconnect(force=True)`).
    fn disconnect(&self) -> BoxFuture<'_, Result<(), VoiceError>>;
}

/// Quanto serve sapere di un canale vocale per decidere se collegarsi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceChannelInfo {
    pub name: String,
    /// Il bot può connettersi (`channel.permissions_for(guild.me).connect`).
    pub can_connect: bool,
}

pub trait VoiceGateway: Send + Sync + 'static {
    /// Il bot è tra i membri del server (`guild.me`).
    fn bot_in_guild(&self, guild_id: u64) -> bool;
    fn channel_info(&self, guild_id: u64, channel_id: u64) -> Option<VoiceChannelInfo>;
    /// Il canale vocale in cui si trova una persona (`member.voice.channel`).
    fn user_voice_channel(&self, guild_id: u64, user_id: u64) -> Option<u64>;
    /// Si collega al canale e avvia l'ascolto. Se il bot è già in un altro canale lo lascia; se in
    /// caso di errore è rimasto collegato, si scollega prima di restituirlo.
    fn connect(&self, guild_id: u64, channel_id: u64) -> impl Future<Output = Result<Arc<dyn VoiceConnection>, VoiceError>> + Send;
}

// --- chi sta parlando ---------------------------------------------------------------------------

/// Il volume (RMS relativo al massimo) da cui si considera che una persona stia parlando.
pub const SPEAKER_THRESHOLD: f64 = 0.012;
/// Dopo quanto senza pacchetti sopra soglia una persona non risulta più in parlato.
pub const SPEAKER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(800);

/// `audioop.rms(pcm, 2) / 32768`: il valore efficace dei campioni a 16 bit, troncato all'intero
/// come fa `audioop`, rispetto al fondo scala.
pub fn pcm_level(pcm: &[i16]) -> f64 {
    let sum_of_squares: f64 = pcm.iter().map(|sample| f64::from(*sample) * f64::from(*sample)).sum();
    let rms = (sum_of_squares / pcm.len() as f64).sqrt() as u32;
    f64::from(rms) / 32768.0
}

#[derive(Debug, Default)]
struct TrackerState {
    /// Per ogni utente il volume dell'ultimo pacchetto sopra soglia e quando è arrivato.
    levels: std::collections::HashMap<u64, (f64, Instant)>,
    last_packet_at: Option<Instant>,
}

/// Misura chi sta parlando dai pacchetti audio decodificati (`SpeakerTracker` di Python). Non
/// conserva audio: tiene solo, per ogni persona, l'ultimo volume sopra soglia.
#[derive(Debug, Default)]
pub struct SpeakerTracker {
    state: std::sync::Mutex<TrackerState>,
}

impl SpeakerTracker {
    fn state(&self) -> std::sync::MutexGuard<'_, TrackerState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Un pacchetto di PCM di `user`. I pacchetti di sconosciuti o vuoti non contano; quelli dei bot
    /// contano come audio in arrivo ma non come parlato.
    pub fn write(&self, user: Option<u64>, is_bot: bool, pcm: &[i16], now: Instant) {
        let Some(user) = user else { return };
        if pcm.is_empty() {
            return;
        }
        let level = pcm_level(pcm);
        let mut state = self.state();
        state.last_packet_at = Some(now);
        if level >= SPEAKER_THRESHOLD && !is_bot {
            state.levels.insert(user, (level, now));
        }
    }

    /// Chi sta parlando ora e con che volume; chi tace da più di 0,8 secondi esce.
    pub fn active_levels(&self, now: Instant) -> std::collections::HashMap<u64, f64> {
        let mut state = self.state();
        state.levels.retain(|_, (_, last_active)| now.saturating_duration_since(*last_active) <= SPEAKER_TIMEOUT);
        state.levels.iter().map(|(user, (level, _))| (*user, *level)).collect()
    }

    pub fn last_packet_at(&self) -> Option<Instant> {
        self.state().last_packet_at
    }

    pub fn cleanup(&self) {
        let mut state = self.state();
        state.levels.clear();
        state.last_packet_at = None;
    }
}

// --- songbird ----------------------------------------------------------------------------------

/// Le persone (bot esclusi) collegate a un canale vocale, dalla cache del gateway.
fn humans_in_channel(cache: &serenity::Cache, guild_id: GuildId, channel_id: ChannelId) -> Vec<VoiceMember> {
    let Some(guild) = cache.guild(guild_id) else { return Vec::new() };
    let mut members: Vec<VoiceMember> = guild
        .voice_states
        .iter()
        .filter(|(_, state)| state.channel_id == Some(channel_id))
        .filter_map(|(user, _)| guild.members.get(user))
        .filter(|member| !member.user.bot)
        .map(|member| VoiceMember { id: member.user.id.get(), name: member.display_name().to_string(), avatar_url: member.face() })
        .collect();
    members.sort_by_key(|member| member.id);
    members
}

/// Un collegamento vocale con ricezione audio.
pub struct SongbirdConnection {
    cache: Arc<serenity::Cache>,
    manager: Arc<Songbird>,
    guild_id: GuildId,
    channel_id: ChannelId,
    tracker: SpeakerTracker,
    /// SSRC (identificatore RTP) -> utente, dagli eventi «speaking».
    ssrc_users: std::sync::Mutex<std::collections::HashMap<u32, u64>>,
    connected: AtomicBool,
    listening: AtomicBool,
}

impl std::fmt::Debug for SongbirdConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SongbirdConnection").field("guild", &self.guild_id).field("channel", &self.channel_id).finish_non_exhaustive()
    }
}

impl SongbirdConnection {
    fn is_bot(&self, user: u64) -> bool {
        self.cache.guild(self.guild_id).and_then(|guild| guild.members.get(&UserId::new(user)).map(|member| member.user.bot)).unwrap_or(false)
    }

    fn ssrc_users(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<u32, u64>> {
        self.ssrc_users.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Un passo di 20 ms: i pacchetti decodificati di chi sta mandando audio.
    fn on_tick(&self, tick: &VoiceTick, now: Instant) {
        if !self.listening.load(Ordering::SeqCst) {
            return;
        }
        for (ssrc, data) in &tick.speaking {
            let Some(pcm) = data.decoded_voice.as_deref() else { continue };
            let user = self.ssrc_users().get(ssrc).copied();
            self.tracker.write(user, user.is_some_and(|user| self.is_bot(user)), pcm, now);
        }
    }
}

impl VoiceSource for SongbirdConnection {
    fn members(&self) -> Vec<VoiceMember> {
        humans_in_channel(&self.cache, self.guild_id, self.channel_id)
    }

    fn levels(&self) -> std::collections::HashMap<u64, f64> {
        self.tracker.active_levels(Instant::now())
    }
}

impl VoiceConnection for SongbirdConnection {
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn is_listening(&self) -> bool {
        self.listening.load(Ordering::SeqCst)
    }

    fn last_packet_at(&self) -> Option<Instant> {
        self.tracker.last_packet_at()
    }

    fn restart_listening(&self) -> Result<(), VoiceError> {
        self.listening.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn stop_listening(&self) {
        self.listening.store(false, Ordering::SeqCst);
        self.tracker.cleanup();
    }

    fn disconnect(&self) -> BoxFuture<'_, Result<(), VoiceError>> {
        Box::pin(async move {
            self.connected.store(false, Ordering::SeqCst);
            self.manager.remove(self.guild_id).await.map_err(|e| VoiceError::Failed(e.to_string()))
        })
    }
}

/// Gli eventi del collegamento vocale che interessano al tracker.
struct Receiver(Arc<SongbirdConnection>);

#[serenity::async_trait]
impl songbird::EventHandler for Receiver {
    async fn act(&self, context: &EventContext<'_>) -> Option<songbird::Event> {
        match context {
            EventContext::SpeakingStateUpdate(speaking) => {
                if let Some(user) = speaking.user_id {
                    self.0.ssrc_users().insert(speaking.ssrc, user.0);
                }
            }
            EventContext::VoiceTick(tick) => self.0.on_tick(tick, Instant::now()),
            EventContext::ClientDisconnect(left) => self.0.ssrc_users().retain(|_, user| *user != left.user_id.0),
            EventContext::DriverConnect(_) | EventContext::DriverReconnect(_) => self.0.connected.store(true, Ordering::SeqCst),
            EventContext::DriverDisconnect(_) => self.0.connected.store(false, Ordering::SeqCst),
            _ => {}
        }
        None
    }
}

/// Il gateway di produzione: legge canali, permessi e presenze dalla cache e si collega con songbird.
#[derive(Clone)]
pub struct SongbirdGateway {
    cache: Arc<serenity::Cache>,
    manager: Arc<Songbird>,
    bot_id: u64,
}

impl SongbirdGateway {
    pub fn new(cache: Arc<serenity::Cache>, manager: Arc<Songbird>, bot_id: u64) -> Self {
        Self { cache, manager, bot_id }
    }

    /// La configurazione di songbird per questo bot: i pacchetti audio si decodificano in PCM, che
    /// serve a misurare chi parla (e non si conserva).
    pub fn config() -> songbird::Config {
        songbird::Config::default().decode_mode(DecodeMode::Decode(DecodeConfig::default()))
    }
}

impl VoiceGateway for SongbirdGateway {
    fn bot_in_guild(&self, guild_id: u64) -> bool {
        self.cache.guild(GuildId::new(guild_id)).is_some_and(|guild| guild.members.contains_key(&UserId::new(self.bot_id)))
    }

    fn channel_info(&self, guild_id: u64, channel_id: u64) -> Option<VoiceChannelInfo> {
        let guild = self.cache.guild(GuildId::new(guild_id))?;
        let channel = guild.channels.get(&ChannelId::new(channel_id))?;
        let me = guild.members.get(&UserId::new(self.bot_id))?;
        Some(VoiceChannelInfo { name: channel.name.clone(), can_connect: guild.user_permissions_in(channel, me).connect() })
    }

    fn user_voice_channel(&self, guild_id: u64, user_id: u64) -> Option<u64> {
        let guild = self.cache.guild(GuildId::new(guild_id))?;
        guild.voice_states.get(&UserId::new(user_id))?.channel_id.map(ChannelId::get)
    }

    async fn connect(&self, guild_id: u64, channel_id: u64) -> Result<Arc<dyn VoiceConnection>, VoiceError> {
        let guild = GuildId::new(guild_id);
        let channel = ChannelId::new(channel_id);
        // Un collegamento che c'era già (anche nello stesso canale) si lascia: il nuovo ha il suo tracker.
        if self.manager.get(guild).is_some() {
            if let Err(e) = self.manager.remove(guild).await {
                warn!("Impossibile lasciare il collegamento vocale precedente del server {guild_id}: {e}");
            }
        }
        let call = self.manager.join(guild, channel).await.map_err(|e| VoiceError::Failed(e.to_string()))?;
        let connection = Arc::new(SongbirdConnection {
            cache: self.cache.clone(),
            manager: self.manager.clone(),
            guild_id: guild,
            channel_id: channel,
            tracker: SpeakerTracker::default(),
            ssrc_users: std::sync::Mutex::default(),
            connected: AtomicBool::new(true),
            listening: AtomicBool::new(true),
        });
        {
            let mut handler = call.lock().await;
            for event in [
                CoreEvent::SpeakingStateUpdate,
                CoreEvent::VoiceTick,
                CoreEvent::ClientDisconnect,
                CoreEvent::DriverConnect,
                CoreEvent::DriverReconnect,
                CoreEvent::DriverDisconnect,
            ] {
                handler.add_global_event(event.into(), Receiver(connection.clone()));
            }
        }
        Ok(connection)
    }
}

/// I finti per i test: registrano le chiamate come fa il finto `VoiceRecvClient` dei golden Python.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::overlay::VoiceMember;

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Il registro delle chiamate: `connect`, `listen`, `stop_listening`, `disconnect`.
    pub(crate) type CallLog = Arc<Mutex<Vec<String>>>;

    #[derive(Debug)]
    pub(crate) struct FakeConnection {
        pub members: Mutex<Vec<VoiceMember>>,
        pub levels: Mutex<HashMap<u64, f64>>,
        pub connected: AtomicBool,
        pub listening: AtomicBool,
        pub last_packet: Mutex<Option<Instant>>,
        pub disconnect_error: Mutex<Option<VoiceError>>,
        pub restart_error: Mutex<Option<VoiceError>>,
        log: CallLog,
    }

    impl FakeConnection {
        pub(crate) fn set_members(&self, ids: &[u64]) {
            *lock(&self.members) = ids.iter().map(|id| member(*id)).collect();
        }
        pub(crate) fn set_levels(&self, levels: &[(u64, f64)]) {
            *lock(&self.levels) = levels.iter().copied().collect();
        }
    }

    pub(crate) fn member(id: u64) -> VoiceMember {
        VoiceMember { id, name: format!("utente{id}"), avatar_url: format!("https://cdn/{id}.png") }
    }

    impl VoiceSource for FakeConnection {
        fn members(&self) -> Vec<VoiceMember> {
            lock(&self.members).clone()
        }
        fn levels(&self) -> HashMap<u64, f64> {
            lock(&self.levels).clone()
        }
    }

    impl VoiceConnection for FakeConnection {
        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::SeqCst)
        }
        fn is_listening(&self) -> bool {
            self.listening.load(Ordering::SeqCst)
        }
        fn last_packet_at(&self) -> Option<Instant> {
            *lock(&self.last_packet)
        }
        fn restart_listening(&self) -> Result<(), VoiceError> {
            lock(&self.log).push("listen".into());
            match lock(&self.restart_error).clone() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
        fn stop_listening(&self) {
            lock(&self.log).push("stop_listening".into());
        }
        fn disconnect(&self) -> BoxFuture<'_, Result<(), VoiceError>> {
            Box::pin(async move {
                if let Some(error) = lock(&self.disconnect_error).clone() {
                    return Err(error);
                }
                lock(&self.log).push("disconnect".into());
                self.connected.store(false, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    #[derive(Default)]
    struct GatewayState {
        bot_in_guild: bool,
        channels: HashMap<(u64, u64), VoiceChannelInfo>,
        user_channels: HashMap<(u64, u64), u64>,
        connect_error: Option<VoiceError>,
        /// Il collegamento riesce ma l'avvio dell'ascolto no: il gateway si scollega e fallisce.
        listen_error: Option<VoiceError>,
        members: Vec<u64>,
        connections: Vec<Arc<FakeConnection>>,
    }

    #[derive(Clone)]
    pub(crate) struct FakeGateway {
        state: Arc<Mutex<GatewayState>>,
        pub log: CallLog,
    }

    impl Default for FakeGateway {
        fn default() -> Self {
            Self { state: Arc::new(Mutex::new(GatewayState { bot_in_guild: true, ..GatewayState::default() })), log: CallLog::default() }
        }
    }

    impl FakeGateway {
        pub(crate) fn set_bot_in_guild(&self, present: bool) {
            lock(&self.state).bot_in_guild = present;
        }
        pub(crate) fn add_channel(&self, guild_id: u64, channel_id: u64, name: &str, can_connect: bool) {
            lock(&self.state).channels.insert((guild_id, channel_id), VoiceChannelInfo { name: name.into(), can_connect });
        }
        pub(crate) fn set_user_channel(&self, guild_id: u64, user_id: u64, channel_id: u64) {
            lock(&self.state).user_channels.insert((guild_id, user_id), channel_id);
        }
        pub(crate) fn fail_connect(&self, error: VoiceError) {
            lock(&self.state).connect_error = Some(error);
        }
        pub(crate) fn fail_listen(&self, error: VoiceError) {
            lock(&self.state).listen_error = Some(error);
        }
        /// Chi è nel canale quando il bot si collega (solo persone).
        pub(crate) fn set_members(&self, ids: &[u64]) {
            lock(&self.state).members = ids.to_vec();
        }
        pub(crate) fn calls(&self) -> Vec<String> {
            lock(&self.log).clone()
        }
        pub(crate) fn clear_calls(&self) {
            lock(&self.log).clear();
        }
        /// Un collegamento che non appartiene a nessuna sessione.
        pub(crate) fn detached_connection(&self) -> Arc<dyn VoiceConnection> {
            Arc::new(FakeConnection {
                members: Mutex::default(),
                levels: Mutex::default(),
                connected: AtomicBool::new(true),
                listening: AtomicBool::new(false),
                last_packet: Mutex::new(None),
                disconnect_error: Mutex::new(None),
                restart_error: Mutex::new(None),
                log: self.log.clone(),
            })
        }
        /// L'ultima connessione creata.
        pub(crate) fn connection(&self) -> Arc<FakeConnection> {
            lock(&self.state).connections.last().cloned().expect("nessun collegamento")
        }
    }

    impl VoiceGateway for FakeGateway {
        fn bot_in_guild(&self, _guild_id: u64) -> bool {
            lock(&self.state).bot_in_guild
        }
        fn channel_info(&self, guild_id: u64, channel_id: u64) -> Option<VoiceChannelInfo> {
            lock(&self.state).channels.get(&(guild_id, channel_id)).cloned()
        }
        fn user_voice_channel(&self, guild_id: u64, user_id: u64) -> Option<u64> {
            lock(&self.state).user_channels.get(&(guild_id, user_id)).copied()
        }
        async fn connect(&self, _guild_id: u64, _channel_id: u64) -> Result<Arc<dyn VoiceConnection>, VoiceError> {
            lock(&self.log).push("connect".into());
            let (connect_error, listen_error, members) = {
                let state = lock(&self.state);
                (state.connect_error.clone(), state.listen_error.clone(), state.members.clone())
            };
            if let Some(error) = connect_error {
                return Err(error);
            }
            lock(&self.log).push("listen".into());
            if let Some(error) = listen_error {
                lock(&self.log).push("disconnect".into());
                return Err(error);
            }
            let connection = Arc::new(FakeConnection {
                members: Mutex::new(members.iter().map(|id| member(*id)).collect()),
                levels: Mutex::default(),
                connected: AtomicBool::new(true),
                listening: AtomicBool::new(true),
                last_packet: Mutex::new(None),
                disconnect_error: Mutex::new(None),
                restart_error: Mutex::new(None),
                log: self.log.clone(),
            });
            lock(&self.state).connections.push(connection.clone());
            Ok(connection)
        }
    }
}

#[cfg(test)]
mod tests;
