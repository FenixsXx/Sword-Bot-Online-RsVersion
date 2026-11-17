//! I comandi `/evento …` di `cogs/live_event.py`: avvio e fine dell'evento, report, link overlay,
//! stato e avvio automatico degli eventi programmati.
//!
//! `/evento avvia`, `collega` e `termina` chiedono il lavoro al [`LiveController`]; finché la voce
//! non sa collegarsi (o se il controller non è collegato) rispondono che non è disponibile.
//!
//! La conferma di `/evento elimina-rapporto` (la vista `DeleteEventReportView` di Python) ricorda
//! evento e motivo in memoria per 60 secondi, come le altre conferme; i pulsanti hanno
//! `custom_id` `evt:del:<richiesta>:yes|no`.

use poise::serenity_prelude::{ButtonStyle, CreateActionRow, CreateButton, ReactionType};
use serde_json::json;
use tracing::{error, warn};

use super::commands::ConfirmAction;
use super::{InteractionIo, Interactions, Io, IoError, Reply};
use crate::discord_api::{ChannelKind, DiscordApi, EventStatus};
use crate::formatting::{casefold, truncate_chars};
use crate::live_event::{self, LinkError};
use crate::live_service::{LiveController, VOICE_UNAVAILABLE};
use crate::notify::{AuditEntry, DiscordSender};

const STAFF_ONLY: &str = "⚠️ Questo comando è riservato allo staff.";
const NO_SESSION: &str = "ℹ️ Non c'è un evento live attivo.";
const START_FIRST: &str = "ℹ️ Avvia prima un evento live.";
const REPORT_NOT_FOUND: &str = "⚠️ Report non trovato o evento ancora attivo.";
const DELETE_ADMIN_ONLY: &str = "⚠️ Solo un amministratore può eliminare report evento.";
const CONFIRM_OWNER_ONLY: &str = "⚠️ Questa conferma è riservata a chi l'ha richiesta.";
const EXPIRED: &str = "⌛ Questa richiesta è scaduta: ripeti il comando.";
const NOT_IN_VOICE: &str = "⚠️ Entra prima in un canale vocale standard. I canali Stage non sono supportati da questa modalità.";
const LINK_WARNING: &str = "Il link contiene una chiave di accesso: condividilo solo con chi deve vedere l'overlay.";

/// `evt:del:<richiesta>:yes|no`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveReportComponent {
    Delete { nonce: u64 },
    Cancel { nonce: u64 },
}

impl LiveReportComponent {
    pub fn parse(custom_id: &str) -> Option<Self> {
        let rest = custom_id.strip_prefix("evt:del:")?;
        let (nonce, choice) = rest.split_once(':')?;
        if nonce.is_empty() || !nonce.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let nonce = nonce.parse().ok()?;
        match choice {
            "yes" => Some(Self::Delete { nonce }),
            "no" => Some(Self::Cancel { nonce }),
            _ => None,
        }
    }

    pub fn id(nonce: u64, delete: bool) -> String {
        format!("evt:del:{nonce}:{}", if delete { "yes" } else { "no" })
    }
}

fn delete_rows(nonce: u64) -> Vec<CreateActionRow> {
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new(LiveReportComponent::id(nonce, true))
            .label("Elimina report")
            .emoji(ReactionType::Unicode("🗑️".into()))
            .style(ButtonStyle::Danger),
        CreateButton::new(LiveReportComponent::id(nonce, false))
            .label("Annulla")
            .emoji(ReactionType::Unicode("✖️".into()))
            .style(ButtonStyle::Secondary),
    ])]
}

fn db_error(e: rusqlite::Error) -> IoError {
    IoError(e.to_string())
}

/// `testo.strip().isdigit()` seguito da `int()`: solo cifre decimali (anche non latine).
fn parse_event_id(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() || !text.chars().all(|c| live_event::decimal_digit(c).is_some()) {
        return None;
    }
    live_event::python_int(text)
}

impl<S: DiscordSender, A: DiscordApi> Interactions<S, A> {
    /// `authorize_staff`: nel server, staff, e — se è stato registrato un server — proprio quello.
    fn authorize_live_staff<I: InteractionIo>(&self, io: &I) -> bool {
        let Some(guild) = io.guild_id() else {
            return false;
        };
        if !self.is_staff(io) {
            return false;
        }
        let allowed = self.db().get_setting("guild_id").ok().flatten().filter(|text| !text.is_empty());
        match allowed {
            None => true,
            Some(text) => match live_event::python_int(&text) {
                Some(id) => i64::try_from(guild).is_ok_and(|guild| guild == id),
                None => {
                    error!("ID server configurato per l'overlay non valido.");
                    false
                }
            },
        }
    }

