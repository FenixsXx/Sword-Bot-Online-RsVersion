//! Dashboard pubblica della Game Night: dati, testi, pulsanti e menu (equivalente di `sfo/hub.py` e
//! dei componenti di `sfo/dashboard.py`).
//!
//! **Differenza di forma da Python.** La dashboard Python usa i *Components V2* di Discord
//! (`LayoutView`, `Container`, `Section`, `TextDisplay`). serenity 0.12 (l'ultima versione) non li
//! supporta: anzi, non sa leggere un messaggio che li contiene, quindi i clic sui suoi pulsanti non
//! arriverebbero mai al bot. Qui la stessa scheda è un embed classico con pulsanti e menu
//! tradizionali (è anche il design precedente di Python, `cards.build_dashboard_embed`). I testi sono
//! gli stessi, parola per parola; cambia solo il contenitore.
//!
//! Funzioni staff (`/staff`) restano fuori dalla dashboard pubblica, come in Python.
// Verrà usato dal messaggio della dashboard, dai pulsanti e dai comandi.
#![allow(dead_code)]

use std::collections::HashMap;

use chrono::DateTime;
use chrono_tz::Tz;
use poise::serenity_prelude::{
    CreateActionRow, CreateAutocompleteResponse, CreateInputText, CreateModal, CreateSelectMenu,
    CreateSelectMenuKind, CreateSelectMenuOption, InputTextStyle, ReactionType,
};
use rusqlite::OptionalExtension;

use crate::cards::{
    EmbedSpec, build_combined_hall_of_fame_embed, build_community_records_embed,
    build_contributor_leaderboard_embed, build_leaderboard_embed, build_profile_embed,
};
use crate::config::{COLOR_PRIMARY, COLOR_STAFF, DEFAULT_TIPI};
use crate::database::{self, Connected, DbResult};
use crate::formatting::{casefold, proposal_image_url, proposal_type_emoji, truncate_chars};
use crate::polls::{PollRow, list_active_polls, poll_voter_count};
use crate::queries::{CurrentSuggestion, PollOption, current_suggestions, poll_options, suggestion_supporter_counts, suggestion_supporter_ranking};
use crate::schedule::next_scheduled_reset;
use crate::suggestions::{Recommendation, staff_recommendation};
use crate::ui::{ButtonKind, ViewButton, button_rows};

pub const MAX_TEXT: usize = 3800;
const MEDALS: [&str; 3] = ["🥇", "🥈", "🥉"];

// --- dati e testi della dashboard ------------------------------------------------------------

/// La proposta del momento: la più sostenuta tra quelle in attesa di selezione.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubSpotlight {
    pub name: String,
    pub tipo: String,
    pub supporters: i64,
    pub image: Option<String>,
    pub proposer: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct HubData {
    pub week: i64,
    pub closes_at: DateTime<Tz>,
    pub polls: Vec<PollRow>,
    pub participants: i64,
    pub proposal_count: i64,
    pub spotlight: Option<HubSpotlight>,
    pub community: Option<PollRow>,
    /// Opzioni del ballottaggio, per voti e poi in ordine alfabetico; `None` senza sondaggio community.
    pub ballot: Option<Vec<PollOption>>,
    pub recent: Vec<CurrentSuggestion>,
    pub supporters: HashMap<i64, i64>,
}

