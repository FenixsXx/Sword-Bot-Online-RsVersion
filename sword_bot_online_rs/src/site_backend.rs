//! Le operazioni su Discord e sui servizi del bot che le route del sito (`site_reports`,
//! `site_tools`) chiedono: pubblicare un messaggio, aggiornare un sondaggio, notificare, registrare
//! nel log staff.
//!
//! Il server HTTP parte prima del gateway, quindi le route non conoscono ancora Discord: parlano con
//! un [`LateBackend`] che, a bot avviato, riceve il [`DiscordBackend`] vero. Prima di allora ogni
//! operazione fallisce con un errore di Discord (le route lo traducono in 5xx).
//!
//! ### Cos'è un "Backend" in questo contesto?
//!
//! Immagina che il sito web (le pagine che lo staff vede nel browser) sia la "reception" di un hotel.
//! Quando qualcuno alla reception deve fare qualcosa che richiede l'intervento dello staff interno
//! (mandare un messaggio su Discord, aggiornare un sondaggio, scrivere nel registro), la reception
//! non lo fa da sola: chiama il "backend".
//!
//! Il **Backend** è l'insieme di funzioni che permettono al sito web di compiere azioni reali su
//! Discord e sul database del bot. Poiché il sito web (server HTTP) si accende *prima* che il bot
//! si connetta a Discord, all'inizio il backend non esiste ancora. Per questo usiamo un sistema
//! a due livelli:
//! 1. **LateBackend** (backend "in ritardo"): un contenitore vuoto che risponde "non pronto" a tutto.
//! 2. **DiscordBackend** (backend vero): viene inserito nel contenitore solo quando il bot è online.
//!
//! ### Cos'è un "Trait" in Rust?
//!
//! Un `trait` è come un "contratto" o un'interfaccia: definisce *quali* funzioni devono esistere
//! e *che tipo* di dati accettano/restituiscono, ma non *come* le fanno. Qualsiasi struct che
//! implementa quel trait promette di fornire quelle funzioni. Qui `SiteBackend` è il contratto.

use std::sync::{Arc, OnceLock};

use tracing::warn;

use crate::cards::EmbedSpec;
use crate::discord_api::{ApiError, ChannelKind, DiscordApi, MessagePayload};
use crate::notify::{AuditEntry, DiscordSender, Notifier};
use crate::polls::RefreshScheduler;
use crate::publisher::{Publisher, PublisherEffects};
use crate::review::{ReviewOutcome, ReviewRequest, ReviewSource, apply_review};
use crate::voice::BoxFuture;

/// Il "contratto" che ogni backend deve rispettare.
/// Definisce tutte le azioni che il sito staff può chiedere di fare sul bot/Discord.
pub trait SiteBackend: Send + Sync + 'static {
    /// Scrive una riga nel registro audit (log delle azioni dello staff) e, se configurato,
    /// la pubblica anche in un canale Discord dedicato all'audit.
    fn audit(&self, entry: AuditEntry);

    /// Chiede a Discord di che tipo è un canale (testo, vocale, categoria, annuncio, ecc.).
    /// Restituisce un "Future" (un'operazione che finirà in futuro) perché è una chiamata di rete.
    fn channel_kind(&self, guild_id: u64, channel_id: u64) -> BoxFuture<'_, Result<ChannelKind, ApiError>>;

    /// Invia un "embed" (un messaggio formattato con titolo, colore, campi, immagine) in un canale.
    /// Restituisce l'ID del messaggio creato.
    fn send_embed(&self, channel_id: u64, embed: EmbedSpec) -> BoxFuture<'_, Result<u64, ApiError>>;

    /// Approva, rifiuta o rimette in revisione una proposta/suggerimento.
    /// `actor_id`: chi sta compiendo l'azione (ID numerico Discord).
    /// `action`: "approve", "reject", o "revoke".
    /// `reason`: la motivazione scritta dallo staff.
    fn apply_review(&self, actor_id: i64, suggestion_id: i64, action: String, reason: String) -> BoxFuture<'_, ReviewOutcome>;

    /// Forza l'aggiornamento di un sondaggio specifico (ricalcola voti, aggiorna il messaggio su Discord).
    fn refresh_poll(&self, poll_id: i64) -> BoxFuture<'_, Result<(), String>>;

    /// Forza l'aggiornamento della dashboard (il messaggio riassuntivo con tutti i sondaggi attivi).
    fn refresh_dashboard(&self) -> BoxFuture<'_, Result<(), String>>;

    /// Manda un messaggio privato (DM) a una lista di utenti che hanno attivato le notifiche.
    fn notify_users(&self, user_ids: Vec<u64>, text: String) -> BoxFuture<'_, ()>;

    /// Pubblica un annuncio nel canale "novità/attività" del server (se configurato).
    fn publish_activity(&self, text: String) -> BoxFuture<'_, ()>;

    /// Scrive una riga nel log visibile solo allo staff (canale Discord o solo database).
    fn staff_log(&self, text: String);
}

