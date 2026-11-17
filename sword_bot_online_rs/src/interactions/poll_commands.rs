//! `cogs/polls.py`: `/sondaggio crea|rimuovi|pubblica`, `/opzione aggiungi|rimuovi` e i loro
//! autocompletamenti.

use rusqlite::OptionalExtension;
use serde_json::json;
use tracing::error;
use unicode_general_category::{GeneralCategory, get_general_category};

use super::commands::{ConfirmAction, confirm_rows};
use super::{InteractionIo, Interactions, Io, IoError, Reply};
use crate::database::Database;
use crate::discord_api::DiscordApi;
use crate::formatting::{casefold, truncate_chars};
use crate::notify::{AuditEntry, DiscordSender};
use crate::queries::poll_options;

const NOT_ADMIN: &str = "⚠️ Non hai i permessi per usare questo comando.";
const POLL_NOT_FOUND: &str = "❌ Sondaggio non trovato.";
const CHANNELS_FIRST: &str = "⚠️ Prima configura i canali con /config.";
const MAX_OPTIONS: usize = 25;
const AUTOCOMPLETE_LIMIT: usize = 25;

/// Gli argomenti di `/sondaggio crea`.
#[derive(Debug, Clone)]
pub struct CreatePollRequest {
    pub name: String,
    pub emoji: String,
    pub color: String,
    pub community: bool,
    pub reason: Option<String>,
}

impl CreatePollRequest {
    /// Con i valori predefiniti di Python: emoji 🎮, colore #5865F2, non community.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), emoji: "🎮".into(), color: "#5865F2".into(), community: false, reason: None }
    }
}

// --- validazione -----------------------------------------------------------------------------

/// `#?[0-9a-fA-F]{6}` (intera stringa).
pub fn valid_color(color: &str) -> bool {
    let digits = color.strip_prefix('#').unwrap_or(color);
    digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `<a?:[A-Za-z0-9_]{2,32}:\d{17,20}>` (intera stringa): un'emoji personalizzata di Discord.
fn valid_custom_emoji(emoji: &str) -> bool {
    let Some(inner) = emoji.strip_prefix('<').and_then(|rest| rest.strip_suffix('>')) else {
        return false;
    };
    let inner = inner.strip_prefix('a').filter(|rest| rest.starts_with(':')).unwrap_or(inner);
    let Some(rest) = inner.strip_prefix(':') else {
        return false;
    };
    let Some((name, id)) = rest.split_once(':') else {
        return false;
    };
    (2..=32).contains(&name.chars().count())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && (17..=20).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_digit())
}

/// Una o più emoji Unicode, secondo le stesse regole di Python: da 1 a 32 caratteri, almeno un
/// simbolo «So»/«Sk» (o un tasto con U+20E3 finale) e nient'altro che simboli, segni, numeri o
/// caratteri di controllo/formato (così passano ZWJ, selettori di variante e toni della pelle).
fn valid_unicode_emoji(emoji: &str) -> bool {
    use GeneralCategory::*;
    let categories: Vec<GeneralCategory> = emoji.chars().map(get_general_category).collect();
    let count = categories.len();
    (1..=32).contains(&count)
        && (categories.iter().any(|c| matches!(c, OtherSymbol | ModifierSymbol)) || emoji.ends_with('\u{20e3}'))
        && categories.iter().all(|c| {
            matches!(
                c,
                MathSymbol | CurrencySymbol | ModifierSymbol | OtherSymbol
                    | NonspacingMark | SpacingMark | EnclosingMark
                    | Control | Format | Surrogate | PrivateUse | Unassigned
                    | DecimalNumber | LetterNumber | OtherNumber
            )
        })
}

pub fn valid_emoji(emoji: &str) -> bool {
    valid_custom_emoji(emoji) || valid_unicode_emoji(emoji)
}

// --- autocompletamento -----------------------------------------------------------------------

/// Nomi dei sondaggi che contengono `current` (come `LIKE`), per posizione, al massimo 25.
/// Discord rifiuta l'intero elenco se una scelta supera i 100 caratteri: quelle più lunghe
/// (create prima del limite) si escludono.
pub fn poll_choices(db: &Database, current: &str) -> rusqlite::Result<Vec<String>> {
    let names: Vec<String> = db.with_conn(|connection| -> rusqlite::Result<_> {
        connection
            .prepare("SELECT name FROM polls WHERE name LIKE ? ORDER BY position LIMIT 25")?
            .query_map([format!("%{current}%")], |row| row.get(0))?
            .collect()
    })?;
    Ok(names.into_iter().filter(|name| name.chars().count() <= 100).collect())
}

