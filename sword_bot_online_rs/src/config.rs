//! Costanti del bot e configurazione letta dalle variabili d'ambiente
//! (equivalente di `sfo/config.py`).
// Le costanti servono ai moduli che porteremo nelle prossime iterazioni.
#![allow(dead_code)]

use std::env;
use std::fmt;

use chrono_tz::Tz;

/// Fuso orario del bot: reset, scadenze e stagioni sono tutti in ora italiana.
pub const TZ: Tz = chrono_tz::Europe::Rome;

// Valori predefiniti: i limiti principali si possono cambiare da /config
// (vedi `settings`), queste costanti restano il valore di partenza.
pub const MAX_VOTES: i64 = 2; // voti per utente per sondaggio
pub const MAX_PROPOSALS: i64 = 3; // proposte a settimana per utente
pub const COOLDOWN_SECONDS: i64 = 30; // pausa tra una proposta e l'altra
pub const DEFAULT_SLOTS: i64 = 5; // proposte nel ballottaggio community
pub const DEFAULT_RESET_WEEKDAY: i64 = 0; // lunedì
pub const DEFAULT_RESET_HOUR: i64 = 12;

pub const BAR_LEN: usize = 10; // lunghezza barra di avanzamento
pub const PAGE_SIZE: usize = 10; // proposte per pagina nell'elenco staff
pub const HISTORY_RETENTION: usize = 200; // settimane di storico da conservare
pub const DEFAULT_TIPI: &str = "Gioco,Film,Karaoke,Altro";
pub const REFRESH_DEBOUNCE_SECONDS: f64 = 2.0;
pub const SIMILAR_SUGGESTION_THRESHOLD: f64 = 0.72;

pub const COLOR_PRIMARY: u32 = 0x5865F2;
pub const COLOR_PRESTIGE: u32 = 0xE67E22;
pub const COLOR_STAFF: u32 = 0x9B59B6;
pub const PROPOSAL_TYPE_EMOJIS: [(&str, &str); 5] = [
    ("gioco", "🎮"),
    ("giochi", "🎮"),
    ("film", "🎬"),
    ("karaoke", "🎤"),
    ("altro", "✨"),
];
pub const CONTRIBUTOR_LEVELS: [(u32, &str); 6] = [
    (0, "🌱 Nuovo"),
    (10, "🥉 Bronzo"),
    (25, "🥈 Argento"),
    (50, "🥇 Oro"),
    (100, "💎 Platino"),
    (200, "👑 Leggenda"),
];
pub const HIDDEN_BADGES: [(&str, &str); 3] = [
    ("night_owl", "🌙 Nottataccia"),
    ("indecisive", "🔄 Indeciso"),
    ("influencer", "📣 Influencer"),
];
pub const HIDDEN_BADGE_HINTS: [(&str, &str); 3] = [
    ("night_owl", "Le idee migliori arrivano quando tutti dormono…"),
    ("indecisive", "Cambiare idea è lecito. Cambiarla spesso, ancora di più."),
    ("influencer", "La costanza nel ballottaggio non passa inosservata."),
];
pub const AUTHOR_STAT_POINTS: [(&str, i64); 3] = [
    ("supports_received", 2),
    ("ballot_entries", 5),
    ("winning_proposals", 10),
];

/// Configurazione d'ambiente mancante o non valida.
#[derive(Debug)]
pub struct ConfigurationError(pub String);

impl fmt::Display for ConfigurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigurationError {}

#[derive(Debug, Clone)]
pub struct EnvironmentConfig {
    pub token: String,
    pub guild_id: Option<u64>,
    pub db_path: String,
}

fn var(name: &str) -> String {
    env::var(name).unwrap_or_default().trim().to_string()
}

pub fn database_path() -> String {
    env::var("DB_PATH").unwrap_or_else(|_| "sword_bot.db".to_string())
}

fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_digit())
}

/// Legge e valida le variabili d'ambiente necessarie all'avvio del bot.
pub fn load_environment() -> Result<EnvironmentConfig, ConfigurationError> {
    let mut problems: Vec<String> = Vec::new();

    let token = var("DISCORD_TOKEN");
    if token.is_empty() {
        problems.push("DISCORD_TOKEN è obbligatoria (token del bot dal Developer Portal).".into());
    }

    let raw_guild = var("GUILD_ID");
    let mut guild_id = None;
    if !raw_guild.is_empty() {
        match raw_guild.parse::<u64>() {
            Ok(id) if is_digits(&raw_guild) => guild_id = Some(id),
            _ => problems.push("GUILD_ID deve essere l'ID numerico del server.".into()),
        }
    }

    let public_url = var("EVENT_OVERLAY_PUBLIC_URL");
    if !public_url.is_empty() && !public_url.starts_with("https://") {
        problems.push("EVENT_OVERLAY_PUBLIC_URL deve iniziare con https://.".into());
    }

    let raw_port = var("EVENT_OVERLAY_PORT");
    if !raw_port.is_empty() {
        let valid = is_digits(&raw_port) && raw_port.parse::<u32>().is_ok_and(|p| (1..65536).contains(&p));
        if !valid {
            problems.push("EVENT_OVERLAY_PORT deve essere una porta tra 1 e 65535.".into());
        }
    }

    let oauth = [var("DISCORD_OAUTH_CLIENT_ID"), var("DISCORD_OAUTH_CLIENT_SECRET")];
    if oauth.iter().any(|v| !v.is_empty()) && !oauth.iter().all(|v| !v.is_empty()) {
        problems.push(
            "Per il login web servono sia DISCORD_OAUTH_CLIENT_ID sia DISCORD_OAUTH_CLIENT_SECRET."
                .into(),
        );
    }

    let supabase = [var("SUPABASE_URL"), var("SUPABASE_SERVICE_ROLE_KEY")];
    if supabase.iter().any(|v| !v.is_empty()) && !supabase.iter().all(|v| !v.is_empty()) {
        problems.push("Per i report servono sia SUPABASE_URL sia SUPABASE_SERVICE_ROLE_KEY.".into());
    }

    if !problems.is_empty() {
        return Err(ConfigurationError(format!(
            "Configurazione non valida:\n- {}",
            problems.join("\n- ")
        )));
    }

    Ok(EnvironmentConfig {
        token,
        guild_id,
        db_path: database_path(),
    })
}
