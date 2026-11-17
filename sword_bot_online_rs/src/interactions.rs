//! Smistamento dei clic su pulsanti, menu e maschere (i callback dei `discord.ui` di Python:
//! `VoteButton`, `MyVotesButton`, `MyVotesSelect`, `ReviewButton`, `RejectModal`, `HubActions`,
//! `HubMoreSelect`, `DashboardPollSelect`, `StaffPanelView`, `SupportLayout`, `SuggestionModal` e
//! le viste di `proposal_flow.py`).
//!
//! In Python le viste ricordano il loro stato in memoria (proposta scelta dallo staff, "Intendevi…?",
//! consenso ai DM). Qui lo stato sta nei `custom_id` o, per la richiesta "Intendevi…?" (nome, tipo e
//! link non starebbero in 100 caratteri), in una piccola mappa temporanea di 3 minuti — come il
//! timeout di 180 secondi della vista Python, che non sopravvive nemmeno lei a un riavvio.
//!
//! L'interazione Discord è dietro [`InteractionIo`]: i gestori si provano con un finto, e gli
//! adattatori per serenity (in fondo) fanno da ponte con l'API vera.
// Collegato all'`event_handler` di poise in `main`.
#![allow(dead_code)]

mod admin;
mod backup_commands;
mod commands;
mod community_commands;
mod live_commands;
mod poll_commands;
mod proposals;
mod staff_site_commands;

pub use admin::{ConfigRequest, ControlComponent, PROPOSAL_MODAL_ID};
pub use live_commands::LiveReportComponent;
pub use staff_site_commands::GeneralComponent;
pub use poll_commands::{CreatePollRequest, option_choices, poll_choices};
#[cfg(test)]
pub use poll_commands::{valid_color, valid_emoji};

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::DateTime;
use chrono_tz::Tz;
use poise::serenity_prelude::{
    self as serenity, ActionRowComponent, ComponentInteraction, ComponentInteractionDataKind, CreateActionRow,
    CreateAllowedMentions, CreateEmbed, CreateInteractionResponse, CreateInteractionResponseFollowup,
    CreateInteractionResponseMessage, CreateModal, EditMessage, ModalInteraction,
};
use serde_json::Value;
use tracing::{error, warn};

use crate::cards::build_embed;
use crate::database::Database;
use crate::discord_api::DiscordApi;
use crate::flows::Services;
use crate::hooks::publish_site_change;
use crate::hub::{
    HubComponent, build_more_section, build_staff_panel, dashboard_poll_menu, modal_link, parse_staff_pick, parse_support_confirm, propose_first_button, staff_pick_button,
    staff_pick_selected_button, staff_proposal_menu, suggestion_modal, SUGGESTION_MODAL_ID, MODAL_LINK_FIELD,
    MODAL_NAME_FIELD, MODAL_TYPE_FIELD,
};
use crate::notify::DiscordSender;
use crate::permissions::is_staff_member;
use crate::polls::{PollComponent, build_view, find_poll, list_active_polls, my_votes_menu, set_user_votes, toggle_vote, vote_summary};
use crate::publisher::PublisherEffects;
use crate::queries::{current_suggestions, poll_options};
use crate::review::{
    REJECT_REASON_FIELD_ID, ReviewAction, ReviewRequest, ReviewSource, apply_review, parse_reject_modal_id,
    parse_review_custom_id, reject_modal,
};
use crate::settings;
use crate::suggestions::{
    SimilarSuggestion, has_notification_preference, register_suggestion, register_suggestion_support,
    set_dm_notifications, similar_suggestions,
};
use crate::ui::{ButtonKind, ViewButton, button_rows};
use crate::v2::{notice_components, support_v2};

// --- l'interazione come la vedono i gestori ---------------------------------------------------

/// Errore nel parlare con Discord (risposta rifiutata, interazione scaduta…).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoError(pub String);

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for IoError {}

/// Contenuto di una risposta (sempre visibile solo a chi ha cliccato).
///
/// Per `update` un campo `None` significa "lascia com'è" e un valore vuoto (`Some("")`,
/// `Some(vec![])`) "togli".
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub content: Option<String>,
    pub embeds: Option<Vec<CreateEmbed>>,
    pub components: Option<Vec<CreateActionRow>>,
    /// Componenti V2 grezzi (la risposta non può avere altro).
    pub v2: Option<Vec<Value>>,
}

impl Reply {
    pub fn text(content: impl Into<String>) -> Self {
        Self { content: Some(content.into()), ..Self::default() }
    }

    pub fn embed(embed: CreateEmbed) -> Self {
        Self { embeds: Some(vec![embed]), ..Self::default() }
    }

    pub fn v2(components: Vec<Value>) -> Self {
        Self { v2: Some(components), ..Self::default() }
    }

    pub fn with_components(mut self, components: Vec<CreateActionRow>) -> Self {
        self.components = Some(components);
        self
    }

    /// Toglie i pulsanti dal messaggio (`view=None`).
    pub fn without_components(mut self) -> Self {
        self.components = Some(Vec::new());
        self
    }

    /// Toglie anche embed e contenuto vecchi (`content=...`, `embed=None`).
    pub fn replacing_embeds(mut self) -> Self {
        self.embeds = Some(Vec::new());
        self
    }
}

type Io<T> = Result<T, IoError>;

/// Un clic (o una maschera inviata) e i modi di rispondere.
pub trait InteractionIo: Send + Sync {
    fn custom_id(&self) -> &str;
    fn user_id(&self) -> i64;
    /// Nome utente di chi ha cliccato (`str(interaction.user)`).
    fn user_name(&self) -> String;
    /// Valori scelti in un menu.
    fn values(&self) -> &[String];
    /// Testo inserito in un campo di una maschera.
    fn modal_field(&self, field_id: &str) -> Option<String>;
    /// Testo del messaggio su cui si è cliccato.
    fn message_content(&self) -> Option<String>;
    fn guild_id(&self) -> Option<u64>;
    fn is_admin(&self) -> bool;
    fn role_ids(&self) -> Vec<u64>;

    /// Conferma subito la ricezione ("sta pensando…"): poi si risponde con `followup`.
    fn defer(&self) -> impl Future<Output = Io<()>> + Send;
    /// Nuovo messaggio privato come risposta.
    fn respond(&self, reply: Reply) -> impl Future<Output = Io<()>> + Send;
    /// Nuovo messaggio visibile a tutti nel canale (`send_message` senza `ephemeral`).
    fn respond_public(&self, reply: Reply) -> impl Future<Output = Io<()>> + Send;
    /// Sostituisce il messaggio su cui si è cliccato.
    fn update(&self, reply: Reply) -> impl Future<Output = Io<()>> + Send;
    /// Messaggio privato dopo un `defer`.
    fn followup(&self, reply: Reply) -> impl Future<Output = Io<()>> + Send;
    fn show_modal(&self, modal: CreateModal) -> impl Future<Output = Io<()>> + Send;
    /// Toglie i pulsanti dal messaggio originale (scheda di revisione di una proposta sparita).
    fn strip_source_components(&self) -> impl Future<Output = Io<()>> + Send;
    /// Cambia il testo del messaggio da cui è partita l'interazione e ne toglie i pulsanti
    /// (`message.edit(content=..., view=None)`); gli embed restano. Senza messaggio non fa nulla.
    fn edit_source_message(&self, content: String) -> impl Future<Output = Io<()>> + Send;
    /// Modifica la risposta già data all'interazione (`edit_original_response`).
    fn edit_response(&self, reply: Reply) -> impl Future<Output = Io<()>> + Send;
    /// Messaggio privato con un file allegato, dopo un `defer`.
    fn followup_file(&self, content: String, filename: String, data: Vec<u8>) -> impl Future<Output = Io<()>> + Send;
    /// Il membro scelto in un menu di utenti.
    fn selected_user(&self) -> Option<crate::control_center::MemberInfo> {
        None
    }
}

// --- richiesta "Intendevi…?" -----------------------------------------------------------------

/// Quanto vive una richiesta "Intendevi…?" (come `timeout=180` della vista Python).
pub const PENDING_TTL: Duration = Duration::from_secs(180);

#[derive(Debug, Clone)]
struct Pending {
    owner: i64,
    nome: String,
    tipo: String,
    link: Option<String>,
    similar: Vec<SimilarSuggestion>,
    created: Instant,
}

/// I componenti del flusso di invio proposta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowComponent {
    /// `flow:sim:<richiesta>:<indice>`: sostieni/unisciti a una proposta simile.
    Similar { nonce: u64, index: usize },
    /// `flow:create:<richiesta>`: "È diversa, creala".
    Create { nonce: u64 },
    /// `flow:optin:yes|no:<utente>`: consenso ai messaggi privati.
    OptIn { enable: bool, owner: i64 },
}

impl FlowComponent {
    pub fn parse(custom_id: &str) -> Option<Self> {
        let number = |text: &str| -> Option<u64> {
            (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())).then(|| text.parse().ok()).flatten()
        };
        let rest = custom_id.strip_prefix("flow:")?;
        let mut parts = rest.split(':');
        match (parts.next()?, parts.next(), parts.next(), parts.next()) {
            ("sim", Some(nonce), Some(index), None) => Some(Self::Similar { nonce: number(nonce)?, index: usize::try_from(number(index)?).ok()? }),
            ("create", Some(nonce), None, None) => Some(Self::Create { nonce: number(nonce)? }),
            ("optin", Some(choice @ ("yes" | "no")), Some(owner), None) => {
                Some(Self::OptIn { enable: choice == "yes", owner: i64::try_from(number(owner)?).ok()? })
            }
            _ => None,
        }
    }

    pub fn similar_id(nonce: u64, index: usize) -> String {
        format!("flow:sim:{nonce}:{index}")
    }

    pub fn create_id(nonce: u64) -> String {
        format!("flow:create:{nonce}")
    }

    pub fn opt_in_id(enable: bool, owner: i64) -> String {
        format!("flow:optin:{}:{owner}", if enable { "yes" } else { "no" })
    }
}

/// La domanda sul consenso ai DM in coda alla conferma (serve anche a ritrovare la conferma).
const OPT_IN_QUESTION: &str = "\n\n🔔 Vuoi un messaggio privato quando la tua proposta viene approvata, entra nel ballottaggio o vince?";

// --- il gestore ------------------------------------------------------------------------------

pub struct Interactions<S: DiscordSender, A: DiscordApi> {
    pub services: Services<S, A>,
    clock: fn() -> DateTime<Tz>,
    pending: Mutex<HashMap<u64, Pending>>,
    pending_ttl: Duration,
    next_nonce: AtomicU64,
    /// Richieste di conferma dei comandi (`/rimuovi-scelta`, `/unisci-proposte`, `/reset-manuale`).
    confirmations: Mutex<HashMap<u64, commands::Confirmation>>,
    confirm_ttl: Duration,
    /// Il servizio del reset, per `/reset-manuale` (si imposta all'avvio con [`Self::set_reset_service`]).
    reset: std::sync::OnceLock<std::sync::Arc<crate::reset::ResetService<S, A>>>,
    /// Il servizio dei backup, per `/backup` (si imposta all'avvio con [`Self::set_backup_service`]).
    backup: std::sync::OnceLock<std::sync::Arc<crate::backups::BackupService>>,
    /// Dominio pubblico e sessione live, per `/site` e `/general` (si imposta con [`Self::set_live_state`]).
    live: std::sync::OnceLock<std::sync::Arc<crate::overlay::LiveState>>,
    /// Avvio e fine dell'evento live, per `/evento avvia|collega|termina` (si imposta con [`Self::set_live_controller`]).
    live_controller: std::sync::OnceLock<std::sync::Arc<dyn crate::live_service::LiveController>>,
    /// Le liste aperte con `/proposte` (pagine e indice), per 5 minuti.
    paginators: Mutex<HashMap<u64, proposals::Paginator>>,
    paginator_ttl: Duration,
}

impl<S: DiscordSender, A: DiscordApi> Interactions<S, A> {
    pub fn new(services: Services<S, A>) -> Self {
        Self::with_clock(services, crate::schedule::now)
    }

    pub fn with_clock(services: Services<S, A>, clock: fn() -> DateTime<Tz>) -> Self {
        // Numeri di richiesta non indovinabili e diversi a ogni avvio.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |elapsed| elapsed.as_nanos() as u64)
            & 0x7FFF_FFFF_FFFF;
        Self { services, clock, pending: Mutex::new(HashMap::new()), pending_ttl: PENDING_TTL,
            next_nonce: AtomicU64::new(seed),
            confirmations: Mutex::new(HashMap::new()),
            confirm_ttl: commands::CONFIRM_TTL,
            reset: std::sync::OnceLock::new(),
            backup: std::sync::OnceLock::new(),
            live: std::sync::OnceLock::new(),
            live_controller: std::sync::OnceLock::new(),
            paginators: Mutex::new(HashMap::new()),
            paginator_ttl: proposals::PAGINATOR_TTL,
        }
    }

    /// Collega il servizio del reset (serve a `/reset-manuale`).
    pub fn set_reset_service(&self, service: std::sync::Arc<crate::reset::ResetService<S, A>>) {
        let _ = self.reset.set(service);
    }

    fn db(&self) -> &Database {
        &self.services.db
    }

    fn now(&self) -> DateTime<Tz> {
        (self.clock)()
    }

    fn effects(&self) -> PublisherEffects<A> {
        PublisherEffects { publisher: std::sync::Arc::clone(&self.services.publisher), scheduler: self.services.scheduler.clone() }
    }

    /// Permessi staff dell'autore del clic: amministratore, oppure un ruolo configurato.
    fn is_staff<I: InteractionIo>(&self, io: &I) -> bool {
        let setting = self.db().get_setting_or("staff_roles", "").unwrap_or_default();
        is_staff_member(io.is_admin(), &setting, io.guild_id(), io.role_ids())
    }

    fn after_vote_change(&self, poll_id: i64) {
        self.services.publisher.schedule_poll_refresh(&self.services.scheduler, poll_id);
        self.services.publisher.schedule_dashboard_refresh(&self.services.scheduler);
        publish_site_change("polls");
    }

    fn refresh_dashboard_and_notify_site(&self) {
        self.services.publisher.schedule_dashboard_refresh(&self.services.scheduler);
        publish_site_change("proposals");
    }

