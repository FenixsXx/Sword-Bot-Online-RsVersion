//! La parte "dati" di `cogs/live_event.py`: le scritture sul database di un evento live, il report
//! e il link dell'overlay. Non parla con Discord né con la voce: riceve istanti e conteggi già
//! pronti, così si prova con gli stessi scenari di Python.
//!
//! Gli istanti sono salvati come `datetime.isoformat()` di Python (`2027-03-05T21:00:00+01:00`),
//! quindi un database scritto da una versione si legge dall'altra.
//!
//! Questo modulo contiene tutta la logica di persistenza per gli eventi live:
//! - Creazione/chiusura degli eventi nel database (tabelle `live_events`, `live_event_participants`)
//! - Tracciamento presenze (chi entra/esce dal canale vocale e per quanto tempo)
//! - Tracciamento parlato (chi parla e per quanti secondi, basato sui pacchetti audio)
//! - Generazione del report finale (embed Discord con statistiche, speaker, confronti)
//! - Link dell'overlay per OBS (con token segreto nel fragment URL)
//! - Recupero eventi interrotti al riavvio del bot

// Le funzioni si collegano ai comandi e all'avvio dell'evento nei prossimi passi.
#![allow(dead_code)]

use std::collections::BTreeSet;

use chrono::{DateTime, FixedOffset};
use chrono_tz::Tz;
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use tracing::error;

use crate::cards::EmbedSpec;
use crate::config::{COLOR_PRESTIGE, COLOR_STAFF};
use crate::database::{Database, DbResult};
use crate::embeds::{TimestampStyle, discord_timestamp};
use crate::formatting::{python_json, truncate_chars};
use crate::schedule::isoformat;

/// I layout dell'overlay: (valore, nome mostrato).
/// Ogni tema cambia l'aspetto visivo della pagina overlay (es. "classic", "podcast", "esport").
pub const OVERLAY_THEMES: [(&str, &str); 5] =
    [("classic", "Classico"), ("podcast", "Podcast"), ("esport", "Esport"), ("minimal", "Minimal"), ("stage", "Stage")];

/// Testo fisso nel footer di ogni report evento.
pub const REPORT_FOOTER: &str =
    "Le durate del parlato sono stime dai pacchetti audio; nessun audio è conservato. Report individuali eliminabili dallo staff.";

// --- link e token ----------------------------------------------------------------------------

/// Perché non si riesce a costruire il link dell'overlay (le eccezioni di `overlay_link`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// Manca l'URL pubblico configurato.
    NoPublicUrl,
    /// Tema non riconosciuto.
    InvalidTheme(String),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPublicUrl => f.write_str("URL pubblico dell'overlay non configurato."),
            Self::InvalidTheme(theme) => write!(f, "Tema overlay non valido: {theme}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// Costruisce il link completo dell'overlay per OBS.
/// Formato: `https://dominio/overlay/<server>?tema=<tema>#<chiave>`
/// La chiave sta nel frammento (dopo `#`), così non arriva mai al server nei log delle richieste HTTP.
pub fn overlay_link(public_url: &str, guild_id: u64, theme: &str, token: &str) -> Result<String, LinkError> {
    if public_url.is_empty() {
        return Err(LinkError::NoPublicUrl);
    }
    if !OVERLAY_THEMES.iter().any(|(value, _)| *value == theme) {
        return Err(LinkError::InvalidTheme(theme.to_string()));
    }
    Ok(format!("{public_url}/overlay/{guild_id}?tema={theme}#{token}"))
}

/// Genera un token casuale "url-safe" (base64 senza padding, 43 caratteri).
/// Equivalente a Python `secrets.token_urlsafe(32)`: 32 byte casuali codificati in base64 URL-safe.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("il generatore casuale del sistema non risponde");
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut text = String::with_capacity(43);
    for chunk in bytes.chunks(3) {
        let mut value = 0u32;
        for (i, byte) in chunk.iter().enumerate() {
            value |= u32::from(*byte) << (16 - 8 * i);
        }
        // 3 byte -> 4 caratteri; con 1 o 2 byte restano 2 o 3 caratteri (niente `=`).
        for i in 0..=chunk.len() {
            text.push(char::from(ALPHABET[((value >> (18 - 6 * i)) & 0x3F) as usize]));
        }
    }
    text
}

/// Recupera o crea la chiave dell'overlay (`live_event_token` nel database).
/// Se non esiste ancora, la genera e la salva.
pub fn overlay_token(db: &Database) -> DbResult<String> {
    overlay_token_with(db, new_token)
}

/// Versione interna che accetta un generatore personalizzato (usata nei test per token fissi).
pub fn overlay_token_with(db: &Database, generate: impl FnOnce() -> String) -> DbResult<String> {
    match db.get_setting("live_event_token")?.filter(|token| !token.is_empty()) {
        Some(token) => Ok(token),
        None => {
            let token = generate();
            db.set_setting("live_event_token", &token)?;
            Ok(token)
        }
    }
}

// --- eventi Discord abilitati ----------------------------------------------------------------

/// Converte una stringa in intero come fa Python `int(testo)`:
/// - Spazi ai bordi ignorati
/// - Segno + o - opzionale
/// - Cifre decimali Unicode (non solo ASCII, es. numeri arabi ١٢)
/// - Underscore `_` come separatori tra cifre (ma non all'inizio/fine o doppi)
pub(crate) fn python_int(text: &str) -> Option<i64> {
    let text = text.trim();
    let (negative, digits) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let mut value: i64 = 0;
    let mut previous_underscore = true; // niente `_` all'inizio
    let mut any = false;
    for c in digits.chars() {
        if c == '_' {
            if previous_underscore {
                return None;
            }
            previous_underscore = true;
            continue;
        }
        let digit = decimal_digit(c)?;
        value = value.checked_mul(10)?.checked_add(i64::from(digit))?;
        previous_underscore = false;
        any = true;
    }
    if !any || previous_underscore {
        return None;
    }
    Some(if negative { -value } else { value })
}

