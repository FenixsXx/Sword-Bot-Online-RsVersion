//! Componenti di interfaccia riutilizzabili (equivalente di `sfo/ui.py`).
//!
//! discord.py modella i pulsanti come una `View` con callback. In poise/serenity non esiste
//! `View`: si invia un messaggio con i componenti e si attendono i clic con un
//! `ComponentInteractionCollector`. [`confirm_view`] è il `ConfirmView` di Python in questa forma:
//! invece di passare una callback `on_confirm`, restituisce l'esito e chi chiama continua da lì.
// Verrà usato dai comandi che chiedono conferma (reset, rimozioni, unione di proposte).
#![allow(dead_code)]

use std::time::Duration;

use poise::serenity_prelude::{
    ButtonStyle, ComponentInteraction, ComponentInteractionCollector, CreateActionRow, CreateButton,
    CreateInteractionResponse, CreateInteractionResponseMessage, ReactionType, UserId,
};
use poise::CreateReply;
use tracing::warn;

use crate::{Context, Error};

pub const DEFAULT_CONFIRM_LABEL: &str = "Conferma";
pub const CANCEL_LABEL: &str = "Annulla";
/// Come `timeout=60` di `ConfirmView`: scade se nessuno clicca per un minuto.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
pub const NOT_OWNER_MESSAGE: &str = "⚠️ Questa conferma è per chi ha avviato il comando.";
pub const CANCELLED_MESSAGE: &str = "Operazione annullata.";

// --- pulsanti persistenti --------------------------------------------------------------------

/// Stile di un pulsante (i colori di `discord.ButtonStyle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Success,
    Danger,
}

impl ButtonKind {
    fn style(self) -> ButtonStyle {
        match self {
            ButtonKind::Primary => ButtonStyle::Primary,
            ButtonKind::Secondary => ButtonStyle::Secondary,
            ButtonKind::Success => ButtonStyle::Success,
            ButtonKind::Danger => ButtonStyle::Danger,
        }
    }
}

/// Descrizione di un pulsante, separata da serenity così i layout si possono confrontare nei test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewButton {
    pub custom_id: String,
    pub label: String,
    pub emoji: Option<&'static str>,
    pub kind: ButtonKind,
    pub disabled: bool,
}

impl ViewButton {
    fn create(&self) -> CreateButton {
        let mut button = CreateButton::new(self.custom_id.as_str())
            .label(self.label.as_str())
            .style(self.kind.style())
            .disabled(self.disabled);
        if let Some(emoji) = self.emoji {
            button = button.emoji(ReactionType::Unicode(emoji.to_string()));
        }
        button
    }
}

/// Pulsanti per riga: Discord ne ammette 5; `discord.ui.View` riempie le righe nell'ordine di
/// inserimento, e così fa questa funzione.
pub const BUTTONS_PER_ROW: usize = 5;

/// Dispone i pulsanti su righe da [`BUTTONS_PER_ROW`], come `discord.ui.View`.
pub fn button_rows(buttons: &[ViewButton]) -> Vec<CreateActionRow> {
    buttons
        .chunks(BUTTONS_PER_ROW)
        .map(|row| CreateActionRow::Buttons(row.iter().map(ViewButton::create).collect()))
        .collect()
}

/// Come è finita la richiesta di conferma.
#[derive(Debug)]
pub enum ConfirmOutcome {
    /// Il proprietario ha premuto il pulsante di conferma. L'interazione non è ancora stata
    /// risposta: chi chiama deve rispondere (di solito `UpdateMessage` che toglie i pulsanti,
    /// così un doppio clic non ripete l'azione).
    Confirmed(ComponentInteraction),
    /// Il proprietario ha premuto "Annulla": il messaggio è già stato aggiornato.
    Cancelled,
    /// Nessun clic entro il timeout: i pulsanti sono stati tolti dal messaggio.
    TimedOut,
}

/// `custom_id` dei due pulsanti, unici per ogni richiesta (derivati dall'ID dell'interazione).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfirmIds {
    confirm: String,
    cancel: String,
}

