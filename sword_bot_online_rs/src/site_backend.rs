//! Le operazioni su Discord e sui servizi del bot che le route del sito (`site_reports`,
//! `site_tools`) chiedono: pubblicare un messaggio, aggiornare un sondaggio, notificare, registrare
//! nel log staff.
//!
//! Il server HTTP parte prima del gateway, quindi le route non conoscono ancora Discord: parlano con
//! un [`LateBackend`] che, a bot avviato, riceve il [`DiscordBackend`] vero. Prima di allora ogni
//! operazione fallisce con un errore di Discord (le route lo traducono in 5xx).

use std::sync::{Arc, OnceLock};

use tracing::warn;

use crate::cards::EmbedSpec;
use crate::discord_api::{ApiError, ChannelKind, DiscordApi, MessagePayload};
use crate::notify::{AuditEntry, DiscordSender, Notifier};
use crate::polls::RefreshScheduler;
use crate::publisher::{Publisher, PublisherEffects};
use crate::review::{ReviewOutcome, ReviewRequest, ReviewSource, apply_review};
use crate::voice::BoxFuture;

pub trait SiteBackend: Send + Sync + 'static {
    /// Scrive una riga nel registro audit (e la pubblica nel canale dell'audit, se c'è).
    fn audit(&self, entry: AuditEntry);
    /// Il tipo di un canale del server.
    fn channel_kind(&self, guild_id: u64, channel_id: u64) -> BoxFuture<'_, Result<ChannelKind, ApiError>>;
    /// Invia un embed in un canale; restituisce l'ID del messaggio.
    fn send_embed(&self, channel_id: u64, embed: EmbedSpec) -> BoxFuture<'_, Result<u64, ApiError>>;
    /// Approva, rifiuta o rimette in revisione una proposta (`core.apply_review`, origine «site»).
    fn apply_review(&self, actor_id: i64, suggestion_id: i64, action: String, reason: String) -> BoxFuture<'_, ReviewOutcome>;
    fn refresh_poll(&self, poll_id: i64) -> BoxFuture<'_, Result<(), String>>;
    fn refresh_dashboard(&self) -> BoxFuture<'_, Result<(), String>>;
    /// Messaggio privato a chi ha attivato le notifiche.
    fn notify_users(&self, user_ids: Vec<u64>, text: String) -> BoxFuture<'_, ()>;
    /// Un annuncio nel canale delle novità.
    fn publish_activity(&self, text: String) -> BoxFuture<'_, ()>;
    fn staff_log(&self, text: String);
}

/// Il backend vero, sopra i servizi del bot.
pub struct DiscordBackend<S: DiscordSender, A: DiscordApi> {
    pub db: crate::database::Database,
    pub notifier: Notifier<S>,
    pub publisher: Arc<Publisher<A>>,
    pub scheduler: RefreshScheduler,
}

impl<S: DiscordSender, A: DiscordApi> SiteBackend for DiscordBackend<S, A> {
    fn audit(&self, entry: AuditEntry) {
        self.notifier.record_staff_audit(entry);
    }

