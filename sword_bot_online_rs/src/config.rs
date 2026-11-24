//! Costanti del bot e configurazione letta dalle variabili d'ambiente
//! (equivalente di `sfo/config.py`).
//!
//! Questo file contiene TUTTE le costanti di configurazione usate dal bot.
//! Le costanti sono valori che non cambiano durante l'esecuzione del programma.
//! Sono scritte in MAIUSCOLO per convenzione (così si riconoscono subito).
//!
//! In Rust, a differenza di Python, le costanti devono avere un tipo esplicito
//! (es. `i64`, `usize`, `&str`, `u32`) e vengono valutate in fase di compilazione,
//! non a runtime. Questo le rende molto veloci.
//!
//! Il modulo usa `std::env` per leggere le variabili d'ambiente (come `DISCORD_TOKEN`,
//! `GUILD_ID`, ecc.) che vengono impostate nel sistema operativo o in un file `.env`.
//! Le variabili d'ambiente servono a tenere i segreti (token, password) fuori dal codice sorgente.

// Le costanti servono ai moduli che porteremo nelle prossime iterazioni.
// `dead_code` permette di avere costanti non usate ancora senza warning del compilatore.
#![allow(dead_code)]

use std::env;
use std::fmt;

use chrono_tz::Tz;

/// Fuso orario del bot: reset, scadenze e stagioni sono tutti in ora italiana.
///
/// `chrono_tz::Tz` è un tipo che rappresenta un fuso orario con tutte le sue regole
/// (ora legale, cambi storici, ecc.). `Europe/Rome` gestisce automaticamente l'ora legale italiana.
/// In Rust i tipi vengono importati con `use` (qui `chrono_tz::Tz`).
pub const TZ: Tz = chrono_tz::Europe::Rome;

// ============================================================================
// VALORI PREDEFINITI (DEFAULT) - Limiti e configurazioni di base
// ============================================================================
// Questi valori si possono cambiare tramite il comando `/config` del bot (vedi modulo `settings`).
// Le costanti qui sotto sono solo i valori INIZIALI usati al primo avvio o se non c'è configurazione salvata.

/// Massimo numero di voti che un singolo utente può esprimere in un sondaggio.
/// `i64` = intero a 64 bit con segno (da -9 quintilioni a +9 quintilioni).
pub const MAX_VOTES: i64 = 2;

/// Massimo numero di proposte che un utente può fare a settimana.
pub const MAX_PROPOSALS: i64 = 3;

/// Secondi di attesa obbligatori tra una proposta e la successiva dello stesso utente.
/// Serve a evitare spam (anti-flood).
pub const COOLDOWN_SECONDS: i64 = 30;

/// Quante proposte entrano nel "ballottaggio community" (il sondaggio finale della settimana).
/// Il ballottaggio è la votazione finale dove la community sceglie tra le proposte più votate.
pub const DEFAULT_SLOTS: i64 = 5;

/// Giorno della settimana per il reset settimanale (0 = lunedì, 6 = domenica).
/// In Rust `chrono` usa 0-6 per lunedì-domenica (diverso da JavaScript che usa 0-6 per domenica-sabato).
pub const DEFAULT_RESET_WEEKDAY: i64 = 0;

/// Ora del reset (in ora italiana, formato 24h). 12 = mezzogiorno.
pub const DEFAULT_RESET_HOUR: i64 = 12;

// ============================================================================
// COSTANTI DI INTERFACCIA UTENTE (UI)
// ============================================================================

/// Lunghezza della barra di avanzamento visuale (in caratteri).
/// `usize` = intero senza segno della dimensione del puntatore (64 bit su sistemi a 64 bit).
/// Si usa per indici di array, lunghezze, ecc. Non può essere negativo.
pub const BAR_LEN: usize = 10;

/// Quante proposte mostrare per pagina nell'elenco staff (paginazione).
pub const PAGE_SIZE: usize = 10;

/// Quante settimane di storico mantenere nel database prima di pulire.
pub const HISTORY_RETENTION: usize = 200;

/// Tipi di proposta predefiniti, separati da virgola.
/// `&str` = "string slice", un riferimento a una stringa immutabile (prestito, non possesso).
/// È diverso da `String` che è una stringa posseduta, modificabile, allocata sull'heap.
pub const DEFAULT_TIPI: &str = "Gioco,Film,Karaoke,Altro";

