//! Revisione staff delle proposte: un'unica logica per pulsanti, comandi e sito (equivalente di
//! `sfo/review.py`).
//!
//! Ogni nuova proposta riceve una scheda nel canale staff con i pulsanti ✅ Approva / ❌ Rifiuta.
//! [`apply_review`] è usata anche da `/proposta` e dal sito.
//!
//! Portato qui: la logica di revisione, la scheda (embed e pulsanti), il riconoscimento dei
//! `custom_id` e la maschera del rifiuto. **Non ancora portato**: `post_review_card`,
//! `update_review_message` e i callback dei pulsanti, che parlano con Discord (qui sono dietro il
//! trait [`ReviewEffects`]), e `schedule_dashboard_refresh` (arriva con `dashboard.py`).
// Verrà usato dai pulsanti di revisione, dai comandi staff e dal sito.
#![allow(dead_code)]

use std::future::Future;

use poise::serenity_prelude::{
    CreateActionRow, CreateEmbed, CreateEmbedFooter, CreateInputText, CreateModal, InputTextStyle,
};
use rusqlite::OptionalExtension;
use serde_json::json;
use tracing::error;

use crate::config::COLOR_STAFF;
use crate::database::{self, Connected, Database, DbResult};
use crate::formatting::{proposal_image_url, proposal_type_emoji};
use crate::hooks::publish_site_change;
use crate::notify::{AuditEntry, DiscordSender, Notifier};
use crate::suggestions::similar_suggestions;
use crate::ui::{ButtonKind, ViewButton, button_rows};

/// Etichetta leggibile di uno stato di revisione; uno stato sconosciuto resta com'è.
pub fn status_label(status: &str) -> &str {
    match status {
        "pending" => "🕓 In attesa di revisione",
        "reviewing" => "🔎 In revisione",
        "approved" => "✅ Approvata",
        "rejected" => "❌ Rifiutata",
        other => other,
    }
}

/// Una transizione di stato ammessa: da quali stati, verso quale, e come si chiama nell'audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub allowed_from: &'static [&'static str],
    pub new_status: &'static str,
    pub audit_action: &'static str,
}

/// `REVIEW_TRANSITIONS` di Python: `review`, `approve` e `reject`.
pub fn transition(action: &str) -> Option<Transition> {
    match action {
        "review" => Some(Transition { allowed_from: &["pending"], new_status: "reviewing", audit_action: "proposal_review_started" }),
        "approve" => Some(Transition { allowed_from: &["pending", "reviewing"], new_status: "approved", audit_action: "proposal_approved" }),
        "reject" => Some(Transition { allowed_from: &["pending", "reviewing"], new_status: "rejected", audit_action: "proposal_rejected" }),
        _ => None,
    }
}

/// Esito di una revisione. `status_code` segue la semantica HTTP (200, 400, 404, 409, 500) così il
/// sito può restituirlo direttamente.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutcome {
    pub ok: bool,
    pub status_code: u16,
    pub message: String,
    pub review_status: Option<String>,
    pub name: Option<String>,
}

impl ReviewOutcome {
    fn failure(status_code: u16, message: impl Into<String>) -> Self {
        Self { ok: false, status_code, message: message.into(), review_status: None, name: None }
    }
}

/// Da dove arriva la revisione (finisce nei dettagli dell'audit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSource {
    Discord,
    Site,
}

impl ReviewSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReviewSource::Discord => "discord",
            ReviewSource::Site => "site",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReviewRequest<'a> {
    pub actor_id: i64,
    pub suggestion_id: i64,
    /// `review`, `approve` o `reject`.
    pub action: &'a str,
    pub reason: Option<&'a str>,
    pub source: ReviewSource,
}

/// Gli aggiornamenti su Discord che seguono una revisione. Li fornisce chi usa il modulo: in
/// produzione l'aggiornamento della scheda nel canale staff e della dashboard.
pub trait ReviewEffects: Send + Sync {
    fn schedule_dashboard_refresh(&self);
    fn update_review_message(&self, suggestion_id: i64) -> impl Future<Output = ()> + Send;
}

/// Nessun effetto: per usare la logica senza Discord (test, import da riga di comando).
pub struct NoEffects;

impl ReviewEffects for NoEffects {
    fn schedule_dashboard_refresh(&self) {}
    async fn update_review_message(&self, _suggestion_id: i64) {}
}

enum Decision {
    NotFound,
    WrongState { current: String, name: String },
    Updated { name: String, authors: Vec<u64> },
}

