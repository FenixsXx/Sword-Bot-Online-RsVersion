//! Operazioni sui messaggi dei canali, dietro un trait (il ruolo di `runtime.bot` e dei metodi di
//! `discord.TextChannel` nei moduli Python di sondaggi, dashboard e revisione).
//!
//! In produzione le esegue [`HttpDiscordApi`] sopra `serenity::Http`; nei test un finto registra le
//! chiamate e simula gli errori di Discord (messaggio sparito, permessi mancanti).
//!
//! Differenza da Python: non c'è la cache (`get_channel`) davanti alle chiamate. Un canale non più
//! accessibile compare come `NotFound`/`Forbidden` dalla chiamata stessa.
// Usato da `publisher` e dai moduli che aggiornano i messaggi nei canali.
#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;

use poise::serenity_prelude::{
    self as serenity, AutoArchiveDuration, ChannelId, CreateActionRow, CreateAllowedMentions, CreateEmbed,
    CreateMessage, CreateThread, EditMessage, GetMessages, MessageId,
};

/// Errori di Discord che il codice distingue: tutto il resto è `Http`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// 404: messaggio o canale inesistente (`discord.NotFound`).
    NotFound,
    /// 403: permessi mancanti (`discord.Forbidden`).
    Forbidden,
    /// Qualsiasi altro errore di rete o di Discord (`discord.HTTPException`).
    Http(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::NotFound => f.write_str("non trovato (404)"),
            ApiError::Forbidden => f.write_str("permessi mancanti (403)"),
            ApiError::Http(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ApiError {}

/// Contenuto di un messaggio. In invio i campi `None` sono omessi; in modifica `None` significa
/// "lascia com'è" e un valore vuoto (`Some("")`, `Some(vec![])`) "togli".
#[derive(Debug, Clone, Default)]
pub struct MessagePayload {
    pub content: Option<String>,
    pub embeds: Option<Vec<CreateEmbed>>,
    pub components: Option<Vec<CreateActionRow>>,
    /// Componenti V2 in JSON grezzo (vedi `v2`). Se presente sostituisce tutti gli altri campi:
    /// Discord vieta `content` ed `embeds` nei messaggi V2.
    pub v2: Option<Vec<serde_json::Value>>,
    /// Fa notificare le persone menzionate (di norma nessuna menzione notifica: `AllowedMentions.none()`).
    pub mention_users: bool,
}

impl MessagePayload {
    /// Un messaggio fatto solo di componenti V2.
    pub fn components_v2(components: Vec<serde_json::Value>) -> Self {
        Self { v2: Some(components), ..Self::default() }
    }

    pub fn embed(embed: CreateEmbed) -> Self {
        Self { embeds: Some(vec![embed]), ..Self::default() }
    }

    pub fn with_components(mut self, components: Vec<CreateActionRow>) -> Self {
        self.components = Some(components);
        self
    }

    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    /// Le menzioni di utenti (non di ruoli né `@everyone`) notificano.
    pub fn mentioning_users(mut self) -> Self {
        self.mention_users = true;
        self
    }
}

/// Un messaggio recente di un canale, quanto basta per ritrovare quello di un sondaggio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentMessage {
    pub id: u64,
    pub author_id: u64,
    /// Testo del piè di pagina del primo embed, se c'è.
    pub footer: Option<String>,
}

/// Tipo di un canale del server, quanto basta per scegliere dove creare un evento.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// Canale testuale o di annunci (`discord.TextChannel`).
    Text,
    Voice,
    Stage,
    /// Qualsiasi altro tipo, oppure un canale che non è di questo server.
    Other,
}

/// Un evento programmato da creare (`Guild.create_scheduled_event`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewScheduledEvent {
    pub name: String,
    /// Inizio e fine come secondi Unix (UTC).
    pub start_unix: i64,
    pub end_unix: i64,
    pub channel_id: u64,
    /// Evento in un canale Stage invece che in un canale vocale.
    pub stage: bool,
    pub description: String,
    /// Motivo mostrato nel registro di controllo di Discord.
    pub reason: String,
}

