//! La logica dei comandi slash di `cogs/core_commands.py` che passa da `Interactions`.
//!
//! In Python `ConfirmView`, `ConfirmReset` e `StaffToolsView` ricordano il loro stato in memoria.
//! Qui lo stato sta in una piccola mappa temporanea (per le conferme: l'azione, il proprietario e il
//! motivo) o nei `custom_id` (per i pulsanti di `/staff`), come per gli altri pulsanti.
//!
//! I comandi poise veri e propri (`crate::commands`) sono sottili: costruiscono un `CommandIo` e
//! chiamano questi metodi, che si provano con l'interazione finta.

use std::time::{Duration, Instant};

use poise::serenity_prelude::{ButtonStyle, CreateActionRow, CreateButton, ReactionType};
use serde_json::json;
use tracing::error;

use super::{Interactions, Io, IoError, InteractionIo, Reply};
use crate::discord_api::DiscordApi;
use crate::notify::{AuditEntry, DiscordSender};
use crate::review::review_message_location;
use crate::suggestions::{merge_suggestions, unpick_suggestion};

/// Quanto dura una richiesta di conferma (`timeout=60` di `ConfirmView`).
pub const CONFIRM_TTL: Duration = Duration::from_secs(60);

pub const NOT_OWNER_MESSAGE: &str = "⚠️ Questa conferma è per chi ha avviato il comando.";
pub const CANCELLED_MESSAGE: &str = "Operazione annullata.";
const EXPIRED_MESSAGE: &str = "⌛ Questa richiesta è scaduta: ripeti il comando.";
const TOOLS_RESERVED: &str = "⚠️ Questi strumenti sono riservati allo staff.";

// --- conferme --------------------------------------------------------------------------------

/// Cosa fare quando il proprietario conferma.
#[derive(Debug, Clone)]
pub(super) enum ConfirmAction {
    Unpick { suggestion_id: i64, reason: Option<String> },
    Merge { keep_id: i64, duplicate_id: i64, reason: Option<String>, duplicate_card: Option<(String, String)> },
    Reset,
    DeletePoll { poll_id: i64, name: String, reason: Option<String> },
    RemoveOption { poll_id: i64, poll_name: String, option_name: String, reason: Option<String> },
    /// `/evento elimina-rapporto`: la conferma ha i suoi pulsanti (`evt:del:…`, in `live_commands`).
    DeleteEventReport { event_id: i64, reason: Option<String> },
}

#[derive(Debug, Clone)]
pub(super) struct Confirmation {
    pub(super) owner: i64,
    pub(super) action: ConfirmAction,
    pub(super) created: Instant,
}

/// `cmd:confirm:<richiesta>:yes|no`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmComponent {
    Yes { nonce: u64 },
    No { nonce: u64 },
}

impl ConfirmComponent {
    pub fn parse(custom_id: &str) -> Option<Self> {
        let rest = custom_id.strip_prefix("cmd:confirm:")?;
        let (nonce, choice) = rest.split_once(':')?;
        if nonce.is_empty() || !nonce.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let nonce = nonce.parse().ok()?;
        match choice {
            "yes" => Some(Self::Yes { nonce }),
            "no" => Some(Self::No { nonce }),
            _ => None,
        }
    }

    pub fn id(nonce: u64, yes: bool) -> String {
        format!("cmd:confirm:{nonce}:{}", if yes { "yes" } else { "no" })
    }
}

/// I pulsanti di una conferma: quello rosso con l'etichetta data e, se richiesto, "Annulla".
pub(super) fn confirm_rows(nonce: u64, confirm_label: &str, with_cancel: bool) -> Vec<CreateActionRow> {
    let mut buttons = vec![CreateButton::new(ConfirmComponent::id(nonce, true)).label(confirm_label).style(ButtonStyle::Danger)];
    if with_cancel {
        buttons.push(CreateButton::new(ConfirmComponent::id(nonce, false)).label("Annulla").style(ButtonStyle::Secondary));
    }
    vec![CreateActionRow::Buttons(buttons)]
}

// --- pulsanti di /staff ----------------------------------------------------------------------

/// `staff:tools:proposals:<utente>` e `staff:tools:control:<utente>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaffToolsComponent {
    Proposals { owner: i64 },
    Control { owner: i64 },
}

impl StaffToolsComponent {
    pub fn parse(custom_id: &str) -> Option<Self> {
        let rest = custom_id.strip_prefix("staff:tools:")?;
        let (kind, owner) = rest.split_once(':')?;
        if owner.is_empty() || !owner.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let owner = owner.parse().ok()?;
        match kind {
            "proposals" => Some(Self::Proposals { owner }),
            "control" => Some(Self::Control { owner }),
            _ => None,
        }
    }

