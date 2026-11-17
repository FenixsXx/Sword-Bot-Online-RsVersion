//! `/site` e `/general` (la parte di `cogs/staff_tools.py`).
//!
//! In Python `GeneralStaffView` ricorda in memoria chi ha aperto il pannello e dura 5 minuti. Qui il
//! proprietario sta nel `custom_id` dei due pulsanti (`staff:general:center:<utente>` e
//! `staff:general:live:<utente>`), come per gli altri pannelli: i pulsanti restano validi anche dopo
//! un riavvio.

use std::sync::Arc;

use poise::serenity_prelude::{ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, ReactionType};

use super::{InteractionIo, Interactions, Io, Reply};
use crate::config::COLOR_STAFF;
use crate::discord_api::DiscordApi;
use crate::notify::DiscordSender;
use crate::overlay::LiveState;

const STAFF_ONLY: &str = "⚠️ Questo comando è riservato allo staff.";
const PANEL_OWNER_ONLY: &str = "⚠️ Questo pannello è riservato a chi l'ha aperto.";
const PANEL_NOT_STAFF_ANYMORE: &str = "⚠️ Non hai più i permessi per usare questo pannello.";
const NO_LIVE_EVENT: &str = "ℹ️ Non c'è un evento live attivo.";
const SITE_MESSAGE: &str = "🔐 Area staff SFO. Accedi con il tuo account Supabase.";
const MISSING_DOMAIN: &str = "⚠️ Il dominio pubblico non è configurato. Imposta `EVENT_OVERLAY_PUBLIC_URL` su un'origine HTTPS.";
const PANEL_TITLE: &str = "🛠️ Strumenti staff";
const PANEL_DESCRIPTION: &str = "Apri il sito SFO, consulta i dati staff o controlla l'evento live. Il pannello è visibile solo a te.";
const PANEL_MISSING_LINKS: &str = "\n\n⚠️ Il link ai siti non è configurato: imposta `EVENT_OVERLAY_PUBLIC_URL` su un'origine HTTPS.";

/// `staff:general:center:<utente>` e `staff:general:live:<utente>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneralComponent {
    Center { owner: i64 },
    Live { owner: i64 },
}

impl GeneralComponent {
    pub fn parse(custom_id: &str) -> Option<Self> {
        let rest = custom_id.strip_prefix("staff:general:")?;
        let (kind, owner) = rest.split_once(':')?;
        if owner.is_empty() || !owner.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let owner = owner.parse().ok()?;
        match kind {
            "center" => Some(Self::Center { owner }),
            "live" => Some(Self::Live { owner }),
            _ => None,
        }
    }

    pub fn center_id(owner: i64) -> String {
        format!("staff:general:center:{owner}")
    }

    pub fn live_id(owner: i64) -> String {
        format!("staff:general:live:{owner}")
    }
}

/// Il pulsante che apre un indirizzo.
fn link_button(label: &str, emoji: &str, url: &str) -> CreateButton {
    CreateButton::new_link(url).label(label).emoji(ReactionType::Unicode(emoji.into()))
}

impl<S: DiscordSender, A: DiscordApi> Interactions<S, A> {
    /// Collega il dominio pubblico e la sessione live (quello che in Python è `LiveEventCog`).
    pub fn set_live_state(&self, state: Arc<LiveState>) {
        let _ = self.live.set(state);
    }

    /// L'area staff del sito, vuota se il dominio pubblico non è configurato.
    fn site_url(&self) -> String {
        self.live.get().map(|live| live.public_url.staff_site_url()).unwrap_or_default()
    }

    /// `/site`: il collegamento all'area staff del sito.
    pub async fn command_site<I: InteractionIo>(&self, io: &I) -> Io<()> {
        if !self.is_staff(io) {
            return io.respond(Reply::text(STAFF_ONLY)).await;
        }
        let site_url = self.site_url();
        if site_url.is_empty() {
            return io.respond(Reply::text(MISSING_DOMAIN)).await;
        }
        let row = CreateActionRow::Buttons(vec![link_button("Apri sito SFO", "🌐", &site_url)]);
        io.respond(Reply::text(SITE_MESSAGE).with_components(vec![row])).await
    }

    /// `/general`: il pannello con i collegamenti utili per lo staff.
    pub async fn command_general<I: InteractionIo>(&self, io: &I) -> Io<()> {
        if !self.is_staff(io) {
            return io.respond(Reply::text(STAFF_ONLY)).await;
        }
        let site_url = self.site_url();
        let mut description = PANEL_DESCRIPTION.to_string();
        if site_url.is_empty() {
            description.push_str(PANEL_MISSING_LINKS);
        }
        let embed = CreateEmbed::new().title(PANEL_TITLE).description(description).colour(COLOR_STAFF);

        let owner = io.user_id();
        let mut buttons = vec![
            CreateButton::new(GeneralComponent::center_id(owner))
                .label("Centro staff")
                .emoji(ReactionType::Unicode("🛠️".into()))
                .style(ButtonStyle::Primary),
            CreateButton::new(GeneralComponent::live_id(owner))
                .label("Stato live")
                .emoji(ReactionType::Unicode("🎙️".into()))
                .style(ButtonStyle::Secondary),
        ];
        if !site_url.is_empty() {
            // `site_url.removesuffix("/staff/") + "/community"`
            let origin = site_url.strip_suffix("/staff/").unwrap_or(&site_url);
            buttons.push(link_button("Sito SFO", "🌐", &site_url));
            buttons.push(link_button("Community", "👥", &format!("{origin}/community")));
        }
        io.respond(Reply::embed(embed).with_components(vec![CreateActionRow::Buttons(buttons)])).await
    }

    pub(super) async fn general_component<I: InteractionIo>(&self, io: &I, component: GeneralComponent) -> Io<()> {
        let (GeneralComponent::Center { owner } | GeneralComponent::Live { owner }) = component;
        // `interaction_check` di `GeneralStaffView`: prima il proprietario, poi i permessi.
        if io.user_id() != owner {
            return io.respond(Reply::text(PANEL_OWNER_ONLY)).await;
        }
        if !self.is_staff(io) {
            return io.respond(Reply::text(PANEL_NOT_STAFF_ANYMORE)).await;
        }
        match component {
            GeneralComponent::Center { .. } => self.command_control_center(io).await,
            GeneralComponent::Live { .. } => self.live_status(io).await,
        }
    }

    /// "Stato live": titolo, canale e partecipanti (bot esclusi) dell'evento in corso.
    async fn live_status<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let Some(session) = self.live.get().and_then(|live| live.session()) else {
            return io.respond(Reply::text(NO_LIVE_EVENT)).await;
        };
        let participants = self.services.publisher.api().voice_participants(session.guild_id, session.channel_id);
        io.respond(Reply::text(format!(
            "🎙️ **{}** è attivo in <#{}> con `{participants}` partecipanti.",
            session.title, session.channel_id
        )))
        .await
    }
}
