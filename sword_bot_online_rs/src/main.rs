//! Sword Bot Online — avvio del bot.
//!
//! Per i benvenuti automatici abilita anche Server Members Intent nel Developer Portal.

mod autocomplete;
mod backups;
mod live_event;
mod live_service;
mod overlay;
mod overlay_web;
mod cards;
mod community;
mod commands;
mod config;
mod control_center;
mod core;
mod database;
mod discord_api;
mod embeds;
mod flows;
mod formatting;
mod gamenight;
mod health;
mod interactions;
mod hub;
mod hooks;
mod notify;
mod permissions;
mod polls;
mod public_api;
mod publisher;
mod queries;
mod reset;
mod review;
mod schedule;
mod seasonal;
mod site_admin;
mod site_auth;
mod site_backend;
mod site_common;
mod site_events;
mod site_reports;
mod site_static;
mod site_stats;
mod site_tools;
mod platform_data;
mod pyiso;
mod settings;
mod stats;
mod suggestions;
mod ui;
mod v2;
mod voice;
mod web;
mod welcome;

use std::sync::Arc;
use std::time::Instant;

use poise::serenity_prelude as serenity;
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

use crate::config::{ConfigurationError, load_environment};
use crate::database::Database;
use crate::discord_api::HttpDiscordApi;
use crate::flows::Services;
use crate::health::{BotStatus, HealthState};
use crate::interactions::Interactions;
use crate::notify::{HttpSender, Notifier};
use crate::polls::RefreshScheduler;
use crate::publisher::Publisher;
use crate::reset::ResetService;
use crate::web::HttpConfig;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

/// Il gestore dei pulsanti con i servizi reali (API HTTP di Discord).
type AppInteractions = Interactions<HttpSender, HttpDiscordApi>;

/// L'evento live con i servizi reali.
type AppLive = live_service::LiveService<HttpSender, HttpDiscordApi, voice::SongbirdGateway>;

/// Stato condiviso tra i comandi (in Python: gli attributi di `SwordBot`).
pub struct Data {
    pub guild_id: Option<serenity::GuildId>,
    pub db: Database,
    /// Smista i clic su pulsanti, menu e maschere (i pulsanti persistenti di Python).
    pub interactions: Arc<AppInteractions>,
    /// L'evento live: presenze, eventi programmati, ciclo dell'overlay.
    pub live: Arc<AppLive>,
}

/// Eventi del gateway oltre ai comandi: qui arrivano i clic sui componenti e le maschere inviate.
async fn event_handler(framework: poise::FrameworkContext<'_, Data, Error>, event: &serenity::FullEvent) -> Result<(), Error> {
    match event {
        serenity::FullEvent::InteractionCreate { interaction } => {
            interactions::dispatch_event(framework.serenity_context, interaction, &framework.user_data.interactions).await;
        }
        // Presenze dell'evento live (`on_voice_state_update`).
        serenity::FullEvent::VoiceStateUpdate { old, new } => {
            let Some(guild_id) = new.guild_id else {
                return Ok(());
            };
            let is_bot = new
                .member
                .as_ref()
                .map(|member| member.user.bot)
                .or_else(|| framework.serenity_context.cache.guild(guild_id).and_then(|guild| guild.members.get(&new.user_id).map(|member| member.user.bot)))
                .unwrap_or(false);
            let before = old.as_ref().and_then(|state| state.channel_id).map(serenity::ChannelId::get);
            framework.user_data.live.on_voice_state_update(guild_id.get(), new.user_id.get(), is_bot, before, new.channel_id.map(serenity::ChannelId::get));
        }
        // Eventi programmati: avvio e fine automatici dell'evento live.
        serenity::FullEvent::GuildScheduledEventCreate { event } => {
            let info = discord_api::scheduled_event_info(event.clone());
            framework.user_data.live.note_event_status(info.id, info.status);
        }
        serenity::FullEvent::GuildScheduledEventUpdate { event } => {
            let info = discord_api::scheduled_event_info(event.clone());
            framework.user_data.live.on_scheduled_event_update(event.guild_id.get(), &info).await;
        }
        serenity::FullEvent::GuildScheduledEventDelete { event } => {
            framework.user_data.live.on_scheduled_event_delete(event.id.get()).await;
        }
        // Come `on_ready`: gli eventi programmati già attivi e abilitati partono subito.
        serenity::FullEvent::CacheReady { guilds } => {
            let guilds: Vec<u64> = guilds.iter().map(|guild| guild.get()).collect();
            framework.user_data.live.on_ready(&guilds).await;
        }
        // Benvenuto ai nuovi membri (`on_member_join` di Python).
        serenity::FullEvent::GuildMemberAddition { new_member } => {
            let (guild_name, member_count) = framework
                .serenity_context
                .cache
                .guild(new_member.guild_id)
                .map(|guild| (guild.name.clone(), guild.member_count))
                .unwrap_or_default();
            let member = community::JoinedMember {
                id: new_member.user.id.get(),
                display_name: new_member.display_name().to_string(),
                avatar_url: new_member.face(),
                guild_id: new_member.guild_id.get(),
                guild_name,
                member_count,
            };
            let data = framework.user_data;
            community::welcome_member(&data.db, data.interactions.services.publisher.api(), &member).await;
        }
        _ => {}
    }
    Ok(())
}