/// Etichette delle opzioni del sondaggio `poll_name` che contengono `current`, al massimo 25,
/// troncate a 100 caratteri.
pub fn option_choices(db: &Database, poll_name: Option<&str>, current: &str) -> rusqlite::Result<Vec<String>> {
    let Some(poll_name) = poll_name else {
        return Ok(Vec::new());
    };
    let poll: Option<i64> = db.with_conn(|c| c.query_row("SELECT id FROM polls WHERE name=?", [poll_name], |r| r.get(0)).optional())?;
    let Some(poll_id) = poll else {
        return Ok(Vec::new());
    };
    let needle = casefold(current);
    Ok(poll_options(db, poll_id)?
        .into_iter()
        .filter(|option| casefold(&option.label).contains(&needle))
        .map(|option| truncate_chars(&option.label, 100))
        .take(AUTOCOMPLETE_LIMIT)
        .collect())
}

// --- comandi ---------------------------------------------------------------------------------

fn db_error(error: rusqlite::Error) -> IoError {
    IoError(error.to_string())
}

fn is_constraint_violation(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(failure, _) if failure.code == rusqlite::ErrorCode::ConstraintViolation)
}

impl<S: DiscordSender, A: DiscordApi> Interactions<S, A> {
    fn poll_id_by_name(&self, name: &str) -> rusqlite::Result<Option<i64>> {
        self.db().with_conn(|c| c.query_row("SELECT id FROM polls WHERE name=?", [name], |r| r.get(0)).optional())
    }