/// Secondi di debounce per l'aggiornamento automatico delle schermate (evita troppi refresh).
/// `f64` = numero a virgola mobile a 64 bit (doppia precisione).
pub const REFRESH_DEBOUNCE_SECONDS: f64 = 2.0;

/// Soglia di similarità (0.0 - 1.0) per suggerire proposte simili ed evitare duplicati.
/// 0.72 = 72% simile. Usa algoritmi di confronto stringhe (es. Levenshtein/Jaro-Winkler).
pub const SIMILAR_SUGGESTION_THRESHOLD: f64 = 0.72;

// ============================================================================
// COLORI DISCORD (formato esadecimale 0xRRGGBB)
// ============================================================================
// Discord usa colori a 24 bit (RGB). In Rust `u32` = intero senza segno a 32 bit.
// L'0x all'inizio indica notazione esadecimale (base 16).

/// Colore principale del bot (blurple Discord).
pub const COLOR_PRIMARY: u32 = 0x5865F2;

/// Colore per oggetti "prestigio" / speciali (arancione).
pub const COLOR_PRESTIGE: u32 = 0xE67E22;

/// Colore per messaggi staff / moderazione (viola).
pub const COLOR_STAFF: u32 = 0x9B59B6;

// ============================================================================
// EMOJI PER TIPI DI PROPOSTA
// ============================================================================
// Array di tuple: ogni elemento è `(&str, &str)` = coppia di riferimenti a stringhe.
// La prima è il tipo (minuscolo), la seconda l'emoji.
// Le tuple sono collezioni di dimensione fissa con tipi diversi per ogni posizione.

/// Associa ogni tipo di proposta alla sua emoji rappresentativa.
/// Supporta anche varianti plurali ("giochi" oltre a "gioco").
pub const PROPOSAL_TYPE_EMOJIS: [(&str, &str); 5] = [
    ("gioco", "🎮"),
    ("giochi", "🎮"),
    ("film", "🎬"),
    ("karaoke", "🎤"),
    ("altro", "✨"),
];

// ============================================================================
// LIVELLI CONTRIBUTOR (badge per utenti attivi)
// ============================================================================
// Array di tuple `(u32, &str)`: soglia punti -> etichetta badge.
// `u32` = intero senza segno a 32 bit (da 0 a ~4 miliardi).

/// Livelli di contributore basati sui punti accumulati.
/// Più punti = badge più prestigioso. Usato per gamification.
pub const CONTRIBUTOR_LEVELS: [(u32, &str); 6] = [
    (0, "🌱 Nuovo"),
    (10, "🥉 Bronzo"),
    (25, "🥈 Argento"),
    (50, "🥇 Oro"),
    (100, "💎 Platino"),
    (200, "👑 Leggenda"),
];

// ============================================================================
// BADGE NASCOSTI (easter eggs)
// ============================================================================
// Badge segreti che si sbloccano con comportamenti particolari.
// Non sono mostrati nell'elenco pubblico, solo quando li ottieni.

/// Badge nascosti: chiave interna -> nome visualizzato.
pub const HIDDEN_BADGES: [(&str, &str); 3] = [
    ("night_owl", "🌙 Nottataccia"),        // Proporre tardi la notte
    ("indecisive", "🔄 Indeciso"),          // Cambiare spesso voto
    ("influencer", "📣 Influencer"),        // Votare costantemente nel ballottaggio
];

/// Suggerimenti/testi di aiuto per i badge nascosti (mostrati quando li sblocchi).
pub const HIDDEN_BADGE_HINTS: [(&str, &str); 3] = [
    ("night_owl", "Le idee migliori arrivano quando tutti dormono…"),
    ("indecisive", "Cambiare idea è lecito. Cambiarla spesso, ancora di più."),
    ("influencer", "La costanza nel ballottaggio non passa inosservata."),
];

// ============================================================================
// PUNTI PER STATISTICHE AUTORE
// ============================================================================
// Quanti punti vale ogni azione per il calcolo del livello contributore.

/// Punti per azione: (nome_azione, punti).
/// - `supports_received`: quando altri supportano la tua proposta (2 punti)
/// - `ballot_entries`: quando la tua proposta entra nel ballottaggio (5 punti)
/// - `winning_proposals`: quando la tua proposta vince il ballottaggio (10 punti)
pub const AUTHOR_STAT_POINTS: [(&str, i64); 3] = [
    ("supports_received", 2),
    ("ballot_entries", 5),
    ("winning_proposals", 10),
];