    /// Smista un clic o una maschera in base al `custom_id`. Gli ID sconosciuti si ignorano
    /// (messaggi di altri moduli, o pulsanti che questo bot non gestisce).
    pub async fn handle<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let id = io.custom_id().to_string();
        if let Some(component) = PollComponent::parse(&id) {
            return match component {
                PollComponent::Vote { option_id } => self.vote_button(io, option_id).await,
                PollComponent::MyVotes { poll_id } => self.my_votes_button(io, poll_id).await,
                PollComponent::MyVotesSelect { poll_id } => self.my_votes_select(io, poll_id).await,
            };
        }
        if let Some((action, suggestion_id)) = parse_review_custom_id(&id) {
            return self.review_button(io, action, suggestion_id).await;
        }
        if let Some(suggestion_id) = parse_reject_modal_id(&id) {
            return self.reject_modal_submitted(io, suggestion_id).await;
        }
        if id == SUGGESTION_MODAL_ID {
            return self.suggestion_modal_submitted(io).await;
        }
        if let Some(component) = proposals::PageComponent::parse(&id) {
            return self.page_component(io, component).await;
        }
        if let Some(component) = ControlComponent::parse(&id) {
            return self.control_component(io, component).await;
        }
        if id == PROPOSAL_MODAL_ID {
            return self.proposal_search_submitted(io).await;
        }
        if let Some(component) = commands::ConfirmComponent::parse(&id) {
            return self.confirm_component(io, component).await;
        }
        if let Some(component) = commands::StaffToolsComponent::parse(&id) {
            return self.staff_tools_component(io, component).await;
        }
        if let Some(component) = LiveReportComponent::parse(&id) {
            return self.live_report_component(io, component).await;
        }
        if let Some(component) = GeneralComponent::parse(&id) {
            return self.general_component(io, component).await;
        }
        if let Some(week) = crate::gamenight::parse_create_button(&id) {
            let is_staff = self.is_staff(io);
            return crate::gamenight::create_button_clicked(self.db(), io, week, is_staff, self.now()).await;
        }
        if let Some(week) = crate::gamenight::parse_event_modal(&id) {
            return crate::gamenight::event_modal_submitted(&self.services, io, week, self.now()).await;
        }
        if let Some(suggestion_id) = parse_support_confirm(&id) {
            return self.support_confirm(io, suggestion_id).await;
        }
        if let Some(suggestion_id) = parse_staff_pick(&id) {
            return self.staff_pick(io, Some(suggestion_id)).await;
        }
        if let Some(component) = HubComponent::parse(&id) {
            return match component {
                HubComponent::Vote => self.hub_vote(io).await,
                HubComponent::Suggest | HubComponent::SuggestFirst => io.show_modal(suggestion_modal()).await,
                HubComponent::Support => self.hub_support(io).await,
                HubComponent::More => self.hub_more(io).await,
                HubComponent::PollSelect => self.hub_poll_select(io).await,
                HubComponent::SupportSelect => self.support_select(io).await,
                HubComponent::StaffProposalSelect => self.staff_proposal_select(io).await,
                HubComponent::StaffPick => self.staff_pick(io, None).await,
            };
        }
        if let Some(component) = FlowComponent::parse(&id) {
            return self.flow_component(io, component).await;
        }
        Ok(())
    }

    // --- voti ---

    async fn vote_button<I: InteractionIo>(&self, io: &I, option_id: i64) -> Io<()> {
        io.defer().await?;
        let result = match toggle_vote(self.db(), io.user_id(), option_id, self.now()) {
            Ok(result) => result,
            Err(e) => {
                error!("Errore SQLite durante la registrazione di un voto: {e}");
                return io.followup(Reply::text("❌ Non è stato possibile registrare il voto. Riprova tra poco.")).await;
            }
        };
        let mut message = result.message;
        if let Some(poll_id) = result.poll_id {
            message.push('\n');
            message.push_str(&vote_summary(self.db(), poll_id, io.user_id()).unwrap_or_default());
        }
        io.followup(Reply::text(message)).await?;
        if let (true, Some(poll_id)) = (result.changed, result.poll_id) {
            self.after_vote_change(poll_id);
        }
        Ok(())
    }

    async fn my_votes_button<I: InteractionIo>(&self, io: &I, poll_id: i64) -> Io<()> {
        let poll = find_poll(self.db(), poll_id).ok().flatten().filter(|poll| poll.is_active());
        let Some(poll) = poll else {
            return io.respond(Reply::text("⚠️ Questo sondaggio non è attivo.")).await;
        };
        if poll_options(self.db(), poll_id).map_or(true, |options| options.is_empty()) {
            return io.respond(Reply::text("ℹ️ Non ci sono ancora opzioni in questo sondaggio.")).await;
        }
        let max_votes = settings::max_votes(self.db()).unwrap_or(2);
        let summary = vote_summary(self.db(), poll_id, io.user_id()).unwrap_or_default();
        let menu = match my_votes_menu(self.db(), poll_id, io.user_id()) {
            Ok(menu) => menu,
            Err(e) => {
                error!("Impossibile costruire il menu dei voti: {e}");
                return io.respond(Reply::text("❌ Non è stato possibile aprire i tuoi voti. Riprova tra poco.")).await;
            }
        };
        io.respond(
            Reply::text(format!(
                "**{}** · seleziona fino a {max_votes} opzioni; togli la spunta per ritirare un voto.\n{summary}",
                poll.name
            ))
            .with_components(vec![CreateActionRow::SelectMenu(menu)]),
        )
        .await
    }

    async fn my_votes_select<I: InteractionIo>(&self, io: &I, poll_id: i64) -> Io<()> {
        let chosen: Vec<i64> = io.values().iter().filter_map(|value| value.parse().ok()).collect();
        let (changed, message) = set_user_votes(self.db(), io.user_id(), poll_id, &chosen, self.now()).unwrap_or_else(|e| {
            error!("Errore SQLite durante l'aggiornamento dei voti: {e}");
            (false, "❌ Non è stato possibile salvare i voti. Riprova tra poco.".to_string())
        });
        let summary = vote_summary(self.db(), poll_id, io.user_id()).unwrap_or_default();
        let mut reply = Reply::text(format!("{message}\n{summary}"));
        if let Ok(menu) = my_votes_menu(self.db(), poll_id, io.user_id()) {
            reply = reply.with_components(vec![CreateActionRow::SelectMenu(menu)]);
        }
        io.update(reply).await?;
        if changed {
            self.after_vote_change(poll_id);
        }
        Ok(())
    }

    // --- revisione ---

    async fn review_button<I: InteractionIo>(&self, io: &I, action: ReviewAction, suggestion_id: i64) -> Io<()> {
        if !self.is_staff(io) {
            return io.respond(Reply::text("⚠️ La revisione delle proposte è riservata allo staff.")).await;
        }
        if action == ReviewAction::Reject {
            return io.show_modal(reject_modal(suggestion_id)).await;
        }
        io.defer().await?;
        let outcome = apply_review(
            self.db(),
            &self.services.notifier,
            &self.effects(),
            ReviewRequest {
                actor_id: io.user_id(),
                suggestion_id,
                action: "approve",
                reason: None,
                source: ReviewSource::Discord,
            },
        )
        .await;
        if outcome.status_code == 404 {
            // Proposta unita o rimossa dal reset: chiudi la scheda.
            if let Err(e) = io.strip_source_components().await {
                warn!("Impossibile chiudere la scheda di revisione {suggestion_id}: {e}");
            }
        }
        io.followup(Reply::text(outcome.message)).await
    }

    async fn reject_modal_submitted<I: InteractionIo>(&self, io: &I, suggestion_id: i64) -> Io<()> {
        // Diversamente da Python controlliamo anche qui lo staff: la maschera arriva da un pulsante
        // già protetto, ma un controllo in più non costa nulla.
        if !self.is_staff(io) {
            return io.respond(Reply::text("⚠️ La revisione delle proposte è riservata allo staff.")).await;
        }
        io.defer().await?;
        let reason = io.modal_field(REJECT_REASON_FIELD_ID).unwrap_or_default();
        let outcome = apply_review(
            self.db(),
            &self.services.notifier,
            &self.effects(),
            ReviewRequest {
                actor_id: io.user_id(),
                suggestion_id,
                action: "reject",
                reason: Some(&reason),
                source: ReviewSource::Discord,
            },
        )
        .await;
        io.followup(Reply::text(outcome.message)).await
    }

    // --- dashboard ---

    async fn hub_vote<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let polls = list_active_polls(self.db(), Some(25)).unwrap_or_default();
        if polls.is_empty() {
            return io.respond(Reply::text("⚠️ Non ci sono sondaggi attivi al momento.")).await;
        }
        let menu = dashboard_poll_menu(self.db(), &polls).map_err(|e| IoError(e.to_string()))?;
        io.respond(Reply::text("🗳️ Scegli a quale sondaggio partecipare:").with_components(vec![CreateActionRow::SelectMenu(menu)])).await
    }

    async fn hub_support<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let suggestions = current_suggestions(self.db(), 25, false).unwrap_or_default();
        let Some(first) = suggestions.first() else {
            return io
                .respond(
                    Reply::text("Non ci sono ancora proposte approvate da supportare. Vuoi essere il primo a proporre qualcosa?")
                        .with_components(button_rows(&[propose_first_button()])),
                )
                .await;
        };
        let layout = support_v2(self.db(), &suggestions, first.id).map_err(|e| IoError(e.to_string()))?;
        io.respond(Reply::v2(layout.components)).await
    }

    async fn support_select<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let Some(selected) = io.values().first().and_then(|value| value.parse::<i64>().ok()) else {
            return Ok(());
        };
        let suggestions = current_suggestions(self.db(), 25, false).unwrap_or_default();
        let layout = support_v2(self.db(), &suggestions, selected).map_err(|e| IoError(e.to_string()))?;
        if layout.missing {
            return io.update(Reply::v2(notice_components("⚠️ Questa proposta non è più disponibile."))).await;
        }
        io.update(Reply::v2(layout.components)).await
    }

    async fn support_confirm<I: InteractionIo>(&self, io: &I, suggestion_id: i64) -> Io<()> {
        io.defer().await?;
        let mut outcome = Ok(());
        self.services
            .support_suggestion(io.user_id(), Some(suggestion_id), self.now(), async |text: String| {
                outcome = io.followup(Reply::text(text)).await;
            })
            .await;
        outcome
    }

    async fn hub_more<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let Some(key) = io.values().first() else {
            return Ok(());
        };
        match build_more_section(self.db(), key, io.user_id(), self.now()) {
            Ok(Some(section)) => io.respond(Reply::embed(section.build())).await,
            Ok(None) => Ok(()),
            Err(e) => {
                error!("Impossibile costruire la sezione «{key}»: {e}");
                io.respond(Reply::text("❌ Non è stato possibile caricare questa sezione. Riprova tra poco.")).await
            }
        }
    }

    async fn hub_poll_select<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let poll_id = io.values().first().and_then(|value| value.parse::<i64>().ok());
        let poll = poll_id.and_then(|id| find_poll(self.db(), id).ok().flatten()).filter(|poll| poll.is_active());
        let Some(poll) = poll else {
            return io.update(Reply::text("⚠️ Questo sondaggio non è più attivo.").replacing_embeds().without_components()).await;
        };
        let embed = build_embed(self.db(), &poll, self.now()).map_err(|e| IoError(e.to_string()))?.build();
        let components = build_view(self.db(), &poll).map_err(|e| IoError(e.to_string()))?;
        io.update(Reply { content: Some(String::new()), embeds: Some(vec![embed]), components: Some(components), v2: None }).await
    }

    // --- pannello staff ---

    /// Apre il pannello di selezione delle proposte (da `/staff`).
    pub async fn open_staff_panel<I: InteractionIo>(&self, io: &I) -> Io<()> {
        const RESERVED: &str = "⚠️ Questo pannello è riservato allo staff.";
        if !self.is_staff(io) {
            return io.respond(Reply::text(RESERVED)).await;
        }
        let configured = |key: &str| self.db().get_setting(key).ok().flatten().is_some_and(|value| !value.is_empty());
        if !configured("poll_channel") || !configured("staff_channel") {
            return io.respond(Reply::text("⚠️ Prima configura i canali con /config.")).await;
        }
        let Some(panel) = build_staff_panel(self.db(), self.now().timestamp()).map_err(|e| IoError(e.to_string()))? else {
            return io.respond(Reply::text("⚠️ Non ci sono proposte in attesa di selezione.")).await;
        };
        let menu = staff_proposal_menu(self.db(), &panel).map_err(|e| IoError(e.to_string()))?;
        let mut components = vec![CreateActionRow::SelectMenu(menu)];
        components.extend(button_rows(&[staff_pick_button(false)]));
        io.respond(Reply { embeds: Some(vec![panel.embed.build()]), components: Some(components), ..Reply::default() }).await
    }

    async fn staff_proposal_select<I: InteractionIo>(&self, io: &I) -> Io<()> {
        if !self.is_staff(io) {
            return io.respond(Reply::text("⚠️ Questo pannello è riservato allo staff.")).await;
        }
        let Some(selected) = io.values().first().and_then(|value| value.parse::<i64>().ok()) else {
            return Ok(());
        };
        let name: String = self
            .db()
            .with_conn(|connection| connection.query_row("SELECT name FROM suggestions WHERE id=?", [selected], |row| row.get(0)))
            .unwrap_or_else(|_| format!("#{selected}"));
        let mut components = Vec::new();
        if let Ok(Some(panel)) = build_staff_panel(self.db(), self.now().timestamp()) {
            if let Ok(menu) = staff_proposal_menu(self.db(), &panel) {
                components.push(CreateActionRow::SelectMenu(menu));
            }
        }
        components.extend(button_rows(&[staff_pick_selected_button(selected)]));
        io.update(Reply::text(format!("✅ Selezionata: **{name}**. Conferma per aggiungerla al ballottaggio.")).with_components(components)).await
    }

    async fn staff_pick<I: InteractionIo>(&self, io: &I, suggestion_id: Option<i64>) -> Io<()> {
        if !self.is_staff(io) {
            return io.respond(Reply::text("⚠️ Questo pannello è riservato allo staff.")).await;
        }
        let Some(suggestion_id) = suggestion_id else {
            return io.respond(Reply::text("⚠️ Prima seleziona una proposta.")).await;
        };
        io.defer().await?;
        let mut outcome = Ok(());
        self.services
            .pick_suggestion(io.user_id(), suggestion_id, None, Some("staff_panel"), self.now(), async |text: String| {
                outcome = io.followup(Reply::text(text)).await;
            })
            .await;
        outcome
    }

    // --- invio di una proposta ---

    async fn suggestion_modal_submitted<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let nome = io.modal_field(MODAL_NAME_FIELD).unwrap_or_default();
        let tipo = io.modal_field(MODAL_TYPE_FIELD).unwrap_or_default();
        let link = io.modal_field(MODAL_LINK_FIELD).unwrap_or_default();
        self.submit_suggestion(io, &nome, &tipo, modal_link(&link).map(str::to_string), true, false).await
    }

    /// Consegna `content` come nuova risposta, o sostituendo il messaggio se `edit`.
    async fn flow_reply<I: InteractionIo>(&self, io: &I, content: String, components: Vec<CreateActionRow>, edit: bool) -> Io<()> {
        if edit {
            io.update(Reply { content: Some(content), embeds: Some(Vec::new()), components: Some(components), v2: None }).await
        } else if components.is_empty() {
            io.respond(Reply::text(content)).await
        } else {
            io.respond(Reply::text(content).with_components(components)).await
        }
    }

    async fn submit_suggestion<I: InteractionIo>(
        &self,
        io: &I,
        nome: &str,
        tipo: &str,
        link: Option<String>,
        check_similar: bool,
        edit: bool,
    ) -> Io<()> {
        let user_id = io.user_id();
        if check_similar {
            let similar = similar_suggestions(self.db(), nome, Some(tipo), 3, None).unwrap_or_default();
            if !similar.is_empty() {
                let names = similar.iter().map(|item| format!("• **{}** ({})", item.name, item.tipo)).collect::<Vec<_>>().join("\n");
                let content = format!(
                    "🤔 Esiste già qualcosa di simile:\n{names}\n\nIntendevi una di queste? Sostenerla aiuta a farla arrivare al ballottaggio."
                );
                let nonce = self.remember(Pending {
                    owner: user_id,
                    nome: nome.to_string(),
                    tipo: tipo.to_string(),
                    link,
                    similar: similar.clone(),
                    created: Instant::now(),
                });
                return self.flow_reply(io, content, similar_rows(nonce, &similar), edit).await;
            }
        }
        let result = register_suggestion(self.db(), user_id, nome, tipo, link.as_deref(), self.now());
        let mut message = result.message;
        let mut components = Vec::new();
        if result.success && !has_notification_preference(self.db(), user_id).unwrap_or(true) {
            components = opt_in_rows(user_id);
            message.push_str(OPT_IN_QUESTION);
        }
        self.flow_reply(io, message, components, edit).await?;
        if result.success {
            self.refresh_dashboard_and_notify_site();
            if let Some(new_id) = result.created_id {
                // L'annuncio pubblico arriva solo dopo l'approvazione staff.
                if let Err(e) = self.services.publisher.post_review_card(new_id).await {
                    error!("Impossibile pubblicare la scheda di revisione {new_id}: {e}");
                }
            }
        }
        Ok(())
    }

    fn remember(&self, pending: Pending) -> u64 {
        let nonce = self.next_nonce.fetch_add(1, Ordering::Relaxed);
        let mut map = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let ttl = self.pending_ttl;
        map.retain(|_, entry| entry.created.elapsed() < ttl);
        map.insert(nonce, pending);
        nonce
    }

    fn take_pending(&self, nonce: u64) -> Option<Pending> {
        let mut map = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = map.get(&nonce)?;
        if entry.created.elapsed() >= self.pending_ttl {
            map.remove(&nonce);
            return None;
        }
        Some(entry.clone())
    }

    fn forget(&self, nonce: u64) {
        self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&nonce);
    }

    async fn flow_component<I: InteractionIo>(&self, io: &I, component: FlowComponent) -> Io<()> {
        // Le richieste "Intendevi…?" sono di chi ha inviato la proposta e scadono dopo 3 minuti.
        if let FlowComponent::Similar { nonce, .. } | FlowComponent::Create { nonce } = component {
            if self.pending_missing_or_foreign(io, nonce).await? {
                return Ok(());
            }
        }
        match component {
            FlowComponent::OptIn { enable, owner } => {
                if io.user_id() != owner {
                    return io.respond(Reply::text("⚠️ Questi bottoni sono per chi ha inviato la proposta.")).await;
                }
                if let Err(e) = set_dm_notifications(self.db(), owner, enable) {
                    error!("Impossibile salvare la preferenza sui DM: {e}");
                }
                let content = io.message_content().unwrap_or_default();
                let confirmation = content.split(OPT_IN_QUESTION).next().unwrap_or_default();
                let tail = if enable {
                    "🔔 Fatto! Puoi cambiare idea quando vuoi con `/notifiche`."
                } else {
                    "👌 Nessuna notifica. Puoi attivarle quando vuoi con `/notifiche`."
                };
                io.update(Reply::text(format!("{confirmation}\n\n{tail}")).without_components()).await
            }
            FlowComponent::Similar { nonce, index } => {
                let pending = self.take_pending(nonce).expect("controllato sopra");
                self.forget(nonce);
                let Some(item) = pending.similar.get(index) else {
                    return Ok(());
                };
                if item.review_status == "approved" {
                    let result = register_suggestion_support(self.db(), io.user_id(), item.id, self.now());
                    io.update(Reply::text(result.message).without_components()).await?;
                    if result.success {
                        self.refresh_dashboard_and_notify_site();
                    }
                    return Ok(());
                }
                // Proposta ancora in revisione: la tua proposta si aggiunge alla sua.
                self.submit_suggestion(io, &item.name, &item.tipo, pending.link, false, true).await
            }
            FlowComponent::Create { nonce } => {
                let pending = self.take_pending(nonce).expect("controllato sopra");
                self.forget(nonce);
                self.submit_suggestion(io, &pending.nome, &pending.tipo, pending.link, false, true).await
            }
        }
    }

    /// `true` se la richiesta non c'è (scaduta) o non è di chi ha cliccato: in tal caso ha già risposto.
    async fn pending_missing_or_foreign<I: InteractionIo>(&self, io: &I, nonce: u64) -> Io<bool> {
        match self.take_pending(nonce) {
            None => {
                io.respond(Reply::text("⚠️ Questa richiesta è scaduta: invia di nuovo la proposta.")).await?;
                Ok(true)
            }
            Some(pending) if pending.owner != io.user_id() => {
                io.respond(Reply::text("⚠️ Questi bottoni sono per chi ha inviato la proposta.")).await?;
                Ok(true)
            }
            Some(_) => Ok(false),
        }
    }
}

/// Un pulsante per ogni proposta simile (max 3), ciascuno sulla sua riga, e "È diversa, creala".
fn similar_rows(nonce: u64, similar: &[SimilarSuggestion]) -> Vec<CreateActionRow> {
    let mut buttons: Vec<ViewButton> = similar
        .iter()
        .take(3)
        .enumerate()
        .map(|(index, item)| {
            let prefix = if item.review_status == "approved" { "👍 Supporta" } else { "➕ Unisciti a" };
            ViewButton {
                custom_id: FlowComponent::similar_id(nonce, index),
                label: format!("{prefix} «{}»", item.name).chars().take(80).collect(),
                emoji: None,
                kind: ButtonKind::Success,
                disabled: false,
            }
        })
        .collect();
    buttons.push(ViewButton {
        custom_id: FlowComponent::create_id(nonce),
        label: "È diversa, creala".into(),
        emoji: Some("✨"),
        kind: ButtonKind::Secondary,
        disabled: false,
    });
    buttons.iter().flat_map(|button| button_rows(std::slice::from_ref(button))).collect()
}

/// "Sì, avvisami" / "No, grazie".
fn opt_in_rows(owner: i64) -> Vec<CreateActionRow> {
    button_rows(&[
        ViewButton { custom_id: FlowComponent::opt_in_id(true, owner), label: "Sì, avvisami".into(), emoji: Some("🔔"), kind: ButtonKind::Primary, disabled: false },
        ViewButton { custom_id: FlowComponent::opt_in_id(false, owner), label: "No, grazie".into(), emoji: None, kind: ButtonKind::Secondary, disabled: false },
    ])
}

// --- adattatori per serenity -----------------------------------------------------------------

fn io_error(error: serenity::Error) -> IoError {
    IoError(error.to_string())
}

fn message_response(reply: &Reply, ephemeral: bool) -> CreateInteractionResponseMessage {
    let mut message = CreateInteractionResponseMessage::new().allowed_mentions(CreateAllowedMentions::new());
    if ephemeral {
        message = message.ephemeral(true);
    }
    if let Some(content) = &reply.content {
        message = message.content(content);
    }
    if let Some(embeds) = &reply.embeds {
        message = message.embeds(embeds.clone());
    }
    if let Some(components) = &reply.components {
        message = message.components(components.clone());
    }
    message
}

fn followup_message(reply: &Reply) -> CreateInteractionResponseFollowup {
    let mut followup = CreateInteractionResponseFollowup::new().ephemeral(true).allowed_mentions(CreateAllowedMentions::new());
    if let Some(content) = &reply.content {
        followup = followup.content(content);
    }
    if let Some(embeds) = &reply.embeds {
        followup = followup.embeds(embeds.clone());
    }
    if let Some(components) = &reply.components {
        followup = followup.components(components.clone());
    }
    followup
}

/// Implementa `InteractionIo` per un tipo di interazione di serenity (clic o maschera).
macro_rules! serenity_io {
    ($name:ident, $interaction:ty, values: $values:expr, modal: $modal:expr, content: $content:expr, selected: $selected:expr) => {
        pub struct $name<'a> {
            pub ctx: &'a serenity::Context,
            pub interaction: &'a $interaction,
        }

        impl InteractionIo for $name<'_> {
            fn custom_id(&self) -> &str {
                &self.interaction.data.custom_id
            }
            fn user_id(&self) -> i64 {
                self.interaction.user.id.get() as i64
            }
            fn user_name(&self) -> String {
                self.interaction.user.name.clone()
            }
            fn values(&self) -> &[String] {
                let get: fn(&$interaction) -> &[String] = $values;
                get(self.interaction)
            }
            fn modal_field(&self, field_id: &str) -> Option<String> {
                let get: fn(&$interaction, &str) -> Option<String> = $modal;
                get(self.interaction, field_id)
            }
            fn message_content(&self) -> Option<String> {
                let get: fn(&$interaction) -> Option<String> = $content;
                get(self.interaction)
            }
            fn guild_id(&self) -> Option<u64> {
                self.interaction.guild_id.map(|id| id.get())
            }
            fn is_admin(&self) -> bool {
                self.interaction.member.as_ref().and_then(|member| member.permissions).is_some_and(|p| p.administrator())
            }
            fn role_ids(&self) -> Vec<u64> {
                self.interaction.member.as_ref().map(|member| member.roles.iter().map(|role| role.get()).collect()).unwrap_or_default()
            }

            async fn defer(&self) -> Io<()> {
                let response = CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new().ephemeral(true));
                self.interaction.create_response(&self.ctx.http, response).await.map_err(io_error)
            }

            async fn respond(&self, reply: Reply) -> Io<()> {
                if let Some(components) = reply.v2 {
                    return crate::v2::respond_with_components(
                        &self.ctx.http,
                        self.interaction.id.get(),
                        &self.interaction.token,
                        components,
                        crate::v2::ResponseKind::NewMessage,
                        true,
                    )
                    .await
                    .map_err(io_error);
                }
                let response = CreateInteractionResponse::Message(message_response(&reply, true));
                self.interaction.create_response(&self.ctx.http, response).await.map_err(io_error)
            }

            async fn respond_public(&self, reply: Reply) -> Io<()> {
                let response = CreateInteractionResponse::Message(message_response(&reply, false));
                self.interaction.create_response(&self.ctx.http, response).await.map_err(io_error)
            }

            async fn update(&self, reply: Reply) -> Io<()> {
                if let Some(components) = reply.v2 {
                    return crate::v2::respond_with_components(
                        &self.ctx.http,
                        self.interaction.id.get(),
                        &self.interaction.token,
                        components,
                        crate::v2::ResponseKind::UpdateMessage,
                        true,
                    )
                    .await
                    .map_err(io_error);
                }
                let response = CreateInteractionResponse::UpdateMessage(message_response(&reply, false));
                self.interaction.create_response(&self.ctx.http, response).await.map_err(io_error)
            }

            async fn followup(&self, reply: Reply) -> Io<()> {
                self.interaction.create_followup(&self.ctx.http, followup_message(&reply)).await.map(|_| ()).map_err(io_error)
            }

            async fn show_modal(&self, modal: CreateModal) -> Io<()> {
                self.interaction.create_response(&self.ctx.http, CreateInteractionResponse::Modal(modal)).await.map_err(io_error)
            }

            async fn strip_source_components(&self) -> Io<()> {
                strip_components(self.ctx, self.interaction.channel_id, source_message_id(self.interaction)).await
            }

            async fn edit_response(&self, reply: Reply) -> Io<()> {
                self.interaction.edit_response(&self.ctx.http, edit_response_message(&reply)).await.map(|_| ()).map_err(io_error)
            }

            async fn followup_file(&self, content: String, filename: String, data: Vec<u8>) -> Io<()> {
                self.interaction.create_followup(&self.ctx.http, file_followup(content, filename, data)).await.map(|_| ()).map_err(io_error)
            }

            fn selected_user(&self) -> Option<crate::control_center::MemberInfo> {
                let get: fn(&$interaction) -> Option<crate::control_center::MemberInfo> = $selected;
                get(self.interaction)
            }

            async fn edit_source_message(&self, content: String) -> Io<()> {
                let Some(message) = source_message_id(self.interaction) else {
                    return Ok(());
                };
                let edit = EditMessage::new().content(content).components(Vec::new()).allowed_mentions(CreateAllowedMentions::new());
                self.interaction.channel_id.edit_message(&self.ctx.http, message, edit).await.map(|_| ()).map_err(io_error)
            }
        }
    };
}

fn file_followup(content: String, filename: String, data: Vec<u8>) -> CreateInteractionResponseFollowup {
    CreateInteractionResponseFollowup::new()
        .ephemeral(true)
        .allowed_mentions(CreateAllowedMentions::new())
        .content(content)
        .add_file(serenity::CreateAttachment::bytes(data, filename))
}

fn edit_response_message(reply: &Reply) -> serenity::EditInteractionResponse {
    let mut edit = serenity::EditInteractionResponse::new().allowed_mentions(CreateAllowedMentions::new());
    if let Some(content) = &reply.content {
        edit = edit.content(content);
    }
    if let Some(embeds) = &reply.embeds {
        edit = edit.embeds(embeds.clone());
    }
    if let Some(components) = &reply.components {
        edit = edit.components(components.clone());
    }
    edit
}

/// `InteractionIo` per un comando slash. Non ha pulsante né messaggio d'origine: `update` e le
/// funzioni sul messaggio sorgente non si applicano e restituiscono un errore.
pub struct CommandIo<'a> {
    pub ctx: &'a serenity::Context,
    pub interaction: &'a serenity::CommandInteraction,
}

fn not_applicable<T>() -> Io<T> {
    Err(IoError("operazione non applicabile a un comando slash".into()))
}

impl InteractionIo for CommandIo<'_> {
    fn custom_id(&self) -> &str {
        ""
    }
    fn user_id(&self) -> i64 {
        self.interaction.user.id.get() as i64
    }
    fn user_name(&self) -> String {
        self.interaction.user.name.clone()
    }
    fn values(&self) -> &[String] {
        &[]
    }
    fn modal_field(&self, _field_id: &str) -> Option<String> {
        None
    }
    fn message_content(&self) -> Option<String> {
        None
    }
    fn guild_id(&self) -> Option<u64> {
        self.interaction.guild_id.map(|id| id.get())
    }
    fn is_admin(&self) -> bool {
        self.interaction.member.as_ref().and_then(|member| member.permissions).is_some_and(|p| p.administrator())
    }
    fn role_ids(&self) -> Vec<u64> {
        self.interaction.member.as_ref().map(|member| member.roles.iter().map(|role| role.get()).collect()).unwrap_or_default()
    }

    async fn defer(&self) -> Io<()> {
        let response = CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new().ephemeral(true));
        self.interaction.create_response(&self.ctx.http, response).await.map_err(io_error)
    }

    async fn respond(&self, reply: Reply) -> Io<()> {
        if let Some(components) = reply.v2 {
            return crate::v2::respond_with_components(
                &self.ctx.http,
                self.interaction.id.get(),
                &self.interaction.token,
                components,
                crate::v2::ResponseKind::NewMessage,
                true,
            )
            .await
            .map_err(io_error);
        }
        let response = CreateInteractionResponse::Message(message_response(&reply, true));
        self.interaction.create_response(&self.ctx.http, response).await.map_err(io_error)
    }

    async fn respond_public(&self, reply: Reply) -> Io<()> {
        let response = CreateInteractionResponse::Message(message_response(&reply, false));
        self.interaction.create_response(&self.ctx.http, response).await.map_err(io_error)
    }

    async fn update(&self, _reply: Reply) -> Io<()> {
        not_applicable()
    }

    async fn followup(&self, reply: Reply) -> Io<()> {
        self.interaction.create_followup(&self.ctx.http, followup_message(&reply)).await.map(|_| ()).map_err(io_error)
    }

    async fn show_modal(&self, modal: CreateModal) -> Io<()> {
        self.interaction.create_response(&self.ctx.http, CreateInteractionResponse::Modal(modal)).await.map_err(io_error)
    }

    async fn strip_source_components(&self) -> Io<()> {
        not_applicable()
    }

    async fn edit_source_message(&self, _content: String) -> Io<()> {
        not_applicable()
    }

    async fn edit_response(&self, reply: Reply) -> Io<()> {
        self.interaction.edit_response(&self.ctx.http, edit_response_message(&reply)).await.map(|_| ()).map_err(io_error)
    }

    async fn followup_file(&self, content: String, filename: String, data: Vec<u8>) -> Io<()> {
        self.interaction.create_followup(&self.ctx.http, file_followup(content, filename, data)).await.map(|_| ()).map_err(io_error)
    }
}

trait HasSourceMessage {
    fn source_message(&self) -> Option<serenity::MessageId>;
}

impl HasSourceMessage for ComponentInteraction {
    fn source_message(&self) -> Option<serenity::MessageId> {
        Some(self.message.id)
    }
}

impl HasSourceMessage for ModalInteraction {
    fn source_message(&self) -> Option<serenity::MessageId> {
        self.message.as_ref().map(|message| message.id)
    }
}

fn source_message_id(interaction: &impl HasSourceMessage) -> Option<serenity::MessageId> {
    interaction.source_message()
}

async fn strip_components(ctx: &serenity::Context, channel: serenity::ChannelId, message: Option<serenity::MessageId>) -> Io<()> {
    let Some(message) = message else {
        return Ok(());
    };
    channel.edit_message(&ctx.http, message, EditMessage::new().components(Vec::new())).await.map(|_| ()).map_err(io_error)
}

serenity_io!(
    ComponentIo,
    ComponentInteraction,
    values: |interaction| match &interaction.data.kind {
        ComponentInteractionDataKind::StringSelect { values } => values.as_slice(),
        _ => &[],
    },
    modal: |_, _| None,
    content: |interaction| Some(interaction.message.content.clone()),
    selected: |interaction| match &interaction.data.kind {
        ComponentInteractionDataKind::UserSelect { values } => {
            let id = *values.first()?;
            let user = interaction.data.resolved.users.get(&id)?;
            let nick = interaction.data.resolved.members.get(&id).and_then(|member| member.nick.clone());
            Some(crate::control_center::MemberInfo {
                id: id.get() as i64,
                display_name: nick.or_else(|| user.global_name.clone()).unwrap_or_else(|| user.name.clone()),
                avatar_url: user.face(),
            })
        }
        _ => None,
    }
);

