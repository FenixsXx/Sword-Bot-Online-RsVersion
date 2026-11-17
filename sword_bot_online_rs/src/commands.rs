//! Comandi slash principali (equivalente di `cogs/core_commands.py`): proposte, ballottaggio,
//! storico, reset e sincronizzazione.
//!
//! I comandi sono sottili: costruiscono un [`CommandIo`] sull'interazione e passano a
//! `Interactions`, dove sta la logica (con le conferme e i pulsanti di `/staff`).

use poise::serenity_prelude::{self as serenity, AutocompleteChoice, CreateAutocompleteResponse, ResolvedOption, ResolvedValue};
use tracing::error;

use crate::autocomplete::{any_suggestion, approved_unpicked_suggestion, open_review_suggestion, pending_review_suggestion, picked_suggestion};
use crate::hub::tipo_autocomplete;
use crate::control_center::MemberInfo;
use crate::interactions::{CommandIo, ConfigRequest, CreatePollRequest, InteractionIo, Reply, option_choices, poll_choices};
use crate::permissions::{admin_only, staff_only};
use crate::{Context, Error};

/// L'interazione del comando corrente. Questi comandi esistono solo come slash.
fn command_io(ctx: Context<'_>) -> Result<CommandIo<'_>, Error> {
    match ctx {
        poise::Context::Application(application) => Ok(CommandIo { ctx: ctx.serenity_context(), interaction: application.interaction }),
        poise::Context::Prefix(_) => Err("questo comando è disponibile solo come comando slash".into()),
    }
}

/// Forza il reset settimanale
#[poise::command(
    slash_command,
    rename = "reset-manuale",
    guild_only,
    default_member_permissions = "ADMINISTRATOR",
    check = "admin_only"
)]
pub async fn reset_manuale(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_manual_reset(&command_io(ctx)?).await?;
    Ok(())
}

/// Risincronizza i comandi slash con Discord
#[poise::command(slash_command, guild_only, default_member_permissions = "ADMINISTRATOR", check = "admin_only")]
pub async fn sync(ctx: Context<'_>) -> Result<(), Error> {
    let io = command_io(ctx)?;
    io.defer().await?;
    let data = ctx.data();
    let result = crate::sync_commands(ctx.serenity_context(), &ctx.framework().options().commands, data.guild_id, &data.db, true).await;
    match result {
        Ok(_) => io.followup(Reply::text("✅ Comandi slash sincronizzati.")).await?,
        Err(e) => {
            error!("Sincronizzazione manuale dei comandi non riuscita: {e}");
            io.followup(Reply::text("❌ Sincronizzazione non riuscita, riprova tra qualche minuto.")).await?;
        }
    }
    Ok(())
}

/// Proponi un gioco o un evento
#[poise::command(slash_command, guild_only)]
pub async fn suggerisci(
    ctx: Context<'_>,
    #[description = "Nome del gioco o evento"]
    #[min_length = 2]
    #[max_length = 80]
    nome: String,
    #[description = "Gioco, Film, Karaoke..."]
    #[autocomplete = "tipo_autocomplete"]
    tipo: String,
    #[description = "Link (facoltativo)"] link: Option<String>,
) -> Result<(), Error> {
    ctx.data().interactions.command_suggest(&command_io(ctx)?, &nome, &tipo, link).await?;
    Ok(())
}

/// Mette una proposta nel sondaggio finale
#[poise::command(slash_command, guild_only, check = "staff_only")]
pub async fn scegli(
    ctx: Context<'_>,
    #[description = "Proposta approvata da inserire (scrivi parte del nome)"]
    #[autocomplete = "approved_unpicked_suggestion"]
    id: i64,
    #[description = "Motivo della selezione (facoltativo)"] motivazione: Option<String>,
) -> Result<(), Error> {
    ctx.data().interactions.command_pick(&command_io(ctx)?, id, motivazione.as_deref()).await?;
    Ok(())
}

/// Toglie una proposta dal sondaggio finale
#[poise::command(slash_command, rename = "rimuovi-scelta", guild_only, check = "staff_only")]
pub async fn rimuovi_scelta(
    ctx: Context<'_>,
    #[description = "Proposta da togliere (scrivi parte del nome)"]
    #[autocomplete = "picked_suggestion"]
    id: i64,
    #[description = "Motivo della rimozione (facoltativo)"] motivazione: Option<String>,
) -> Result<(), Error> {
    ctx.data().interactions.command_unpick(&command_io(ctx)?, id, motivazione).await?;
    Ok(())
}