pub fn hub_data(db: &impl Connected, now: DateTime<Tz>) -> DbResult<HubData> {
    let week = database::current_week(db)?;
    let polls = list_active_polls(db, None)?;
    let (participants, proposal_count): (i64, i64) = db.with_conn(|connection| -> DbResult<_> {
        let participants = connection.query_row(
            "SELECT COUNT(DISTINCT v.user_id) FROM votes v
             JOIN options o ON o.id=v.option_id
             JOIN polls p ON p.id=o.poll_id WHERE p.active=1",
            [],
            |row| row.get(0),
        )?;
        let approved = connection.query_row(
            "SELECT COUNT(*) FROM suggestions WHERE week=? AND review_status='approved'",
            [week],
            |row| row.get(0),
        )?;
        Ok((participants, approved))
    })?;
    let spotlight = match suggestion_supporter_ranking(db, week, 1, true)?.first() {
        None => None,
        Some(row) => {
            let (link, proposer): (Option<String>, Option<i64>) = db.with_conn(|connection| -> DbResult<_> {
                let link = connection
                    .query_row("SELECT link FROM suggestions WHERE id=?", [row.id], |r| r.get::<_, Option<String>>(0))
                    .optional()?
                    .flatten();
                let proposer = connection
                    .query_row(
                        "SELECT user_id FROM proposals WHERE suggestion_id=? AND week=? ORDER BY id LIMIT 1",
                        [row.id, week],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok((link, proposer))
            })?;
            Some(HubSpotlight {
                name: row.name.clone(),
                tipo: row.tipo.clone(),
                supporters: row.supporters,
                image: proposal_image_url(link.as_deref()),
                proposer,
            })
        }
    };
    let community = polls.iter().find(|poll| poll.is_community.is_some_and(|flag| flag != 0)).cloned();
    let ballot = match &community {
        Some(community) => {
            let mut options = poll_options(db, community.id)?;
            options.sort_by_key(|option| (-option.n, casefold(&option.label)));
            Some(options)
        }
        None => None,
    };
    let recent = current_suggestions(db, 3, false)?;
    let supporters = suggestion_supporter_counts(db, week, &recent.iter().map(|s| s.id).collect::<Vec<_>>())?;
    Ok(HubData {
        week,
        closes_at: next_scheduled_reset(db, now)?,
        polls,
        participants,
        proposal_count,
        spotlight,
        community,
        ballot,
        recent,
        supporters,
    })
}

pub fn ballot_text(data: &HubData) -> String {
    let Some(ballot) = &data.ballot else {
        return "Sondaggio community non configurato.".to_string();
    };
    let Some(first) = ballot.first() else {
        return "Lo staff sta scegliendo le proposte: torna a votare tra poco!".to_string();
    };
    if first.n == 0 {
        return format!("{} proposte pronte · nessun voto ancora: sii il primo! 🗳️", ballot.len());
    }
    ballot
        .iter()
        .take(3)
        .enumerate()
        .map(|(index, option)| format!("{} **{}** · `{} voti`", MEDALS[index], option.label, option.n))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn recent_text(data: &HubData) -> String {
    if data.recent.is_empty() {
        return "Ancora nessuna proposta questa settimana: premi **➕ Proponi** e apri le danze!".to_string();
    }
    data.recent
        .iter()
        .map(|item| {
            format!(
                "{} **{}** · `{} sostenitori`",
                proposal_type_emoji(&item.tipo),
                item.name,
                data.supporters.get(&item.id).copied().unwrap_or(0)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// I blocchi di testo della scheda, nell'ordine e con i limiti della `Container` di Python.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubCard {
    pub header: String,
    pub stats: String,
    /// Testo della proposta del momento e la sua copertina.
    pub spotlight: Option<(String, Option<String>)>,
    pub ballot: String,
    pub recent: String,
}

pub fn hub_card(data: &HubData) -> HubCard {
    let header = "## 🎮 Game Night Hub\nProponi un'attività, vota i sondaggi e sostieni le idee della community.".to_string();
    let stats = format!(
        "**Settimana #{}** · chiusura <t:{}:R>\n👥 `{}` votanti · ✨ `{}` proposte approvate · 🗳️ `{}` sondaggi aperti",
        data.week,
        data.closes_at.timestamp(),
        data.participants,
        data.proposal_count,
        data.polls.len()
    );
    let spotlight = data.spotlight.as_ref().map(|spotlight| {
        let mut text = format!(
            "### ⭐ Proposta del momento\n{} **{}** · `{} sostenitori`",
            proposal_type_emoji(&spotlight.tipo),
            spotlight.name,
            spotlight.supporters
        );
        if let Some(proposer) = spotlight.proposer.filter(|id| *id != 0) {
            text.push_str(&format!("\nProposta da <@{proposer}>"));
        }
        (truncate_chars(&text, 1000), spotlight.image.clone())
    });
    HubCard {
        header,
        stats,
        spotlight,
        ballot: truncate_chars(&format!("### 🏆 Ballottaggio community\n{}", ballot_text(data)), 1200),
        recent: truncate_chars(&format!("### ✨ Proposte recenti\n{}", recent_text(data)), 1200),
    }
}

/// Il testo di un blocco senza la sua intestazione `### …`.
fn without_heading(block: &str) -> &str {
    block.split_once('\n').map_or(block, |(_, rest)| rest)
}

/// La scheda come embed classico: titolo, introduzione e statistiche, poi le tre sezioni.
pub fn hub_embed(card: &HubCard) -> EmbedSpec {
    let intro = without_heading(&card.header);
    let mut embed = EmbedSpec::new("🎮 Game Night Hub", format!("{intro}\n\n{}", card.stats), COLOR_PRIMARY);
    if let Some((text, image)) = &card.spotlight {
        embed = embed.field("⭐ Proposta del momento", truncate_chars(without_heading(text), 1024), false);
        embed.thumbnail = image.clone();
    }
    embed
        .field("🏆 Ballottaggio community", truncate_chars(without_heading(&card.ballot), 1024), false)
        .field("✨ Proposte recenti", truncate_chars(without_heading(&card.recent), 1024), false)
}

// --- pulsanti e menu della dashboard ---------------------------------------------------------

/// Sezioni del menu "Altro…": `(chiave, etichetta, emoji)`.
pub const MORE_SECTIONS: [(&str, &str, &str); 5] = [
    ("leaderboard", "Classifica della settimana", "📊"),
    ("contributors", "Classifica utenti", "🏆"),
    ("history", "Storico e Hall of Fame", "📚"),
    ("records", "Record della community", "🏅"),
    ("profile", "Il mio stato", "👤"),
];

/// Cosa significa il `custom_id` di un componente della dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubComponent {
    Vote,
    Suggest,
    Support,
    More,
    PollSelect,
    StaffProposalSelect,
    StaffPick,
    /// Pulsante "Proponi la prima" quando non ci sono proposte da supportare.
    SuggestFirst,
    /// Menu della scheda di supporto (messaggio privato).
    SupportSelect,
}

impl HubComponent {
    pub const fn custom_id(self) -> &'static str {
        match self {
            HubComponent::Vote => "hub:vote",
            HubComponent::Suggest => "hub:suggest",
            HubComponent::Support => "hub:support",
            HubComponent::More => "hub:more",
            HubComponent::PollSelect => "hub:poll-select",
            HubComponent::StaffProposalSelect => "hub:staff-proposal-select",
            HubComponent::StaffPick => "hub:staff-pick",
            HubComponent::SuggestFirst => "hub:suggest-first",
            HubComponent::SupportSelect => "hub:support-select",
        }
    }

    pub fn parse(custom_id: &str) -> Option<Self> {
        [
            Self::Vote,
            Self::Suggest,
            Self::Support,
            Self::More,
            Self::PollSelect,
            Self::StaffProposalSelect,
            Self::StaffPick,
            Self::SuggestFirst,
            Self::SupportSelect,
        ]
        .into_iter()
        .find(|component| component.custom_id() == custom_id)
    }
}

/// I tre pulsanti sotto la dashboard.
pub fn hub_buttons() -> Vec<ViewButton> {
    vec![
        ViewButton { custom_id: HubComponent::Vote.custom_id().into(), label: "Vota".into(), emoji: Some("🗳️"), kind: ButtonKind::Primary, disabled: false },
        ViewButton { custom_id: HubComponent::Suggest.custom_id().into(), label: "Proponi".into(), emoji: Some("➕"), kind: ButtonKind::Success, disabled: false },
        ViewButton { custom_id: HubComponent::Support.custom_id().into(), label: "Supporta".into(), emoji: Some("👍"), kind: ButtonKind::Success, disabled: false },
    ]
}

fn unicode_emoji(text: &str) -> ReactionType {
    ReactionType::Unicode(text.to_string())
}

/// Il menu "Altro… classifiche, storico, il tuo profilo".
pub fn hub_more_menu() -> CreateSelectMenu {
    let options = MORE_SECTIONS
        .iter()
        .map(|(key, label, emoji)| CreateSelectMenuOption::new(*label, *key).emoji(unicode_emoji(emoji)))
        .collect();
    CreateSelectMenu::new(HubComponent::More.custom_id(), CreateSelectMenuKind::String { options })
        .placeholder("Altro… classifiche, storico, il tuo profilo")
}

/// Righe di componenti della dashboard: i tre pulsanti e il menu.
pub fn hub_components() -> Vec<CreateActionRow> {
    let mut rows = button_rows(&hub_buttons());
    rows.push(CreateActionRow::SelectMenu(hub_more_menu()));
    rows
}

/// L'embed scelto nel menu "Altro…"; `None` per una chiave sconosciuta.
pub fn build_more_section(db: &impl Connected, key: &str, user_id: i64, now: DateTime<Tz>) -> DbResult<Option<EmbedSpec>> {
    Ok(Some(match key {
        "leaderboard" => build_leaderboard_embed(db)?,
        "contributors" => build_contributor_leaderboard_embed(db, 10)?,
        "history" => build_combined_hall_of_fame_embed(db)?,
        "records" => build_community_records_embed(db)?,
        "profile" => build_profile_embed(db, user_id, now)?,
        _ => return Ok(None),
    }))
}

/// Menu "Scegli un sondaggio" (pulsante Vota), al massimo 25 sondaggi.
pub fn dashboard_poll_menu(db: &impl Connected, polls: &[PollRow]) -> DbResult<CreateSelectMenu> {
    let mut options = Vec::new();
    for poll in polls.iter().take(25) {
        let voters = poll_voter_count(db, poll.id)?;
        let mut option = CreateSelectMenuOption::new(truncate_chars(&poll.name, 100), poll.id.to_string()).description(format!("{voters} votanti"));
        if let Some(emoji) = poll.emoji.as_deref().filter(|emoji| !emoji.is_empty() && emoji.chars().count() <= 100) {
            option = option.emoji(unicode_emoji(emoji));
        }
        options.push(option);
    }
    Ok(CreateSelectMenu::new(HubComponent::PollSelect.custom_id(), CreateSelectMenuKind::String { options })
        .placeholder("Scegli un sondaggio")
        .min_values(1)
        .max_values(1))
}

/// Pulsante "Proponi la prima" (nessuna proposta da supportare).
pub fn propose_first_button() -> ViewButton {
    ViewButton { custom_id: HubComponent::SuggestFirst.custom_id().into(), label: "Proponi la prima".into(), emoji: Some("➕"), kind: ButtonKind::Success, disabled: false }
}

// --- modulo "Proponi un'attività" ------------------------------------------------------------

pub const SUGGESTION_MODAL_ID: &str = "hub:suggest-modal";
pub const MODAL_NAME_FIELD: &str = "name";
pub const MODAL_TYPE_FIELD: &str = "tipo";
pub const MODAL_LINK_FIELD: &str = "link";

pub fn suggestion_modal() -> CreateModal {
    CreateModal::new(SUGGESTION_MODAL_ID, "Proponi un'attività").components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Nome del gioco o evento", MODAL_NAME_FIELD)
                .placeholder("Es. Jackbox")
                .min_length(2)
                .max_length(80),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Tipo", MODAL_TYPE_FIELD)
                .placeholder("Gioco, Film, Karaoke o un tipo configurato")
                .max_length(40),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Link (facoltativo)", MODAL_LINK_FIELD)
                .placeholder("https://...")
                .required(false)
                .max_length(300),
        ),
    ])
}

