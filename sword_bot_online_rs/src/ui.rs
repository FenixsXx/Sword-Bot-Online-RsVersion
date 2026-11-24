//! Componenti di interfaccia riutilizzabili (equivalente di `sfo/ui.py`).
//!
//! Questo modulo fornisce componenti UI per interagire con gli utenti Discord.
//!
//! # Panoramica
//!
//! I componenti Discord (pulsanti, menu a tendina, modali) sono modi per
//! permettere all'utente di interagire senza scrivere testo lungo.
//! Questo modulo fornisce componenti riutilizzabili per:
//! - Conferme (sì/no)
//! - Pulsanti di azione
//! - Gestione dei timeout
//!
//! # Differenze da discord.py
//!
//! `discord.py` usa un sistema a "callback" con `View`. I pulsanti hanno una
//! funzione che si esegue quando l'utente clicca.
//!
//! In `poise`/`serenity` Rust, il sistema è diverso:
//! - Si invia un messaggio con i pulsanti
//! - Si "raccoglie" le interazioni con `ComponentInteractionCollector`
//! - Si elabora l'esito in un loop
//!
//! Per questo motivo `confirm_view` ha un comportamento diverso:
//! - **discord.py**: passi una callback, l'utente clicca, la callback si esegue
//! - **Rust**: passi un contesto, l'utente clicca, la funzione restituisce l'esito
//!
//! # Esempio d'uso
//!
//! ```text
//! match confirm_view(ctx, "Cancellare tutto?", "Sì, cancella", 60s).await? {
//!     ConfirmOutcome::Confirmed(interaction) => {
//!         // L'utente ha premuto "Sì"
//!         interaction.create_response(...).await?;
//!     }
//!     ConfirmOutcome::Cancelled => {
//!         // L'utente ha premuto "Annulla"
//!     }
//!     ConfirmOutcome::TimedOut => {
//!         // Nessun clic entro 60 secondi
//!     }
//! }
//! ```
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
///
/// Discord ha stili fissi per i pulsanti:
/// - Primary (Blu): Azione principale
/// - Secondary (Grigio): Azione neutra o annullamento
/// - Success (Verde): Conferma positiva
/// - Danger (Rosso): Azione distruttiva (es. elimina)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Blu, per azioni principali.
    Primary,
    /// Grigio, per azioni secondarie o annullamento.
    Secondary,
    /// Verde, per azioni positive.
    Success,
    /// Rosso, per azioni potenzialmente distruttive.
    Danger,
}

impl ButtonKind {
    /// Converte il nostro tipo `ButtonKind` nello stile nativo di Discord.
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
///
/// Questo oggetto è una rappresentazione indipendente dal framework per poter
/// testare i layout dei pulsanti (es. quanti pulsanti per riga) senza dover
/// avviare un bot Discord reale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewButton {
    /// ID personalizzato per identificare il clic (es. `b:0`)
    pub custom_id: String,
    /// Testo mostrato sul pulsante
    pub label: String,
    /// Emoji opzionale (es. `"📋"`)
    pub emoji: Option<&'static str>,
    /// Stile del pulsante (colore)
    pub kind: ButtonKind,
    /// Se il pulsante è disabilitato (non cliccabile)
    pub disabled: bool,
}

impl ViewButton {
    /// Converte il nostro `ViewButton` in un pulsante nativo `CreateButton` di serenity.
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
///
/// Questo enum rappresenta tutti i possibili esiti di una finestra di conferma:
/// - `Confirmed`: l'utente ha premuto il pulsante di conferma. Restituisce
///   l'interazione così puoi rispondere e aggiornare il messaggio.
/// - `Cancelled`: l'utente ha premuto "Annulla". Il messaggio è già stato aggiornato.
/// - `TimedOut`: nessuno ha cliccato entro il tempo limite (60 secondi). I pulsanti
///   vengono rimossi automaticamente.
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

/// `custom_id` dei due pulsanti, unici per ogni richiesta.
///
/// Questi ID sono unici per ogni richiesta di conferma e devono essere
/// unici anche all'interno di Discord (max 100 caratteri).
///
/// # Regole di sicurezza
///
/// - Non possono essere usati per attacchi XSS
/// - Non possono essere usati per violare la privacy
/// - Devono essere unici per ogni richiesta
///
/// # Esempio
///
/// ```text
/// confirm_id = "sfo:confirm:12345:yes"
/// cancel_id = "sfo:confirm:12345:no"
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfirmIds {
    /// ID del pulsante di conferma (sì)
    confirm: String,
    /// ID del pulsante di annullamento (no)
    cancel: String,
}

impl ConfirmIds {
    /// Crea un nuovo set di ID unici per una richiesta di conferma.
    ///
    /// Usa un nonce (numero casuale) per garantire unicità.
    ///
    /// # Parametri
    ///
    /// - `nonce`: Un numero univoco per questa richiesta (es. `ctx.id()`)
    ///
    /// # Esempio
    ///
    /// ```text
    /// let ids = ConfirmIds::new(12345);
    /// assert_eq!(ids.confirm, "sfo:confirm:12345:yes");
    /// assert_eq!(ids.cancel, "sfo:confirm:12345:no");
    /// ```
    fn new(nonce: u64) -> Self {
        Self { confirm: format!("sfo:confirm:{nonce}:yes"), cancel: format!("sfo:confirm:{nonce}:no") }
    }
}

/// Cosa significa un clic su uno dei componenti raccolti.
///
/// Quando raccogliamo le interazioni, dobbiamo classificare ogni clic
/// per capire se è una conferma, un annullamento, un altro utente,
/// o un clic su un componente non nostro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Click {
    /// Il proprietario ha cliccato "Conferma"
    Confirm,
    /// Il proprietario ha cliccato "Annulla"
    Cancel,
    /// Un altro utente: non può rispondere a una conferma non sua.
    NotOwner,
    /// Un componente di un'altra richiesta (es. clic su un pulsante vecchio).
    Unrelated,
}