/// Il valore di una cifra decimale Unicode (categoria `Nd` = Decimal Number).
/// I gruppi di dieci cifre sono contigui in Unicode (es. 0-9, ٠-٩, ०-९, ecc.).
pub(crate) fn decimal_digit(c: char) -> Option<u32> {
    use unicode_general_category::{GeneralCategory, get_general_category};
    if c.is_ascii_digit() {
        return c.to_digit(10);
    }
    if get_general_category(c) != GeneralCategory::DecimalNumber {
        return None;
    }
    let mut start = u32::from(c);
    while char::from_u32(start - 1).is_some_and(|before| get_general_category(before) == GeneralCategory::DecimalNumber) {
        start -= 1;
    }
    Some((u32::from(c) - start) % 10)
}

/// Restituisce l'insieme degli ID degli eventi Discord che hanno l'avvio automatico abilitato.
/// Legge l'impostazione `scheduled_event_optins` (JSON array) dal database.
/// Un valore illeggibile si registra e vale "nessuno" (insieme vuoto).
pub fn scheduled_event_optins(db: &Database) -> BTreeSet<i64> {
    let raw = db.get_setting_or("scheduled_event_optins", "[]").unwrap_or_else(|_| "[]".into());
    let parsed: Option<BTreeSet<i64>> = serde_json::from_str::<Value>(&raw).ok().and_then(|value| {
        value.as_array()?.iter().map(optin_value).collect::<Option<BTreeSet<i64>>>()
    });
    parsed.unwrap_or_else(|| {
        error!("Impossibile leggere gli eventi Discord abilitati.");
        BTreeSet::new()
    })
}

/// Converte un singolo valore JSON in `i64` per la lista degli opt-in.
/// Accetta bool (true=1, false=0), number, o stringa (con `python_int`).
fn optin_value(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(flag) => Some(i64::from(*flag)),
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::String(text) => python_int(text),
        _ => None,
    }
}

/// Salva la lista degli eventi abilitati come JSON array compatto.
pub fn save_scheduled_event_optins(db: &Database, ids: &BTreeSet<i64>) -> DbResult<()> {
    db.set_setting("scheduled_event_optins", python_json(&json!(ids.iter().collect::<Vec<_>>())))
}

// --- istanti ---------------------------------------------------------------------------------

/// Crea un errore di conversione per rusqlite (usato quando il parsing ISO fallisce).
fn conversion_error(message: String) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(message.into())
}

/// Parsa una stringa ISO 8601/RFC3339 in `DateTime<FixedOffset>`.
/// Equivale a Python `datetime.fromisoformat(testo)` per gli istanti che salviamo.
fn parse_instant(text: &str) -> DbResult<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(text).map_err(|e| conversion_error(format!("istante non valido «{text}»: {e}")))
}

/// Converte un `DateTime<FixedOffset>` in stringa ISO 8601 con microsecondi se non zero.
/// Equivale a Python `datetime.isoformat()`.
fn instant_text(value: &DateTime<FixedOffset>) -> String {
    let micros = value.timestamp_subsec_micros();
    let base = value.format("%Y-%m-%dT%H:%M:%S");
    let offset = value.format("%:z");
    if micros == 0 { format!("{base}{offset}") } else { format!("{base}.{micros:06}{offset}") }
}

/// Secondi interi tra due istanti, arrotondati per difetto, minimo 0.
/// Equivale a `max(0, int((fine - inizio).total_seconds()))`.
fn elapsed_seconds(from: &DateTime<FixedOffset>, to: &DateTime<FixedOffset>) -> i64 {
    let micros = to.signed_duration_since(*from).num_microseconds().unwrap_or(i64::MAX);
    ((micros as f64 / 1e6) as i64).max(0)
}

/// Tipo alias: fette di tuple (user_id, secondi_totali_parlato), nell'ordine di comparsa.
pub type SpeakingSeconds = [(u64, f64)];

/// Quanti secondi di parlato per un utente sono già salvati nel database (da `persisted`).
fn persisted_of(persisted: &SpeakingSeconds, user_id: u64) -> f64 {
    persisted.iter().find(|(id, _)| *id == user_id).map_or(0.0, |(_, seconds)| *seconds)
}

/// Salva nel database solo il delta di parlato non ancora persistito.
/// Usa `ON CONFLICT` per sommare ai secondi esistenti (upsert).
fn save_speaking(tx: &Transaction<'_>, event_id: i64, total: &SpeakingSeconds, persisted: &SpeakingSeconds) -> DbResult<()> {
    for (user_id, seconds) in total {
        let delta = (seconds - persisted_of(persisted, *user_id)).max(0.0);
        if delta == 0.0 {
            continue;
        }
        tx.execute(
            "INSERT INTO live_event_participants (event_id, user_id, speaking_seconds) VALUES(?,?,?) \
             ON CONFLICT(event_id,user_id) DO UPDATE SET \
             speaking_seconds=live_event_participants.speaking_seconds + excluded.speaking_seconds",
            params![event_id, *user_id as i64, delta],
        )?;
    }
    Ok(())
}