/// Il backend "vero", quello che ha accesso a tutti i servizi del bot attivi.
/// È generico su due interfacce: `DiscordSender` (per mandare cose su Discord) e `DiscordApi` (per chiedere cose a Discord).
pub struct DiscordBackend<S: DiscordSender, A: DiscordApi> {
    /// Il database SQLite per leggere/scrivere dati persistenti.
    pub db: crate::database::Database,
    /// Il sistema di notifiche (audit, log staff, DM, canale attività).
    pub notifier: Notifier<S>,
    /// Il "Publisher" che gestisce la pubblicazione e l'aggiornamento di messaggi complessi (sondaggi, dashboard).
    pub publisher: Arc<Publisher<A>>,
    /// Lo scheduler che decide quando aggiornare automaticamente i sondaggi.
    pub scheduler: RefreshScheduler,
}

/// Implementazione concreta del contratto `SiteBackend` usando i servizi reali del bot.
impl<S: DiscordSender, A: DiscordApi> SiteBackend for DiscordBackend<S, A> {
    fn audit(&self, entry: AuditEntry) {
        // Delega al notificatore: salva nel DB e, se c'è un canale audit, manda il messaggio su Discord.
        self.notifier.record_staff_audit(entry);
    }

    fn channel_kind(&self, guild_id: u64, channel_id: u64) -> BoxFuture<'_, Result<ChannelKind, ApiError>> {
        // Chiede all'API di Discord il tipo di canale. Box::pin trasforma la async fn in un BoxFuture.
        Box::pin(async move { self.publisher.api().channel_kind(guild_id, channel_id).await })
    }

    fn send_embed(&self, channel_id: u64, embed: EmbedSpec) -> BoxFuture<'_, Result<u64, ApiError>> {
        // Costruisce l'embed e lo manda tramite l'API Discord. Restituisce l'ID del messaggio.
        Box::pin(async move { self.publisher.api().send_message(channel_id, MessagePayload::embed(embed.build())).await })
    }

    fn apply_review(&self, actor_id: i64, suggestion_id: i64, action: String, reason: String) -> BoxFuture<'_, ReviewOutcome> {
        // Prepara gli "effetti collaterali" necessari (pubblicare messaggi, aggiornare scheduler).
        let effects = PublisherEffects { publisher: self.publisher.clone(), scheduler: self.scheduler.clone() };
        // Esegue la logica centrale di revisione (approva/rifiuta/ritira).
        // La richiesta di revisione viene costruita DENTRO l'async block perché `apply_review`
        // prende `&str` (borrow), ma il futuro deve essere autosufficiente (ownership).
        Box::pin(async move {
            let request = ReviewRequest { actor_id, suggestion_id, action: &action, reason: Some(&reason), source: ReviewSource::Site };
            apply_review(&self.db, &self.notifier, &effects, request).await
        })
    }

    fn refresh_poll(&self, poll_id: i64) -> BoxFuture<'_, Result<(), String>> {
        // Delega al publisher l'aggiornamento forzato di quel sondaggio.
        Box::pin(async move { self.publisher.refresh_poll(poll_id).await.map_err(|e| e.to_string()) })
    }

    fn refresh_dashboard(&self) -> BoxFuture<'_, Result<(), String>> {
        // Delega al publisher l'aggiornamento forzato della dashboard.
        Box::pin(async move { self.publisher.refresh_dashboard().await.map_err(|e| e.to_string()) })
    }

    fn notify_users(&self, user_ids: Vec<u64>, text: String) -> BoxFuture<'_, ()> {
        // Manda notifiche private (DM) agli utenti specificati.
        Box::pin(async move { self.notifier.send_personal_notifications(&user_ids, &text).await })
    }

    fn publish_activity(&self, text: String) -> BoxFuture<'_, ()> {
        // Pubblica nel canale attività/novità.
        Box::pin(async move { self.notifier.publish_activity(text, None).await })
    }

    fn staff_log(&self, text: String) {
        // Scrive nel log staff (solitamente un canale Discord dedicato).
        self.notifier.staff_log(text);
    }
}

/// Un "contenitore" che aspetta che il backend vero sia pronto.
/// All'inizio è vuoto. Quando il bot finisce di avviarsi, qualcuno chiama `set()` per metterci dentro il `DiscordBackend`.
/// Finché è vuoto, tutte le operazioni rispondono "non pronto" (errore 503 Service Unavailable).
///
/// `OnceLock` è una cella di memoria che può essere scritta **una sola volta**. È thread-safe.
#[derive(Default)]
pub struct LateBackend {
    inner: OnceLock<Arc<dyn SiteBackend>>,
}

impl LateBackend {
    /// Inserisce il backend vero. Se chiamato una seconda volta, non fa nulla (OnceLock).
    pub fn set(&self, backend: Arc<dyn SiteBackend>) {
        let _ = self.inner.set(backend);
    }
}

/// Crea un errore standard "servizio non disponibile" (HTTP 503).
fn not_ready() -> ApiError {
    ApiError::Http("il bot non è ancora pronto".into())
}

/// Il `LateBackend` implementa lo stesso contratto `SiteBackend`, ma delega al contenuto se c'è,
/// altrimenti restituisce errore "non pronto".
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
            None => Box::pin(async {}), // Silenzioso: se il bot non è pronto, le notifiche vanno perse.
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
        // Se il bot non è pronto, il log staff va perso silenziosamente.
    }
}