//! Reset settimanale: chiusura della settimana, storico e riapertura (equivalente di `sfo/reset.py`).
//!
//! * [`reset_week`]: la parte di database, in una sola transazione (premi e statistiche, istantanea
//!   della settimana nello storico, pulizia, riapertura dei sondaggi, nuova settimana);
//! * [`ResetService`]: tutto quello che segue su Discord (aggiornamento dei messaggi, riepilogo,
//!   notifiche, log staff), il promemoria delle 24 ore e i due cicli periodici.
//!
//! Dopo il reset il vincitore viene proposto allo staff per creare l'evento della Game Night
//! (vedi `gamenight`).
// Collegato in `main` (cicli) e ai comandi di amministrazione.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration};
use chrono_tz::Tz;
use poise::serenity_prelude::{CreateActionRow, CreateButton, ReactionType};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use tracing::{error, warn};

use crate::cards::{EmbedSpec, build_weekly_recap_embed};
use crate::config::{COLOR_PRIMARY, HISTORY_RETENTION};
use crate::database::{self, Database, DbResult};
use crate::discord_api::{ApiError, DiscordApi, MessagePayload};
use crate::embeds::discord_timestamp_for;
use crate::flows::Services;
use crate::gamenight::offer_event_creation;
use crate::formatting::{casefold, python_json};
use crate::hooks::publish_site_change;
use crate::notify::{AuditEntry, DiscordSender};
use crate::queries::{poll_options, suggestion_supporter_counts};
use crate::schedule::{
    self, isoformat, last_scheduled_reset, next_scheduled_reset, poll_closing_time, reminder_time,
    shift_wall_clock, weekly_reset_pending,
};
use crate::settings;
use crate::stats::{
    Winner, finalize_week_streaks, record_weekly_contributor, record_weekly_winner_stats, update_community_records,
};

const MEDALS: [&str; 3] = ["🥇", "🥈", "🥉"];

// --- parte di database -----------------------------------------------------------------------

/// Riapre i sondaggi chiusi per scadenza e li fa durare fino al prossimo reset.
///
/// I sondaggi chiusi a mano dallo staff hanno `closes_at` NULL e restano chiusi. Se esiste già un
/// sondaggio community attivo, un altro community scaduto non si riapre (ne basta uno).
pub fn reopen_polls_for_new_week(connection: &Connection, closing_time: f64) -> DbResult<()> {
    let closed: Vec<(i64, Option<i64>)> = connection
        .prepare("SELECT id, is_community FROM polls WHERE active=0 AND closes_at IS NOT NULL ORDER BY id DESC")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<DbResult<_>>()?;
    for (id, is_community) in closed {
        let community = is_community.is_some_and(|flag| flag != 0);
        let active_community_exists = community
            && connection
                .query_row("SELECT 1 FROM polls WHERE is_community=1 AND active=1", [], |_| Ok(()))
                .map(|()| true)
                .or_else(|e| if e == rusqlite::Error::QueryReturnedNoRows { Ok(false) } else { Err(e) })?;
        if active_community_exists {
            connection.execute("UPDATE polls SET closes_at=NULL WHERE id=?", [id])?;
            continue;
        }
        connection.execute("UPDATE polls SET active=1 WHERE id=?", [id])?;
    }
    connection.execute("UPDATE polls SET closes_at=? WHERE active=1", [closing_time])?;
    Ok(())
}

