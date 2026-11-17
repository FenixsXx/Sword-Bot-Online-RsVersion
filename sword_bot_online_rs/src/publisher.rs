//! Aggiornamento dei messaggi nei canali: sondaggi, dashboard e schede di revisione
//! (`refresh_poll`, `ensure_poll_thread` di `sfo/polls.py`; `refresh_dashboard`, `refresh_all` di
//! `sfo/dashboard.py`; `post_review_card`, `update_review_message` di `sfo/review.py`).
//!
//! Tutto passa da [`DiscordApi`], quindi si prova con un finto senza Discord. L'orologio è un
//! parametro (`clock`) per poter simulare qualsiasi istante.
//!
//! Differenze da Python:
//! * niente cache (`get_channel`): un canale inaccessibile si vede dalla prima chiamata, non prima;
//! * il messaggio di un sondaggio si ritrova nella cronologia dal piè di pagina `POLL_ID=<id>`
//!   *esatto*: in Python `POLL_ID=1` era contenuto anche in `POLL_ID=12` e il sondaggio 1 poteva
//!   appropriarsi del messaggio del 12.
// Verrà usato dal reset, dai pulsanti e dai comandi.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::DateTime;
use chrono_tz::Tz;
use tracing::{error, info, warn};

use crate::cards::build_embed;
use crate::database::Database;
use crate::discord_api::{ApiError, DiscordApi, MessagePayload};
use crate::formatting::truncate_chars;
use crate::hub::{hub_card, hub_components, hub_data, hub_embed};
use crate::polls::{
    PollRow, RefreshScheduler, build_view, ensure_closes_at, find_poll, list_active_polls,
    release_poll_message_claim, set_poll_message_id, set_poll_thread_id, try_claim_poll_message,
};
use crate::review::{ReviewEffects, build_review_card, review_message_location, save_review_message};
use crate::schedule;
use crate::v2::hub_v2_components;

/// Errore di un aggiornamento: database o Discord.
#[derive(Debug)]
pub enum PublishError {
    Db(rusqlite::Error),
    Api(ApiError),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishError::Db(error) => write!(f, "database: {error}"),
            PublishError::Api(error) => write!(f, "Discord: {error}"),
        }
    }
}

impl std::error::Error for PublishError {}

impl From<rusqlite::Error> for PublishError {
    fn from(error: rusqlite::Error) -> Self {
        PublishError::Db(error)
    }
}

impl From<ApiError> for PublishError {
    fn from(error: ApiError) -> Self {
        PublishError::Api(error)
    }
}

/// Come si presenta la dashboard.
///
/// * `Components2` (predefinito): riquadro con sezioni e miniatura, come in Python. Usa i Components V2
///   di Discord, inviati come JSON grezzo (serenity non ha i tipi V2 ma legge i clic: vedi `v2`).
/// * `Classic`: embed con pulsanti e menu tradizionali. Si attiva con l'impostazione
///   `dashboard_style` = `classic`, ad esempio se Discord cambiasse il comportamento dei V2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardStyle {
    Components2,
    Classic,
}

/// Un ID Discord salvato come testo nelle impostazioni o nel database.
fn parse_id(raw: Option<&str>) -> Option<u64> {
    raw.and_then(|text| text.trim().parse::<u64>().ok()).filter(|id| *id != 0)
}

pub struct Publisher<A: DiscordApi> {
    api: A,
    db: Database,
    clock: fn() -> DateTime<Tz>,
    poll_locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    dashboard_lock: tokio::sync::Mutex<()>,
}

impl<A: DiscordApi> Publisher<A> {
    pub fn new(api: A, db: Database) -> Self {
        Self::with_clock(api, db, schedule::now)
    }

    pub fn with_clock(api: A, db: Database, clock: fn() -> DateTime<Tz>) -> Self {
        Self { api, db, clock, poll_locks: Mutex::new(HashMap::new()), dashboard_lock: tokio::sync::Mutex::new(()) }
    }

    /// L'API Discord usata per i messaggi (anche per il riepilogo e il promemoria del reset).
    pub fn api(&self) -> &A {
        &self.api
    }

    fn poll_channel(&self) -> Option<u64> {
        parse_id(self.db.get_setting("poll_channel").ok().flatten().as_deref())
    }

    fn poll_lock(&self, poll_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.poll_locks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(locks.entry(poll_id).or_default())
    }

    // --- sondaggi ---

    /// Prende il lock del sondaggio (`refresh_locks[poll_id]` di Python): finché è tenuto, nessun
    /// aggiornamento del suo messaggio può partire.
    pub async fn lock_poll(&self, poll_id: i64) -> tokio::sync::OwnedMutexGuard<()> {
        self.poll_lock(poll_id).lock_owned().await
    }

