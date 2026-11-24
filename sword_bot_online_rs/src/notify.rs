//! Log staff, audit, feed attività e notifiche private (equivalente di `sfo/notify.py`).
//!
//! In Python questi sono funzioni globali che leggono il bot da `runtime.bot`. Qui vivono in
//! [`Notifier`], che possiede il database e un "mittente" Discord ([`DiscordSender`]): in produzione
//! è [`HttpSender`] (l'API HTTP di serenity), nei test un finto che registra i messaggi.
//!
//! Tutti i messaggi partono con `allowed_mentions` vuoto (come `AllowedMentions.none()` del bot
//! Python): una menzione dentro a un nome di proposta non notifica nessuno.
// Verrà usato da comandi, reset, sondaggi e revisione.
#![allow(dead_code)]

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use poise::serenity_prelude::{self as serenity, CreateAllowedMentions, CreateEmbed, CreateMessage};
use serde_json::Value;
use tracing::{debug, error, warn};

use crate::database::{Database, DbResult};
use crate::embeds::{self, EmbedKind};
use crate::formatting::{proposal_image_url, python_json};
use crate::schedule;

/// Attesa prima di inviare i log staff accumulati: più righe ravvicinate partono in un solo messaggio.
pub const STAFF_LOG_DEBOUNCE: Duration = Duration::from_millis(500);
/// Lunghezza massima di un messaggio di log (Discord ne accetta 2000).
pub const STAFF_LOG_CHUNK_LIMIT: usize = 1900;

// --- invio su Discord --------------------------------------------------------------------------

/// Errore di invio, già ridotto a testo: ai chiamanti basta per il log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendError(pub String);

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Messaggio in uscita verso un canale.
#[derive(Debug, Clone, Default)]
pub struct OutgoingMessage {
    pub content: String,
    pub embed: Option<CreateEmbed>,
}

/// Chi sa davvero spedire messaggi: l'API di Discord, o un finto nei test.
pub trait DiscordSender: Send + Sync + 'static {
    fn send_to_channel(
        &self,
        channel_id: u64,
        message: OutgoingMessage,
    ) -> impl Future<Output = Result<(), SendError>> + Send;

    fn send_direct_message(
        &self,
        user_id: u64,
        content: String,
    ) -> impl Future<Output = Result<(), SendError>> + Send;
}

/// Il mittente di produzione, sopra `serenity::Http`.
///
/// Differenza da Python: non consulta la cache (`get_channel`/`get_user`) prima di chiamare l'API,
/// quindi un canale non più accessibile si manifesta come errore di invio (loggato), non come
/// "silenziosamente ignorato".
#[derive(Clone)]
pub struct HttpSender {
    http: Arc<serenity::Http>,
}

impl HttpSender {
    pub fn new(http: Arc<serenity::Http>) -> Self {
        Self { http }
    }
}

impl DiscordSender for HttpSender {
    async fn send_to_channel(&self, channel_id: u64, message: OutgoingMessage) -> Result<(), SendError> {
        let mut builder = CreateMessage::new()
            .content(message.content)
            .allowed_mentions(CreateAllowedMentions::new());
        if let Some(embed) = message.embed {
            builder = builder.embed(embed);
        }
        serenity::ChannelId::new(channel_id)
            .send_message(&self.http, builder)
            .await
            .map(|_| ())
            .map_err(|error| SendError(error.to_string()))
    }

    async fn send_direct_message(&self, user_id: u64, content: String) -> Result<(), SendError> {
        let builder = CreateMessage::new().content(content).allowed_mentions(CreateAllowedMentions::new());
        serenity::UserId::new(user_id)
            .direct_message(&self.http, builder)
            .await
            .map(|_| ())
            .map_err(|error| SendError(error.to_string()))
    }
}

// --- utilità di testo --------------------------------------------------------------------------

/// Primi `limit` caratteri (come lo slicing `text[:limit]` di Python, che conta i code point).
fn truncate_chars(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((byte_index, _)) => text[..byte_index].to_string(),
        None => text.to_string(),
    }
}

/// Unisce le righe in messaggi di al massimo `limit` caratteri, andando a capo tra una riga e l'altra.
///
/// Una singola riga più lunga del limite viene spezzata: in Python produceva un messaggio vuoto
/// (rifiutato da Discord) seguito da uno troppo lungo (rifiutato anch'esso).
pub fn chunk_staff_log(lines: &[String], limit: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut chunk = String::new();
    let mut chunk_len = 0usize;
    for line in lines {
        let mut remaining: &str = line;
        // Le righe troppo lunghe si spezzano in pezzi da `limit` caratteri.
        while remaining.chars().count() > limit {
            if chunk_len > 0 {
                chunks.push(std::mem::take(&mut chunk));
                chunk_len = 0;
            }
            let split = remaining.char_indices().nth(limit).map_or(remaining.len(), |(index, _)| index);
            chunks.push(remaining[..split].to_string());
            remaining = &remaining[split..];
        }
        if remaining.is_empty() && line.chars().count() > limit {
            continue; // la riga era un multiplo esatto del limite: nulla da aggiungere
        }
        let line_len = remaining.chars().count();
        let addition_len = if chunk_len == 0 { line_len } else { line_len + 1 };
        if chunk_len + addition_len > limit {
            chunks.push(std::mem::take(&mut chunk));
            chunk.push_str(remaining);
            chunk_len = line_len;
        } else {
            if chunk_len > 0 {
                chunk.push('\n');
            }
            chunk.push_str(remaining);
            chunk_len += addition_len;
        }
    }
    if chunk_len > 0 {
        chunks.push(chunk);
    }
    chunks
}