pub trait DiscordApi: Send + Sync + 'static {
    /// ID dell'utente del bot (per riconoscere i propri messaggi).
    fn bot_user_id(&self) -> u64;

    /// Invia un messaggio (nessuna menzione notifica qualcuno). Restituisce l'ID del messaggio.
    fn send_message(&self, channel_id: u64, payload: MessagePayload) -> impl Future<Output = Result<u64, ApiError>> + Send;

    fn edit_message(
        &self,
        channel_id: u64,
        message_id: u64,
        payload: MessagePayload,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    fn delete_message(&self, channel_id: u64, message_id: u64) -> impl Future<Output = Result<(), ApiError>> + Send;

    /// Gli ultimi `limit` messaggi del canale, dal più recente.
    fn recent_messages(&self, channel_id: u64, limit: u8) -> impl Future<Output = Result<Vec<RecentMessage>, ApiError>> + Send;

    /// Crea un thread da un messaggio (archiviazione automatica a 24 ore). Restituisce l'ID del thread.
    fn create_thread(&self, channel_id: u64, message_id: u64, name: String) -> impl Future<Output = Result<u64, ApiError>> + Send;

    /// Tipo del canale, se appartiene al server indicato (altrimenti `Other`).
    fn channel_kind(&self, guild_id: u64, channel_id: u64) -> impl Future<Output = Result<ChannelKind, ApiError>> + Send;

    /// Crea un evento programmato visibile a tutto il server. Restituisce l'ID dell'evento.
    fn create_scheduled_event(&self, guild_id: u64, event: NewScheduledEvent) -> impl Future<Output = Result<u64, ApiError>> + Send;

    /// Chi è collegato al canale vocale o Stage (`channel.members`), con e senza i bot. Si legge
    /// dalla cache del gateway: se il server o un membro non sono in cache, quel membro non conta.
    fn voice_member_counts(&self, guild_id: u64, channel_id: u64) -> VoiceCounts;

    /// Quanti utenti (bot esclusi) sono collegati al canale.
    fn voice_participants(&self, guild_id: u64, channel_id: u64) -> usize {
        self.voice_member_counts(guild_id, channel_id).humans
    }

    /// Gli eventi programmati del server (`guild.scheduled_events`).
    fn scheduled_events(&self, guild_id: u64) -> impl Future<Output = Result<Vec<ScheduledEventInfo>, ApiError>> + Send;
}

/// Quante persone ci sono in un canale vocale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VoiceCounts {
    /// Tutti i membri collegati, bot compresi.
    pub total: usize,
    /// Solo le persone (bot esclusi).
    pub humans: usize,
}

/// Lo stato di un evento programmato di Discord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStatus {
    Scheduled,
    Active,
    Completed,
    Canceled,
}

/// Quanto serve sapere di un evento programmato.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledEventInfo {
    pub id: u64,
    pub name: String,
    pub status: EventStatus,
    /// Il canale dell'evento (assente per gli eventi esterni).
    pub channel_id: Option<u64>,
}

/// Un evento programmato di serenity, ridotto a quanto serve.
pub fn scheduled_event_info(event: serenity::ScheduledEvent) -> ScheduledEventInfo {
    ScheduledEventInfo {
        id: event.id.get(),
        name: event.name,
        status: match event.status {
            serenity::ScheduledEventStatus::Scheduled => EventStatus::Scheduled,
            serenity::ScheduledEventStatus::Active => EventStatus::Active,
            serenity::ScheduledEventStatus::Completed => EventStatus::Completed,
            _ => EventStatus::Canceled,
        },
        channel_id: event.channel_id.map(|channel| channel.get()),
    }
}

/// L'implementazione di produzione, sopra `serenity::Http`.
#[derive(Clone)]
pub struct HttpDiscordApi {
    http: Arc<serenity::Http>,
    bot_user_id: u64,
    /// La cache del gateway (stati vocali e membri), per `voice_participants`.
    cache: Option<Arc<serenity::Cache>>,
}

impl HttpDiscordApi {
    pub fn new(http: Arc<serenity::Http>, bot_user_id: u64) -> Self {
        Self { http, bot_user_id, cache: None }
    }

    pub fn with_cache(mut self, cache: Arc<serenity::Cache>) -> Self {
        self.cache = Some(cache);
        self
    }
}

fn map_error(error: serenity::Error) -> ApiError {
    if let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response)) = &error {
        match response.status_code.as_u16() {
            404 => return ApiError::NotFound,
            403 => return ApiError::Forbidden,
            _ => {}
        }
    }
    ApiError::Http(error.to_string())
}

impl DiscordApi for HttpDiscordApi {
    fn bot_user_id(&self) -> u64 {
        self.bot_user_id
    }