/// Il link del modulo: vuoto significa "nessun link" (`str(self.link) or None`).
pub fn modal_link(raw: &str) -> Option<&str> {
    (!raw.is_empty()).then_some(raw)
}

/// Suggerimenti per il campo "tipo": i tipi configurati che contengono il testo digitato (max 25).
/// Come in Python, qui i tipi non vengono ripuliti dagli spazi.
pub fn tipo_choices(db: &impl Connected, current: &str) -> DbResult<Vec<String>> {
    let configured = database::setting(db, "tipi")?.filter(|text| !text.is_empty()).unwrap_or_else(|| DEFAULT_TIPI.to_string());
    let wanted = current.to_lowercase();
    Ok(configured.split(',').filter(|tipo| tipo.to_lowercase().contains(&wanted)).take(25).map(str::to_string).collect())
}

pub async fn tipo_autocomplete(ctx: crate::Context<'_>, current: &str) -> CreateAutocompleteResponse {
    let choices = tipo_choices(&ctx.data().db, current).unwrap_or_default();
    choices.into_iter().fold(CreateAutocompleteResponse::new(), |response, tipo| response.add_string_choice(tipo.clone(), tipo))
}

// --- scheda di supporto (messaggio privato) --------------------------------------------------

pub const SUPPORT_INTRO: &str = "Puoi sostenere ogni proposta una volta. Il sostegno non consuma le tue proposte settimanali.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportCard {
    /// Testo completo, righe separate da `\n` (la prima è il titolo `## …`).
    pub text: String,
    pub image: Option<String>,
}