// --- audit -------------------------------------------------------------------------------------

/// Un'azione staff da registrare. I campi opzionali vuoti valgono "assente", come in Python.
#[derive(Debug, Clone, Default)]
pub struct AuditEntry {
    pub actor_id: Option<u64>,
    pub action: String,
    pub entity_type: Option<String>,
    pub entity_id: Option<String>,
    pub reason: Option<String>,
    pub details: Option<Value>,
}

impl AuditEntry {
    pub fn new(actor_id: Option<u64>, action: impl Into<String>) -> Self {
        Self { actor_id, action: action.into(), ..Self::default() }
    }

    /// Entità toccata dall'azione, es. `("suggestion", 42)`.
    pub fn entity(mut self, entity_type: impl Into<String>, entity_id: impl ToString) -> Self {
        self.entity_type = Some(entity_type.into());
        self.entity_id = Some(entity_id.to_string());
        self
    }

    /// Solo il tipo dell'entità, senza ID (es. `"database"`).
    pub fn entity_type(mut self, entity_type: impl Into<String>) -> Self {
        self.entity_type = Some(entity_type.into());
        self
    }

    pub fn reason(mut self, reason: Option<&str>) -> Self {
        self.reason = reason.map(str::to_string);
        self
    }

    pub fn details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    fn details_json(&self) -> String {
        python_json(&self.details.clone().unwrap_or_else(|| Value::Object(Default::default())))
    }
}

/// Testo del messaggio di audit nel canale configurato.
pub fn audit_message(occurred_at_unix: i64, entry: &AuditEntry) -> String {
    let actor = match entry.actor_id {
        Some(id) if id != 0 => format!("<@{id}>"),
        _ => "Sistema".to_string(),
    };
    let entity_type = entry.entity_type.as_deref().filter(|text| !text.is_empty()).unwrap_or("—");
    let entity_id = entry.entity_id.as_deref().unwrap_or("—");
    let reason = entry.reason.as_deref().filter(|text| !text.is_empty()).unwrap_or("—");
    let content = format!(
        "**Audit · {}** · <t:{occurred_at_unix}:F>\nAttore: {actor}\nEntità: {entity_type} · `{entity_id}`\nMotivo: {}\nDettagli: `{}`",
        truncate_chars(&entry.action, 100),
        truncate_chars(reason, 300),
        truncate_chars(&entry.details_json(), 900),
    );
    truncate_chars(&content, 1900)
}

// --- notificatore ------------------------------------------------------------------------------

#[derive(Default)]
/// Log staff state used for debouncing messages in the staff log.
///
/// La struttura traccia le righe accumulate e se un flush task è già in corso.
/// Le righe vicine sono destinate a partire in un solo messaggio (dopo
/// [`STAFF_LOG_DEBOUNCE`](crate::notify::STAFF_LOG_DEBOUNCE)).
struct StaffLogState {
    pending: Vec<String>,
    /// C'è già un task che svuota la coda.
    running: bool,
}

/// Inner container for the [`Notifier`].
///
/// Possiede il database, il mittente Discord clonabile, lo stato del log staff
/// e l'ID canale di audit configurabile.
struct Inner<S> {
    db: Database,
    sender: S,
    staff_log: Mutex<StaffLogState>,
    /// `BOT_AUDIT_CHANNEL_ID`: canale privato per l'audit (default: canale staff).
    audit_channel_env: Option<String>,
}

/// Log staff, audit, feed attività e DM. Clonabile a basso costo.
///
/// È parametrato su un [`DiscordSender`]: in produzione `HttpSender` sopra
/// `serenity::Http`, nei test un `FakeSender` che registra ciò che "invia".
pub struct Notifier<S: DiscordSender> {
    inner: Arc<Inner<S>>,
}

impl<S: DiscordSender> Clone for Notifier<S> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl<S: DiscordSender> Notifier<S> {
    /// Legge `BOT_AUDIT_CHANNEL_ID` dall'ambiente.
    pub fn new(db: Database, sender: S) -> Self {
        Self::with_audit_channel(db, sender, std::env::var("BOT_AUDIT_CHANNEL_ID").ok())
    }

    /// Inizializza un nuovo `Notifier` con un canale audit opzionale.
///
/// Se `audit_channel_env` è `None`, il canale viene determinato al runtime leggendo
/// `BOT_AUDIT_CHANNEL_ID` oppure cadendo sul canale staff configurato.
pub fn with_audit_channel(db: Database, sender: S, audit_channel_env: Option<String>) -> Self {
        Self {
            inner: Arc::new(Inner { db, sender, staff_log: Mutex::new(StaffLogState::default()), audit_channel_env }),
        }
    }

