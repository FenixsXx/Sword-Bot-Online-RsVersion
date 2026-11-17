//! Flussi che uniscono database, notifiche e aggiornamento dei messaggi: scelta di una proposta per
//! il ballottaggio e sostegno (`pick_suggestion`, `support_suggestion` di `sfo/dashboard.py`).
//!
//! Le funzioni non conoscono l'interazione Discord: la risposta all'utente (visibile solo a lui) si
//! consegna a `reply`, che viene chiamata nel momento giusto della sequenza (prima di audit,
//! aggiornamenti e notifiche, come in Python dove `followup.send` precede il resto).
// Verrà usato dai pulsanti, dal pannello staff e dai comandi `/scegli` e `/supporta`.
#![allow(dead_code)]

use std::sync::Arc;

use chrono::DateTime;
use chrono_tz::Tz;
use rusqlite::params;
use serde_json::{Map, Value, json};
use tracing::error;

use crate::database::{self, Database};
use crate::discord_api::DiscordApi;
use crate::notify::{AuditEntry, DiscordSender, Notifier};
use crate::polls::RefreshScheduler;
use crate::publisher::Publisher;
use crate::suggestions::{pick_suggestion_in_db, register_suggestion_support};

/// Tutto ciò che serve ai flussi.
pub struct Services<S: DiscordSender, A: DiscordApi> {
    pub db: Database,
    pub notifier: Notifier<S>,
    pub publisher: Arc<Publisher<A>>,
    pub scheduler: RefreshScheduler,
}

impl<S: DiscordSender, A: DiscordApi> Services<S, A> {
    /// Gli autori distinti di una proposta in una settimana.
    fn authors(&self, suggestion_id: i64, week: i64) -> Vec<u64> {
        let rows: rusqlite::Result<Vec<i64>> = self.db.with_conn(|connection| {
            connection
                .prepare("SELECT DISTINCT user_id FROM proposals WHERE suggestion_id=? AND week=?")?
                .query_map([suggestion_id, week], |row| row.get(0))?
                .collect()
        });
        match rows {
            Ok(ids) => ids.into_iter().filter_map(|id| u64::try_from(id).ok()).collect(),
            Err(e) => {
                error!("Impossibile leggere gli autori della proposta {suggestion_id}: {e}");
                Vec::new()
            }
        }
    }

    fn setting_is_set(&self, key: &str) -> bool {
        self.db.get_setting(key).ok().flatten().is_some_and(|value| !value.is_empty())
    }

    /// Inserisce una proposta approvata nel ballottaggio community (`/scegli` e pannello staff).
    ///
    /// `source` è `Some("staff_panel")` quando arriva dal pannello staff.
    pub async fn pick_suggestion(
        &self,
        user_id: i64,
        suggestion_id: i64,
        reason: Option<&str>,
        source: Option<&str>,
        now: DateTime<Tz>,
        reply: impl AsyncFnOnce(String),
    ) {
        if !self.setting_is_set("poll_channel") || !self.setting_is_set("staff_channel") {
            reply("⚠️ Prima configura i canali con /config.".into()).await;
            return;
        }
        let outcome = match pick_suggestion_in_db(&self.db, suggestion_id, user_id, now) {
            Ok(outcome) => outcome,
            Err(e) => {
                error!("Errore SQLite durante la selezione di una proposta: {e}");
                reply("❌ Non è stato possibile scegliere la proposta. Riprova tra poco.".into()).await;
                return;
            }
        };
        if let Some(error_message) = outcome.error {
            reply(error_message).await;
            return;
        }
        let (Some(suggestion), Some(community_id)) = (outcome.suggestion, outcome.poll_id) else {
            reply("❌ Proposta o sondaggio community non disponibili.".into()).await;
            return;
        };
        let note = outcome
            .prior
            .map(|prior| match prior.picked_at.as_deref() {
                Some(picked_at) => format!("\n⚠️ Già scelta in passato ({}).", picked_at.chars().take(10).collect::<String>()),
                None => "\n⚠️ Già scelta in passato.".to_string(),
            })
            .unwrap_or_default();
        reply(format!("✅ **{}** è stata aggiunta al ballottaggio.{note}", suggestion.name)).await;

        let mut details = Map::new();
        details.insert("name".into(), json!(suggestion.name));
        details.insert("week".into(), json!(suggestion.week));
        if let Some(source) = source.filter(|source| !source.is_empty()) {
            details.insert("source".into(), Value::String(source.to_string()));
        }
        self.notifier.record_staff_audit(
            AuditEntry::new(u64::try_from(user_id).ok(), "proposal_selected")
                .entity("suggestion", suggestion_id)
                .reason(reason)
                .details(Value::Object(details)),
        );
        // Un errore di Discord qui non annulla la scelta, già salvata.
        let refreshed = async {
            self.publisher.refresh_poll(community_id).await?;
            self.publisher.refresh_dashboard().await
        };
        if let Err(e) = refreshed.await {
            error!("Scelta salvata, ma l'aggiornamento dei messaggi Discord è fallito: {e}");
        }
        self.notifier
            .publish_activity(format!("🏆 **{}** è entrata nel ballottaggio community!", suggestion.name), None)
            .await;
        self.notifier
            .send_personal_notifications(
                &self.authors(suggestion_id, suggestion.week),
                &format!("🏆 La tua proposta **{}** è entrata nel ballottaggio community!", suggestion.name),
            )
            .await;
        let origin = if source == Some("staff_panel") { " dal pannello staff" } else { "" };
        self.notifier.staff_log(format!(
            "✅ <@{user_id}> ha scelto «{}» ({}){origin}.{note}",
            suggestion.name, suggestion.tipo
        ));
    }