/// Unisce due proposte duplicate
#[poise::command(slash_command, rename = "unisci-proposte", guild_only, check = "staff_only")]
pub async fn unisci_proposte(
    ctx: Context<'_>,
    #[description = "Proposta da tenere"]
    #[autocomplete = "any_suggestion"]
    principale: i64,
    #[description = "Proposta da assorbire"]
    #[autocomplete = "any_suggestion"]
    duplicata: i64,
    #[description = "Motivo dell'unione (facoltativo)"] motivazione: Option<String>,
) -> Result<(), Error> {
    ctx.data().interactions.command_merge(&command_io(ctx)?, principale, duplicata, motivazione).await?;
    Ok(())
}

/// Strumenti dello staff: selezione proposte e centro controllo
#[poise::command(slash_command, guild_only, check = "staff_only")]
pub async fn staff(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_staff(&command_io(ctx)?).await?;
    Ok(())
}

/// Ultime settimane chiuse
#[poise::command(slash_command, guild_only, check = "staff_only")]
pub async fn storico(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_history(&command_io(ctx)?).await?;
    Ok(())
}

/// Giorno della chiusura settimanale: il valore inviato a Discord è l'indice (0 = lunedì).
#[derive(Debug, Clone, Copy, poise::ChoiceParameter)]
pub enum ResetDay {
    #[name = "Lunedì"]
    Lunedi,
    #[name = "Martedì"]
    Martedi,
    #[name = "Mercoledì"]
    Mercoledi,
    #[name = "Giovedì"]
    Giovedi,
    #[name = "Venerdì"]
    Venerdi,
    #[name = "Sabato"]
    Sabato,
    #[name = "Domenica"]
    Domenica,
}

/// Imposta canali, ruoli staff e slot del ballottaggio
#[poise::command(slash_command, guild_only, default_member_permissions = "ADMINISTRATOR")]
#[allow(clippy::too_many_arguments)]
pub async fn config(
    ctx: Context<'_>,
    #[description = "Canale dove compaiono i sondaggi"]
    #[channel_types("Text", "News")]
    canale_sondaggi: Option<serenity::ChannelId>,
    #[description = "Canale riservato allo staff"]
    #[channel_types("Text", "News")]
    canale_staff: Option<serenity::ChannelId>,
    #[description = "Canale dove pubblicare i report SFO"]
    #[channel_types("Text", "News")]
    canale_eventi: Option<serenity::ChannelId>,
    #[description = "Canale dove accogliere i nuovi membri"]
    #[channel_types("Text", "News")]
    canale_benvenuto: Option<serenity::ChannelId>,
    #[description = "Ruolo da aggiungere/togliere dallo staff"] ruolo_staff: Option<serenity::RoleId>,
    #[description = "Quante proposte scelte dallo staff nel sondaggio finale (3-5)"]
    #[min = 3]
    #[max = 5]
    slot: Option<i64>,
    #[description = "Abilita o disabilita i messaggi di benvenuto"] accoglienza_attiva: Option<bool>,
    #[description = "Quanti voti ha ogni persona in ciascun sondaggio"]
    #[min = 1]
    #[max = 10]
    voti_per_sondaggio: Option<i64>,
    #[description = "Quante proposte può inviare ogni persona a settimana"]
    #[min = 1]
    #[max = 20]
    proposte_a_settimana: Option<i64>,
    #[description = "Giorno della chiusura settimanale"] giorno_reset: Option<ResetDay>,
    #[description = "Ora della chiusura settimanale (0-23, ora italiana)"]
    #[min = 0]
    #[max = 23]
    ora_reset: Option<i64>,
    #[description = "Avviso pubblico 24 ore prima del reset"] promemoria_reset: Option<bool>,
    #[description = "Canale vocale dove creare gli eventi della Game Night"]
    #[channel_types("Voice", "Stage")]
    canale_game_night: Option<serenity::ChannelId>,
    #[description = "Motivo della modifica (facoltativo)"] motivazione: Option<String>,
) -> Result<(), Error> {
    let request = ConfigRequest {
        poll_channel: canale_sondaggi.map(|id| id.get()),
        staff_channel: canale_staff.map(|id| id.get()),
        event_report_channel: canale_eventi.map(|id| id.get()),
        welcome_channel: canale_benvenuto.map(|id| id.get()),
        staff_role: ruolo_staff.map(|id| id.get()),
        slots: slot,
        welcome_enabled: accoglienza_attiva,
        max_votes: voti_per_sondaggio,
        max_proposals: proposte_a_settimana,
        reset_weekday: giorno_reset.map(|day| day as i64),
        reset_hour: ora_reset,
        reset_reminder: promemoria_reset,
        game_night_channel: canale_game_night.map(|id| id.get()),
        reason: motivazione,
    };
    ctx.data().interactions.command_config(&command_io(ctx)?, request).await?;
    Ok(())
}