    /// Restituisce il guardiano mutuo per lo stato del log staff.
    ///
    /// Se il thread è stato bloccato (poisoning), recupera l'ultimo stato conosciuto.
    fn staff_log_state(&self) -> MutexGuard<'_, StaffLogState> {
        self.inner.staff_log.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Restituisce l'ID canale salvato nelle impostazioni per la chiave data.
    ///
    /// Restituisce `None` se l'impostazione manca, è vuota o non è un ID valido.
    /// Logga un errore sul fallimento di lettura dal database.
    fn channel_setting(&self, key: &str) -> Option<u64> {
        match self.inner.db.get_setting(key) {
            Ok(Some(raw)) if !raw.trim().is_empty() => parse_channel_id(&raw, key),
            Ok(_) => None,
            Err(e) => {
                error!("Impossibile leggere l'impostazione '{key}': {e}");
                None
            }
        }
    }

    /// Avvia `task` in background; senza un runtime tokio attivo lo scarta (in Python:
    /// "event loop non attivo").
    /// Avvia `task` in background; senza un runtime tokio attivo lo scarta (in Python:
/// "event loop non attivo").
fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) -> bool {
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(task);
                true
            }
            Err(_) => {
                debug!("Task in background non avviato: runtime tokio non attivo.");
                false
            }
        }
    }

    // --- log staff ---

    /// Accoda una riga nel canale staff. Righe ravvicinate partono insieme, in un solo messaggio
    /// (dopo [`STAFF_LOG_DEBOUNCE`]). Senza `staff_channel` configurato non fa nulla.
    pub fn staff_log(&self, text: impl Into<String>) {
        let Some(channel_id) = self.channel_setting("staff_channel") else {
            return;
        };
        let start_flusher = {
            let mut state = self.staff_log_state();
            state.pending.push(text.into());
            !std::mem::replace(&mut state.running, true)
        };
        if start_flusher {
            let this = self.clone();
            if !self.spawn(async move { this.flush_staff_log(channel_id).await }) {
                self.staff_log_state().running = false;
            }
        }
    }

    async fn flush_staff_log(&self, channel_id: u64) {
        loop {
            tokio::time::sleep(STAFF_LOG_DEBOUNCE).await;
            let lines = {
                let mut state = self.staff_log_state();
                if state.pending.is_empty() {
                    // Svuotamento e `running = false` sotto lo stesso lock: nessuna riga si perde.
                    state.running = false;
                    return;
                }
                std::mem::take(&mut state.pending)
            };
            for chunk in chunk_staff_log(&lines, STAFF_LOG_CHUNK_LIMIT) {
                let message = OutgoingMessage { content: chunk, embed: None };
                if let Err(e) = self.inner.sender.send_to_channel(channel_id, message).await {
                    error!("Impossibile inviare il log nel canale staff {channel_id}: {e}");
                }
            }
        }
    }

    // --- audit ---

    /// Registra un'azione staff nel database e la pubblica (in background) nel canale di audit.
    /// Un errore del database viene loggato e l'azione non viene pubblicata.
    pub fn record_staff_audit(&self, entry: AuditEntry) {
        let occurred_at = schedule::now();
        let stored = self.inner.db.transaction(|tx| -> DbResult<()> {
            tx.execute(
                "INSERT INTO staff_audit
                 (occurred_at, actor_id, action, entity_type, entity_id, reason, details)
                 VALUES(?,?,?,?,?,?,?)",
                rusqlite::params![
                    schedule::isoformat(occurred_at),
                    entry.actor_id.and_then(|id| i64::try_from(id).ok()),
                    truncate_chars(&entry.action, 100),
                    entry.entity_type.as_deref().filter(|t| !t.is_empty()).map(|t| truncate_chars(t, 100)),
                    entry.entity_id.as_deref().map(|id| truncate_chars(id, 200)),
                    entry.reason.as_deref().filter(|r| !r.is_empty()).map(|r| truncate_chars(r, 500)),
                    truncate_chars(&entry.details_json(), 4000),
                ],
            )?;
            Ok(())
        });
        if let Err(e) = stored {
            error!("Impossibile registrare l'azione staff {}: {e}", entry.action);
            return;
        }
        let this = self.clone();
        self.spawn(async move { this.publish_staff_audit(occurred_at.timestamp(), &entry).await });
    }

    /// Restituisce l'ID canale di audit da usare.
///
/// Controlla prima `BOT_AUDIT_CHANNEL_ID` (se impostato), altrimenti cade sul canale staff
/// configurato. Restituisce `None` se né l'ambiente né l'impostazione forniscono un ID valido.
fn audit_channel(&self) -> Option<u64> {
        match self.inner.audit_channel_env.as_deref().map(str::trim).filter(|raw| !raw.is_empty()) {
            Some(raw) => parse_channel_id(raw, "BOT_AUDIT_CHANNEL_ID"),
            None => self.channel_setting("staff_channel"),
        }
    }

    /// Pubblica un'azione audit nel canale di audit configurato.