serenity_io!(
    ModalIo,
    ModalInteraction,
    values: |_| &[],
    modal: |interaction, field_id| {
        interaction.data.components.iter().flat_map(|row| row.components.iter()).find_map(|component| match component {
            ActionRowComponent::InputText(input) if input.custom_id == field_id => Some(input.value.clone().unwrap_or_default()),
            _ => None,
        })
    },
    content: |interaction| interaction.message.as_ref().map(|message| message.content.clone()),
    selected: |_| None
);

/// Ingresso unico dall'`event_handler` di poise: clic, scelte nei menu e maschere inviate.
pub async fn dispatch_event<S: DiscordSender, A: DiscordApi>(
    ctx: &serenity::Context,
    interaction: &serenity::Interaction,
    interactions: &Interactions<S, A>,
) {
    let result = match interaction {
        serenity::Interaction::Component(component) => interactions.handle(&ComponentIo { ctx, interaction: component }).await,
        serenity::Interaction::Modal(modal) => interactions.handle(&ModalIo { ctx, interaction: modal }).await,
        _ => return,
    };
    if let Err(e) = result {
        // Interazione scaduta o risposta rifiutata: l'utente vedrà «interazione fallita», noi il log.
        error!("Gestione dell'interazione non riuscita: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::discord_api::testing::{Call, FakeApi};
    use crate::notify::Notifier;
    use crate::notify::testing::FakeSender;
    use crate::polls::RefreshScheduler;
    use crate::publisher::Publisher;
    use chrono::TimeZone;
    use serde_json::json;
    use std::sync::Arc;

    fn clock() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()
    }

    // --- interazione finta -----------------------------------------------------------------

    #[derive(Debug, Clone)]
    struct Recorded {
        content: Option<String>,
        embeds: Option<Vec<Value>>,
        components: Option<Vec<Value>>,
        v2: Option<Vec<Value>>,
    }

    impl Recorded {
        fn of(reply: &Reply) -> Self {
            Recorded {
                content: reply.content.clone(),
                embeds: reply.embeds.as_ref().map(|list| list.iter().map(|e| serde_json::to_value(e).unwrap()).collect()),
                components: reply.components.as_ref().map(|list| list.iter().map(|c| serde_json::to_value(c).unwrap()).collect()),
                v2: reply.v2.clone(),
            }
        }

        fn text(&self) -> &str {
            self.content.as_deref().unwrap_or_default()
        }

        /// I `custom_id` di tutti i pulsanti e menu.
        fn component_ids(&self) -> Vec<String> {
            let mut ids = Vec::new();
            for row in self.components.iter().chain(self.v2.iter()).flatten() {
                if let Some(children) = row["components"].as_array() {
                    ids.extend(children.iter().filter_map(|c| c["custom_id"].as_str().map(str::to_string)));
                }
            }
            ids
        }

        fn labels(&self) -> Vec<String> {
            self.components
                .iter()
                .flatten()
                .flat_map(|row| row["components"].as_array().cloned().unwrap_or_default())
                .filter_map(|c| c["label"].as_str().map(str::to_string))
                .collect()
        }
    }

    #[derive(Debug, Clone)]
    enum Event {
        Defer,
        Respond(Recorded),
        RespondPublic(Recorded),
        Update(Recorded),
        Followup(Recorded),
        Modal(Value),
        StripComponents,
        EditSource(String),
        EditResponse(Recorded),
        File { content: String, filename: String, data: Vec<u8> },
    }

    struct FakeIo {
        custom_id: String,
        user: i64,
        values: Vec<String>,
        fields: HashMap<String, String>,
        message_content: Option<String>,
        admin: bool,
        roles: Vec<u64>,
        guild: Option<u64>,
        selected: Option<crate::control_center::MemberInfo>,
        events: Mutex<Vec<Event>>,
    }

    impl FakeIo {
        fn new(custom_id: &str, user: i64) -> Self {
            Self {
                custom_id: custom_id.to_string(),
                user,
                values: Vec::new(),
                fields: HashMap::new(),
                message_content: None,
                admin: false,
                roles: Vec::new(),
                guild: Some(1),
                selected: None,
                events: Mutex::new(Vec::new()),
            }
        }
        fn staff(mut self) -> Self {
            self.admin = true;
            self
        }
        fn no_guild(mut self) -> Self {
            self.guild = None;
            self
        }
        fn selecting(mut self, user: crate::control_center::MemberInfo) -> Self {
            self.selected = Some(user);
            self
        }
        fn value(mut self, value: impl ToString) -> Self {
            self.values.push(value.to_string());
            self
        }
        fn field(mut self, id: &str, text: &str) -> Self {
            self.fields.insert(id.to_string(), text.to_string());
            self
        }
        fn content(mut self, text: &str) -> Self {
            self.message_content = Some(text.to_string());
            self
        }
        fn events(&self) -> Vec<Event> {
            self.events.lock().unwrap().clone()
        }
        fn push(&self, event: Event) -> Io<()> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
        /// L'unica risposta registrata (escluso `defer`).
        fn only(&self) -> Event {
            let events: Vec<Event> = self.events().into_iter().filter(|e| !matches!(e, Event::Defer)).collect();
            assert_eq!(events.len(), 1, "{events:?}");
            events[0].clone()
        }
        fn recorded(&self) -> Recorded {
            match self.only() {
                Event::Respond(r) | Event::RespondPublic(r) | Event::Update(r) | Event::Followup(r) => r,
                other => panic!("attesa una risposta, trovato {other:?}"),
            }
        }
        fn deferred(&self) -> bool {
            matches!(self.events().first(), Some(Event::Defer))
        }
    }

    impl InteractionIo for FakeIo {
        fn custom_id(&self) -> &str {
            &self.custom_id
        }
        fn user_id(&self) -> i64 {
            self.user
        }
        fn user_name(&self) -> String {
            format!("utente{}", self.user)
        }
        fn values(&self) -> &[String] {
            &self.values
        }
        fn modal_field(&self, field_id: &str) -> Option<String> {
            self.fields.get(field_id).cloned()
        }
        fn message_content(&self) -> Option<String> {
            self.message_content.clone()
        }
        fn guild_id(&self) -> Option<u64> {
            self.guild
        }
        fn is_admin(&self) -> bool {
            self.admin
        }
        fn role_ids(&self) -> Vec<u64> {
            self.roles.clone()
        }
        async fn defer(&self) -> Io<()> {
            self.push(Event::Defer)
        }
        async fn respond(&self, reply: Reply) -> Io<()> {
            self.push(Event::Respond(Recorded::of(&reply)))
        }
        async fn respond_public(&self, reply: Reply) -> Io<()> {
            self.push(Event::RespondPublic(Recorded::of(&reply)))
        }
        async fn update(&self, reply: Reply) -> Io<()> {
            self.push(Event::Update(Recorded::of(&reply)))
        }
        async fn followup(&self, reply: Reply) -> Io<()> {
            self.push(Event::Followup(Recorded::of(&reply)))
        }
        async fn show_modal(&self, modal: CreateModal) -> Io<()> {
            self.push(Event::Modal(serde_json::to_value(modal).unwrap()))
        }
        async fn strip_source_components(&self) -> Io<()> {
            self.push(Event::StripComponents)
        }
        async fn edit_source_message(&self, content: String) -> Io<()> {
            self.push(Event::EditSource(content))
        }
        async fn edit_response(&self, reply: Reply) -> Io<()> {
            self.push(Event::EditResponse(Recorded::of(&reply)))
        }
        async fn followup_file(&self, content: String, filename: String, data: Vec<u8>) -> Io<()> {
            self.push(Event::File { content, filename, data })
        }
        fn selected_user(&self) -> Option<crate::control_center::MemberInfo> {
            self.selected.clone()
        }
    }

    // --- ambiente --------------------------------------------------------------------------

    struct Env {
        interactions: Interactions<FakeSender, FakeApi>,
        db: Database,
        sender: FakeSender,
        api: FakeApi,
    }

    fn env() -> Env {
        env_at(clock)
    }

    fn env_at(clock: fn() -> DateTime<Tz>) -> Env {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, clock()).unwrap();
        db.set_setting("poll_channel", 500).unwrap();
        db.set_setting("staff_channel", 600).unwrap();
        let sender = FakeSender::default();
        let api = FakeApi::default();
        let notifier = Notifier::with_audit_channel(db.clone(), sender.clone(), None);
        let publisher = Arc::new(Publisher::with_clock(api.clone(), db.clone(), clock));
        let services = Services { db: db.clone(), notifier, publisher, scheduler: RefreshScheduler::new() };
        Env { interactions: Interactions::with_clock(services, clock), db, sender, api }
    }

    impl Env {
        async fn run(&self, io: &FakeIo) {
            self.interactions.handle(io).await.unwrap();
        }

        fn approved(&self, user: i64, name: &str) -> i64 {
            let id = register_suggestion(&self.db, user, name, "Gioco", None, clock()).created_id.unwrap();
            self.db.execute("UPDATE suggestions SET review_status='approved' WHERE id=?", [id]).unwrap();
            id
        }

        fn option_of(&self, poll: i64) -> i64 {
            self.db.with_conn(|c| c.query_row("SELECT MIN(id) FROM options WHERE poll_id=?", [poll], |r| r.get(0)).unwrap())
        }

        fn count(&self, sql: &str) -> i64 {
            self.db.with_conn(|c| c.query_row(sql, [], |r| r.get(0)).unwrap())
        }
    }

    // --- voti ------------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn vote_button_toggles_and_refreshes_the_poll_and_dashboard() {
        let e = env();
        let option = e.option_of(1);
        let io = FakeIo::new(&format!("vote:{option}"), 7);
        e.run(&io).await;
        assert!(io.deferred());
        let reply = io.recorded();
        assert!(matches!(io.events().last(), Some(Event::Followup(_))));
        assert_eq!(reply.text(), "✅ Voto registrato.\n🗳️ I tuoi voti: **Among Us** · `1` voto rimasto");
        tokio::time::sleep(Duration::from_secs(3)).await;
        let calls = e.api.calls();
        assert!(calls.iter().filter(|c| matches!(c, Call::Send { channel: 500, .. })).count() >= 2, "messaggio del sondaggio e dashboard: {calls:?}");

        let again = FakeIo::new(&format!("vote:{option}"), 7);
        e.run(&again).await;
        assert_eq!(again.recorded().text(), "🗑️ Voto rimosso.\n🗳️ I tuoi voti: nessuno · `2` voti rimasti");
    }

    #[tokio::test(start_paused = true)]
    async fn vote_button_reports_refusals_and_errors() {
        let e = env();
        let missing = FakeIo::new("vote:99999", 7);
        e.run(&missing).await;
        assert_eq!(missing.recorded().text(), "❌ Questa opzione non esiste più.", "senza riepilogo: il sondaggio non c'è");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(e.api.calls().is_empty(), "un voto non registrato non aggiorna i messaggi");

        let option = e.option_of(2);
        e.run(&FakeIo::new(&format!("vote:{option}"), 7)).await;
        e.run(&FakeIo::new(&format!("vote:{}", option + 1), 7)).await;
        let third = FakeIo::new(&format!("vote:{}", option + 2), 7);
        e.run(&third).await;
        assert!(third.recorded().text().starts_with("⚠️ Hai già usato i tuoi 2 voti"), "{}", third.recorded().text());
        // Rimuovere un voto scrive in `vote_change_counts`: con la tabella mancante è un errore generico.
        e.db.execute("DROP TABLE vote_change_counts", []).unwrap();
        let removal = FakeIo::new(&format!("vote:{option}"), 7);
        e.run(&removal).await;
        assert_eq!(removal.recorded().text(), "❌ Non è stato possibile registrare il voto. Riprova tra poco.");
    }

    #[tokio::test(start_paused = true)]
    async fn my_votes_button_opens_a_menu_with_the_current_votes() {
        let e = env();
        e.run(&FakeIo::new(&format!("vote:{}", e.option_of(1)), 7)).await;
        let io = FakeIo::new("myvotes:1", 7);
        e.run(&io).await;
        let reply = io.recorded();
        assert!(matches!(io.only(), Event::Respond(_)));
        assert_eq!(
            reply.text(),
            "**Giochi gratis da scaricare** · seleziona fino a 2 opzioni; togli la spunta per ritirare un voto.\n🗳️ I tuoi voti: **Among Us** · `1` voto rimasto"
        );
        assert_eq!(reply.component_ids(), ["myvotes-select:1"]);
        let menu = &reply.components.unwrap()[0]["components"][0];
        assert_eq!(menu["options"][0]["default"], true, "il voto già dato è preselezionato");
    }

    #[tokio::test(start_paused = true)]
    async fn my_votes_button_refuses_inactive_and_empty_polls() {
        let e = env();
        e.db.execute("UPDATE polls SET active=0 WHERE id=2", []).unwrap();
        let inactive = FakeIo::new("myvotes:2", 7);
        e.run(&inactive).await;
        assert_eq!(inactive.recorded().text(), "⚠️ Questo sondaggio non è attivo.");
        let missing = FakeIo::new("myvotes:999", 7);
        e.run(&missing).await;
        assert_eq!(missing.recorded().text(), "⚠️ Questo sondaggio non è attivo.");
        let community = FakeIo::new("myvotes:5", 7); // il sondaggio community parte senza opzioni
        e.run(&community).await;
        assert_eq!(community.recorded().text(), "ℹ️ Non ci sono ancora opzioni in questo sondaggio.");
    }

    #[tokio::test(start_paused = true)]
    async fn my_votes_select_replaces_votes_and_updates_the_message() {
        let e = env();
        let first = e.option_of(1);
        let io = FakeIo::new("myvotes-select:1", 7).value(first).value(first + 1);
        e.run(&io).await;
        assert!(matches!(io.only(), Event::Update(_)));
        let reply = io.recorded();
        assert_eq!(reply.text(), "✅ Voti aggiornati.\n🗳️ I tuoi voti: **Among Us**, **Supermarket** · `0` voti rimasti");
        assert_eq!(reply.component_ids(), ["myvotes-select:1"], "il menu resta, con i voti aggiornati");
        assert_eq!(e.count("SELECT COUNT(*) FROM votes WHERE user_id=7"), 2);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!e.api.calls().is_empty(), "il cambiamento aggiorna sondaggio e dashboard");

        let too_many = FakeIo::new("myvotes-select:1", 7).value(first).value(first + 1).value(first + 2);
        e.run(&too_many).await;
        assert!(too_many.recorded().text().starts_with("⚠️ Puoi scegliere al massimo 2 opzioni."));
        let same = FakeIo::new("myvotes-select:1", 7).value(first).value(first + 1);
        e.run(&same).await;
        assert!(same.recorded().text().starts_with("ℹ️ Nessuna modifica ai tuoi voti."));
        let garbage = FakeIo::new("myvotes-select:1", 8).value("abc");
        e.run(&garbage).await;
        assert!(garbage.recorded().text().starts_with("ℹ️ Nessuna modifica"), "valori non validi: come nessuna scelta");
    }

    // --- revisione -------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn review_buttons_are_for_staff_only() {
        let e = env();
        let id = register_suggestion(&e.db, 1, "Among Us", "Gioco", None, clock()).created_id.unwrap();
        let outsider = FakeIo::new(&format!("review:approve:{id}"), 5);
        e.run(&outsider).await;
        assert_eq!(outsider.recorded().text(), "⚠️ La revisione delle proposte è riservata allo staff.");
        assert_eq!(e.count("SELECT COUNT(*) FROM suggestions WHERE review_status='approved'"), 0);

        // Un ruolo staff configurato basta (e il ruolo @everyone, con lo stesso ID del server, no).
        e.db.set_setting("staff_roles", "77,1").unwrap();
        let mut moderator = FakeIo::new(&format!("review:approve:{id}"), 5);
        moderator.roles = vec![77];
        e.run(&moderator).await;
        assert!(moderator.deferred(), "{:?}", moderator.events());
        let mut everyone = FakeIo::new(&format!("review:reject:{id}"), 6);
        everyone.roles = vec![1];
        e.run(&everyone).await;
        assert!(everyone.recorded().text().contains("riservata allo staff"));
    }

    #[tokio::test(start_paused = true)]
    async fn approve_defers_applies_the_review_and_answers_privately() {
        let e = env();
        let id = register_suggestion(&e.db, 1, "Among Us", "Gioco", None, clock()).created_id.unwrap();
        let io = FakeIo::new(&format!("review:approve:{id}"), 9).staff();
        e.run(&io).await;
        assert!(io.deferred());
        assert_eq!(io.recorded().text(), "✅ Approvata: **Among Us**.");
        assert_eq!(e.count("SELECT COUNT(*) FROM suggestions WHERE review_status='approved'"), 1);
        let again = FakeIo::new(&format!("review:approve:{id}"), 9).staff();
        e.run(&again).await;
        assert_eq!(again.recorded().text(), "⚠️ Stato attuale: ✅ Approvata.");
    }

    #[tokio::test(start_paused = true)]
    async fn approving_a_vanished_proposal_closes_the_card() {
        let e = env();
        let io = FakeIo::new("review:approve:999", 9).staff();
        e.run(&io).await;
        let events = io.events();
        assert!(matches!(events[0], Event::Defer));
        assert!(matches!(events[1], Event::StripComponents), "scheda chiusa: {events:?}");
        assert!(matches!(&events[2], Event::Followup(r) if r.text() == "❌ Proposta non trovata questa settimana."));
    }

    #[tokio::test(start_paused = true)]
    async fn reject_opens_a_modal_and_the_submission_rejects_with_the_reason() {
        let e = env();
        let id = register_suggestion(&e.db, 1, "Monopoly", "Gioco", None, clock()).created_id.unwrap();
        let click = FakeIo::new(&format!("review:reject:{id}"), 9).staff();
        e.run(&click).await;
        let Event::Modal(modal) = click.only() else { panic!("attesa una maschera") };
        assert_eq!(modal["custom_id"], format!("review-reject:{id}"));

        let submit = FakeIo::new(&format!("review-reject:{id}"), 9).staff().field("reason", "Troppo lungo");
        e.run(&submit).await;
        assert!(submit.deferred());
        assert_eq!(submit.recorded().text(), "❌ Rifiutata: **Monopoly**.");
        let reason: String = e.db.with_conn(|c| c.query_row("SELECT reason FROM staff_audit WHERE action='proposal_rejected'", [], |r| r.get(0)).unwrap());
        assert_eq!(reason, "Troppo lungo");

        let outsider = FakeIo::new(&format!("review-reject:{id}"), 5).field("reason", "No");
        e.run(&outsider).await;
        assert!(outsider.recorded().text().contains("riservata allo staff"), "controllo staff anche sulla maschera");
        let empty_reason = FakeIo::new("review-reject:999", 9).staff();
        e.run(&empty_reason).await;
        assert_eq!(empty_reason.recorded().text(), "❌ Inserisci il motivo del rifiuto.");
    }

    // --- dashboard -------------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn hub_vote_lists_the_active_polls() {
        let e = env();
        let io = FakeIo::new("hub:vote", 7);
        e.run(&io).await;
        let reply = io.recorded();
        assert_eq!(reply.text(), "🗳️ Scegli a quale sondaggio partecipare:");
        let menu = &reply.components.clone().unwrap()[0]["components"][0];
        assert_eq!(menu["custom_id"], "hub:poll-select");
        assert_eq!(menu["options"].as_array().unwrap().len(), 5);
        assert_eq!(menu["options"][0]["description"], "0 votanti");

        e.db.execute("UPDATE polls SET active=0", []).unwrap();
        let none = FakeIo::new("hub:vote", 7);
        e.run(&none).await;
        assert_eq!(none.recorded().text(), "⚠️ Non ci sono sondaggi attivi al momento.");
    }

    #[tokio::test(start_paused = true)]
    async fn hub_poll_select_replaces_the_menu_with_the_poll() {
        let e = env();
        let io = FakeIo::new("hub:poll-select", 7).value(1);
        e.run(&io).await;
        assert!(matches!(io.only(), Event::Update(_)));
        let reply = io.recorded();
        assert_eq!(reply.content.as_deref(), Some(""), "il testo del menu viene tolto");
        assert_eq!(reply.embeds.as_ref().unwrap()[0]["title"], "🎮  Giochi gratis da scaricare");
        assert!(reply.component_ids().iter().any(|id| id.starts_with("vote:")), "pulsanti di voto");

        e.db.execute("UPDATE polls SET active=0 WHERE id=2", []).unwrap();
        let closed = FakeIo::new("hub:poll-select", 7).value(2);
        e.run(&closed).await;
        let reply = closed.recorded();
        assert_eq!(reply.text(), "⚠️ Questo sondaggio non è più attivo.");
        assert_eq!((reply.embeds, reply.components), (Some(vec![]), Some(vec![])));
    }

    #[tokio::test(start_paused = true)]
    async fn hub_support_opens_the_v2_card_or_invites_to_propose() {
        let e = env();
        let none = FakeIo::new("hub:support", 7);
        e.run(&none).await;
        let reply = none.recorded();
        assert!(reply.text().starts_with("Non ci sono ancora proposte approvate da supportare."));
        assert_eq!(reply.component_ids(), ["hub:suggest-first"]);

        e.approved(1, "Among Us");
        e.approved(2, "Jackbox");
        let io = FakeIo::new("hub:support", 7);
        e.run(&io).await;
        let reply = io.recorded();
        let v2 = reply.v2.expect("scheda V2");
        assert_eq!(v2[0]["type"], 17, "riquadro con la scheda");
        assert!(v2[0]["components"][2]["content"].as_str().unwrap().starts_with("## 🎮 Jackbox"), "la proposta più recente per prima");
        assert_eq!(v2.len(), 3, "riquadro, menu e pulsante");
    }

    #[tokio::test(start_paused = true)]
    async fn support_select_switches_proposal_or_reports_a_missing_one() {
        let e = env();
        let first = e.approved(1, "Among Us");
        e.approved(2, "Jackbox");
        let io = FakeIo::new("hub:support-select", 7).value(first);
        e.run(&io).await;
        assert!(matches!(io.only(), Event::Update(_)));
        let v2 = io.recorded().v2.unwrap();
        assert!(v2[0]["components"][2]["content"].as_str().unwrap().starts_with("## 🎮 Among Us"));
        assert_eq!(v2[2]["components"][0]["custom_id"], format!("hub:support-confirm:{first}"));

        let gone = FakeIo::new("hub:support-select", 7).value(999);
        e.run(&gone).await;
        assert_eq!(gone.recorded().v2.unwrap(), json!([{"type": 10, "content": "⚠️ Questa proposta non è più disponibile."}]).as_array().unwrap().clone());
    }

    #[tokio::test(start_paused = true)]
    async fn support_confirm_registers_the_support_and_refreshes_the_dashboard() {
        let e = env();
        let id = e.approved(1, "Among Us");
        let io = FakeIo::new(&format!("hub:support-confirm:{id}"), 7);
        e.run(&io).await;
        assert!(io.deferred());
        assert_eq!(io.recorded().text(), "✅ Hai supportato **Among Us**. Grazie!");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(e.db.get_setting("dashboard_message").unwrap().is_some(), "dashboard pubblicata");
        let again = FakeIo::new(&format!("hub:support-confirm:{id}"), 7);
        e.run(&again).await;
        assert_eq!(again.recorded().text(), "⚠️ Hai già supportato questa proposta.");
    }

    #[tokio::test(start_paused = true)]
    async fn hub_more_shows_each_section_privately() {
        let e = env();
        for (key, title) in [
            ("leaderboard", "📊 Classifica della settimana"),
            ("contributors", "🏆 Classifica utenti"),
            ("history", "📚 Hall of Fame"),
            ("records", "🏅 Record della community"),
            ("profile", "👤 Il mio stato"),
        ] {
            let io = FakeIo::new("hub:more", 7).value(key);
            e.run(&io).await;
            assert!(matches!(io.only(), Event::Respond(_)), "{key}");
            assert_eq!(io.recorded().embeds.unwrap()[0]["title"], title, "{key}");
        }
        let unknown = FakeIo::new("hub:more", 7).value("boh");
        e.run(&unknown).await;
        assert!(unknown.events().is_empty(), "chiave sconosciuta: nessuna risposta");
    }

    #[tokio::test(start_paused = true)]
    async fn suggest_buttons_open_the_form() {
        let e = env();
        for id in ["hub:suggest", "hub:suggest-first"] {
            let io = FakeIo::new(id, 7);
            e.run(&io).await;
            let Event::Modal(modal) = io.only() else { panic!("attesa una maschera") };
            assert_eq!(modal["custom_id"], "hub:suggest-modal");
        }
    }

    // --- pannello staff --------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn staff_panel_opens_only_for_staff_with_channels_and_proposals() {
        let e = env();
        let outsider = FakeIo::new("x", 5);
        e.interactions.open_staff_panel(&outsider).await.unwrap();
        assert_eq!(outsider.recorded().text(), "⚠️ Questo pannello è riservato allo staff.");

        let staff = || FakeIo::new("x", 9).staff();
        let none = staff();
        e.interactions.open_staff_panel(&none).await.unwrap();
        assert_eq!(none.recorded().text(), "⚠️ Non ci sono proposte in attesa di selezione.");

        e.db.execute("DELETE FROM settings WHERE key='staff_channel'", []).unwrap();
        let unconfigured = staff();
        e.interactions.open_staff_panel(&unconfigured).await.unwrap();
        assert_eq!(unconfigured.recorded().text(), "⚠️ Prima configura i canali con /config.");
        e.db.set_setting("staff_channel", 600).unwrap();

        e.approved(1, "Among Us");
        let io = staff();
        e.interactions.open_staff_panel(&io).await.unwrap();
        let reply = io.recorded();
        assert_eq!(reply.embeds.as_ref().unwrap()[0]["title"], "🛠️ Selezione proposte");
        assert_eq!(reply.component_ids(), ["hub:staff-proposal-select", "hub:staff-pick"]);
        assert_eq!(reply.components.unwrap()[1]["components"][0]["disabled"], true, "pulsante spento finché non si sceglie");
    }

    #[tokio::test(start_paused = true)]
    async fn staff_select_enables_the_pick_button_and_pick_runs_the_flow() {
        let e = env();
        let id = e.approved(1, "Among Us");
        let select = FakeIo::new("hub:staff-proposal-select", 9).staff().value(id);
        e.run(&select).await;
        let reply = select.recorded();
        assert!(matches!(select.only(), Event::Update(_)));
        assert_eq!(reply.text(), "✅ Selezionata: **Among Us**. Conferma per aggiungerla al ballottaggio.");
        assert_eq!(reply.component_ids(), ["hub:staff-proposal-select".to_string(), format!("hub:staff-pick:{id}")]);
        assert_eq!(reply.components.unwrap()[1]["components"][0]["disabled"], false);

        let pick = FakeIo::new(&format!("hub:staff-pick:{id}"), 9).staff();
        e.run(&pick).await;
        assert!(pick.deferred());
        assert_eq!(pick.recorded().text(), "✅ **Among Us** è stata aggiunta al ballottaggio.");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let audit: String = e.db.with_conn(|c| c.query_row("SELECT details FROM staff_audit WHERE action='proposal_selected'", [], |r| r.get(0)).unwrap());
        assert!(audit.contains(r#""source": "staff_panel""#), "{audit}");
    }

    #[tokio::test(start_paused = true)]
    async fn staff_panel_controls_refuse_outsiders_and_unselected_picks() {
        let e = env();
        let id = e.approved(1, "Among Us");
        for io in [FakeIo::new("hub:staff-proposal-select", 5).value(id), FakeIo::new(&format!("hub:staff-pick:{id}"), 5)] {
            e.run(&io).await;
            assert_eq!(io.recorded().text(), "⚠️ Questo pannello è riservato allo staff.");
        }
        assert_eq!(e.count("SELECT COUNT(*) FROM suggestions WHERE status='picked'"), 0);
        let nothing = FakeIo::new("hub:staff-pick", 9).staff();
        e.run(&nothing).await;
        assert_eq!(nothing.recorded().text(), "⚠️ Prima seleziona una proposta.");
    }

    // --- invio di una proposta -------------------------------------------------------------

    fn submit(user: i64, name: &str, link: &str) -> FakeIo {
        FakeIo::new("hub:suggest-modal", user).field("name", name).field("tipo", "Gioco").field("link", link)
    }

    #[tokio::test(start_paused = true)]
    async fn new_proposal_is_registered_with_an_opt_in_offer_and_a_review_card() {
        let e = env();
        let io = submit(7, "Among Us", "");
        e.run(&io).await;
        assert!(matches!(io.only(), Event::Respond(_)));
        let reply = io.recorded();
        assert!(reply.text().starts_with("✅ Proposta registrata: **Among Us** (Gioco)."));
        assert!(reply.text().ends_with("entra nel ballottaggio o vince?"));
        assert_eq!(reply.component_ids(), ["flow:optin:yes:7", "flow:optin:no:7"]);
        assert_eq!(reply.labels(), ["Sì, avvisami", "No, grazie"]);
        assert!(e.api.calls().iter().any(|c| matches!(c, Call::Send { channel: 600, .. })), "{:?}", e.api.calls());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(e.db.get_setting("dashboard_message").unwrap().is_some());
        assert_eq!(e.count("SELECT COUNT(*) FROM suggestion_review_messages"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_opt_in_offer_is_made_only_once_and_answers_update_the_message() {
        let e = env();
        let first = submit(7, "Among Us", "");
        e.run(&first).await;
        let confirmation = first.recorded().text().to_string();

        let yes = FakeIo::new("flow:optin:yes:7", 7).content(&confirmation);
        e.run(&yes).await;
        assert!(matches!(yes.only(), Event::Update(_)));
        let reply = yes.recorded();
        assert_eq!(reply.components, Some(vec![]), "pulsanti tolti");
        assert_eq!(
            reply.text(),
            format!("{}\n\n🔔 Fatto! Puoi cambiare idea quando vuoi con `/notifiche`.", confirmation.split("\n\n🔔 Vuoi").next().unwrap())
        );
        assert_eq!(e.count("SELECT dm_enabled FROM user_notification_preferences WHERE user_id=7"), 1);

        let second = submit(7, "Jackbox", "");
        e.run(&second).await;
        assert!(second.recorded().component_ids().is_empty(), "ha già scelto: nessuna domanda");
        assert!(!second.recorded().text().contains("🔔"));

        let no = FakeIo::new("flow:optin:no:8", 8).content("✅ ok\n\n🔔 Vuoi un messaggio privato quando la tua proposta viene approvata, entra nel ballottaggio o vince?");
        e.run(&no).await;
        assert!(no.recorded().text().ends_with("👌 Nessuna notifica. Puoi attivarle quando vuoi con `/notifiche`."));
        assert_eq!(e.count("SELECT dm_enabled FROM user_notification_preferences WHERE user_id=8"), 0);
        let intruder = FakeIo::new("flow:optin:yes:7", 99);
        e.run(&intruder).await;
        assert_eq!(intruder.recorded().text(), "⚠️ Questi bottoni sono per chi ha inviato la proposta.");
        assert_eq!(e.count("SELECT COUNT(*) FROM user_notification_preferences WHERE user_id=99"), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn validation_errors_are_shown_without_buttons_or_side_effects() {
        let e = env();
        let io = submit(7, "x", "");
        e.run(&io).await;
        assert_eq!(io.recorded().text(), "❌ Il nome deve contenere da 2 a 80 caratteri.");
        assert!(io.recorded().component_ids().is_empty());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(e.api.calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn similar_proposals_ask_what_you_meant_and_supporting_works() {
        let e = env();
        let approved = e.approved(1, "Jackbox Party");
        let io = submit(2, "Jackbox Party Pack", "");
        e.run(&io).await;
        let reply = io.recorded();
        assert_eq!(
            reply.text(),
            "🤔 Esiste già qualcosa di simile:\n• **Jackbox Party** (Gioco)\n\nIntendevi una di queste? Sostenerla aiuta a farla arrivare al ballottaggio."
        );
        assert_eq!(reply.labels(), ["👍 Supporta «Jackbox Party»", "È diversa, creala"]);
        assert_eq!(e.count("SELECT COUNT(*) FROM proposals WHERE user_id=2"), 0, "nulla viene creato finché non scegli");

        let ids = reply.component_ids();
        let click = FakeIo::new(&ids[0], 2);
        e.run(&click).await;
        assert!(matches!(click.only(), Event::Update(_)));
        assert_eq!(click.recorded().text(), "✅ Hai supportato **Jackbox Party**. Grazie!");
        assert_eq!(click.recorded().components, Some(vec![]));
        assert_eq!(e.count(&format!("SELECT COUNT(*) FROM suggestion_supports WHERE suggestion_id={approved} AND user_id=2")), 1);
        let again = FakeIo::new(&ids[0], 2);
        e.run(&again).await;
        assert_eq!(again.recorded().text(), "⚠️ Questa richiesta è scaduta: invia di nuovo la proposta.", "la richiesta è usa-e-getta");
    }

    #[tokio::test(start_paused = true)]
    async fn create_anyway_and_joining_a_pending_proposal_register_the_proposal() {
        let e = env();
        e.approved(1, "Jackbox Party");
        let io = submit(2, "Jackbox Party Pack", "https://example.com/jpp");
        e.run(&io).await;
        let create = FakeIo::new(&io.recorded().component_ids()[1], 2);
        e.run(&create).await;
        assert!(matches!(create.only(), Event::Update(_)));
        let reply = create.recorded();
        assert!(reply.text().starts_with("✅ Proposta registrata: **Jackbox Party Pack** (Gioco)."), "{}", reply.text());
        assert_eq!(reply.embeds, Some(vec![]));
        let link: Option<String> = e.db.with_conn(|c| c.query_row("SELECT link FROM suggestions WHERE name='Jackbox Party Pack'", [], |r| r.get(0)).unwrap());
        assert_eq!(link.as_deref(), Some("https://example.com/jpp"), "il link della richiesta si conserva");

        let pending = register_suggestion(&e.db, 3, "Karaoke Night", "Gioco", None, clock()).created_id.unwrap();
        let io = submit(4, "Karaoke Night 2", "");
        e.run(&io).await;
        assert_eq!(io.recorded().labels()[0], "➕ Unisciti a «Karaoke Night»");
        let join = FakeIo::new(&io.recorded().component_ids()[0], 4);
        e.run(&join).await;
        assert!(join.recorded().text().contains("**Karaoke Night**"), "{}", join.recorded().text());
        assert_eq!(e.count(&format!("SELECT COUNT(*) FROM proposals WHERE suggestion_id={pending}")), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_proposer_can_answer_and_requests_expire() {
        let e = env();
        e.approved(1, "Jackbox Party");
        let io = submit(2, "Jackbox Party Pack", "");
        e.run(&io).await;
        let ids = io.recorded().component_ids();
        let intruder = FakeIo::new(&ids[1], 99);
        e.run(&intruder).await;
        assert_eq!(intruder.recorded().text(), "⚠️ Questi bottoni sono per chi ha inviato la proposta.");
        let owner = FakeIo::new(&ids[1], 2);
        e.run(&owner).await;
        assert!(owner.recorded().text().starts_with("✅ Proposta registrata"), "l'intruso non consuma la richiesta");

        let mut short_lived = env();
        short_lived.interactions.pending_ttl = Duration::ZERO;
        short_lived.approved(1, "Jackbox Party");
        let io = submit(2, "Jackbox Party Pack", "");
        short_lived.run(&io).await;
        let late = FakeIo::new(&io.recorded().component_ids()[1], 2);
        short_lived.run(&late).await;
        assert_eq!(late.recorded().text(), "⚠️ Questa richiesta è scaduta: invia di nuovo la proposta.");
    }

    // --- smistamento -----------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn unknown_custom_ids_are_ignored() {
        let e = env();
        for id in ["", "qualcosa", "vote:abc", "hub:boh", "flow:sim:1", "flow:optin:maybe:1", "gamenight:event:3", "review:review:1"] {
            let io = FakeIo::new(id, 7);
            e.run(&io).await;
            assert!(io.events().is_empty(), "{id:?}: {:?}", io.events());
        }
    }

    #[test]
    fn flow_custom_ids_round_trip_and_reject_garbage() {
        assert_eq!(FlowComponent::parse(&FlowComponent::similar_id(12, 2)), Some(FlowComponent::Similar { nonce: 12, index: 2 }));
        assert_eq!(FlowComponent::parse(&FlowComponent::create_id(99)), Some(FlowComponent::Create { nonce: 99 }));
        assert_eq!(FlowComponent::parse(&FlowComponent::opt_in_id(true, 5)), Some(FlowComponent::OptIn { enable: true, owner: 5 }));
        assert_eq!(FlowComponent::parse(&FlowComponent::opt_in_id(false, 5)), Some(FlowComponent::OptIn { enable: false, owner: 5 }));
        for garbage in ["flow:", "flow:sim:1", "flow:sim:x:1", "flow:sim:1:2:3", "flow:create:", "flow:create:-1", "flow:optin:yes", "flow:optin:maybe:1", "flow:other:1", "sim:1:2"] {
            assert_eq!(FlowComponent::parse(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn every_custom_id_the_bot_creates_is_handled_by_the_dispatcher() {
        // Ogni ID costruito altrove deve avere un percorso nel dispatcher (altrimenti il clic non fa nulla).
        use crate::hub::{staff_pick_selected_button, support_confirm_custom_id};
        use crate::polls::{my_votes_custom_id, my_votes_select_custom_id, vote_custom_id};
        use crate::review::{reject_modal_custom_id, review_custom_id};
        let handled = |id: &str| {
            PollComponent::parse(id).is_some()
                || parse_review_custom_id(id).is_some()
                || parse_reject_modal_id(id).is_some()
                || id == SUGGESTION_MODAL_ID
                || parse_support_confirm(id).is_some()
                || parse_staff_pick(id).is_some()
                || HubComponent::parse(id).is_some()
                || FlowComponent::parse(id).is_some()
        };
        let mut ids = vec![
            vote_custom_id(1),
            my_votes_custom_id(1),
            my_votes_select_custom_id(1),
            review_custom_id(ReviewAction::Approve, 1),
            review_custom_id(ReviewAction::Reject, 1),
            reject_modal_custom_id(1),
            SUGGESTION_MODAL_ID.to_string(),
            support_confirm_custom_id(1),
            staff_pick_selected_button(1).custom_id,
            FlowComponent::similar_id(1, 0),
            FlowComponent::create_id(1),
            FlowComponent::opt_in_id(true, 1),
        ];
        ids.extend(
            [
                HubComponent::Vote,
                HubComponent::Suggest,
                HubComponent::Support,
                HubComponent::More,
                HubComponent::PollSelect,
                HubComponent::StaffProposalSelect,
                HubComponent::StaffPick,
                HubComponent::SuggestFirst,
                HubComponent::SupportSelect,
            ]
            .map(|component| component.custom_id().to_string()),
        );
        for id in ids {
            assert!(handled(&id), "nessun gestore per {id}");
        }
    }

    // --- Game Night ------------------------------------------------------------------------

    /// Domenica 2027-02-28, 18:00 (Roma): lo stesso istante di `golden_gamenight.py`.
    fn gamenight_clock() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2027, 2, 28, 18, 0, 0).unwrap()
    }

    /// Gli eventi di un'interazione nel formato `calls` di `golden_gamenight.py`.
    fn gamenight_calls(io: &FakeIo) -> Vec<Value> {
        io.events()
            .into_iter()
            .map(|event| match event {
                Event::Defer => json!(["defer", true, true]),
                Event::Respond(r) => json!(["send_message", r.text(), true]),
                Event::Followup(r) => json!(["followup", r.text(), true]),
                Event::EditSource(text) => json!(["message_edit", text, null]),
                Event::Modal(modal) => {
                    let fields: Vec<Value> = modal["components"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|row| {
                            let input = &row["components"][0];
                            json!([
                                input["label"],
                                input.get("value").cloned().unwrap_or(Value::Null),
                                input.get("min_length").cloned().unwrap_or(Value::Null),
                                input["max_length"],
                                input.get("required").cloned().unwrap_or(json!(true)),
                                input["style"],
                            ])
                        })
                        .collect();
                    json!(["send_modal", modal["title"], fields])
                }
                other => panic!("evento inatteso: {other:?}"),
            })
            .collect()
    }

    fn unix(iso: &str) -> i64 {
        DateTime::parse_from_rfc3339(iso).unwrap().timestamp()
    }

    /// Tutti i casi di pulsante e maschera di `golden_gamenight.py`, nello stesso ordine.
    #[tokio::test(start_paused = true)]
    async fn gamenight_button_and_modal_match_the_python_implementation() {
        use crate::discord_api::ChannelKind;
        let golden: Value = serde_json::from_str(include_str!("golden/gamenight_expected.json")).unwrap();
        let golden = &golden["flows"];
        let e = env_at(gamenight_clock);
        e.db.set_setting("week_winner:7", r#"{"suggestion_id": 1, "name": "Among Us", "votes": 3, "tie_break": null}"#).unwrap();

        let click = async |e: &Env, week: i64, staff: bool| {
            let io = FakeIo::new(&format!("gamenight:create:{week}"), 99);
            let io = if staff { io.staff() } else { io };
            e.run(&io).await;
            gamenight_calls(&io)
        };
        let mut logged = 0;
        let submit = async |e: &Env, week: i64, start: &str, description: &str, guild: bool, logged: &mut usize| {
            let io = FakeIo::new(&format!("gamenight:modal:{week}"), 99).staff().field("start", start).field("description", description);
            let io = if guild { io } else { io.no_guild() };
            let before = e.api.scheduled_events().len();
            e.run(&io).await;
            tokio::time::sleep(Duration::from_secs(2)).await; // log staff raggruppato
            let events: Vec<Value> = e.api.scheduled_events()[before..]
                .iter()
                .map(|(_, ev)| {
                    json!({
                        "name": ev.name, "start": ev.start_unix, "end": ev.end_unix, "channel": ev.channel_id == 700,
                        "entity_type": if ev.stage { "stage_instance" } else { "voice" }, "privacy": "guild_only",
                        "description": ev.description, "reason": ev.reason,
                    })
                })
                .collect();
            let lines: Vec<String> = e
                .sender
                .sent()
                .into_iter()
                .filter_map(|s| match s {
                    crate::notify::testing::Sent::Channel { channel_id: 600, content, .. } => Some(content),
                    _ => None,
                })
                .skip(*logged)
                .flat_map(|content| content.lines().map(str::to_string).collect::<Vec<_>>())
                .filter(|line| line.starts_with('📅')) // il golden registra solo il log staff, non l'audit
                .collect();
            *logged = e.sender.sent().iter().filter(|s| matches!(s, crate::notify::testing::Sent::Channel { channel_id: 600, .. })).count();
            json!({"calls": gamenight_calls(&io), "events": events, "staff_log": lines})
        };
        let settings = |e: &Env| -> Value {
            e.db.with_conn(|c| {
                let mut statement = c
                    .prepare("SELECT key,value FROM settings WHERE key LIKE 'scheduled_event%' OR key LIKE 'week_event_created:%' ORDER BY key")
                    .unwrap();
                let rows: Vec<Value> = statement.query_map([], |r| Ok(json!([r.get::<_, String>(0)?, r.get::<_, String>(1)?]))).unwrap().map(Result::unwrap).collect();
                Value::Array(rows)
            })
        };

        let mut actual = serde_json::Map::new();
        actual.insert("button_not_staff".into(), json!(click(&e, 7, false).await));
        actual.insert("button_no_winner".into(), json!(click(&e, 99, true).await));
        actual.insert("button_ok".into(), json!(click(&e, 7, true).await));
        actual.insert("bad_format".into(), submit(&e, 7, "domani sera", "", true, &mut logged).await);
        actual.insert("past".into(), submit(&e, 7, "01/03/2027 18:04", "", true, &mut logged).await);
        actual.insert("too_far".into(), submit(&e, 7, "30/04/2027 18:01", "", true, &mut logged).await);
        actual.insert("no_guild".into(), submit(&e, 7, "05/03/2027 21:00", "", false, &mut logged).await);
        actual.insert("no_channel".into(), submit(&e, 7, "05/03/2027 21:00", "", true, &mut logged).await);
        e.db.set_setting("game_night_channel", 700).unwrap();
        actual.insert("text_channel".into(), submit(&e, 7, "05/03/2027 21:00", "", true, &mut logged).await);
        actual.insert("missing_channel".into(), submit(&e, 7, "05/03/2027 21:00", "", true, &mut logged).await);
        e.api.set_channel_kind(700, ChannelKind::Voice);
        e.api.fail_event(Some(crate::discord_api::ApiError::Http("boom".into())));
        actual.insert("http_error".into(), submit(&e, 7, "05/03/2027 21:00", "", true, &mut logged).await);
        e.api.fail_event(None);
        actual.insert("settings_after_error".into(), settings(&e));
        actual.insert("voice_success".into(), submit(&e, 7, "05/03/2027 21:00", "", true, &mut logged).await);
        actual.insert("settings_after_voice".into(), settings(&e));
        actual.insert("button_already".into(), json!(click(&e, 7, true).await));
        e.db.set_setting("week_winner:12", &format!(r#"{{"suggestion_id": 5, "name": "Gartic Phone {}", "votes": 9, "tie_break": "anzianità"}}"#, "x".repeat(120))).unwrap();
        e.api.set_channel_kind(700, ChannelKind::Stage);
        let long_description = format!("  Serata speciale!  \n{}", "d".repeat(600));
        actual.insert("stage_success".into(), submit(&e, 12, "28/03/2027 03:00", &long_description, true, &mut logged).await);
        e.api.set_channel_kind(700, ChannelKind::Voice);
        actual.insert("gap_success".into(), submit(&e, 12, "28/03/2027 02:30", "", true, &mut logged).await);
        actual.insert("settings_after_stage".into(), settings(&e));
        let audit: Vec<Value> = e.db.with_conn(|c| {
            let mut statement = c
                .prepare("SELECT actor_id,action,entity_type,entity_id,reason,details FROM staff_audit WHERE action='game_night_event_created' ORDER BY id")
                .unwrap();
            statement
                .query_map([], |r| {
                    Ok(json!([r.get::<_, Option<i64>>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, String>(5)?]))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect()
        });
        actual.insert("audit".into(), json!(audit));
        let ids: Vec<u64> = e.api.calls().into_iter().filter_map(|c| match c { Call::ScheduledEvent { event, .. } => Some(event), _ => None }).collect();
        assert_eq!(ids.len(), 3);
        actual.insert("proposal_names".into(), json!(ids.iter().map(|id| crate::gamenight::event_proposal_name(&e.db, *id).unwrap()).collect::<Vec<_>>()));
        actual.insert("created_count".into(), json!(ids.len()));

        // Gli ID degli eventi sono quelli del finto Discord: si sostituiscono con i segnaposto del golden.
        let mut text = serde_json::to_string(&Value::Object(actual)).unwrap();
        for (number, id) in ids.iter().enumerate() {
            text = text.replace(&id.to_string(), &format!("{{id{number}}}"));
        }
        let actual: Value = serde_json::from_str(&text).unwrap();

        for (name, expected) in golden.as_object().unwrap() {
            if name == "no_message" {
                continue;
            }
            let mut expected = expected.clone();
            // Python scrive inizio e fine come ISO 8601; qui sono secondi Unix (l'ora inesistente
            // delle 02:30 ha la stessa istante di Python, solo scritta diversamente).
            if let Some(events) = expected.get_mut("events").and_then(Value::as_array_mut) {
                for event in events {
                    for key in ["start", "end"] {
                        event[key] = json!(unix(event[key].as_str().unwrap()));
                    }
                }
            }
            assert_eq!(actual[name], expected, "caso «{name}» diverso da Python");
        }
    }

    #[test]
    fn gamenight_custom_ids_round_trip() {
        use crate::gamenight::{create_button_id, event_modal_id, parse_create_button, parse_event_modal};
        assert_eq!(parse_create_button(&create_button_id(7)), Some(7));
        assert_eq!(parse_event_modal(&event_modal_id(12)), Some(12));
        for garbage in ["gamenight:create:", "gamenight:create:x", "gamenight:create:-1", "gamenight:create:1:2", "gamenight:modal:7", "vote:7"] {
            assert_eq!(parse_create_button(garbage), None, "{garbage}");
        }
        assert_eq!(parse_event_modal("gamenight:create:7"), None);
    }

    // --- comandi slash ---------------------------------------------------------------------

    async fn picked(e: &Env, name: &str) -> i64 {
        let id = e.approved(1, name);
        e.interactions.services.pick_suggestion(99, id, None, None, clock(), async |_text: String| {}).await;
        id
    }

    fn audit_rows(e: &Env, action: &str) -> Vec<(Option<i64>, Option<String>, Option<String>, String)> {
        e.db.with_conn(|c| {
            let mut statement = c
                .prepare("SELECT actor_id, entity_id, reason, details FROM staff_audit WHERE action=? ORDER BY id")
                .unwrap();
            statement
                .query_map([action], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        })
    }

    /// Il testo di un messaggio, ignorando `defer`.
    fn last_text(io: &FakeIo) -> String {
        match io.events().into_iter().rev().find(|event| !matches!(event, Event::Defer)).expect("nessuna risposta") {
            Event::Respond(r) | Event::RespondPublic(r) | Event::Update(r) | Event::Followup(r) | Event::EditResponse(r) => r.text().to_string(),
            other => panic!("atteso un testo, trovato {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn staff_command_offers_the_two_tools_and_only_to_their_owner() {
        let e = env();
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_staff(&io).await.unwrap();
        let reply = io.recorded();
        assert!(reply.text().starts_with("🛠️ **Strumenti staff**\n• **Selezione proposte**"));
        assert!(reply.text().ends_with("con i bottoni Approva/Rifiuta."));
        assert_eq!(reply.labels(), ["Selezione proposte", "Centro controllo"]);
        assert_eq!(reply.component_ids(), ["staff:tools:proposals:99", "staff:tools:control:99"]);

        // Un altro utente, o chi non è più staff, riceve l'avviso.
        let stranger = FakeIo::new("staff:tools:proposals:99", 5).staff();
        e.run(&stranger).await;
        assert_eq!(last_text(&stranger), "⚠️ Questi strumenti sono riservati allo staff.");
        let demoted = FakeIo::new("staff:tools:proposals:99", 99);
        e.run(&demoted).await;
        assert_eq!(last_text(&demoted), "⚠️ Questi strumenti sono riservati allo staff.");

        // «Selezione proposte» apre il pannello (qui: nessuna proposta in attesa).
        let panel = FakeIo::new("staff:tools:proposals:99", 99).staff();
        e.run(&panel).await;
        assert_eq!(last_text(&panel), "⚠️ Non ci sono proposte in attesa di selezione.");
        e.approved(1, "Among Us");
        let panel = FakeIo::new("staff:tools:proposals:99", 99).staff();
        e.run(&panel).await;
        assert!(panel.recorded().embeds.is_some(), "il pannello ha un embed con le proposte");

        // «Centro controllo» apre il centro di controllo (vedi i test del centro controllo).
        let control = FakeIo::new("staff:tools:control:99", 99).staff();
        e.run(&control).await;
        assert_eq!(control.recorded().embeds.unwrap()[0]["title"], "🛠️ Centro controllo staff");
    }

    #[tokio::test(start_paused = true)]
    async fn history_command_lists_closed_weeks_or_says_there_are_none() {
        let e = env();
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_history(&io).await.unwrap();
        assert_eq!(last_text(&io), "Nessuna settimana in archivio.");
        e.db
            .execute(
                "INSERT INTO history(week,closed_at,data) VALUES (5,'2026-10-26T12:00:00+01:00','{\"polls\": [], \"suggestions\": [], \"participants\": 3}')",
                [],
            )
            .unwrap();
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_history(&io).await.unwrap();
        assert!(last_text(&io).contains('5'));
    }

    #[tokio::test(start_paused = true)]
    async fn unpick_asks_for_confirmation_then_removes_the_proposal() {
        let e = env();
        let id = picked(&e, "Among Us").await;
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_unpick(&io, id, Some("doppione".into())).await.unwrap();
        let ask = io.recorded();
        assert!(ask.text().starts_with("⚠️ Togliere la proposta dal ballottaggio cancella i voti"));
        assert_eq!(ask.labels(), ["Togli dal ballottaggio", "Annulla"]);
        let ids = ask.component_ids();
        assert_eq!(e.count("SELECT COUNT(*) FROM suggestions WHERE status='picked'"), 1, "nulla cambia prima della conferma");

        // Un altro utente non può rispondere e la richiesta resta aperta.
        let stranger = FakeIo::new(&ids[0], 5);
        e.run(&stranger).await;
        assert_eq!(last_text(&stranger), "⚠️ Questa conferma è per chi ha avviato il comando.");

        let yes = FakeIo::new(&ids[0], 99).staff();
        e.run(&yes).await;
        let update = yes.recorded();
        assert_eq!(update.text(), "🗑️ **Among Us** tolta dal ballottaggio.");
        assert_eq!(update.components.as_deref(), Some(&[][..]), "i pulsanti spariscono");
        assert_eq!(e.count("SELECT COUNT(*) FROM suggestions WHERE status='picked'"), 0);
        let audit = audit_rows(&e, "proposal_unselected");
        assert_eq!(audit.len(), 1);
        assert_eq!((audit[0].0, audit[0].1.as_deref(), audit[0].2.as_deref()), (Some(99), Some(id.to_string().as_str()), Some("doppione")));
        assert_eq!(audit[0].3, format!(r#"{{"name": "Among Us", "week": 1}}"#));

        // Un secondo clic non ripete l'azione.
        let again = FakeIo::new(&ids[0], 99).staff();
        e.run(&again).await;
        assert!(last_text(&again).contains("scaduta"));
        assert_eq!(audit_rows(&e, "proposal_unselected").len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn unpick_can_be_cancelled_and_reports_missing_proposals() {
        let e = env();
        let id = picked(&e, "Among Us").await;
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_unpick(&io, id, None).await.unwrap();
        let ids = io.recorded().component_ids();
        let no = FakeIo::new(&ids[1], 99).staff();
        e.run(&no).await;
        assert_eq!(no.recorded().text(), "Operazione annullata.");
        assert_eq!(e.count("SELECT COUNT(*) FROM suggestions WHERE status='picked'"), 1);

        // Proposta che non è (più) nel ballottaggio.
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_unpick(&io, 9999, None).await.unwrap();
        let yes = FakeIo::new(&io.recorded().component_ids()[0], 99).staff();
        e.run(&yes).await;
        assert_eq!(yes.recorded().text(), "⚠️ Proposta non trovata tra quelle nel ballottaggio.");
        assert!(audit_rows(&e, "proposal_unselected").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn unpick_needs_the_channels_and_confirmations_expire() {
        let e = env();
        e.db.execute("DELETE FROM settings WHERE key='staff_channel'", []).unwrap();
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_unpick(&io, 1, None).await.unwrap();
        assert_eq!(io.recorded().text(), "⚠️ Prima configura i canali con /config.");
        assert!(io.recorded().components.is_none());

        let mut short = env();
        short.interactions.confirm_ttl = Duration::ZERO;
        let io = FakeIo::new("", 99).staff();
        short.interactions.command_unpick(&io, 1, None).await.unwrap();
        let click = FakeIo::new(&io.recorded().component_ids()[0], 99).staff();
        short.run(&click).await;
        assert!(click.recorded().text().contains("scaduta"));
    }

    #[tokio::test(start_paused = true)]
    async fn merge_asks_then_merges_audits_and_updates_both_cards() {
        let e = env();
        let keep = e.approved(1, "Among Us");
        let duplicate = e.approved(2, "Among Us Online");
        e.interactions.services.publisher.post_review_card(duplicate).await.unwrap();
        let (channel, message) = crate::review::review_message_location(&e.db, duplicate).unwrap().expect("scheda pubblicata");
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_merge(&io, keep, duplicate, Some("stesso gioco".into())).await.unwrap();
        let ask = io.recorded();
        assert!(ask.text().starts_with("🔀 Proposte, sostegni e link della duplicata passeranno alla principale"));
        assert_eq!(ask.labels(), ["Unisci", "Annulla"]);

        let yes = FakeIo::new(&ask.component_ids()[0], 99).staff();
        e.run(&yes).await;
        assert_eq!(e.count(&format!("SELECT COUNT(*) FROM suggestions WHERE id={duplicate}")), 0, "la duplicata è eliminata");
        assert_eq!(e.count(&format!("SELECT COUNT(*) FROM suggestions WHERE id={keep}")), 1);
        assert_eq!(yes.recorded().components.as_deref(), Some(&[][..]));
        let audit = audit_rows(&e, "proposals_merged");
        assert_eq!(audit.len(), 1);
        assert_eq!((audit[0].1.as_deref(), audit[0].2.as_deref()), (Some(keep.to_string().as_str()), Some("stesso gioco")));
        assert_eq!(
            audit[0].3,
            format!(r#"{{"source_id": {duplicate}, "source_name": "Among Us Online", "target_name": "Among Us"}}"#)
        );
        // La scheda della duplicata si chiude con la nota, anche se la proposta non esiste più.
        let stored = e.api.message(channel.parse().unwrap(), message.parse().unwrap()).expect("scheda ancora nel canale");
        assert_eq!(stored.content.as_deref(), Some("🔀 Unita in «Among Us» da <@99>."));
    }

    #[tokio::test(start_paused = true)]
    async fn merge_refusals_are_shown_and_not_audited() {
        let e = env();
        let a = e.approved(1, "Among Us");
        let film = register_suggestion(&e.db, 2, "Il Gladiatore", "Film", None, clock()).created_id.unwrap();
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_merge(&io, a, film, None).await.unwrap();
        let yes = FakeIo::new(&io.recorded().component_ids()[0], 99).staff();
        e.run(&yes).await;
        assert_eq!(yes.recorded().text(), "❌ Non puoi unire proposte di tipo diverso.");
        assert_eq!(e.count("SELECT COUNT(*) FROM suggestions"), 2);
        assert!(audit_rows(&e, "proposals_merged").is_empty());
    }

    fn with_reset_service(e: &Env) {
        let services = Services {
            db: e.db.clone(),
            notifier: e.interactions.services.notifier.clone(),
            publisher: Arc::clone(&e.interactions.services.publisher),
            scheduler: e.interactions.services.scheduler.clone(),
        };
        e.interactions.set_reset_service(Arc::new(crate::reset::ResetService::with_clock(services, Some(1), clock)));
    }

    #[tokio::test(start_paused = true)]
    async fn manual_reset_asks_with_a_single_button_then_runs_the_reset() {
        let e = env();
        with_reset_service(&e);
        e.approved(1, "Among Us");
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_manual_reset(&io).await.unwrap();
        let ask = io.recorded();
        assert_eq!(ask.text(), "⚠️ Vuoi davvero azzerare voti e proposte della settimana? I risultati verranno salvati nello storico.");
        assert_eq!(ask.labels(), ["Conferma reset"]);
        assert_eq!(e.db.current_week().unwrap(), 1);

        let yes = FakeIo::new(&ask.component_ids()[0], 99).staff();
        e.run(&yes).await;
        let events = yes.events();
        assert!(matches!(&events[0], Event::Update(r) if r.text() == "Reset in corso…" && r.components.as_deref() == Some(&[][..])), "{events:?}");
        assert!(matches!(&events[1], Event::EditResponse(r) if r.text() == "✅ Reset completato."), "{events:?}");
        assert_eq!(events.len(), 2);
        assert_eq!(e.db.current_week().unwrap(), 2);
        assert_eq!(audit_rows(&e, "weekly_reset_forced").len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn manual_reset_is_for_its_owner_and_runs_once() {
        let e = env();
        with_reset_service(&e);
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_manual_reset(&io).await.unwrap();
        let id = io.recorded().component_ids()[0].clone();
        let stranger = FakeIo::new(&id, 5).staff();
        e.run(&stranger).await;
        assert_eq!(last_text(&stranger), "⚠️ Questa conferma è per chi ha avviato il comando.");
        assert_eq!(e.db.current_week().unwrap(), 1);
        e.run(&FakeIo::new(&id, 99).staff()).await;
        e.run(&FakeIo::new(&id, 99).staff()).await;
        assert_eq!(e.db.current_week().unwrap(), 2, "il secondo clic non ripete il reset");
    }

    #[tokio::test(start_paused = true)]
    async fn manual_reset_reports_a_failure() {
        let e = env();
        with_reset_service(&e);
        e.db.execute("DROP TABLE web_sessions", []).unwrap(); // il reset fallisce a metà
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_manual_reset(&io).await.unwrap();
        let yes = FakeIo::new(&io.recorded().component_ids()[0], 99).staff();
        e.run(&yes).await;
        assert!(matches!(yes.events().last(), Some(Event::EditResponse(r)) if r.text() == "❌ Reset non riuscito, controlla i log"));
        assert_eq!(e.db.current_week().unwrap(), 1);
        assert!(audit_rows(&e, "weekly_reset_forced").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn pick_command_defers_and_answers_privately() {
        let e = env();
        let id = e.approved(1, "Among Us");
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_pick(&io, id, Some("È una bomba")).await.unwrap();
        assert!(io.deferred());
        assert_eq!(last_text(&io), "✅ **Among Us** è stata aggiunta al ballottaggio.");
        assert_eq!(e.count("SELECT COUNT(*) FROM suggestions WHERE status='picked'"), 1);
        let audit = audit_rows(&e, "proposal_selected");
        assert_eq!(audit[0].2.as_deref(), Some("È una bomba"));
        // Seconda volta: la proposta non è più selezionabile.
        let again = FakeIo::new("", 99).staff();
        e.interactions.command_pick(&again, id, None).await.unwrap();
        assert!(!last_text(&again).starts_with("✅"));
    }

    #[tokio::test(start_paused = true)]
    async fn suggest_command_registers_the_proposal() {
        let e = env();
        let io = FakeIo::new("", 7);
        e.interactions.command_suggest(&io, "Among Us", "Gioco", None).await.unwrap();
        assert_eq!(e.count("SELECT COUNT(*) FROM suggestions"), 1);
        assert!(last_text(&io).contains("Among Us"));
        // Tipo non valido: rifiutato con il messaggio di Python, senza proposte nuove.
        let bad = FakeIo::new("", 7);
        e.interactions.command_suggest(&bad, "Serie TV", "Serie", None).await.unwrap();
        assert_eq!(e.count("SELECT COUNT(*) FROM suggestions"), 1);
    }

    // --- /config e /feed-attivita ---------------------------------------------------------

    /// Mercoledì 2027-03-03, 10:00 (Roma): lo stesso istante di `golden_admin_config.py`.
    fn admin_clock() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2027, 3, 3, 10, 0, 0).unwrap()
    }

    fn rows(e: &Env, sql: &str, columns: usize) -> Value {
        e.db.with_conn(|c| {
            let mut statement = c.prepare(sql).unwrap();
            let rows: Vec<Value> = statement
                .query_map([], |row| {
                    let mut values = Vec::new();
                    for index in 0..columns {
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

    /// Tutti i 26 scenari di `golden_admin_config.py`, nello stesso ordine: testo della risposta,
    /// impostazioni, registro audit, scadenza dei sondaggi e che cosa è stato aggiornato.
    #[tokio::test(start_paused = true)]
    async fn config_and_feed_commands_match_the_python_implementation() {
        use crate::interactions::ConfigRequest;
        let golden: Vec<Value> = serde_json::from_str(include_str!("golden/admin_config_expected.json")).unwrap();
        let e = env_at(admin_clock);
        // Lo stato di partenza di Python: nessun canale configurato.
        e.db.execute("DELETE FROM settings WHERE key IN ('poll_channel','staff_channel')", []).unwrap();
        // `initialize` scrive la data di oggi: il golden è stato generato il 2026-11-11.
        e.db.execute("UPDATE settings SET value='2026-11-11' WHERE key IN ('awards_since','stats_since')", []).unwrap();

        for case in &golden {
            let label = case["label"].as_str().unwrap();
            let io = FakeIo::new("", 99);
            let io = if matches!(label, "non_admin" | "feed_non_admin") { io } else { io.staff() };
            let calls_before = e.api.calls().len();
            let request = |f: &dyn Fn(&mut ConfigRequest)| {
                let mut request = ConfigRequest::default();
                f(&mut request);
                request
            };
            let config = |r: ConfigRequest| e.interactions.command_config(&io, r);
            let feed = |enabled: bool, channel: Option<u64>, reason: Option<&str>| e.interactions.command_activity_feed(&io, enabled, channel, reason.map(str::to_string));
            match label {
                "riepilogo_iniziale" | "riepilogo_finale" => config(request(&|_| {})).await,
                "non_admin" => config(request(&|r| r.slots = Some(4))).await,
                "accoglienza_senza_canale" => config(request(&|r| r.welcome_enabled = Some(true))).await,
                "canali_base" => config(request(&|r| (r.poll_channel, r.staff_channel, r.reason) = (Some(111), Some(222), Some("setup".into())))).await,
                "benvenuto" => config(request(&|r| (r.event_report_channel, r.welcome_channel, r.welcome_enabled) = (Some(333), Some(444), Some(true)))).await,
                "accoglienza_spenta" => config(request(&|r| r.welcome_enabled = Some(false))).await,
                "ruolo_aggiunto" | "ruolo_rimosso" => config(request(&|r| r.staff_role = Some(555))).await,
                "ruoli_ordinati" => config(request(&|r| r.staff_role = Some(20))).await,
                "ruoli_ordinati_2" => config(request(&|r| r.staff_role = Some(3))).await,
                "ruolo_everyone" => config(request(&|r| r.staff_role = Some(1))).await,
                "slot_e_limiti" => config(request(&|r| (r.slots, r.max_votes, r.max_proposals) = (Some(4), Some(3), Some(7)))).await,
                "reset_venerdi_18" => config(request(&|r| (r.reset_weekday, r.reset_hour) = (Some(4), Some(18)))).await,
                "reset_solo_giorno" => config(request(&|r| r.reset_weekday = Some(0))).await,
                "reset_solo_ora" => config(request(&|r| r.reset_hour = Some(0))).await,
                "promemoria_spento" => config(request(&|r| r.reset_reminder = Some(false))).await,
                "promemoria_acceso_e_motivo" => config(request(&|r| (r.reset_reminder, r.reason) = (Some(true), Some("serve".into())))).await,
                "game_night" => config(request(&|r| r.game_night_channel = Some(666))).await,
                "tutto_insieme" => {
                    config(request(&|r| {
                        (r.poll_channel, r.slots, r.staff_role, r.game_night_channel, r.reset_hour) = (Some(112), Some(5), Some(1), Some(667), Some(9));
                    }))
                    .await
                }
                "feed_non_admin" | "feed_senza_canale" => feed(true, None, None).await,
                "feed_attivo" => feed(true, Some(777), Some("annunci")).await,
                "feed_spento_con_canale" => feed(false, None, None).await,
                "feed_spento_nuovo_canale" => feed(false, Some(778), None).await,
                "feed_attivo_canale_gia_noto" => feed(true, None, None).await,
                other => panic!("scenario sconosciuto: {other}"),
            }
            .unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await; // aggiornamenti e log staff

            let sent = json!([[last_text(&io), true]]);
            assert_eq!(sent, case["sent"], "{label}: risposta");
            let state = &case["state"];
            assert_eq!(rows(&e, "SELECT key,value FROM settings WHERE substr(key,1,10)<>'dashboard_' ORDER BY key", 2), state["settings"], "{label}: impostazioni");
            assert_eq!(
                rows(&e, "SELECT actor_id,action,entity_type,entity_id,reason,details FROM staff_audit ORDER BY id", 6),
                state["audit"],
                "{label}: audit"
            );
            // Il golden non esegue l'aggiornamento dei messaggi (che in Rust, vero, assegna la scadenza ai
            // sondaggi che non ce l'hanno): dove Python non ha una scadenza non si confronta.
            let mut polls = rows(&e, "SELECT id,active,closes_at FROM polls ORDER BY id", 3);
            for (poll, wanted) in polls.as_array_mut().unwrap().iter_mut().zip(state["polls"].as_array().unwrap()) {
                if wanted[2].is_null() {
                    poll[2] = Value::Null;
                }
            }
            assert_eq!(polls, state["polls"], "{label}: sondaggi");
            // Che cosa si è aggiornato: tutti i messaggi (sondaggi + dashboard) o solo la dashboard.
            let touched = e.api.calls().len() - calls_before;
            let expected = match (case["poll_channel_set"].as_bool().unwrap(), case["refresh"][0].as_str()) {
                (true, Some("all")) => "all",
                (true, Some("dashboard")) => "dashboard",
                _ => "none",
            };
            let actual = match touched {
                0 => "none",
                1 => "dashboard",
                _ => "all",
            };
            assert_eq!(actual, expected, "{label}: aggiornamento dei messaggi ({touched} chiamate)");
        }
    }

    // --- centro controllo -------------------------------------------------------------------

    use crate::control_center::MemberInfo;
    use crate::interactions::admin::{ControlComponent, panel_rows, proposal_search_modal};

    fn embed_title(reply: &Recorded) -> String {
        reply.embeds.as_ref().and_then(|embeds| embeds.first()).and_then(|e| e["title"].as_str()).unwrap_or_default().to_string()
    }

    /// Un componente del pannello nel formato di `golden_export_panel.py`.
    fn layout_of(rows: &[Value]) -> Vec<Value> {
        let mut items = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            for component in row["components"].as_array().unwrap() {
                items.push(match component["type"].as_u64().unwrap() {
                    3 => json!({
                        "kind": "string_select", "row": index, "placeholder": component["placeholder"],
                        "min": component["min_values"], "max": component["max_values"],
                        "options": component["options"].as_array().unwrap().iter()
                            .map(|o| json!([o["label"], o["value"], o["emoji"]["name"], o.get("default").cloned().unwrap_or(json!(false))]))
                            .collect::<Vec<_>>(),
                    }),
                    5 => json!({"kind": "user_select", "row": index, "placeholder": component["placeholder"], "min": component["min_values"], "max": component["max_values"], "options": null}),
                    2 => json!({"kind": "button", "row": index, "label": component["label"], "emoji": component["emoji"]["name"], "style": component["style"]}),
                    other => panic!("tipo di componente inatteso: {other}"),
                });
            }
        }
        items
    }

    /// Lo stesso layout di `StaffControlPanelView`: menu sulla prima riga, controlli della sezione
    /// sulla seconda, «Esporta» solo agli amministratori.
    #[test]
    fn panel_layout_matches_the_python_view() {
        let golden: Value = serde_json::from_str(include_str!("golden/export_panel_expected.json")).unwrap();
        for (key, expected) in golden["layouts"].as_object().unwrap() {
            let (section, can_export) = key.split_once(':').unwrap();
            let section = (section != "None").then_some(section);
            let rows: Vec<Value> = panel_rows(99, section, can_export == "True").iter().map(|r| serde_json::to_value(r).unwrap()).collect();
            let actual = layout_of(&rows);
            let expected: Vec<Value> = expected
                .as_array()
                .unwrap()
                .iter()
                .map(|item| {
                    let kind = item["kind"].as_str().unwrap();
                    if kind == "StaffSectionSelect" {
                        json!({"kind": "string_select", "row": item["row"], "placeholder": item["placeholder"], "min": item["min"], "max": item["max"], "options": item["options"]})
                    } else if kind == "StaffUserSelect" {
                        json!({"kind": "user_select", "row": item["row"], "placeholder": item["placeholder"], "min": item["min"], "max": item["max"], "options": null})
                    } else {
                        json!({"kind": "button", "row": item["row"], "label": item["label"], "emoji": item["emoji"], "style": item["style"]})
                    }
                })
                .collect();
            assert_eq!(actual, expected, "sezione {key}");
        }
    }

    #[test]
    fn proposal_search_modal_and_export_confirmation_match_the_python_views() {
        let golden: Value = serde_json::from_str(include_str!("golden/export_panel_expected.json")).unwrap();
        let modal = serde_json::to_value(proposal_search_modal()).unwrap();
        assert_eq!(modal["title"], golden["modal"]["title"]);
        let fields: Vec<Value> = modal["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let c = &row["components"][0];
                json!([c["label"], c["placeholder"], c.get("min_length").cloned().unwrap_or(Value::Null), c["max_length"], c.get("required").cloned().unwrap_or(json!(true)), c["style"]])
            })
            .collect();
        assert_eq!(json!(fields), golden["modal"]["fields"]);
    }

    #[test]
    fn control_custom_ids_round_trip_and_reject_garbage() {
        for component in [
            ControlComponent::Section { owner: 7 },
            ControlComponent::Search { owner: 7 },
            ControlComponent::User { owner: 7 },
            ControlComponent::Refresh { owner: 7 },
            ControlComponent::Export { owner: 7 },
            ControlComponent::ExportYes { owner: 7 },
            ControlComponent::ExportNo { owner: 7 },
        ] {
            assert_eq!(ControlComponent::parse(&component.id()), Some(component));
        }
        for garbage in ["ctl:", "ctl:section", "ctl:section:", "ctl:section:x", "ctl:other:1", "ctl:section:-1", "ctl:proposal-modal", "cmd:confirm:1:yes"] {
            assert_eq!(ControlComponent::parse(garbage), None, "{garbage}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn control_center_opens_for_staff_with_the_home_screen() {
        let e = env();
        let outsider = FakeIo::new("", 5);
        e.interactions.command_control_center(&outsider).await.unwrap();
        assert_eq!(last_text(&outsider), "⚠️ Questo centro di controllo è riservato allo staff.");

        let admin = FakeIo::new("", 99).staff();
        e.interactions.command_control_center(&admin).await.unwrap();
        let reply = admin.recorded();
        assert_eq!(embed_title(&reply), "🛠️ Centro controllo staff");
        assert_eq!(reply.component_ids(), ["ctl:section:99"]);

        // Anche il pulsante «Centro controllo» di /staff lo apre.
        let from_tools = FakeIo::new("staff:tools:control:99", 99).staff();
        e.run(&from_tools).await;
        assert_eq!(embed_title(&from_tools.recorded()), "🛠️ Centro controllo staff");
    }

    #[tokio::test(start_paused = true)]
    async fn section_menu_switches_screen_and_checks_the_owner_and_permissions() {
        let e = env();
        let select = |user: i64, section: &str| FakeIo::new("ctl:section:99", user).value(section);

        let stranger = select(5, "handbook").staff();
        e.run(&stranger).await;
        assert_eq!(last_text(&stranger), "⚠️ Questo pannello è riservato a chi l'ha aperto.");
        let demoted = select(99, "handbook");
        e.run(&demoted).await;
        assert_eq!(last_text(&demoted), "⚠️ Non hai più i permessi per usare il pannello.");

        for (section, title) in [
            ("handbook", "📖 Handbook contributor"),
            ("analytics", "📈 Community analytics"),
            ("proposals", "✨ Analisi proposte"),
            ("users", "👥 Analisi utenti"),
            ("system", "⚙️ Stato del sistema"),
        ] {
            let io = select(99, section).staff();
            e.run(&io).await;
            let reply = match io.only() {
                Event::Update(reply) => reply,
                other => panic!("atteso un aggiornamento, trovato {other:?}"),
            };
            assert_eq!(embed_title(&reply), title, "{section}");
            // Il menu ora ha la sezione scelta e i controlli giusti.
            let expected: Vec<Value> = panel_rows(99, Some(section), true).iter().map(|r| serde_json::to_value(r).unwrap()).collect();
            assert_eq!(reply.components, Some(expected), "{section}: componenti");
        }
        // Una sezione sconosciuta non fa nulla.
        let unknown = select(99, "altro").staff();
        e.run(&unknown).await;
        assert!(unknown.events().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn export_button_is_hidden_from_staff_who_are_not_admins() {
        let e = env();
        e.db.set_setting("staff_roles", "5").unwrap();
        let mut io = FakeIo::new("ctl:section:99", 99).value("system");
        io.roles = vec![5]; // staff per ruolo, ma non amministratore
        e.run(&io).await;
        let reply = match io.only() {
            Event::Update(reply) => reply,
            other => panic!("{other:?}"),
        };
        assert_eq!(reply.component_ids(), ["ctl:section:99", "ctl:refresh:99"], "senza «Esporta»");
    }

    #[tokio::test(start_paused = true)]
    async fn proposal_search_opens_a_modal_and_shows_the_analysis_privately() {
        let e = env();
        e.approved(1, "Among Us");
        let stranger = FakeIo::new("ctl:search:99", 5).staff();
        e.run(&stranger).await;
        assert_eq!(last_text(&stranger), "⚠️ Questa ricerca è riservata a chi ha aperto il pannello.");
        let demoted = FakeIo::new("ctl:search:99", 99);
        e.run(&demoted).await;
        assert_eq!(last_text(&demoted), "⚠️ Non hai i permessi per usare questa funzione.");

        let open = FakeIo::new("ctl:search:99", 99).staff();
        e.run(&open).await;
        assert!(matches!(open.only(), Event::Modal(modal) if modal["title"] == "Analisi proposta"));

        let submit = FakeIo::new("ctl:proposal-modal", 99).staff().field("name", "among us").field("tipo", "");
        e.run(&submit).await;
        assert_eq!(embed_title(&submit.recorded()), "✨ Analisi · among us");
        let missing = FakeIo::new("ctl:proposal-modal", 99).staff().field("name", "Minecraft").field("tipo", "Gioco");
        e.run(&missing).await;
        assert_eq!(embed_title(&missing.recorded()), "✨ Analisi proposta");
        let demoted = FakeIo::new("ctl:proposal-modal", 99).field("name", "Among Us");
        e.run(&demoted).await;
        assert_eq!(last_text(&demoted), "⚠️ Non hai più i permessi per consultare i dati staff.");
    }

    #[tokio::test(start_paused = true)]
    async fn user_select_shows_the_member_summary() {
        let e = env();
        let member = MemberInfo { id: 7, display_name: "Ada".into(), avatar_url: "https://cdn.example/7.png".into() };
        let stranger = FakeIo::new("ctl:user:99", 5).staff().selecting(member.clone());
        e.run(&stranger).await;
        assert_eq!(last_text(&stranger), "⚠️ Questa ricerca è riservata a chi ha aperto il pannello.");
        let demoted = FakeIo::new("ctl:user:99", 99).selecting(member.clone());
        e.run(&demoted).await;
        assert_eq!(last_text(&demoted), "⚠️ Non hai i permessi per usare questa funzione.");
        let ok = FakeIo::new("ctl:user:99", 99).staff().selecting(member);
        e.run(&ok).await;
        let reply = ok.recorded();
        assert_eq!(embed_title(&reply), "👤 Analisi utente · Ada");
        assert_eq!(reply.embeds.unwrap()[0]["thumbnail"]["url"], "https://cdn.example/7.png");
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_button_verifies_the_records_or_reports_the_failure() {
        let e = env();
        let stranger = FakeIo::new("ctl:refresh:99", 5).staff();
        e.run(&stranger).await;
        assert_eq!(last_text(&stranger), "⚠️ Questa verifica è riservata a chi ha aperto il pannello.");
        let ok = FakeIo::new("ctl:refresh:99", 99).staff();
        e.run(&ok).await;
        assert!(last_text(&ok).starts_with("✅ Classifica e Hall of Fame verificate"));
        e.db.execute("DROP TABLE community_records", []).unwrap();
        let broken = FakeIo::new("ctl:refresh:99", 99).staff();
        e.run(&broken).await;
        assert_eq!(last_text(&broken), "❌ Verifica non riuscita. Controlla i log del bot.");
    }

    #[tokio::test(start_paused = true)]
    async fn export_asks_for_confirmation_then_sends_the_redacted_file() {
        let e = env();
        e.interactions.services.db.set_setting("live_event_token", "segreto").unwrap();
        let stranger = FakeIo::new("ctl:export:99", 5).staff();
        e.run(&stranger).await;
        assert_eq!(last_text(&stranger), "⚠️ Questo export è riservato a chi ha aperto il pannello.");
        let not_admin = FakeIo::new("ctl:export:99", 99);
        e.run(&not_admin).await;
        assert_eq!(last_text(&not_admin), "⚠️ L'esportazione è riservata agli amministratori.");

        let ask = FakeIo::new("ctl:export:99", 99).staff();
        e.run(&ask).await;
        let reply = ask.recorded();
        assert!(reply.text().starts_with("L'esportazione crea un backup SQLite temporaneo"));
        assert_eq!(reply.labels(), ["Conferma export redatto", "Annulla"]);
        assert_eq!(reply.component_ids(), ["ctl:export-yes:99", "ctl:export-no:99"]);

        let stranger = FakeIo::new("ctl:export-yes:99", 5).staff();
        e.run(&stranger).await;
        assert_eq!(last_text(&stranger), "⚠️ Questa conferma è riservata a chi l'ha richiesta.");
        let not_admin = FakeIo::new("ctl:export-yes:99", 99);
        e.run(&not_admin).await;
        assert_eq!(last_text(&not_admin), "⚠️ L'esportazione è riservata agli amministratori.");

        let yes = FakeIo::new("ctl:export-yes:99", 99).staff();
        e.run(&yes).await;
        let events = yes.events();
        assert!(matches!(events[0], Event::Defer));
        let Event::File { content, filename, data } = &events[1] else { panic!("atteso il file, trovato {events:?}") };
        assert!(content.starts_with("✅ Export SQLite pronto."));
        assert_eq!(filename, "sword-bot-export-redatto.db");
        assert!(data.starts_with(b"SQLite format 3\0"));
        assert_eq!(events.len(), 2);
        let audit = audit_rows(&e, "redacted_database_exported");
        assert_eq!(audit.len(), 1);
        assert_eq!((audit[0].0, audit[0].1.as_deref()), (Some(99), None));
        let entity_type: String = e.db.with_conn(|c| c.query_row("SELECT entity_type FROM staff_audit WHERE action='redacted_database_exported'", [], |r| r.get(0)).unwrap());
        assert_eq!(entity_type, "database");

        let no = FakeIo::new("ctl:export-no:99", 99).staff();
        e.run(&no).await;
        let cancelled = match no.only() {
            Event::Update(reply) => reply,
            other => panic!("{other:?}"),
        };
        assert_eq!(cancelled.text(), "Export annullato.");
        assert_eq!(cancelled.components.as_deref(), Some(&[][..]));
        assert_eq!(cancelled.embeds.as_deref(), Some(&[][..]));
        let stranger = FakeIo::new("ctl:export-no:99", 5).staff();
        e.run(&stranger).await;
        assert_eq!(last_text(&stranger), "⚠️ Questa conferma è riservata a chi l'ha richiesta.");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_export_reports_the_error_and_is_not_audited() {
        let e = env();
        e.db.execute("DROP TABLE suggestion_supports", []).unwrap(); // la copia redatta non si può costruire
        let yes = FakeIo::new("ctl:export-yes:99", 99).staff();
        e.run(&yes).await;
        let events = yes.events();
        assert!(matches!(events[0], Event::Defer));
        assert!(matches!(&events[1], Event::Followup(r) if r.text() == "❌ Export non riuscito. Controlla i log del bot."), "{events:?}");
        assert!(audit_rows(&e, "redacted_database_exported").is_empty());
    }

    // --- /proposte e /proposta -----------------------------------------------------------

    fn proposals_env() -> Env {
        let e = env();
        e.db.execute_batch(include_str!("golden/cards_fixture.sql")).unwrap();
        e.db.execute_batch(include_str!("golden/proposals_list_fixture.sql")).unwrap();
        e
    }

    /// Un embed come lo scrive `golden_proposals_list.py`.
    fn embed_summary(reply: &Recorded) -> Value {
        let embed = &reply.embeds.as_ref().expect("c'è un embed")[0];
        json!({
            "title": embed["title"], "description": embed["description"], "color": embed["color"],
            "footer": embed["footer"]["text"],
        })
    }

    /// I componenti nel formato del golden (menu e pulsanti con la loro riga), in ordine canonico.
    fn view_summary(components: Option<&Vec<Value>>) -> Value {
        let mut items = Vec::new();
        for (row, component) in components.into_iter().flatten().enumerate() {
            for child in component["components"].as_array().unwrap() {
                items.push(if child["type"] == 3 {
                    json!({
                        "select": child["options"].as_array().unwrap().iter()
                            .map(|o| json!([o["label"], o["value"], o["description"], o.get("default").cloned().unwrap_or(json!(false))]))
                            .collect::<Vec<_>>(),
                        "placeholder": child["placeholder"], "row": row,
                    })
                } else {
                    json!({"button": child["label"], "style": child["style"], "row": row})
                });
            }
        }
        items.sort_by_key(|item| item.to_string());
        Value::Array(items)
    }

    fn golden_view(view: &Value) -> Value {
        let mut items: Vec<Value> = view.as_array().cloned().unwrap_or_default();
        items.sort_by_key(|item| item.to_string());
        Value::Array(items)
    }

    #[tokio::test(start_paused = true)]
    async fn proposals_list_and_paginator_match_the_python_implementation() {
        let golden: Value = serde_json::from_str(include_str!("golden/proposals_list_expected.json")).unwrap();
        let e = proposals_env();
        let mut first_page_ids = Vec::new();
        for (label, tipo, staff) in [
            ("all", None, true),
            ("gioco", Some("gioco"), true),
            ("film_upper", Some("FILM"), true),
            ("karaoke", Some("Karaoke"), true),
            ("none", Some("Serie"), true),
            ("not_staff", None, false),
        ] {
            let io = FakeIo::new("", 99);
            let io = if staff { io.staff() } else { io };
            e.interactions.command_proposals(&io, tipo).await.unwrap();
            let reply = io.recorded();
            let wanted = &golden[label];
            if wanted["embed"].is_null() {
                assert_eq!(json!(reply.text()), wanted["send"], "{label}");
                continue;
            }
            assert_eq!(embed_summary(&reply), wanted["embed"], "{label}: pagina");
            assert_eq!(view_summary(reply.components.as_ref()), golden_view(&wanted["view"]), "{label}: componenti");
            if label == "all" {
                first_page_ids = reply.component_ids();
            }
        }
        assert_eq!(first_page_ids.len(), 3, "menu, indietro, avanti");
        let (jump, prev, next) = (first_page_ids[0].clone(), first_page_ids[1].clone(), first_page_ids[2].clone());

        let total = golden["pages"].as_u64().unwrap();
        for step in golden["navigation"].as_array().unwrap() {
            let label = step["label"].as_str().unwrap();
            let (id, user, value) = match label {
                "next" | "next2" | "next_wrap" => (&next, 99, None),
                "prev" | "prev_back" | "prev_wrap" => (&prev, 99, None),
                "jump_last" => (&jump, 99, Some(total - 1)),
                "jump_middle" => (&jump, 99, Some(total / 2)),
                "stranger_next" => (&next, 5, None),
                "stranger_jump" => (&jump, 5, Some(0)),
                other => panic!("passo sconosciuto: {other}"),
            };
            let io = FakeIo::new(id, user).staff();
            let io = match value {
                Some(page) => io.value(page),
                None => io,
            };
            e.run(&io).await;
            let call = &step["calls"][0];
            match io.only() {
                Event::Update(reply) => {
                    assert_eq!(embed_summary(&reply), call["embed"], "{label}: pagina");
                    assert_eq!(view_summary(reply.components.as_ref()), golden_view(&call["view"]), "{label}: componenti");
                }
                Event::Respond(reply) => assert_eq!(json!(reply.text()), call["send"], "{label}"),
                other => panic!("{label}: evento inatteso {other:?}"),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_list_is_replaced_by_a_notice() {
        let mut e = proposals_env();
        e.interactions.paginator_ttl = Duration::ZERO;
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_proposals(&io, None).await.unwrap();
        let next = io.recorded().component_ids()[2].clone();
        let click = FakeIo::new(&next, 99).staff();
        e.run(&click).await;
        let Event::Update(reply) = click.only() else { panic!("atteso un aggiornamento") };
        assert!(reply.text().contains("scaduta"));
        assert_eq!(reply.components.as_deref(), Some(&[][..]));
    }

    #[test]
    fn page_custom_ids_round_trip_and_reject_garbage() {
        use crate::interactions::proposals::PageComponent;
        assert_eq!(PageComponent::parse(&PageComponent::id(5, "prev")), Some(PageComponent::Previous { nonce: 5 }));
        assert_eq!(PageComponent::parse(&PageComponent::id(5, "next")), Some(PageComponent::Next { nonce: 5 }));
        assert_eq!(PageComponent::parse(&PageComponent::id(5, "jump")), Some(PageComponent::Jump { nonce: 5 }));
        for garbage in ["plist:", "plist:5", "plist:x:next", "plist:5:other", "plist:-1:next", "vote:5"] {
            assert_eq!(PageComponent::parse(garbage), None, "{garbage}");
        }
    }

    #[test]
    fn jump_menu_shows_a_window_of_25_pages_around_the_current_one() {
        use crate::interactions::proposals::paginator_rows;
        let labels = |total: usize, index: usize| -> (Vec<String>, Vec<bool>) {
            let rows = serde_json::to_value(paginator_rows(1, total, index)).unwrap();
            let options = rows[0]["components"][0]["options"].as_array().unwrap().clone();
            (
                options.iter().map(|o| o["label"].as_str().unwrap().to_string()).collect(),
                options.iter().map(|o| o.get("default").and_then(Value::as_bool).unwrap_or(false)).collect(),
            )
        };
        let (names, defaults) = labels(60, 30);
        assert_eq!((names.first().unwrap().as_str(), names.last().unwrap().as_str(), names.len()), ("Pagina 19", "Pagina 43", 25));
        assert_eq!(defaults.iter().position(|d| *d), Some(12));
        let (names, _) = labels(60, 0);
        assert_eq!((names.first().unwrap().as_str(), names.last().unwrap().as_str()), ("Pagina 1", "Pagina 25"));
        let (names, _) = labels(60, 59);
        assert_eq!((names.first().unwrap().as_str(), names.last().unwrap().as_str()), ("Pagina 36", "Pagina 60"));
        let (names, _) = labels(3, 1);
        assert_eq!(names.len(), 3);
        // Una sola pagina: niente menu, solo le frecce.
        let rows = serde_json::to_value(paginator_rows(1, 1, 0)).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn review_commands_apply_the_review_privately_and_check_permissions() {
        let e = env();
        let id = register_suggestion(&e.db, 7, "Among Us", "Gioco", None, clock()).created_id.unwrap();

        let outsider = FakeIo::new("", 5);
        e.interactions.command_review(&outsider, id, "approve", None).await.unwrap();
        assert_eq!(last_text(&outsider), "⚠️ Non hai i permessi per revisionare le proposte.");

        let io = FakeIo::new("", 99).staff();
        e.interactions.command_review(&io, id, "review", None).await.unwrap();
        assert!(io.deferred());
        assert!(last_text(&io).starts_with("✅") || last_text(&io).contains("Among Us"), "{}", last_text(&io));
        let status: String = e.db.with_conn(|c| c.query_row("SELECT review_status FROM suggestions WHERE id=?", [id], |r| r.get(0)).unwrap());
        assert_eq!(status, "reviewing");

        let io = FakeIo::new("", 99).staff();
        e.interactions.command_review(&io, id, "reject", Some("doppione")).await.unwrap();
        let status: String = e.db.with_conn(|c| c.query_row("SELECT review_status FROM suggestions WHERE id=?", [id], |r| r.get(0)).unwrap());
        assert_eq!(status, "rejected");
        assert_eq!(audit_rows(&e, "proposal_rejected")[0].2.as_deref(), Some("doppione"));

        let io = FakeIo::new("", 99).staff();
        e.interactions.command_review(&io, 9999, "approve", None).await.unwrap();
        assert_eq!(last_text(&io), "❌ Proposta non trovata questa settimana.");
    }

    // --- /sondaggio e /opzione --------------------------------------------------------------

    use crate::interactions::{CreatePollRequest, option_choices, poll_choices};

    /// Le risposte di un'interazione nel formato di `golden_polls_commands.py`.
    fn poll_out(io: &FakeIo) -> Vec<Value> {
        let mut out = Vec::new();
        for event in io.events() {
            match event {
                Event::Defer => out.push(json!(["defer"])),
                Event::Respond(r) => {
                    out.push(json!(["send", r.text()]));
                    let buttons: Vec<Value> = r
                        .components
                        .iter()
                        .flatten()
                        .flat_map(|row| row["components"].as_array().cloned().unwrap_or_default())
                        .map(|b| json!([b["label"], b["style"]]))
                        .collect();
                    if !buttons.is_empty() {
                        out.push(json!(["buttons", buttons]));
                    }
                }
                Event::Update(r) => out.push(json!(["edit", r.text(), r.components.as_deref() == Some(&[][..])])),
                Event::Followup(r) => out.push(json!(["followup", r.text()])),
                other => panic!("evento inatteso: {other:?}"),
            }
        }
        out
    }

    fn poll_state(e: &Env) -> Value {
        json!({
            "polls": rows(e, "SELECT id,name,emoji,color,position,active,is_community FROM polls ORDER BY id", 7),
            "options": rows(e, "SELECT id,poll_id,label,suggestion_id FROM options ORDER BY id", 4),
            "suggestions": rows(e, "SELECT id,name,status,picked_by FROM suggestions ORDER BY id", 4),
            "audit": rows(e, "SELECT actor_id,action,entity_type,entity_id,reason,details FROM staff_audit ORDER BY id", 6),
            "stats": rows(e, "SELECT user_id,ballot_entries FROM user_stats ORDER BY user_id", 2),
            "weekly": rows(e, "SELECT user_id,week,activity_count,points FROM weekly_activity ORDER BY user_id,week", 4),
            "claims": rows(e, "SELECT poll_id FROM poll_message_claims ORDER BY poll_id", 1),
        })
    }

    /// Le stesse regole di Python su colore ed emoji, su tutte le emoji provate dal golden.
    #[test]
    fn emoji_and_color_validation_match_python() {
        use crate::interactions::{valid_color, valid_emoji};
        let golden: Value = serde_json::from_str(include_str!("golden/polls_commands_expected.json")).unwrap();
        let emojis = golden["emojis"].as_array().unwrap();
        for (number, emoji) in emojis.iter().enumerate() {
            let label = format!("emoji_{number}");
            let case = golden["log"].as_array().unwrap().iter().find(|c| c["label"] == label.as_str()).unwrap();
            let accepted = case["out"][0][1].as_str().unwrap().starts_with("✅");
            assert_eq!(valid_emoji(emoji.as_str().unwrap()), accepted, "{label}: {emoji}");
        }
        for (color, valid) in [("red", false), ("#12345", false), ("#GGGGGG", false), ("1234567", false), ("#1234567", false), ("5865F2", true), ("#abcdef", true), ("#ABCDEF", true), ("##123456", false), ("", false)] {
            assert_eq!(valid_color(color), valid, "{color}");
        }
    }

    /// Tutti i 70 scenari di `golden_polls_commands.py`, nello stesso ordine.
    #[tokio::test(start_paused = true)]
    async fn poll_and_option_commands_match_the_python_implementation() {
        use crate::discord_api::ApiError;
        let golden: Value = serde_json::from_str(include_str!("golden/polls_commands_expected.json")).unwrap();
        let e = env();
        e.db.execute("DELETE FROM settings WHERE key IN ('poll_channel','staff_channel')", []).unwrap();
        let mut logged = 0usize;

        for case in golden["log"].as_array().unwrap() {
            let label = case["label"].as_str().unwrap();
            // Preparativi che lo script Python esegue tra uno scenario e l'altro.
            match label {
                "opzione_non_staff" => {
                    for (user, name) in [(1, "Among Us"), (2, "Jackbox Party")] {
                        let id = register_suggestion(&e.db, user, name, "Gioco", None, clock()).created_id.unwrap();
                        e.db.execute("UPDATE suggestions SET review_status='approved' WHERE id=?", [id]).unwrap();
                        let outcome = crate::suggestions::pick_suggestion_in_db(&e.db, id, 99, clock()).unwrap();
                        assert!(outcome.error.is_none(), "{:?}", outcome.error);
                    }
                }
                "opzione_sondaggio_mancante" => {
                    e.db.set_setting("poll_channel", 500).unwrap();
                    e.db.set_setting("staff_channel", 600).unwrap();
                }
                "opzione_massimo_25" => {
                    let poll_id: i64 = e.db.with_conn(|c| c.query_row("SELECT id FROM polls WHERE name='Giochi da Discord'", [], |r| r.get(0)).unwrap());
                    for number in 0..30 {
                        let existing: i64 = e.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM options WHERE poll_id=?", [poll_id], |r| r.get(0)).unwrap());
                        if existing >= 25 {
                            break;
                        }
                        e.db.execute("INSERT INTO options(poll_id,label) VALUES(?,?)", rusqlite::params![poll_id, format!("Extra {number}")]).unwrap();
                    }
                }
                "pubblica_senza_canale" => {
                    e.db.execute("DELETE FROM settings WHERE key='poll_channel'", []).unwrap();
                }
                "pubblica_ok" => e.db.set_setting("poll_channel", 500).unwrap(),
                "elimina_non_admin" => {
                    for (name, message) in [("Emo 0", "7001"), ("Col 5", "7002"), ("Col 6", "7003"), ("Col 7", "7004")] {
                        e.db.execute("UPDATE polls SET message_id=? WHERE name=?", rusqlite::params![message, name]).unwrap();
                    }
                    e.db.execute("INSERT OR IGNORE INTO poll_message_claims(poll_id,expires_at) SELECT id, 1 FROM polls WHERE name='Col 7'", []).unwrap();
                }
                "elimina_messaggio_gia_sparito" => e.api.fail_delete(Some(ApiError::NotFound)),
                "elimina_messaggio_non_eliminabile" => e.api.fail_delete(Some(ApiError::Http("boom".into()))),
                "elimina_con_claim" => e.api.fail_delete(None),
                _ => {}
            }

            let admin = !matches!(label, "crea_non_admin" | "opzione_non_staff" | "rimuovi_opzione_non_staff" | "pubblica_non_admin" | "elimina_non_admin");
            let io = FakeIo::new("", 99);
            let io = if admin { io.staff() } else { io };
            let option_command = label.starts_with("opzione_") || label.starts_with("rimuovi_opzione_");
            let community: String = e.db.with_conn(|c| c.query_row("SELECT name FROM polls WHERE is_community=1", [], |r| r.get(0)).unwrap_or_default());
            let poll = "Giochi da Discord";
            let result = if let Some(number) = label.strip_prefix("colore_") {
                let colors = ["red", "#12345", "#GGGGGG", "1234567", "#1234567", "5865F2", "#abcdef", "#ABCDEF"];
                let mut request = CreatePollRequest::new(format!("Col {number}"));
                request.color = colors[number.parse::<usize>().unwrap()].to_string();
                e.interactions.command_poll_create(&io, request).await
            } else if let Some(number) = label.strip_prefix("emoji_") {
                let mut request = CreatePollRequest::new(format!("Emo {number}"));
                request.emoji = golden["emojis"][number.parse::<usize>().unwrap()].as_str().unwrap().to_string();
                e.interactions.command_poll_create(&io, request).await
            } else {
                match label {
                    "crea_non_admin" => e.interactions.command_poll_create(&io, CreatePollRequest::new("Nuovo")).await,
                    "crea_duplicato" => e.interactions.command_poll_create(&io, CreatePollRequest::new("Emo 0")).await,
                    "crea_community_esistente" => {
                        let mut request = CreatePollRequest::new("Altra community");
                        request.community = true;
                        e.interactions.command_poll_create(&io, request).await
                    }
                    "crea_nome_lungo" => {
                        let mut request = CreatePollRequest::new("N".repeat(100));
                        request.reason = Some("test".into());
                        e.interactions.command_poll_create(&io, request).await
                    }
                    "crea_colore_senza_cancelletto" => {
                        let mut request = CreatePollRequest::new("Senza cancelletto");
                        request.color = "123abc".into();
                        request.emoji = "🎲".into();
                        e.interactions.command_poll_create(&io, request).await
                    }
                    "opzione_non_staff" | "opzione_senza_canali" => e.interactions.command_option_add(&io, poll, "Nuova", None).await,
                    "opzione_sondaggio_mancante" => e.interactions.command_option_add(&io, "Inesistente", "Nuova", None).await,
                    "opzione_ok" => e.interactions.command_option_add(&io, poll, "Scacchi veloci", Some("richiesta".into())).await,
                    "opzione_duplicata_casefold" => e.interactions.command_option_add(&io, poll, "SCACCHI VELOCI", None).await,
                    "opzione_massimo_25" => e.interactions.command_option_add(&io, poll, "Una di troppo", None).await,
                    "rimuovi_opzione_non_staff" => e.interactions.command_option_remove(&io, poll, "Extra 0", None).await,
                    "rimuovi_opzione_sondaggio_mancante" => e.interactions.command_option_remove(&io, "Inesistente", "x", None).await,
                    "rimuovi_opzione_annulla" => e.interactions.command_option_remove(&io, poll, "Extra 0", None).await,
                    "rimuovi_opzione_ok" => e.interactions.command_option_remove(&io, poll, "Extra 0", Some("inutile".into())).await,
                    "rimuovi_opzione_mancante" => e.interactions.command_option_remove(&io, poll, "Non esiste", None).await,
                    "rimuovi_opzione_scelta" => e.interactions.command_option_remove(&io, &community, "Among Us", None).await,
                    "pubblica_non_admin" | "pubblica_senza_canale" | "pubblica_ok" => e.interactions.command_poll_publish(&io).await,
                    "elimina_non_admin" => e.interactions.command_poll_remove(&io, "Emo 0", None).await,
                    "elimina_mancante" => e.interactions.command_poll_remove(&io, "Inesistente", None).await,
                    "elimina_annulla" | "elimina_ok" => {
                        e.interactions.command_poll_remove(&io, "Emo 0", (label == "elimina_ok").then(|| "pulizia".to_string())).await
                    }
                    "elimina_messaggio_gia_sparito" => e.interactions.command_poll_remove(&io, "Col 5", None).await,
                    "elimina_messaggio_non_eliminabile" => e.interactions.command_poll_remove(&io, "Col 6", None).await,
                    "elimina_con_claim" => e.interactions.command_poll_remove(&io, "Col 7", None).await,
                    "elimina_community_con_proposte" => e.interactions.command_poll_remove(&io, &community, None).await,
                    "elimina_sparito_prima_della_conferma" => e.interactions.command_poll_remove(&io, "Senza cancelletto", None).await,
                    other => panic!("scenario sconosciuto: {other}"),
                }
            };
            result.unwrap();
            let _ = option_command;

            // Conferma (o annulla) come negli scenari del golden.
            let mut confirm_out = None;
            if !case["confirm_out"].is_null() || label == "elimina_sparito_prima_della_conferma" {
                let ids = io.recorded().component_ids();
                let pressed = if matches!(label, "rimuovi_opzione_annulla" | "elimina_annulla") { &ids[1] } else { &ids[0] };
                if label == "elimina_sparito_prima_della_conferma" {
                    e.db.execute("DELETE FROM polls WHERE name='Senza cancelletto'", []).unwrap();
                }
                let second = FakeIo::new(pressed, 99).staff();
                e.run(&second).await;
                confirm_out = Some(poll_out(&second));
            }
            tokio::time::sleep(Duration::from_secs(3)).await; // log staff raggruppato e aggiornamenti

            assert_eq!(json!(poll_out(&io)), case["out"], "{label}: risposta");
            if let Some(confirm_out) = confirm_out {
                assert_eq!(json!(confirm_out), case["confirm_out"], "{label}: dopo la conferma");
            }
            let lines: Vec<String> = e
                .sender
                .sent()
                .into_iter()
                .filter_map(|s| match s {
                    crate::notify::testing::Sent::Channel { channel_id: 600, content, .. } => Some(content),
                    _ => None,
                })
                .skip(logged)
                .flat_map(|content| content.lines().map(str::to_string).collect::<Vec<_>>())
                .filter(|line| line.starts_with('➕') || line.starts_with('➖'))
                .collect();
            logged = e.sender.sent().iter().filter(|s| matches!(s, crate::notify::testing::Sent::Channel { channel_id: 600, .. })).count();
            assert_eq!(json!(lines), case["staff_log"], "{label}: log staff");
            let state = poll_state(&e);
            for key in ["polls", "options", "suggestions", "audit", "stats", "weekly", "claims"] {
                let wanted: Value = case["state"][key].clone();
                let wanted = if key == "polls" {
                    // Il golden ha anche il messaggio di ogni sondaggio, che qui lo scrive l'aggiornamento vero.
                    Value::Array(wanted.as_array().unwrap().iter().map(|row| Value::Array(row.as_array().unwrap()[..7].to_vec())).collect())
                } else {
                    wanted
                };
                assert_eq!(state[key], wanted, "{label}: {key}");
            }
        }

        // Autocompletamento sullo stato finale, come in Python.
        e.db.execute("INSERT INTO polls(name,emoji,color,position) VALUES(?,?,?,?)", rusqlite::params!["L".repeat(101), "🎮", "#5865F2", 99]).unwrap();
        for (key, wanted) in golden["autocomplete"].as_object().unwrap() {
            let wanted: Vec<String> = wanted.as_array().unwrap().iter().map(|pair| pair[0].as_str().unwrap().to_string()).collect();
            let actual = if let Some(current) = key.strip_prefix("poll:") {
                poll_choices(&e.db, current).unwrap()
            } else if key == "option:long" {
                e.db.execute(
                    "INSERT INTO options(poll_id,label) VALUES((SELECT id FROM polls WHERE name='Giochi gratis da scaricare'),?)",
                    ["Q".repeat(150)],
                )
                .unwrap();
                option_choices(&e.db, Some("Giochi gratis da scaricare"), "q").unwrap()
            } else {
                let rest = key.strip_prefix("option:").unwrap();
                let (poll, current) = rest.rsplit_once(':').unwrap();
                option_choices(&e.db, (poll != "None").then_some(poll), current).unwrap()
            };
            assert_eq!(actual, wanted, "autocompletamento «{key}»");
        }
    }

    // --- community e stagioni ---------------------------------------------------------------

    use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

    /// Orologio regolabile per `classifica-stagione` (l'unico test che lo usa).
    static SEASON_NOW: AtomicI64 = AtomicI64::new(0);

    fn season_clock() -> DateTime<Tz> {
        TZ.timestamp_opt(SEASON_NOW.load(AtomicOrdering::SeqCst), 0).unwrap()
    }

    fn set_season_now(year: i32, month: u32, day: u32, hour: u32, minute: u32) {
        SEASON_NOW.store(TZ.with_ymd_and_hms(year, month, day, hour, minute, 0).unwrap().timestamp(), AtomicOrdering::SeqCst);
    }

    /// Un embed nel formato di `golden_community.py`.
    fn full_embed_summary(reply: &Recorded) -> Value {
        let embed = &reply.embeds.as_ref().expect("c'è un embed")[0];
        json!({
            "title": embed["title"],
            "description": embed["description"],
            "color": embed["color"],
            "fields": embed.get("fields").and_then(Value::as_array).map(|fields| fields.iter()
                .map(|f| json!([f["name"], f["value"], f.get("inline").cloned().unwrap_or(json!(false))])).collect::<Vec<_>>()).unwrap_or_default(),
            "thumbnail": embed["thumbnail"]["url"],
            "image": embed["image"]["url"],
            "footer": embed["footer"]["text"],
        })
    }

    fn public_reply(io: &FakeIo) -> Recorded {
        match io.only() {
            Event::RespondPublic(reply) => reply,
            other => panic!("attesa una risposta pubblica, trovato {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn guide_matches_the_python_implementation() {
        let golden: Value = serde_json::from_str(include_str!("golden/community_expected.json")).unwrap();
        let e = env();
        for (label, url, settings) in [
            ("base", "", None),
            ("con_sito", "https://overlay.example.com/", None),
            ("con_sito_slash", "https://overlay.example.com//", Some((5, 9, 4, 18))),
        ] {
            if let Some((votes, proposals, weekday, hour)) = settings {
                e.db.set_setting("max_votes", votes).unwrap();
                e.db.set_setting("max_proposals", proposals).unwrap();
                e.db.set_setting("reset_weekday", weekday).unwrap();
                e.db.set_setting("reset_hour", hour).unwrap();
            }
            let io = FakeIo::new("", 7);
            e.interactions.command_guide(&io, url).await.unwrap();
            let reply = public_reply(&io);
            let wanted = &golden["guida"][label];
            assert_eq!(full_embed_summary(&reply), wanted["embed"], "{label}");
            assert_eq!(wanted["ephemeral"], false);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn notification_preferences_match_the_python_implementation() {
        let golden: Value = serde_json::from_str(include_str!("golden/community_expected.json")).unwrap();
        let e = env();
        for case in golden["notifiche"].as_array().unwrap() {
            let label = case["label"].as_str().unwrap();
            let (user, enabled) = match label {
                "stato_iniziale" | "stato_attivo" | "stato_disattivo" => (700, None),
                "attiva" => (700, Some(true)),
                "disattiva" => (700, Some(false)),
                "altro_utente_attiva" => (701, Some(true)),
                other => panic!("scenario sconosciuto: {other}"),
            };
            let io = FakeIo::new("", user);
            e.interactions.command_notifications(&io, enabled).await.unwrap();
            assert!(matches!(io.only(), Event::Respond(_)), "{label}: le notifiche si mostrano in privato");
            assert_eq!(json!(last_text(&io)), case["out"]["content"], "{label}");
            assert_eq!(rows(&e, "SELECT user_id,dm_enabled FROM user_notification_preferences ORDER BY user_id", 2), case["rows"], "{label}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn season_leaderboard_matches_the_python_implementation() {
        let golden: Value = serde_json::from_str(include_str!("golden/community_expected.json")).unwrap();
        set_season_now(2026, 11, 11, 18, 0); // come `initialize` quando è stato generato il golden
        let e = env_at(season_clock);
        e.db.execute_batch(include_str!("golden/cards_fixture.sql")).unwrap();
        e.db.execute_batch(include_str!("golden/admin_fixture.sql")).unwrap();
        e.db.execute_batch(include_str!("golden/community_fixture.sql")).unwrap();

        let members: std::collections::HashMap<i64, &str> = [(1, "Ada"), (2, "Bruno"), (401, "Ugo"), (402, "Vera"), (600, "Zoe"), (603, "Max")].into();
        let without_guild = |id: i64| format!("<@{id}>");
        let with_guild = |id: i64| members.get(&id).map_or_else(|| format!("<@{id}>"), |name| (*name).to_string());
        let scenarios: [(&str, (i32, u32, u32, u32, u32), bool); 8] = [
            ("primavera_senza_guild", (2027, 3, 3, 10, 0), false),
            ("primavera_con_guild", (2027, 3, 3, 10, 0), true),
            ("inverno", (2027, 1, 15, 10, 0), false),
            ("estate", (2027, 7, 1, 10, 0), true),
            ("autunno", (2026, 11, 11, 18, 0), false),
            ("fine_stagione", (2027, 5, 31, 23, 59), false),
            ("pareggio_ampio", (2027, 3, 3, 10, 0), true),
            ("pareggio_ampio_senza_guild", (2027, 3, 3, 10, 0), false),
        ];
        for (label, (y, mo, d, h, mi), guild) in scenarios {
            if label == "pareggio_ampio" {
                e.db.execute_batch(include_str!("golden/community_fixture_b.sql")).unwrap();
            }
            set_season_now(y, mo, d, h, mi);
            let embed = if guild { e.interactions.build_season(&with_guild) } else { e.interactions.build_season(&without_guild) }.unwrap();
            let io = FakeIo::new("", 7);
            e.interactions.command_season(&io, embed).await.unwrap();
            let reply = public_reply(&io);
            assert_eq!(full_embed_summary(&reply), golden["stagione"][label]["embed"], "{label}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn profile_leaderboard_and_records_are_public_and_use_the_card_builders() {
        use crate::cards::{build_community_records_embed, build_contributor_leaderboard_embed, build_public_profile_embed};
        let e = env();
        e.approved(1, "Among Us");
        let member = MemberInfo { id: 1, display_name: "Ada".into(), avatar_url: "https://cdn.example/1.png".into() };
        let io = FakeIo::new("", 1);
        e.interactions.command_profile(&io, Some(member.clone())).await.unwrap();
        let expected = build_public_profile_embed(&e.db, 1, "Ada", "https://cdn.example/1.png", clock()).unwrap().build();
        assert_eq!(public_reply(&io).embeds.unwrap()[0], serde_json::to_value(expected).unwrap());

        // Senza un membro: avviso privato.
        let io = FakeIo::new("", 1);
        e.interactions.command_profile(&io, None).await.unwrap();
        assert!(matches!(io.only(), Event::Respond(r) if r.text() == "⚠️ Non riesco a recuperare il profilo del membro."));

        let io = FakeIo::new("", 1);
        e.interactions.command_contributor_leaderboard(&io).await.unwrap();
        assert_eq!(public_reply(&io).embeds.unwrap()[0], serde_json::to_value(build_contributor_leaderboard_embed(&e.db, 10).unwrap().build()).unwrap());
        let io = FakeIo::new("", 1);
        e.interactions.command_community_records(&io).await.unwrap();
        assert_eq!(public_reply(&io).embeds.unwrap()[0], serde_json::to_value(build_community_records_embed(&e.db).unwrap().build()).unwrap());
    }

    // --- /backup ----------------------------------------------------------------------------

    fn backup_clock() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2027, 3, 8, 3, 15, 0).unwrap() // lunedì: si crea anche la copia settimanale
    }

    fn backup_utc_clock() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2027, 3, 8, 2, 15, 0).unwrap()
    }

    type BackupEnv = Arc<Mutex<HashMap<String, String>>>;

    fn with_backups(e: &Env, root: std::path::PathBuf) -> BackupEnv {
        let vars: BackupEnv = Arc::default();
        let lookup = Arc::clone(&vars);
        let env: Arc<dyn Fn(&str) -> Option<String> + Send + Sync> = Arc::new(move |key| lookup.lock().unwrap().get(key).cloned());
        let service = crate::backups::BackupService::with(e.db.clone(), root, backup_clock, backup_utc_clock, env);
        e.interactions.set_backup_service(Arc::new(service));
        vars
    }

    /// Le risposte nel formato del golden, con la cartella dei backup sostituita da `<ROOT>`.
    fn backup_out(io: &FakeIo, root: &std::path::Path) -> Vec<Value> {
        let plain = |text: &str| text.replace(&root.display().to_string(), "<ROOT>").replace('\\', "/");
        io.events()
            .into_iter()
            .map(|event| match event {
                Event::Defer => json!(["defer", null]),
                Event::Respond(r) => json!(["send", plain(r.text())]),
                Event::Followup(r) => json!(["followup", plain(r.text())]),
                other => panic!("evento inatteso: {other:?}"),
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn backup_commands_match_the_python_implementation() {
        let golden: Value = serde_json::from_str(include_str!("golden/backups_expected.json")).unwrap();
        let e = env();
        let root = tempfile::tempdir().unwrap();
        let vars = with_backups(&e, root.path().to_path_buf());
        let key = String::from_utf8(golden["fernet_key"].as_str().unwrap().as_bytes().to_vec()).unwrap();

        for case in golden["commands"].as_array().unwrap() {
            let label = case["label"].as_str().unwrap();
            let io = FakeIo::new("", 99);
            let io = if label.ends_with("non_admin") { io } else { io.staff() };
            match label {
                "stato_non_admin" | "stato_vuoto" | "stato_dopo" | "stato_con_errore_cloud" => e.interactions.command_backup_status(&io).await.unwrap(),
                "crea_non_admin" | "crea_ok" => e.interactions.command_backup_create(&io).await.unwrap(),
                "crea_con_errore_cloud" => {
                    *vars.lock().unwrap() = [
                        ("BACKUP_S3_BUCKET", "bk"), ("BACKUP_S3_ENDPOINT_URL", "https://127.0.0.1:1"), ("BACKUP_S3_ACCESS_KEY_ID", "AK"),
                        ("BACKUP_S3_SECRET_ACCESS_KEY", "SK"), ("BACKUP_ENCRYPTION_KEY", key.as_str()),
                    ]
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect();
                    e.interactions.command_backup_create(&io).await.unwrap()
                }
                "audit" => {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    let actual = rows(&e, "SELECT actor_id,action,entity_type,entity_id,reason,details FROM staff_audit WHERE action='database_backup_created'", 6);
                    assert_eq!(actual, case["rows"], "audit del backup manuale");
                    continue;
                }
                "crea_fallito" => {
                    // La cartella dei backup è dentro un file: lo snapshot non si può scrivere.
                    let blocker = tempfile::tempdir().unwrap();
                    let file = blocker.path().join("file");
                    std::fs::write(&file, "x").unwrap();
                    let other = env();
                    with_backups(&other, file.join("sotto"));
                    other.interactions.command_backup_create(&io).await.unwrap();
                    assert_eq!(json!(backup_out(&io, root.path())), case["out"], "{label}");
                    continue;
                }
                other => panic!("scenario sconosciuto: {other}"),
            }
            assert_eq!(json!(backup_out(&io, root.path())), case["out"], "{label}");
        }
    }

    // --- /site e /general -------------------------------------------------------------------

    /// Un pulsante o un menu come lo vede l'utente: tipo, stile, testi, indirizzo e opzioni.
    fn simple_component(c: &Value) -> Value {
        let mut out = serde_json::Map::new();
        for key in ["type", "style", "label", "url", "placeholder"] {
            if let Some(value) = c.get(key).filter(|v| !v.is_null()) {
                out.insert(key.to_string(), value.clone());
            }
        }
        if c.get("custom_id").is_some_and(|v| !v.is_null()) {
            out.insert("custom_id".into(), json!("<id>"));
        }
        if let Some(name) = c["emoji"]["name"].as_str() {
            out.insert("emoji".into(), json!(name));
        }
        if let Some(options) = c["options"].as_array() {
            out.insert("options".into(), json!(options.iter().map(|o| json!([o["label"], o["value"], o["emoji"]["name"]])).collect::<Vec<_>>()));
        }
        Value::Object(out)
    }

    /// Una risposta nella forma comune a Python e a Rust: testo, embed (titolo, descrizione, colore) e righe.
    fn simple_view(content: &Value, embed: &Value, rows: &Value) -> Value {
        json!({
            "content": content,
            "embed": if embed.is_null() { Value::Null } else { json!([embed["title"], embed["description"], embed["color"]]) },
            "components": match rows.as_array() {
                None => Value::Null,
                Some(rows) => Value::Array(rows.iter().map(|row| json!(row["components"].as_array().unwrap().iter().map(simple_component).collect::<Vec<_>>())).collect()),
            },
        })
    }

    fn recorded_view(reply: &Recorded) -> Value {
        let content = reply.content.as_ref().map_or(Value::Null, |text| json!(text));
        let embed = reply.embeds.as_ref().and_then(|list| list.first()).cloned().unwrap_or(Value::Null);
        // «Nessun pulsante» (iew=None) è una lista vuota in Rust.
        let rows = reply.components.as_ref().filter(|rows| !rows.is_empty()).map_or(Value::Null, |rows| Value::Array(rows.clone()));
        simple_view(&content, &embed, &rows)
    }

    fn staff_site_golden(entry: &Value) -> Value {
        assert_eq!(entry["ephemeral"], json!(true), "le risposte sono private");
        simple_view(&entry["content"], &entry["embed"], &entry["components"])
    }

    fn with_public_url(e: &Env, url: &str) {
        e.interactions.set_live_state(Arc::new(crate::overlay::LiveState::new(crate::overlay::PublicUrl::parse(url))));
    }

    #[tokio::test(start_paused = true)]
    async fn site_and_general_commands_match_the_python_implementation() {
        let golden: Value = serde_json::from_str(include_str!("golden/staff_tools_expected.json")).unwrap();
        let cases = golden["commands"].as_array().unwrap();
        assert_eq!(cases.len(), 11);
        for case in cases {
            let label = case["label"].as_str().unwrap();
            let e = env();
            let url = if label.contains("porta") { "https://x.it:8443" } else if label.contains("senza_dominio") { "" } else { "https://overlay.example.it" };
            // «senza cog live»: nessun dominio e nessuna sessione, come se il cog non fosse caricato.
            if !label.contains("senza_cog_live") {
                with_public_url(&e, url);
            }
            let mut io = FakeIo::new("", if label.contains("porta") { 12_345_678_901_234_567 } else { 99 });
            io.admin = !label.contains("non_staff") && !label.contains("ruolo");
            if label.contains("ruolo") {
                e.db.set_setting("staff_roles", "10,1").unwrap();
                io.roles = vec![if label.ends_with("everyone") { 1 } else { 10 }];
            }
            if label.starts_with("site") {
                e.interactions.command_site(&io).await.unwrap();
            } else {
                e.interactions.command_general(&io).await.unwrap();
            }
            assert!(matches!(io.only(), Event::Respond(_)), "{label}: risposta privata");
            assert_eq!(recorded_view(&io.recorded()), staff_site_golden(&case["out"][0]), "{label}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn general_panel_buttons_match_the_python_implementation() {
        let golden: Value = serde_json::from_str(include_str!("golden/staff_tools_expected.json")).unwrap();
        let cases = golden["buttons"].as_array().unwrap();
        assert_eq!(cases.len(), 10);
        for case in cases {
            let label = case["label"].as_str().unwrap();
            let e = env();
            if label != "live_assente" {
                with_public_url(&e, "https://overlay.example.it");
            }
            // Gli utenti collegati (bot esclusi) per ogni scenario della sessione.
            let participants = match label {
                "live_attivo" | "live_altro_utente" => Some(3),
                "live_solo_bot" | "live_vuoto" => Some(0),
                _ => None,
            };
            if let Some(count) = participants {
                e.interactions.live.get().unwrap().set_session(Some(crate::overlay::LiveSessionInfo { title: "Serata *Giochi*".into(), guild_id: 1, channel_id: 555, ..Default::default() }));
                e.api.set_voice_participants(1, 555, count);
            }
            let id = if label.starts_with("live") { GeneralComponent::live_id(99) } else { GeneralComponent::center_id(99) };
            let mut io = FakeIo::new(&id, if label.ends_with("altro_utente") { 5 } else { 99 });
            io.admin = !matches!(label, "non_piu_staff" | "centro_staff_ruolo");
            e.run(&io).await;
            assert_eq!(case["allowed"], json!(!label.ends_with("altro_utente") && io.admin), "{label}: il golden dice se la vista lascia passare");
            assert!(matches!(io.only(), Event::Respond(_)), "{label}: risposta privata");
            assert_eq!(recorded_view(&io.recorded()), staff_site_golden(&case["out"][0]), "{label}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn general_component_ids_round_trip_and_reject_garbage() {
        assert_eq!(GeneralComponent::parse(&GeneralComponent::center_id(7)), Some(GeneralComponent::Center { owner: 7 }));
        assert_eq!(GeneralComponent::parse(&GeneralComponent::live_id(7)), Some(GeneralComponent::Live { owner: 7 }));
        for garbage in ["staff:general:", "staff:general:other:1", "staff:general:live:", "staff:general:live:x", "staff:general:live", "staff:tools:live:1"] {
            assert_eq!(GeneralComponent::parse(garbage), None, "{garbage}");
        }
        // Il pulsante «Centro staff» apre lo stesso pannello di /centro-staff.
        let e = env();
        let button = FakeIo::new(&GeneralComponent::center_id(99), 99).staff();
        e.run(&button).await;
        let command = FakeIo::new("", 99).staff();
        e.interactions.command_control_center(&command).await.unwrap();
        assert_eq!(recorded_view(&button.recorded()), recorded_view(&command.recorded()));
    }

    // --- /evento -----------------------------------------------------------------------------

    fn live_golden(entry: &Value) -> Value {
        simple_view(&entry["content"], &entry["embed"], &entry["components"])
    }

    /// L'unica risposta: `send` per un messaggio nuovo, `edit` per una modifica del messaggio.
    fn live_response(io: &FakeIo) -> (&'static str, Value) {
        match io.only() {
            Event::Respond(r) => ("send", recorded_view(&r)),
            Event::Update(r) => ("edit", recorded_view(&r)),
            other => panic!("risposta inattesa: {other:?}"),
        }
    }

    const LIVE_FIXTURE: &str = "
INSERT INTO live_events(id,guild_id,title,channel_id,channel_name,started_at,last_seen_at,ended_at,peak_participants,participant_count,end_reason) VALUES
 (1,1,'Game Night 1',555,'vocale','2027-01-01T21:00:00+01:00','2027-01-01T23:00:00+01:00','2027-01-01T23:00:00+01:00',8,2,'manuale'),
 (2,1,'Game Night 2',555,'vocale','2027-01-08T21:00:00+01:00','2027-01-08T22:30:00+01:00','2027-01-08T22:30:00+01:00',9,1,'disconnessione'),
 (3,1,'In corso',555,'vocale','2027-03-01T21:00:00+01:00','2027-03-01T21:00:00+01:00',NULL,2,1,'manual');
INSERT INTO live_event_participants(event_id,user_id,joined_at,participation_seconds,speaking_seconds) VALUES
 (1,101,NULL,7200,600),(1,102,NULL,3600,300),(2,101,NULL,5400,100.5),(3,103,'2027-03-01T21:00:00+01:00',0,0);
";

    fn live_env() -> Env {
        let e = env();
        e.db.execute_batch(LIVE_FIXTURE).unwrap();
        e.db.execute("DELETE FROM settings WHERE key='guild_id'", []).unwrap();
        e.interactions.set_live_state(Arc::new(
            crate::overlay::LiveState::new(crate::overlay::PublicUrl::parse("https://overlay.example.it")).with_token_generator(|| "nuovo-token".to_string()),
        ));
        e
    }

    fn live_session(e: &Env, guild: u64) {
        e.interactions.live.get().unwrap().set_session(Some(crate::overlay::LiveSessionInfo { title: "Serata".into(), guild_id: guild, channel_id: 555, ..Default::default() }));
        e.api.set_voice_counts(guild, 555, 3, 2);
    }

    fn io_for(label: &str) -> FakeIo {
        let io = FakeIo::new("", 99);
        let io = if label.contains("non_admin") || label.contains("non_staff") { io } else { io.staff() };
        if label.contains("fuori_dal_server") { io.no_guild() } else { io }
    }

    #[tokio::test(start_paused = true)]
    async fn live_event_reports_and_deletion_match_the_python_implementation() {
        let golden: Value = serde_json::from_str(include_str!("golden/live_commands_expected.json")).unwrap();
        let cases = golden["cases"].as_array().unwrap();
        let e = live_env();
        let mut checked = 0;
        let mut check = |label: &str, io: &FakeIo| {
            let want = cases.iter().find(|c| c["label"] == label).unwrap_or_else(|| panic!("golden senza {label}"));
            let (kind, view) = live_response(io);
            assert_eq!(kind, want["out"][0]["kind"], "{label}: tipo di risposta");
            assert_eq!(view, live_golden(&want["out"][0]), "{label}");
            checked += 1;
        };

        // /evento rapporto
        for (label, id) in [("rapporto_non_staff", None), ("rapporto_fuori_dal_server", None), ("rapporto_elenco", None), ("rapporto_dettaglio", Some(1)), ("rapporto_attivo", Some(3)), ("rapporto_inesistente", Some(99))] {
            let io = io_for(label);
            e.interactions.command_event_report(&io, id).await.unwrap();
            check(label, &io);
        }
        for (label, setting, id) in [("rapporto_altro_server", "999", None), ("rapporto_server_non_valido", "abc", None), ("rapporto_server_giusto", "1", Some(2))] {
            e.db.set_setting("guild_id", setting).unwrap();
            let io = io_for(label);
            e.interactions.command_event_report(&io, id).await.unwrap();
            check(label, &io);
        }

        // /evento elimina-rapporto
        for (label, id) in [("elimina_non_admin", 1), ("elimina_fuori_dal_server", 1), ("elimina_inesistente", 99), ("elimina_attivo", 3)] {
            let io = io_for(label);
            e.interactions.command_event_delete_report(&io, id, None).await.unwrap();
            check(label, &io);
        }
        let ask = io_for("elimina_richiesta");
        e.interactions.command_event_delete_report(&ask, 1, Some("doppione".into())).await.unwrap();
        check("elimina_richiesta", &ask);

        // Le richieste di conferma si aprono prima dei clic (come le viste di Python).
        let request = |event: i64, reason: Option<&str>| {
            let ask = FakeIo::new("", 99).staff();
            futures_block(e.interactions.command_event_delete_report(&ask, event, reason.map(str::to_string))).unwrap();
            let id = ask.recorded().component_ids();
            (id[0].clone(), id[1].clone())
        };
        let requests = [
            ("conferma_altro_utente", request(1, Some("doppione")), true, FakeIo::new("", 5).staff()),
            ("conferma_non_admin", request(1, Some("doppione")), true, FakeIo::new("", 99)),
            ("conferma_fuori_dal_server", request(1, Some("doppione")), true, FakeIo::new("", 99).staff().no_guild()),
            ("annulla_altro_utente", request(1, None), false, FakeIo::new("", 5).staff()),
            ("annulla", request(1, None), false, FakeIo::new("", 99).staff()),
            ("conferma_ok", request(1, Some("doppione")), true, FakeIo::new("", 99).staff()),
            ("conferma_gia_eliminato", request(1, Some("doppione")), true, FakeIo::new("", 99).staff()),
            ("conferma_senza_motivo", request(2, None), true, FakeIo::new("", 99).staff()),
        ];
        for (label, (yes, no), confirm, mut io) in requests {
            io.custom_id = if confirm { yes } else { no };
            e.run(&io).await;
            check(label, &io);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(table_of(&e, "SELECT id FROM live_events ORDER BY id"), golden["delete_state"]["events"]);
        assert_eq!(table_of(&e, "SELECT event_id,user_id FROM live_event_participants ORDER BY rowid"), golden["delete_state"]["participants"]);
        assert_eq!(
            table_of(&e, "SELECT actor_id,action,entity_type,entity_id,reason,details FROM staff_audit WHERE action='live_event_report_deleted' ORDER BY id"),
            golden["delete_state"]["audit"]
        );
        assert_eq!(checked, 9 + 4 + 1 + 8);
    }

    fn table_of(e: &Env, sql: &str) -> Value {
        e.db.with_conn(|c| {
            let mut statement = c.prepare(sql).unwrap();
            let columns = statement.column_count();
            let rows = statement
                .query_map([], |row| {
                    Ok((0..columns)
                        .map(|i| match row.get_ref(i).unwrap() {
                            rusqlite::types::ValueRef::Null => Value::Null,
                            rusqlite::types::ValueRef::Integer(n) => json!(n),
                            rusqlite::types::ValueRef::Real(f) => json!(f),
                            rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                            rusqlite::types::ValueRef::Blob(_) => Value::Null,
                        })
                        .collect::<Vec<_>>())
                })
                .unwrap();
            Value::Array(rows.map(|row| Value::Array(row.unwrap())).collect())
        })
    }

    /// Esegue un futuro già pronto a completarsi senza attese (le risposte del finto non attendono).
    fn futures_block<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        match future.as_mut().poll(&mut std::task::Context::from_waker(waker)) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("il futuro doveva completarsi subito"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn live_event_status_links_and_scheduling_match_the_python_implementation() {
        use crate::discord_api::{ChannelKind, EventStatus, ScheduledEventInfo};
        let golden: Value = serde_json::from_str(include_str!("golden/live_commands_expected.json")).unwrap();
        let cases = golden["cases"].as_array().unwrap();
        let e = live_env();
        e.db.set_setting("guild_id", "1").unwrap();
        e.api.set_channel_kind(555, ChannelKind::Voice);
        e.api.set_channel_kind(556, ChannelKind::Stage);
        e.api.set_channel_kind(600, ChannelKind::Text);
        e.api.set_channel_kind(601, ChannelKind::Voice);
        let event = |id: u64, name: &str, status, channel: Option<u64>| ScheduledEventInfo { id, name: name.to_string(), status, channel_id: channel };
        let events = vec![
            event(1001, "Serata Among Us", EventStatus::Scheduled, Some(555)),
            event(1002, "Già iniziata", EventStatus::Active, Some(555)),
            event(1003, "Evento in stage", EventStatus::Scheduled, Some(556)),
            event(1004, "Evento esterno", EventStatus::Scheduled, None),
            event(1005, &format!("Torneo {}", "x".repeat(120)), EventStatus::Scheduled, Some(555)),
            event(1006, "Altra serata", EventStatus::Scheduled, Some(555)),
            event(1007, "Finita", EventStatus::Completed, Some(555)),
        ];
        e.api.set_scheduled_events(1, events);
        let mut checked = 0;
        let mut check = |label: &str, io: &FakeIo| {
            let want = cases.iter().find(|c| c["label"] == label).unwrap_or_else(|| panic!("golden senza {label}"));
            let (kind, view) = live_response(io);
            assert_eq!(kind, want["out"][0]["kind"], "{label}: tipo di risposta");
            assert_eq!(view, live_golden(&want["out"][0]), "{label}");
            checked += 1;
        };
        let live = e.interactions.live.get().unwrap();

        // stato
        for label in ["stato_non_staff", "stato_senza_sessione"] {
            let io = io_for(label);
            e.interactions.command_event_status(&io).await.unwrap();
            check(label, &io);
        }
        live_session(&e, 1);
        let io = io_for("stato_attivo");
        e.interactions.command_event_status(&io).await.unwrap();
        check("stato_attivo", &io);

        // rigenera-link (un overlay collegato si chiude solo quando il link viene davvero revocato)
        let (overlay, mut overlay_inbox) = tokio::sync::mpsc::channel(4);
        let _overlay = live.hub.register(overlay);
        let io = io_for("rigenera_non_admin");
        e.interactions.command_event_rotate_link(&io).await.unwrap();
        check("rigenera_non_admin", &io);
        live.set_session(None);
        let io = io_for("rigenera_senza_sessione");
        e.interactions.command_event_rotate_link(&io).await.unwrap();
        check("rigenera_senza_sessione", &io);
        live_session(&e, 2);
        let io = io_for("rigenera_altro_server");
        e.interactions.command_event_rotate_link(&io).await.unwrap();
        check("rigenera_altro_server", &io);
        assert!(overlay_inbox.try_recv().is_err(), "nessuna revoca: l'overlay resta collegato");
        assert_eq!(live.hub.client_count(), 1);
        live_session(&e, 1);
        e.db.set_setting("live_event_token", "vecchio-token").unwrap();
        let io = io_for("rigenera_ok");
        e.interactions.command_event_rotate_link(&io).await.unwrap();
        check("rigenera_ok", &io);
        assert_eq!(e.db.get_setting("live_event_token").unwrap(), golden["token_after_rotate"].as_str().map(str::to_string));
        assert_eq!(overlay_inbox.try_recv().unwrap(), crate::overlay_web::Outgoing::Close(1008, "Link overlay revocato"));
        assert_eq!(live.hub.client_count(), 0);

        // link
        for (value, _) in crate::live_event::OVERLAY_THEMES {
            let label = format!("link_{value}");
            let io = io_for(&label);
            e.interactions.command_event_theme_link(&io, value).await.unwrap();
            check(&label, &io);
        }
        let io = io_for("link_non_staff");
        e.interactions.command_event_theme_link(&io, "classic").await.unwrap();
        check("link_non_staff", &io);
        live.set_session(None);
        let io = io_for("link_senza_sessione");
        e.interactions.command_event_theme_link(&io, "classic").await.unwrap();
        check("link_senza_sessione", &io);

        // programma
        e.db.set_setting("scheduled_event_optins", "[]").unwrap();
        e.db.set_setting("staff_channel", "").unwrap();
        for (label, staff_channel, id) in [
            ("programma_non_admin", None, "1001"),
            ("programma_non_numerico", None, "abc"),
            ("programma_inesistente", None, "5555"),
            ("programma_gia_iniziato", None, "1002"),
            ("programma_stage", None, "1003"),
            ("programma_esterno", None, "1004"),
            ("programma_senza_canale_staff", None, "1001"),
            ("programma_canale_staff_vocale", Some("601"), "1001"),
            ("programma_canale_staff_non_valido", Some("abc"), "1001"),
            ("programma_ok", Some("600"), "1001"),
            ("programma_ok_secondo", None, "1006"),
            ("programma_ripetuto", None, "1001"),
        ] {
            if let Some(channel) = staff_channel {
                e.db.set_setting("staff_channel", channel).unwrap();
            }
            let io = io_for(label);
            e.interactions.command_event_schedule(&io, id).await.unwrap();
            check(label, &io);
        }
        assert_eq!(e.db.get_setting("scheduled_event_optins").unwrap(), golden["optins_after_opt_in"].as_str().map(str::to_string));

        // rimuovi-programma
        for (label, id) in [
            ("rimuovi_non_admin", "1001"),
            ("rimuovi_non_numerico", "abc"),
            ("rimuovi_vuoto", "   "),
            ("rimuovi_non_abilitato", "42"),
            ("rimuovi_ok", " 1006 "),
        ] {
            let io = io_for(label);
            e.interactions.command_event_unschedule(&io, id).await.unwrap();
            check(label, &io);
        }
        assert_eq!(e.db.get_setting("scheduled_event_optins").unwrap(), golden["optins_after_opt_out"].as_str().map(str::to_string));
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            table_of(&e, "SELECT actor_id,action,entity_type,entity_id,reason,details FROM staff_audit WHERE action LIKE 'scheduled_event_%' ORDER BY id"),
            golden["scheduled_audit"]
        );
        assert_eq!(checked, 3 + 4 + 7 + 12 + 5);

        // autocompletamenti
        let as_json = |choices: Vec<(String, String)>| json!(choices.into_iter().map(|(name, value)| json!([name, value])).collect::<Vec<_>>());
        let mut seen = 0;
        for (index, case) in golden["autocomplete"].as_array().unwrap().iter().enumerate() {
            let (kind, current) = (case[0].as_str().unwrap(), case[1].as_str().unwrap());
            if index == 8 {
                crate::live_event::save_scheduled_event_optins(&e.db, &std::collections::BTreeSet::from([1001, 1005, 1007, 777, 31])).unwrap();
            }
            let actual = match kind {
                "programma" => e.interactions.scheduled_event_choices(Some(1), current).await,
                "programma_senza_server" => e.interactions.scheduled_event_choices(None, current).await,
                "rimuovi" => e.interactions.opted_in_event_choices(Some(1), current).await,
                "rimuovi_senza_server" => e.interactions.opted_in_event_choices(None, current).await,
                other => panic!("tipo sconosciuto {other}"),
            };
            assert_eq!(as_json(actual), case[2], "{kind} {current:?}");
            seen += 1;
        }
        assert_eq!(seen, 15);
    }

    #[tokio::test(start_paused = true)]
    async fn live_event_requests_expire_and_voice_commands_are_unavailable() {
        let mut e = live_env();
        let ask = FakeIo::new("", 99).staff();
        e.interactions.command_event_delete_report(&ask, 1, None).await.unwrap();
        let ids = ask.recorded().component_ids();
        assert_eq!(LiveReportComponent::parse(&ids[0]).map(|c| matches!(c, LiveReportComponent::Delete { .. })), Some(true));
        assert_eq!(LiveReportComponent::parse(&ids[1]).map(|c| matches!(c, LiveReportComponent::Cancel { .. })), Some(true));
        for garbage in ["evt:del:", "evt:del:x:yes", "evt:del:5:maybe", "evt:del:5", "cmd:confirm:5:yes"] {
            assert_eq!(LiveReportComponent::parse(garbage), None, "{garbage}");
        }
        // Scaduti i 60 secondi la richiesta non c'è più (le conferme misurano il tempo con `Instant`).
        e.interactions.confirm_ttl = Duration::ZERO;
        let late = FakeIo::new(&ids[0], 99).staff();
        e.run(&late).await;
        let (kind, view) = live_response(&late);
        assert_eq!(kind, "edit");
        assert_eq!(view["content"], "⌛ Questa richiesta è scaduta: ripeti il comando.");
        assert_eq!(e.count("SELECT COUNT(*) FROM live_events WHERE id=1"), 1, "non si cancella nulla");

        // Senza il servizio dell'evento live i tre comandi dicono che la voce non c'è.
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_event_start(&io, 555).await.unwrap();
        assert!(io.deferred() && io.recorded().text().contains("non sono ancora disponibili"));
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_event_connect(&io).await.unwrap();
        assert!(io.recorded().text().contains("non sono ancora disponibili"));
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_event_stop(&io).await.unwrap();
        assert!(io.deferred() && io.recorded().text().contains("non sono ancora disponibili"));
        let outsider = FakeIo::new("", 99);
        e.interactions.command_event_start(&outsider, 555).await.unwrap();
        assert_eq!(outsider.recorded().text(), "⚠️ Questo comando è riservato allo staff.");
        assert!(!outsider.deferred());
    }

    #[tokio::test(start_paused = true)]
    async fn live_event_start_connect_and_stop_match_the_python_implementation() {
        use crate::discord_api::ChannelKind;
        use crate::live_service::LiveService;
        use crate::overlay::{LiveState, PublicUrl};
        use crate::voice::fake::FakeGateway;
        let golden: Value = serde_json::from_str(include_str!("golden/live_service_expected.json")).unwrap();
        let cases = golden["commands"].as_array().unwrap();
        let case = |label: &str| cases.iter().find(|c| c["label"] == label).unwrap_or_else(|| panic!("golden senza {label}")).clone();
        let e = env();
        e.db.set_setting("guild_id", "1").unwrap();
        e.api.set_channel_kind(555, ChannelKind::Voice);
        e.api.set_channel_kind(556, ChannelKind::Stage);
        let live = Arc::new(LiveState::new(PublicUrl::parse("https://overlay.example.it")).with_token_generator(|| "token-fisso".to_string()));
        e.interactions.set_live_state(live.clone());
        let gateway = FakeGateway::default();
        gateway.add_channel(1, 555, "Sala Eventi", true);
        gateway.set_members(&[10, 11]);
        let service = Arc::new(LiveService::new(e.db.clone(), e.interactions.services.notifier.clone(), e.interactions.services.publisher.clone(), gateway.clone(), live, true));
        e.interactions.set_live_controller(service.clone());
        let calls_of = |label: &str| -> Vec<String> { case(label)["calls"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect() };

        // Chi non è staff non può avviare né fermare, e non si «sta pensando».
        for (label, which) in [("avvia_non_staff", 0), ("termina_non_staff", 1), ("collega_non_staff", 2)] {
            let io = FakeIo::new("", 99);
            match which {
                0 => e.interactions.command_event_start(&io, 555).await.unwrap(),
                1 => e.interactions.command_event_stop(&io).await.unwrap(),
                _ => e.interactions.command_event_connect(&io).await.unwrap(),
            }
            assert!(!io.deferred(), "{label}");
            assert_eq!(last_text(&io), case(label)["out"][0][1].as_str().unwrap(), "{label}");
        }

        // avvia
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_event_start(&io, 555).await.unwrap();
        assert!(io.deferred());
        assert_eq!(last_text(&io), case("avvia_ok")["out"][1][1].as_str().unwrap());
        assert_eq!(gateway.calls(), calls_of("avvia_ok"));
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_event_start(&io, 555).await.unwrap();
        assert_eq!(last_text(&io), case("avvia_gia_attivo")["out"][1][1].as_str().unwrap());

        // termina
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_event_stop(&io).await.unwrap();
        assert!(io.deferred());
        assert_eq!(last_text(&io), case("termina_ok")["out"][1][1].as_str().unwrap());
        let actions: Vec<String> = table_of(&e, "SELECT action FROM staff_audit ORDER BY id").as_array().unwrap().iter().map(|row| row[0].as_str().unwrap().to_string()).collect();
        let expected: Vec<String> = case("termina_ok")["db"]["audit"].as_array().unwrap().iter().map(|row| row[1].as_str().unwrap().to_string()).collect();
        assert_eq!(actions, expected);
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_event_stop(&io).await.unwrap();
        assert_eq!(last_text(&io), case("termina_senza_evento")["out"][1][1].as_str().unwrap());

        // collega
        for (label, channel) in [("collega_senza_voce", None), ("collega_non_in_canale", None), ("collega_in_stage", Some(556))] {
            if let Some(channel) = channel {
                gateway.set_user_channel(1, 99, channel);
            }
            let io = FakeIo::new("", 99).staff();
            e.interactions.command_event_connect(&io).await.unwrap();
            assert!(!io.deferred(), "{label}");
            assert_eq!(last_text(&io), case(label)["out"][0][1].as_str().unwrap(), "{label}");
        }
        gateway.set_user_channel(1, 99, 555);
        gateway.clear_calls();
        let io = FakeIo::new("", 99).staff();
        e.interactions.command_event_connect(&io).await.unwrap();
        assert!(io.deferred());
        assert_eq!(last_text(&io), case("collega_ok")["out"][1][1].as_str().unwrap());
        assert_eq!(gateway.calls(), calls_of("collega_ok"));
        assert!(service.is_active());
    }
}