// ============================================================================
// GESTIONE ERRORI DI CONFIGURAZIONE
// ============================================================================

/// Errore personalizzato per configurazione mancante o non valida.
///
/// In Rust gli errori sono valori, non eccezioni (niente `try/catch`).
/// Si usa il tipo `Result<T, E>` che può essere `Ok(T)` (successo) o `Err(E)` (errore).
/// Qui definiamo un nostro tipo di errore che contiene un messaggio (`String`).
///
/// `#[derive(Debug)]` genera automaticamente l'implementazione per stampare l'errore
/// con `{:?}` (utile per debug). `pub` rende il tipo visibile fuori dal modulo.
#[derive(Debug)]
pub struct ConfigurationError(pub String);

// `impl fmt::Display` permette di usare `{}` per stampare l'errore in modo user-friendly.
// `fmt::Formatter` è il "buffer" dove scrivere l'output formattato.
impl fmt::Display for ConfigurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)  // Scrive la stringa interna nel formatter
    }
}

// `impl std::error::Error` marca questo tipo come "errore standard" di Rust.
// Permette di usare `?` per propagare l'errore e di fare downcasting se serve.
impl std::error::Error for ConfigurationError {}

// ============================================================================
// STRUTTURA CONFIGURAZIONE AMBIENTE
// ============================================================================

/// Contiene i valori letti dalle variabili d'ambiente all'avvio.
///
/// `#[derive(Debug, Clone)]` genera automaticamente:
/// - `Debug`: stampa con `{:?}` per debug
/// - `Clone`: permette di duplicare la struttura (copia profonda dei campi)
///
/// I campi sono:
/// - `token`: String (posseduta) = token segreto del bot Discord
/// - `guild_id`: Option<u64> = ID del server Discord (None se non impostato)
/// - `db_path`: String = percorso del file database SQLite
///
/// `Option<T>` è un tipo fondamentale di Rust: rappresenta un valore che PUÒ mancare.
/// È l'alternativa sicura al `null`/`None` di altri linguaggi.
/// Vale `Some(valore)` se c'è un valore, `None` se manca.
/// Obbliga a gestire esplicitamente l'assenza (no crash per null pointer).
#[derive(Debug, Clone)]
pub struct EnvironmentConfig {
    pub token: String,
    pub guild_id: Option<u64>,
    pub db_path: String,
}

// ============================================================================
// FUNZIONI DI UTILITÀ INTERNE
// ============================================================================

/// Legge una variabile d'ambiente, restituisce stringa vuota se manca o è vuota.
///
/// `env::var(name)` restituisce `Result<String, VarError>`.
/// `.unwrap_or_default()` trasforma `Err` in `String::new()` (stringa vuota).
/// `.trim()` rimuove spazi/bianchi agli estremi.
/// `.to_string()` converte in `String` posseduta (da `&str`).
fn var(name: &str) -> String {
    env::var(name).unwrap_or_default().trim().to_string()
}

/// Restituisce il percorso del database da variabile d'ambiente o default.
///
/// `unwrap_or_else(|_| ...)` esegue la closure (funzione anonima) solo se c'è errore.
/// `database_path()` è una funzione pubblica usata da `load_environment()`.
pub fn database_path() -> String {
    env::var("DB_PATH").unwrap_or_else(|_| "sword_bot.db".to_string())
}

/// Controlla se una stringa contiene SOLO cifre ASCII (0-9) e non è vuota.
///
/// `value.chars()` itera sui caratteri Unicode.
/// `.all(|c| c.is_ascii_digit())` verifica che TUTTI soddisfino la condizione.
/// Serve per validare ID numerici (evita "123abc" o stringhe vuote).
fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_digit())
}

// ============================================================================
// FUNZIONE PRINCIPALE: CARICAMENTO CONFIGURAZIONE
// ============================================================================