/// Classifica un clic per capire cosa significa.
///
/// Questa funzione è il cuore della logica di controllo:
/// 1. Prima verifica se il pulsante cliccato appartiene a QUESTA richiesta
/// 2. Poi verifica se chi ha cliccato è il PROPRIETARIO (chi ha avviato il comando)
/// 3. Infine determina se è conferma o annullamento
///
/// # Parametri
///
/// - `custom_id`: L'ID del pulsante cliccato
/// - `ids`: Gli ID di conferma per questa richiesta
/// - `user`: Chi ha cliccato
/// - `owner`: Chi ha avviato il comando
///
/// # Ritorno
///
/// - `Click::Confirm`: Proprietario ha cliccato "Sì"
/// - `Click::Cancel`: Proprietario ha cliccato "No"
/// - `Click::NotOwner`: Qualcun altro ha cliccato
/// - `Click::Unrelated`: Pulsante di un'altra richiesta
fn classify(custom_id: &str, ids: &ConfirmIds, user: UserId, owner: UserId) -> Click {
    if custom_id != ids.confirm && custom_id != ids.cancel {
        return Click::Unrelated;
    }
    if user != owner {
        return Click::NotOwner;
    }
    if custom_id == ids.confirm { Click::Confirm } else { Click::Cancel }
}

/// Crea i componenti (pulsanti) per una conferma.
///
/// Genera una riga con due pulsanti:
/// - Conferma: stile Danger (rosso), etichetta personalizzata
/// - Annulla: stile Secondary (grigio), etichetta "Annulla"
///
/// # Parametri
///
/// - `ids`: Gli ID unici per questa conferma
/// - `confirm_label`: Cosa scrivere sul pulsante di conferma (es. "Elimina", "Reset")
///
/// # Note
///
/// Il pulsante di conferma usa `Danger` (rosso) perché solitamente
/// queste conferme sono per azioni distruttive.
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
/// Chiede conferma prima di un'azione irreversibile, con un messaggio visibile solo a chi ha
/// avviato il comando.
///
/// Solo chi ha avviato il comando può rispondere; gli altri ricevono un avviso privato e la
/// richiesta resta aperta. Il timeout riparte a ogni clic, come in `discord.ui.View`.
///
/// # Parametri
///
/// - `ctx`: Contesto del comando poise/serenity
/// - `content`: Testo della domanda (es. `"Vuoi davvero eliminare questa proposta?"`)
/// - `confirm_label`: Etichetta sul pulsante di conferma (es. `"Elimina"`)
/// - `timeout`: Durata massima prima della scadenza (es. `Duration::from_secs(60)`)
///
/// # Ritorno
///
/// Un `Result<ConfirmOutcome, Error>` che indica se l'utente ha confermato,
/// annullato o se è scaduto il tempo.
///
/// # Esempio di utilizzo
///
/// ```text
/// match confirm_view(ctx, "Resetta il database?", "Conferma Reset", Duration::from_secs(30)).await? {
///     ConfirmOutcome::Confirmed(interaction) => {
///         // Esegui il reset
///         db.reset()?;
///         // Rispondi all'interazione
///         interaction.create_response(ctx, ...).await?;
///     }
///     ConfirmOutcome::Cancelled => {
///         // L'utente ha annullato
///     }
///     ConfirmOutcome::TimedOut => {
///         // Timeout scaduto
///     }
/// }
/// ```
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