    pub fn proposals_id(owner: i64) -> String {
        format!("staff:tools:proposals:{owner}")
    }

    pub fn control_id(owner: i64) -> String {
        format!("staff:tools:control:{owner}")
    }
}

const STAFF_TOOLS_TEXT: &str = "🛠️ **Strumenti staff**\n\
• **Selezione proposte**: scegli cosa inserire nel ballottaggio, con la proposta consigliata.\n\
• **Centro controllo**: analisi, revisioni e impostazioni.\n\
Le nuove proposte arrivano anche nel canale staff con i bottoni Approva/Rifiuta.";

// --- i comandi -------------------------------------------------------------------------------

impl<S: DiscordSender, A: DiscordApi> Interactions<S, A> {
    pub(super) fn setting_is_set(&self, key: &str) -> bool {
        self.db().get_setting(key).ok().flatten().is_some_and(|value| !value.is_empty())
    }

    pub(super) fn remember_confirmation(&self, owner: i64, action: ConfirmAction) -> u64 {
        let nonce = self.next_nonce.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut map = self.confirmations.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let ttl = self.confirm_ttl;
        map.retain(|_, entry| entry.created.elapsed() < ttl);
        map.insert(nonce, Confirmation { owner, action, created: Instant::now() });
        nonce
    }

    /// `/suggerisci`.
    pub async fn command_suggest<I: InteractionIo>(&self, io: &I, nome: &str, tipo: &str, link: Option<String>) -> Io<()> {
        self.submit_suggestion(io, nome, tipo, link, true, false).await
    }

    /// `/scegli`: inserisce una proposta approvata nel ballottaggio.
    pub async fn command_pick<I: InteractionIo>(&self, io: &I, suggestion_id: i64, reason: Option<&str>) -> Io<()> {
        io.defer().await?;
        let mut outcome = Ok(());
        self.services
            .pick_suggestion(io.user_id(), suggestion_id, reason, None, self.now(), async |text: String| {
                outcome = io.followup(Reply::text(text)).await;
            })
            .await;
        outcome
    }

    /// `/rimuovi-scelta`: chiede conferma, poi toglie la proposta dal ballottaggio.
    pub async fn command_unpick<I: InteractionIo>(&self, io: &I, suggestion_id: i64, reason: Option<String>) -> Io<()> {
        if !self.setting_is_set("poll_channel") || !self.setting_is_set("staff_channel") {
            return io.respond(Reply::text("⚠️ Prima configura i canali con /config.")).await;
        }
        let nonce = self.remember_confirmation(io.user_id(), ConfirmAction::Unpick { suggestion_id, reason });
        io.respond(
            Reply::text(
                "⚠️ Togliere la proposta dal ballottaggio cancella i voti che ha ricevuto e i punti «ballottaggio» dei suoi autori. Continuare?",
            )
            .with_components(confirm_rows(nonce, "Togli dal ballottaggio", true)),
        )
        .await
    }

    /// `/unisci-proposte`: chiede conferma, poi unisce la duplicata nella principale.
    pub async fn command_merge<I: InteractionIo>(&self, io: &I, keep_id: i64, duplicate_id: i64, reason: Option<String>) -> Io<()> {
        // La scheda della duplicata va ritrovata prima dell'unione, che la elimina.
        let duplicate_card = review_message_location(self.db(), duplicate_id).unwrap_or_else(|e| {
            error!("Impossibile leggere la scheda della proposta {duplicate_id}: {e}");
            None
        });
        let nonce = self.remember_confirmation(io.user_id(), ConfirmAction::Merge { keep_id, duplicate_id, reason, duplicate_card });
        io.respond(
            Reply::text(
                "🔀 Proposte, sostegni e link della duplicata passeranno alla principale, e la duplicata verrà eliminata. Continuare?",
            )
            .with_components(confirm_rows(nonce, "Unisci", true)),
        )
        .await
    }