/// Sincronizza i comandi slash solo se sono cambiati dall'ultima volta.
///
/// Discord limita la frequenza delle sincronizzazioni: farla a ogni avvio è inutile e, con
/// riavvii frequenti, porta al rate limit. L'hash dell'ultimo payload sincronizzato è nella
/// tabella `settings` (chiave `command_sync_hash:<scope>`).
async fn sync_commands(
    ctx: &serenity::Context,
    commands: &[poise::Command<Data, Error>],
    guild_id: Option<serenity::GuildId>,
    db: &Database,
    force: bool,
) -> Result<bool, Error> {
    // `serde_json::Value` ordina le chiavi: payload stabile, come `sort_keys=True` in Python.
    let payload = serde_json::to_value(poise::builtins::create_application_commands(commands))?;
    let digest = Sha256::digest(serde_json::to_string(&payload)?.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let scope = match guild_id {
        Some(guild) => format!("guild:{guild}"),
        None => "global".to_string(),
    };
    let setting_key = format!("command_sync_hash:{scope}");
    if !force && db.get_setting(&setting_key)?.as_deref() == Some(digest.as_str()) {
        info!("Comandi slash invariati ({scope}): sincronizzazione saltata.");
        return Ok(false);
    }
    match guild_id {
        Some(guild) => poise::builtins::register_in_guild(ctx, commands, guild).await?,
        None => poise::builtins::register_globally(ctx, commands).await?,
    }
    db.set_setting(&setting_key, digest)?;
    info!("Comandi slash sincronizzati ({scope}).");
    Ok(true)
}

/// Gestore globale degli errori (equivalente di `tree.error` / `on_app_command_error`).
async fn on_error(err: poise::FrameworkError<'_, Data, Error>) {
    let (title, description, ctx) = match err {
        poise::FrameworkError::CommandCheckFailed { ctx, .. } => (
            "Permesso negato",
            "Non hai i permessi per usare questo comando.",
            ctx,
        ),
        poise::FrameworkError::Command { error, ctx, .. } => {
            error!(command = %ctx.command().name, "Errore durante l'esecuzione di un comando Discord: {error}");
            (
                "Errore durante il comando",
                "🎲 Ops, il server ha fallito il tiro salvezza. Riprova tra poco!",
                ctx,
            )
        }
        // Tutti gli altri errori (argomenti, cooldown, setup...) hanno la gestione predefinita.
        other => {
            if let Err(e) = poise::builtins::on_error(other).await {
                error!("Errore nella gestione dell'errore: {e}");
            }
            return;
        }
    };

    let reply = poise::CreateReply::default()
        .embed(embeds::error(title, description))
        .ephemeral(true);
    // `ctx.send` usa da solo `followup` se l'interazione ha già risposto.
    if let Err(e) = ctx.send(reply).await {
        // Interazione scaduta o già chiusa: l'errore originale è già nei log.
        warn!("Impossibile notificare l'errore all'utente: interazione non più valida ({e}).");
    }
}

#[tokio::main]
async fn main() {
    // Approssima l'avvio del processo (base di `uptime_seconds` in /health).
    let started_at = Instant::now();
    // Livello predefinito INFO, come `logging.info` del bot Python.
    tracing_subscriber::fmt().init();

    let environment = match load_environment() {
        Ok(env) => env,
        Err(ConfigurationError(message)) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };
    let guild_id = environment.guild_id.map(serenity::GuildId::new);

    // Apre il database, applica le migrazioni e inizializza i dati (equivalente di `core.initialize`).
    let db = match Database::open(&environment.db_path).and_then(|db| {
        core::initialize(&db, schedule::now())?;
        Ok(db)
    }) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Impossibile aprire il database '{}': {e}", environment.db_path);
            std::process::exit(1);
        }
    };

    // Server HTTP: /health e overlay; le altre route (API, sito staff) si uniranno
    // allo stesso `Router` man mano che le porteremo.
    let bot_status = BotStatus::new();
    let http_config = HttpConfig::from_env();
    // Dominio pubblico dell'overlay e sessione live: servono alle route web e, dopo l'avvio del
    // gateway, ai comandi `/site`, `/general` ed `/evento`.
    let live = Arc::new(overlay::LiveState::new(overlay::PublicUrl::from_env()));
    let overlay_state = overlay_web::OverlayState::new(live.clone(), db.clone());
    // Sito staff: accesso con Discord (la directory del server arriva a bot avviato) e file del sito.
    let site_directory = Arc::new(site_auth::LateDirectory::default());
    let site_auth = Arc::new(site_auth::SiteAuth::new(
        db.clone(),
        site_directory.clone(),
        Arc::new(site_auth::DiscordOAuth::new()),
        site_auth::AuthConfig::from_env(),
        environment.guild_id,
    ));
    // Statistiche del sito (con la loro cache) e aggiornamenti in tempo reale: entrambi si svuotano o
    // si avvisano quando il bot segnala che i dati sono cambiati.
    let platform_data = Arc::new(platform_data::PlatformData::new(db.clone(), site_directory.clone(), environment.guild_id));
    let site_events = site_events::SiteEventHub::new(site_auth.clone(), site_events::HubConfig::default());
    {
        let data = platform_data.clone();
        hooks::add_site_change_listener(Arc::new(move |_topic| data.clear_read_cache()));
        let hub = site_events.clone();
        hooks::add_site_change_listener(Arc::new(move |topic| hub.publish(topic)));
    }
    // Report degli eventi (Supabase) e strumenti staff del sito: parlano con Discord e con i servizi del
    // bot tramite un backend che si collega a bot avviato.
    let site_backend = Arc::new(site_backend::LateBackend::default());
    let late_live = Arc::new(live_service::LateLive::default());
    let backup_service = Arc::new(backups::BackupService::new(db.clone()));
    let supabase_url = std::env::var("SUPABASE_URL").unwrap_or_default().trim_end_matches('/').to_string();
    let supabase_key = std::env::var("SUPABASE_SERVICE_ROLE_KEY").unwrap_or_default();
    let reports_state = site_reports::ReportsState {
        auth: site_auth.clone(),
        db: db.clone(),
        backend: site_backend.clone(),
        store: Arc::new(site_reports::SupabaseStore::new(&supabase_url, &supabase_key)),
        supabase_url,
        service_key: supabase_key,
        fallback_guild_id: environment.guild_id,
        send_lock: Arc::new(tokio::sync::Mutex::new(())),
        clock: site_reports::rome_now,
    };
    let tools_state = site_tools::ToolsState {
        auth: site_auth.clone(),
        db: db.clone(),
        backend: site_backend.clone(),
        directory: site_directory.clone(),
        live_state: live.clone(),
        live: late_live.clone(),
        backups: backup_service.clone(),
        clock: schedule::now,
    };
    let late_admin_ops = Arc::new(site_admin::LateAdminOps::default());
    let admin_state = site_admin::AdminState {
        auth: site_auth.clone(),
        db: db.clone(),
        backend: site_backend.clone(),
        ops: late_admin_ops.clone(),
        directory: site_directory.clone(),
        clock: schedule::now,
        preview_delay: std::time::Duration::from_secs(site_admin::PREVIEW_SECONDS),
    };
    let router = health::routes(HealthState::new(bot_status.clone(), db.clone(), started_at))
        .merge(overlay_web::routes(overlay_state))
        .merge(site_auth.routes())
        .merge(site_stats::routes(site_stats::StatsState { auth: site_auth.clone(), data: platform_data.clone() }))
        .merge(public_api::routes(public_api::PublicApiState::new(
            db.clone(),
            platform_data,
            &std::env::var("PUBLIC_API_TOKEN").unwrap_or_default(),
            schedule::now,
        )))
        .merge(site_events.routes())
        .merge(site_reports::routes(reports_state))
        .merge(site_tools::routes(tools_state))
        .merge(site_admin::routes(admin_state))
        .merge(site_static::routes(http_config.site_dir.clone()));
    let http_server = web::serve(&http_config, router).await;
    // Come in Python, l'evento live parte (e lo stato si pubblica) solo se il server HTTP è in ascolto.
    let overlay_running = http_server.is_some();
    // Il servizio dell'evento live nasce all'avvio del gateway; qui serve per chiuderlo allo spegnimento.
    let live_slot: Arc<std::sync::OnceLock<Arc<AppLive>>> = Arc::default();
    let setup_live_slot = live_slot.clone();
    // Il gestore delle connessioni vocali (songbird): riceve gli eventi vocali del gateway Discord e
    // decodifica l'audio per misurare chi parla.
    let voice_manager = songbird::Songbird::serenity_from_config(voice::SongbirdGateway::config());
    let setup_voice = voice_manager.clone();
    let setup_site_directory = site_directory.clone();
    let setup_site_backend = site_backend.clone();
    let setup_late_live = late_live.clone();
    let setup_admin_ops = late_admin_ops.clone();
    let setup_backups = backup_service.clone();

    // `Intents.default()` + `members` (privilegiato: va abilitato nel Developer Portal).
    let intents = serenity::GatewayIntents::non_privileged() | serenity::GatewayIntents::GUILD_MEMBERS;

    let setup_status = bot_status.clone();
    let setup_live = live.clone();
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: commands::all(),
            // Come `commands.when_mentioned`: nessun prefisso testuale, solo la menzione del bot.
            prefix_options: poise::PrefixFrameworkOptions {
                prefix: None,
                mention_as_prefix: true,
                ..Default::default()
            },
            // `AllowedMentions.none()`
            allowed_mentions: Some(serenity::CreateAllowedMentions::new()),
            on_error: |err| Box::pin(on_error(err)),
            event_handler: |framework, event| Box::pin(event_handler(framework, event)),
            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                info!("Online come {}", ready.user.name);
                setup_status.mark_ready(framework.shard_manager().clone());
                sync_commands(ctx, &framework.options().commands, guild_id, &db, false).await?;

                // Servizi che parlano con Discord: invio/modifica dei messaggi, notifiche, aggiornamenti.
                let api = HttpDiscordApi::new(ctx.http.clone(), ready.user.id.get()).with_cache(ctx.cache.clone());
                let publisher = Arc::new(Publisher::new(api, db.clone()));
                let notifier = Notifier::new(db.clone(), HttpSender::new(ctx.http.clone()));
                let live_notifier = notifier.clone();
                let backend_notifier = notifier.clone();
                let scheduler = RefreshScheduler::new();
                let backend_scheduler = scheduler.clone();
                let services = Services { db: db.clone(), notifier: notifier.clone(), publisher: publisher.clone(), scheduler: scheduler.clone() };

                // Reset settimanale e promemoria: due cicli (ogni minuto e ogni 5 minuti) come
                // `reset_loop` e `reminder_loop` di Python. Il primo passo parte subito.
                let reset_service = Arc::new(ResetService::new(
                    Services { db: db.clone(), notifier, publisher: publisher.clone(), scheduler },
                    guild_id.map(|id| id.get()),
                ));
                reset_service.start_loops();

                // Chiusura automatica dei sondaggi scaduti, ogni minuto (`_expire_polls` di Python).
                // Un errore si registra e il ciclo continua: il passo successivo riprova.
                let expiry = publisher.clone();
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        ticker.tick().await;
                        if let Err(e) = expiry.close_expired_polls().await {
                            error!("Chiusura automatica dei sondaggi non riuscita; riprovo tra un minuto: {e}");
                        }
                    }
                });
                let interactions = Arc::new(Interactions::new(services));
                interactions.set_reset_service(reset_service);

                // Backup SQLite (e copia cifrata su R2, se configurata) ogni giorno alle 03:15 di Roma.
                let backups = setup_backups;
                backups.start_daily_loop();
                interactions.set_backup_service(backups);

                // Dominio pubblico (`EVENT_OVERLAY_PUBLIC_URL`) e sessione live, per `/site` e `/general`.
                // Un evento rimasto aperto per un riavvio si chiude con le presenze fino all'ultimo istante visto.
                if let Err(e) = live_event::recover_interrupted_events(&db) {
                    error!("Impossibile chiudere gli eventi live interrotti: {e}");
                }
                if let Some(problem) = &setup_live.public_url.configuration_error {
                    warn!("{problem}");
                }
                interactions.set_live_state(setup_live.clone());

                // Il sito staff legge server e membri dalla cache del gateway (o da Discord).
                setup_site_directory.set(Arc::new(site_auth::DiscordDirectory::new(ctx.cache.clone(), ctx.http.clone())));

                // L'evento live: avvio e fine, presenze, ciclo dell'overlay e collegamento vocale.
                let gateway = voice::SongbirdGateway::new(ctx.cache.clone(), setup_voice, ready.user.id.get());
                let live_service = Arc::new(live_service::LiveService::new(db.clone(), live_notifier, publisher.clone(), gateway, setup_live, overlay_running));
                interactions.set_live_controller(live_service.clone());
                setup_late_live.set(live_service.clone());
                let site_ops = Arc::new(site_backend::DiscordBackend {
                    db: db.clone(),
                    notifier: backend_notifier,
                    publisher: publisher.clone(),
                    scheduler: backend_scheduler,
                });
                setup_admin_ops.set(site_ops.clone());
                setup_site_backend.set(site_ops);
                let _ = setup_live_slot.set(live_service.clone());
                if overlay_running {
                    tokio::spawn(live_service.clone().run_loop());
                }

                // Come `on_ready` di Python: all'avvio rinfresca sondaggi e dashboard (se il canale
                // dei sondaggi è configurato), così i pulsanti dei messaggi esistenti sono allineati.
                tokio::spawn(async move {
                    if let Err(e) = publisher.refresh_all().await {
                        error!("Aggiornamento iniziale dei messaggi non riuscito: {e}");
                    }
                });
                Ok(Data { guild_id, db, interactions, live: live_service })
            })
        })
        .build();

    let mut client = match serenity::ClientBuilder::new(&environment.token, intents)
        .voice_manager_arc(voice_manager)
        .framework(framework)
        .await
    {
        Ok(client) => client,
        Err(e) => {
            eprintln!("Impossibile creare il client Discord: {e}");
            std::process::exit(1);
        }
    };

    // Ctrl+C ferma il bot come `KeyboardInterrupt` in Python: gli shard si chiudono e l'evento in
    // corso viene concluso con il suo report.
    let shard_manager = client.shard_manager.clone();
    let result = tokio::select! {
        result = client.start() => result,
        _ = tokio::signal::ctrl_c() => {
            info!("Arresto richiesto (Ctrl+C).");
            shard_manager.shutdown_all().await;
            Ok(())
        }
    };
    if let Some(service) = live_slot.get() {
        service.finish_event("arresto_bot").await;
    }
    live.hub.close_all(1001, "Bot in arresto");
    site_events.shutdown();
    // Il tempo di mandare le chiusure agli overlay collegati.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // Da qui `/health` risponde 503 (equivalente di `bot.is_closed()`).
    bot_status.mark_closed();
    if let Err(e) = result {
        error!("Il client Discord si è fermato: {e}");
        std::process::exit(1);
    }
}
