//! Logica e implementazione dei comandi slash `/config`, `/feed-attivita` e del centro di controllo dello staff.
//!
//! Questo modulo replica e estende le funzionalità originariamente presenti in `cogs/admin.py` del bot Discord in Python. Fornisce i metodi
//! che vengono richiamati dai veri comandi `poise` (vedi `crate::commands`), dopo che questi hanno raccolto gli argomenti dall'utente.
//!
//! ### Concetti chiave:
//! - **`Interactions`**: La struttura principale che gestisce le interazioni con Discord.
//! - **`InteractionIo`**: Un trait (interfaccia) che astrae l'input/output dell'interazione, permettendo di testare la logica con implementazioni finte.
//! - **`ConfigRequest`**: Una struttura dati che contiene tutti i possibili parametri per il comando `/config`.
//! - **`ControlComponent`**: Un enum che definisce i diversi tipi di componenti interattivi (bottoni, menu a tendina) presenti nel "Centro controllo staff".
//! - **`AuditEntry`**: Registra le azioni dello staff per scopi di auditing e trasparenza.
//! - **`rusqlite::Error`**: Errore standard per le operazioni sul database SQLite.
//! - **`BTreeSet`**: Una collezione ordinata di elementi unici, usata qui per gestire i ruoli dello staff senza duplicati e in ordine.

use std::collections::BTreeSet;

use poise::serenity_prelude::{
    ButtonStyle, CreateActionRow, CreateButton, CreateInputText, CreateModal, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption, InputTextStyle, ReactionType,
};
use serde_json::json;
use tracing::error;

use super::{InteractionIo, Interactions, Io, IoError, Reply};
use crate::cards::EmbedSpec;
use crate::control_center::{self, SECTIONS};
use crate::discord_api::DiscordApi;
use crate::hooks::publish_site_change;
use crate::notify::{AuditEntry, DiscordSender};
use crate::schedule::{isoformat, last_scheduled_reset, poll_closing_time};
use crate::settings;

const NOT_ADMIN: &str = "⚠️ Non hai i permessi per usare questo comando.";

/// Gli argomenti (o "opzioni") che possono essere passati al comando slash `/config`.
/// Ogni campo è un `Option<T>`, il che significa che l'utente può scegliere di impostare solo alcune
/// di queste opzioni, lasciando le altre invariate. I canali e i ruoli sono rappresentati da ID numerici di Discord (`u64`).
#[derive(Debug, Clone, Default)]
pub struct ConfigRequest {
    /// L'ID del canale Discord dove verranno pubblicati i sondaggi.
    pub poll_channel: Option<u64>,
    /// L'ID del canale Discord riservato allo staff, dove arrivano le notifiche interne.
    pub staff_channel: Option<u64>,
    /// L'ID del canale Discord dove vengono inviati i report degli eventi live.
    pub event_report_channel: Option<u64>,
    /// L'ID del canale Discord dove il bot invierà messaggi di benvenuto ai nuovi membri.
    pub welcome_channel: Option<u64>,
    /// L'ID del ruolo Discord che identifica i membri dello staff.
    pub staff_role: Option<u64>,
    /// Il numero di "slot" disponibili per le proposte nel ballottaggio settimanale.
    pub slots: Option<i64>,
    /// Indica se i messaggi di benvenuto automatici devono essere attivati (`true`) o disattivati (`false`).
    pub welcome_enabled: Option<bool>,
    /// Il numero massimo di voti che un utente può esprimere per ciascun sondaggio.
    pub max_votes: Option<i64>,
    /// Il numero massimo di proposte che possono essere presentate in una settimana.
    pub max_proposals: Option<i64>,
    /// Il giorno della settimana in cui avviene il reset settimanale. 0 = lunedì, 6 = domenica.
    pub reset_weekday: Option<i64>,
    /// L'ora del giorno in cui avviene il reset settimanale (in formato 24 ore).
    pub reset_hour: Option<i64>,
    /// Indica se deve essere inviato un promemoria prima del reset settimanale (`true`) o meno (`false`).
    pub reset_reminder: Option<bool>,
    /// L'ID del canale Discord dedicato alle "Game Night" o eventi simili.
    pub game_night_channel: Option<u64>,
    /// Un motivo facoltativo per la modifica della configurazione, registrato nell'audit dello staff.
    pub reason: Option<String>,
}