    /// Dimentica il lock di un sondaggio eliminato (`refresh_locks.pop`).
    pub fn forget_poll_lock(&self, poll_id: i64) {
        self.poll_locks.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&poll_id);
    }

    /// Aggiorna (o ricrea) il messaggio di un sondaggio nel canale.
    pub async fn refresh_poll(&self, poll_id: i64) -> Result<(), PublishError> {
        let active = |poll: &Option<PollRow>| poll.as_ref().is_some_and(PollRow::is_active);
        if !active(&find_poll(&self.db, poll_id)?) || self.poll_channel().is_none() {
            return Ok(());
        }
        let lock = self.poll_lock(poll_id);
        let _guard = lock.lock().await;
        let now = (self.clock)();
        let (Some(mut poll), Some(channel)) = (find_poll(&self.db, poll_id)?, self.poll_channel()) else {
            return Ok(());
        };
        if !poll.is_active() {
            return Ok(());
        }
        if poll.closes_at.is_none() {
            ensure_closes_at(&self.db, poll_id, now)?;
            let Some(reloaded) = find_poll(&self.db, poll_id)? else {
                return Ok(());
            };
            poll = reloaded;
        }
        let payload = MessagePayload::embed(build_embed(&self.db, &poll, now)?.build()).with_components(build_view(&self.db, &poll)?);

        if let Some(message_id) = parse_id(poll.message_id.as_deref()) {
            match self.api.edit_message(channel, message_id, payload.clone()).await {
                Ok(()) => {
                    self.ensure_poll_thread(channel, message_id, &poll).await;
                    return Ok(());
                }
                // Il messaggio non c'è più: si ricrea.
                Err(ApiError::NotFound) => {}
                Err(other) => return Err(other.into()),
            }
        }

        let now_ts = now.timestamp_millis() as f64 / 1000.0;
        if !try_claim_poll_message(&self.db, poll_id, now_ts).inspect_err(|e| {
            error!("Impossibile acquisire il lock persistente per il sondaggio {poll_id}: {e}");
        })? {
            return Ok(()); // un altro refresh sta già creando il messaggio
        }
        let result = self.publish_new_poll_message(channel, &poll, payload).await;
        if let Err(e) = release_poll_message_claim(&self.db, poll_id) {
            error!("Impossibile rilasciare il lock del sondaggio {poll_id}: {e}");
        }
        result
    }

    /// Ritrova il messaggio del sondaggio nelle ultime 100 righe del canale, o ne crea uno.
    async fn publish_new_poll_message(&self, channel: u64, poll: &PollRow, payload: MessagePayload) -> Result<(), PublishError> {
        let marker = format!("POLL_ID={}", poll.id);
        let recent = match self.api.recent_messages(channel, 100).await {
            Ok(recent) => recent,
            Err(ApiError::NotFound | ApiError::Forbidden) => {
                warn!("Canale sondaggi {channel} non trovato o non accessibile; impossibile aggiornare il sondaggio {}.", poll.id);
                return Ok(());
            }
            Err(other) => return Err(other.into()),
        };
        let bot = self.api.bot_user_id();
        let existing = recent.iter().find(|message| {
            message.author_id == bot
                && message.footer.as_deref().is_some_and(|footer| footer == marker || footer.ends_with(&format!(" {marker}")))
        });
        if let Some(message) = existing {
            set_poll_message_id(&self.db, poll.id, message.id)?;
            self.api.edit_message(channel, message.id, payload).await?;
            return Ok(());
        }
        let message_id = self.api.send_message(channel, payload).await?;
        set_poll_message_id(&self.db, poll.id, message_id)?;
        self.ensure_poll_thread(channel, message_id, poll).await;
        Ok(())
    }

    /// Crea il thread di discussione del sondaggio se non c'è. Un errore viene solo loggato.
    pub async fn ensure_poll_thread(&self, channel: u64, message_id: u64, poll: &PollRow) {
        if poll.thread_id.as_deref().is_some_and(|id| !id.is_empty()) {
            return;
        }
        let name = truncate_chars(&format!("{} · Discussione", poll.name), 100);
        match self.api.create_thread(channel, message_id, name).await {
            Ok(thread_id) => {
                if let Err(e) = set_poll_thread_id(&self.db, poll.id, thread_id) {
                    error!("Impossibile salvare il thread del sondaggio {}: {e}", poll.id);
                }
            }
            Err(e) => error!("Impossibile creare il thread per il sondaggio {}: {e}", poll.id),
        }
    }


    /// Elimina un sondaggio: il messaggio nel canale (se c'è) e poi, in una transazione, le proposte
    /// scelte tornano «in attesa», il sondaggio e il suo lock di pubblicazione spariscono.
    ///
    /// `None` se il sondaggio non esiste (più); altrimenti `Some(true)` se anche il messaggio è stato
    /// eliminato (o non c'era più), `Some(false)` se Discord non ha permesso di toglierlo.
    pub async fn delete_poll(&self, poll_id: i64) -> Result<Option<bool>, PublishError> {
        let lock = self.poll_lock(poll_id);
        let result = {
            let _guard = lock.lock().await;
            self.delete_poll_locked(poll_id).await
        };
        // Come `refresh_locks.pop` di Python: il lock di un sondaggio sparito non serve più.
        if find_poll(&self.db, poll_id).ok().flatten().is_none() {
            self.poll_locks.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&poll_id);
        }
        result
    }

    async fn delete_poll_locked(&self, poll_id: i64) -> Result<Option<bool>, PublishError> {
        let Some(poll) = find_poll(&self.db, poll_id)? else {
            return Ok(None);
        };
        let mut message_deleted = true;
        if let (Some(message), Some(channel)) = (parse_id(poll.message_id.as_deref()), self.poll_channel()) {
            match self.api.delete_message(channel, message).await {
                Ok(()) | Err(ApiError::NotFound) => {}
                Err(e) => {
                    message_deleted = false;
                    error!("Impossibile eliminare il messaggio Discord del sondaggio {poll_id}: {e}");
                }
            }
        }
        let now = (self.clock)();
        self.db.transaction(|tx| -> Result<(), rusqlite::Error> {
            crate::suggestions::release_poll_suggestions(tx, poll_id, now)?;
            tx.execute("DELETE FROM polls WHERE id=?", [poll_id])?;
            tx.execute("DELETE FROM poll_message_claims WHERE poll_id=?", [poll_id])?;
            Ok(())
        })?;
        Ok(Some(message_deleted))
    }

    /// Chiude i sondaggi scaduti e ne aggiorna il messaggio e la dashboard (`close_expired_polls`
    /// di Python). Restituisce quanti ne ha chiusi.
    pub async fn close_expired_polls(&self) -> Result<usize, PublishError> {
        let closed = crate::polls::close_expired_polls(&self.db, (self.clock)())?;
        for poll in &closed {
            self.update_closed_poll_message(poll).await;
        }
        if !closed.is_empty() {
            self.refresh_dashboard().await?;
        }
        Ok(closed.len())
    }

    /// Aggiorna il messaggio di un sondaggio appena chiuso (senza ricrearlo se non c'è più).
    async fn update_closed_poll_message(&self, poll: &PollRow) {
        let (Some(message), Some(channel)) = (parse_id(poll.message_id.as_deref()), self.poll_channel()) else {
            return;
        };
        let now = (self.clock)();
        let payload = match (build_embed(&self.db, poll, now), build_view(&self.db, poll)) {
            (Ok(embed), Ok(view)) => MessagePayload::embed(embed.build()).with_components(view),
            (Err(e), _) => {
                error!("Impossibile costruire il messaggio del sondaggio {}: {e}", poll.id);
                return;
            }
            (_, Err(e)) => {
                error!("Impossibile costruire i pulsanti del sondaggio {}: {e}", poll.id);
                return;
            }
        };
        if let Err(e) = self.api.edit_message(channel, message, payload).await {
            error!("Sondaggio {} chiuso nel database, ma il messaggio Discord non è stato aggiornato: {e}", poll.id);
        }
    }

    // --- dashboard ---

    /// Lo stile della dashboard: V2 se l'impostazione `dashboard_style` non dice `classic`.
    pub fn dashboard_style(&self) -> DashboardStyle {
        match self.db.get_setting("dashboard_style") {
            Ok(Some(value)) if value.trim().eq_ignore_ascii_case("classic") => DashboardStyle::Classic,
            _ => DashboardStyle::Components2,
        }
    }

    /// Aggiorna la dashboard nel canale sondaggi; se il messaggio salvato non c'è più (o non si può
    /// modificare) ne pubblica uno nuovo e ricorda dove.
    pub async fn refresh_dashboard(&self) -> Result<(), PublishError> {
        let Some(channel) = self.poll_channel() else {
            return Ok(());
        };
        let _guard = self.dashboard_lock.lock().await;
        let now = (self.clock)();
        let card = hub_card(&hub_data(&self.db, now)?);
        // Il messaggio (da inviare o da sostituire) nello stile scelto: V2 come in Python, oppure classico.
        let payload = match self.dashboard_style() {
            DashboardStyle::Components2 => MessagePayload::components_v2(hub_v2_components(&card)),
            DashboardStyle::Classic => MessagePayload::embed(hub_embed(&card).build()).with_components(hub_components()),
        };

        let saved_message = parse_id(self.db.get_setting("dashboard_message")?.as_deref());
        let saved_channel = self.db.get_setting("dashboard_channel")?;
        if let Some(message_id) = saved_message.filter(|_| saved_channel.as_deref() == Some(channel.to_string().as_str())) {
            // Modifica diretta senza leggere prima il messaggio: una chiamata API invece di due.
            // In modifica si toglie anche l'eventuale contenuto vecchio (`content=None`).
            let edit = MessagePayload { content: payload.v2.is_none().then(String::new), ..payload.clone() };
            match self.api.edit_message(channel, message_id, edit).await {
                Ok(()) => return Ok(()),
                Err(ApiError::NotFound) => {}
                Err(other) => {
                    // Una vecchia dashboard non sempre si può convertire: si sostituisce.
                    info!("Sostituisco la vecchia dashboard {message_id} con la nuova ({other}).");
                    if let Err(e) = self.api.delete_message(channel, message_id).await {
                        info!("Vecchia dashboard {message_id} non eliminata: {e}");
                    }
                }
            }
        }
        match self.api.send_message(channel, payload).await {
            Ok(message_id) => {
                self.db.set_setting("dashboard_channel", channel)?;
                self.db.set_setting("dashboard_message", message_id)?;
                Ok(())
            }
            Err(e) => {
                error!("Impossibile aggiornare la dashboard nel canale sondaggi {channel}: {e}");
                Ok(())
            }
        }
    }

    /// Aggiorna tutti i sondaggi attivi e poi la dashboard. Un errore su un sondaggio non ferma gli altri.
    pub async fn refresh_all(&self) -> Result<(), PublishError> {
        for poll in list_active_polls(&self.db, None)? {
            if let Err(e) = self.refresh_poll(poll.id).await {
                error!("Impossibile aggiornare il sondaggio {}: {e}", poll.id);
            }
        }
        self.refresh_dashboard().await
    }

    // --- schede di revisione ---

    fn staff_channel(&self) -> Option<u64> {
        parse_id(self.db.get_setting("staff_channel").ok().flatten().as_deref())
    }

    /// Pubblica la scheda di revisione di una nuova proposta nel canale staff.
    pub async fn post_review_card(&self, suggestion_id: i64) -> Result<(), PublishError> {
        let Some(channel) = self.staff_channel() else {
            return Ok(());
        };
        let Some(card) = build_review_card(&self.db, suggestion_id, None)? else {
            return Ok(());
        };
        let payload = MessagePayload::embed(card.embed.clone()).with_components(card.components());
        match self.api.send_message(channel, payload).await {
            Ok(message_id) => Ok(save_review_message(&self.db, suggestion_id, channel, message_id)?),
            Err(e) => {
                error!("Impossibile pubblicare la scheda di revisione {suggestion_id}: {e}");
                Ok(())
            }
        }
    }

    /// Allinea la scheda nel canale staff allo stato attuale (o la chiude se la proposta non c'è più).
    pub async fn update_review_message(&self, suggestion_id: i64, note: Option<&str>, location: Option<(String, String)>) {
        let location = match location {
            Some(location) => Some(location),
            None => review_message_location(&self.db, suggestion_id).unwrap_or_else(|e| {
                error!("Impossibile leggere la posizione della scheda {suggestion_id}: {e}");
                None
            }),
        };
        let Some((channel, message)) = location else {
            return;
        };
        let (Some(channel), Some(message)) = (parse_id(Some(&channel)), parse_id(Some(&message))) else {
            return;
        };
        let payload = match build_review_card(&self.db, suggestion_id, note) {
            Ok(Some(card)) => MessagePayload::embed(card.embed.clone()).with_components(card.components()),
            Ok(None) => MessagePayload {
                content: Some(note.filter(|note| !note.is_empty()).unwrap_or("Proposta non più disponibile.").to_string()),
                embeds: Some(Vec::new()),
                components: Some(Vec::new()),
                ..MessagePayload::default()
            },
            Err(e) => {
                error!("Impossibile costruire la scheda di revisione {suggestion_id}: {e}");
                return;
            }
        };
        match self.api.edit_message(channel, message, payload).await {
            Ok(()) | Err(ApiError::NotFound) => {}
            Err(e) => error!("Impossibile aggiornare la scheda di revisione {suggestion_id}: {e}"),
        }
    }

    // --- pianificazione ---

    /// Pianifica un aggiornamento differito (e raggruppato) del sondaggio.
    pub fn schedule_poll_refresh(self: &Arc<Self>, scheduler: &RefreshScheduler, poll_id: i64) -> bool {
        let this = Arc::clone(self);
        scheduler.schedule_poll_refresh(poll_id, move || async move { this.refresh_poll(poll_id).await })
    }

    /// Pianifica un aggiornamento differito (e raggruppato) della dashboard.
    pub fn schedule_dashboard_refresh(self: &Arc<Self>, scheduler: &RefreshScheduler) -> bool {
        let this = Arc::clone(self);
        scheduler.schedule("dashboard", move || async move { this.refresh_dashboard().await })
    }
}