    async fn send_message(&self, channel_id: u64, payload: MessagePayload) -> Result<u64, ApiError> {
        if let Some(components) = payload.v2 {
            // `Http::send_message` accetta qualsiasi corpo serializzabile: il JSON V2 passa così com'è.
            let body = crate::v2::message_body(components, false);
            let sent = self.http.send_message(ChannelId::new(channel_id), Vec::new(), &body).await.map_err(map_error)?;
            return Ok(sent.id.get());
        }
        let mentions = if payload.mention_users { CreateAllowedMentions::new().all_users(true) } else { CreateAllowedMentions::new() };
        let mut message = CreateMessage::new().allowed_mentions(mentions);
        if let Some(content) = payload.content {
            message = message.content(content);
        }
        if let Some(embeds) = payload.embeds {
            message = message.embeds(embeds);
        }
        if let Some(components) = payload.components {
            message = message.components(components);
        }
        let sent = ChannelId::new(channel_id).send_message(&self.http, message).await.map_err(map_error)?;
        Ok(sent.id.get())
    }

    async fn edit_message(&self, channel_id: u64, message_id: u64, payload: MessagePayload) -> Result<(), ApiError> {
        if let Some(components) = payload.v2 {
            let body = crate::v2::message_body(components, true);
            self.http
                .edit_message(ChannelId::new(channel_id), MessageId::new(message_id), &body, Vec::new())
                .await
                .map_err(map_error)?;
            return Ok(());
        }
        let mut edit = EditMessage::new().allowed_mentions(CreateAllowedMentions::new());
        if let Some(content) = payload.content {
            edit = edit.content(content);
        }
        if let Some(embeds) = payload.embeds {
            edit = edit.embeds(embeds);
        }
        if let Some(components) = payload.components {
            edit = edit.components(components);
        }
        ChannelId::new(channel_id).edit_message(&self.http, MessageId::new(message_id), edit).await.map_err(map_error)?;
        Ok(())
    }

    async fn delete_message(&self, channel_id: u64, message_id: u64) -> Result<(), ApiError> {
        ChannelId::new(channel_id).delete_message(&self.http, MessageId::new(message_id)).await.map_err(map_error)
    }

    async fn recent_messages(&self, channel_id: u64, limit: u8) -> Result<Vec<RecentMessage>, ApiError> {
        let messages = ChannelId::new(channel_id).messages(&self.http, GetMessages::new().limit(limit)).await.map_err(map_error)?;
        Ok(messages
            .into_iter()
            .map(|message| RecentMessage {
                id: message.id.get(),
                author_id: message.author.id.get(),
                footer: message.embeds.first().and_then(|embed| embed.footer.as_ref()).map(|footer| footer.text.clone()),
            })
            .collect())
    }

    async fn create_thread(&self, channel_id: u64, message_id: u64, name: String) -> Result<u64, ApiError> {
        let thread = ChannelId::new(channel_id)
            .create_thread_from_message(
                &self.http,
                MessageId::new(message_id),
                CreateThread::new(name).auto_archive_duration(AutoArchiveDuration::OneDay),
            )
            .await
            .map_err(map_error)?;
        Ok(thread.id.get())
    }

    async fn channel_kind(&self, guild_id: u64, channel_id: u64) -> Result<ChannelKind, ApiError> {
        let channel = self.http.get_channel(ChannelId::new(channel_id)).await.map_err(map_error)?;
        Ok(match channel {
            serenity::Channel::Guild(channel) if channel.guild_id.get() == guild_id => match channel.kind {
                serenity::ChannelType::Text | serenity::ChannelType::News => ChannelKind::Text,
                serenity::ChannelType::Voice => ChannelKind::Voice,
                serenity::ChannelType::Stage => ChannelKind::Stage,
                _ => ChannelKind::Other,
            },
            _ => ChannelKind::Other,
        })
    }

    async fn create_scheduled_event(&self, guild_id: u64, event: NewScheduledEvent) -> Result<u64, ApiError> {
        let timestamp = |unix: i64| serenity::Timestamp::from_unix_timestamp(unix).map_err(|e| ApiError::Http(e.to_string()));
        let kind = if event.stage { serenity::ScheduledEventType::StageInstance } else { serenity::ScheduledEventType::Voice };
        let builder = serenity::CreateScheduledEvent::new(kind, event.name, timestamp(event.start_unix)?)
            .end_time(timestamp(event.end_unix)?)
            .channel_id(ChannelId::new(event.channel_id))
            .description(event.description)
            .audit_log_reason(&event.reason);
        let created = serenity::GuildId::new(guild_id).create_scheduled_event(&self.http, builder).await.map_err(map_error)?;
        Ok(created.id.get())
    }