fn db_error(error: rusqlite::Error) -> IoError {
    IoError(error.to_string())
}

fn on_off(value: bool, on: &str, off: &str) -> String {
    if value { on } else { off }.to_string()
}

impl<S: DiscordSender, A: DiscordApi> Interactions<S, A> {
    /// `/config`: imposta canali, ruoli staff, limiti e orario del reset; senza opzioni mostra
    /// la configurazione corrente.
    pub async fn command_config<I: InteractionIo>(&self, io: &I, request: ConfigRequest) -> Io<()> {
        if !io.is_admin() {
            return io.respond(Reply::text(NOT_ADMIN)).await;
        }
        let db = self.db();
        if request.welcome_enabled == Some(true) && request.welcome_channel.is_none() && !self.setting_is_set("welcome_channel") {
            return io.respond(Reply::text("⚠️ Configura prima `canale_benvenuto`.")).await;
        }
        let mut out: Vec<String> = Vec::new();
        if let Some(id) = request.poll_channel {
            db.set_setting("poll_channel", id).map_err(db_error)?;
            out.push(format!("Canale sondaggi: <#{id}>"));
        }
        if let Some(id) = request.staff_channel {
            db.set_setting("staff_channel", id).map_err(db_error)?;
            out.push(format!("Canale staff: <#{id}>"));
        }
        if let Some(id) = request.event_report_channel {
            db.set_setting("event_report_channel", id).map_err(db_error)?;
            out.push(format!("Canale report eventi: <#{id}>"));
        }
        if let Some(id) = request.welcome_channel {
            db.set_setting("welcome_channel", id).map_err(db_error)?;
            out.push(format!("Canale benvenuto: <#{id}>"));
        }
        if let Some(enabled) = request.welcome_enabled {
            db.set_setting("welcome_enabled", i64::from(enabled)).map_err(db_error)?;
            out.push(format!("Accoglienza: {}", on_off(enabled, "attiva", "disattivata")));
        }
        if let Some(role) = request.staff_role {
            if io.guild_id() == Some(role) {
                out.push("❌ @everyone non può essere un ruolo staff: renderebbe staff tutti i membri.".to_string());
            } else {
                let current = db.get_setting_or("staff_roles", "").map_err(db_error)?;
                let mut ids: BTreeSet<String> = current.split(',').filter(|value| !value.is_empty()).map(str::to_string).collect();
                let key = role.to_string();
                if ids.remove(&key) {
                    out.push(format!("<@&{role}> **rimosso** dallo staff"));
                } else {
                    ids.insert(key);
                    out.push(format!("<@&{role}> **aggiunto** allo staff"));
                }
                db.set_setting("staff_roles", ids.into_iter().collect::<Vec<_>>().join(",")).map_err(db_error)?;
            }
        }
        if let Some(slots) = request.slots {
            db.set_setting("slots", slots).map_err(db_error)?;
            out.push(format!("Slot ballottaggio: {slots}"));
        }
        if let Some(votes) = request.max_votes {
            db.set_setting("max_votes", votes).map_err(db_error)?;
            out.push(format!("Voti per sondaggio: {votes}"));
        }
        if let Some(proposals) = request.max_proposals {
            db.set_setting("max_proposals", proposals).map_err(db_error)?;
            out.push(format!("Proposte a settimana: {proposals}"));
        }
        if request.reset_weekday.is_some() || request.reset_hour.is_some() {
            if let Some(weekday) = request.reset_weekday {
                db.set_setting("reset_weekday", weekday).map_err(db_error)?;
            }
            if let Some(hour) = request.reset_hour {
                db.set_setting("reset_hour", hour).map_err(db_error)?;
            }
            // Il nuovo orario parte dalla prossima occorrenza: senza questo, spostare il reset
            // indietro nella settimana farebbe scattare subito un reset.
            let now = self.now();
            db.set_setting("last_reset", isoformat(last_scheduled_reset(db, now).map_err(db_error)?)).map_err(db_error)?;
            let closing = poll_closing_time(db, now).map_err(db_error)?;
            db.transaction(|tx| tx.execute("UPDATE polls SET closes_at=? WHERE active=1", [closing])).map_err(db_error)?;
            out.push(format!("Reset settimanale: {}", settings::reset_label(db).map_err(db_error)?));
        }
        if let Some(enabled) = request.reset_reminder {
            db.set_setting("reset_reminder_enabled", i64::from(enabled)).map_err(db_error)?;
            out.push(format!("Promemoria reset: {}", on_off(enabled, "attivo", "disattivato")));
        }
        if let Some(id) = request.game_night_channel {
            db.set_setting("game_night_channel", id).map_err(db_error)?;
            out.push(format!("Canale Game Night: <#{id}>"));
        }
        if out.is_empty() {
            out.push(self.config_summary().map_err(db_error)?);
        }

        let changed = request.poll_channel.is_some()
            || request.staff_channel.is_some()
            || request.event_report_channel.is_some()
            || request.welcome_channel.is_some()
            || request.staff_role.is_some()
            || request.slots.is_some()
            || request.welcome_enabled.is_some()
            || request.max_votes.is_some()
            || request.max_proposals.is_some()
            || request.reset_weekday.is_some()
            || request.reset_hour.is_some()
            || request.reset_reminder.is_some()
            || request.game_night_channel.is_some();
        let mut text = out.join("\n");
        let mut refresh_everything = false;
        if changed {
            text = format!("✅ {text}");
            let mut entry = AuditEntry::new(u64::try_from(io.user_id()).ok(), "configuration_updated")
                .reason(request.reason.as_deref())
                .details(json!({
                    "poll_channel": request.poll_channel,
                    "staff_channel": request.staff_channel,
                    "event_report_channel": request.event_report_channel,
                    "welcome_channel": request.welcome_channel,
                    "staff_role": request.staff_role,
                    "slots": request.slots,
                    "welcome_enabled": request.welcome_enabled,
                    "max_votes": request.max_votes,
                    "max_proposals": request.max_proposals,
                    "reset_weekday": request.reset_weekday,
                    "reset_hour": request.reset_hour,
                    "reset_reminder_enabled": request.reset_reminder,
                    "game_night_channel": request.game_night_channel,
                }));
            entry.entity_type = Some("settings".to_string());
            self.services.notifier.record_staff_audit(entry);
            publish_site_change("settings");
            // Limiti e orari compaiono nei messaggi dei sondaggi: si aggiornano tutti.
            refresh_everything = request.max_votes.is_some() || request.reset_weekday.is_some() || request.reset_hour.is_some();
        }
        io.respond(Reply::text(text)).await?;
        let publisher = &self.services.publisher;
        let refreshed = if refresh_everything { publisher.refresh_all().await } else { publisher.refresh_dashboard().await };
        if let Err(e) = refreshed {
            error!("Aggiornamento dei messaggi dopo /config non riuscito: {e}");
        }
        Ok(())
    }