/// Configura il canale dedicato e attiva o disattiva il feed
#[poise::command(slash_command, rename = "feed-attivita", guild_only, default_member_permissions = "ADMINISTRATOR")]
pub async fn feed_attivita(
    ctx: Context<'_>,
    #[description = "Attiva o disattiva gli annunci automatici"] attivo: bool,
    #[description = "Canale testuale in cui pubblicarli"]
    #[channel_types("Text", "News")]
    canale: Option<serenity::ChannelId>,
    #[description = "Motivo della modifica (facoltativo)"] motivazione: Option<String>,
) -> Result<(), Error> {
    ctx.data().interactions.command_activity_feed(&command_io(ctx)?, attivo, canale.map(|id| id.get()), motivazione).await?;
    Ok(())
}

/// Apri il centro di controllo e le analisi per lo staff
#[poise::command(slash_command, rename = "centro-staff", guild_only)]
pub async fn centro_staff(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_control_center(&command_io(ctx)?).await?;
    Ok(())
}

/// Elenco delle proposte della settimana
#[poise::command(slash_command, guild_only)]
pub async fn proposte(
    ctx: Context<'_>,
    #[description = "Filtra per tipo (facoltativo)"]
    #[autocomplete = "tipo_autocomplete"]
    tipo: Option<String>,
) -> Result<(), Error> {
    ctx.data().interactions.command_proposals(&command_io(ctx)?, tipo.as_deref()).await?;
    Ok(())
}