/// Cambia lo stato di revisione di una proposta e fa seguire audit, notifiche e aggiornamenti.
pub async fn apply_review<S: DiscordSender, E: ReviewEffects>(
    db: &Database,
    notifier: &Notifier<S>,
    effects: &E,
    request: ReviewRequest<'_>,
) -> ReviewOutcome {
    let Some(transition) = transition(request.action) else {
        return ReviewOutcome::failure(400, "❌ Azione di revisione non valida.");
    };
    let reason = request.reason.map(str::trim).filter(|reason| !reason.is_empty());
    if request.action == "reject" && reason.is_none() {
        return ReviewOutcome::failure(400, "❌ Inserisci il motivo del rifiuto.");
    }
    let suggestion_id = request.suggestion_id;
    let decision = db.transaction(|tx| -> DbResult<Decision> {
        let week = database::current_week(tx)?;
        let found: Option<(String, String)> = tx
            .query_row(
                "SELECT name, review_status FROM suggestions WHERE id=? AND week=?",
                [suggestion_id, week],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((name, current)) = found else {
            return Ok(Decision::NotFound);
        };
        if !transition.allowed_from.contains(&current.as_str()) {
            return Ok(Decision::WrongState { current, name });
        }
        tx.execute(
            "UPDATE suggestions SET review_status=? WHERE id=? AND week=?",
            rusqlite::params![transition.new_status, suggestion_id, week],
        )?;
        let authors: Vec<i64> = tx
            .prepare("SELECT DISTINCT user_id FROM proposals WHERE suggestion_id=? AND week=?")?
            .query_map([suggestion_id, week], |row| row.get(0))?
            .collect::<DbResult<_>>()?;
        Ok(Decision::Updated { name, authors: authors.into_iter().filter_map(|id| u64::try_from(id).ok()).collect() })
    });
    let (name, authors) = match decision {
        Err(e) => {
            error!("Revisione della proposta {suggestion_id} non riuscita: {e}");
            return ReviewOutcome::failure(500, "❌ Non è stato possibile aggiornare la proposta. Riprova tra poco.");
        }
        Ok(Decision::NotFound) => return ReviewOutcome::failure(404, "❌ Proposta non trovata questa settimana."),
        Ok(Decision::WrongState { current, name }) => {
            return ReviewOutcome {
                ok: false,
                status_code: 409,
                message: format!("⚠️ Stato attuale: {}.", status_label(&current)),
                review_status: Some(current),
                name: Some(name),
            };
        }
        Ok(Decision::Updated { name, authors }) => (name, authors),
    };

    let new_status = transition.new_status;
    notifier.record_staff_audit(
        AuditEntry::new(Some(request.actor_id as u64), transition.audit_action)
            .entity("suggestion", suggestion_id)
            .reason(reason)
            .details(json!({"name": name, "review_status": new_status, "source": request.source.as_str()})),
    );
    if new_status == "approved" {
        notifier
            .send_personal_notifications(
                &authors,
                &format!("✅ La tua proposta **{name}** è stata approvata dallo staff e ora può ricevere sostegni."),
            )
            .await;
        effects.schedule_dashboard_refresh();
        if let Err(e) = notifier.announce_approved_proposal(suggestion_id).await {
            error!("Impossibile annunciare la proposta {suggestion_id}: {e}");
        }
    } else if new_status == "rejected" {
        notifier
            .send_personal_notifications(
                &authors,
                &format!("ℹ️ La tua proposta **{name}** è stata rifiutata. Motivo: {}", reason.unwrap_or_default()),
            )
            .await;
    }
    effects.update_review_message(suggestion_id).await;
    publish_site_change("proposals");
    let origin = if request.source == ReviewSource::Site { " dal sito" } else { "" };
    notifier.staff_log(format!(
        "📋 <@{}> ha aggiornato la proposta **{name}** a {}{origin}.",
        request.actor_id,
        status_label(new_status)
    ));
    ReviewOutcome {
        ok: true,
        status_code: 200,
        message: format!("{}: **{name}**.", status_label(new_status)),
        review_status: Some(new_status.to_string()),
        name: Some(name),
    }
}

// --- scheda di revisione ---------------------------------------------------------------------

/// Azione di un pulsante della scheda.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewAction {
    Approve,
    Reject,
}

impl ReviewAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReviewAction::Approve => "approve",
            ReviewAction::Reject => "reject",
        }
    }
}

pub fn review_custom_id(action: ReviewAction, suggestion_id: i64) -> String {
    format!("review:{}:{suggestion_id}", action.as_str())
}

/// Riconosce `review:approve:<ID>` / `review:reject:<ID>` (corrispondenza completa, cifre ASCII).
pub fn parse_review_custom_id(custom_id: &str) -> Option<(ReviewAction, i64)> {
    let rest = custom_id.strip_prefix("review:")?;
    let (action, id) = rest.split_once(':')?;
    let action = match action {
        "approve" => ReviewAction::Approve,
        "reject" => ReviewAction::Reject,
        _ => return None,
    };
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((action, id.parse().ok()?))
}

/// La scheda di una proposta nel canale staff.
#[derive(Debug, Clone)]
pub struct ReviewCard {
    pub embed: CreateEmbed,
    pub buttons: Vec<ViewButton>,
}