    /// La configurazione corrente, quando `/config` non ha opzioni.
    fn config_summary(&self) -> rusqlite::Result<String> {
        let db = self.db();
        let get = |key: &str| db.get_setting_or(key, "0");
        let staff_roles = db.get_setting("staff_roles")?.filter(|value| !value.is_empty()).unwrap_or_else(|| "nessuno".to_string());
        let game_night = match db.get_setting("game_night_channel")?.filter(|value| !value.is_empty()) {
            Some(id) => format!("<#{id}>"),
            None => "non impostato".to_string(),
        };
        let welcome = on_off(db.get_setting_or("welcome_enabled", "1")? == "1", "attiva", "disattivata");
        Ok(format!(
            "Canale sondaggi: <#{}>\nCanale staff: <#{}>\nCanale report eventi: <#{}>\nCanale benvenuto: <#{}>\nAccoglienza: {welcome}\nRuoli staff: {staff_roles}\nSlot: {}\nVoti per sondaggio: {} · proposte a settimana: {}\nReset: {} · promemoria {}\nCanale Game Night: {game_night}",
            get("poll_channel")?,
            get("staff_channel")?,
            get("event_report_channel")?,
            get("welcome_channel")?,
            settings::ballot_slots(db)?,
            settings::max_votes(db)?,
            settings::max_proposals(db)?,
            settings::reset_label(db)?,
            on_off(settings::reminder_enabled(db)?, "attivo", "disattivato"),
        ))
    }