    /// Registra il sostegno e gestisce traguardi e notifiche.
    pub async fn support_suggestion(
        &self,
        user_id: i64,
        suggestion_id: Option<i64>,
        now: DateTime<Tz>,
        reply: impl AsyncFnOnce(String),
    ) {
        let Some(suggestion_id) = suggestion_id else {
            reply("⚠️ Seleziona prima una proposta.".into()).await;
            return;
        };
        let result = register_suggestion_support(&self.db, user_id, suggestion_id, now);
        reply(result.message).await;
        if !result.success {
            return;
        }
        self.publisher.schedule_dashboard_refresh(&self.scheduler);
        let Some(milestone) = result.milestone else {
            return;
        };
        let name: Option<String> = self
            .db
            .with_conn(|connection| {
                connection
                    .query_row("SELECT name FROM suggestions WHERE id=?", params![suggestion_id], |row| row.get(0))
                    .ok()
            });
        let Some(name) = name else {
            return;
        };
        self.notifier.publish_activity(format!("🔥 **{name}** ha raggiunto **{milestone} sostenitori**!"), None).await;
        if milestone == 25 {
            let week = database::current_week(&self.db).unwrap_or(1);
            self.notifier
                .send_personal_notifications(
                    &self.authors(suggestion_id, week),
                    &format!("🎉 La tua proposta **{name}** ha raggiunto 25 sostenitori!"),
                )
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::discord_api::testing::{Call, FakeApi};
    use crate::notify::testing::{FakeSender, Sent};
    use crate::suggestions::register_suggestion;
    use chrono::TimeZone;
    use std::sync::Mutex;
    use std::time::Duration;

    fn now() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()
    }

    struct Fixture {
        services: Services<FakeSender, FakeApi>,
        sender: FakeSender,
        api: FakeApi,
    }

    fn fixture() -> Fixture {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, now()).unwrap();
        db.set_setting("poll_channel", 500).unwrap();
        db.set_setting("staff_channel", 600).unwrap();
        db.set_setting("activity_channel", 700).unwrap();
        db.set_setting("activity_feed_enabled", "1").unwrap();
        let sender = FakeSender::default();
        let api = FakeApi::default();
        let notifier = Notifier::with_audit_channel(db.clone(), sender.clone(), None);
        let publisher = Arc::new(Publisher::with_clock(api.clone(), db.clone(), now));
        Fixture { services: Services { db, notifier, publisher, scheduler: RefreshScheduler::new() }, sender, api }
    }

    fn approved(f: &Fixture, user: i64, name: &str) -> i64 {
        let id = register_suggestion(&f.services.db, user, name, "Gioco", None, now()).created_id.unwrap();
        f.services.db.execute("UPDATE suggestions SET review_status='approved' WHERE id=?", [id]).unwrap();
        id
    }