    /// `/reset-manuale`: chiede conferma (un solo pulsante, come `ConfirmReset`).
    pub async fn command_manual_reset<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let nonce = self.remember_confirmation(io.user_id(), ConfirmAction::Reset);
        io.respond(
            Reply::text("⚠️ Vuoi davvero azzerare voti e proposte della settimana? I risultati verranno salvati nello storico.")
                .with_components(confirm_rows(nonce, "Conferma reset", false)),
        )
        .await
    }

    /// `/staff`: i due strumenti dello staff.
    pub async fn command_staff<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let owner = io.user_id();
        let buttons = vec![
            CreateButton::new(StaffToolsComponent::proposals_id(owner))
                .label("Selezione proposte")
                .emoji(ReactionType::Unicode("🗳️".into()))
                .style(ButtonStyle::Primary),
            CreateButton::new(StaffToolsComponent::control_id(owner))
                .label("Centro controllo")
                .emoji(ReactionType::Unicode("🛠️".into()))
                .style(ButtonStyle::Secondary),
        ];
        io.respond(Reply::text(STAFF_TOOLS_TEXT).with_components(vec![CreateActionRow::Buttons(buttons)])).await
    }

    /// `/storico`: le ultime settimane chiuse.
    pub async fn command_history<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let lines = crate::cards::history_summary_lines(self.db(), 10).map_err(|e| IoError(e.to_string()))?;
        if lines.is_empty() {
            return io.respond(Reply::text("Nessuna settimana in archivio.")).await;
        }
        io.respond(Reply::text(lines.join("\n"))).await
    }

    // --- pulsanti ---

    pub(super) async fn staff_tools_component<I: InteractionIo>(&self, io: &I, component: StaffToolsComponent) -> Io<()> {
        let (StaffToolsComponent::Proposals { owner } | StaffToolsComponent::Control { owner }) = component;
        // `interaction_check` della vista Python: solo chi ha aperto gli strumenti, e solo se è staff.
        if io.user_id() != owner || !self.is_staff(io) {
            return io.respond(Reply::text(TOOLS_RESERVED)).await;
        }
        match component {
            StaffToolsComponent::Proposals { .. } => self.open_staff_panel(io).await,
            StaffToolsComponent::Control { .. } => self.command_control_center(io).await,
        }
    }

    pub(super) async fn confirm_component<I: InteractionIo>(&self, io: &I, component: ConfirmComponent) -> Io<()> {
        let (ConfirmComponent::Yes { nonce } | ConfirmComponent::No { nonce }) = component;
        enum Lookup {
            Missing,
            Foreign,
            Found(Confirmation),
        }
        let lookup = {
            let mut map = self.confirmations.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match map.get(&nonce) {
                None => Lookup::Missing,
                Some(entry) if entry.created.elapsed() >= self.confirm_ttl => {
                    map.remove(&nonce);
                    Lookup::Missing
                }
                // Non è la sua richiesta: gli altri non possono rispondere, ma la richiesta resta aperta.
                Some(entry) if entry.owner != io.user_id() => Lookup::Foreign,
                // Si toglie subito: un doppio clic non ripete l'azione.
                Some(_) => map.remove(&nonce).map_or(Lookup::Missing, Lookup::Found),
            }
        };
        let entry = match lookup {
            Lookup::Foreign => return io.respond(Reply::text(NOT_OWNER_MESSAGE)).await,
            Lookup::Missing => None,
            Lookup::Found(entry) => Some(entry),
        };
        let Some(entry) = entry else {
            return io.update(Reply::text(EXPIRED_MESSAGE).without_components()).await;
        };
        if matches!(component, ConfirmComponent::No { .. }) {
            return io.update(Reply::text(CANCELLED_MESSAGE).without_components()).await;
        }
        match entry.action {
            ConfirmAction::Unpick { suggestion_id, reason } => self.confirmed_unpick(io, suggestion_id, reason).await,
            ConfirmAction::Merge { keep_id, duplicate_id, reason, duplicate_card } => {
                self.confirmed_merge(io, keep_id, duplicate_id, reason, duplicate_card).await
            }
            ConfirmAction::Reset => self.confirmed_reset(io).await,
            ConfirmAction::DeletePoll { poll_id, name, reason } => self.confirmed_delete_poll(io, poll_id, name, reason).await,
            ConfirmAction::RemoveOption { poll_id, poll_name, option_name, reason } => {
                self.confirmed_remove_option(io, poll_id, poll_name, option_name, reason).await
            }
            // Ha i suoi pulsanti: un clic con questo `custom_id` non dovrebbe arrivare qui.
            ConfirmAction::DeleteEventReport { .. } => io.update(Reply::text(EXPIRED_MESSAGE).without_components()).await,
        }
    }

    async fn confirmed_unpick<I: InteractionIo>(&self, io: &I, suggestion_id: i64, reason: Option<String>) -> Io<()> {
        let result = match unpick_suggestion(self.db(), suggestion_id, self.now()) {
            Ok(result) => result,
            Err(e) => {
                error!("Errore SQLite durante la rimozione di una proposta scelta: {e}");
                return io.update(Reply::text("❌ Non è stato possibile rimuovere la proposta. Riprova tra poco.").without_components()).await;
            }
        };
        let Some((suggestion, poll_id)) = result else {
            return io.update(Reply::text("⚠️ Proposta non trovata tra quelle nel ballottaggio.").without_components()).await;
        };
        io.update(Reply::text(format!("🗑️ **{}** tolta dal ballottaggio.", suggestion.name)).without_components()).await?;
        let notifier = &self.services.notifier;
        notifier.record_staff_audit(
            AuditEntry::new(u64::try_from(io.user_id()).ok(), "proposal_unselected")
                .entity("suggestion", suggestion.id)
                .reason(reason.as_deref())
                .details(json!({"name": suggestion.name, "week": suggestion.week})),
        );
        if let Some(poll_id) = poll_id.filter(|id| *id != 0) {
            self.services.publisher.schedule_poll_refresh(&self.services.scheduler, poll_id);
        }
        self.services.publisher.schedule_dashboard_refresh(&self.services.scheduler);
        crate::hooks::publish_site_change("proposals");
        crate::hooks::publish_site_change("polls");
        notifier.staff_log(format!("❌ <@{}> ha tolto «{}» dal ballottaggio.", io.user_id(), suggestion.name));
        Ok(())
    }

    async fn confirmed_merge<I: InteractionIo>(
        &self,
        io: &I,
        keep_id: i64,
        duplicate_id: i64,
        reason: Option<String>,
        duplicate_card: Option<(String, String)>,
    ) -> Io<()> {
        let outcome = match merge_suggestions(self.db(), keep_id, duplicate_id) {
            Ok(outcome) => outcome,
            Err(e) => {
                error!("Errore SQLite durante l'unione delle proposte: {e}");
                return io.update(Reply::text("❌ Non è stato possibile unire le proposte. Controlla i log.").without_components()).await;
            }
        };
        io.update(Reply::text(outcome.message.clone()).without_components()).await?;
        let (true, Some(keep), Some(duplicate)) = (outcome.ok, outcome.keep, outcome.duplicate) else {
            return Ok(());
        };
        let notifier = &self.services.notifier;
        notifier.record_staff_audit(
            AuditEntry::new(u64::try_from(io.user_id()).ok(), "proposals_merged")
                .entity("suggestion", keep.id)
                .reason(reason.as_deref())
                .details(json!({"source_id": duplicate.id, "source_name": duplicate.name, "target_name": keep.name})),
        );
        let publisher = &self.services.publisher;
        let note = format!("🔀 Unita in «{}» da <@{}>.", keep.name, io.user_id());
        publisher.update_review_message(duplicate_id, Some(&note), duplicate_card).await;
        publisher.update_review_message(keep.id, None, None).await;
        publisher.schedule_dashboard_refresh(&self.services.scheduler);
        crate::hooks::publish_site_change("proposals");
        notifier.staff_log(format!("🔀 <@{}> ha unito «{}» in «{}».", io.user_id(), duplicate.name, keep.name));
        Ok(())
    }

    async fn confirmed_reset<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let Some(service) = self.reset.get() else {
            return io.update(Reply::text("❌ Reset non riuscito, controlla i log").without_components()).await;
        };
        io.update(Reply::text("Reset in corso…").without_components()).await?;
        match service.manual_reset(io.user_id(), &io.user_name()).await {
            Ok(_) => io.edit_response(Reply::text("✅ Reset completato.")).await,
            Err(e) => {
                error!("Reset manuale non riuscito: {e}");
                io.edit_response(Reply::text("❌ Reset non riuscito, controlla i log")).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_ids_round_trip_and_reject_garbage() {
        assert_eq!(ConfirmComponent::parse(&ConfirmComponent::id(42, true)), Some(ConfirmComponent::Yes { nonce: 42 }));
        assert_eq!(ConfirmComponent::parse(&ConfirmComponent::id(42, false)), Some(ConfirmComponent::No { nonce: 42 }));
        for garbage in ["cmd:confirm:", "cmd:confirm:x:yes", "cmd:confirm:1:maybe", "cmd:confirm:1", "cmd:confirm:-1:yes", "sfo:confirm:1:yes"] {
            assert_eq!(ConfirmComponent::parse(garbage), None, "{garbage}");
        }
        assert_eq!(StaffToolsComponent::parse(&StaffToolsComponent::proposals_id(7)), Some(StaffToolsComponent::Proposals { owner: 7 }));
        assert_eq!(StaffToolsComponent::parse(&StaffToolsComponent::control_id(7)), Some(StaffToolsComponent::Control { owner: 7 }));
        for garbage in ["staff:tools:", "staff:tools:other:1", "staff:tools:proposals:", "staff:tools:proposals:x", "staff:tools:proposals"] {
            assert_eq!(StaffToolsComponent::parse(garbage), None, "{garbage}");
        }
    }
}