    /// `/feed-attivita`: attiva o disattiva gli annunci automatici e sceglie il canale.
    pub async fn command_activity_feed<I: InteractionIo>(
        &self,
        io: &I,
        enabled: bool,
        channel: Option<u64>,
        reason: Option<String>,
    ) -> Io<()> {
        if !io.is_admin() {
            return io.respond(Reply::text(NOT_ADMIN)).await;
        }
        let db = self.db();
        if let Some(id) = channel {
            db.set_setting("activity_channel", id).map_err(db_error)?;
        }
        let channel_id = db.get_setting("activity_channel").map_err(db_error)?.filter(|value| !value.is_empty());
        if enabled && channel_id.is_none() {
            return io.respond(Reply::text("⚠️ Indica un canale con l'opzione `canale` prima di attivare il feed.")).await;
        }
        db.set_setting("activity_feed_enabled", i64::from(enabled)).map_err(db_error)?;
        let mut entry = AuditEntry::new(u64::try_from(io.user_id()).ok(), "activity_feed_updated")
            .reason(reason.as_deref())
            .details(json!({"enabled": enabled, "channel_id": channel_id}));
        entry.entity_type = Some("settings".to_string());
        self.services.notifier.record_staff_audit(entry);
        let message = match (&channel_id, enabled) {
            (Some(id), true) => format!("✅ Feed attività attivato in <#{id}>."),
            (Some(id), false) => format!("✅ Feed attività disattivato. Il canale configurato resta <#{id}>."),
            (None, _) => "✅ Feed attività disattivato.".to_string(),
        };
        io.respond(Reply::text(message)).await
    }
}

// --- centro controllo ------------------------------------------------------------------------

/// Modale «Analisi proposta» e i suoi campi.
pub const PROPOSAL_MODAL_ID: &str = "ctl:proposal-modal";
pub const PROPOSAL_NAME_FIELD: &str = "name";
pub const PROPOSAL_TYPE_FIELD: &str = "tipo";

const PANEL_OWNER_ONLY: &str = "⚠️ Questo pannello è riservato a chi l'ha aperto.";
const PANEL_NOT_STAFF_ANYMORE: &str = "⚠️ Non hai più i permessi per usare il pannello.";
const SEARCH_OWNER_ONLY: &str = "⚠️ Questa ricerca è riservata a chi ha aperto il pannello.";
const NO_PERMISSION_FOR_FUNCTION: &str = "⚠️ Non hai i permessi per usare questa funzione.";
const EXPORT_ADMINS_ONLY: &str = "⚠️ L'esportazione è riservata agli amministratori.";
const EXPORT_FILENAME: &str = "sword-bot-export-redatto.db";

/// I componenti del pannello. Ognuno porta nel `custom_id` chi ha aperto il pannello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlComponent {
    /// Menu delle sezioni.
    Section { owner: i64 },
    /// «Cerca proposta» (sezione Proposte).
    Search { owner: i64 },
    /// Selezione di un membro (sezione Utenti).
    User { owner: i64 },
    /// «Verifica e aggiorna record» (sezione Sistema).
    Refresh { owner: i64 },
    /// «Esporta database redatto» (sezione Sistema, solo amministratori).
    Export { owner: i64 },
    /// Conferma dell'export.
    ExportYes { owner: i64 },
    /// Annulla l'export.
    ExportNo { owner: i64 },
}