    /// `/sondaggio crea`.
    pub async fn command_poll_create<I: InteractionIo>(&self, io: &I, request: CreatePollRequest) -> Io<()> {
        if !io.is_admin() {
            return io.respond(Reply::text(NOT_ADMIN)).await;
        }
        if !valid_color(&request.color) {
            return io.respond(Reply::text("❌ Colore non valido, usa il formato #RRGGBB.")).await;
        }
        if !valid_emoji(&request.emoji) {
            return io
                .respond(Reply::text("❌ Emoji non valida: usa una singola emoji o un'emoji personalizzata di Discord."))
                .await;
        }
        let color = format!("#{}", request.color.trim_start_matches('#'));
        let created = self.db().transaction(|tx| -> rusqlite::Result<Result<i64, &'static str>> {
            if request.community {
                let active: i64 = tx.query_row("SELECT COUNT(*) FROM polls WHERE is_community=1 AND active=1", [], |r| r.get(0))?;
                if active > 0 {
                    return Ok(Err("⚠️ Esiste già un sondaggio community attivo."));
                }
            }
            let position: i64 = tx.query_row("SELECT COALESCE(MAX(position),0)+1 FROM polls", [], |r| r.get(0))?;
            tx.execute(
                "INSERT INTO polls(name,emoji,color,position,is_community) VALUES(?,?,?,?,?)",
                rusqlite::params![request.name, request.emoji, color, position, i64::from(request.community)],
            )?;
            Ok(Ok(tx.last_insert_rowid()))
        });
        let poll_id = match created {
            Ok(Ok(id)) => id,
            Ok(Err(message)) => return io.respond(Reply::text(message)).await,
            Err(e) if is_constraint_violation(&e) => {
                return io.respond(Reply::text("⚠️ Esiste già un sondaggio con questo nome.")).await;
            }
            Err(e) => {
                error!("Errore SQLite durante la creazione di un sondaggio: {e}");
                return io.respond(Reply::text("❌ Non è stato possibile creare il sondaggio. Controlla i log.")).await;
            }
        };
        io.respond(Reply::text(format!("✅ Sondaggio **{}** creato.", request.name))).await?;
        self.services.notifier.record_staff_audit(
            AuditEntry::new(u64::try_from(io.user_id()).ok(), "poll_created")
                .entity("poll", poll_id)
                .reason(request.reason.as_deref())
                .details(json!({"name": request.name, "community": request.community})),
        );
        let publisher = &self.services.publisher;
        if let Err(e) = publisher.refresh_poll(poll_id).await {
            error!("Impossibile pubblicare il sondaggio {poll_id}: {e}");
        }
        if let Err(e) = publisher.refresh_dashboard().await {
            error!("Aggiornamento della dashboard non riuscito: {e}");
        }
        Ok(())
    }

    /// `/sondaggio rimuovi`: chiede conferma, poi elimina il sondaggio con opzioni e voti.
    pub async fn command_poll_remove<I: InteractionIo>(&self, io: &I, name: &str, reason: Option<String>) -> Io<()> {
        if !io.is_admin() {
            return io.respond(Reply::text(NOT_ADMIN)).await;
        }
        let Some(poll_id) = self.poll_id_by_name(name).map_err(db_error)? else {
            return io.respond(Reply::text(POLL_NOT_FOUND)).await;
        };
        let nonce = self.remember_confirmation(io.user_id(), ConfirmAction::DeletePoll { poll_id, name: name.to_string(), reason });
        io.respond(
            Reply::text(format!("🗑️ Eliminare il sondaggio **{name}** con tutte le sue opzioni e i voti? L'operazione non si può annullare."))
                .with_components(confirm_rows(nonce, "Elimina", true)),
        )
        .await
    }

    pub(super) async fn confirmed_delete_poll<I: InteractionIo>(&self, io: &I, poll_id: i64, name: String, reason: Option<String>) -> Io<()> {
        io.update(Reply::text("⏳ Eliminazione in corso…").without_components()).await?;
        let deleted = self.services.publisher.delete_poll(poll_id).await.map_err(|e| IoError(e.to_string()))?;
        let Some(message_deleted) = deleted else {
            return io.followup(Reply::text(POLL_NOT_FOUND)).await;
        };
        let message = if message_deleted {
            format!("🗑️ Sondaggio **{name}** eliminato.")
        } else {
            format!("⚠️ Sondaggio **{name}** eliminato dal database, ma non è stato possibile rimuovere il messaggio Discord. Controlla i log.")
        };
        io.followup(Reply::text(message)).await?;
        self.services.notifier.record_staff_audit(
            AuditEntry::new(u64::try_from(io.user_id()).ok(), "poll_deleted")
                .entity("poll", poll_id)
                .reason(reason.as_deref())
                .details(json!({"name": name})),
        );
        if let Err(e) = self.services.publisher.refresh_dashboard().await {
            error!("Aggiornamento della dashboard non riuscito: {e}");
        }
        Ok(())
    }

    /// `/sondaggio pubblica`: pubblica o ricrea tutti i messaggi dei sondaggi.
    pub async fn command_poll_publish<I: InteractionIo>(&self, io: &I) -> Io<()> {
        if !io.is_admin() {
            return io.respond(Reply::text(NOT_ADMIN)).await;
        }
        if !self.setting_is_set("poll_channel") {
            return io.respond(Reply::text("⚠️ Prima imposta il canale con /config.")).await;
        }
        io.defer().await?;
        if let Err(e) = self.services.publisher.refresh_all().await {
            error!("Aggiornamento di tutti i sondaggi non riuscito: {e}");
        }
        io.followup(Reply::text("✅ Sondaggi aggiornati.")).await
    }

    /// `/opzione aggiungi`.
    pub async fn command_option_add<I: InteractionIo>(&self, io: &I, poll_name: &str, name: &str, reason: Option<String>) -> Io<()> {
        if !self.is_staff(io) {
            return io.respond(Reply::text(NOT_ADMIN)).await;
        }
        if !self.setting_is_set("poll_channel") || !self.setting_is_set("staff_channel") {
            return io.respond(Reply::text(CHANNELS_FIRST)).await;
        }
        let Some(poll_id) = self.poll_id_by_name(poll_name).map_err(db_error)? else {
            return io.respond(Reply::text(POLL_NOT_FOUND)).await;
        };
        let existing = poll_options(self.db(), poll_id).map_err(db_error)?;
        if existing.len() >= MAX_OPTIONS {
            return io.respond(Reply::text("⚠️ Massimo 25 opzioni per sondaggio.")).await;
        }
        let wanted = casefold(name);
        if existing.iter().any(|option| casefold(&option.label) == wanted) {
            return io.respond(Reply::text("⚠️ Questa opzione esiste già nel sondaggio.")).await;
        }
        self.db().execute("INSERT INTO options(poll_id,label) VALUES(?,?)", rusqlite::params![poll_id, name]).map_err(db_error)?;
        self.services.notifier.record_staff_audit(
            AuditEntry::new(u64::try_from(io.user_id()).ok(), "poll_option_added")
                .entity("poll", poll_id)
                .reason(reason.as_deref())
                .details(json!({"poll": poll_name, "option": name})),
        );
        io.respond(Reply::text(format!("✅ Aggiunta **{name}**."))).await?;
        let publisher = &self.services.publisher;
        if let Err(e) = publisher.refresh_poll(poll_id).await {
            error!("Aggiornamento del sondaggio {poll_id} non riuscito: {e}");
        }
        if let Err(e) = publisher.refresh_dashboard().await {
            error!("Aggiornamento della dashboard non riuscito: {e}");
        }
        self.services.notifier.staff_log(format!("➕ <@{}> ha aggiunto «{name}» a **{poll_name}**.", io.user_id()));
        Ok(())
    }

    /// `/opzione rimuovi`: chiede conferma, poi toglie l'opzione (e riporta «in attesa» la proposta
    /// scelta che rappresentava).
    pub async fn command_option_remove<I: InteractionIo>(&self, io: &I, poll_name: &str, name: &str, reason: Option<String>) -> Io<()> {
        if !self.is_staff(io) {
            return io.respond(Reply::text(NOT_ADMIN)).await;
        }
        if !self.setting_is_set("poll_channel") || !self.setting_is_set("staff_channel") {
            return io.respond(Reply::text(CHANNELS_FIRST)).await;
        }
        let Some(poll_id) = self.poll_id_by_name(poll_name).map_err(db_error)? else {
            return io.respond(Reply::text(POLL_NOT_FOUND)).await;
        };
        let nonce = self.remember_confirmation(
            io.user_id(),
            ConfirmAction::RemoveOption { poll_id, poll_name: poll_name.to_string(), option_name: name.to_string(), reason },
        );
        io.respond(
            Reply::text(format!("🗑️ Rimuovere l'opzione **{name}** da **{poll_name}**? I suoi voti andranno persi."))
                .with_components(confirm_rows(nonce, "Rimuovi", true)),
        )
        .await
    }

    pub(super) async fn confirmed_remove_option<I: InteractionIo>(
        &self,
        io: &I,
        poll_id: i64,
        poll_name: String,
        option_name: String,
        reason: Option<String>,
    ) -> Io<()> {
        let now = self.now();
        let removed = self.db().transaction(|tx| -> rusqlite::Result<Option<Option<i64>>> {
            let option: Option<(i64, Option<i64>)> = tx
                .query_row("SELECT id, suggestion_id FROM options WHERE poll_id=? AND label=?", rusqlite::params![poll_id, option_name], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            let Some((option_id, suggestion_id)) = option else {
                return Ok(None);
            };
            if let Some(suggestion_id) = suggestion_id {
                let unpicked = tx.execute(
                    "UPDATE suggestions SET status='pending', picked_by=NULL, picked_at=NULL WHERE id=? AND status='picked'",
                    [suggestion_id],
                )?;
                if unpicked > 0 {
                    let week = crate::database::current_week(tx)?;
                    crate::stats::revoke_ballot_entry_stats(tx, suggestion_id, week, now)?;
                }
            }
            tx.execute("DELETE FROM options WHERE id=?", [option_id])?;
            Ok(Some(suggestion_id))
        });
        let suggestion_id = match removed {
            Err(e) => {
                error!("Errore SQLite durante la rimozione di un'opzione: {e}");
                return io.update(Reply::text("❌ Non è stato possibile rimuovere l'opzione. Riprova tra poco.").without_components()).await;
            }
            Ok(None) => return io.update(Reply::text("❌ Opzione non trovata.").without_components()).await,
            Ok(Some(suggestion_id)) => suggestion_id,
        };
        io.update(Reply::text(format!("🗑️ Rimossa **{option_name}**.")).without_components()).await?;
        self.services.notifier.record_staff_audit(
            AuditEntry::new(u64::try_from(io.user_id()).ok(), "poll_option_removed")
                .entity("poll", poll_id)
                .reason(reason.as_deref())
                .details(json!({"poll": poll_name, "option": option_name})),
        );
        crate::hooks::publish_site_change("polls");
        if suggestion_id.is_some() {
            crate::hooks::publish_site_change("proposals");
        }
        let publisher = &self.services.publisher;
        if let Err(e) = publisher.refresh_poll(poll_id).await {
            error!("Aggiornamento del sondaggio {poll_id} non riuscito: {e}");
        }
        if let Err(e) = publisher.refresh_dashboard().await {
            error!("Aggiornamento della dashboard non riuscito: {e}");
        }
        self.services.notifier.staff_log(format!("➖ <@{}> ha rimosso «{option_name}» da **{poll_name}**.", io.user_id()));
        Ok(())
    }
}