    fn channel_kind(&self, guild_id: u64, channel_id: u64) -> BoxFuture<'_, Result<ChannelKind, ApiError>> {
        Box::pin(async move { self.publisher.api().channel_kind(guild_id, channel_id).await })
    }

    fn send_embed(&self, channel_id: u64, embed: EmbedSpec) -> BoxFuture<'_, Result<u64, ApiError>> {
        Box::pin(async move { self.publisher.api().send_message(channel_id, MessagePayload::embed(embed.build())).await })
    }

    fn apply_review(&self, actor_id: i64, suggestion_id: i64, action: String, reason: String) -> BoxFuture<'_, ReviewOutcome> {
        Box::pin(async move {
            let effects = PublisherEffects { publisher: self.publisher.clone(), scheduler: self.scheduler.clone() };
            let request = ReviewRequest { actor_id, suggestion_id, action: &action, reason: Some(&reason), source: ReviewSource::Site };
            apply_review(&self.db, &self.notifier, &effects, request).await
        })
    }

    fn refresh_poll(&self, poll_id: i64) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move { self.publisher.refresh_poll(poll_id).await.map_err(|e| e.to_string()) })
    }

    fn refresh_dashboard(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move { self.publisher.refresh_dashboard().await.map_err(|e| e.to_string()) })
    }

    fn notify_users(&self, user_ids: Vec<u64>, text: String) -> BoxFuture<'_, ()> {
        Box::pin(async move { self.notifier.send_personal_notifications(&user_ids, &text).await })
    }

    fn publish_activity(&self, text: String) -> BoxFuture<'_, ()> {
        Box::pin(async move { self.notifier.publish_activity(text, None).await })
    }

    fn staff_log(&self, text: String) {
        self.notifier.staff_log(text);
    }
}

/// Il backend collegato a bot avviato.
#[derive(Default)]
pub struct LateBackend {
    inner: OnceLock<Arc<dyn SiteBackend>>,
}

impl LateBackend {
    pub fn set(&self, backend: Arc<dyn SiteBackend>) {
        let _ = self.inner.set(backend);
    }
}

fn not_ready() -> ApiError {
    ApiError::Http("il bot non è ancora pronto".into())
}

impl SiteBackend for LateBackend {
    fn audit(&self, entry: AuditEntry) {
        match self.inner.get() {
            Some(backend) => backend.audit(entry),
            None => warn!("Audit perso: il bot non è ancora pronto."),
        }
    }

    fn channel_kind(&self, guild_id: u64, channel_id: u64) -> BoxFuture<'_, Result<ChannelKind, ApiError>> {
        match self.inner.get() {
            Some(backend) => backend.channel_kind(guild_id, channel_id),
            None => Box::pin(async { Err(not_ready()) }),
        }
    }

    fn send_embed(&self, channel_id: u64, embed: EmbedSpec) -> BoxFuture<'_, Result<u64, ApiError>> {
        match self.inner.get() {
            Some(backend) => backend.send_embed(channel_id, embed),
            None => Box::pin(async { Err(not_ready()) }),
        }
    }

    fn apply_review(&self, actor_id: i64, suggestion_id: i64, action: String, reason: String) -> BoxFuture<'_, ReviewOutcome> {
        match self.inner.get() {
            Some(backend) => backend.apply_review(actor_id, suggestion_id, action, reason),
            None => Box::pin(async {
                ReviewOutcome { ok: false, status_code: 503, message: "Il bot non è ancora pronto".into(), review_status: None, name: None }
            }),
        }
    }

    fn refresh_poll(&self, poll_id: i64) -> BoxFuture<'_, Result<(), String>> {
        match self.inner.get() {
            Some(backend) => backend.refresh_poll(poll_id),
            None => Box::pin(async { Err("il bot non è ancora pronto".to_string()) }),
        }
    }

    fn refresh_dashboard(&self) -> BoxFuture<'_, Result<(), String>> {
        match self.inner.get() {
            Some(backend) => backend.refresh_dashboard(),
            None => Box::pin(async { Err("il bot non è ancora pronto".to_string()) }),
        }
    }

    fn notify_users(&self, user_ids: Vec<u64>, text: String) -> BoxFuture<'_, ()> {
        match self.inner.get() {
            Some(backend) => backend.notify_users(user_ids, text),
            None => Box::pin(async {}),
        }
    }

    fn publish_activity(&self, text: String) -> BoxFuture<'_, ()> {
        match self.inner.get() {
            Some(backend) => backend.publish_activity(text),
            None => Box::pin(async {}),
        }
    }

    fn staff_log(&self, text: String) {
        if let Some(backend) = self.inner.get() {
            backend.staff_log(text);
        }
    }
}