    /// Raccoglie le risposte con la loro posizione nella sequenza.
    fn collector() -> (Arc<Mutex<Vec<String>>>, impl AsyncFnOnce(String)) {
        let replies = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&replies);
        (replies, async move |text: String| sink.lock().unwrap().push(text))
    }

    fn channel_texts(sender: &FakeSender, channel: u64) -> Vec<String> {
        sender
            .sent()
            .into_iter()
            .filter_map(|s| match s {
                Sent::Channel { channel_id, content, .. } if channel_id == channel => Some(content),
                _ => None,
            })
            .collect()
    }

    // --- pick ------------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn pick_needs_the_poll_and_staff_channels() {
        let f = fixture();
        let id = approved(&f, 1, "Among Us");
        f.services.db.execute("DELETE FROM settings WHERE key='staff_channel'", []).unwrap();
        let (replies, reply) = collector();
        f.services.pick_suggestion(99, id, None, None, now(), reply).await;
        assert_eq!(*replies.lock().unwrap(), ["⚠️ Prima configura i canali con /config."]);
        let picked: i64 = f.services.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM suggestions WHERE status='picked'", [], |r| r.get(0)).unwrap());
        assert_eq!(picked, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn pick_runs_the_whole_follow_up() {
        let f = fixture();
        let id = approved(&f, 1, "Among Us");
        f.services.db.execute("INSERT INTO user_notification_preferences(user_id,dm_enabled) VALUES (1,1)", []).unwrap();
        let (replies, reply) = collector();
        f.services.pick_suggestion(99, id, Some("È una bomba"), None, now(), reply).await;
        tokio::time::sleep(Duration::from_secs(2)).await; // log staff raggruppato

        assert_eq!(*replies.lock().unwrap(), ["✅ **Among Us** è stata aggiunta al ballottaggio."]);
        // Audit con motivo e senza «source».
        let (action, reason, details): (String, Option<String>, String) = f.services.db.with_conn(|c| {
            c.query_row(
                "SELECT action, reason, details FROM staff_audit WHERE action='proposal_selected'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        });
        assert_eq!((action.as_str(), reason.as_deref()), ("proposal_selected", Some("È una bomba")));
        assert_eq!(details, r#"{"name": "Among Us", "week": 1}"#);
        // Messaggi: sondaggio community e dashboard aggiornati, feed attività, DM all'autore, log staff.
        assert!(f.api.calls().iter().any(|c| matches!(c, Call::Send { channel: 500, .. })), "sondaggio e dashboard pubblicati");
        assert!(f.services.db.get_setting("dashboard_message").unwrap().is_some());
        assert_eq!(channel_texts(&f.sender, 700), ["🏆 **Among Us** è entrata nel ballottaggio community!"]);
        assert!(f.sender.sent().iter().any(|s| matches!(s, Sent::Direct { user_id: 1, content } if content == "🏆 La tua proposta **Among Us** è entrata nel ballottaggio community!")));
        assert!(channel_texts(&f.sender, 600).contains(&"✅ <@99> ha scelto «Among Us» (Gioco).".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn pick_from_the_staff_panel_is_marked_and_prior_picks_are_noted() {
        let f = fixture();
        f.services.db.execute(
            "INSERT INTO picked_suggestions(original_suggestion_id,week,norm_name,name,tipo,picked_at)
             VALUES(500,0,'amongus','Among Us','Gioco','2026-01-05T12:00:00+01:00')",
            [],
        )
        .unwrap();
        let id = approved(&f, 1, "Among Us");
        let (replies, reply) = collector();
        f.services.pick_suggestion(99, id, None, Some("staff_panel"), now(), reply).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(*replies.lock().unwrap(), ["✅ **Among Us** è stata aggiunta al ballottaggio.\n⚠️ Già scelta in passato (2026-01-05)."]);
        let details: String = f.services.db.with_conn(|c| c.query_row("SELECT details FROM staff_audit WHERE action='proposal_selected'", [], |r| r.get(0)).unwrap());
        assert_eq!(details, r#"{"name": "Among Us", "week": 1, "source": "staff_panel"}"#);
        assert!(channel_texts(&f.sender, 600).contains(&"✅ <@99> ha scelto «Among Us» (Gioco) dal pannello staff.\n⚠️ Già scelta in passato (2026-01-05).".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn pick_reports_refusals_and_database_errors_without_side_effects() {
        let f = fixture();
        let pending = register_suggestion(&f.services.db, 1, "Bozza", "Gioco", None, now()).created_id.unwrap();
        let (replies, reply) = collector();
        f.services.pick_suggestion(99, pending, None, None, now(), reply).await;
        assert_eq!(*replies.lock().unwrap(), ["⚠️ La proposta deve essere approvata prima del ballottaggio."]);
        assert!(f.api.calls().is_empty() && f.sender.sent().is_empty(), "nessun effetto");

        f.services.db.execute("DROP TABLE options", []).unwrap();
        let ok = approved(&f, 2, "Altra");
        let (replies, reply) = collector();
        f.services.pick_suggestion(99, ok, None, None, now(), reply).await;
        assert_eq!(*replies.lock().unwrap(), ["❌ Non è stato possibile scegliere la proposta. Riprova tra poco."]);
    }

    #[tokio::test(start_paused = true)]
    async fn discord_failures_do_not_undo_the_pick() {
        let f = fixture();
        let id = approved(&f, 1, "Among Us");
        f.api.fail_send(Some(crate::discord_api::ApiError::Forbidden));
        f.api.fail_history(Some(crate::discord_api::ApiError::Http("boom".into())));
        let (replies, reply) = collector();
        f.services.pick_suggestion(99, id, None, None, now(), reply).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(replies.lock().unwrap().len(), 1);
        let status: String = f.services.db.with_conn(|c| c.query_row("SELECT status FROM suggestions WHERE id=?", [id], |r| r.get(0)).unwrap());
        assert_eq!(status, "picked");
        assert_eq!(channel_texts(&f.sender, 700).len(), 1, "il resto della sequenza prosegue");
    }

    // --- support ---------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn support_needs_a_selection_and_reports_refusals() {
        let f = fixture();
        let (replies, reply) = collector();
        f.services.support_suggestion(5, None, now(), reply).await;
        assert_eq!(*replies.lock().unwrap(), ["⚠️ Seleziona prima una proposta."]);

        let id = approved(&f, 1, "Among Us");
        let (replies, reply) = collector();
        f.services.support_suggestion(1, Some(id), now(), reply).await;
        assert_eq!(*replies.lock().unwrap(), ["⚠️ Hai già proposto questa attività: il tuo sostegno è già conteggiato."]);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(f.api.calls().is_empty(), "un rifiuto non aggiorna la dashboard");
    }

    #[tokio::test(start_paused = true)]
    async fn support_refreshes_the_dashboard_and_announces_milestones() {
        let f = fixture();
        let id = approved(&f, 1, "Among Us");
        f.services.db.execute("INSERT INTO user_notification_preferences(user_id,dm_enabled) VALUES (1,1)", []).unwrap();
        for user in 100..123 {
            let (replies, reply) = collector();
            f.services.support_suggestion(user, Some(id), now(), reply).await;
            assert_eq!(replies.lock().unwrap()[0], "✅ Hai supportato **Among Us**. Grazie!");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        // La dashboard è stata pubblicata una volta sola (aggiornamenti raggruppati).
        assert_eq!(f.api.calls().iter().filter(|c| matches!(c, Call::Send { channel: 500, .. })).count(), 1);
        // L'autore più 23 sostenitori = 24: traguardi a 10 e 20 (non ancora 25).
        assert_eq!(channel_texts(&f.sender, 700), ["🔥 **Among Us** ha raggiunto **10 sostenitori**!", "🔥 **Among Us** ha raggiunto **20 sostenitori**!"]);
        assert!(!f.sender.sent().iter().any(|s| matches!(s, Sent::Direct { .. })), "nessun DM prima del traguardo 25");

        let (_, reply) = collector();
        f.services.support_suggestion(200, Some(id), now(), reply).await; // il 25° sostenitore (autore incluso)
        let texts = channel_texts(&f.sender, 700);
        assert!(texts.iter().any(|t| t.contains("**25 sostenitori**")), "{texts:?}");
        assert!(f.sender.sent().iter().any(|s| matches!(s, Sent::Direct { user_id: 1, content } if content == "🎉 La tua proposta **Among Us** ha raggiunto 25 sostenitori!")));
    }
}