/// Revisione delle proposte della community
#[poise::command(slash_command, subcommands("proposta_revisiona", "proposta_approva", "proposta_rifiuta"), subcommand_required)]
pub async fn proposta(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Prende in carico una proposta
#[poise::command(slash_command, rename = "revisiona")]
pub async fn proposta_revisiona(
    ctx: Context<'_>,
    #[description = "Proposta da prendere in carico (scrivi parte del nome)"]
    #[autocomplete = "pending_review_suggestion"]
    id: i64,
) -> Result<(), Error> {
    ctx.data().interactions.command_review(&command_io(ctx)?, id, "review", None).await?;
    Ok(())
}

/// Approva una proposta
#[poise::command(slash_command, rename = "approva")]
pub async fn proposta_approva(
    ctx: Context<'_>,
    #[description = "Proposta da approvare (scrivi parte del nome)"]
    #[autocomplete = "open_review_suggestion"]
    id: i64,
) -> Result<(), Error> {
    ctx.data().interactions.command_review(&command_io(ctx)?, id, "approve", None).await?;
    Ok(())
}

/// Rifiuta una proposta
#[poise::command(slash_command, rename = "rifiuta")]
pub async fn proposta_rifiuta(
    ctx: Context<'_>,
    #[description = "Proposta da rifiutare (scrivi parte del nome)"]
    #[autocomplete = "open_review_suggestion"]
    id: i64,
    #[description = "Motivo comunicato agli autori della proposta"]
    #[min_length = 1]
    #[max_length = 200]
    motivo: String,
) -> Result<(), Error> {
    ctx.data().interactions.command_review(&command_io(ctx)?, id, "reject", Some(&motivo)).await?;
    Ok(())
}

// --- sondaggi e opzioni (cogs/polls.py) -------------------------------------------------------

/// Il valore di un'opzione già scritta (anche dentro un sottocomando), per gli autocompletamenti
/// che dipendono da un altro argomento (`interaction.namespace` di Python).
fn typed_option(ctx: Context<'_>, name: &str) -> Option<String> {
    fn find(options: &[ResolvedOption<'_>], name: &str) -> Option<String> {
        for option in options {
            match &option.value {
                ResolvedValue::String(value) if option.name == name => return Some((*value).to_string()),
                ResolvedValue::Autocomplete { value, .. } if option.name == name => return Some((*value).to_string()),
                ResolvedValue::SubCommand(inner) | ResolvedValue::SubCommandGroup(inner) => {
                    if let Some(found) = find(inner, name) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        None
    }
    match ctx {
        poise::Context::Application(application) => find(&application.interaction.data.options(), name),
        poise::Context::Prefix(_) => None,
    }
}

fn choices_response(names: Vec<String>) -> CreateAutocompleteResponse {
    CreateAutocompleteResponse::new().set_choices(names.into_iter().map(|name| AutocompleteChoice::new(name.clone(), name)).collect::<Vec<_>>())
}

async fn poll_autocomplete(ctx: Context<'_>, current: &str) -> CreateAutocompleteResponse {
    choices_response(poll_choices(&ctx.data().db, current).unwrap_or_else(|e| {
        error!("Autocompletamento dei sondaggi non riuscito: {e}");
        Vec::new()
    }))
}

async fn option_autocomplete(ctx: Context<'_>, current: &str) -> CreateAutocompleteResponse {
    let poll = typed_option(ctx, "sondaggio");
    choices_response(option_choices(&ctx.data().db, poll.as_deref(), current).unwrap_or_else(|e| {
        error!("Autocompletamento delle opzioni non riuscito: {e}");
        Vec::new()
    }))
}

/// Gestione sondaggi
#[poise::command(
    slash_command,
    subcommands("sondaggio_crea", "sondaggio_rimuovi", "sondaggio_pubblica"),
    subcommand_required,
    default_member_permissions = "ADMINISTRATOR"
)]
pub async fn sondaggio(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Crea un nuovo sondaggio/categoria
#[poise::command(slash_command, rename = "crea")]
pub async fn sondaggio_crea(
    ctx: Context<'_>,
    #[description = "Nome della categoria"]
    #[min_length = 1]
    #[max_length = 100]
    nome: String,
    #[description = "Emoji del titolo"] emoji: Option<String>,
    #[description = "Colore HEX, es. #5865F2"] colore: Option<String>,
    #[description = "È il sondaggio delle proposte della community?"] community: Option<bool>,
    #[description = "Motivo della modifica (facoltativo)"] motivazione: Option<String>,
) -> Result<(), Error> {
    let mut request = CreatePollRequest::new(nome);
    if let Some(emoji) = emoji {
        request.emoji = emoji;
    }
    if let Some(color) = colore {
        request.color = color;
    }
    request.community = community.unwrap_or(false);
    request.reason = motivazione;
    ctx.data().interactions.command_poll_create(&command_io(ctx)?, request).await?;
    Ok(())
}

/// Elimina un sondaggio
#[poise::command(slash_command, rename = "rimuovi")]
pub async fn sondaggio_rimuovi(
    ctx: Context<'_>,
    #[description = "…"]
    #[autocomplete = "poll_autocomplete"]
    nome: String,
    #[description = "Motivo della rimozione (facoltativo)"] motivazione: Option<String>,
) -> Result<(), Error> {
    ctx.data().interactions.command_poll_remove(&command_io(ctx)?, &nome, motivazione).await?;
    Ok(())
}

/// Pubblica o ricrea tutti i messaggi dei sondaggi
#[poise::command(slash_command, rename = "pubblica")]
pub async fn sondaggio_pubblica(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_poll_publish(&command_io(ctx)?).await?;
    Ok(())
}

/// Gestione opzioni dei sondaggi
#[poise::command(slash_command, subcommands("opzione_aggiungi", "opzione_rimuovi"), subcommand_required)]
pub async fn opzione(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Aggiunge un'opzione a un sondaggio
#[poise::command(slash_command, rename = "aggiungi")]
pub async fn opzione_aggiungi(
    ctx: Context<'_>,
    #[description = "…"]
    #[autocomplete = "poll_autocomplete"]
    sondaggio: String,
    #[description = "…"]
    #[min_length = 1]
    #[max_length = 80]
    nome: String,
    #[description = "Motivo della modifica (facoltativo)"] motivazione: Option<String>,
) -> Result<(), Error> {
    ctx.data().interactions.command_option_add(&command_io(ctx)?, &sondaggio, &nome, motivazione).await?;
    Ok(())
}

/// Rimuove un'opzione da un sondaggio
#[poise::command(slash_command, rename = "rimuovi")]
pub async fn opzione_rimuovi(
    ctx: Context<'_>,
    #[description = "…"]
    #[autocomplete = "poll_autocomplete"]
    sondaggio: String,
    #[description = "…"]
    #[autocomplete = "option_autocomplete"]
    nome: String,
    #[description = "Motivo della rimozione (facoltativo)"] motivazione: Option<String>,
) -> Result<(), Error> {
    ctx.data().interactions.command_option_remove(&command_io(ctx)?, &sondaggio, &nome, motivazione).await?;
    Ok(())
}

// --- community e stagioni (cogs/community.py, cogs/seasons.py) ---------------------------------

/// Scopri come partecipare alla Game Night
#[poise::command(slash_command, guild_only)]
pub async fn guida(ctx: Context<'_>) -> Result<(), Error> {
    let public_url = std::env::var("EVENT_OVERLAY_PUBLIC_URL").unwrap_or_default();
    ctx.data().interactions.command_guide(&command_io(ctx)?, &public_url).await?;
    Ok(())
}

/// Mostra il tuo profilo o quello di un utente
#[poise::command(slash_command, guild_only)]
pub async fn profilo(
    ctx: Context<'_>,
    #[description = "La persona di cui vuoi vedere il profilo"] utente: Option<serenity::Member>,
) -> Result<(), Error> {
    let member = match utente {
        Some(member) => Some(member),
        None => ctx.author_member().await.map(|member| member.into_owned()),
    };
    let info = member.map(|member| MemberInfo { id: member.user.id.get() as i64, display_name: member.display_name().to_string(), avatar_url: member.face() });
    ctx.data().interactions.command_profile(&command_io(ctx)?, info).await?;
    Ok(())
}

/// Mostra la classifica pubblica dei contributor
#[poise::command(slash_command, rename = "classifica-utenti", guild_only)]
pub async fn classifica_utenti(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_contributor_leaderboard(&command_io(ctx)?).await?;
    Ok(())
}

/// Mostra i record storici della community
#[poise::command(slash_command, rename = "record-community", guild_only)]
pub async fn record_community(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_community_records(&command_io(ctx)?).await?;
    Ok(())
}

/// Gestisci le notifiche personali via messaggio privato
#[poise::command(slash_command, guild_only)]
pub async fn notifiche(
    ctx: Context<'_>,
    #[description = "Abilita o disabilita i DM personali"] attive: Option<bool>,
) -> Result<(), Error> {
    ctx.data().interactions.command_notifications(&command_io(ctx)?, attive).await?;
    Ok(())
}

/// Mostra la classifica della stagione corrente e i campioni
#[poise::command(slash_command, rename = "classifica-stagione", guild_only)]
pub async fn classifica_stagione(ctx: Context<'_>) -> Result<(), Error> {
    // Il nome nel server se il membro è in cache, altrimenti la menzione (`_member_label` di Python).
    // Si costruisce l'embed qui, senza `await` mentre si tiene la cache.
    let embed = {
        let guild = ctx.guild_id().and_then(|id| ctx.cache().guild(id));
        let label = |user_id: i64| {
            guild
                .as_ref()
                .and_then(|guild| guild.members.get(&serenity::UserId::new(user_id as u64)))
                .map_or_else(|| format!("<@{user_id}>"), |member| member.display_name().to_string())
        };
        ctx.data().interactions.build_season(&label)?
    };
    ctx.data().interactions.command_season(&command_io(ctx)?, embed).await?;
    Ok(())
}

// --- evento live (cogs/live_event.py) ------------------------------------------------------

fn value_choices(choices: Vec<(String, String)>) -> CreateAutocompleteResponse {
    CreateAutocompleteResponse::new().set_choices(choices.into_iter().map(|(name, value)| AutocompleteChoice::new(name, value)).collect::<Vec<_>>())
}

async fn scheduled_event_autocomplete(ctx: Context<'_>, current: &str) -> CreateAutocompleteResponse {
    value_choices(ctx.data().interactions.scheduled_event_choices(ctx.guild_id().map(|id| id.get()), current).await)
}

async fn opted_in_event_autocomplete(ctx: Context<'_>, current: &str) -> CreateAutocompleteResponse {
    value_choices(ctx.data().interactions.opted_in_event_choices(ctx.guild_id().map(|id| id.get()), current).await)
}

/// Avvia e controlla la modalità evento live
#[poise::command(
    slash_command,
    subcommands(
        "evento_rapporto",
        "evento_elimina_rapporto",
        "evento_avvia",
        "evento_collega",
        "evento_termina",
        "evento_stato",
        "evento_rigenera_link",
        "evento_link",
        "evento_programma",
        "evento_rimuovi_programma"
    ),
    subcommand_required
)]
pub async fn evento(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Mostra i report evento recenti o il dettaglio di un report
#[poise::command(slash_command, rename = "rapporto", guild_only)]
pub async fn evento_rapporto(
    ctx: Context<'_>,
    #[description = "ID del report da consultare"]
    #[min = 1]
    #[max = 2147483647]
    id_rapporto: Option<i64>,
) -> Result<(), Error> {
    ctx.data().interactions.command_event_report(&command_io(ctx)?, id_rapporto).await?;
    Ok(())
}

/// Elimina un report evento e i dati individuali associati
#[poise::command(slash_command, rename = "elimina-rapporto", guild_only)]
pub async fn evento_elimina_rapporto(
    ctx: Context<'_>,
    #[description = "ID del report da eliminare"]
    #[min = 1]
    #[max = 2147483647]
    id_rapporto: i64,
    #[description = "Motivo dell'eliminazione (facoltativo)"] motivazione: Option<String>,
) -> Result<(), Error> {
    ctx.data().interactions.command_event_delete_report(&command_io(ctx)?, id_rapporto, motivazione).await?;
    Ok(())
}

/// Avvia l'overlay collegandoti a un canale vocale
#[poise::command(slash_command, rename = "avvia", guild_only)]
pub async fn evento_avvia(
    ctx: Context<'_>,
    #[description = "Canale vocale della Game Night"]
    #[channel_types("Voice")]
    #[rename = "canale"]
    canale: serenity::GuildChannel,
) -> Result<(), Error> {
    ctx.data().interactions.command_event_start(&command_io(ctx)?, canale.id.get()).await?;
    Ok(())
}

/// Avvia l'overlay nel tuo canale vocale
#[poise::command(slash_command, rename = "collega", guild_only)]
pub async fn evento_collega(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_event_connect(&command_io(ctx)?).await?;
    Ok(())
}

/// Termina l'evento e disconnette il bot
#[poise::command(slash_command, rename = "termina", guild_only)]
pub async fn evento_termina(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_event_stop(&command_io(ctx)?).await?;
    Ok(())
}

/// Mostra lo stato dell'evento live
#[poise::command(slash_command, rename = "stato", guild_only)]
pub async fn evento_stato(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_event_status(&command_io(ctx)?).await?;
    Ok(())
}

/// Revoca il link overlay precedente e ne crea uno nuovo
#[poise::command(slash_command, rename = "rigenera-link", guild_only)]
pub async fn evento_rigenera_link(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_event_rotate_link(&command_io(ctx)?).await?;
    Ok(())
}

/// Genera un link OBS con il layout grafico scelto
#[poise::command(slash_command, rename = "link", guild_only)]
pub async fn evento_link(ctx: Context<'_>, #[description = "…"] tema: String) -> Result<(), Error> {
    ctx.data().interactions.command_event_theme_link(&command_io(ctx)?, &tema).await?;
    Ok(())
}

/// Abilita l'avvio automatico per un evento Discord
#[poise::command(slash_command, rename = "programma", guild_only)]
pub async fn evento_programma(
    ctx: Context<'_>,
    #[description = "Evento programmato da abilitare"]
    #[autocomplete = "scheduled_event_autocomplete"]
    evento: String,
) -> Result<(), Error> {
    ctx.data().interactions.command_event_schedule(&command_io(ctx)?, &evento).await?;
    Ok(())
}

/// Disabilita l'avvio automatico per un evento Discord
#[poise::command(slash_command, rename = "rimuovi-programma", guild_only)]
pub async fn evento_rimuovi_programma(
    ctx: Context<'_>,
    #[description = "ID dell'evento da disabilitare"]
    #[autocomplete = "opted_in_event_autocomplete"]
    evento_id: String,
) -> Result<(), Error> {
    ctx.data().interactions.command_event_unschedule(&command_io(ctx)?, &evento_id).await?;
    Ok(())
}

/// I layout dell'overlay come scelte di testo: poise ha solo scelte numeriche, ma il comando
/// di Python le manda come testo (`classic`, `podcast`…), e così si mantiene.
fn overlay_theme_option(builder: serenity::CreateCommandOption) -> serenity::CreateCommandOption {
    crate::live_event::OVERLAY_THEMES
        .iter()
        .fold(builder.kind(serenity::CommandOptionType::String), |builder, (value, label)| builder.add_string_choice(*label, *value))
}

/// `/evento` con le scelte di testo del layout.
fn evento_group() -> poise::Command<crate::Data, Error> {
    let mut command = evento();
    for subcommand in command.subcommands.iter_mut().filter(|subcommand| subcommand.name == "link") {
        for parameter in subcommand.parameters.iter_mut().filter(|parameter| parameter.name == "tema") {
            parameter.type_setter = Some(overlay_theme_option);
        }
    }
    command
}

// --- strumenti staff (cogs/staff_tools.py) -------------------------------------------------

/// Apri il sito SFO dello staff
#[poise::command(slash_command, guild_only)]
pub async fn site(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_site(&command_io(ctx)?).await?;
    Ok(())
}

/// Apri il pannello con i collegamenti utili per lo staff
#[poise::command(slash_command, guild_only)]
pub async fn general(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_general(&command_io(ctx)?).await?;
    Ok(())
}

// --- backup (cogs/backups.py) --------------------------------------------------------------

/// Backup del database
#[poise::command(slash_command, subcommands("backup_stato", "backup_crea"), subcommand_required, default_member_permissions = "ADMINISTRATOR")]
pub async fn backup(_ctx: Context<'_>) -> Result<(), Error> {
    Ok(())
}

/// Mostra lo stato dei backup
#[poise::command(slash_command, rename = "stato")]
pub async fn backup_stato(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_backup_status(&command_io(ctx)?).await?;
    Ok(())
}

/// Crea subito un backup SQLite
#[poise::command(slash_command, rename = "crea")]
pub async fn backup_crea(ctx: Context<'_>) -> Result<(), Error> {
    ctx.data().interactions.command_backup_create(&command_io(ctx)?).await?;
    Ok(())
}

/// Tutti i comandi di questo modulo: prima `cogs/core_commands.py`, poi `cogs/admin.py`.
pub fn all() -> Vec<poise::Command<crate::Data, Error>> {
    vec![
        reset_manuale(),
        sync(),
        suggerisci(),
        scegli(),
        rimuovi_scelta(),
        unisci_proposte(),
        staff(),
        storico(),
        config(),
        feed_attivita(),
        centro_staff(),
        proposte(),
        proposta(),
        sondaggio(),
        opzione(),
        guida(),
        profilo(),
        classifica_utenti(),
        record_community(),
        notifiche(),
        classifica_stagione(),
        backup(),
        site(),
        general(),
        evento_group(),
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    /// Un'opzione (o sottocomando) nella forma comune a poise e `discord.py`.
    ///
    /// poise scrive liste vuote dove discord.py omette il campo, e i limiti come numeri decimali
    /// (3.0); per Discord è lo stesso, qui si confrontano i valori. Gli interi senza limiti li
    /// scrive col massimo che Discord accetta (±2^53−1): equivale a un `int` di discord.py.
    fn normalize(option: &Value) -> Value {
        let value = |key: &str| match option.get(key) {
            None => Value::Null,
            Some(Value::Array(items)) if items.is_empty() => Value::Null,
            Some(Value::Number(n)) if n.as_f64().is_some_and(|v| v.abs() == 9_007_199_254_740_991.0) => Value::Null,
            Some(Value::Number(n)) if n.is_f64() && n.as_f64().unwrap().fract() == 0.0 => json!(n.as_f64().unwrap() as i64),
            Some(other) => other.clone(),
        };
        let mut out = json!({
            "name": option["name"], "description": option["description"], "type": option["type"],
            "required": option.get("required").cloned().unwrap_or(json!(false)),
            "min_length": value("min_length"), "max_length": value("max_length"),
            "autocomplete": option.get("autocomplete").filter(|v| v.as_bool() == Some(true)).cloned().unwrap_or(Value::Null),
            "min_value": value("min_value"), "max_value": value("max_value"),
            "channel_types": value("channel_types"),
            "choices": match value("choices") {
                Value::Array(choices) => Value::Array(choices.iter().map(|c| json!({"name": c["name"], "value": c["value"]})).collect()),
                other => other,
            },
        });
        if let Some(nested) = option.get("options").and_then(Value::as_array).filter(|items| !items.is_empty()) {
            out["options"] = Value::Array(nested.iter().map(normalize).collect());
        }
        out
    }

    /// Confronta le definizioni dei comandi `names` con il payload di `discord.py` (`golden`):
    /// nome, descrizione, opzioni e sottocomandi, permessi e restrizione al server. Del golden si
    /// guardano solo i comandi indicati.
    fn assert_payload_matches(names: &[&str], golden: &str) {
        let expected: Vec<Value> =
            serde_json::from_str::<Vec<Value>>(golden).unwrap().into_iter().filter(|c| names.contains(&c["name"].as_str().unwrap())).collect();
        let all = serde_json::to_value(poise::builtins::create_application_commands(&super::all())).unwrap();
        assert_eq!(expected.len(), names.len(), "il golden contiene tutti i comandi richiesti");
        for wanted in &expected {
            let name = wanted["name"].as_str().unwrap();
            let command = all.as_array().unwrap().iter().find(|c| c["name"] == name).unwrap_or_else(|| panic!("manca /{name}"));
            assert_eq!(command["description"], wanted["description"], "{name}: descrizione");
            let options: Vec<Value> = command.get("options").and_then(Value::as_array).map(|options| options.iter().map(normalize).collect()).unwrap_or_default();
            let wanted_options: Vec<Value> = wanted["options"]
                .as_array()
                .unwrap()
                .iter()
                .map(|option| {
                    // Come `normalize`, ma dal golden: campi assenti = Null, `required` sempre presente.
                    fn complete(option: &Value) -> Value {
                        let mut option = option.clone();
                        option["required"] = json!(option["required"].as_bool().unwrap_or(false));
                        for key in ["min_length", "max_length", "autocomplete", "min_value", "max_value", "channel_types", "choices"] {
                            if option.get(key).is_none() {
                                option[key] = Value::Null;
                            }
                        }
                        if let Some(nested) = option.get("options").and_then(Value::as_array).cloned() {
                            option["options"] = Value::Array(nested.iter().map(complete).collect());
                        }
                        option
                    }
                    complete(option)
                })
                .collect();
            assert_eq!(options, wanted_options, "{name}: opzioni");
            let permissions = command.get("default_member_permissions").and_then(Value::as_str).map(|p| p.parse::<u64>().unwrap());
            assert_eq!(permissions.map_or(Value::Null, |p| json!(p)), wanted["default_member_permissions"], "{name}: permessi");
            let guild_only = command.get("dm_permission") == Some(&json!(false)) || command.get("contexts") == Some(&json!([0]));
            assert_eq!(json!(guild_only), wanted["guild_only"], "{name}: solo nel server");
        }
    }

    #[test]
    fn core_command_definitions_match_the_python_payload() {
        assert_payload_matches(
            &["reset-manuale", "sync", "suggerisci", "scegli", "rimuovi-scelta", "unisci-proposte", "staff", "storico"],
            include_str!("golden/commands_core_expected.json"),
        );
    }

    #[test]
    fn admin_command_definitions_match_the_python_payload() {
        assert_payload_matches(&["config", "feed-attivita", "centro-staff"], include_str!("golden/commands_admin_expected.json"));
    }

    #[test]
    fn proposal_command_definitions_match_the_python_payload() {
        assert_payload_matches(&["proposte"], include_str!("golden/commands_proposals_list_expected.json"));
        assert_payload_matches(&["proposta"], include_str!("golden/commands_proposals_review_expected.json"));
    }

    #[test]
    fn live_event_command_definitions_match_the_python_payload() {
        assert_payload_matches(&["evento"], include_str!("golden/commands_live_expected.json"));
    }

    #[test]
    fn staff_tools_command_definitions_match_the_python_payload() {
        assert_payload_matches(&["site", "general"], include_str!("golden/commands_staff_tools_expected.json"));
    }

    #[test]
    fn backup_command_definitions_match_the_python_payload() {
        assert_payload_matches(&["backup"], include_str!("golden/commands_backups_expected.json"));
    }

    #[test]
    fn community_command_definitions_match_the_python_payload() {
        assert_payload_matches(&["guida", "profilo", "classifica-utenti", "record-community", "notifiche"], include_str!("golden/commands_community_expected.json"));
        assert_payload_matches(&["classifica-stagione"], include_str!("golden/commands_seasons_expected.json"));
    }

    #[test]
    fn poll_command_definitions_match_the_python_payload() {
        assert_payload_matches(&["sondaggio"], include_str!("golden/commands_polls_expected.json"));
        assert_payload_matches(&["opzione"], include_str!("golden/commands_options_expected.json"));
    }
}