///
/// Se non è stato configurato alcun canale, la funzione ritorna senza fare nulla.
async fn publish_staff_audit(&self, occurred_at_unix: i64, entry: &AuditEntry) {
        let Some(channel_id) = self.audit_channel() else {
            return;
        };
        let message = OutgoingMessage { content: audit_message(occurred_at_unix, entry), embed: None };
        if let Err(e) = self.inner.sender.send_to_channel(channel_id, message).await {
            error!("Impossibile inviare l'azione audit nel canale configurato: {e}");
        }
    }

    // --- feed attività ---

    /// Pubblica nel feed attività, se è attivo (`activity_feed_enabled=1`) e c'è un canale.
    pub async fn publish_activity(&self, text: impl Into<String>, embed: Option<CreateEmbed>) {
        match self.inner.db.get_setting_or("activity_feed_enabled", "0") {
            Ok(flag) if flag == "1" => {}
            Ok(_) => return,
            Err(e) => {
                error!("Impossibile leggere activity_feed_enabled: {e}");
                return;
            }
        }
        let Some(channel_id) = self.channel_setting("activity_channel") else {
            return;
        };
        let message = OutgoingMessage { content: text.into(), embed };
        if let Err(e) = self.inner.sender.send_to_channel(channel_id, message).await {
            error!("Impossibile pubblicare l'attività nel canale {channel_id}: {e}");
        }
    }

    /// Annuncia una proposta nel feed attività solo dopo l'approvazione staff.
    pub async fn announce_approved_proposal(&self, suggestion_id: i64) -> DbResult<()> {
        let found = self.inner.db.with_conn(|connection| -> DbResult<Option<(String, String, Option<String>, Option<i64>)>> {
            use rusqlite::OptionalExtension;
            let Some((name, tipo, week, link)) = connection
                .query_row(
                    "SELECT name, tipo, week, link FROM suggestions WHERE id=? AND review_status='approved'",
                    [suggestion_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, Option<String>>(3)?)),
                )
                .optional()?
            else {
                return Ok(None);
            };
            let proposer = connection
                .query_row(
                    "SELECT user_id FROM proposals WHERE suggestion_id=? AND week=? ORDER BY ts, id LIMIT 1",
                    [suggestion_id, week],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            Ok(Some((name, tipo, link, proposer)))
        })?;
        let Some((name, tipo, link, proposer)) = found else {
            return Ok(());
        };

        let mut lines = vec!["🔥 **Nuova proposta**".to_string(), format!("**{name}** · {tipo}")];
        if let Some(user_id) = proposer {
            lines.push(format!("Proposta da <@{user_id}>"));
        }
        // Copertina Steam/YouTube: rende l'annuncio più riconoscibile.
        let embed = match (proposal_image_url(link.as_deref()), link.as_deref()) {
            (Some(image_url), Some(link)) => {
                Some(embeds::embed(EmbedKind::Info).title(truncate_chars(&name, 256)).url(link).image(image_url))
            }
            _ => None,
        };
        self.publish_activity(lines.join("\n"), embed).await;
        Ok(())
    }

    // --- notifiche private ---

    /// Invia un DM a ciascun utente (una volta sola) che ha attivato le notifiche private.
    pub async fn send_personal_notifications(&self, user_ids: &[u64], message: &str) {
        let mut seen = std::collections::HashSet::new();
        for &user_id in user_ids {
            if !seen.insert(user_id) {
                continue;
            }
            let enabled = self.inner.db.with_conn(|connection| -> DbResult<bool> {
                use rusqlite::OptionalExtension;
                let value: Option<Option<i64>> = connection
                    .query_row(
                        "SELECT dm_enabled FROM user_notification_preferences WHERE user_id=?",
                        [user_id as i64],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(value.flatten().is_some_and(|flag| flag != 0))
            });
            match enabled {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    error!("Impossibile leggere le preferenze di notifica dell'utente {user_id}: {e}");
                    continue;
                }
            }
            if let Err(e) = self.inner.sender.send_direct_message(user_id, message.to_string()).await {
                error!("Impossibile inviare una notifica privata all'utente {user_id}: {e}");
            }
        }
    }
}

/// Analizza una stringa contenente un ID canale dal valore ambiente o dalle impostazioni.
///
/// Restituisce `Some(id)` se la stringa è un numero `u64` non nullo, altrimenti registra un
/// warning e restituisce `None`.
fn parse_channel_id(raw: &str, source: &str) -> Option<u64> {
    match raw.trim().parse::<u64>() {
        Ok(id) if id != 0 => Some(id),
        _ => {
            warn!("ID canale non valido in {source}: {raw:?}");
            None
        }
    }
}