impl ControlComponent {
    pub fn parse(custom_id: &str) -> Option<Self> {
        let rest = custom_id.strip_prefix("ctl:")?;
        let (kind, owner) = rest.rsplit_once(':')?;
        if owner.is_empty() || !owner.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let owner = owner.parse().ok()?;
        Some(match kind {
            "section" => Self::Section { owner },
            "search" => Self::Search { owner },
            "user" => Self::User { owner },
            "refresh" => Self::Refresh { owner },
            "export" => Self::Export { owner },
            "export-yes" => Self::ExportYes { owner },
            "export-no" => Self::ExportNo { owner },
            _ => return None,
        })
    }

    pub fn id(self) -> String {
        let (kind, owner) = match self {
            Self::Section { owner } => ("section", owner),
            Self::Search { owner } => ("search", owner),
            Self::User { owner } => ("user", owner),
            Self::Refresh { owner } => ("refresh", owner),
            Self::Export { owner } => ("export", owner),
            Self::ExportYes { owner } => ("export-yes", owner),
            Self::ExportNo { owner } => ("export-no", owner),
        };
        format!("ctl:{kind}:{owner}")
    }
}

/// I componenti del pannello per una sezione (`StaffControlPanelView`): il menu delle sezioni e,
/// sotto, i controlli della sezione. «Esporta» compare solo per gli amministratori.
pub fn panel_rows(owner: i64, section: Option<&str>, can_export: bool) -> Vec<CreateActionRow> {
    let options = SECTIONS
        .iter()
        .map(|(label, value, emoji)| {
            CreateSelectMenuOption::new(*label, *value).emoji(ReactionType::Unicode((*emoji).to_string())).default_selection(Some(*value) == section)
        })
        .collect();
    let select = CreateSelectMenu::new(ControlComponent::Section { owner }.id(), CreateSelectMenuKind::String { options })
        .placeholder("Scegli una sezione")
        .min_values(1)
        .max_values(1);
    let mut rows = vec![CreateActionRow::SelectMenu(select)];
    let button = |component: ControlComponent, label: &str, emoji: &str, style: ButtonStyle| {
        CreateButton::new(component.id()).label(label).emoji(ReactionType::Unicode(emoji.to_string())).style(style)
    };
    match section {
        Some("proposals") => {
            rows.push(CreateActionRow::Buttons(vec![button(ControlComponent::Search { owner }, "Cerca proposta", "🔎", ButtonStyle::Primary)]));
        }
        Some("users") => {
            let users = CreateSelectMenu::new(ControlComponent::User { owner }.id(), CreateSelectMenuKind::User { default_users: None })
                .placeholder("Cerca un membro del server")
                .min_values(1)
                .max_values(1);
            rows.push(CreateActionRow::SelectMenu(users));
        }
        Some("system") => {
            let mut buttons = vec![button(ControlComponent::Refresh { owner }, "Verifica e aggiorna record", "♻️", ButtonStyle::Secondary)];
            if can_export {
                buttons.push(button(ControlComponent::Export { owner }, "Esporta database redatto", "📤", ButtonStyle::Secondary));
            }
            rows.push(CreateActionRow::Buttons(buttons));
        }
        _ => {}
    }
    rows
}

/// La maschera di ricerca di una proposta.
pub fn proposal_search_modal() -> CreateModal {
    CreateModal::new(PROPOSAL_MODAL_ID, "Analisi proposta").components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Nome della proposta", PROPOSAL_NAME_FIELD)
                .placeholder("Es. Minecraft")
                .min_length(2)
                .max_length(80),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Tipo (facoltativo)", PROPOSAL_TYPE_FIELD)
                .placeholder("Es. Gioco")
                .required(false)
                .max_length(40),
        ),
    ])
}