/// Legge e valida tutte le variabili d'ambiente necessarie all'avvio del bot.
///
/// Restituisce `Result<EnvironmentConfig, ConfigurationError>`:
/// - `Ok(config)` se tutto è valido, contiene la configurazione pronta all'uso
/// - `Err(ConfigurationError)` se manca qualcosa o i valori non sono validi
///
/// Questa funzione viene chiamata una sola volta all'avvio in `main.rs`.
/// Se fallisce, il bot si ferma subito con messaggio d'errore chiaro.
pub fn load_environment() -> Result<EnvironmentConfig, ConfigurationError> {
    // `Vec<String>` = vettore (array dinamico) di stringhe possedute.
    // Qui accumuliamo tutti i problemi trovati per mostrarli insieme alla fine.
    let mut problems: Vec<String> = Vec::new();

    // --- DISCORD_TOKEN (obbligatorio) ---
    let token = var("DISCORD_TOKEN");
    if token.is_empty() {
        problems.push("DISCORD_TOKEN è obbligatoria (token del bot dal Developer Portal).".into());
    }

    // --- GUILD_ID (opzionale, ma se c'è deve essere numerico) ---
    let raw_guild = var("GUILD_ID");
    let mut guild_id = None;  // Option<u64> parte come None
    if !raw_guild.is_empty() {
        // `parse::<u64>()` tenta la conversione stringa -> numero.
        // Restituisce `Result<u64, ParseIntError>`.
        match raw_guild.parse::<u64>() {
            // `Ok(id) if is_digits(&raw_guild)` = pattern matching con guard (condizione extra).
            // Verifica che il parsing sia riuscito E che la stringa originale sia solo cifre.
            Ok(id) if is_digits(&raw_guild) => guild_id = Some(id),
            // `_` = "qualsiasi altro caso" (errore parsing O stringa con non-cifre).
            _ => problems.push("GUILD_ID deve essere l'ID numerico del server.".into()),
        }
    }

    // --- EVENT_OVERLAY_PUBLIC_URL (opzionale, ma se c'è deve essere HTTPS) ---
    let public_url = var("EVENT_OVERLAY_PUBLIC_URL");
    if !public_url.is_empty() && !public_url.starts_with("https://") {
        problems.push("EVENT_OVERLAY_PUBLIC_URL deve iniziare con https://.".into());
    }

    // --- EVENT_OVERLAY_PORT (opzionale, ma se c'è deve essere porta valida 1-65535) ---
    let raw_port = var("EVENT_OVERLAY_PORT");
    if !raw_port.is_empty() {
        // `is_ok_and(|p| ...)` = vero se Result è Ok E la closure restituisce true.
        let valid = is_digits(&raw_port) && raw_port.parse::<u32>().is_ok_and(|p| (1..65536).contains(&p));
        if !valid {
            problems.push("EVENT_OVERLAY_PORT deve essere una porta tra 1 e 65535.".into());
        }
    }

    // --- OAUTH DISCORD (serve per login web: servono ENTRAMBI o NESSUNO) ---
    // Array di 2 stringhe: [client_id, client_secret]
    let oauth = [var("DISCORD_OAUTH_CLIENT_ID"), var("DISCORD_OAUTH_CLIENT_SECRET")];
    // `.any(|v| !v.is_empty())` = almeno uno è presente
    // `.all(|v| !v.is_empty())` = tutti e due sono presenti
    // XOR logico: se uno c'è e l'altro no -> errore
    if oauth.iter().any(|v| !v.is_empty()) && !oauth.iter().all(|v| !v.is_empty()) {
        problems.push(
            "Per il login web servono sia DISCORD_OAUTH_CLIENT_ID sia DISCORD_OAUTH_CLIENT_SECRET."
                .into(),
        );
    }

    // --- SUPABASE (serve per report: servono ENTRAMBI o NESSUNO) ---
    let supabase = [var("SUPABASE_URL"), var("SUPABASE_SERVICE_ROLE_KEY")];
    if supabase.iter().any(|v| !v.is_empty()) && !supabase.iter().all(|v| !v.is_empty()) {
        problems.push("Per i report servono sia SUPABASE_URL sia SUPABASE_SERVICE_ROLE_KEY.".into());
    }

    // --- SE CI SONO PROBLEMI, RESTITUISCI ERRORE CON TUTTI I MESSAGGI ---
    if !problems.is_empty() {
        // `format!` crea una String formattata (come f-string in Python).
        // `problems.join("\n- ")` unisce i messaggi con newline e trattino.
        return Err(ConfigurationError(format!(
            "Configurazione non valida:\n- {}",
            problems.join("\n- ")
        )));
    }

    // --- SUCCESSO: RESTITUISCI LA CONFIGURAZIONE VALIDATA ---
    Ok(EnvironmentConfig {
        token,
        guild_id,
        db_path: database_path(),
    })
}