/// Scheda di una proposta approvata della settimana; `None` se non c'è o non è approvata.
pub fn support_card(db: &impl Connected, suggestion_id: i64) -> DbResult<Option<SupportCard>> {
    let current_week = database::current_week(db)?;
    let suggestion: Option<(i64, String, String, Option<String>, Option<String>, String)> = db.with_conn(|connection| {
        connection
            .query_row(
                "SELECT week, name, tipo, link, status, review_status FROM suggestions WHERE id=? AND week=?",
                [suggestion_id, current_week],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .optional()
    })?;
    let Some((week, name, tipo, link, status, review_status)) = suggestion else {
        return Ok(None);
    };
    if review_status != "approved" {
        return Ok(None);
    }
    let supporters = suggestion_supporter_counts(db, week, &[suggestion_id])?.get(&suggestion_id).copied().unwrap_or(0);
    let proposer: Option<i64> = db.with_conn(|connection| {
        connection
            .query_row(
                "SELECT user_id FROM proposals WHERE suggestion_id=? AND week=? ORDER BY id LIMIT 1",
                [suggestion_id, week],
                |row| row.get(0),
            )
            .optional()
    })?;
    let mut lines = vec![
        format!("## {} {name}", proposal_type_emoji(&tipo)),
        format!("**{tipo}** · {}", if status.as_deref() == Some("picked") { "🏆 Nel ballottaggio" } else { "In valutazione" }),
        format!("👥 Sostenitori: `{supporters}`"),
    ];
    if let Some(user_id) = proposer {
        lines.push(format!("Proposta da <@{user_id}>"));
    }
    if let Some(link) = link.as_deref().filter(|link| !link.is_empty()) {
        lines.push(format!("[Apri link]({link})"));
    }
    Ok(Some(SupportCard { text: truncate_chars(&lines.join("\n"), 2000), image: proposal_image_url(link.as_deref()) }))
}

/// La scheda come embed classico.
pub fn support_card_embed(card: &SupportCard) -> EmbedSpec {
    let (title, rest) = card.text.split_once('\n').unwrap_or((&card.text, ""));
    let mut embed = EmbedSpec::new(title.trim_start_matches("## "), rest, COLOR_PRIMARY);
    embed.thumbnail = card.image.clone();
    embed
}

pub const SUPPORT_CONFIRM_PREFIX: &str = "hub:support-confirm:";

pub fn support_confirm_custom_id(suggestion_id: i64) -> String {
    format!("{SUPPORT_CONFIRM_PREFIX}{suggestion_id}")
}

/// ID della proposta da `hub:support-confirm:<ID>` (cifre ASCII).
pub fn parse_support_confirm(custom_id: &str) -> Option<i64> {
    let id = custom_id.strip_prefix(SUPPORT_CONFIRM_PREFIX)?;
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then(|| id.parse().ok()).flatten()
}

/// Il messaggio privato del pulsante "Supporta": introduzione, scheda, menu e pulsante.
#[derive(Debug, Clone)]
pub struct SupportMessage {
    pub intro: &'static str,
    /// `None` se la proposta scelta non è più disponibile.
    pub card: Option<EmbedSpec>,
    pub select: CreateSelectMenu,
    pub button: Option<ViewButton>,
}

impl SupportMessage {
    pub fn missing(&self) -> bool {
        self.card.is_none()
    }

    pub fn components(&self) -> Vec<CreateActionRow> {
        let mut rows = vec![CreateActionRow::SelectMenu(self.select.clone())];
        if let Some(button) = &self.button {
            rows.extend(button_rows(std::slice::from_ref(button)));
        }
        rows
    }
}

/// Menu delle proposte da supportare (max 25), con quella scelta preselezionata.
pub fn support_select(db: &impl Connected, suggestions: &[CurrentSuggestion], selected_id: i64) -> DbResult<CreateSelectMenu> {
    let week = database::current_week(db)?;
    let counts = suggestion_supporter_counts(db, week, &suggestions.iter().map(|s| s.id).collect::<Vec<_>>())?;
    let options = suggestions
        .iter()
        .take(25)
        .map(|item| {
            CreateSelectMenuOption::new(truncate_chars(&item.name, 100), item.id.to_string())
                .description(truncate_chars(&format!("{} · {} sostenitori", item.tipo, counts.get(&item.id).copied().unwrap_or(0)), 100))
                .emoji(unicode_emoji(proposal_type_emoji(&item.tipo)))
                .default_selection(item.id == selected_id)
        })
        .collect();
    Ok(CreateSelectMenu::new(HubComponent::SupportSelect.custom_id(), CreateSelectMenuKind::String { options })
        .placeholder("Scegli una proposta da supportare"))
}

pub fn support_message(db: &impl Connected, suggestions: &[CurrentSuggestion], selected_id: i64) -> DbResult<SupportMessage> {
    let card = support_card(db, selected_id)?;
    let button = card.is_some().then(|| ViewButton {
        custom_id: support_confirm_custom_id(selected_id),
        label: "Supporta questa proposta".into(),
        emoji: Some("👍"),
        kind: ButtonKind::Success,
        disabled: false,
    });
    Ok(SupportMessage {
        intro: SUPPORT_INTRO,
        card: card.as_ref().map(support_card_embed),
        select: support_select(db, suggestions, selected_id)?,
        button,
    })
}

// --- pannello staff: selezione delle proposte ------------------------------------------------

/// Il pannello "Selezione proposte" dello staff.
#[derive(Debug, Clone)]
pub struct StaffPanel {
    pub embed: EmbedSpec,
    /// Proposte in attesa, la più consigliata per prima.
    pub suggestions: Vec<CurrentSuggestion>,
    pub recommended_id: Option<i64>,
    pub scores: HashMap<i64, Recommendation>,
}

/// Costruisce il pannello (in attesa di selezione, max 25); `None` se non c'è nulla da scegliere.
/// `now` è un timestamp Unix.
pub fn build_staff_panel(db: &impl Connected, now: i64) -> DbResult<Option<StaffPanel>> {
    let mut suggestions = current_suggestions(db, 25, true)?;
    if suggestions.is_empty() {
        return Ok(None);
    }
    let (recommended_id, scores) = staff_recommendation(db, &suggestions, now)?;
    // Per punteggio, poi sostenitori, poi attesa: dal più alto; a parità resta l'ordine di partenza.
    let key = |suggestion: &CurrentSuggestion| {
        let score = &scores[&suggestion.id];
        (score.score, score.supporters, score.waiting_days)
    };
    suggestions.sort_by(|a, b| key(b).cmp(&key(a)));
    let lines: Vec<String> = suggestions
        .iter()
        .take(10)
        .map(|suggestion| {
            let score = &scores[&suggestion.id];
            format!(
                "{}{} **{}**\n└ `{} sostenitori` · `{} gg`{}",
                if Some(suggestion.id) == recommended_id { "⭐ " } else { "• " },
                proposal_type_emoji(&suggestion.tipo),
                suggestion.name,
                score.supporters,
                score.waiting_days,
                if score.won_recently { " · vittoria recente: −25" } else { "" }
            )
        })
        .collect();
    let embed = EmbedSpec::new(
        "🛠️ Selezione proposte",
        format!(
            "⭐ Consigliata: più sostenitori e tempo in attesa; le proposte vincitrici nelle ultime 4 settimane ricevono una penalità. Il suggerimento non seleziona automaticamente.\n\n{}",
            lines.join("\n")
        ),
        COLOR_STAFF,
    );
    Ok(Some(StaffPanel { embed, suggestions, recommended_id, scores }))
}

/// Menu del pannello staff.
pub fn staff_proposal_menu(db: &impl Connected, panel: &StaffPanel) -> DbResult<CreateSelectMenu> {
    let week = database::current_week(db)?;
    let counts = suggestion_supporter_counts(db, week, &panel.suggestions.iter().map(|s| s.id).collect::<Vec<_>>())?;
    let options = panel
        .suggestions
        .iter()
        .map(|suggestion| {
            let description = format!(
                "{}{} {} · {} sostenitori",
                if Some(suggestion.id) == panel.recommended_id { "⭐ Consigliata · " } else { "" },
                proposal_type_emoji(&suggestion.tipo),
                suggestion.tipo,
                counts.get(&suggestion.id).copied().unwrap_or(0)
            );
            CreateSelectMenuOption::new(truncate_chars(&suggestion.name, 100), suggestion.id.to_string())
                .description(truncate_chars(&description, 100))
                .emoji(unicode_emoji(proposal_type_emoji(&suggestion.tipo)))
        })
        .collect();
    Ok(CreateSelectMenu::new(HubComponent::StaffProposalSelect.custom_id(), CreateSelectMenuKind::String { options })
        .placeholder("Scegli una proposta da inserire nel ballottaggio")
        .min_values(1)
        .max_values(1))
}

pub const STAFF_PICK_PREFIX: &str = "hub:staff-pick:";

/// Pulsante "Aggiungi al ballottaggio" dopo la scelta: l'ID della proposta sta nella `custom_id`
/// (in Python lo ricordava la `View` in memoria, che qui non esiste).
pub fn staff_pick_selected_button(suggestion_id: i64) -> ViewButton {
    ViewButton {
        custom_id: format!("{STAFF_PICK_PREFIX}{suggestion_id}"),
        label: "Aggiungi al ballottaggio".into(),
        emoji: Some("✅"),
        kind: ButtonKind::Success,
        disabled: false,
    }
}

/// ID della proposta da `hub:staff-pick:<ID>` (cifre ASCII).
pub fn parse_staff_pick(custom_id: &str) -> Option<i64> {
    let id = custom_id.strip_prefix(STAFF_PICK_PREFIX)?;
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then(|| id.parse().ok()).flatten()
}

/// Pulsante "Aggiungi al ballottaggio": disabilitato finché non si sceglie una proposta.
pub fn staff_pick_button(enabled: bool) -> ViewButton {
    ViewButton {
        custom_id: HubComponent::StaffPick.custom_id().into(),
        label: "Aggiungi al ballottaggio".into(),
        emoji: Some("✅"),
        kind: ButtonKind::Success,
        disabled: !enabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::database::Database;
    use chrono::TimeZone;

    fn now() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()
    }

    fn db() -> Database {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, now()).unwrap();
        db
    }

    /// I blocchi di testo della scheda nella forma della `Container` di Python.
    fn card_json(card: &HubCard) -> serde_json::Value {
        use serde_json::json;
        let mut items = vec![json!(["text", card.header]), json!(["separator"]), json!(["text", card.stats])];
        match &card.spotlight {
            Some((text, Some(image))) => items.push(json!(["section", text, image])),
            Some((text, None)) => items.push(json!(["text", text])),
            None => {}
        }
        items.push(json!(["text", card.ballot]));
        items.push(json!(["text", card.recent]));
        serde_json::Value::Array(items)
    }

    fn options_json(menu: &CreateSelectMenu) -> serde_json::Value {
        use serde_json::json;
        let menu = serde_json::to_value(menu).unwrap();
        serde_json::Value::Array(
            menu["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| json!({
                    "label": o["label"],
                    "value": o["value"],
                    "description": o["description"],
                    "emoji": o["emoji"]["name"],
                    "default": o["default"].as_bool().unwrap_or(false),
                }))
                .collect(),
        )
    }

    /// Testi, menu e pannello staff costruiti dal vero Python sulla stessa fixture di `cards`.
    #[test]
    fn hub_matches_the_python_implementation() {
        use serde_json::{Value, json};

        let golden: Value = serde_json::from_str(include_str!("golden/hub_expected.json")).unwrap();
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, now()).unwrap();
        db.execute_batch(include_str!("golden/cards_fixture.sql")).unwrap();

        // --- scheda della dashboard
        let data = hub_data(&db, now()).unwrap();
        assert_eq!(card_json(&hub_card(&data)), golden["hub_card"], "scheda della dashboard");
        assert_eq!(json!(ballot_text(&data)), golden["ballot_text"]);
        assert_eq!(json!(recent_text(&data)), golden["recent_text"]);

        // --- scheda di supporto per ogni proposta
        let suggestions = current_suggestions(&db, 25, false).unwrap();
        assert_eq!(json!(suggestions.iter().map(|s| s.id).collect::<Vec<_>>()), golden["support_list_ids"]);
        for selected in [1, 2, 3, 4, 5, 6, 7, 999] {
            let card = support_card(&db, selected).unwrap();
            let container = card.as_ref().map(|card| {
                let body = match &card.image {
                    Some(image) => json!(["section", card.text, image]),
                    None => json!(["text", card.text]),
                };
                json!([["text", SUPPORT_INTRO], ["separator"], body])
            });
            let message = support_message(&db, &suggestions, selected).unwrap();
            let menu = serde_json::to_value(&message.select).unwrap();
            let actual = json!({
                "missing": message.missing(),
                "container": container,
                "options": options_json(&message.select),
                "placeholder": menu["placeholder"],
                "button": message.button.as_ref().map(|b| b.label.clone()),
            });
            assert_eq!(actual, golden["support"][selected.to_string()], "supporto {selected}");
        }

        // --- menu del pulsante "Vota"
        let polls = list_active_polls(&db, Some(25)).unwrap();
        let poll_menu = dashboard_poll_menu(&db, &polls).unwrap();
        assert_eq!(
            json!({"placeholder": serde_json::to_value(&poll_menu).unwrap()["placeholder"], "options": options_json(&poll_menu)}),
            golden["poll_select"]
        );

        // --- pannello staff
        let panel = build_staff_panel(&db, 1_800_000_000).unwrap().expect("ci sono proposte in attesa");
        let staff_menu = staff_proposal_menu(&db, &panel).unwrap();
        let pick = staff_pick_button(false);
        let actual = json!({
            "title": panel.embed.title,
            "description": panel.embed.description,
            "color": panel.embed.color,
            "options": options_json(&staff_menu),
            "placeholder": serde_json::to_value(&staff_menu).unwrap()["placeholder"],
            "pick": [pick.label, pick.emoji, pick.disabled, pick.custom_id],
        });
        assert_eq!(actual, golden["staff_panel"], "pannello staff");

        // --- varianti limite, stesse modifiche di Python nello stesso ordine
        let snap = |db: &Database| {
            let data = hub_data(db, now()).unwrap();
            json!({"card": card_json(&hub_card(&data)), "ballot": ballot_text(&data), "recent": recent_text(&data)})
        };
        db.execute("UPDATE suggestions SET link='https://store.steampowered.com/app/570/' WHERE id=4", []).unwrap();
        assert_eq!(snap(&db), golden["variants"]["spotlight_image"], "spotlight con copertina");
        db.execute("DELETE FROM votes WHERE option_id IN (100,101,102,103)", []).unwrap();
        assert_eq!(snap(&db), golden["variants"]["no_votes"], "ballottaggio senza voti");
        db.execute("DELETE FROM options WHERE poll_id=5", []).unwrap();
        assert_eq!(snap(&db), golden["variants"]["empty_ballot"], "ballottaggio vuoto");
        db.execute("UPDATE polls SET is_community=0 WHERE id=5", []).unwrap();
        assert_eq!(snap(&db), golden["variants"]["no_community"], "nessun sondaggio community");
        db.execute("UPDATE suggestions SET review_status='pending'", []).unwrap();
        assert_eq!(snap(&db), golden["variants"]["nothing_approved"], "nessuna proposta approvata");
    }

    #[test]
    fn custom_ids_round_trip_and_stay_unique() {
        let all = [
            HubComponent::Vote,
            HubComponent::Suggest,
            HubComponent::Support,
            HubComponent::More,
            HubComponent::PollSelect,
            HubComponent::StaffProposalSelect,
            HubComponent::StaffPick,
            HubComponent::SuggestFirst,
            HubComponent::SupportSelect,
        ];
        let ids: std::collections::HashSet<&str> = all.iter().map(|c| c.custom_id()).collect();
        assert_eq!(ids.len(), all.len());
        for component in all {
            assert_eq!(HubComponent::parse(component.custom_id()), Some(component));
        }
        assert_eq!(HubComponent::parse("hub:"), None);
        assert_eq!(HubComponent::parse("vote:1"), None);
        // Gli ID dei pulsanti persistenti sono quelli di Python: i vecchi messaggi continuano a funzionare.
        assert_eq!(
            [HubComponent::Vote, HubComponent::Suggest, HubComponent::Support, HubComponent::More].map(HubComponent::custom_id),
            ["hub:vote", "hub:suggest", "hub:support", "hub:more"]
        );
        assert_eq!(parse_support_confirm(&support_confirm_custom_id(42)), Some(42));
        assert_eq!(parse_support_confirm("hub:support-confirm:x"), None);
        assert_eq!(parse_support_confirm("hub:support-confirm:"), None);
    }

    #[test]
    fn components_have_three_buttons_and_the_more_menu() {
        let rows = serde_json::to_value(hub_components()).unwrap();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        let buttons = rows[0]["components"].as_array().unwrap();
        assert_eq!(
            buttons.iter().map(|b| (b["custom_id"].as_str().unwrap(), b["label"].as_str().unwrap(), b["style"].as_u64().unwrap())).collect::<Vec<_>>(),
            [("hub:vote", "Vota", 1), ("hub:suggest", "Proponi", 3), ("hub:support", "Supporta", 3)]
        );
        let menu = &rows[1]["components"][0];
        assert_eq!(menu["custom_id"], "hub:more");
        assert_eq!(menu["placeholder"], "Altro… classifiche, storico, il tuo profilo");
        let options = menu["options"].as_array().unwrap();
        assert_eq!(options.iter().map(|o| o["value"].as_str().unwrap()).collect::<Vec<_>>(), ["leaderboard", "contributors", "history", "records", "profile"]);
        assert_eq!(options[4]["label"], "Il mio stato");
        assert_eq!(options[0]["emoji"]["name"], "📊");
    }

    #[test]
    fn modal_matches_the_python_form() {
        let json = serde_json::to_value(suggestion_modal()).unwrap();
        assert_eq!((json["custom_id"].as_str(), json["title"].as_str()), (Some("hub:suggest-modal"), Some("Proponi un'attività")));
        let inputs: Vec<&serde_json::Value> = json["components"].as_array().unwrap().iter().map(|row| &row["components"][0]).collect();
        assert_eq!(inputs[0]["label"], "Nome del gioco o evento");
        assert_eq!((inputs[0]["min_length"].as_u64(), inputs[0]["max_length"].as_u64()), (Some(2), Some(80)));
        assert_eq!(inputs[1]["label"], "Tipo");
        assert_eq!(inputs[1]["max_length"], 40);
        assert_eq!(inputs[2]["required"], false);
        assert_eq!(inputs[2]["max_length"], 300);
        assert!(inputs.iter().all(|input| input["style"] == 1), "campi su una riga");
        assert_eq!(modal_link(""), None);
        assert_eq!(modal_link("https://x.test"), Some("https://x.test"));
    }

    #[test]
    fn tipo_choices_filter_and_do_not_trim_like_python() {
        let db = db();
        assert_eq!(tipo_choices(&db, "").unwrap(), ["Gioco", "Film", "Karaoke", "Altro"]);
        assert_eq!(tipo_choices(&db, "FI").unwrap(), ["Film"]);
        assert_eq!(tipo_choices(&db, "o").unwrap(), ["Gioco", "Karaoke", "Altro"], "\"Film\" non contiene la o");
        db.set_setting("tipi", "Quiz, Serata ,Altro").unwrap();
        assert_eq!(tipo_choices(&db, "se").unwrap(), [" Serata "], "gli spazi restano (come in Python)");
        db.set_setting("tipi", "").unwrap();
        assert_eq!(tipo_choices(&db, "gio").unwrap(), ["Gioco"], "vuoto: i predefiniti");
        let many = (1..=40).map(|n| format!("T{n}")).collect::<Vec<_>>().join(",");
        db.set_setting("tipi", many).unwrap();
        assert_eq!(tipo_choices(&db, "T").unwrap().len(), 25);
    }

    #[test]
    fn more_sections_dispatch_to_the_cards() {
        let db = db();
        for (key, _, _) in MORE_SECTIONS {
            assert!(build_more_section(&db, key, 1, now()).unwrap().is_some(), "{key}");
        }
        assert!(build_more_section(&db, "boh", 1, now()).unwrap().is_none());
        let profile = build_more_section(&db, "profile", 42, now()).unwrap().unwrap();
        assert_eq!(profile.title.as_deref(), Some("👤 Il mio stato"));
    }

    #[test]
    fn staff_panel_is_empty_without_pending_suggestions() {
        let db = db();
        assert!(build_staff_panel(&db, 0).unwrap().is_none());
        assert!(staff_pick_button(false).disabled);
        assert!(!staff_pick_button(true).disabled);
    }

    #[test]
    fn hub_data_for_an_empty_server_uses_friendly_texts() {
        let db = db();
        let data = hub_data(&db, now()).unwrap();
        assert_eq!((data.participants, data.proposal_count), (0, 0));
        assert_eq!(data.spotlight, None);
        assert_eq!(ballot_text(&data), "Lo staff sta scegliendo le proposte: torna a votare tra poco!");
        assert_eq!(recent_text(&data), "Ancora nessuna proposta questa settimana: premi **➕ Proponi** e apri le danze!");
        let card = hub_card(&data);
        assert!(card.spotlight.is_none());
        assert!(card.stats.contains("`5` sondaggi aperti"), "{}", card.stats);
        let embed = hub_embed(&card);
        assert_eq!(embed.title.as_deref(), Some("🎮 Game Night Hub"));
        assert_eq!(embed.fields.len(), 2, "senza proposta del momento: solo ballottaggio e recenti");
    }
}