/// Gli effetti Discord di una revisione (`ReviewEffects`) con il `Publisher` vero.
pub struct PublisherEffects<A: DiscordApi> {
    pub publisher: Arc<Publisher<A>>,
    pub scheduler: RefreshScheduler,
}

impl<A: DiscordApi> ReviewEffects for PublisherEffects<A> {
    fn schedule_dashboard_refresh(&self) {
        self.publisher.schedule_dashboard_refresh(&self.scheduler);
    }

    async fn update_review_message(&self, suggestion_id: i64) {
        self.publisher.update_review_message(suggestion_id, None, None).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::discord_api::testing::{Call, FakeApi};
    use chrono::TimeZone;
    use std::time::Duration;

    const POLL_CHANNEL: u64 = 500;
    const STAFF_CHANNEL: u64 = 600;

    fn clock() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()
    }

    struct Fixture {
        db: Database,
        api: FakeApi,
        publisher: Arc<Publisher<FakeApi>>,
    }

    fn fixture() -> Fixture {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, clock()).unwrap();
        db.set_setting("poll_channel", POLL_CHANNEL).unwrap();
        db.set_setting("staff_channel", STAFF_CHANNEL).unwrap();
        let api = FakeApi::default();
        let publisher = Arc::new(Publisher::with_clock(api.clone(), db.clone(), clock));
        Fixture { db, api, publisher }
    }

    fn poll(f: &Fixture, id: i64) -> PollRow {
        find_poll(&f.db, id).unwrap().unwrap()
    }

    fn sends(api: &FakeApi) -> usize {
        api.calls().iter().filter(|call| matches!(call, Call::Send { .. })).count()
    }

    // --- sondaggi --------------------------------------------------------------------------

    #[tokio::test]
    async fn first_refresh_creates_the_message_with_a_thread_and_a_deadline() {
        let f = fixture();
        f.publisher.refresh_poll(1).await.unwrap();
        let poll = poll(&f, 1);
        let message_id: u64 = poll.message_id.as_deref().unwrap().parse().unwrap();
        let stored = f.api.message(POLL_CHANNEL, message_id).expect("messaggio pubblicato");
        assert_eq!(stored.footer.as_deref(), Some("0 votanti · POLL_ID=1"));
        assert_eq!(stored.embeds[0]["title"], "🎮  Giochi gratis da scaricare");
        assert!(!stored.components.is_empty(), "pulsanti di voto");
        assert!(poll.closes_at.is_some(), "scadenza assegnata al prossimo reset");
        assert!(poll.thread_id.is_some());
        assert!(f.api.calls().contains(&Call::Thread { channel: POLL_CHANNEL, message: message_id, name: "Giochi gratis da scaricare · Discussione".into() }));
        let claims: i64 = f.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM poll_message_claims", [], |r| r.get(0)).unwrap());
        assert_eq!(claims, 0, "claim rilasciato");
    }

    #[tokio::test]
    async fn second_refresh_edits_in_place_without_a_new_thread() {
        let f = fixture();
        f.publisher.refresh_poll(1).await.unwrap();
        f.publisher.refresh_poll(1).await.unwrap();
        let calls = f.api.calls();
        assert_eq!(sends(&f.api), 1);
        assert_eq!(calls.iter().filter(|c| matches!(c, Call::Edit { .. })).count(), 1);
        assert_eq!(calls.iter().filter(|c| matches!(c, Call::Thread { .. })).count(), 1);
    }

    #[tokio::test]
    async fn deleted_message_is_recreated_and_stale_adoptions_are_avoided() {
        let f = fixture();
        f.publisher.refresh_poll(1).await.unwrap();
        let first: u64 = poll(&f, 1).message_id.unwrap().parse().unwrap();
        f.api.delete_message(POLL_CHANNEL, first).await.unwrap(); // qualcuno l'ha cancellato
        f.publisher.refresh_poll(1).await.unwrap();
        let second: u64 = poll(&f, 1).message_id.unwrap().parse().unwrap();
        assert_ne!(second, first, "il messaggio sparito viene ricreato");
        assert_eq!(sends(&f.api), 2);
        assert!(f.api.message(POLL_CHANNEL, second).is_some());
    }

    #[tokio::test]
    async fn history_matching_is_exact_and_limited_to_the_bots_own_messages() {
        let f = fixture();
        f.db.execute("INSERT INTO polls(id,name,emoji,color,position,active) VALUES(12,'Dodici','🔢','#111111',9,1)", []).unwrap();
        // Messaggi preesistenti: del sondaggio 12 (footer che contiene "POLL_ID=1"), di un altro utente, e del bot.
        let other_poll = f.api.seed_message(POLL_CHANNEL, f.api.bot_id, Some("3 votanti · POLL_ID=12"));
        let someone_else = f.api.seed_message(POLL_CHANNEL, 999, Some("0 votanti · POLL_ID=1"));
        let right = f.api.seed_message(POLL_CHANNEL, f.api.bot_id, Some("2 votanti · POLL_ID=1"));
        f.publisher.refresh_poll(1).await.unwrap();
        let adopted: u64 = poll(&f, 1).message_id.unwrap().parse().unwrap();
        assert_eq!(adopted, right);
        assert_ne!(adopted, other_poll, "POLL_ID=1 non deve prendere il messaggio del 12");
        assert_ne!(adopted, someone_else);
        assert_eq!(sends(&f.api), 0, "adottato, non ricreato");
        // Il messaggio adottato viene aggiornato con il nuovo embed.
        assert_eq!(f.api.message(POLL_CHANNEL, right).unwrap().footer.as_deref(), Some("0 votanti · POLL_ID=1"));
    }

    #[tokio::test]
    async fn nothing_happens_for_inactive_polls_or_without_a_channel() {
        let f = fixture();
        f.db.execute("UPDATE polls SET active=0 WHERE id=2", []).unwrap();
        f.publisher.refresh_poll(2).await.unwrap();
        f.publisher.refresh_poll(999).await.unwrap();
        f.db.execute("DELETE FROM settings WHERE key='poll_channel'", []).unwrap();
        f.publisher.refresh_poll(1).await.unwrap();
        f.publisher.refresh_dashboard().await.unwrap();
        assert!(f.api.calls().is_empty());
    }

    #[tokio::test]
    async fn a_live_claim_prevents_duplicate_messages_and_failures_release_it() {
        let f = fixture();
        let now_ts = clock().timestamp() as f64;
        assert!(try_claim_poll_message(&f.db, 1, now_ts).unwrap(), "qualcun altro ha il claim");
        f.publisher.refresh_poll(1).await.unwrap();
        assert!(f.api.calls().is_empty(), "claim vivo: niente invio");
        release_poll_message_claim(&f.db, 1).unwrap();

        f.api.fail_send(Some(ApiError::Forbidden));
        let result = f.publisher.refresh_poll(1).await;
        assert!(matches!(result, Err(PublishError::Api(ApiError::Forbidden))));
        let claims: i64 = f.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM poll_message_claims", [], |r| r.get(0)).unwrap());
        assert_eq!(claims, 0, "il claim si rilascia anche dopo un errore");
    }

    #[tokio::test]
    async fn an_inaccessible_channel_is_a_warning_not_an_error() {
        let f = fixture();
        f.api.fail_history(Some(ApiError::Forbidden));
        f.publisher.refresh_poll(1).await.unwrap();
        assert_eq!(sends(&f.api), 0);
        assert!(poll(&f, 1).message_id.is_none());
        // Un errore di rete invece sì.
        f.api.fail_history(Some(ApiError::Http("500".into())));
        assert!(f.publisher.refresh_poll(1).await.is_err());
    }

    #[tokio::test]
    async fn edit_errors_other_than_not_found_are_reported() {
        let f = fixture();
        f.publisher.refresh_poll(1).await.unwrap();
        f.api.fail_edit(Some(ApiError::Forbidden));
        assert!(matches!(f.publisher.refresh_poll(1).await, Err(PublishError::Api(ApiError::Forbidden))));
    }

    #[tokio::test]
    async fn thread_failure_keeps_the_message_and_retries_later() {
        let f = fixture();
        f.api.fail_thread(Some(ApiError::Forbidden));
        f.publisher.refresh_poll(1).await.unwrap();
        let poll_row = poll(&f, 1);
        assert!(poll_row.message_id.is_some());
        assert!(poll_row.thread_id.is_none());
        f.api.fail_thread(None);
        f.publisher.refresh_poll(1).await.unwrap(); // la modifica riprova a creare il thread
        assert!(poll(&f, 1).thread_id.is_some());
    }

    #[tokio::test]
    async fn long_poll_names_give_a_thread_name_within_100_characters() {
        let f = fixture();
        f.db.execute("UPDATE polls SET name=? WHERE id=1", ["x".repeat(150)]).unwrap();
        f.publisher.refresh_poll(1).await.unwrap();
        let name = f.api.calls().into_iter().find_map(|c| match c {
            Call::Thread { name, .. } => Some(name),
            _ => None,
        });
        assert_eq!(name.unwrap().chars().count(), 100);
    }

    // --- dashboard -------------------------------------------------------------------------

    #[tokio::test]
    async fn dashboard_is_sent_once_as_components_v2_then_edited_and_ids_are_remembered() {
        let f = fixture();
        assert_eq!(f.publisher.dashboard_style(), DashboardStyle::Components2, "V2 è il predefinito, come in Python");
        f.publisher.refresh_dashboard().await.unwrap();
        let message: u64 = f.db.get_setting("dashboard_message").unwrap().unwrap().parse().unwrap();
        assert_eq!(f.db.get_setting("dashboard_channel").unwrap().as_deref(), Some("500"));
        let stored = f.api.message(POLL_CHANNEL, message).unwrap();
        assert!(stored.v2, "inviato con il flag dei Components V2");
        assert!(stored.embeds.is_empty() && stored.content.is_none(), "Discord vieta contenuto ed embed nei V2");
        assert_eq!(stored.components.len(), 3, "riquadro, pulsanti e menu «Altro»");
        assert_eq!(stored.components[0]["type"], 17);
        assert!(stored.components[0]["components"][0]["content"].as_str().unwrap().starts_with("## 🎮 Game Night Hub"));

        f.publisher.refresh_dashboard().await.unwrap();
        assert_eq!(sends(&f.api), 1);
        assert_eq!(f.api.calls().iter().filter(|c| matches!(c, Call::Edit { .. })).count(), 1);
        assert!(f.api.message(POLL_CHANNEL, message).unwrap().v2, "la modifica resta V2");
    }

    #[tokio::test]
    async fn dashboard_can_fall_back_to_the_classic_embed_style() {
        let f = fixture();
        f.db.set_setting("dashboard_style", "Classic").unwrap();
        assert_eq!(f.publisher.dashboard_style(), DashboardStyle::Classic);
        f.publisher.refresh_dashboard().await.unwrap();
        let message: u64 = f.db.get_setting("dashboard_message").unwrap().unwrap().parse().unwrap();
        let stored = f.api.message(POLL_CHANNEL, message).unwrap();
        assert!(!stored.v2);
        assert_eq!(stored.embeds[0]["title"], "🎮 Game Night Hub");
        assert_eq!(stored.components.len(), 2, "pulsanti e menu «Altro»");

        // Passare a V2 modifica lo stesso messaggio (togliendo gli embed); se Discord rifiutasse la
        // conversione, scatta la sostituzione già provata.
        f.db.set_setting("dashboard_style", "components2").unwrap();
        f.publisher.refresh_dashboard().await.unwrap();
        let converted = f.api.message(POLL_CHANNEL, message).unwrap();
        assert!(converted.v2 && converted.embeds.is_empty());
        assert_eq!(sends(&f.api), 1);
    }

    #[tokio::test]
    async fn an_unconvertible_classic_dashboard_is_replaced_by_the_v2_one() {
        let f = fixture();
        f.db.set_setting("dashboard_style", "classic").unwrap();
        f.publisher.refresh_dashboard().await.unwrap();
        let old: u64 = f.db.get_setting("dashboard_message").unwrap().unwrap().parse().unwrap();
        f.db.set_setting("dashboard_style", "components2").unwrap();
        f.api.fail_edit(Some(ApiError::Http("400: non convertibile ai componenti v2".into())));
        f.publisher.refresh_dashboard().await.unwrap();
        let new: u64 = f.db.get_setting("dashboard_message").unwrap().unwrap().parse().unwrap();
        assert_ne!(old, new);
        assert!(f.api.calls().contains(&Call::Delete { channel: POLL_CHANNEL, message: old }));
        assert!(f.api.message(POLL_CHANNEL, new).unwrap().v2);
        assert!(f.api.message(POLL_CHANNEL, old).is_none());
    }

    #[tokio::test]
    async fn dashboard_is_replaced_when_missing_uneditable_or_in_another_channel() {
        let f = fixture();
        f.publisher.refresh_dashboard().await.unwrap();
        let first = f.db.get_setting("dashboard_message").unwrap().unwrap();

        f.api.fail_edit(Some(ApiError::NotFound));
        f.publisher.refresh_dashboard().await.unwrap();
        let second = f.db.get_setting("dashboard_message").unwrap().unwrap();
        assert_ne!(first, second, "messaggio sparito: nuovo invio");
        assert!(!f.api.calls().iter().any(|c| matches!(c, Call::Delete { .. })), "già sparito: niente eliminazione");

        f.api.fail_edit(Some(ApiError::Http("non convertibile".into())));
        f.publisher.refresh_dashboard().await.unwrap();
        let third = f.db.get_setting("dashboard_message").unwrap().unwrap();
        assert_ne!(second, third);
        assert!(f.api.calls().contains(&Call::Delete { channel: POLL_CHANNEL, message: second.parse().unwrap() }), "la vecchia viene eliminata");

        // Cambio di canale: il vecchio messaggio non si tocca, se ne pubblica uno nuovo.
        f.api.fail_edit(None);
        let edits_before = f.api.calls().iter().filter(|c| matches!(c, Call::Edit { .. })).count();
        f.db.set_setting("poll_channel", 501).unwrap();
        f.publisher.refresh_dashboard().await.unwrap();
        assert_eq!(f.api.calls().iter().filter(|c| matches!(c, Call::Edit { .. })).count(), edits_before);
        assert_eq!(f.db.get_setting("dashboard_channel").unwrap().as_deref(), Some("501"));
    }

    #[tokio::test]
    async fn dashboard_send_failure_is_logged_and_nothing_is_remembered() {
        let f = fixture();
        f.api.fail_send(Some(ApiError::Forbidden));
        f.publisher.refresh_dashboard().await.unwrap();
        assert_eq!(f.db.get_setting("dashboard_message").unwrap(), None);
        assert_eq!(f.db.get_setting("dashboard_channel").unwrap(), None);
    }

    #[tokio::test]
    async fn refresh_all_updates_every_active_poll_then_the_dashboard() {
        let f = fixture();
        f.db.execute("UPDATE polls SET active=0 WHERE id=3", []).unwrap();
        f.publisher.refresh_all().await.unwrap();
        assert_eq!(sends(&f.api), 4 + 1, "4 sondaggi attivi + dashboard");
        let last = f.api.calls().into_iter().rev().find(|c| matches!(c, Call::Send { .. })).unwrap();
        let Call::Send { message, .. } = last else { unreachable!() };
        assert_eq!(f.db.get_setting("dashboard_message").unwrap(), Some(message.to_string()), "la dashboard è per ultima");
        // Un errore su un sondaggio non ferma gli altri.
        let f2 = fixture();
        f2.api.fail_history(Some(ApiError::Http("boom".into())));
        f2.publisher.refresh_all().await.unwrap();
        assert_eq!(sends(&f2.api), 1, "solo la dashboard (i sondaggi hanno fallito)");
    }

    // --- schede di revisione ---------------------------------------------------------------

    fn proposal(f: &Fixture, user: i64, name: &str) -> i64 {
        crate::suggestions::register_suggestion(&f.db, user, name, "Gioco", None, clock()).created_id.unwrap()
    }

    #[tokio::test]
    async fn review_card_is_posted_remembered_and_kept_in_sync() {
        let f = fixture();
        let id = proposal(&f, 1, "Among Us");
        f.publisher.post_review_card(id).await.unwrap();
        let (channel, message) = review_message_location(&f.db, id).unwrap().unwrap();
        assert_eq!(channel, STAFF_CHANNEL.to_string());
        let stored = f.api.message(STAFF_CHANNEL, message.parse().unwrap()).unwrap();
        assert_eq!(stored.embeds[0]["title"], "🎮 Among Us");
        assert_eq!(stored.components[0]["components"][0]["disabled"], false);

        // Dopo l'approvazione la scheda si aggiorna e i pulsanti si spengono.
        f.db.execute("UPDATE suggestions SET review_status='approved' WHERE id=?", [id]).unwrap();
        f.publisher.update_review_message(id, None, None).await;
        let updated = f.api.message(STAFF_CHANNEL, message.parse().unwrap()).unwrap();
        assert!(updated.embeds[0]["description"].as_str().unwrap().contains("✅ Approvata"));
        assert_eq!(updated.components[0]["components"][0]["disabled"], true);

        // Proposta unita/rimossa: la scheda diventa un avviso senza embed né pulsanti. Eliminare la
        // proposta cancella a cascata la posizione salvata, quindi chi unisce la legge prima.
        let location = review_message_location(&f.db, id).unwrap();
        f.db.execute("DELETE FROM proposals WHERE suggestion_id=?", [id]).unwrap();
        f.db.execute("DELETE FROM suggestions WHERE id=?", [id]).unwrap();
        assert_eq!(review_message_location(&f.db, id).unwrap(), None, "cascata");
        f.publisher.update_review_message(id, None, location.clone()).await;
        let closed = f.api.message(STAFF_CHANNEL, message.parse().unwrap()).unwrap();
        assert_eq!(closed.content.as_deref(), Some("Proposta non più disponibile."));
        assert!(closed.embeds.is_empty() && closed.components.is_empty());
        f.publisher.update_review_message(id, Some("Unita a #3"), location).await;
        assert_eq!(f.api.message(STAFF_CHANNEL, message.parse().unwrap()).unwrap().content.as_deref(), Some("Unita a #3"));
    }

    #[tokio::test]
    async fn review_card_needs_a_staff_channel_a_proposal_and_a_known_message() {
        let f = fixture();
        let id = proposal(&f, 1, "Among Us");
        f.publisher.post_review_card(999).await.unwrap();
        f.db.execute("DELETE FROM settings WHERE key='staff_channel'", []).unwrap();
        f.publisher.post_review_card(id).await.unwrap();
        f.publisher.update_review_message(id, None, None).await; // nessuna posizione salvata
        assert!(f.api.calls().is_empty());
        f.db.set_setting("staff_channel", STAFF_CHANNEL).unwrap();
        f.api.fail_send(Some(ApiError::Forbidden));
        f.publisher.post_review_card(id).await.unwrap();
        assert_eq!(review_message_location(&f.db, id).unwrap(), None, "invio fallito: niente da ricordare");
        // Modificare una scheda sparita non è un errore.
        f.publisher.update_review_message(id, None, Some(("600".into(), "12345".into()))).await;
    }

    // --- pianificazione --------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn scheduled_refreshes_are_debounced_per_target() {
        let f = fixture();
        let scheduler = RefreshScheduler::new();
        for _ in 0..3 {
            assert!(f.publisher.schedule_dashboard_refresh(&scheduler));
        }
        f.publisher.schedule_poll_refresh(&scheduler, 1);
        f.publisher.schedule_poll_refresh(&scheduler, 1);
        assert_eq!(scheduler.pending_count(), 2);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(sends(&f.api), 2, "una dashboard e un messaggio di sondaggio");
        assert_eq!(scheduler.pending_count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn publisher_effects_plug_into_apply_review() {
        use crate::notify::Notifier;
        use crate::notify::testing::FakeSender;
        use crate::review::{ReviewRequest, ReviewSource, apply_review};
        let f = fixture();
        let id = proposal(&f, 1, "Among Us");
        f.publisher.post_review_card(id).await.unwrap();
        let effects = PublisherEffects { publisher: Arc::clone(&f.publisher), scheduler: RefreshScheduler::new() };
        let notifier = Notifier::with_audit_channel(f.db.clone(), FakeSender::default(), None);
        let outcome = apply_review(
            &f.db,
            &notifier,
            &effects,
            ReviewRequest { actor_id: 9, suggestion_id: id, action: "approve", reason: None, source: ReviewSource::Discord },
        )
        .await;
        assert!(outcome.ok);
        let (_, message) = review_message_location(&f.db, id).unwrap().unwrap();
        let stored = f.api.message(STAFF_CHANNEL, message.parse().unwrap()).unwrap();
        assert!(stored.embeds[0]["description"].as_str().unwrap().contains("✅ Approvata"), "la scheda è stata aggiornata");
        tokio::time::sleep(Duration::from_secs(3)).await; // dashboard differita
        assert!(f.db.get_setting("dashboard_message").unwrap().is_some());
    }

    // --- chiusura per scadenza ed eliminazione --------------------------------------------------

    #[tokio::test]
    async fn expired_polls_are_closed_and_their_message_is_updated() {
        let f = fixture();
        f.publisher.refresh_poll(1).await.unwrap();
        f.publisher.refresh_poll(2).await.unwrap();
        let message: u64 = poll(&f, 1).message_id.as_deref().unwrap().parse().unwrap();
        // Il sondaggio 1 è scaduto; il 2 no; il 3 è scaduto ma non ha mai avuto un messaggio.
        f.db.execute("UPDATE polls SET closes_at=1 WHERE id IN (1,3)", []).unwrap();
        let before = f.api.calls().len();
        assert_eq!(f.publisher.close_expired_polls().await.unwrap(), 2);
        assert!(!poll(&f, 1).is_active() && !poll(&f, 3).is_active());
        assert!(poll(&f, 2).is_active());
        let edits: Vec<Call> = f.api.calls()[before..].iter().filter(|c| matches!(c, Call::Edit { .. })).cloned().collect();
        assert!(edits.contains(&Call::Edit { channel: POLL_CHANNEL, message }), "messaggio del sondaggio chiuso aggiornato: {edits:?}");
        assert_eq!(sends(&f.api), 3, "nessun messaggio nuovo per il sondaggio chiuso senza messaggio");
        // Il messaggio chiuso non ha più i pulsanti di voto attivi.
        let stored = f.api.message(POLL_CHANNEL, message).unwrap();
        assert!(stored.embeds[0]["title"].is_string());
        // Niente da chiudere: nessuna chiamata a Discord.
        let calls = f.api.calls().len();
        assert_eq!(f.publisher.close_expired_polls().await.unwrap(), 0);
        assert_eq!(f.api.calls().len(), calls);
    }

    #[tokio::test]
    async fn deleting_a_poll_removes_the_message_releases_proposals_and_claims() {
        let f = fixture();
        f.publisher.refresh_poll(1).await.unwrap();
        let message: u64 = poll(&f, 1).message_id.as_deref().unwrap().parse().unwrap();
        f.db.execute("INSERT OR REPLACE INTO poll_message_claims(poll_id,expires_at) VALUES (1, 99999999999)", []).unwrap();
        assert_eq!(f.publisher.delete_poll(1).await.unwrap(), Some(true));
        assert!(find_poll(&f.db, 1).unwrap().is_none());
        assert!(f.api.message(POLL_CHANNEL, message).is_none(), "messaggio eliminato");
        let claims: i64 = f.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM poll_message_claims WHERE poll_id=1", [], |r| r.get(0)).unwrap());
        assert_eq!(claims, 0);
        let options: i64 = f.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM options WHERE poll_id=1", [], |r| r.get(0)).unwrap());
        assert_eq!(options, 0, "le opzioni spariscono con il sondaggio");
        // Un secondo tentativo: non c'è più.
        assert_eq!(f.publisher.delete_poll(1).await.unwrap(), None);
    }

    #[tokio::test]
    async fn deleting_a_poll_tolerates_a_missing_message_and_reports_an_undeletable_one() {
        let f = fixture();
        f.publisher.refresh_poll(1).await.unwrap();
        f.publisher.refresh_poll(2).await.unwrap();
        f.api.fail_delete(Some(ApiError::NotFound));
        assert_eq!(f.publisher.delete_poll(1).await.unwrap(), Some(true), "messaggio già sparito: va bene");
        f.api.fail_delete(Some(ApiError::Http("boom".into())));
        assert_eq!(f.publisher.delete_poll(2).await.unwrap(), Some(false), "messaggio non eliminabile");
        assert!(find_poll(&f.db, 2).unwrap().is_none(), "il sondaggio sparisce comunque dal database");
    }
}