/// Finto mittente Discord per i test di questo e degli altri moduli: registra ciò che "invia".
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// Quello che il finto mittente ha "inviato".
    #[derive(Debug, Clone)]
    pub(crate) enum Sent {
        Channel { channel_id: u64, content: String, embed: Option<Value> },
        Direct { user_id: u64, content: String },
    }

    #[derive(Clone, Default)]
    pub(crate) struct FakeSender {
        pub(crate) sent: Arc<Mutex<Vec<Sent>>>,
        /// Utenti/canali a cui l'invio fallisce (come un DM chiuso o un 403).
        pub(crate) failing: Arc<Mutex<Vec<u64>>>,
    }

    impl FakeSender {
        pub(crate) fn sent(&self) -> Vec<Sent> {
            self.sent.lock().unwrap().clone()
        }
        pub(crate) fn fail_for(&self, id: u64) {
            self.failing.lock().unwrap().push(id);
        }
    }

    impl DiscordSender for FakeSender {
        async fn send_to_channel(&self, channel_id: u64, message: OutgoingMessage) -> Result<(), SendError> {
            if self.failing.lock().unwrap().contains(&channel_id) {
                return Err(SendError("403 Forbidden".into()));
            }
            self.sent.lock().unwrap().push(Sent::Channel {
                channel_id,
                content: message.content,
                embed: message.embed.map(|embed| serde_json::to_value(embed).unwrap()),
            });
            Ok(())
        }
        async fn send_direct_message(&self, user_id: u64, content: String) -> Result<(), SendError> {
            if self.failing.lock().unwrap().contains(&user_id) {
                return Err(SendError("Cannot send messages to this user".into()));
            }
            self.sent.lock().unwrap().push(Sent::Direct { user_id, content });
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeSender, Sent};
    use super::*;
    use serde_json::json;

    fn notifier() -> (Notifier<FakeSender>, FakeSender, Database) {
        let db = Database::open_in_memory().unwrap();
        let sender = FakeSender::default();
        // Ambiente isolato: i test non dipendono da BOT_AUDIT_CHANNEL_ID del processo.
        (Notifier::with_audit_channel(db.clone(), sender.clone(), None), sender, db)
    }

    fn channel_texts(sender: &FakeSender) -> Vec<(u64, String)> {
        sender
            .sent()
            .into_iter()
            .filter_map(|sent| match sent {
                Sent::Channel { channel_id, content, .. } => Some((channel_id, content)),
                Sent::Direct { .. } => None,
            })
            .collect()
    }

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    // --- testo -------------------------------------------------------------------------------

    #[test]
    fn python_json_uses_python_separators_and_keeps_order_and_unicode() {
        let value = json!({"name": "Caffè «☕»", "week": 3, "tags": ["a", "b"], "nested": {"ok": true, "none": null}, "x": 1.5});
        assert_eq!(
            python_json(&value),
            r#"{"name": "Caffè «☕»", "week": 3, "tags": ["a", "b"], "nested": {"ok": true, "none": null}, "x": 1.5}"#
        );
        assert_eq!(python_json(&json!({})), "{}");
        assert_eq!(python_json(&json!([])), "[]");
        assert_eq!(python_json(&json!("a\"b\\c\n")), r#""a\"b\\c\n""#);
    }

    #[test]
    fn staff_log_chunks_join_lines_and_respect_the_limit() {
        assert!(chunk_staff_log(&[], 1900).is_empty());
        assert_eq!(chunk_staff_log(&lines(&["a", "b", "c"]), 1900), ["a\nb\nc"]);
        // 3 righe da 10 caratteri con limite 25: 10+1+10=21 ok, +1+10=32 > 25 -> nuovo messaggio.
        let ten = "0123456789";
        assert_eq!(chunk_staff_log(&lines(&[ten, ten, ten]), 25), [format!("{ten}\n{ten}"), ten.to_string()]);
        // Esattamente al limite resta nello stesso messaggio (Python: `> limit` spezza).
        assert_eq!(chunk_staff_log(&lines(&[ten, ten]), 21), [format!("{ten}\n{ten}")]);
        assert_eq!(chunk_staff_log(&lines(&[ten, ten]), 20).len(), 2);
    }

    #[test]
    fn overlong_lines_are_split_instead_of_producing_empty_messages() {
        let long = "x".repeat(45);
        let chunks = chunk_staff_log(&lines(&["prima", &long, "dopo"]), 20);
        assert!(chunks.iter().all(|chunk| !chunk.is_empty() && chunk.chars().count() <= 20), "{chunks:?}");
        assert_eq!(chunks.join("").replace('\n', ""), format!("prima{long}dopo"));
        // Riga multipla esatta del limite: nessun messaggio vuoto.
        let exact = chunk_staff_log(&lines(&[&"y".repeat(40)]), 20);
        assert_eq!(exact, ["y".repeat(20), "y".repeat(20)]);
        // Caratteri non ASCII: si conta per caratteri, non per byte.
        let accented = chunk_staff_log(&lines(&[&"è".repeat(30)]), 20);
        assert_eq!(accented.iter().map(|c| c.chars().count()).collect::<Vec<_>>(), [20, 10]);
    }

    #[test]
    fn audit_message_matches_the_python_layout() {
        let entry = AuditEntry::new(Some(123), "proposal_unselected")
            .entity("suggestion", 42)
            .reason(Some("Duplicata"))
            .details(json!({"name": "Among Us", "week": 3}));
        assert_eq!(
            audit_message(1_794_222_000, &entry),
            "**Audit · proposal_unselected** · <t:1794222000:F>\nAttore: <@123>\nEntità: suggestion · `42`\nMotivo: Duplicata\nDettagli: `{\"name\": \"Among Us\", \"week\": 3}`"
        );
        let bare = AuditEntry::new(None, "reset");
        assert_eq!(
            audit_message(1, &bare),
            "**Audit · reset** · <t:1:F>\nAttore: Sistema\nEntità: — · `—`\nMotivo: —\nDettagli: `{}`"
        );
    }

    /// Riga di database e messaggio prodotti dal vero `sfo/notify.py` (con un bot finto)
    /// per gli stessi input: unicode, numeri, null, booleani annidati; attore 0; tipo vuoto.
    #[tokio::test(start_paused = true)]
    async fn matches_the_python_audit_output_for_tricky_inputs() {
        let (notifier, _sender, db) = notifier();
        let tricky = AuditEntry::new(Some(7), "caffè")
            .entity("tipo", "x")
            .reason(Some("Motivo con «accenti» e ☕"))
            .details(json!({"nome": "Caffè ☕", "lista": [1, 2.5, null, true], "ann": {"k": "v"}}));
        let zero_actor = AuditEntry::new(Some(0), "zero_attore").entity("", 0).reason(Some("")).details(json!({}));

        assert_eq!(
            audit_message(0, &tricky),
            "**Audit · caffè** · <t:0:F>\nAttore: <@7>\nEntità: tipo · `x`\nMotivo: Motivo con «accenti» e ☕\nDettagli: `{\"nome\": \"Caffè ☕\", \"lista\": [1, 2.5, null, true], \"ann\": {\"k\": \"v\"}}`"
        );
        assert_eq!(
            audit_message(0, &zero_actor),
            "**Audit · zero_attore** · <t:0:F>\nAttore: Sistema\nEntità: — · `0`\nMotivo: —\nDettagli: `{}`"
        );

        notifier.record_staff_audit(tricky);
        notifier.record_staff_audit(zero_actor);
        let rows = audit_rows(&db);
        // (actor_id, action, entity_type, entity_id, reason, details) come le righe scritte da Python.
        assert_eq!(
            (rows[0].1, rows[0].2.as_str(), rows[0].3.as_deref(), rows[0].4.as_deref(), rows[0].5.as_deref(), rows[0].6.as_str()),
            (
                Some(7),
                "caffè",
                Some("tipo"),
                Some("x"),
                Some("Motivo con «accenti» e ☕"),
                r#"{"nome": "Caffè ☕", "lista": [1, 2.5, null, true], "ann": {"k": "v"}}"#
            )
        );
        assert_eq!(
            (rows[1].1, rows[1].2.as_str(), rows[1].3.as_deref(), rows[1].4.as_deref(), rows[1].5.as_deref(), rows[1].6.as_str()),
            (Some(0), "zero_attore", None, Some("0"), None, "{}")
        );
    }

    #[test]
    fn audit_message_is_truncated_field_by_field_and_overall() {
        let entry = AuditEntry::new(Some(1), "a".repeat(150))
            .entity("t", 1)
            .reason(Some("r".repeat(400).as_str()))
            .details(json!({"k": "d".repeat(2000)}));
        let message = audit_message(5, &entry);
        assert!(message.starts_with(&format!("**Audit · {}**", "a".repeat(100))));
        assert!(message.contains(&format!("Motivo: {}\n", "r".repeat(300))));
        assert!(message.chars().count() <= 1900);
        let details_line = message.lines().last().unwrap();
        // "Dettagli: `" + 900 caratteri di JSON + apice di chiusura.
        assert_eq!(details_line.chars().count(), "Dettagli: `".chars().count() + 900 + 1);

        // Il tipo di entità non ha un limite proprio: il taglio finale a 1900 protegge il messaggio.
        let huge = AuditEntry::new(Some(1), "azione").entity("t".repeat(3000), 1);
        assert_eq!(audit_message(5, &huge).chars().count(), 1900);
    }

    // --- log staff ---------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn staff_log_does_nothing_without_a_channel() {
        let (notifier, sender, _db) = notifier();
        notifier.staff_log("ciao");
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(sender.sent().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn staff_log_batches_close_lines_and_starts_again_afterwards() {
        let (notifier, sender, db) = notifier();
        db.set_setting("staff_channel", 777).unwrap();

        notifier.staff_log("uno");
        notifier.staff_log("due");
        tokio::time::sleep(Duration::from_millis(100)).await;
        notifier.staff_log("tre"); // ancora dentro la finestra di 500 ms
        assert!(sender.sent().is_empty(), "niente prima dello scadere dell'attesa");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(channel_texts(&sender), [(777, "uno\ndue\ntre".to_string())]);

        // Dopo lo svuotamento la coda riparte da capo.
        notifier.staff_log("quattro");
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(channel_texts(&sender).len(), 2);
        assert_eq!(channel_texts(&sender)[1], (777, "quattro".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn staff_log_failure_does_not_stop_later_chunks_or_batches() {
        let (notifier, sender, db) = notifier();
        db.set_setting("staff_channel", 777).unwrap();
        sender.fail_for(777);
        notifier.staff_log("perso");
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(sender.sent().is_empty());
        sender.failing.lock().unwrap().clear();
        notifier.staff_log("arriva");
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(channel_texts(&sender), [(777, "arriva".to_string())]);
    }

    #[tokio::test(start_paused = true)]
    async fn staff_log_splits_big_batches_into_several_messages() {
        let (notifier, sender, db) = notifier();
        db.set_setting("staff_channel", 5).unwrap();
        for index in 0..4 {
            notifier.staff_log(format!("{index}{}", "z".repeat(599)));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let texts = channel_texts(&sender);
        // 600+1+600+1+600 = 1802 entra in 1900; la quarta riga apre un secondo messaggio.
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[0].1.lines().count(), 3);
        assert_eq!(texts[1].1.lines().count(), 1);
        assert!(texts.iter().all(|(_, text)| text.chars().count() <= 1900));
    }

    #[test]
    fn staff_log_outside_a_runtime_is_dropped_without_panicking() {
        let (notifier, _sender, db) = notifier();
        db.set_setting("staff_channel", 5).unwrap();
        notifier.staff_log("nessun runtime"); // non deve andare in panic
        assert!(!notifier.staff_log_state().running, "il flag non resta bloccato");
    }

    // --- audit -------------------------------------------------------------------------------

    fn audit_rows(db: &Database) -> Vec<(String, Option<i64>, String, Option<String>, Option<String>, Option<String>, String)> {
        db.with_conn(|c| {
            c.prepare("SELECT occurred_at, actor_id, action, entity_type, entity_id, reason, details FROM staff_audit ORDER BY id")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        })
    }

    #[tokio::test(start_paused = true)]
    async fn record_staff_audit_stores_a_row_and_publishes_it() {
        let (notifier, sender, db) = notifier();
        db.set_setting("staff_channel", 900).unwrap();
        let entry = AuditEntry::new(Some(123), "proposal_unselected")
            .entity("suggestion", 42)
            .reason(Some("Duplicata"))
            .details(json!({"name": "Among Us", "week": 3}));
        notifier.record_staff_audit(entry);
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        let rows = audit_rows(&db);
        assert_eq!(rows.len(), 1);
        let (occurred_at, actor, action, entity_type, entity_id, reason, details) = rows[0].clone();
        assert!(chrono::DateTime::parse_from_rfc3339(&occurred_at).is_ok(), "ISO 8601 con offset: {occurred_at}");
        assert_eq!(
            (actor, action.as_str(), entity_type.as_deref(), entity_id.as_deref(), reason.as_deref()),
            (Some(123), "proposal_unselected", Some("suggestion"), Some("42"), Some("Duplicata"))
        );
        assert_eq!(details, r#"{"name": "Among Us", "week": 3}"#, "stesso testo di json.dumps");

        let texts = channel_texts(&sender);
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].0, 900);
        assert!(texts[0].1.starts_with("**Audit · proposal_unselected** · <t:"));
        assert!(texts[0].1.contains("Attore: <@123>\nEntità: suggestion · `42`\nMotivo: Duplicata\n"));
    }

    #[tokio::test(start_paused = true)]
    async fn record_staff_audit_truncates_and_normalises_empty_values() {
        let (notifier, _sender, db) = notifier();
        let entry = AuditEntry::new(None, "x".repeat(150))
            .entity("", "e".repeat(300))
            .reason(Some(""))
            .details(json!({"big": "d".repeat(5000)}));
        notifier.record_staff_audit(entry);
        let rows = audit_rows(&db);
        let (_, actor, action, entity_type, entity_id, reason, details) = rows[0].clone();
        assert_eq!(actor, None);
        assert_eq!(action.len(), 100);
        assert_eq!(entity_type, None, "tipo vuoto = assente");
        assert_eq!(entity_id.unwrap().len(), 200);
        assert_eq!(reason, None, "motivo vuoto = assente");
        assert_eq!(details.chars().count(), 4000);

        notifier.record_staff_audit(AuditEntry::new(Some(1), "senza_dettagli"));
        assert_eq!(audit_rows(&db)[1].6, "{}");
    }

    #[tokio::test(start_paused = true)]
    async fn audit_channel_env_overrides_the_staff_channel() {
        let db = Database::open_in_memory().unwrap();
        db.set_setting("staff_channel", 900).unwrap();
        let sender = FakeSender::default();
        let notifier = Notifier::with_audit_channel(db.clone(), sender.clone(), Some("4242".into()));
        notifier.record_staff_audit(AuditEntry::new(Some(1), "azione"));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(channel_texts(&sender)[0].0, 4242);

        // Valore vuoto: si ricade sul canale staff, come `env or setting` in Python.
        let blank = Notifier::with_audit_channel(db, sender.clone(), Some("  ".into()));
        blank.record_staff_audit(AuditEntry::new(Some(1), "azione"));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(channel_texts(&sender)[1].0, 900);
    }

    #[tokio::test(start_paused = true)]
    async fn audit_without_channel_is_still_stored() {
        let (notifier, sender, db) = notifier();
        notifier.record_staff_audit(AuditEntry::new(Some(1), "solo_db"));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(audit_rows(&db).len(), 1);
        assert!(sender.sent().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn audit_database_error_is_logged_and_nothing_is_published() {
        let (notifier, sender, db) = notifier();
        db.set_setting("staff_channel", 900).unwrap();
        db.execute("DROP TABLE staff_audit", []).unwrap();
        notifier.record_staff_audit(AuditEntry::new(Some(1), "fallita"));
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(sender.sent().is_empty());
    }

    #[test]
    fn audit_outside_a_runtime_still_writes_to_the_database() {
        let (notifier, _sender, db) = notifier();
        db.set_setting("staff_channel", 900).unwrap();
        notifier.record_staff_audit(AuditEntry::new(Some(1), "senza_runtime"));
        assert_eq!(audit_rows(&db).len(), 1);
    }

    // --- feed attività -----------------------------------------------------------------------

    #[tokio::test]
    async fn activity_feed_needs_the_flag_and_a_channel() {
        let (notifier, sender, db) = notifier();
        notifier.publish_activity("niente", None).await;
        db.set_setting("activity_channel", 55).unwrap();
        notifier.publish_activity("ancora niente (feed spento)", None).await;
        db.set_setting("activity_feed_enabled", "0").unwrap();
        notifier.publish_activity("spento", None).await;
        assert!(sender.sent().is_empty());

        db.set_setting("activity_feed_enabled", "1").unwrap();
        notifier.publish_activity("acceso", None).await;
        assert_eq!(channel_texts(&sender), [(55, "acceso".to_string())]);

        db.set_setting("activity_channel", "non-un-numero").unwrap();
        notifier.publish_activity("canale rotto", None).await;
        assert_eq!(sender.sent().len(), 1, "ID canale non valido: ignorato");
    }

    async fn announce_setup() -> (Notifier<FakeSender>, FakeSender, Database) {
        let (notifier, sender, db) = notifier();
        db.set_setting("activity_feed_enabled", "1").unwrap();
        db.set_setting("activity_channel", 55).unwrap();
        db.execute_batch(
            "INSERT INTO suggestions(id,week,norm_name,name,tipo,link,review_status) VALUES
                (1,1,'a','Among Us','Gioco','https://www.youtube.com/watch?v=dQw4w9WgXcQ','approved'),
                (2,1,'b','Senza copertina','Film','https://example.com/x','approved'),
                (3,1,'c','Non approvata','Gioco',NULL,'pending'),
                (4,1,'d','Orfana','Altro',NULL,'approved');
             INSERT INTO proposals(suggestion_id,user_id,week,ts) VALUES (1,20,1,200),(1,10,1,100),(2,30,1,50);",
        )
        .unwrap();
        (notifier, sender, db)
    }

    #[tokio::test]
    async fn approved_proposal_is_announced_with_cover_embed() {
        let (notifier, sender, _db) = announce_setup().await;
        notifier.announce_approved_proposal(1).await.unwrap();
        let Sent::Channel { channel_id, content, embed } = sender.sent().remove(0) else { panic!("atteso un canale") };
        assert_eq!(channel_id, 55);
        assert_eq!(content, "🔥 **Nuova proposta**\n**Among Us** · Gioco\nProposta da <@10>", "il primo proponente per ts");
        let embed = embed.expect("copertina YouTube");
        assert_eq!(embed["title"], "Among Us");
        assert_eq!(embed["url"], "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(embed["image"]["url"], "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg");
    }

    #[tokio::test]
    async fn announcement_without_cover_proposer_or_approval() {
        let (notifier, sender, _db) = announce_setup().await;
        notifier.announce_approved_proposal(2).await.unwrap(); // link senza copertina
        notifier.announce_approved_proposal(3).await.unwrap(); // non approvata
        notifier.announce_approved_proposal(4).await.unwrap(); // senza proponente
        notifier.announce_approved_proposal(999).await.unwrap(); // inesistente
        let texts = channel_texts(&sender);
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[0].1, "🔥 **Nuova proposta**\n**Senza copertina** · Film\nProposta da <@30>");
        assert_eq!(texts[1].1, "🔥 **Nuova proposta**\n**Orfana** · Altro");
        assert!(sender.sent().iter().all(|sent| matches!(sent, Sent::Channel { embed: None, .. })));
    }

    // --- notifiche private -------------------------------------------------------------------

    #[tokio::test]
    async fn personal_notifications_reach_only_opted_in_users_once_each() {
        let (notifier, sender, db) = notifier();
        db.execute_batch(
            "INSERT INTO user_notification_preferences(user_id,dm_enabled) VALUES (1,1),(2,0),(4,1),(5,1);",
        )
        .unwrap();
        sender.fail_for(4); // DM chiusi
        notifier.send_personal_notifications(&[1, 2, 3, 1, 4, 5, 5], "La tua proposta è stata scelta!").await;
        let direct: Vec<(u64, String)> = sender
            .sent()
            .into_iter()
            .filter_map(|sent| match sent {
                Sent::Direct { user_id, content } => Some((user_id, content)),
                Sent::Channel { .. } => None,
            })
            .collect();
        assert_eq!(
            direct,
            [(1, "La tua proposta è stata scelta!".to_string()), (5, "La tua proposta è stata scelta!".to_string())],
            "2 non ha acconsentito, 3 non ha preferenze, 4 fallisce ma non blocca 5, doppioni ignorati"
        );
    }
}