    fn is_server_admin<I: InteractionIo>(io: &I) -> bool {
        io.guild_id().is_some() && io.is_admin()
    }

    /// Il dominio pubblico dell'overlay (`self.public_url`), vuoto se non c'è.
    fn overlay_public_url(&self) -> String {
        self.live.get().map(|live| live.public_url.public_url.clone()).unwrap_or_default()
    }

    /// La funzione che genera le chiavi dell'overlay (quella vera, o una fissa nei test).
    fn token_generator(&self) -> fn() -> String {
        self.live.get().map_or(live_event::new_token as fn() -> String, |live| live.token_generator)
    }

    fn link_for(&self, guild_id: u64, theme: &str) -> Result<String, IoError> {
        let token = live_event::overlay_token_with(self.db(), self.token_generator()).map_err(db_error)?;
        live_event::overlay_link(&self.overlay_public_url(), guild_id, theme, &token).map_err(|e: LinkError| IoError(e.to_string()))
    }

    /// La sessione live in corso nel server dell'interazione, se c'è.
    fn session_here<I: InteractionIo>(&self, io: &I) -> Option<crate::overlay::LiveSessionInfo> {
        let session = self.live.get()?.session()?;
        (Some(session.guild_id) == io.guild_id()).then_some(session)
    }

    // --- report ---

    /// `/evento rapporto [id_rapporto]`.
    pub async fn command_event_report<I: InteractionIo>(&self, io: &I, report_id: Option<i64>) -> Io<()> {
        if !self.authorize_live_staff(io) {
            return io.respond(Reply::text(STAFF_ONLY)).await;
        }
        let Some(report_id) = report_id else {
            let list = live_event::recent_reports_embed(self.db()).map_err(db_error)?;
            return io.respond(Reply::embed(list.build())).await;
        };
        match live_event::build_event_report(self.db(), report_id).map_err(db_error)? {
            Some(report) => io.respond(Reply::embed(report.build())).await,
            None => io.respond(Reply::text(REPORT_NOT_FOUND)).await,
        }
    }

    /// `/evento elimina-rapporto`: chiede conferma prima di cancellare.
    pub async fn command_event_delete_report<I: InteractionIo>(&self, io: &I, report_id: i64, reason: Option<String>) -> Io<()> {
        if !Self::is_server_admin(io) {
            return io.respond(Reply::text(DELETE_ADMIN_ONLY)).await;
        }
        if !live_event::report_exists(self.db(), report_id).map_err(db_error)? {
            return io.respond(Reply::text(REPORT_NOT_FOUND)).await;
        }
        let nonce = self.remember_confirmation(io.user_id(), ConfirmAction::DeleteEventReport { event_id: report_id, reason });
        io.respond(
            Reply::text(format!(
                "Eliminare il report `#{report_id}` e i dati individuali di presenza e parlato? L'operazione non è reversibile."
            ))
            .with_components(delete_rows(nonce)),
        )
        .await
    }