impl ConfirmIds {
    fn new(nonce: u64) -> Self {
        Self { confirm: format!("sfo:confirm:{nonce}:yes"), cancel: format!("sfo:confirm:{nonce}:no") }
    }
}

/// Cosa significa un clic su uno dei componenti raccolti.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Click {
    Confirm,
    Cancel,
    /// Un altro utente: non può rispondere a una conferma non sua.
    NotOwner,
    /// Un componente di un'altra richiesta.
    Unrelated,
}

/// Come `interaction_check` + callback dei pulsanti: prima il controllo del proprietario,
/// poi quale pulsante è stato premuto.
fn classify(custom_id: &str, ids: &ConfirmIds, user: UserId, owner: UserId) -> Click {
    if custom_id != ids.confirm && custom_id != ids.cancel {
        return Click::Unrelated;
    }
    if user != owner {
        return Click::NotOwner;
    }
    if custom_id == ids.confirm { Click::Confirm } else { Click::Cancel }
}

fn components(ids: &ConfirmIds, confirm_label: &str) -> Vec<CreateActionRow> {
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new(ids.confirm.as_str()).label(confirm_label).style(ButtonStyle::Danger),
        CreateButton::new(ids.cancel.as_str()).label(CANCEL_LABEL).style(ButtonStyle::Secondary),
    ])]
}