/// I due pulsanti della conferma dell'export.
fn export_confirm_rows(owner: i64) -> Vec<CreateActionRow> {
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new(ControlComponent::ExportYes { owner }.id())
            .label("Conferma export redatto")
            .emoji(ReactionType::Unicode("📤".into()))
            .style(ButtonStyle::Primary),
        CreateButton::new(ControlComponent::ExportNo { owner }.id())
            .label("Annulla")
            .emoji(ReactionType::Unicode("✖️".into()))
            .style(ButtonStyle::Secondary),
    ])]
}

impl<S: DiscordSender, A: DiscordApi> Interactions<S, A> {
    /// `/centro-staff`: apre il centro controllo (solo staff).
    pub async fn command_control_center<I: InteractionIo>(&self, io: &I) -> Io<()> {
        if !self.is_staff(io) {
            return io.respond(Reply::text("⚠️ Questo centro di controllo è riservato allo staff.")).await;
        }
        let owner = io.user_id();
        io.respond(Reply::embed(control_center::staff_home_embed().build()).with_components(panel_rows(owner, None, io.is_admin()))).await
    }

    /// L'embed di una sezione del pannello.
    fn section_embed(&self, section: &str) -> Result<Option<EmbedSpec>, IoError> {
        let db = self.db();
        Ok(Some(match section {
            "handbook" => control_center::handbook_embed(),
            "analytics" => control_center::analytics_embed(db, self.now()).map_err(db_error)?,
            "proposals" => control_center::proposals_section_embed(),
            "users" => control_center::users_embed(db).map_err(db_error)?,
            "system" => control_center::system_embed(db, &control_center::SystemInfo::from_environment()).map_err(db_error)?,
            _ => return Ok(None),
        }))
    }

    pub(super) async fn control_component<I: InteractionIo>(&self, io: &I, component: ControlComponent) -> Io<()> {
        match component {
            ControlComponent::Section { owner } => {
                if io.user_id() != owner {
                    return io.respond(Reply::text(PANEL_OWNER_ONLY)).await;
                }
                if !self.is_staff(io) {
                    return io.respond(Reply::text(PANEL_NOT_STAFF_ANYMORE)).await;
                }
                let Some(section) = io.values().first().cloned() else {
                    return Ok(());
                };
                let Some(embed) = self.section_embed(&section)? else {
                    return Ok(());
                };
                io.update(Reply { embeds: Some(vec![embed.build()]), components: Some(panel_rows(owner, Some(&section), io.is_admin())), ..Reply::default() }).await
            }
            ControlComponent::Search { owner } => {
                if let Some(refusal) = self.panel_refusal(io, owner, SEARCH_OWNER_ONLY) {
                    return io.respond(Reply::text(refusal)).await;
                }
                io.show_modal(proposal_search_modal()).await
            }
            ControlComponent::User { owner } => {
                if let Some(refusal) = self.panel_refusal(io, owner, SEARCH_OWNER_ONLY) {
                    return io.respond(Reply::text(refusal)).await;
                }
                let Some(member) = io.selected_user() else {
                    return Ok(());
                };
                io.respond(Reply::embed(control_center::user_embed(self.db(), &member).map_err(db_error)?.build())).await
            }
            ControlComponent::Refresh { owner } => {
                if let Some(refusal) = self.panel_refusal(io, owner, "⚠️ Questa verifica è riservata a chi ha aperto il pannello.") {
                    return io.respond(Reply::text(refusal)).await;
                }
                self.refresh_staff_data(io).await
            }
            ControlComponent::Export { owner } => {
                if io.user_id() != owner {
                    return io.respond(Reply::text("⚠️ Questo export è riservato a chi ha aperto il pannello.")).await;
                }
                if !io.is_admin() {
                    return io.respond(Reply::text(EXPORT_ADMINS_ONLY)).await;
                }
                io.respond(
                    Reply::text(
                        "L'esportazione crea un backup SQLite temporaneo e rimuove i dati individuali di voto/supporto, i report evento nominativi, i punti stagionali individuali e le preferenze private. Vuoi continuare?",
                    )
                    .with_components(export_confirm_rows(io.user_id())),
                )
                .await
            }
            ControlComponent::ExportYes { owner } => {
                if io.user_id() != owner {
                    return io.respond(Reply::text("⚠️ Questa conferma è riservata a chi l'ha richiesta.")).await;
                }
                if !io.is_admin() {
                    return io.respond(Reply::text(EXPORT_ADMINS_ONLY)).await;
                }
                self.export_database(io).await
            }
            ControlComponent::ExportNo { owner } => {
                if io.user_id() != owner {
                    return io.respond(Reply::text("⚠️ Questa conferma è riservata a chi l'ha richiesta.")).await;
                }
                io.update(Reply { content: Some("Export annullato.".into()), embeds: Some(Vec::new()), components: Some(Vec::new()), v2: None }).await
            }
        }
    }