/// Chiude le presenze ancora aperte di un evento (chi era dentro al momento della fine).
/// Per ogni utente con `joined_at` non nullo: calcola i secondi trascorsi e li aggiunge a `participation_seconds`,
/// poi azzera `joined_at` per marcare la presenza come chiusa.
fn close_open_presences(tx: &Transaction<'_>, event_id: i64, ended: &DateTime<FixedOffset>) -> DbResult<()> {
    let open: Vec<(i64, String)> = tx
        .prepare("SELECT user_id, joined_at FROM live_event_participants WHERE event_id=? AND joined_at IS NOT NULL")?
        .query_map([event_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (user_id, joined_at) in open {
        let elapsed = elapsed_seconds(&parse_instant(&joined_at)?, ended);
        tx.execute(
            "UPDATE live_event_participants SET participation_seconds=participation_seconds+?, joined_at=NULL WHERE event_id=? AND user_id=?",
            params![elapsed, event_id, user_id],
        )?;
    }
    Ok(())
}

// --- scritture dell'evento -------------------------------------------------------------------

/// Chiude gli eventi rimasti aperti perché il bot si è fermato (crash o riavvio).
/// Viene chiamata all'avvio del bot (`_recover_interrupted_events`).
/// Per ogni evento senza `ended_at`: usa `last_seen_at` (o `started_at` se manca) come fine,
/// chiude le presenze aperte, conta i partecipanti e marca l'evento come chiuso con motivo "riavvio_bot".
pub fn recover_interrupted_events(db: &Database) -> DbResult<()> {
    db.transaction(|tx| {
        let unfinished: Vec<(i64, String, Option<String>)> = tx
            .prepare("SELECT id, started_at, last_seen_at FROM live_events WHERE ended_at IS NULL")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;
        for (id, started_at, last_seen_at) in unfinished {
            let ended = parse_instant(last_seen_at.as_deref().filter(|text| !text.is_empty()).unwrap_or(&started_at))?;
            close_open_presences(tx, id, &ended)?;
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM live_event_participants WHERE event_id=?", [id], |row| row.get(0))?;
            tx.execute(
                "UPDATE live_events SET ended_at=?, participant_count=?, end_reason='riavvio_bot' WHERE id=?",
                params![instant_text(&ended), count, id],
            )?;
        }
        Ok(())
    })
}

/// I dati necessari per creare un nuovo evento.
pub struct NewEvent<'a> {
    pub guild_id: u64,
    pub scheduled_event_id: Option<u64>,
    pub title: &'a str,
    pub channel_id: u64,
    pub channel_name: &'a str,
    pub started_at: DateTime<Tz>,
}

/// Crea le righe del database per un nuovo evento:
/// - Inserisce in `live_events` (evento principale)
/// - Per ogni membro nel canale: inserisce in `live_event_participants` con `joined_at = started_at`
///   e registra `community_activity(action='event_join')`
/// Restituisce l'ID del report creato.
pub fn create_event_rows(db: &Database, event: &NewEvent<'_>, members: &[u64]) -> DbResult<i64> {
    let started = isoformat(event.started_at);
    db.transaction(|tx| {
        tx.execute(
            "INSERT INTO live_events (guild_id, scheduled_event_id, title, channel_id, channel_name, started_at, last_seen_at, peak_participants) \
             VALUES(?,?,?,?,?,?,?,?)",
            params![
                event.guild_id as i64,
                event.scheduled_event_id.map(|id| id as i64),
                event.title,
                event.channel_id as i64,
                event.channel_name,
                started,
                started,
                members.len() as i64,
            ],
        )?;
        let event_id = tx.last_insert_rowid();
        for member in members {
            tx.execute(
                "INSERT INTO live_event_participants (event_id, user_id, joined_at) VALUES(?,?,?)",
                params![event_id, *member as i64, started],
            )?;
            tx.execute(
                "INSERT INTO community_activity(action,occurred_at) VALUES('event_join',?)",
                [event.started_at.timestamp()],
            )?;
        }
        Ok(event_id)
    })
}

/// Salva periodicamente (ogni 15 secondi) il parlato maturato e aggiorna `last_seen_at`.
/// Chiamata dal ciclo di pubblicazione (`_persist_live_progress` in Python).
pub fn persist_progress(db: &Database, event_id: i64, total: &SpeakingSeconds, persisted: &SpeakingSeconds, at: DateTime<Tz>) -> DbResult<()> {
    db.transaction(|tx| {
        save_speaking(tx, event_id, total, persisted)?;
        tx.execute("UPDATE live_events SET last_seen_at=? WHERE id=?", params![isoformat(at), event_id])?;
        Ok(())
    })
}

/// Registra un cambio di presenza (utente entra/esce dal canale dell'evento).
/// - Se `was_present=true`: chiude la presenza aperta (calcola secondi trascorsi da `joined_at` ad ora).
/// - Se `is_present=true`: apre una nuova presenza (imposta `joined_at = now`).
/// - Aggiorna `peak_participants` se `present_now` supera il picco attuale.
/// - Registra `community_activity('event_join')` all'ingresso.
/// Restituisce il nuovo picco di presenti.
pub fn record_presence_change(
    db: &Database,
    event_id: i64,
    user_id: u64,
    was_present: bool,
    is_present: bool,
    now: DateTime<Tz>,
    present_now: i64,
    peak: i64,
) -> DbResult<i64> {
    let now_text = isoformat(now);
    let now_fixed = now.fixed_offset();
    db.transaction(|tx| {
        if was_present {
            let joined: Option<Option<String>> = tx
                .query_row(
                    "SELECT joined_at FROM live_event_participants WHERE event_id=? AND user_id=?",
                    params![event_id, user_id as i64],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(Some(joined_at)) = joined.filter(|joined| joined.as_deref().is_some_and(|text| !text.is_empty())) {
                let elapsed = elapsed_seconds(&parse_instant(&joined_at)?, &now_fixed);
                tx.execute(
                    "UPDATE live_event_participants SET participation_seconds=participation_seconds+?, joined_at=NULL WHERE event_id=? AND user_id=?",
                    params![elapsed, event_id, user_id as i64],
                )?;
            }
        }
        if is_present {
            tx.execute(
                "INSERT INTO live_event_participants (event_id, user_id, joined_at) VALUES(?,?,?) \
                 ON CONFLICT(event_id,user_id) DO UPDATE SET joined_at=excluded.joined_at \
                 WHERE live_event_participants.joined_at IS NULL",
                params![event_id, user_id as i64, now_text],
            )?;
            tx.execute("INSERT INTO community_activity(action,occurred_at) VALUES('event_join',?)", [now.timestamp()])?;
        }
        let peak = peak.max(present_now);
        tx.execute("UPDATE live_events SET peak_participants=? WHERE id=?", params![peak, event_id])?;
        Ok(peak)
    })
}

/// Chiude definitivamente l'evento:
/// 1. Chiude le presenze ancora aperte (`close_open_presences`).
/// 2. Salva il parlato residuo non ancora persistito (`save_speaking`).
/// 3. Conta i partecipanti unici totali.
/// 4. Aggiorna `live_events` con `ended_at`, `last_seen_at`, `peak_participants` (max tra picco sessione e presenti finali),
///    `participant_count` e `end_reason`.
pub fn finish_event_rows(
    db: &Database,
    event_id: i64,
    total: &SpeakingSeconds,
    persisted: &SpeakingSeconds,
    session_peak: i64,
    present_now: i64,
    ended_at: DateTime<Tz>,
    reason: &str,
) -> DbResult<()> {
    let ended_text = isoformat(ended_at);
    let ended = ended_at.fixed_offset();
    db.transaction(|tx| {
        close_open_presences(tx, event_id, &ended)?;
        save_speaking(tx, event_id, total, persisted)?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM live_event_participants WHERE event_id=?", [event_id], |row| row.get(0))?;
        tx.execute(
            "UPDATE live_events SET last_seen_at=?, ended_at=?, peak_participants=?, participant_count=?, end_reason=? WHERE id=?",
            params![ended_text, ended_text, session_peak.max(present_now), count, reason, event_id],
        )?;
        Ok(())
    })
}

/// Elimina un report concluso (solo se `ended_at IS NOT NULL`).
/// Restituisce quante righe ha tolto (0 o 1). Cascata: i partecipanti vengono eliminati via FK.
pub fn delete_report(db: &Database, event_id: i64) -> DbResult<usize> {
    db.transaction(|tx| tx.execute("DELETE FROM live_events WHERE id=? AND ended_at IS NOT NULL", [event_id]))
}

/// Verifica se un report concluso esiste (ha `ended_at` non nullo).
pub fn report_exists(db: &Database, event_id: i64) -> DbResult<bool> {
    Ok(db
        .with_conn(|c| c.query_row("SELECT 1 FROM live_events WHERE id=? AND ended_at IS NOT NULL", [event_id], |_| Ok(())).optional())?
        .is_some())
}

// --- report ----------------------------------------------------------------------------------

/// Formatta una percentuale con segno: `+20%` o `-25%`.
fn signed_percent(change: f64) -> String {
    let text = format!("{:.0}", change * 100.0);
    if change >= 0.0 { format!("+{text}%") } else { format!("{text}%") }
}

/// Confronta le metriche di questo evento con la media degli ultimi 10 eventi conclusi precedenti.
/// Restituisce una stringa formattata per l'embed, o `None` se non ci sono eventi precedenti.
pub fn event_comparison(db: &Database, event_id: i64, unique: i64, peak: i64, duration: i64) -> DbResult<Option<String>> {
    struct Previous {
        participants: i64,
        peak: i64,
        started: String,
        ended: String,
    }
    let previous: Vec<Previous> = db.with_conn(|c| -> DbResult<_> {
        c.prepare(
            "SELECT participant_count, peak_participants, started_at, ended_at FROM live_events \
             WHERE ended_at IS NOT NULL AND id<>? AND started_at<(SELECT started_at FROM live_events WHERE id=?) \
             ORDER BY started_at DESC LIMIT 10",
        )?
        .query_map(params![event_id, event_id], |row| {
            Ok(Previous { participants: row.get(0)?, peak: row.get(1)?, started: row.get(2)?, ended: row.get(3)? })
        })?
        .collect()
    })?;
    if previous.is_empty() {
        return Ok(None);
    }
    let count = previous.len() as f64;
    let average_unique = previous.iter().map(|row| row.participants).sum::<i64>() as f64 / count;
    let average_peak = previous.iter().map(|row| row.peak).sum::<i64>() as f64 / count;
    let mut total_duration = 0.0;
    for row in &previous {
        let micros = parse_instant(&row.ended)?.signed_duration_since(parse_instant(&row.started)?).num_microseconds().unwrap_or(0);
        total_duration += (micros as f64 / 1e6).max(0.0);
    }
    let average_duration = total_duration / count;

    let trend = |value: f64, average: f64| -> String {
        if average <= 0.0 {
            return String::new();
        }
        let change = (value - average) / average;
        if change.abs() < 0.05 {
            return " ➖ in linea".to_string();
        }
        format!(" {} {}", if change > 0.0 { "🔺" } else { "🔻" }, signed_percent(change))
    };
    Ok(Some(format!(
        "Partecipanti `{unique}` vs media `{average_unique:.1}`{}\nPicco `{peak}` vs media `{average_peak:.1}`{}\nDurata `{} min` vs media `{} min`{}\n*Media su {} eventi precedenti.*",
        trend(unique as f64, average_unique),
        trend(peak as f64, average_peak),
        duration / 60,
        average_duration as i64 / 60,
        trend(duration as f64, average_duration),
        previous.len(),
    )))
}

/// Costruisce l'embed Discord del report per un evento concluso.
/// Restituisce `None` se l'evento non esiste o è ancora attivo.
/// L'embed contiene:
/// - Titolo e descrizione base (canale, inizio, durata, motivo fine)
/// - Campo "Partecipazione" (unici, picco, presenza media)
/// - Campo "Rispetto agli eventi precedenti" (confronto con media ultimi 10)
/// - Campo "Nuovi partecipanti" (chi è alla prima Game Night)
/// - Campo "Proposta vincitrice" (se l'evento era collegato a una proposta)
/// - Campo "Speaker principali" (top 5 per tempo di parlato, con percentuale)
/// - Footer fisso `REPORT_FOOTER`
pub fn build_event_report(db: &Database, event_id: i64) -> DbResult<Option<EmbedSpec>> {
    struct Event {
        title: String,
        channel_id: i64,
        started_at: String,
        ended_at: String,
        peak: i64,
        end_reason: String,
        scheduled_event_id: Option<i64>,
    }
    let event = db
        .with_conn(|c| {
            c.query_row(
                "SELECT title, channel_id, started_at, ended_at, peak_participants, end_reason, scheduled_event_id \
                 FROM live_events WHERE id=? AND ended_at IS NOT NULL",
                [event_id],
                |row| {
                    Ok(Event {
                        title: row.get(0)?,
                        channel_id: row.get(1)?,
                        started_at: row.get(2)?,
                        ended_at: row.get(3)?,
                        peak: row.get(4)?,
                        end_reason: row.get(5)?,
                        scheduled_event_id: row.get(6)?,
                    })
                },
            )
            .optional()
        })?;
    let Some(event) = event else {
        return Ok(None);
    };
    // Partecipanti ordinati per parlato (desc) poi presenza (desc).
    let participants: Vec<(i64, i64, f64)> = db.with_conn(|c| -> DbResult<_> {
        c.prepare(
            "SELECT user_id, participation_seconds, speaking_seconds FROM live_event_participants WHERE event_id=? \
             ORDER BY speaking_seconds DESC, participation_seconds DESC",
        )?
        .query_map([event_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect()
    })?;
    let started = parse_instant(&event.started_at)?;
    let ended = parse_instant(&event.ended_at)?;
    let duration = elapsed_seconds(&started, &ended);
    let total_speaking: f64 = participants.iter().map(|(_, _, speaking)| speaking).sum();
    // Top 5 speaker con parlato > 0.
    let speakers: Vec<String> = participants
        .iter()
        .take(5)
        .filter(|(_, _, speaking)| *speaking > 0.0 && total_speaking > 0.0)
        .map(|(user, _, speaking)| format!("<@{user}> · `{}s` · `{:.0}%`", *speaking as i64, speaking / total_speaking * 100.0))
        .collect();
    let total_presence: i64 = participants.iter().map(|(_, presence, _)| presence).sum();
    let average_presence = if participants.is_empty() { 0 } else { (total_presence as f64 / participants.len() as f64) as i64 };

    let mut embed = EmbedSpec::new(
        format!("📊 Report evento #{event_id} · {}", truncate_chars(&event.title, 180)),
        format!(
            "Canale: <#{}>\nInizio: {}\nDurata: `{}h {}m`\nFine: `{}`",
            event.channel_id,
            discord_timestamp(&started, TimestampStyle::LongDateTime),
            duration / 3600,
            duration % 3600 / 60,
            event.end_reason,
        ),
        COLOR_PRESTIGE,
    )
    .field(
        "👥 Partecipazione",
        format!(
            "Partecipanti unici `{}` · picco `{}` · presenza media `{} min`",
            participants.len(),
            event.peak,
            average_presence / 60
        ),
        false,
    );
    if let Some(comparison) = event_comparison(db, event_id, participants.len() as i64, event.peak, duration)? {
        embed = embed.field("📈 Rispetto agli eventi precedenti", comparison, false);
    }
    // Conta i nuovi partecipanti: chi non ha MAI partecipato a eventi precedenti (con started_at minore).
    let newcomers: i64 = db.with_conn(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM live_event_participants p WHERE p.event_id=? AND NOT EXISTS ( \
                 SELECT 1 FROM live_event_participants old JOIN live_events e ON e.id=old.event_id \
                 WHERE old.user_id=p.user_id AND old.event_id<>p.event_id AND e.started_at<? )",
            params![event_id, event.started_at],
            |row| row.get(0),
        )
    })?;
    if !participants.is_empty() {
        let text = if newcomers > 0 { format!("`{newcomers}` alla loro prima Game Night") } else { "Nessun nuovo partecipante questa volta.".to_string() };
        embed = embed.field("🌱 Nuovi partecipanti", text, true);
    }
    // Se l'evento era collegato a una proposta (scheduled_event_id), mostra il nome della proposta.
    let proposal = match event.scheduled_event_id.filter(|id| *id != 0) {
        Some(id) => crate::gamenight::event_proposal_name(db, id as u64)?.filter(|name| !name.is_empty()),
        None => None,
    };
    if let Some(name) = proposal {
        embed = embed.field("🏆 Proposta vincitrice", format!("**{name}**"), true);
    }
    let speakers = if speakers.is_empty() { "Nessun parlato rilevato.".to_string() } else { speakers.join("\n") };
    Ok(Some(embed.field("🎤 Speaker principali", speakers, false).footer(REPORT_FOOTER)))
}

/// Embed con gli ultimi 10 report conclusi (per `/evento rapporto` senza ID).
pub fn recent_reports_embed(db: &Database) -> DbResult<EmbedSpec> {
    let lines: Vec<String> = db.with_conn(|c| -> DbResult<_> {
        c.prepare(
            "SELECT id, title, started_at, participant_count, peak_participants FROM live_events \
             WHERE ended_at IS NOT NULL ORDER BY id DESC LIMIT 10",
        )?
        .query_map([], |row| {
            let (id, title, started, count, peak): (i64, String, String, i64, i64) =
                (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?);
            Ok(format!(
                "`#{id}` **{}** · `{count} partecipanti` · picco `{peak}` · `{}`",
                truncate_chars(&title, 100),
                truncate_chars(&started, 10)
            ))
        })?
        .collect()
    })?;
    let description = if lines.is_empty() { "Nessun report archiviato.".to_string() } else { lines.join("\n") };
    Ok(EmbedSpec::new("📚 Report eventi recenti", description, COLOR_STAFF).footer("Usa /evento rapporto id_rapporto:<ID> per il dettaglio."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use chrono::TimeZone;

    fn golden() -> Value {
        serde_json::from_str(include_str!("golden/live_core_expected.json")).unwrap()
    }

    fn test_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()).unwrap();
        db
    }

    /// Le righe di una query come `[[valore, …], …]` (stessa forma dei golden Python).
    fn table(db: &Database, sql: &str) -> Value {
        db.with_conn(|c| {
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

    /// Un embed nella forma di `Embed.to_dict()` di discord.py (senza `flags` e `type`).
    fn embed_json(spec: &EmbedSpec) -> Value {
        json!({
            "title": spec.title,
            "description": spec.description,
            "color": spec.color,
            "fields": spec.fields.iter().map(|f| json!({"name": f.name, "value": f.value, "inline": f.inline})).collect::<Vec<_>>(),
            "footer": spec.footer.as_ref().map(|text| json!({"text": text})),
        })
    }

    fn expected_embed(value: &Value) -> Value {
        let mut value = value.clone();
        let map = value.as_object_mut().unwrap();
        map.remove("flags");
        map.remove("type");
        if map.get("fields").is_none() {
            map.insert("fields".into(), json!([]));
        }
        value
    }

    fn normalized(mut value: Value) -> Value {
        // `footer: null` e `footer` assente sono lo stesso.
        if let Some(map) = value.as_object_mut() {
            if map.get("footer").is_some_and(Value::is_null) {
                map.remove("footer");
            }
        }
        value
    }

    #[test]
    fn overlay_links_and_token_match_python() {
        let golden = golden();
        for case in golden["links"].as_array().unwrap() {
            let (name, expected) = (case[0].as_str().unwrap(), case[1].as_str().unwrap());
            let actual = match name {
                "senza_dominio" => overlay_link("", 1234, "classic", "token-fisso").map_err(|e| format!("RuntimeError: {e}")),
                theme => overlay_link("https://overlay.example.it", 1234, theme, "token-fisso").map_err(|e| format!("ValueError: {e}")),
            };
            assert_eq!(actual.unwrap_or_else(|e| e), expected, "{name}");
        }
        let db = test_db();
        assert_eq!(overlay_token_with(&db, || "token-fisso".into()).unwrap(), "token-fisso");
        assert_eq!(overlay_token_with(&db, || panic!("il token esiste già")).unwrap(), "token-fisso");
        assert_eq!(db.get_setting("live_event_token").unwrap().as_deref(), Some("token-fisso"));
        // Un token vuoto si rigenera, come `if not token`.
        db.set_setting("live_event_token", "").unwrap();
        assert_eq!(overlay_token_with(&db, || "nuovo".into()).unwrap(), "nuovo");
    }

    #[test]
    fn generated_tokens_are_url_safe_and_different() {
        let (a, b) = (new_token(), new_token());
        assert_ne!(a, b);
        for token in [&a, &b] {
            assert_eq!(token.len(), 43, "32 byte in base64 senza padding");
            assert!(token.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'), "{token}");
        }
    }

    #[test]
    fn scheduled_event_optins_match_python() {
        let golden = golden();
        let db = test_db();
        for case in golden["optins"]["cases"].as_array().unwrap() {
            let raw = case[0].as_str().unwrap();
            db.set_setting("scheduled_event_optins", raw).unwrap();
            let actual: Vec<i64> = scheduled_event_optins(&db).into_iter().collect();
            assert_eq!(json!(actual), case[1], "{raw:?}");
        }
        save_scheduled_event_optins(&db, &BTreeSet::from([30, 10, 20])).unwrap();
        assert_eq!(db.get_setting("scheduled_event_optins").unwrap().as_deref(), golden["optins"]["saved"].as_str());
        save_scheduled_event_optins(&db, &BTreeSet::new()).unwrap();
        assert_eq!(db.get_setting("scheduled_event_optins").unwrap().as_deref(), golden["optins"]["saved_empty"].as_str());
    }

    const REPORT_FIXTURE: &str = "
INSERT INTO live_events(id,guild_id,scheduled_event_id,title,channel_id,channel_name,started_at,last_seen_at,ended_at,peak_participants,participant_count,end_reason) VALUES
 (1,1,NULL,'Game Night 1',555,'vocale','2027-01-01T21:00:00+01:00','2027-01-01T23:00:00+01:00','2027-01-01T23:00:00+01:00',8,10,'manuale'),
 (2,1,NULL,'Game Night 2',555,'vocale','2027-01-08T21:00:00+01:00','2027-01-08T22:30:00+01:00','2027-01-08T22:30:00+01:00',9,12,'disconnessione'),
 (3,1,NULL,'Serata silenziosa',555,'vocale','2027-01-15T21:00:00+01:00','2027-01-15T23:15:00+01:00','2027-01-15T23:15:00+01:00',4,4,'manuale'),
 (4,1,NULL,'Lampo',555,'vocale','2027-01-22T21:00:00+01:00','2027-01-22T21:00:00.500000+01:00','2027-01-22T21:00:00.500000+01:00',0,0,'errore'),
 (5,1,777,'__LUNGO__',556,'altro','2027-02-01T21:00:00.250000+01:00','2027-02-01T23:00:30.500000+01:00','2027-02-01T23:00:30.500000+01:00',5,7,'completato'),
 (6,1,NULL,'Senza partecipanti',555,'vocale','2027-02-08T21:00:00+01:00','2027-02-08T21:30:00+01:00','2027-02-08T21:30:00+01:00',0,0,'annullato'),
 (7,1,NULL,'In corso',555,'vocale','2027-03-01T21:00:00+01:00','2027-03-01T21:00:00+01:00',NULL,2,2,'manual'),
 (8,1,778,'Pari alle medie',555,'vocale','2027-02-15T21:00:00+01:00','2027-02-15T23:00:00+01:00','2027-02-15T23:00:00+01:00',7,8,'manuale');
INSERT INTO live_event_participants(event_id,user_id,joined_at,participation_seconds,speaking_seconds) VALUES
 (1,101,NULL,7200,600),(1,102,NULL,3600,300),(1,103,NULL,1800,0),
 (2,101,NULL,5400,100.5),(2,104,NULL,100,0),
 (3,105,NULL,600,0),(3,106,NULL,900,0),
 (5,101,NULL,7200,1000),(5,102,NULL,3600,500.75),(5,107,NULL,3000,250),(5,108,NULL,2000,100),(5,109,NULL,1000,50),(5,110,NULL,900,25),(5,111,NULL,60,0),
 (8,101,NULL,100,1),(8,112,NULL,200,2);
";

    #[test]
    fn reports_and_comparisons_match_python() {
        let golden = golden();
        let db = test_db();
        db.execute_batch(&REPORT_FIXTURE.replace("__LUNGO__", &format!("Serata lunga é🎮 {}", "x".repeat(190)))).unwrap();
        db.set_setting("scheduled_event_proposal:777", "Among Us & co").unwrap();
        let reports = golden["reports"].as_object().unwrap();
        assert_eq!(reports.len(), 9);
        for (id, expected) in reports {
            let actual = build_event_report(&db, id.parse().unwrap()).unwrap();
            match (actual, expected) {
                (None, Value::Null) => {}
                (Some(spec), expected) => assert_eq!(normalized(embed_json(&spec)), normalized(expected_embed(expected)), "report {id}"),
                (None, _) => panic!("report {id} atteso"),
            }
        }
        for case in golden["comparisons"].as_array().unwrap() {
            let [id, unique, peak, duration, expected] = case.as_array().unwrap().as_slice() else { panic!("caso malformato") };
            let actual = event_comparison(&db, id.as_i64().unwrap(), unique.as_i64().unwrap(), peak.as_i64().unwrap(), duration.as_i64().unwrap()).unwrap();
            assert_eq!(actual.map(Value::from).unwrap_or(Value::Null), *expected, "confronto per {id}");
        }
    }

    #[test]
    fn recent_reports_list_and_deletion() {
        let db = test_db();
        assert_eq!(recent_reports_embed(&db).unwrap().description_text(), "Nessun report archiviato.");
        db.execute_batch(&REPORT_FIXTURE.replace("__LUNGO__", "Serata")).unwrap();
        let list = recent_reports_embed(&db).unwrap();
        let lines: Vec<&str> = list.description_text().lines().collect();
        // Il 7 è ancora attivo e non compare; gli altri dal più recente.
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0], "`#8` **Pari alle medie** · `8 partecipanti` · picco `7` · `2027-02-15`");
        assert_eq!(lines[6], "`#1` **Game Night 1** · `10 partecipanti` · picco `8` · `2027-01-01`");
        assert_eq!(list.footer.as_deref(), Some("Usa /evento rapporto id_rapporto:<ID> per il dettaglio."));
        assert_eq!(list.color, COLOR_STAFF);

        assert!(report_exists(&db, 1).unwrap());
        assert!(!report_exists(&db, 7).unwrap(), "un evento ancora attivo non è un report");
        assert_eq!(delete_report(&db, 7).unwrap(), 0);
        assert_eq!(delete_report(&db, 1).unwrap(), 1);
        assert_eq!(delete_report(&db, 1).unwrap(), 0);
        let left: i64 = db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM live_event_participants WHERE event_id=1", [], |r| r.get(0)).unwrap());
        assert_eq!(left, 0, "i dati individuali spariscono con il report");
    }

    #[test]
    fn interrupted_events_are_closed_like_python() {
        let golden = golden();
        let db = test_db();
        db.execute_batch(
            "
INSERT INTO live_events(id,guild_id,title,channel_id,channel_name,started_at,last_seen_at,peak_participants,participant_count) VALUES
 (1,1,'Interrotto A',555,'vocale','2027-04-01T20:00:00+02:00','2027-04-01T20:30:00+02:00',3,0),
 (2,1,'Interrotto B',555,'vocale','2027-04-01T21:00:00+02:00','2027-04-01T21:00:00+02:00',0,0),
 (3,1,'Concluso',555,'vocale','2027-03-01T20:00:00+01:00','2027-03-01T21:00:00+01:00',1,1);
UPDATE live_events SET ended_at='2027-03-01T21:00:00+01:00' WHERE id=3;
INSERT INTO live_event_participants(event_id,user_id,joined_at,participation_seconds,speaking_seconds) VALUES
 (1,1,'2027-04-01T20:05:00+02:00',10,1.5),(1,2,'2027-04-01T20:10:30.500000+02:00',0,0),(1,3,NULL,60,0),(1,4,'2027-04-01T20:45:00+02:00',0,0),
 (3,5,'2027-03-01T20:10:00+01:00',0,0);
",
        )
        .unwrap();
        recover_interrupted_events(&db).unwrap();
        assert_eq!(table(&db, "SELECT id,ended_at,participant_count,end_reason FROM live_events ORDER BY id"), golden["recover"]["events"]);
        assert_eq!(
            table(&db, "SELECT event_id,user_id,joined_at,participation_seconds,speaking_seconds FROM live_event_participants ORDER BY rowid"),
            golden["recover"]["participants"]
        );
    }

    fn at(hour: u32, minute: u32, second: u32) -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2027, 3, 5, hour, minute, second).unwrap()
    }

    fn evening_tables(db: &Database) -> Value {
        json!({
            "events": table(db, "SELECT id,guild_id,scheduled_event_id,title,channel_id,channel_name,started_at,last_seen_at,ended_at,peak_participants,participant_count,end_reason FROM live_events ORDER BY id"),
            "participants": table(db, "SELECT event_id,user_id,joined_at,participation_seconds,speaking_seconds FROM live_event_participants ORDER BY rowid"),
            "activity": table(db, "SELECT action,occurred_at FROM community_activity WHERE action='event_join' ORDER BY id"),
        })
    }

    fn expected_tables(step: &Value) -> Value {
        json!({"events": step["events"], "participants": step["participants"], "activity": step["activity"]})
    }

    #[test]
    fn a_whole_evening_writes_the_same_rows_as_python() {
        let golden = golden();
        let steps = golden["evening"].as_array().unwrap();
        let step = |name: &str| steps.iter().find(|s| s[0] == name).unwrap_or_else(|| panic!("passo {name}"));
        let db = test_db();

        // Avvio con 3 persone (il bot non conta).
        let event = NewEvent { guild_id: 1, scheduled_event_id: None, title: "🎮 Game Night", channel_id: 555, channel_name: "vocale", started_at: at(21, 0, 0) };
        let id = create_event_rows(&db, &event, &[10, 11, 12]).unwrap();
        assert_eq!(id, 1);
        let mut peak = 3;
        // 21:10:30 esce 11 (2 presenti); 21:20 entra 13; 21:25 entra 14; 21:30 rientra 11.
        peak = record_presence_change(&db, id, 11, true, false, at(21, 10, 30), 2, peak).unwrap();
        peak = record_presence_change(&db, id, 13, false, true, at(21, 20, 0), 3, peak).unwrap();
        peak = record_presence_change(&db, id, 14, false, true, at(21, 25, 0), 4, peak).unwrap();
        peak = record_presence_change(&db, id, 11, false, true, at(21, 30, 0), 5, peak).unwrap();
        assert_eq!(peak, step("dopo_presenze")[2].as_i64().unwrap());
        let presences = evening_tables(&db);
        // L'ID dell'evento nei golden dipende da quanti eventi c'erano prima: si confrontano le righe.
        assert_eq!(presences["participants"], step("dopo_presenze")[1]["participants"].clone());
        assert_eq!(presences["activity"], step("dopo_presenze")[1]["activity"]);

        // Ogni 15 secondi (qui alle 21:40) il parlato maturato si salva.
        let first = [(10, 125.5), (12, 30.25)];
        persist_progress(&db, id, &first, &[], at(21, 40, 0)).unwrap();
        assert_eq!(evening_tables(&db)["participants"], step("dopo_progresso")[1]["participants"].clone());
        assert_eq!(evening_tables(&db)["events"][0][7], step("dopo_progresso")[1]["events"][0][7], "ultimo istante visto");

        // Fine: 22:30:10, tre persone presenti, picco 5.
        let total = [(10, 140.0), (12, 30.25), (15, 3.0)];
        finish_event_rows(&db, id, &total, &first, peak, 3, at(22, 30, 10), "manuale").unwrap();
        let finished = step("fine");
        assert_eq!(evening_tables(&db)["participants"], finished[2]["participants"].clone());
        assert_eq!(evening_tables(&db)["events"], finished[2]["events"]);
        assert_eq!(evening_tables(&db)["activity"], finished[2]["activity"]);
        let report = build_event_report(&db, id).unwrap().unwrap();
        assert_eq!(normalized(embed_json(&report)), normalized(expected_embed(&finished[3])));
    }

    #[test]
    fn signed_percentages_round_like_python() {
        assert_eq!(signed_percent(0.2), "+20%");
        assert_eq!(signed_percent(-0.25), "-25%");
        assert_eq!(signed_percent(0.4), "+40%");
        assert_eq!(signed_percent(-1.0), "-100%");
    }

    #[test]
    fn python_int_accepts_what_int_accepts() {
        for (text, expected) in [("12", Some(12)), (" 5 ", Some(5)), ("+7", Some(7)), ("-3", Some(-3)), ("1_0", Some(10)), ("١٢", Some(12)), ("0x1", None), ("", None), ("_1", None), ("1_", None), ("1__0", None), ("a", None), ("--1", None), ("+", None)] {
            assert_eq!(python_int(text), expected, "{text:?}");
        }
    }
}