impl ReviewCard {
    pub fn components(&self) -> Vec<CreateActionRow> {
        button_rows(&self.buttons)
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Scheda di revisione per il canale staff; `None` se la proposta non esiste più.
pub fn build_review_card(db: &impl Connected, suggestion_id: i64, note: Option<&str>) -> DbResult<Option<ReviewCard>> {
    let suggestion: Option<(i64, String, String, Option<String>, String)> = db.with_conn(|connection| {
        connection
            .query_row(
                "SELECT week, name, tipo, link, review_status FROM suggestions WHERE id=?",
                [suggestion_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()
    })?;
    let Some((week, name, tipo, link, review_status)) = suggestion else {
        return Ok(None);
    };
    let proposers: Vec<i64> = db.with_conn(|connection| -> DbResult<Vec<i64>> {
        connection
            .prepare(
                "SELECT user_id FROM proposals WHERE suggestion_id=? AND week=?
                 GROUP BY user_id ORDER BY MIN(ts)",
            )?
            .query_map([suggestion_id, week], |row| row.get(0))?
            .collect()
    })?;

    let mut description = format!("**{tipo}** · {}", status_label(&review_status));
    if let Some(note) = note.filter(|note| !note.is_empty()) {
        description.push('\n');
        description.push_str(note);
    }
    let mentions = proposers.iter().take(10).map(|user_id| format!("<@{user_id}>")).collect::<Vec<_>>().join(", ");
    let mut embed = CreateEmbed::new()
        .title(truncate_chars(&format!("{} {name}", proposal_type_emoji(&tipo)), 256))
        .description(description)
        .colour(COLOR_STAFF)
        .field("Proposta da", if mentions.is_empty() { "—".to_string() } else { mentions }, true);
    if let Some(link) = link.as_deref().filter(|link| !link.is_empty()) {
        embed = embed.field("Link", format!("[Apri link]({link})"), true);
    }
    let similar = similar_suggestions(db, &name, Some(&tipo), 3, Some(suggestion_id))?;
    if !similar.is_empty() {
        let lines = similar
            .iter()
            .map(|item| format!("`#{}` **{}** · {}", item.id, item.name, status_label(&item.review_status)))
            .collect::<Vec<_>>()
            .join("\n");
        embed = embed.field(
            "Possibili duplicati",
            format!("{}\nUnisci con `/unisci-proposte`.", truncate_chars(&lines, 1024)),
            false,
        );
    }
    if let Some(image_url) = proposal_image_url(link.as_deref()) {
        embed = embed.thumbnail(image_url);
    }
    embed = embed.footer(CreateEmbedFooter::new(format!("Proposta #{suggestion_id} · settimana {week}")));

    let open_for_review = matches!(review_status.as_str(), "pending" | "reviewing");
    let buttons = [ReviewAction::Approve, ReviewAction::Reject]
        .into_iter()
        .map(|action| {
            let approve = action == ReviewAction::Approve;
            ViewButton {
                custom_id: review_custom_id(action, suggestion_id),
                label: if approve { "Approva" } else { "Rifiuta" }.to_string(),
                emoji: Some(if approve { "✅" } else { "❌" }),
                kind: if approve { ButtonKind::Success } else { ButtonKind::Danger },
                disabled: !open_for_review,
            }
        })
        .collect();
    Ok(Some(ReviewCard { embed, buttons }))
}

/// Dove si trova la scheda di una proposta nel canale staff: `(canale, messaggio)`.
pub fn review_message_location(db: &impl Connected, suggestion_id: i64) -> DbResult<Option<(String, String)>> {
    db.with_conn(|connection| {
        connection
            .query_row(
                "SELECT channel_id, message_id FROM suggestion_review_messages WHERE suggestion_id=?",
                [suggestion_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    })
}

/// Ricorda dove è stata pubblicata la scheda (una sola per proposta).
pub fn save_review_message(db: &Database, suggestion_id: i64, channel_id: u64, message_id: u64) -> DbResult<()> {
    db.transaction(|tx| -> DbResult<()> {
        tx.execute(
            "INSERT INTO suggestion_review_messages(suggestion_id, channel_id, message_id)
             VALUES(?,?,?)
             ON CONFLICT(suggestion_id) DO UPDATE SET
                 channel_id=excluded.channel_id, message_id=excluded.message_id",
            rusqlite::params![suggestion_id, channel_id.to_string(), message_id.to_string()],
        )?;
        Ok(())
    })
}

// --- maschera del rifiuto --------------------------------------------------------------------

/// Campo di testo della maschera del rifiuto.
pub const REJECT_REASON_FIELD_ID: &str = "reason";
pub const REJECT_REASON_MIN_LENGTH: u16 = 3;
pub const REJECT_REASON_MAX_LENGTH: u16 = 200;

pub fn reject_modal_custom_id(suggestion_id: i64) -> String {
    format!("review-reject:{suggestion_id}")
}

/// ID della proposta da una maschera di rifiuto inviata (`review-reject:<ID>`).
pub fn parse_reject_modal_id(custom_id: &str) -> Option<i64> {
    let id = custom_id.strip_prefix("review-reject:")?;
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then(|| id.parse().ok()).flatten()
}

/// La maschera "Rifiuta proposta" con il motivo (3-200 caratteri, inviato agli autori).
pub fn reject_modal(suggestion_id: i64) -> CreateModal {
    CreateModal::new(reject_modal_custom_id(suggestion_id), "Rifiuta proposta").components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Paragraph, "Motivo (verrà inviato agli autori)", REJECT_REASON_FIELD_ID)
                .min_length(REJECT_REASON_MIN_LENGTH)
                .max_length(REJECT_REASON_MAX_LENGTH),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::notify::testing::{FakeSender, Sent};
    use crate::suggestions::register_suggestion;
    use chrono::{DateTime, TimeZone};
    use chrono_tz::Tz;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn now() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()
    }

    struct Fixture {
        db: Database,
        notifier: Notifier<FakeSender>,
        sender: FakeSender,
    }

    fn fixture() -> Fixture {
        let db = Database::open_in_memory().unwrap();
        db.seed_defaults().unwrap();
        let sender = FakeSender::default();
        let notifier = Notifier::with_audit_channel(db.clone(), sender.clone(), None);
        Fixture { db, notifier, sender }
    }

    fn propose(f: &Fixture, user: i64, name: &str) -> i64 {
        let result = register_suggestion(&f.db, user, name, "Gioco", None, now());
        assert!(result.success, "{}", result.message);
        result.created_id.unwrap()
    }

    fn request(actor: i64, id: i64, action: &'static str, reason: Option<&'static str>) -> ReviewRequest<'static> {
        ReviewRequest { actor_id: actor, suggestion_id: id, action, reason, source: ReviewSource::Discord }
    }

    async fn review(f: &Fixture, request: ReviewRequest<'static>) -> ReviewOutcome {
        apply_review(&f.db, &f.notifier, &NoEffects, request).await
    }

    fn review_status(f: &Fixture, id: i64) -> String {
        f.db.with_conn(|c| c.query_row("SELECT review_status FROM suggestions WHERE id=?", [id], |r| r.get(0)).unwrap())
    }

    // --- apply_review ----------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn apply_review_approves_from_pending_and_blocks_double_review() {
        // Stesso caso di tests/test_features.py::test_apply_review_approves_from_pending_and_blocks_double_review.
        let f = fixture();
        let id = propose(&f, 1, "Gartic Phone");
        let outcome = review(&f, request(99, id, "approve", None)).await;
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(outcome.review_status.as_deref(), Some("approved"));
        assert_eq!(outcome.message, "✅ Approvata: **Gartic Phone**.");
        assert_eq!((outcome.status_code, outcome.name.as_deref()), (200, Some("Gartic Phone")));

        let again = review(&f, request(99, id, "reject", Some("doppione"))).await;
        assert!(!again.ok);
        assert_eq!(again.status_code, 409);
        assert_eq!(again.message, "⚠️ Stato attuale: ✅ Approvata.");
        assert_eq!(again.review_status.as_deref(), Some("approved"));
        assert_eq!(review_status(&f, id), "approved", "la seconda revisione non cambia nulla");
    }

    #[tokio::test(start_paused = true)]
    async fn reject_requires_a_reason() {
        // Stesso caso di tests/test_features.py::test_reject_requires_a_reason.
        let f = fixture();
        let id = propose(&f, 1, "Monopoly");
        let outcome = review(&f, request(99, id, "reject", Some("  "))).await;
        assert_eq!((outcome.ok, outcome.status_code), (false, 400));
        assert_eq!(outcome.message, "❌ Inserisci il motivo del rifiuto.");
        assert_eq!(review_status(&f, id), "pending");
        let rejected = review(&f, request(99, id, "reject", Some("  Troppo violento  "))).await;
        assert!(rejected.ok);
        assert_eq!(rejected.message, "❌ Rifiutata: **Monopoly**.");
        assert_eq!(review_status(&f, id), "rejected");
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_unknown_and_wrong_state_requests_have_http_like_codes() {
        let f = fixture();
        let id = propose(&f, 1, "Among Us");
        let invalid = review(&f, request(99, id, "elimina", None)).await;
        assert_eq!((invalid.status_code, invalid.message.as_str()), (400, "❌ Azione di revisione non valida."));
        let missing = review(&f, request(99, 999, "approve", None)).await;
        assert_eq!((missing.status_code, missing.message.as_str()), (404, "❌ Proposta non trovata questa settimana."));
        // review: solo da "pending" a "reviewing", una volta.
        assert!(review(&f, request(99, id, "review", None)).await.ok);
        assert_eq!(review_status(&f, id), "reviewing");
        let second = review(&f, request(99, id, "review", None)).await;
        assert_eq!((second.status_code, second.message.as_str()), (409, "⚠️ Stato attuale: 🔎 In revisione."));
        // Da "reviewing" si può approvare.
        assert!(review(&f, request(99, id, "approve", None)).await.ok);
        // Un'altra settimana la proposta non si trova più.
        f.db.set_setting("week", 2).unwrap();
        assert_eq!(review(&f, request(99, id, "approve", None)).await.status_code, 404);
    }

    #[tokio::test(start_paused = true)]
    async fn database_failure_returns_500_and_changes_nothing() {
        let f = fixture();
        let id = propose(&f, 1, "Among Us");
        f.db.execute("DROP TABLE proposals", []).unwrap();
        let outcome = review(&f, request(99, id, "approve", None)).await;
        assert_eq!((outcome.ok, outcome.status_code), (false, 500));
        assert_eq!(outcome.message, "❌ Non è stato possibile aggiornare la proposta. Riprova tra poco.");
        assert_eq!(review_status(&f, id), "pending", "transazione annullata");
    }

    #[tokio::test(start_paused = true)]
    async fn review_is_audited_with_actor_reason_and_source() {
        let f = fixture();
        let id = propose(&f, 1, "Among Us");
        let site = ReviewRequest { source: ReviewSource::Site, ..request(77, id, "reject", Some("Doppione")) };
        apply_review(&f.db, &f.notifier, &NoEffects, site).await;
        let row: (i64, String, String, String, String, String) = f.db.with_conn(|c| {
            c.query_row(
                "SELECT actor_id, action, entity_type, entity_id, reason, details FROM staff_audit",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap()
        });
        assert_eq!(
            row,
            (
                77,
                "proposal_rejected".into(),
                "suggestion".into(),
                id.to_string(),
                "Doppione".into(),
                r#"{"name": "Among Us", "review_status": "rejected", "source": "site"}"#.into()
            )
        );
        // Una revisione che fallisce non lascia tracce nell'audit.
        let before: i64 = f.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM staff_audit", [], |r| r.get(0)).unwrap());
        review(&f, request(77, id, "approve", None)).await; // 409
        let after: i64 = f.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM staff_audit", [], |r| r.get(0)).unwrap());
        assert_eq!(before, after);
    }

    #[tokio::test(start_paused = true)]
    async fn approval_notifies_authors_announces_and_logs() {
        let f = fixture();
        f.db.set_setting("staff_channel", 900).unwrap();
        f.db.set_setting("activity_channel", 55).unwrap();
        f.db.set_setting("activity_feed_enabled", "1").unwrap();
        let id = propose(&f, 1, "Among Us");
        // Un secondo autore (stessa proposta) e solo il primo ha i DM attivi.
        assert!(register_suggestion(&f.db, 2, "among us", "Gioco", None, now()).success);
        f.db.execute_batch("INSERT INTO user_notification_preferences(user_id,dm_enabled) VALUES (1,1),(2,0);").unwrap();

        review(&f, request(99, id, "approve", None)).await;
        tokio::time::sleep(Duration::from_secs(2)).await; // il log staff parte dopo il raggruppamento

        let sent = f.sender.sent();
        let direct: Vec<(u64, String)> = sent
            .iter()
            .filter_map(|s| match s {
                Sent::Direct { user_id, content } => Some((*user_id, content.clone())),
                Sent::Channel { .. } => None,
            })
            .collect();
        assert_eq!(
            direct,
            [(1, "✅ La tua proposta **Among Us** è stata approvata dallo staff e ora può ricevere sostegni.".to_string())]
        );
        let channel: Vec<(u64, String)> = sent
            .iter()
            .filter_map(|s| match s {
                Sent::Channel { channel_id, content, .. } => Some((*channel_id, content.clone())),
                Sent::Direct { .. } => None,
            })
            .collect();
        assert!(channel.iter().any(|(id, text)| *id == 55 && text.starts_with("🔥 **Nuova proposta**\n**Among Us** · Gioco\nProposta da <@1>")), "annuncio nel feed: {channel:?}");
        assert!(channel.iter().any(|(id, text)| *id == 900 && text.contains("**Audit · proposal_approved**")), "audit: {channel:?}");
        assert!(channel.iter().any(|(id, text)| *id == 900 && text == "📋 <@99> ha aggiornato la proposta **Among Us** a ✅ Approvata."), "log staff: {channel:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn rejection_sends_the_reason_and_site_reviews_say_so_in_the_log() {
        let f = fixture();
        f.db.set_setting("staff_channel", 900).unwrap();
        let id = propose(&f, 1, "Monopoly");
        f.db.execute("INSERT INTO user_notification_preferences(user_id,dm_enabled) VALUES (1,1)", []).unwrap();
        let site = ReviewRequest { source: ReviewSource::Site, ..request(99, id, "reject", Some("Troppo lungo")) };
        apply_review(&f.db, &f.notifier, &NoEffects, site).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let sent = f.sender.sent();
        assert!(sent.iter().any(|s| matches!(s, Sent::Direct { user_id: 1, content } if content == "ℹ️ La tua proposta **Monopoly** è stata rifiutata. Motivo: Troppo lungo")));
        assert!(sent.iter().any(|s| matches!(s, Sent::Channel { content, .. } if content == "📋 <@99> ha aggiornato la proposta **Monopoly** a ❌ Rifiutata dal sito.")));
        // Il motivo non viene inventato quando si rifiuta: nessun annuncio, nessuna notifica di approvazione.
        assert!(!sent.iter().any(|s| matches!(s, Sent::Channel { content, .. } if content.contains("Nuova proposta"))));
    }

    #[derive(Default)]
    struct CountingEffects {
        dashboard: AtomicUsize,
        card_updates: AtomicUsize,
    }

    impl ReviewEffects for Arc<CountingEffects> {
        fn schedule_dashboard_refresh(&self) {
            self.dashboard.fetch_add(1, Ordering::SeqCst);
        }
        async fn update_review_message(&self, _suggestion_id: i64) {
            self.card_updates.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn effects_run_on_success_only_and_the_dashboard_only_on_approval() {
        let f = fixture();
        let effects = Arc::new(CountingEffects::default());
        let id = propose(&f, 1, "Among Us");
        let other = propose(&f, 2, "Jackbox");
        apply_review(&f.db, &f.notifier, &effects, request(99, id, "approve", None)).await;
        assert_eq!((effects.dashboard.load(Ordering::SeqCst), effects.card_updates.load(Ordering::SeqCst)), (1, 1));
        apply_review(&f.db, &f.notifier, &effects, request(99, other, "reject", Some("No"))).await;
        assert_eq!((effects.dashboard.load(Ordering::SeqCst), effects.card_updates.load(Ordering::SeqCst)), (1, 2), "il rifiuto non tocca la dashboard");
        apply_review(&f.db, &f.notifier, &effects, request(99, id, "approve", None)).await; // 409
        apply_review(&f.db, &f.notifier, &effects, request(99, 999, "approve", None)).await; // 404
        assert_eq!((effects.dashboard.load(Ordering::SeqCst), effects.card_updates.load(Ordering::SeqCst)), (1, 2), "gli errori non hanno effetti");
    }

    #[tokio::test(start_paused = true)]
    async fn site_listeners_are_told_about_proposal_changes() {
        use crate::hooks::{Listener, add_site_change_listener};
        use std::sync::Mutex;
        let topics = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&topics);
        let listener: Listener = Arc::new(move |topic: &str| seen.lock().unwrap().push(topic.to_string()));
        add_site_change_listener(listener);
        let f = fixture();
        let id = propose(&f, 1, "Among Us");
        review(&f, request(99, id, "approve", None)).await;
        assert!(topics.lock().unwrap().iter().any(|topic| topic == "proposals"));
    }

    // --- scheda ----------------------------------------------------------------------------

    #[test]
    fn card_shows_type_status_proposers_link_and_footer() {
        let f = fixture();
        let result = register_suggestion(&f.db, 1, "Among Us", "Gioco", Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), now());
        let id = result.created_id.unwrap();
        register_suggestion(&f.db, 2, "among us", "Gioco", None, now());
        let card = build_review_card(&f.db, id, None).unwrap().unwrap();
        let json = serde_json::to_value(&card.embed).unwrap();
        assert_eq!(json["title"], "🎮 Among Us");
        assert_eq!(json["description"], "**Gioco** · 🕓 In attesa di revisione");
        assert_eq!(json["color"], COLOR_STAFF);
        let fields = json["fields"].as_array().unwrap();
        assert_eq!((fields[0]["name"].as_str(), fields[0]["value"].as_str(), fields[0]["inline"].as_bool()), (Some("Proposta da"), Some("<@1>, <@2>"), Some(true)));
        assert_eq!((fields[1]["name"].as_str(), fields[1]["value"].as_str()), (Some("Link"), Some("[Apri link](https://www.youtube.com/watch?v=dQw4w9WgXcQ)")));
        assert_eq!(fields.len(), 2);
        assert_eq!(json["thumbnail"]["url"], "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg");
        assert_eq!(json["footer"]["text"], format!("Proposta #{id} · settimana 1"));
        assert_eq!(card.buttons.len(), 2);
        assert_eq!((card.buttons[0].custom_id.as_str(), card.buttons[0].label.as_str(), card.buttons[0].kind), (format!("review:approve:{id}").as_str(), "Approva", ButtonKind::Success));
        assert_eq!((card.buttons[1].custom_id.as_str(), card.buttons[1].emoji, card.buttons[1].kind), (format!("review:reject:{id}").as_str(), Some("❌"), ButtonKind::Danger));
        assert!(card.buttons.iter().all(|b| !b.disabled));
    }

    #[test]
    fn card_lists_possible_duplicates_and_disables_buttons_once_reviewed() {
        let f = fixture();
        let first = propose(&f, 1, "Jackbox Party");
        let second = propose(&f, 2, "Jackbox Party Pack");
        let card = build_review_card(&f.db, second, Some("Nota dello staff")).unwrap().unwrap();
        let json = serde_json::to_value(&card.embed).unwrap();
        assert_eq!(json["description"], "**Gioco** · 🕓 In attesa di revisione\nNota dello staff");
        let duplicates = json["fields"].as_array().unwrap().iter().find(|field| field["name"] == "Possibili duplicati").unwrap();
        assert_eq!(duplicates["value"], format!("`#{first}` **Jackbox Party** · 🕓 In attesa di revisione\nUnisci con `/unisci-proposte`."));
        assert_eq!(duplicates["inline"], false);

        f.db.execute("UPDATE suggestions SET review_status='approved' WHERE id=?", [second]).unwrap();
        let approved = build_review_card(&f.db, second, None).unwrap().unwrap();
        assert!(approved.buttons.iter().all(|b| b.disabled));
        assert!(serde_json::to_value(&approved.embed).unwrap()["description"].as_str().unwrap().contains("✅ Approvata"));
        f.db.execute("UPDATE suggestions SET review_status='reviewing' WHERE id=?", [second]).unwrap();
        assert!(build_review_card(&f.db, second, None).unwrap().unwrap().buttons.iter().all(|b| !b.disabled), "in revisione è ancora aperta");
    }

    #[test]
    fn card_handles_missing_proposals_many_proposers_and_long_titles() {
        let f = fixture();
        assert!(build_review_card(&f.db, 999, None).unwrap().is_none());
        let id = propose(&f, 1, &"x".repeat(80));
        f.db.set_setting("max_proposals", 20).unwrap();
        for user in 2..=14 {
            f.db.execute("INSERT INTO proposals(suggestion_id,user_id,week,ts) VALUES(?,?,1,?)", [id, user, 100 + user]).unwrap();
        }
        let json = serde_json::to_value(&build_review_card(&f.db, id, None).unwrap().unwrap().embed).unwrap();
        let proposers = json["fields"][0]["value"].as_str().unwrap();
        assert_eq!(proposers.matches("<@").count(), 10, "al massimo 10 proponenti");
        assert!(json["title"].as_str().unwrap().chars().count() <= 256);
        assert!(json["thumbnail"].is_null());
    }

    #[test]
    fn card_proposers_are_ordered_by_first_submission() {
        let f = fixture();
        let id = propose(&f, 5, "Among Us");
        f.db.execute("UPDATE proposals SET ts=300", []).unwrap();
        f.db.execute("INSERT INTO proposals(suggestion_id,user_id,week,ts) VALUES(?,?,1,?)", [id, 3, 100]).unwrap();
        let json = serde_json::to_value(&build_review_card(&f.db, id, None).unwrap().unwrap().embed).unwrap();
        assert_eq!(json["fields"][0]["value"], "<@3>, <@5>");
    }

    // --- scenario completo contro Python ----------------------------------------------------

    fn buttons_json(buttons: &[ViewButton]) -> serde_json::Value {
        serde_json::Value::Array(
            buttons
                .iter()
                .map(|b| {
                    let style = match b.kind {
                        ButtonKind::Primary => 1,
                        ButtonKind::Secondary => 2,
                        ButtonKind::Success => 3,
                        ButtonKind::Danger => 4,
                    };
                    json!([b.custom_id, b.label, b.emoji, style, b.disabled])
                })
                .collect(),
        )
    }

    fn embed_json(embed: &CreateEmbed) -> serde_json::Value {
        let data = serde_json::to_value(embed).unwrap();
        json!({
            "title": data["title"],
            "description": data["description"],
            "color": data["color"],
            "fields": data["fields"].as_array().map_or_else(Vec::new, |fields| fields
                .iter()
                .map(|f| json!([f["name"], f["value"], f["inline"].as_bool().unwrap_or(false)]))
                .collect()),
            "thumbnail": data["thumbnail"]["url"],
            "footer": data["footer"]["text"],
        })
    }

    fn table(db: &Database, name: &str, columns: &str, order: &str, filter: &str) -> serde_json::Value {
        db.with_conn(|connection| {
            let mut statement = connection.prepare(&format!("SELECT {columns} FROM {name} {filter} ORDER BY {order}")).unwrap();
            let width = statement.column_count();
            let rows: Vec<serde_json::Value> = statement
                .query_map([], |row| {
                    Ok(serde_json::Value::Array(
                        (0..width)
                            .map(|index| match row.get_ref(index).unwrap() {
                                rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                                rusqlite::types::ValueRef::Integer(n) => n.into(),
                                rusqlite::types::ValueRef::Real(n) => n.into(),
                                rusqlite::types::ValueRef::Text(t) => String::from_utf8(t.to_vec()).unwrap().into(),
                                rusqlite::types::ValueRef::Blob(_) => panic!("blob inattesi"),
                            })
                            .collect(),
                    ))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            serde_json::Value::Array(rows)
        })
    }

    /// Stesso scenario di `golden_polls_review.py`, eseguito sul vero codice Python: revisioni con
    /// audit, schede di revisione, votazioni (pulsanti e menu), scadenze, layout e menu "I miei voti".
    #[tokio::test(start_paused = true)]
    async fn polls_and_review_scenario_matches_the_python_implementation() {
        use crate::polls::{close_expired_polls, find_poll, my_votes_menu, poll_buttons, set_user_votes, toggle_vote, vote_summary};
        use crate::queries::poll_options;
        use serde_json::{Value, json};

        let golden: Value = serde_json::from_str(include_str!("golden/polls_review_scenario.json")).unwrap();
        let f = fixture();
        let db = &f.db;
        let mut log = serde_json::Map::new();

        // --- proposte e revisione
        let among = propose(&f, 1, "Among Us");
        let monopoly = propose(&f, 2, "Monopoly");
        let jack = propose(&f, 3, "Jackbox Party");
        let jackpack = propose(&f, 4, "Jackbox Party Pack");
        let gartic = register_suggestion(db, 5, "Gartic Phone", "Gioco", Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), now()).created_id.unwrap();
        assert!(register_suggestion(db, 6, "among us", "Gioco", None, now()).success);
        log.insert("ids".into(), json!([among, monopoly, jack, jackpack, gartic]));

        let run_review = |label: &'static str, actor: i64, id: i64, action: &'static str, reason: Option<&'static str>, source: ReviewSource| {
            let (db, notifier) = (f.db.clone(), f.notifier.clone());
            let future = async move {
                apply_review(&db, &notifier, &NoEffects, ReviewRequest { actor_id: actor, suggestion_id: id, action, reason, source }).await
            };
            (label, future)
        };
        let mut outcomes = Vec::new();
        for (label, actor, id, action, reason, source) in [
            ("approve_among", 99, among, "approve", None, ReviewSource::Discord),
            ("reject_after_approve", 99, among, "reject", Some("doppione"), ReviewSource::Discord),
            ("reject_no_reason", 99, monopoly, "reject", Some("  "), ReviewSource::Discord),
            ("reject_monopoly", 99, monopoly, "reject", Some(" Troppo lungo "), ReviewSource::Discord),
            ("invalid_action", 99, jack, "elimina", None, ReviewSource::Discord),
            ("missing", 99, 999, "approve", None, ReviewSource::Discord),
            ("review_jack", 99, jack, "review", None, ReviewSource::Discord),
            ("review_jack_again", 99, jack, "review", None, ReviewSource::Discord),
            ("approve_jack_site", 77, jack, "approve", None, ReviewSource::Site),
            ("approve_gartic_reason", 88, gartic, "approve", Some("  ottima  "), ReviewSource::Discord),
        ] {
            let (label, future) = run_review(label, actor, id, action, reason, source);
            outcomes.push((label, future.await));
        }
        for (label, outcome) in outcomes {
            log.insert(label.into(), json!([outcome.ok, outcome.status_code, outcome.message, outcome.review_status, outcome.name]));
        }

        // --- schede di revisione
        let mut cards = serde_json::Map::new();
        for (label, id, note) in [
            ("jackpack", jackpack, None),
            ("gartic", gartic, None),
            ("among_approved", among, None),
            ("monopoly_rejected", monopoly, None),
            ("jack", jack, None),
            ("jackpack_note", jackpack, Some("Nota dello staff")),
        ] {
            let card = build_review_card(db, id, note).unwrap().unwrap();
            cards.insert(label.into(), json!({"embed": embed_json(&card.embed), "buttons": buttons_json(&card.buttons)}));
        }
        log.insert("card_missing".into(), json!([null, null]));
        assert!(build_review_card(db, 999, None).unwrap().is_none());

        // --- sondaggio e voti
        let make_poll = |name: &str, labels: &[String], closes_at: Option<f64>| -> (i64, Vec<i64>) {
            db.execute("INSERT INTO polls(name, active, closes_at) VALUES(?,1,?)", rusqlite::params![name, closes_at]).unwrap();
            let poll_id = db.with_conn(|c| c.last_insert_rowid());
            let ids = labels
                .iter()
                .map(|label| {
                    db.execute("INSERT INTO options(poll_id,label) VALUES(?,?)", rusqlite::params![poll_id, label]).unwrap();
                    db.with_conn(|c| c.last_insert_rowid())
                })
                .collect();
            (poll_id, ids)
        };
        let names = |labels: &[&str]| labels.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let ts = now().timestamp() as f64;
        let (poll_id, ids) = make_poll("Voti", &names(&["A", "B", "C"]), Some(ts + 3600.0));
        let (a, b, c) = (ids[0], ids[1], ids[2]);

        let vote = |log: &mut serde_json::Map<String, Value>, label: &str, user: i64, option: i64| {
            let r = toggle_vote(db, user, option, now()).unwrap();
            log.insert(label.into(), json!([r.changed, r.message, r.poll_id]));
        };
        let setv = |log: &mut serde_json::Map<String, Value>, label: &str, user: i64, options: &[i64], poll: i64| {
            let (ok, message) = set_user_votes(db, user, poll, options, now()).unwrap();
            log.insert(label.into(), json!([ok, message]));
        };
        vote(&mut log, "t1_a", 1, a);
        vote(&mut log, "t1_b", 1, b);
        vote(&mut log, "t1_c_full", 1, c);
        vote(&mut log, "t1_remove_a", 1, a);
        vote(&mut log, "t1_c", 1, c);
        vote(&mut log, "t_missing", 1, 99999);
        setv(&mut log, "s2_ab", 2, &[a, b], poll_id);
        setv(&mut log, "s2_bc", 2, &[b, c], poll_id);
        setv(&mut log, "s2_all", 2, &[a, b, c], poll_id);
        setv(&mut log, "s2_same", 2, &[b, c], poll_id);
        setv(&mut log, "s2_dup", 3, &[a, a, b], poll_id);
        setv(&mut log, "s2_invalid", 3, &[a, 99999], poll_id);
        setv(&mut log, "s2_empty_none", 4, &[], poll_id);
        setv(&mut log, "s2_missing_poll", 4, &[a], 99999);
        log.insert("summary_1".into(), json!(vote_summary(db, poll_id, 1).unwrap()));
        log.insert("summary_none".into(), json!(vote_summary(db, poll_id, 50).unwrap()));
        for _ in 0..4 {
            toggle_vote(db, 9, a, now()).unwrap();
            toggle_vote(db, 9, a, now()).unwrap();
        }
        toggle_vote(db, 9, a, now()).unwrap();
        toggle_vote(db, 9, a, now()).unwrap();
        db.set_setting("max_votes", 3).unwrap();
        setv(&mut log, "s5_three", 5, &[a, b, c], poll_id);
        log.insert("summary_5".into(), json!(vote_summary(db, poll_id, 5).unwrap()));
        db.set_setting("max_votes", 2).unwrap();
        db.execute("UPDATE polls SET active=0 WHERE id=?", [poll_id]).unwrap();
        vote(&mut log, "t_inactive", 6, a);
        setv(&mut log, "s_inactive", 6, &[a], poll_id);
        db.execute("UPDATE polls SET active=1 WHERE id=?", [poll_id]).unwrap();

        // --- layout, scadenza e menu
        let (expired_id, _) = make_poll("Scaduto", &names(&["Opzione"]), Some(ts - 1.0));
        let many_labels: Vec<String> = (1..=12).map(|n| format!("Opzione {n}")).collect();
        let (many_id, _) = make_poll("Tanti", &many_labels, None);
        let (few_id, _) = make_poll("Pochi", &["Uno".into(), "Due".into(), "x".repeat(100)], None);
        let (empty_id, _) = make_poll("Vuoto", &[], None);
        log.insert("closed_count".into(), json!(close_expired_polls(db, now()).unwrap().len()));
        let mut views = serde_json::Map::new();
        for (label, id) in [("voti", poll_id), ("scaduto", expired_id), ("tanti", many_id), ("pochi", few_id), ("vuoto", empty_id)] {
            let poll = find_poll(db, id).unwrap().unwrap();
            views.insert(label.into(), buttons_json(&poll_buttons(&poll, &poll_options(db, id).unwrap())));
        }
        let expired = find_poll(db, expired_id).unwrap().unwrap();
        log.insert("expired_state".into(), json!([expired.active, i64::from(expired.closes_at.is_some())]));

        let menu_json = |poll: i64, user: i64| serde_json::to_value(my_votes_menu(db, poll, user).unwrap()).unwrap();
        let menu = menu_json(poll_id, 1);
        log.insert(
            "menu".into(),
            json!({
                "placeholder": menu["placeholder"], "min": menu["min_values"], "max": menu["max_values"],
                "options": menu["options"].as_array().unwrap().iter()
                    .map(|o| json!([o["label"], o["value"], o["description"], o["default"]]))
                    .collect::<Vec<_>>(),
            }),
        );
        let many_menu = menu_json(many_id, 1);
        log.insert("menu_many".into(), json!({"max": many_menu["max_values"], "count": many_menu["options"].as_array().unwrap().len()}));

        // --- confronto
        for (label, expected) in golden["log"].as_object().unwrap() {
            assert_eq!(log.get(label), Some(expected), "esito «{label}»");
        }
        assert_eq!(log.len(), golden["log"].as_object().unwrap().len(), "stesse voci di log");
        for (label, expected) in golden["cards"].as_object().unwrap() {
            assert_eq!(&cards[label], expected, "scheda «{label}»");
        }
        for (label, expected) in golden["views"].as_object().unwrap() {
            assert_eq!(&views[label], expected, "layout «{label}»");
        }
        let tables = [
            ("suggestions_review", table(db, "suggestions", "id,review_status", "id", "")),
            ("staff_audit", table(db, "staff_audit", "actor_id,action,entity_type,entity_id,reason,details", "id", "")),
            ("votes", table(db, "votes", "option_id,user_id", "option_id,user_id", "")),
            ("vote_change_counts", table(db, "vote_change_counts", "poll_id,user_id,change_count", "poll_id,user_id", "")),
            ("daily_vote_activity", table(db, "daily_vote_activity", "user_id,poll_id,day", "user_id,poll_id", "")),
            ("weekly_activity", table(db, "weekly_activity", "user_id,week,activity_count,points", "user_id,week", "")),
            ("user_streaks", table(db, "user_streaks", "user_id,current_streak,best_streak,last_active_week", "user_id", "")),
            ("user_hidden_badges", table(db, "user_hidden_badges", "user_id,badge_key", "user_id,badge_key", "WHERE badge_key<>'night_owl'")),
            ("polls_active", table(db, "polls", "id,name,active", "id", "")),
            ("community_activity", {
                let rows: Vec<Value> = db.with_conn(|c| {
                    c.prepare("SELECT action, COUNT(*) FROM community_activity GROUP BY action ORDER BY action")
                        .unwrap()
                        .query_map([], |r| Ok(json!([r.get::<_, String>(0)?, r.get::<_, i64>(1)?])))
                        .unwrap()
                        .map(Result::unwrap)
                        .collect()
                });
                Value::Array(rows)
            }),
        ];
        for (name, actual) in &tables {
            assert_eq!(actual, &golden["tables"][*name], "tabella «{name}»");
        }
        assert_eq!(tables.len(), golden["tables"].as_object().unwrap().len(), "stesse tabelle");
    }

    // --- custom_id, posizione della scheda, maschera ----------------------------------------

    #[test]
    fn review_custom_ids_round_trip_and_reject_garbage() {
        assert_eq!(parse_review_custom_id("review:approve:42"), Some((ReviewAction::Approve, 42)));
        assert_eq!(parse_review_custom_id(&review_custom_id(ReviewAction::Reject, 7)), Some((ReviewAction::Reject, 7)));
        for garbage in ["review:approve:", "review:approve:x", "review:review:1", "review:approve", "review:approve:1:2", "vote:1", "", "Review:approve:1", "review:approve:-1", "review:approve:\u{663}"] {
            assert_eq!(parse_review_custom_id(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn review_message_location_is_saved_and_replaced() {
        let f = fixture();
        let id = propose(&f, 1, "Among Us");
        assert_eq!(review_message_location(&f.db, id).unwrap(), None);
        save_review_message(&f.db, id, 900, 1_234_567_890_123).unwrap();
        assert_eq!(review_message_location(&f.db, id).unwrap(), Some(("900".into(), "1234567890123".into())));
        save_review_message(&f.db, id, 901, 55).unwrap();
        assert_eq!(review_message_location(&f.db, id).unwrap(), Some(("901".into(), "55".into())));
        let rows: i64 = f.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM suggestion_review_messages", [], |r| r.get(0)).unwrap());
        assert_eq!(rows, 1);
    }

    #[test]
    fn reject_modal_asks_for_a_3_to_200_character_reason() {
        let json = serde_json::to_value(reject_modal(42)).unwrap();
        assert_eq!(json["custom_id"], "review-reject:42");
        assert_eq!(json["title"], "Rifiuta proposta");
        let input = &json["components"][0]["components"][0];
        assert_eq!(input["label"], "Motivo (verrà inviato agli autori)");
        assert_eq!(input["custom_id"], REJECT_REASON_FIELD_ID);
        assert_eq!(input["style"], 2, "paragrafo");
        assert_eq!((input["min_length"].as_u64(), input["max_length"].as_u64()), (Some(3), Some(200)));
        assert!(input["label"].as_str().unwrap().chars().count() <= 45, "limite Discord per le etichette");
        assert_eq!(parse_reject_modal_id("review-reject:42"), Some(42));
        assert_eq!(parse_reject_modal_id("review-reject:"), None);
        assert_eq!(parse_reject_modal_id("review-reject:x"), None);
        assert_eq!(parse_reject_modal_id("review:reject:42"), None);
    }

    #[test]
    fn statuses_have_labels_and_unknown_ones_pass_through() {
        assert_eq!(status_label("pending"), "🕓 In attesa di revisione");
        assert_eq!(status_label("reviewing"), "🔎 In revisione");
        assert_eq!(status_label("approved"), "✅ Approvata");
        assert_eq!(status_label("rejected"), "❌ Rifiutata");
        assert_eq!(status_label("boh"), "boh");
        assert_eq!(transition("review").unwrap().new_status, "reviewing");
        assert_eq!(transition("approve").unwrap().allowed_from, ["pending", "reviewing"]);
        assert_eq!(transition("reject").unwrap().audit_action, "proposal_rejected");
        assert_eq!(transition("approva"), None);
    }
}