    /// Il rifiuto per un controllo del pannello: non è di chi l'ha aperto, o non è più staff.
    fn panel_refusal<I: InteractionIo>(&self, io: &I, owner: i64, owner_message: &'static str) -> Option<&'static str> {
        if io.user_id() != owner {
            Some(owner_message)
        } else if !self.is_staff(io) {
            Some(NO_PERMISSION_FOR_FUNCTION)
        } else {
            None
        }
    }

    /// «Verifica e aggiorna record»: ricalcola i record correnti, senza toccare i massimi archiviati.
    async fn refresh_staff_data<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let result = self.db().transaction(|tx| -> rusqlite::Result<()> {
            let week = crate::database::current_week(tx)?;
            crate::stats::update_community_records(tx, week)?;
            crate::cards::build_contributor_leaderboard_embed(tx, 10)?;
            crate::cards::build_combined_hall_of_fame_embed(tx)?;
            Ok(())
        });
        if let Err(e) = result {
            error!("Errore SQLite durante la verifica dei dati staff: {e}");
            return io.respond(Reply::text("❌ Verifica non riuscita. Controlla i log del bot.")).await;
        }
        io.respond(Reply::text(
            "✅ Classifica e Hall of Fame verificate; i record correnti sono stati aggiornati senza cancellare o riscrivere i massimi archiviati.",
        ))
        .await
    }

    /// Crea l'export redatto e lo invia come file, poi lo registra nell'audit.
    async fn export_database<I: InteractionIo>(&self, io: &I) -> Io<()> {
        io.defer().await?;
        let db = self.db().clone();
        let exported = tokio::task::spawn_blocking(move || control_center::create_redacted_export(&db)).await;
        let sent = match exported {
            Ok(Ok(bytes)) => {
                let text = "✅ Export SQLite pronto. I voti e i supporti individuali, le preferenze DM, i badge segreti, i punti stagionali individuali, i report evento nominativi e le settimane di ballottaggio personali sono stati rimossi; restano solo i riepiloghi aggregati.";
                io.followup_file(text.to_string(), EXPORT_FILENAME.to_string(), bytes).await.map_err(|e| e.to_string())
            }
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match sent {
            Ok(()) => {
                let mut entry = AuditEntry::new(u64::try_from(io.user_id()).ok(), "redacted_database_exported");
                entry.entity_type = Some("database".to_string());
                self.services.notifier.record_staff_audit(entry);
                Ok(())
            }
            Err(e) => {
                error!("Impossibile creare o inviare l'export SQLite redatto: {e}");
                io.followup(Reply::text("❌ Export non riuscito. Controlla i log del bot.")).await
            }
        }
    }

    /// La maschera «Analisi proposta» inviata.
    pub(super) async fn proposal_search_submitted<I: InteractionIo>(&self, io: &I) -> Io<()> {
        if !self.is_staff(io) {
            return io.respond(Reply::text("⚠️ Non hai più i permessi per consultare i dati staff.")).await;
        }
        let name = io.modal_field(PROPOSAL_NAME_FIELD).unwrap_or_default();
        let tipo = io.modal_field(PROPOSAL_TYPE_FIELD).filter(|value| !value.is_empty());
        let embed = control_center::proposal_analytics_embed(self.db(), &name, tipo.as_deref()).map_err(db_error)?;
        io.respond(Reply::embed(embed.build())).await
    }
}