    pub(super) async fn live_report_component<I: InteractionIo>(&self, io: &I, component: LiveReportComponent) -> Io<()> {
        let (LiveReportComponent::Delete { nonce } | LiveReportComponent::Cancel { nonce }) = component;
        // Cosa c'è dietro la richiesta: non esiste/scaduta, è di un altro, oppure è sua.
        enum Lookup {
            Missing,
            Foreign,
            Mine { event_id: i64, reason: Option<String> },
        }
        let lookup = {
            let mut map = self.confirmations.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match map.get(&nonce) {
                Some(entry) if entry.created.elapsed() >= self.confirm_ttl => {
                    map.remove(&nonce);
                    Lookup::Missing
                }
                Some(entry) => match &entry.action {
                    ConfirmAction::DeleteEventReport { event_id, reason } => {
                        if entry.owner == io.user_id() {
                            Lookup::Mine { event_id: *event_id, reason: reason.clone() }
                        } else {
                            Lookup::Foreign
                        }
                    }
                    _ => Lookup::Missing,
                },
                None => Lookup::Missing,
            }
        };
        let (event_id, reason) = match lookup {
            Lookup::Foreign => return io.respond(Reply::text(CONFIRM_OWNER_ONLY)).await,
            Lookup::Missing => return io.update(Reply::text(EXPIRED).without_components()).await,
            Lookup::Mine { event_id, reason } => (event_id, reason),
        };
        if matches!(component, LiveReportComponent::Cancel { .. }) {
            self.confirmations.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&nonce);
            return io.update(Reply::text("Eliminazione annullata.").without_components()).await;
        }
        // Chi non è più amministratore può riprovare: la richiesta resta aperta.
        if !Self::is_server_admin(io) {
            return io.respond(Reply::text(DELETE_ADMIN_ONLY)).await;
        }
        self.confirmations.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&nonce);
        let deleted = match live_event::delete_report(self.db(), event_id) {
            Ok(deleted) => deleted,
            Err(e) => {
                error!("Impossibile eliminare il report evento {event_id}: {e}");
                return io.update(Reply::text("❌ Eliminazione non riuscita; controlla i log del bot.").without_components()).await;
            }
        };
        let message = if deleted > 0 {
            self.services.notifier.record_staff_audit(
                AuditEntry::new(u64::try_from(io.user_id()).ok(), "live_event_report_deleted").entity("live_event", event_id).reason(reason.as_deref()),
            );
            format!("✅ Report `#{event_id}` e dati individuali eliminati.")
        } else {
            "⚠️ Report non trovato o già eliminato.".to_string()
        };
        io.update(Reply::text(message).without_components()).await
    }

    // --- stato e link ---

    /// `/evento stato`.
    pub async fn command_event_status<I: InteractionIo>(&self, io: &I) -> Io<()> {
        if !self.authorize_live_staff(io) {
            return io.respond(Reply::text(STAFF_ONLY)).await;
        }
        let Some(session) = self.live.get().and_then(|live| live.session()) else {
            return io.respond(Reply::text(NO_SESSION)).await;
        };
        let counts = self.services.publisher.api().voice_member_counts(session.guild_id, session.channel_id);
        io.respond(Reply::text(format!(
            "🎮 Evento live attivo in <#{}> · `{}` partecipanti vocali.",
            session.channel_id, counts.total
        )))
        .await
    }

    /// `/evento rigenera-link`: cambia la chiave e restituisce il nuovo link.
    pub async fn command_event_rotate_link<I: InteractionIo>(&self, io: &I) -> Io<()> {
        if !Self::is_server_admin(io) {
            return io.respond(Reply::text("⚠️ Solo un amministratore può revocare il link overlay.")).await;
        }
        let Some(session) = self.session_here(io) else {
            return io.respond(Reply::text(START_FIRST)).await;
        };
        self.db().set_setting("live_event_token", (self.token_generator())()).map_err(db_error)?;
        let link = self.link_for(session.guild_id, "classic")?;
        // Prima la risposta (Discord ne concede 3 secondi), poi si chiudono gli overlay con la vecchia
        // chiave. Si chiudono comunque, anche se la risposta non parte.
        let response = io.respond(Reply::text(format!("✅ Link precedente revocato. Nuovo link OBS:\n{link}"))).await;
        if let Some(live) = self.live.get() {
            live.hub.close_all(1008, "Link overlay revocato");
        }
        response
    }

    /// `/evento link tema:<layout>`.
    pub async fn command_event_theme_link<I: InteractionIo>(&self, io: &I, theme: &str) -> Io<()> {
        if !self.authorize_live_staff(io) {
            return io.respond(Reply::text(STAFF_ONLY)).await;
        }
        let Some(session) = self.session_here(io) else {
            return io.respond(Reply::text(START_FIRST)).await;
        };
        let link = self.link_for(session.guild_id, theme)?;
        let name = live_event::OVERLAY_THEMES.iter().find(|(value, _)| *value == theme).map_or(theme, |(_, label)| *label);
        io.respond(Reply::text(format!("✅ Link OBS · layout **{name}**:\n{link}\n{LINK_WARNING}"))).await
    }

    /// Collega il servizio dell'evento live (serve a `/evento avvia|collega|termina`).
    pub fn set_live_controller(&self, controller: std::sync::Arc<dyn LiveController>) {
        let _ = self.live_controller.set(controller);
    }

    /// Conferma subito la ricezione, avvia l'evento e risponde con l'esito.
    async fn start_and_reply<I: InteractionIo>(&self, io: &I, guild_id: u64, channel_id: u64) -> Io<()> {
        io.defer().await?;
        let Some(controller) = self.live_controller.get() else {
            return io.followup(Reply::text(VOICE_UNAVAILABLE)).await;
        };
        let actor = u64::try_from(io.user_id()).unwrap_or_default();
        let outcome = controller.start(guild_id, channel_id, actor).await;
        io.followup(Reply::text(outcome.message(channel_id))).await
    }

    /// `/evento avvia canale:<canale vocale>`.
    pub async fn command_event_start<I: InteractionIo>(&self, io: &I, channel_id: u64) -> Io<()> {
        let (true, Some(guild_id)) = (self.authorize_live_staff(io), io.guild_id()) else {
            return io.respond(Reply::text(STAFF_ONLY)).await;
        };
        self.start_and_reply(io, guild_id, channel_id).await
    }

    /// `/evento collega`: avvia l'evento nel canale vocale in cui si trova chi lo chiede.
    pub async fn command_event_connect<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let (true, Some(guild_id)) = (self.authorize_live_staff(io), io.guild_id()) else {
            return io.respond(Reply::text(STAFF_ONLY)).await;
        };
        let user_id = u64::try_from(io.user_id()).unwrap_or_default();
        let Some(controller) = self.live_controller.get() else {
            return io.respond(Reply::text(VOICE_UNAVAILABLE)).await;
        };
        let Some(channel_id) = controller.standard_voice_channel_of(guild_id, user_id).await else {
            return io.respond(Reply::text(NOT_IN_VOICE)).await;
        };
        self.start_and_reply(io, guild_id, channel_id).await
    }

    /// `/evento termina`.
    pub async fn command_event_stop<I: InteractionIo>(&self, io: &I) -> Io<()> {
        if !self.authorize_live_staff(io) {
            return io.respond(Reply::text(STAFF_ONLY)).await;
        }
        io.defer().await?;
        let Some(controller) = self.live_controller.get() else {
            return io.followup(Reply::text(VOICE_UNAVAILABLE)).await;
        };
        let actor = u64::try_from(io.user_id()).unwrap_or_default();
        match controller.stop(actor).await {
            None => io.followup(Reply::text(NO_SESSION)).await,
            Some(event_id) => {
                io.followup(Reply::text(format!("✅ Evento live terminato; il bot ha lasciato il canale vocale. Report salvato come `#{event_id}`."))).await
            }
        }
    }

    // --- avvio automatico ---

    /// `/evento programma evento:<id>`.
    pub async fn command_event_schedule<I: InteractionIo>(&self, io: &I, event: &str) -> Io<()> {
        let (Some(guild), true) = (io.guild_id(), io.is_admin()) else {
            return io.respond(Reply::text("⚠️ Solo un amministratore può abilitare gli eventi automatici.")).await;
        };
        let Some(event_id) = live_event::python_int(event) else {
            return io.respond(Reply::text("⚠️ Seleziona un evento programmato valido dal menu.")).await;
        };
        let api = self.services.publisher.api();
        let events = api.scheduled_events(guild).await.map_err(|e| IoError(e.to_string()))?;
        let Some(scheduled) = events.iter().find(|candidate| i64::try_from(candidate.id).is_ok_and(|id| id == event_id)) else {
            return io.respond(Reply::text("⚠️ Evento programmato non trovato o già iniziato.")).await;
        };
        if scheduled.status != EventStatus::Scheduled {
            return io.respond(Reply::text("⚠️ Evento programmato non trovato o già iniziato.")).await;
        }
        let voice = match scheduled.channel_id {
            Some(channel) => api.channel_kind(guild, channel).await.unwrap_or(ChannelKind::Other) == ChannelKind::Voice,
            None => false,
        };
        if !voice {
            return io
                .respond(Reply::text(
                    "⚠️ L'avvio automatico richiede un evento collegato a un canale vocale standard; i canali Stage e gli eventi esterni non sono supportati.",
                ))
                .await;
        }
        let staff_channel = self.db().get_setting("staff_channel").ok().flatten().and_then(|text| live_event::python_int(&text));
        let staff_is_text = match staff_channel.and_then(|id| u64::try_from(id).ok()) {
            Some(channel) => api.channel_kind(guild, channel).await.unwrap_or(ChannelKind::Other) == ChannelKind::Text,
            None => false,
        };
        if !staff_is_text {
            return io
                .respond(Reply::text("⚠️ Configura prima il canale staff con `/config`: il link OBS automatico viene inviato solo lì."))
                .await;
        }
        let mut opted_in = live_event::scheduled_event_optins(self.db());
        opted_in.insert(event_id);
        live_event::save_scheduled_event_optins(self.db(), &opted_in).map_err(db_error)?;
        self.services.notifier.record_staff_audit(
            AuditEntry::new(u64::try_from(io.user_id()).ok(), "scheduled_event_opted_in")
                .entity("scheduled_event", event_id)
                .details(json!({"title": scheduled.name})),
        );
        io.respond(Reply::text(format!(
            "✅ **{}** abilitato. All'avvio dell'evento Discord il bot si collegherà al canale vocale; alla sua conclusione salverà il report e si disconnetterà. Verifica che i partecipanti siano informati della raccolta di durate stimate; l'audio non viene salvato.",
            scheduled.name
        )))
        .await
    }

    /// `/evento rimuovi-programma evento_id:<id>`.
    pub async fn command_event_unschedule<I: InteractionIo>(&self, io: &I, event: &str) -> Io<()> {
        if !Self::is_server_admin(io) {
            return io.respond(Reply::text("⚠️ Solo un amministratore può modificare gli eventi automatici.")).await;
        }
        // Gli ID degli eventi Discord superano il limite delle opzioni intere: arrivano come testo.
        let Some(event_id) = parse_event_id(event) else {
            return io.respond(Reply::text("❌ ID evento non valido: scegli un evento dall'elenco o incolla l'ID numerico.")).await;
        };
        let mut opted_in = live_event::scheduled_event_optins(self.db());
        if !opted_in.remove(&event_id) {
            return io.respond(Reply::text("ℹ️ Questo evento non era abilitato.")).await;
        }
        live_event::save_scheduled_event_optins(self.db(), &opted_in).map_err(db_error)?;
        self.services.notifier.record_staff_audit(
            AuditEntry::new(u64::try_from(io.user_id()).ok(), "scheduled_event_opted_out").entity("scheduled_event", event_id),
        );
        io.respond(Reply::text(format!(
            "✅ Avvio automatico disabilitato per l'evento `{event_id}`. Un evento già attivo non viene terminato da questa modifica."
        )))
        .await
    }

    // --- autocompletamento ---

    /// Gli eventi ancora da iniziare il cui nome contiene `current` (al massimo 25).
    pub async fn scheduled_event_choices(&self, guild_id: Option<u64>, current: &str) -> Vec<(String, String)> {
        let Some(guild) = guild_id else {
            return Vec::new();
        };
        let events = match self.services.publisher.api().scheduled_events(guild).await {
            Ok(events) => events,
            Err(e) => {
                warn!("Impossibile leggere gli eventi programmati per l'autocompletamento: {e}");
                return Vec::new();
            }
        };
        let needle = casefold(current);
        events
            .iter()
            .filter(|event| event.status == EventStatus::Scheduled && casefold(&event.name).contains(&needle))
            .take(25)
            .map(|event| (truncate_chars(&event.name, 100), event.id.to_string()))
            .collect()
    }

    /// Gli eventi già abilitati all'avvio automatico (al massimo 25).
    pub async fn opted_in_event_choices(&self, guild_id: Option<u64>, current: &str) -> Vec<(String, String)> {
        let events = match guild_id {
            Some(guild) => self.services.publisher.api().scheduled_events(guild).await.unwrap_or_default(),
            None => Vec::new(),
        };
        let needle = casefold(current);
        let mut choices = Vec::new();
        for event_id in live_event::scheduled_event_optins(self.db()) {
            let event = events.iter().find(|candidate| i64::try_from(candidate.id).is_ok_and(|id| id == event_id));
            let label = match event {
                Some(event) => format!("{} · {event_id}", event.name),
                None => format!("Evento {event_id}"),
            };
            if casefold(&label).contains(&needle) {
                choices.push((truncate_chars(&label, 100), event_id.to_string()));
            }
        }
        choices.truncate(25);
        choices
    }
}