/// Salva nello storico un'istantanea della settimana: sondaggi con i voti, proposte con sostenitori
/// e varianti, partecipanti. Il formato è quello letto da `cards::history_summary_lines`.
pub fn snapshot_week(connection: &Connection, week: i64, now: DateTime<Tz>) -> DbResult<()> {
    // Sondaggi e opzioni, in ordine di posizione.
    let rows: Vec<(i64, String, Option<i64>, Option<String>, Option<i64>, i64)> = connection
        .prepare(
            "SELECT p.id, p.name, p.is_community, o.label, o.suggestion_id,
                    COUNT(v.user_id) AS votes
             FROM polls p
             LEFT JOIN options o ON o.poll_id=p.id
             LEFT JOIN votes v ON v.option_id=o.id
             GROUP BY p.id, o.id
             ORDER BY p.position, p.id, o.id",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?
        .collect::<DbResult<_>>()?;
    let mut polls: Vec<(i64, String, bool, Vec<Value>)> = Vec::new();
    for (id, name, is_community, label, suggestion_id, votes) in rows {
        if polls.last().is_none_or(|(last_id, ..)| *last_id != id) {
            polls.push((id, name, is_community.is_some_and(|flag| flag != 0), Vec::new()));
        }
        if let Some(label) = label {
            polls.last_mut().expect("appena inserito").3.push(json!({"label": label, "votes": votes, "suggestion_id": suggestion_id}));
        }
    }
    let polls: Vec<Value> =
        polls.into_iter().map(|(_, name, is_community, options)| json!({"name": name, "is_community": is_community, "options": options})).collect();

    // Proposte della settimana con una riga per ogni invio.
    type Variant = (Option<String>, Option<String>);
    struct Entry {
        id: i64,
        fields: (String, String, Option<String>, Option<String>, Option<i64>, Option<String>),
        variants: Vec<Variant>,
    }
    let rows: Vec<(i64, (String, String, Option<String>, Option<String>, Option<i64>, Option<String>), Option<i64>, Variant)> = connection
        .prepare(
            "SELECT s.id, s.name, s.tipo, s.link, s.status, s.picked_by, s.picked_at,
                    p.user_id, p.raw_name, p.raw_link
             FROM suggestions s
             LEFT JOIN proposals p ON p.suggestion_id=s.id
             WHERE s.week=?
             ORDER BY s.id, p.id",
        )?
        .query_map([week], |row| {
            Ok((
                row.get(0)?,
                (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?),
                row.get(7)?,
                (row.get(8)?, row.get(9)?),
            ))
        })?
        .collect::<DbResult<_>>()?;
    let mut entries: Vec<Entry> = Vec::new();
    for (id, fields, user_id, variant) in rows {
        if entries.last().is_none_or(|entry| entry.id != id) {
            entries.push(Entry { id, fields, variants: Vec::new() });
        }
        if user_id.is_some() {
            entries.last_mut().expect("appena inserito").variants.push(variant);
        }
    }
    let ids: Vec<i64> = entries.iter().map(|entry| entry.id).collect();
    let counts = suggestion_supporter_counts(connection, week, &ids)?;
    let suggestions: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let (name, tipo, link, status, picked_by, picked_at) = &entry.fields;
            json!({
                "id": entry.id,
                "name": name,
                "tipo": tipo,
                "link": link,
                "status": status,
                "picked_by": picked_by,
                "picked_at": picked_at,
                "supporters": counts.get(&entry.id).copied().unwrap_or(0),
                "variants": entry.variants.iter().map(|(name, link)| json!({"name": name, "link": link})).collect::<Vec<_>>(),
            })
        })
        .collect();

    let participants: i64 = connection.query_row("SELECT COUNT(DISTINCT user_id) FROM votes", [], |row| row.get(0))?;
    let data = json!({"polls": polls, "suggestions": suggestions, "participants": participants});
    connection.execute(
        "INSERT INTO history(week,closed_at,data) VALUES(?,?,?)",
        params![week, isoformat(now), python_json(&data)],
    )?;
    Ok(())
}

/// Risultato della parte di database del reset, per ciò che segue su Discord.
#[derive(Debug, Clone, PartialEq)]
pub struct ResetOutcome {
    /// La settimana appena chiusa.
    pub week: i64,
    pub new_week: i64,
    pub winner: Option<Winner>,
    pub contributors: Vec<i64>,
    pub points: i64,
    /// Proposte vincitrici per autore: `(utente, nomi)`.
    pub winning_by_user: Vec<(i64, Vec<String>)>,
}