/// Chiede conferma prima di un'azione irreversibile, con un messaggio visibile solo a chi ha
/// avviato il comando.
///
/// Solo chi ha avviato il comando può rispondere; gli altri ricevono un avviso privato e la
/// richiesta resta aperta. Il timeout riparte a ogni clic, come in `discord.ui.View`.
///
/// Differenza da Python: alla scadenza i pulsanti vengono tolti (in Python restavano visibili ma
/// inerti e un clic mostrava "interazione fallita").
pub async fn confirm_view(
    ctx: Context<'_>,
    content: &str,
    confirm_label: &str,
    timeout: Duration,
) -> Result<ConfirmOutcome, Error> {
    let ids = ConfirmIds::new(ctx.id());
    let owner = ctx.author().id;
    let handle = ctx
        .send(CreateReply::default().content(content).ephemeral(true).components(components(&ids, confirm_label)))
        .await?;
    let message_id = handle.message().await?.id;

    loop {
        let Some(interaction) =
            ComponentInteractionCollector::new(ctx).message_id(message_id).timeout(timeout).await
        else {
            if let Err(e) = handle.edit(ctx, CreateReply::default().content(content).components(Vec::new())).await {
                warn!("Impossibile togliere i pulsanti di conferma scaduti: {e}");
            }
            return Ok(ConfirmOutcome::TimedOut);
        };

        match classify(&interaction.data.custom_id, &ids, interaction.user.id, owner) {
            Click::Confirm => return Ok(ConfirmOutcome::Confirmed(interaction)),
            Click::Cancel => {
                interaction
                    .create_response(
                        ctx,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .content(CANCELLED_MESSAGE)
                                .components(Vec::new()),
                        ),
                    )
                    .await?;
                return Ok(ConfirmOutcome::Cancelled);
            }
            Click::NotOwner => {
                interaction
                    .create_response(
                        ctx,
                        CreateInteractionResponse::Message(
                            CreateInteractionResponseMessage::new().content(NOT_OWNER_MESSAGE).ephemeral(true),
                        ),
                    )
                    .await?;
            }
            Click::Unrelated => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: UserId = UserId::new(10);
    const OTHER: UserId = UserId::new(99);

    // Esempio d'uso reale (come `/rimuovi-scelta` in Python); verifica anche che i tipi tornino.
    #[poise::command(slash_command)]
    async fn confirm_probe(ctx: Context<'_>) -> Result<(), Error> {
        match confirm_view(ctx, "Continuare?", "Togli dal ballottaggio", DEFAULT_TIMEOUT).await? {
            ConfirmOutcome::Confirmed(interaction) => {
                interaction
                    .create_response(
                        ctx,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new().content("Fatto.").components(Vec::new()),
                        ),
                    )
                    .await?;
            }
            ConfirmOutcome::Cancelled | ConfirmOutcome::TimedOut => {}
        }
        Ok(())
    }

    #[test]
    fn confirm_view_can_be_used_from_a_poise_command() {
        assert_eq!(confirm_probe().name, "confirm_probe");
    }

    #[test]
    fn buttons_are_laid_out_five_per_row_in_order() {
        let button = |index: usize| ViewButton {
            custom_id: format!("b:{index}"),
            label: format!("Bottone {index}"),
            emoji: (index == 0).then_some("📋"),
            kind: if index == 0 { ButtonKind::Primary } else { ButtonKind::Secondary },
            disabled: index % 2 == 1,
        };
        let buttons: Vec<ViewButton> = (0..11).map(button).collect();
        let rows = serde_json::to_value(button_rows(&buttons)).unwrap();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.iter().map(|row| row["components"].as_array().unwrap().len()).collect::<Vec<_>>(), [5, 5, 1]);
        let first = &rows[0]["components"][0];
        assert_eq!((first["custom_id"].as_str(), first["label"].as_str(), first["style"].as_u64()), (Some("b:0"), Some("Bottone 0"), Some(1)));
        assert_eq!(first["emoji"]["name"], "📋");
        assert!(rows[0]["components"][1]["emoji"].is_null());
        assert_eq!(rows[0]["components"][1]["disabled"], true);
        assert_eq!(rows[2]["components"][0]["custom_id"], "b:10");
        assert!(button_rows(&[]).is_empty());
    }

    #[test]
    fn owner_can_confirm_or_cancel() {
        let ids = ConfirmIds::new(7);
        assert_eq!(classify(&ids.confirm, &ids, OWNER, OWNER), Click::Confirm);
        assert_eq!(classify(&ids.cancel, &ids, OWNER, OWNER), Click::Cancel);
    }

    #[test]
    fn other_users_are_rejected_on_both_buttons() {
        let ids = ConfirmIds::new(7);
        assert_eq!(classify(&ids.confirm, &ids, OTHER, OWNER), Click::NotOwner);
        assert_eq!(classify(&ids.cancel, &ids, OTHER, OWNER), Click::NotOwner);
    }

    #[test]
    fn components_of_other_requests_are_ignored() {
        let ids = ConfirmIds::new(7);
        let other = ConfirmIds::new(8);
        assert_eq!(classify(&other.confirm, &ids, OWNER, OWNER), Click::Unrelated);
        assert_eq!(classify("qualcosa-altro", &ids, OTHER, OWNER), Click::Unrelated);
    }

    #[test]
    fn ids_are_unique_per_request_and_within_discord_limits() {
        let first = ConfirmIds::new(1);
        let second = ConfirmIds::new(2);
        assert_ne!(first, second);
        assert_ne!(first.confirm, first.cancel);
        // Discord limita il custom_id a 100 caratteri; un ID interazione (snowflake) ne ha ~19.
        assert!(ConfirmIds::new(u64::MAX).confirm.len() <= 100);
    }

    #[test]
    fn buttons_carry_the_requested_labels_and_styles() {
        let ids = ConfirmIds::new(7);
        let rows = serde_json::to_value(components(&ids, "Togli dal ballottaggio")).unwrap();
        let buttons = &rows[0]["components"];
        assert_eq!(buttons[0]["label"], "Togli dal ballottaggio");
        assert_eq!(buttons[0]["style"], 4, "Danger");
        assert_eq!(buttons[0]["custom_id"], ids.confirm.as_str());
        assert_eq!(buttons[1]["label"], CANCEL_LABEL);
        assert_eq!(buttons[1]["style"], 2, "Secondary");
        assert_eq!(buttons[1]["custom_id"], ids.cancel.as_str());
    }
}