    fn voice_member_counts(&self, guild_id: u64, channel_id: u64) -> VoiceCounts {
        let Some(cache) = &self.cache else {
            return VoiceCounts::default();
        };
        let Some(guild) = cache.guild(serenity::GuildId::new(guild_id)) else {
            return VoiceCounts::default();
        };
        let mut counts = VoiceCounts::default();
        for (user, state) in &guild.voice_states {
            if !state.channel_id.is_some_and(|channel| channel.get() == channel_id) {
                continue;
            }
            let Some(member) = guild.members.get(user) else {
                continue;
            };
            counts.total += 1;
            if !member.user.bot {
                counts.humans += 1;
            }
        }
        counts
    }

    async fn scheduled_events(&self, guild_id: u64) -> Result<Vec<ScheduledEventInfo>, ApiError> {
        let events = serenity::GuildId::new(guild_id).scheduled_events(&self.http, false).await.map_err(map_error)?;
        Ok(events.into_iter().map(scheduled_event_info).collect())
    }
}

/// Finto per i test: tiene i messaggi in memoria e registra le chiamate.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Messaggio come lo vedrebbe Discord, con i builder già convertiti in JSON.
    #[derive(Debug, Clone, PartialEq)]
    pub(crate) struct StoredMessage {
        pub content: Option<String>,
        pub embeds: Vec<Value>,
        pub components: Vec<Value>,
        pub footer: Option<String>,
        /// Messaggio con componenti V2 (flag `IS_COMPONENTS_V2`).
        pub v2: bool,
        /// Le menzioni di utenti notificano.
        pub mentions_users: bool,
    }

    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum Call {
        Send { channel: u64, message: u64 },
        Edit { channel: u64, message: u64 },
        Delete { channel: u64, message: u64 },
        History { channel: u64 },
        Thread { channel: u64, message: u64, name: String },
        ScheduledEvent { guild: u64, event: u64 },
    }

    #[derive(Default)]
    struct State {
        next_id: u64,
        messages: HashMap<(u64, u64), StoredMessage>,
        authors: HashMap<(u64, u64), u64>,
        order: Vec<(u64, u64)>,
        calls: Vec<Call>,
        edit_error: Option<ApiError>,
        send_error: Option<ApiError>,
        history_error: Option<ApiError>,
        thread_error: Option<ApiError>,
        delete_error: Option<ApiError>,
        channel_kinds: HashMap<u64, ChannelKind>,
        channel_guilds: HashMap<u64, u64>,
        channel_errors: HashMap<u64, ApiError>,
        events: Vec<(u64, NewScheduledEvent)>,
        event_error: Option<ApiError>,
        voice: HashMap<(u64, u64), VoiceCounts>,
        guild_events: HashMap<u64, Vec<ScheduledEventInfo>>,
    }

    #[derive(Clone)]
    pub(crate) struct FakeApi {
        state: Arc<Mutex<State>>,
        pub(crate) bot_id: u64,
    }

    impl Default for FakeApi {
        fn default() -> Self {
            Self { state: Arc::new(Mutex::new(State { next_id: 1000, ..State::default() })), bot_id: 7 }
        }
    }

    impl FakeApi {
        fn state(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().unwrap()
        }

        pub(crate) fn calls(&self) -> Vec<Call> {
            self.state().calls.clone()
        }

        pub(crate) fn message(&self, channel: u64, message: u64) -> Option<StoredMessage> {
            self.state().messages.get(&(channel, message)).cloned()
        }

        /// I messaggi di un canale, dal più vecchio.
        pub(crate) fn messages_in(&self, channel: u64) -> Vec<StoredMessage> {
            let state = self.state();
            state.order.iter().filter(|(c, _)| *c == channel).filter_map(|key| state.messages.get(key).cloned()).collect()
        }

        /// Aggiunge un messaggio "già presente" nel canale, scritto da `author`.
        pub(crate) fn seed_message(&self, channel: u64, author: u64, footer: Option<&str>) -> u64 {
            let mut state = self.state();
            state.next_id += 1;
            let id = state.next_id;
            state.messages.insert(
                (channel, id),
                StoredMessage { content: None, embeds: vec![], components: vec![], footer: footer.map(str::to_string), v2: false, mentions_users: false },
            );
            state.authors.insert((channel, id), author);
            state.order.push((channel, id));
            id
        }

        pub(crate) fn fail_edit(&self, error: Option<ApiError>) {
            self.state().edit_error = error;
        }
        pub(crate) fn fail_send(&self, error: Option<ApiError>) {
            self.state().send_error = error;
        }
        pub(crate) fn fail_history(&self, error: Option<ApiError>) {
            self.state().history_error = error;
        }
        pub(crate) fn fail_thread(&self, error: Option<ApiError>) {
            self.state().thread_error = error;
        }
        pub(crate) fn fail_delete(&self, error: Option<ApiError>) {
            self.state().delete_error = error;
        }
        pub(crate) fn fail_event(&self, error: Option<ApiError>) {
            self.state().event_error = error;
        }
        /// Dichiara il tipo di un canale del server (i canali non dichiarati sono `Other`).
        pub(crate) fn set_channel_kind(&self, channel_id: u64, kind: ChannelKind) {
            self.state().channel_kinds.insert(channel_id, kind);
        }
        /// Fa fallire la lettura del tipo di un canale (canale non trovato, accesso negato…).
        pub(crate) fn fail_channel(&self, channel_id: u64, error: ApiError) {
            self.state().channel_errors.insert(channel_id, error);
        }
        /// Dichiara a quale server appartiene un canale (di default vale per qualsiasi server).
        pub(crate) fn set_channel_guild(&self, channel_id: u64, guild_id: u64) {
            self.state().channel_guilds.insert(channel_id, guild_id);
        }
        /// Gli eventi programmati creati finora, con il server.
        pub(crate) fn scheduled_events(&self) -> Vec<(u64, NewScheduledEvent)> {
            self.state().events.clone()
        }
        /// Quanti utenti (non bot) sono collegati a un canale vocale.
        pub(crate) fn set_voice_participants(&self, guild_id: u64, channel_id: u64, count: usize) {
            self.set_voice_counts(guild_id, channel_id, count, count);
        }
        /// Presenze in un canale vocale, con e senza i bot.
        pub(crate) fn set_voice_counts(&self, guild_id: u64, channel_id: u64, total: usize, humans: usize) {
            self.state().voice.insert((guild_id, channel_id), VoiceCounts { total, humans });
        }
        /// Gli eventi programmati del server.
        pub(crate) fn set_scheduled_events(&self, guild_id: u64, events: Vec<ScheduledEventInfo>) {
            self.state().guild_events.insert(guild_id, events);
        }
    }

    fn store(payload: &MessagePayload, previous: Option<&StoredMessage>) -> StoredMessage {
        if let Some(components) = &payload.v2 {
            // Un messaggio V2 non ha contenuto né embed (in modifica quelli vecchi si azzerano).
            return StoredMessage { content: None, embeds: vec![], components: components.clone(), footer: None, v2: true, mentions_users: false };
        }
        let embeds: Vec<Value> = match &payload.embeds {
            Some(embeds) => embeds.iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
            None => previous.map(|p| p.embeds.clone()).unwrap_or_default(),
        };
        let components: Vec<Value> = match &payload.components {
            Some(rows) => rows.iter().map(|r| serde_json::to_value(r).unwrap()).collect(),
            None => previous.map(|p| p.components.clone()).unwrap_or_default(),
        };
        let content = match &payload.content {
            Some(text) if text.is_empty() => None,
            Some(text) => Some(text.clone()),
            None => previous.and_then(|p| p.content.clone()),
        };
        let footer = embeds.first().and_then(|e| e["footer"]["text"].as_str()).map(str::to_string);
        StoredMessage { content, embeds, components, footer, v2: previous.is_some_and(|p| p.v2), mentions_users: payload.mention_users }
    }

    impl DiscordApi for FakeApi {
        fn bot_user_id(&self) -> u64 {
            self.bot_id
        }

        async fn send_message(&self, channel_id: u64, payload: MessagePayload) -> Result<u64, ApiError> {
            let mut state = self.state();
            if let Some(error) = state.send_error.clone() {
                return Err(error);
            }
            state.next_id += 1;
            let id = state.next_id;
            let message = store(&payload, None);
            state.messages.insert((channel_id, id), message);
            let bot = self.bot_id;
            state.authors.insert((channel_id, id), bot);
            state.order.push((channel_id, id));
            state.calls.push(Call::Send { channel: channel_id, message: id });
            Ok(id)
        }

        async fn edit_message(&self, channel_id: u64, message_id: u64, payload: MessagePayload) -> Result<(), ApiError> {
            let mut state = self.state();
            state.calls.push(Call::Edit { channel: channel_id, message: message_id });
            if let Some(error) = state.edit_error.clone() {
                return Err(error);
            }
            let previous = state.messages.get(&(channel_id, message_id)).cloned().ok_or(ApiError::NotFound)?;
            let updated = store(&payload, Some(&previous));
            state.messages.insert((channel_id, message_id), updated);
            Ok(())
        }

        async fn delete_message(&self, channel_id: u64, message_id: u64) -> Result<(), ApiError> {
            let mut state = self.state();
            state.calls.push(Call::Delete { channel: channel_id, message: message_id });
            if let Some(error) = state.delete_error.clone() {
                return Err(error);
            }
            state.messages.remove(&(channel_id, message_id)).map(|_| ()).ok_or(ApiError::NotFound)
        }

        async fn recent_messages(&self, channel_id: u64, limit: u8) -> Result<Vec<RecentMessage>, ApiError> {
            let mut state = self.state();
            state.calls.push(Call::History { channel: channel_id });
            if let Some(error) = state.history_error.clone() {
                return Err(error);
            }
            let ids: Vec<(u64, u64)> = state.order.iter().rev().filter(|(c, _)| *c == channel_id).copied().take(usize::from(limit)).collect();
            Ok(ids
                .into_iter()
                .filter_map(|key| {
                    let message = state.messages.get(&key)?;
                    Some(RecentMessage { id: key.1, author_id: state.authors[&key], footer: message.footer.clone() })
                })
                .collect())
        }

        async fn create_thread(&self, channel_id: u64, message_id: u64, name: String) -> Result<u64, ApiError> {
            let mut state = self.state();
            state.calls.push(Call::Thread { channel: channel_id, message: message_id, name });
            if let Some(error) = state.thread_error.clone() {
                return Err(error);
            }
            state.next_id += 1;
            Ok(state.next_id)
        }

        async fn channel_kind(&self, guild_id: u64, channel_id: u64) -> Result<ChannelKind, ApiError> {
            let state = self.state();
            if let Some(error) = state.channel_errors.get(&channel_id) {
                return Err(error.clone());
            }
            if state.channel_guilds.get(&channel_id).is_some_and(|guild| *guild != guild_id) {
                return Ok(ChannelKind::Other);
            }
            Ok(state.channel_kinds.get(&channel_id).copied().unwrap_or(ChannelKind::Other))
        }

        async fn create_scheduled_event(&self, guild_id: u64, event: NewScheduledEvent) -> Result<u64, ApiError> {
            let mut state = self.state();
            if let Some(error) = state.event_error.clone() {
                return Err(error);
            }
            state.next_id += 1;
            let id = state.next_id;
            state.events.push((guild_id, event));
            state.calls.push(Call::ScheduledEvent { guild: guild_id, event: id });
            Ok(id)
        }

        fn voice_member_counts(&self, guild_id: u64, channel_id: u64) -> VoiceCounts {
            self.state().voice.get(&(guild_id, channel_id)).copied().unwrap_or_default()
        }

        async fn scheduled_events(&self, guild_id: u64) -> Result<Vec<ScheduledEventInfo>, ApiError> {
            Ok(self.state().guild_events.get(&guild_id).cloned().unwrap_or_default())
        }
    }

    #[test]
    fn fake_stores_edits_and_reports_missing_messages() {
        let api = FakeApi::default();
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            let id = api.send_message(5, MessagePayload::default().with_content("ciao")).await.unwrap();
            assert_eq!(api.message(5, id).unwrap().content.as_deref(), Some("ciao"));
            api.edit_message(5, id, MessagePayload::embed(CreateEmbed::new().title("T"))).await.unwrap();
            let stored = api.message(5, id).unwrap();
            assert_eq!(stored.content.as_deref(), Some("ciao"), "in modifica il contenuto non indicato resta");
            assert_eq!(stored.embeds[0]["title"], "T");
            api.edit_message(5, id, MessagePayload::default().with_content("")).await.unwrap();
            assert_eq!(api.message(5, id).unwrap().content, None, "contenuto vuoto = tolto");
            assert_eq!(api.edit_message(5, 999, MessagePayload::default()).await, Err(ApiError::NotFound));
            assert_eq!(api.recent_messages(5, 10).await.unwrap()[0].author_id, 7);
            api.delete_message(5, id).await.unwrap();
            assert_eq!(api.delete_message(5, id).await, Err(ApiError::NotFound));
        });
        let _ = json!(null);
    }
}