/// Chiude la settimana in una sola transazione.
///
/// `scheduled_reset` è il reset programmato che si sta recuperando (cicli automatici): i punti dei
/// vincitori appartengono alla settimana appena chiusa, quindi si attribuiscono a 3 giorni e mezzo
/// prima, e non alla stagione in cui cade il lunedì del reset. Con `None` (reset manuale) vale `now`.
pub fn reset_week(db: &Database, scheduled_reset: Option<DateTime<Tz>>, now: DateTime<Tz>) -> DbResult<ResetOutcome> {
    let reset_at = match scheduled_reset {
        Some(at) => at,
        None => last_scheduled_reset(db, now)?,
    };
    let awarded_at = scheduled_reset.map_or(now, |at| shift_wall_clock(at, -(ChronoDuration::days(3) + ChronoDuration::hours(12))));
    let closing_time = poll_closing_time(db, now)?;
    let week = db.current_week()?;
    db.transaction(|tx| -> DbResult<ResetOutcome> {
        let winner = record_weekly_winner_stats(tx, week, now, Some(awarded_at))?;
        let (contributors, points) = record_weekly_contributor(tx, week, now)?;
        finalize_week_streaks(tx, week)?;
        update_community_records(tx, week)?;
        let winning: Vec<(i64, String)> = tx
            .prepare("SELECT user_id, name FROM user_winning_proposals WHERE week=? ORDER BY user_id, name")?
            .query_map([week], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<DbResult<_>>()?;
        let mut winning_by_user: Vec<(i64, Vec<String>)> = Vec::new();
        for (user_id, name) in winning {
            match winning_by_user.last_mut() {
                Some((last, names)) if *last == user_id => names.push(name),
                _ => winning_by_user.push((user_id, vec![name])),
            }
        }
        snapshot_week(tx, week, now)?;
        tx.execute("DELETE FROM votes", [])?;
        tx.execute("DELETE FROM options WHERE poll_id IN (SELECT id FROM polls WHERE is_community=1)", [])?;
        tx.execute("DELETE FROM proposals WHERE week<=?", [week])?;
        tx.execute(
            "INSERT OR IGNORE INTO picked_suggestions
             (original_suggestion_id, week, norm_name, name, tipo, link, picked_by, picked_at)
             SELECT id, week, norm_name, name, tipo, link, picked_by, picked_at
             FROM suggestions WHERE week<=? AND status='picked'",
            [week],
        )?;
        tx.execute("DELETE FROM suggestions WHERE week<=?", [week])?;
        tx.execute(
            "INSERT INTO settings(key,value) VALUES('week',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [(week + 1).to_string()],
        )?;
        tx.execute(
            "INSERT INTO settings(key,value) VALUES('last_reset',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [isoformat(reset_at)],
        )?;
        reopen_polls_for_new_week(tx, closing_time)?;
        tx.execute(
            "DELETE FROM history WHERE id NOT IN (SELECT id FROM history ORDER BY id DESC LIMIT ?)",
            [HISTORY_RETENTION as i64],
        )?;
        tx.execute("DELETE FROM community_activity WHERE occurred_at<?", [now.timestamp() - 400 * 86400])?;
        tx.execute("DELETE FROM web_sessions WHERE expires_at<?", [isoformat(now)])?;
        Ok(ResetOutcome { week, new_week: week + 1, winner, contributors, points, winning_by_user })
    })
}

// --- promemoria ------------------------------------------------------------------------------

/// Embed del promemoria "Ultime 24 ore per votare!" con la classifica parziale.
pub fn reminder_embed(db: &Database, next_reset: DateTime<Tz>) -> DbResult<EmbedSpec> {
    let community: Option<i64> = db.with_conn(|connection| {
        use rusqlite::OptionalExtension;
        connection.query_row("SELECT id FROM polls WHERE is_community=1 AND active=1", [], |row| row.get(0)).optional()
    })?;
    let mut lines = Vec::new();
    if let Some(poll_id) = community {
        let mut options = poll_options(db, poll_id)?;
        options.sort_by_key(|option| (-option.n, casefold(&option.label)));
        lines = options
            .iter()
            .take(3)
            .enumerate()
            .map(|(index, option)| format!("{} **{}** · `{} voti`", MEDALS[index], option.label, option.n))
            .collect();
    }
    let voters: i64 = db.with_conn(|connection| connection.query_row("SELECT COUNT(DISTINCT user_id) FROM votes", [], |row| row.get(0)))?;
    let mut description = format!(
        "La settimana si chiude {}. Finora hanno votato `{voters}` persone: hai ancora voti da usare? Premi **🗳️ Vota** nella dashboard.",
        discord_timestamp_for(next_reset.timestamp())
    );
    if !lines.is_empty() {
        description.push_str("\n\n**Classifica parziale**\n");
        description.push_str(&lines.join("\n"));
    }
    Ok(EmbedSpec::new("⏰ Ultime 24 ore per votare!", description, COLOR_PRIMARY))
}

/// Link al messaggio della dashboard.
pub fn jump_url(guild_id: u64, channel_id: u64, message_id: u64) -> String {
    format!("https://discord.com/channels/{guild_id}/{channel_id}/{message_id}")
}

// --- servizio --------------------------------------------------------------------------------

/// Tutto ciò che segue al reset su Discord e i cicli periodici.
pub struct ResetService<S: DiscordSender, A: DiscordApi> {
    pub services: Services<S, A>,
    /// ID del server (per il link alla dashboard nel promemoria).
    pub guild_id: Option<u64>,
    clock: fn() -> DateTime<Tz>,
}

impl<S: DiscordSender, A: DiscordApi> ResetService<S, A> {
    pub fn new(services: Services<S, A>, guild_id: Option<u64>) -> Self {
        Self::with_clock(services, guild_id, schedule::now)
    }

    pub fn with_clock(services: Services<S, A>, guild_id: Option<u64>, clock: fn() -> DateTime<Tz>) -> Self {
        Self { services, guild_id, clock }
    }

    fn db(&self) -> &Database {
        &self.services.db
    }

    /// Esegue il reset: database, poi messaggi, riepilogo, notifiche e log staff.
    ///
    /// Un errore del database lo restituisce (nulla è cambiato); un errore di Discord dopo il reset
    /// si registra nei log senza annullare nulla.
    pub async fn do_reset(&self, reason: &str, scheduled_reset: Option<DateTime<Tz>>) -> DbResult<ResetOutcome> {
        let outcome = reset_week(self.db(), scheduled_reset, (self.clock)())?;
        if let Err(e) = self.services.publisher.refresh_all().await {
            error!("Reset completato nel database, ma aggiornare i sondaggi è fallito: {e}");
        }
        publish_site_change("polls");
        publish_site_change("proposals");
        self.publish_weekly_recap(outcome.week, outcome.winner.as_ref()).await;
        // Ricorda il vincitore e propone allo staff di creare l'evento (messaggio con pulsante).
        offer_event_creation(self.db(), self.services.publisher.api(), outcome.week, outcome.winner.as_ref()).await;
        let notifier = &self.services.notifier;
        if !outcome.contributors.is_empty() {
            let mentions = outcome.contributors.iter().map(|id| format!("<@{id}>")).collect::<Vec<_>>().join(", ");
            notifier.publish_activity(format!("🌟 **Contributor della settimana**\n{mentions} · **+{} punti**", outcome.points), None).await;
            let ids: Vec<u64> = outcome.contributors.iter().filter_map(|id| u64::try_from(*id).ok()).collect();
            notifier
                .send_personal_notifications(
                    &ids,
                    &format!("🌟 Sei contributor della settimana #{} con {} punti! Complimenti.", outcome.week, outcome.points),
                )
                .await;
        }
        for (user_id, names) in &outcome.winning_by_user {
            let proposal_names = names.iter().map(|name| format!("**{name}**")).collect::<Vec<_>>().join(", ");
            if let Ok(id) = u64::try_from(*user_id) {
                notifier
                    .send_personal_notifications(&[id], &format!("🏆 La tua proposta {proposal_names} ha vinto nella settimana #{}!", outcome.week))
                    .await;
            }
        }
        let current_week = database::current_week(self.db()).unwrap_or(outcome.new_week);
        notifier.staff_log(format!("🔄 Reset settimanale eseguito ({reason}). Nuova settimana: {current_week}."));
        Ok(outcome)
    }

    /// Reset manuale confermato da un amministratore (`ConfirmReset` di Python): esegue il reset e
    /// registra l'azione nell'audit. La conferma "sei sicuro?" è di chi invoca (`ui::confirm_view`).
    pub async fn manual_reset(&self, actor_id: i64, actor_name: &str) -> DbResult<ResetOutcome> {
        let outcome = self.do_reset(&format!("manuale da {actor_name}"), None).await?;
        let week = database::current_week(self.db())?;
        self.services.notifier.record_staff_audit(
            AuditEntry::new(u64::try_from(actor_id).ok(), "weekly_reset_forced")
                .entity("weekly_reset", week)
                .reason(Some("Avvio manuale confermato")),
        );
        Ok(outcome)
    }

    /// Pubblica il riepilogo della settimana nel canale sondaggi.
    pub async fn publish_weekly_recap(&self, week: i64, winner: Option<&Winner>) {
        let Some(channel) = self.poll_channel() else {
            warn!("Riepilogo settimana {week} non pubblicato: canale sondaggi non configurato.");
            return;
        };
        let embed = match build_weekly_recap_embed(self.db(), week, winner, (self.clock)()) {
            Ok(spec) => spec.build(),
            Err(e) => {
                error!("Impossibile costruire il riepilogo della settimana {week}: {e}");
                return;
            }
        };
        if let Err(e) = self.services.publisher.api().send_message(channel, MessagePayload::embed(embed)).await {
            error!("Impossibile pubblicare il riepilogo della settimana {week} nel canale {channel}: {e}");
        }
    }

    fn poll_channel(&self) -> Option<u64> {
        self.db().get_setting("poll_channel").ok().flatten().and_then(|raw| raw.trim().parse::<u64>().ok()).filter(|id| *id != 0)
    }

    /// Un passo del ciclo del reset (ogni minuto): al primo avvio memorizza il reset corrente senza
    /// eseguirlo; se un reset programmato è passato (anche per un riavvio in ritardo) lo esegue.
    pub async fn reset_tick(&self) {
        if let Err(e) = self.try_reset_tick().await {
            error!("Errore nel ciclo di reset settimanale; verrà ritentato al prossimo controllo: {e}");
        }
    }

    async fn try_reset_tick(&self) -> DbResult<()> {
        let now = (self.clock)();
        let target = last_scheduled_reset(self.db(), now)?;
        if self.db().get_setting("last_reset")?.is_none() {
            self.db().set_setting("last_reset", isoformat(target))?; // primo avvio: niente reset
            return Ok(());
        }
        if weekly_reset_pending(self.db(), now)? {
            self.do_reset("automatico", Some(target)).await?;
        }
        Ok(())
    }

    /// Promemoria pubblico 24 ore prima del reset, con la classifica parziale.
    /// Restituisce `true` se è stato inviato.
    pub async fn send_reset_reminder(&self, now: DateTime<Tz>) -> DbResult<bool> {
        let db = self.db();
        if !settings::reminder_enabled(db)? || weekly_reset_pending(db, now)? {
            return Ok(false);
        }
        let next_reset = next_scheduled_reset(db, now)?;
        if now < reminder_time(db, now)? {
            return Ok(false);
        }
        let key = isoformat(next_reset);
        if db.get_setting("last_reset_reminder")?.as_deref() == Some(key.as_str()) {
            return Ok(false);
        }
        let Some(channel) = self.poll_channel() else {
            return Ok(false);
        };
        let mut payload = MessagePayload::embed(reminder_embed(db, next_reset)?.build());
        let dashboard = db.get_setting("dashboard_message")?.and_then(|raw| raw.trim().parse::<u64>().ok());
        let same_channel = db.get_setting("dashboard_channel")?.as_deref() == Some(channel.to_string().as_str());
        if let (Some(message), Some(guild), true) = (dashboard, self.guild_id, same_channel) {
            let button = CreateButton::new_link(jump_url(guild, channel, message)).label("Vai alla dashboard").emoji(ReactionType::Unicode("🎮".into()));
            payload = payload.with_components(vec![CreateActionRow::Buttons(vec![button])]);
        }
        match self.services.publisher.api().send_message(channel, payload).await {
            Ok(_) => {}
            Err(e @ (ApiError::Forbidden | ApiError::NotFound | ApiError::Http(_))) => {
                error!("Promemoria del reset non pubblicato nel canale {channel}: {e}");
                return Ok(false);
            }
        }
        db.set_setting("last_reset_reminder", key)?;
        Ok(true)
    }

    /// Un passo del ciclo del promemoria (ogni 5 minuti).
    pub async fn reminder_tick(&self) {
        if let Err(e) = self.send_reset_reminder((self.clock)()).await {
            error!("Errore nel promemoria del reset; riprovo al prossimo controllo: {e}");
        }
    }

    /// Avvia i due cicli periodici: il reset ogni minuto e il promemoria ogni 5 minuti (il primo
    /// passo parte subito, come `tasks.loop` di discord.py).
    pub fn start_loops(self: &Arc<Self>) -> [tokio::task::JoinHandle<()>; 2] {
        let reset = Arc::clone(self);
        let reminder = Arc::clone(self);
        [
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(60));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    reset.reset_tick().await;
                }
            }),
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(300));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    reminder.reminder_tick().await;
                }
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::discord_api::testing::{Call, FakeApi, StoredMessage};
    use crate::gamenight::remember_winner;
    use crate::notify::Notifier;
    use crate::notify::testing::{FakeSender, Sent};
    use crate::polls::RefreshScheduler;
    use crate::publisher::Publisher;
    use chrono::TimeZone;

    /// Lunedì 2027-03-01, 30 secondi dopo il reset delle 12:00 (a cavallo dell'inizio della primavera).
    fn frozen() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2027, 3, 1, 12, 0, 30).unwrap()
    }

    fn setup_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()).unwrap();
        db.execute_batch(include_str!("golden/cards_fixture.sql")).unwrap();
        db.execute_batch(include_str!("golden/reset_extras.sql")).unwrap();
        db
    }

    /// Colonne di una tabella come JSON, nello stesso formato degli script Python.
    fn table(db: &Database, name: &str, columns: &str, order: &str) -> Value {
        let count = columns.split(',').count();
        db.with_conn(|connection| {
            let mut statement = connection.prepare(&format!("SELECT {columns} FROM {name} ORDER BY {order}")).unwrap();
            let rows: Vec<Value> = statement
                .query_map([], |row| {
                    let mut values = Vec::new();
                    for index in 0..count {
                        values.push(match row.get_ref(index)? {
                            rusqlite::types::ValueRef::Null => Value::Null,
                            rusqlite::types::ValueRef::Integer(n) => json!(n),
                            rusqlite::types::ValueRef::Real(f) => json!(f),
                            rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                            rusqlite::types::ValueRef::Blob(_) => Value::Null,
                        });
                    }
                    Ok(Value::Array(values))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            Value::Array(rows)
        })
    }

    /// Le stesse tabelle e colonne di `golden_reset.py`.
    fn state(db: &Database) -> Value {
        const TABLES: &[(&str, &str, &str)] = &[
            ("settings", "key,value", "key"),
            ("polls", "id,name,active,closes_at,is_community", "id"),
            ("options", "id,poll_id,label,suggestion_id", "id"),
            ("votes", "option_id,user_id", "option_id,user_id"),
            ("suggestions", "id,week,name,status", "id"),
            ("proposals", "id,suggestion_id,user_id,week", "id"),
            ("suggestion_supports", "suggestion_id,user_id,week", "suggestion_id,user_id"),
            ("picked_suggestions", "original_suggestion_id,week,norm_name,name,tipo,link,picked_by,picked_at", "original_suggestion_id"),
            ("history", "id,week,closed_at,data", "id"),
            ("user_stats", "user_id,proposals_submitted,supports_given,supports_received,ballot_entries,winning_proposals", "user_id"),
            ("weekly_activity", "user_id,week,activity_count,points", "user_id,week"),
            ("season_activity", "season_key,user_id,week,activity_count,points", "season_key,user_id,week"),
            ("user_streaks", "user_id,current_streak,best_streak,last_active_week", "user_id"),
            ("weekly_streak_records", "user_id,week,streak", "user_id,week"),
            ("community_records", "record_key,metric,label,week,user_id", "record_key"),
            ("weekly_awards", "week,user_ids,points,recorded_at", "week"),
            ("user_winning_proposals", "user_id,week,suggestion_id,name,votes", "user_id,week"),
            ("user_ballot_weeks", "user_id,week", "user_id,week"),
            ("user_hidden_badges", "user_id,badge_key,unlocked_at", "user_id,badge_key"),
            ("community_activity", "action,occurred_at", "occurred_at,action"),
            ("web_sessions", "session_id,user_id", "session_id"),
            ("staff_audit", "actor_id,action,entity_type,entity_id,reason", "id"),
        ];
        let mut map = serde_json::Map::new();
        for (name, columns, order) in TABLES {
            map.insert((*name).to_string(), table(db, name, columns, order));
        }
        Value::Object(map)
    }

    fn assert_same_state(actual: &Value, golden: &str) {
        let expected: Value = serde_json::from_str(golden).unwrap();
        for (name, rows) in expected.as_object().unwrap() {
            assert_eq!(&actual[name], rows, "tabella «{name}» diversa da Python");
        }
        assert_eq!(actual.as_object().unwrap().len(), expected.as_object().unwrap().len());
    }

    #[test]
    fn automatic_reset_matches_the_python_state() {
        let db = setup_db();
        let target = last_scheduled_reset(&db, frozen()).unwrap();
        assert_eq!(isoformat(target), "2027-03-01T12:00:00+01:00");
        let outcome = reset_week(&db, Some(target), frozen()).unwrap();
        assert_eq!((outcome.week, outcome.new_week), (7, 8));
        remember_winner(&db, outcome.week, outcome.winner.as_ref().expect("c'è un vincitore")).unwrap();
        assert_same_state(&state(&db), include_str!("golden/reset_auto_expected.json"));
    }

    /// Il reset manuale assegna i punti alla stagione di adesso, non a quella della settimana chiusa.
    #[test]
    fn manual_reset_matches_the_python_state() {
        let db = setup_db();
        let outcome = reset_week(&db, None, frozen()).unwrap();
        remember_winner(&db, outcome.week, outcome.winner.as_ref().expect("c'è un vincitore")).unwrap();
        assert_same_state(&state(&db), include_str!("golden/reset_manual_expected.json"));
    }

    #[test]
    fn a_failing_reset_changes_nothing() {
        let db = setup_db();
        // Una tabella mancante a metà transazione: il reset intero deve annullarsi.
        db.execute("DROP TABLE web_sessions", []).unwrap();
        let snapshot = |db: &Database| {
            json!([
                table(db, "settings", "key,value", "key"),
                table(db, "votes", "option_id,user_id", "option_id,user_id"),
                table(db, "suggestions", "id,week,name,status", "id"),
                table(db, "history", "id,week,closed_at,data", "id"),
                table(db, "user_stats", "user_id,winning_proposals", "user_id"),
            ])
        };
        let before = snapshot(&db);
        assert!(reset_week(&db, None, frozen()).is_err());
        assert_eq!(snapshot(&db), before);
        assert_eq!(db.current_week().unwrap(), 7);
    }

    // --- servizio ---------------------------------------------------------------------------

    struct Fixture {
        service: Arc<ResetService<FakeSender, FakeApi>>,
        sender: FakeSender,
        api: FakeApi,
        db: Database,
    }

    fn fixture_with(db: Database) -> Fixture {
        let sender = FakeSender::default();
        let api = FakeApi::default();
        let notifier = Notifier::with_audit_channel(db.clone(), sender.clone(), None);
        let publisher = Arc::new(Publisher::with_clock(api.clone(), db.clone(), frozen));
        let services = Services { db: db.clone(), notifier, publisher, scheduler: RefreshScheduler::new() };
        Fixture { service: Arc::new(ResetService::with_clock(services, Some(4242), frozen)), sender, api, db }
    }

    fn sent_messages(f: &Fixture, channel: u64) -> Vec<StoredMessage> {
        f.api
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Send { channel: c, message } if c == channel => f.api.message(c, message),
                _ => None,
            })
            .collect()
    }

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2027, 3, day, hour, minute, 0).unwrap()
    }

    fn feb(day: u32, hour: u32, minute: u32) -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2027, 2, day, hour, minute, 0).unwrap()
    }

    /// Stessi passi di `golden_reminder.py`, con un canale finto: embed, pulsante e chiave di
    /// deduplicazione devono coincidere con quelli del vero codice Python.
    #[tokio::test]
    async fn reminder_matches_the_python_implementation() {
        let f = fixture_with(setup_db());
        let golden: Vec<Value> = serde_json::from_str(include_str!("golden/reminder_expected.json")).unwrap();
        let mut cases = golden.iter();
        let db = &f.db;

        let mut step = async |label: &str, when: DateTime<Tz>| {
            let case = cases.next().unwrap();
            assert_eq!(case[0], label, "ordine dei casi");
            let before = sent_messages(&f, 500).len();
            let result = f.service.send_reset_reminder(when).await.unwrap();
            assert_eq!(json!(result), case[1], "{label}: esito");
            let sent = sent_messages(&f, 500);
            let expected = case[2].as_array().unwrap();
            assert_eq!(sent.len() - before, expected.len(), "{label}: messaggi inviati");
            for (message, expected) in sent[before..].iter().zip(expected) {
                let embed = &message.embeds[0];
                let want = &expected["embed"];
                assert_eq!(embed["title"], want["title"], "{label}: titolo");
                assert_eq!(embed["description"], want["description"], "{label}: descrizione");
                assert_eq!(embed["color"], want["color"], "{label}: colore");
                assert!(embed.get("fields").is_none_or(|fields| fields.as_array().unwrap().is_empty()));
                assert_eq!(embed["footer"]["text"].as_str(), want["footer"].as_str());
                assert_eq!(embed.get("timestamp").is_some(), want["has_timestamp"].as_bool().unwrap());
                let buttons: Vec<Value> = message
                    .components
                    .iter()
                    .flat_map(|row| row["components"].as_array().unwrap().clone())
                    .map(|b| json!([b["label"], b["emoji"]["name"], b["url"]]))
                    .collect();
                assert_eq!(json!(buttons), expected["buttons"], "{label}: pulsanti");
            }
            assert_eq!(db.get_setting("last_reset_reminder").unwrap().map_or(Value::Null, Value::from), case[3], "{label}: chiave");
        };

        db.set_setting("poll_channel", 500).unwrap();
        step("troppo_presto", feb(28, 11, 59)).await;
        db.set_setting("reset_reminder_enabled", "0").unwrap();
        step("disattivato", feb(28, 12, 30)).await;
        db.set_setting("reset_reminder_enabled", "1").unwrap();
        db.execute("DELETE FROM settings WHERE key='poll_channel'", []).unwrap();
        step("senza_canale", feb(28, 12, 30)).await;
        db.set_setting("poll_channel", 500).unwrap();
        step("senza_dashboard", feb(28, 12, 30)).await;
        db.execute("DELETE FROM settings WHERE key='last_reset_reminder'", []).unwrap();
        db.set_setting("dashboard_message", 900).unwrap();
        db.set_setting("dashboard_channel", 500).unwrap();
        step("con_dashboard", feb(28, 12, 30)).await;
        step("gia_inviato", feb(28, 13, 0)).await;
        db.execute("DELETE FROM settings WHERE key='last_reset_reminder'", []).unwrap();
        db.set_setting("dashboard_channel", 501).unwrap();
        step("dashboard_altro_canale", feb(28, 18, 0)).await;
        db.execute("DELETE FROM settings WHERE key='last_reset_reminder'", []).unwrap();
        f.api.fail_send(Some(ApiError::Forbidden));
        step("canale_negato", feb(28, 18, 0)).await;
        f.api.fail_send(None);
        step("reset_in_attesa", at(1, 12, 30)).await;
        db.execute("DELETE FROM votes", []).unwrap();
        db.execute("DELETE FROM options WHERE poll_id=5", []).unwrap();
        db.set_setting("last_reset", "2027-03-01T12:00:00+01:00").unwrap();
        step("senza_classifica", at(7, 13, 0)).await;
        assert!(cases.next().is_none(), "tutti i casi Python sono stati verificati");
    }

    fn configured() -> Fixture {
        let db = setup_db();
        db.set_setting("poll_channel", 500).unwrap();
        db.set_setting("staff_channel", 600).unwrap();
        db.set_setting("activity_channel", 700).unwrap();
        db.set_setting("activity_feed_enabled", "1").unwrap();
        fixture_with(db)
    }

    #[tokio::test(start_paused = true)]
    async fn do_reset_closes_the_week_and_announces_it() {
        let f = configured();
        let target = last_scheduled_reset(&f.db, frozen()).unwrap();
        f.db.execute("INSERT INTO user_notification_preferences(user_id,dm_enabled) VALUES (1,1)", []).unwrap();
        let outcome = f.service.do_reset("automatico", Some(target)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await; // log staff raggruppato

        assert_eq!(f.db.current_week().unwrap(), 8);
        assert_eq!(
            f.db.get_setting("week_winner:7").unwrap().as_deref(),
            Some(r#"{"suggestion_id": 1, "name": "Among Us", "votes": 3, "tie_break": null}"#)
        );
        // Il riepilogo è uguale a quello costruito dallo storico appena scritto.
        let recap = build_weekly_recap_embed(&f.db, 7, outcome.winner.as_ref(), frozen()).unwrap().build();
        let recap = serde_json::to_value(recap).unwrap();
        assert!(sent_messages(&f, 500).iter().any(|m| m.embeds.first() == Some(&recap)), "riepilogo nel canale sondaggi");
        // Log staff e attività.
        let staff: Vec<String> = f
            .sender
            .sent()
            .into_iter()
            .filter_map(|s| match s {
                Sent::Channel { channel_id: 600, content, .. } => Some(content),
                _ => None,
            })
            .collect();
        assert!(staff.iter().any(|text| text.contains("Reset settimanale eseguito (automatico). Nuova settimana: 8.")), "{staff:?}");
        let activity = f
            .sender
            .sent()
            .into_iter()
            .any(|s| matches!(s, Sent::Channel { channel_id: 700, content, .. } if content.contains("Contributor della settimana")));
        assert_eq!(activity, !outcome.contributors.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn do_reset_without_channels_still_resets() {
        let f = fixture_with(setup_db());
        f.service.do_reset("automatico", None).await.unwrap();
        assert_eq!(f.db.current_week().unwrap(), 8);
        assert!(f.api.calls().is_empty(), "nessun canale configurato: nessun messaggio");
    }

    #[tokio::test(start_paused = true)]
    async fn manual_reset_is_audited() {
        let f = configured();
        f.service.manual_reset(99, "Tester").await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let audit: (Option<i64>, String, String, Option<String>, Option<String>) = f.db.with_conn(|c| {
            c.query_row(
                "SELECT actor_id,action,entity_type,entity_id,reason FROM staff_audit WHERE action='weekly_reset_forced'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap()
        });
        assert_eq!(audit, (Some(99), "weekly_reset_forced".into(), "weekly_reset".into(), Some("8".into()), Some("Avvio manuale confermato".into())));
        let logged = f
            .sender
            .sent()
            .into_iter()
            .any(|s| matches!(s, Sent::Channel { channel_id: 600, content, .. } if content.contains("(manuale da Tester)")));
        assert!(logged);
    }

    #[tokio::test(start_paused = true)]
    async fn reset_tick_follows_the_python_loop() {
        // Primo avvio (nessun last_reset): si memorizza il reset corrente senza eseguirlo.
        let f = fixture_with(setup_db());
        f.db.execute("DELETE FROM settings WHERE key='last_reset'", []).unwrap();
        f.service.reset_tick().await;
        assert_eq!(f.db.current_week().unwrap(), 7);
        assert_eq!(f.db.get_setting("last_reset").unwrap().as_deref(), Some("2027-03-01T12:00:00+01:00"));
        // Già aggiornato: niente da fare.
        f.service.reset_tick().await;
        assert_eq!(f.db.current_week().unwrap(), 7);
        // Reset programmato passato (anche per un riavvio in ritardo): si esegue una sola volta.
        f.db.set_setting("last_reset", "2027-02-22T12:00:00+01:00").unwrap();
        f.service.reset_tick().await;
        assert_eq!(f.db.current_week().unwrap(), 8);
        assert_eq!(f.db.get_setting("last_reset").unwrap().as_deref(), Some("2027-03-01T12:00:00+01:00"));
        f.service.reset_tick().await;
        assert_eq!(f.db.current_week().unwrap(), 8);
    }

    #[test]
    fn closed_polls_reopen_unless_the_staff_closed_them() {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, frozen()).unwrap();
        db.execute("UPDATE polls SET active=0, closes_at=1000 WHERE id IN (1,5)", []).unwrap(); // scaduti
        db.execute("UPDATE polls SET active=0, closes_at=NULL WHERE id=2", []).unwrap(); // chiuso a mano
        db.with_conn(|c| reopen_polls_for_new_week(c, 2000.0).unwrap());
        let polls = table(&db, "polls", "id,active,closes_at", "id");
        let polls = polls.as_array().unwrap();
        assert_eq!(polls[0], json!([1, 1, 2000.0]));
        assert_eq!(polls[1], json!([2, 0, null]));
        assert_eq!(polls[4], json!([5, 1, 2000.0]));
    }

    #[test]
    fn expired_community_poll_stays_closed_when_another_is_active() {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, frozen()).unwrap();
        db.execute(
            "INSERT INTO polls(id,name,position,active,closes_at,is_community) VALUES (50,'Seconda',9,0,1000,1)",
            [],
        )
        .unwrap();
        db.with_conn(|c| reopen_polls_for_new_week(c, 2000.0).unwrap());
        let polls = table(&db, "polls", "id,active,closes_at", "id");
        assert_eq!(polls.as_array().unwrap().last().unwrap(), &json!([50, 0, null]));
    }
}
