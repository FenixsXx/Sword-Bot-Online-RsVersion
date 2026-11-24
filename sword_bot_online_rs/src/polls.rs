//! Sondaggi: gestione dei voti, layout dei pulsanti e chiusura automatica.
//!
//! Questo file contiene la logica principale per far funzionare i sondaggi (polls) su Discord.
//! Gestisce le regole di voto (quante opzioni scegliere), l'interfaccia utente (pulsanti e menu)
//! e la logica temporale per chiudere i sondaggi quando scade il tempo.
//!
//! Le interazioni effettive con gli utenti Discord (clic sui pulsanti) sono gestite
//! in un altro modulo (`interactions`), mentre la pubblicazione del messaggio è
//! delegata al modulo `publisher`.
//!
//! Concetti chiave:
//! - `PollRow`: Rappresenta un singolo sondaggio salvato nel database.
//! - `PollComponent`: Un modo per "etichettare" i pulsanti, così il bot sa cosa
//!   succede quando un utente clicca su un pulsante specifico.
//! - `RefreshScheduler`: Un sistema intelligente per aggiornare i messaggi su Discord.
//!   Se arrivano molte richieste di aggiornamento vicine, le raggruppa per non intasare Discord.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::DateTime;
use chrono_tz::Tz;
use poise::serenity_prelude::{CreateActionRow, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption};
use rusqlite::{Connection, OptionalExtension, Row, params};
use tokio::task::JoinHandle;
use tracing::{debug, error};

use crate::config::REFRESH_DEBOUNCE_SECONDS;
use crate::database::{self, Connected, Database, DbResult};
use crate::queries::{PollOption, poll_options};
use crate::schedule::{self, weekly_reset_pending};
use crate::settings;
use crate::stats::{HiddenBadge, record_user_activity, unlock_hidden_badge};
use crate::ui::{ButtonKind, ViewButton, button_rows};

/// Costante: se un sondaggio ha fino a 10 opzioni, mostriamo un pulsante dedicato per ognuna.
/// Se ne ha di più, mostriamo un unico pulsante generico "Vota".
pub const BUTTON_VOTING_MAX_OPTIONS: usize = 10;
/// Discord impone un limite di 25 elementi (opzioni nel menu o pulsanti) per messaggio.
const MAX_OPTIONS_SHOWN: usize = 25;

/// Rappresenta una riga della tabella `polls` nel database SQLite.
/// Contiene tutti i dati descrittivi di un sondaggio, inclusi lo stato attivo/chiuso
/// e gli ID dei messaggi Discord collegati.
#[derive(Debug, Clone, PartialEq)]
pub struct PollRow {
    pub id: i64,
    pub name: String,
    pub emoji: Option<String>,
    pub color: Option<String>,
    pub position: Option<i64>,
    pub active: Option<i64>,              // 1 se il sondaggio è aperto e attivo, 0 o NULL se chiuso
    pub is_community: Option<i64>,
    pub message_id: Option<String>,       // ID del messaggio di Discord in cui appare il sondaggio
    pub thread_id: Option<String>,        // ID del thread di discussione collegato al sondaggio
    /// Scadenza del sondaggio espressa come timestamp Unix (secondi).
    /// Rimane valorizzata anche dopo la chiusura per indicare quando è scaduto.
    pub closes_at: Option<f64>,
}

impl PollRow {
    /// Funzione di utilità per mappare una riga grezza del database SQLite (`rusqlite::Row`)
    /// nella nostra struttura dati tipizzata `PollRow`.
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            emoji: row.get("emoji")?,
            color: row.get("color")?,
            position: row.get("position")?,
            active: row.get("active")?,
            is_community: row.get("is_community")?,
            message_id: row.get("message_id")?,
            thread_id: row.get("thread_id")?,
            closes_at: row.get("closes_at")?,
        })
    }

    /// Verifica se il sondaggio è attivo. Nel database SQLite il campo `active` può essere
    /// `NULL` oppure `0` (considerati come falso), mentre qualsiasi altro valore diverso da zero
    /// indica che il sondaggio è attivo.
    pub fn is_active(&self) -> bool {
        self.active.is_some_and(|flag| flag != 0)
    }
}

/// Cerca un sondaggio specifico nel database utilizzando il suo ID univoco.
/// Ritorna `Some(PollRow)` se trovato, altrimenti `None`.
pub fn find_poll(db: &impl Connected, poll_id: i64) -> DbResult<Option<PollRow>> {
    db.with_conn(|connection| {
        connection.query_row("SELECT * FROM polls WHERE id=?", [poll_id], PollRow::from_row).optional()
    })
}

/// Restituisce l'elenco di tutti i sondaggi attualmente attivi, ordinati per posizione e ID.
/// Accetta un limite opzionale sul numero di risultati da restituire.
pub fn list_active_polls(db: &impl Connected, limit: Option<i64>) -> DbResult<Vec<PollRow>> {
    db.with_conn(|connection| {
        connection
            .prepare("SELECT * FROM polls WHERE active=1 ORDER BY position, id LIMIT ?")?
            .query_map([limit.unwrap_or(-1)], PollRow::from_row)?
            .collect()
    })
}

/// Conta il numero di utenti unici che hanno espresso almeno un voto in un determinato sondaggio.
/// Sfrutta il costrutto SQL `COUNT(DISTINCT v.user_id)` per evitare di contare più volte lo stesso utente
/// se ha votato per più opzioni nello stesso sondaggio.
pub fn poll_voter_count(db: &impl Connected, poll_id: i64) -> DbResult<i64> {
    db.with_conn(|connection| {
        connection.query_row(
            "SELECT COUNT(DISTINCT v.user_id) FROM votes v
             JOIN options o ON o.id = v.option_id WHERE o.poll_id = ?",
            [poll_id],
            |row| row.get(0),
        )
    })
}

// --- layout ----------------------------------------------------------------------------------

/// Tronca una stringa in modo sicuro per non superare il numero massimo di caratteri consentito,
/// rispettando i confini dei caratteri Unicode (UTF-8).
fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Genera il `custom_id` univoco per il pulsante di voto di una specifica opzione.
/// Formato: `"vote:<ID opzione>"`.
pub fn vote_custom_id(option_id: i64) -> String {
    format!("vote:{option_id}")
}

/// Genera il `custom_id` per il pulsante "I miei voti" o "Vota" associato a un sondaggio.
/// Formato: `"myvotes:<ID sondaggio>"`.
pub fn my_votes_custom_id(poll_id: i64) -> String {
    format!("myvotes:{poll_id}")
}

/// Genera il `custom_id` per il menu a tendina (select menu) nel messaggio privato/effimero
/// "I miei voti" di un sondaggio.
pub fn my_votes_select_custom_id(poll_id: i64) -> String {
    format!("myvotes-select:{poll_id}")
}

/// Enum che rappresenta e interpreta il significato dei componenti interattivi di Discord (pulsanti o menu)
/// basandosi sul loro `custom_id`. Questo permette al bot di capire quale azione compiere
/// quando un utente interagisce con un elemento dell'interfaccia.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollComponent {
    /// Pulsante per votare o rimuovere il voto da una singola opzione (`vote:<ID opzione>`).
    Vote { option_id: i64 },
    /// Pulsante per aprire il pannello di gestione voti dell'utente (`myvotes:<ID sondaggio>`).
    MyVotes { poll_id: i64 },
    /// Selezione dal menu a tendina delle proprie preferenze (`myvotes-select:<ID sondaggio>`).
    MyVotesSelect { poll_id: i64 },
}

impl PollComponent {
    /// Analizza una stringa `custom_id` e la decodifica in un'azione specifica (`PollComponent`).
    /// Ritorna `None` se la stringa non appartiene a nessun componente noto dei sondaggi.
    /// Questa funzione è fondamentale per gestire correttamente i clic sugli elementi anche dopo
    /// un riavvio del bot, interpretando l'ID testuale salvato nell'interfaccia di Discord.
    pub fn parse(custom_id: &str) -> Option<Self> {
        let number = |text: &str| -> Option<i64> {
            (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())).then(|| text.parse().ok()).flatten()
        };
        if let Some(rest) = custom_id.strip_prefix("vote:") {
            return number(rest).map(|option_id| Self::Vote { option_id });
        }
        if let Some(rest) = custom_id.strip_prefix("myvotes-select:") {
            return number(rest).map(|poll_id| Self::MyVotesSelect { poll_id });
        }
        if let Some(rest) = custom_id.strip_prefix("myvotes:") {
            return number(rest).map(|poll_id| Self::MyVotes { poll_id });
        }
        None
    }
}

/// Costruisce la lista dei pulsanti interattivi da mostrare sotto al messaggio del sondaggio su Discord.
/// La strategia cambia in base al numero di opzioni:
/// - Se le opzioni sono poche (fino a `BUTTON_VOTING_MAX_OPTIONS`, ovvero 10), viene creato un pulsante
///   dedicato per ogni singola opzione, più un pulsante finale "I miei voti".
/// - Se le opzioni sono molte (oltre 10), i pulsanti singoli vengono omessi e viene mostrato
///   un unico pulsante centrale "Vota" per aprire il menu di selezione.
/// Se il sondaggio non è attivo, tutti i pulsanti vengono impostati come disabilitati (`disabled = true`).
pub fn poll_buttons(poll: &PollRow, options: &[PollOption]) -> Vec<ViewButton> {
    let options = &options[..options.len().min(MAX_OPTIONS_SHOWN)];
    let disabled = !poll.is_active();
    let mut buttons = Vec::new();
    let compact = options.len() <= BUTTON_VOTING_MAX_OPTIONS;
    if compact {
        for option in options {
            buttons.push(ViewButton {
                custom_id: vote_custom_id(option.id),
                label: truncate_chars(&option.label, 80),
                emoji: None,
                kind: ButtonKind::Secondary,
                disabled,
            });
        }
    }
    buttons.push(ViewButton {
        custom_id: my_votes_custom_id(poll.id),
        label: if compact { "I miei voti" } else { "Vota" }.to_string(),
        emoji: Some(if compact { "📋" } else { "🗳️" }),
        kind: ButtonKind::Primary,
        disabled: disabled || options.is_empty(),
    });
    buttons
}

/// Costruisce l'intera vista grafica del sondaggio (righe di pulsanti formattate per Discord)
/// leggendo le opzioni aggiornate direttamente dal database.
pub fn build_view(db: &impl Connected, poll: &PollRow) -> DbResult<Vec<CreateActionRow>> {
    let options = poll_options(db, poll.id)?;
    Ok(button_rows(&poll_buttons(poll, &options)))
}

/// Crea il menu a tendina (Select Menu) "I miei voti" per un utente specifico.
/// Questo menu mostra le opzioni disponibili nel sondaggio (fino a 25) e pre-seleziona
/// automaticamente quelle che l'utente ha già votato in precedenza.
pub fn my_votes_menu(db: &impl Connected, poll_id: i64, user_id: i64) -> DbResult<CreateSelectMenu> {
    let options: Vec<PollOption> = poll_options(db, poll_id)?.into_iter().take(MAX_OPTIONS_SHOWN).collect();
    let chosen: HashSet<i64> = db.with_conn(|connection| user_votes(connection, poll_id, user_id))?.into_iter().map(|vote| vote.0).collect();
    let max_votes = settings::max_votes(db)?;
    let menu_options: Vec<CreateSelectMenuOption> = options
        .iter()
        .map(|option| {
            let mut entry = CreateSelectMenuOption::new(truncate_chars(&option.label, 100), option.id.to_string())
                .default_selection(chosen.contains(&option.id));
            if let Some(tipo) = option.tipo.as_deref().filter(|tipo| !tipo.is_empty()) {
                entry = entry.description(tipo);
            }
            entry
        })
        .collect();
    let max_values = (max_votes.min(options.len() as i64)).max(1) as u8;
    Ok(CreateSelectMenu::new(my_votes_select_custom_id(poll_id), CreateSelectMenuKind::String { options: menu_options })
        .placeholder("Scegli le tue opzioni preferite")
        .min_values(0)
        .max_values(max_values))
}

// --- voti ------------------------------------------------------------------------------------

/// Interroga il database per recuperare tutti i voti espressi da un utente in un sondaggio specifico.
/// Restituisce un vettore di tuple `(ID opzione, etichetta dell'opzione)` ordinate per ID.
pub fn user_votes(connection: &Connection, poll_id: i64, user_id: i64) -> DbResult<Vec<(i64, String)>> {
    connection
        .prepare(
            "SELECT o.id, o.label FROM votes v JOIN options o ON o.id=v.option_id
             WHERE o.poll_id=? AND v.user_id=? ORDER BY o.id",
        )?
        .query_map([poll_id, user_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

/// Genera una stringa di riepilogo personale per l'utente, mostrando quali opzioni ha votato
/// e quanti voti ha ancora a disposizione (es. `🗳️ I tuoi voti: **A**, **B** · `0` voti rimasti`).
pub fn vote_summary(db: &impl Connected, poll_id: i64, user_id: i64) -> DbResult<String> {
    let votes = db.with_conn(|connection| user_votes(connection, poll_id, user_id))?;
    let remaining = (settings::max_votes(db)? - votes.len() as i64).max(0);
    let chosen = if votes.is_empty() {
        "nessuno".to_string()
    } else {
        votes.iter().map(|(_, label)| format!("**{label}**")).collect::<Vec<_>>().join(", ")
    };
    let unit = if remaining == 1 { "voto rimasto" } else { "voti rimasti" };
    Ok(format!("🗳️ I tuoi voti: {chosen} · `{remaining}` {unit}"))
}

/// Tiene traccia delle volte in cui un utente cambia i propri voti all'interno di un sondaggio.
/// Se un utente cambia idea più di 5 volte nello stesso sondaggio, sblocca automaticamente
/// il badge nascosto "Indeciso" (`HiddenBadge::Indecisive`).
fn count_vote_change(connection: &Connection, poll_id: i64, user_id: i64, now: DateTime<Tz>) -> DbResult<()> {
    connection.execute(
        "INSERT INTO vote_change_counts(poll_id,user_id,change_count)
         VALUES(?,?,1)
         ON CONFLICT(poll_id,user_id)
         DO UPDATE SET change_count=vote_change_counts.change_count+1",
        [poll_id, user_id],
    )?;
    let changes: i64 = connection.query_row(
        "SELECT change_count FROM vote_change_counts WHERE poll_id=? AND user_id=?",
        [poll_id, user_id],
        |row| row.get(0),
    )?;
    if changes > 5 {
        unlock_hidden_badge(connection, user_id, HiddenBadge::Indecisive, now)?;
    }
    Ok(())
}

/// Registra l'attività di voto dell'utente per il calcolo delle statistiche e delle streak della community.
/// Un voto conta come attività al massimo una volta al giorno per sondaggio: continuare a mettere
/// e togliere voti ripetutamente nello stesso giorno non gonfia artificialmente le statistiche.
fn record_vote_activity(connection: &Connection, poll_id: i64, user_id: i64, now: DateTime<Tz>) -> DbResult<()> {
    connection.execute("INSERT INTO community_activity(action,occurred_at) VALUES('vote',?)", [now.timestamp()])?;
    let first_today = connection.execute(
        "INSERT OR IGNORE INTO daily_vote_activity(user_id,poll_id,day) VALUES(?,?,?)",
        params![user_id, poll_id, now.date_naive().to_string()],
    )?;
    if first_today > 0 {
        let week = database::current_week(connection)?;
        record_user_activity(connection, user_id, week, 0, now)?;
    }
    Ok(())
}

/// Rappresenta l'esito dell'operazione di toggle (aggiunta/rimozione) di un voto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToggleResult {
    /// Indica se lo stato del voto è effettivamente cambiato.
    pub changed: bool,
    /// Messaggio descrittivo da mostrare all'utente in risposta.
    pub message: String,
    pub poll_id: Option<i64>,
}

impl ToggleResult {
    fn unchanged(message: impl Into<String>, poll_id: Option<i64>) -> Self {
        Self { changed: false, message: message.into(), poll_id }
    }
}

/// Aggiunge o rimuove il voto di un utente per una specifica opzione (funzionamento a interruttore/toggle).
/// Esegue controlli di sicurezza: verifica che il sondaggio esista, che sia attivo,
/// e che l'utente non abbia superato il numero massimo di voti consentiti.
pub fn toggle_vote(db: &Database, user_id: i64, option_id: i64, now: DateTime<Tz>) -> DbResult<ToggleResult> {
    db.transaction(|tx| -> DbResult<ToggleResult> {
        let option: Option<(i64, Option<i64>)> = tx
            .query_row(
                "SELECT o.poll_id, p.active FROM options o
                 JOIN polls p ON p.id = o.poll_id WHERE o.id=?",
                [option_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((poll_id, active)) = option else {
            return Ok(ToggleResult::unchanged("❌ Questa opzione non esiste più.", None));
        };
        if !active.is_some_and(|flag| flag != 0) {
            return Ok(ToggleResult::unchanged("⚠️ Questo sondaggio non è attivo.", Some(poll_id)));
        }
        let already = tx
            .query_row("SELECT 1 FROM votes WHERE option_id=? AND user_id=?", [option_id, user_id], |_| Ok(()))
            .optional()?
            .is_some();
        if already {
            tx.execute("DELETE FROM votes WHERE option_id=? AND user_id=?", [option_id, user_id])?;
            count_vote_change(tx, poll_id, user_id, now)?;
            return Ok(ToggleResult { changed: true, message: "🗑️ Voto rimosso.".into(), poll_id: Some(poll_id) });
        }
        let max_votes = settings::max_votes(tx)?;
        if user_votes(tx, poll_id, user_id)?.len() as i64 >= max_votes {
            return Ok(ToggleResult::unchanged(
                format!("⚠️ Hai già usato i tuoi {max_votes} voti: togline uno o usa **📋 I miei voti** per cambiarli."),
                Some(poll_id),
            ));
        }
        tx.execute("INSERT INTO votes(option_id,user_id) VALUES(?,?)", [option_id, user_id])?;
        record_vote_activity(tx, poll_id, user_id, now)?;
        Ok(ToggleResult { changed: true, message: "✅ Voto registrato.".into(), poll_id: Some(poll_id) })
    })
}

/// Sostituisce in blocco i voti di un utente in un sondaggio con una nuova lista di opzioni scelte
/// (tipicamente inviata tramite il menu a tendina "I miei voti").
/// Ritorna una tupla `(bool, String)` indicando se ci sono stati cambiamenti e il messaggio di esito.
pub fn set_user_votes(
    db: &Database,
    user_id: i64,
    poll_id: i64,
    option_ids: &[i64],
    now: DateTime<Tz>,
) -> DbResult<(bool, String)> {
    // I doppioni nell'elenco vengono ignorati, mentre l'ordine di scelta viene preservato.
    let mut seen = HashSet::new();
    let wanted: Vec<i64> = option_ids.iter().copied().filter(|id| seen.insert(*id)).collect();
    db.transaction(|tx| -> DbResult<(bool, String)> {
        let poll: Option<Option<i64>> = tx
            .query_row("SELECT active FROM polls WHERE id=?", [poll_id], |row| row.get(0))
            .optional()?;
        let Some(active) = poll else {
            return Ok((false, "❌ Questo sondaggio non esiste più.".into()));
        };
        if !active.is_some_and(|flag| flag != 0) {
            return Ok((false, "⚠️ Questo sondaggio non è attivo.".into()));
        }
        let max_votes = settings::max_votes(tx)?;
        if wanted.len() as i64 > max_votes {
            return Ok((false, format!("⚠️ Puoi scegliere al massimo {max_votes} opzioni.")));
        }
        let valid: HashSet<i64> = tx
            .prepare("SELECT id FROM options WHERE poll_id=?")?
            .query_map([poll_id], |row| row.get(0))?
            .collect::<DbResult<_>>()?;
        if !wanted.iter().all(|id| valid.contains(id)) {
            return Ok((false, "❌ Alcune opzioni non esistono più: riapri il menu.".into()));
        }
        let current: HashSet<i64> = user_votes(tx, poll_id, user_id)?.into_iter().map(|vote| vote.0).collect();
        let wanted_set: HashSet<i64> = wanted.iter().copied().collect();
        let mut removed: Vec<i64> = current.difference(&wanted_set).copied().collect();
        removed.sort_unstable();
        let added: Vec<i64> = wanted.iter().copied().filter(|id| !current.contains(id)).collect();
        if removed.is_empty() && added.is_empty() {
            return Ok((false, "ℹ️ Nessuna modifica ai tuoi voti.".into()));
        }
        for option_id in &removed {
            tx.execute("DELETE FROM votes WHERE option_id=? AND user_id=?", [*option_id, user_id])?;
        }
        if !removed.is_empty() {
            count_vote_change(tx, poll_id, user_id, now)?;
        }
        for option_id in &added {
            tx.execute("INSERT INTO votes(option_id,user_id) VALUES(?,?)", [*option_id, user_id])?;
        }
        if !added.is_empty() {
            record_vote_activity(tx, poll_id, user_id, now)?;
        }
        Ok((true, "✅ Voti aggiornati.".into()))
    })
}

// --- scadenze --------------------------------------------------------------------------------

/// Controlla e chiude automaticamente tutti i sondaggi la cui scadenza (`closes_at`) è superata.
/// Restituisce l'elenco delle righe dei sondaggi appena chiusi, in modo che il chiamante possa
/// aggiornare i messaggi corrispondenti su Discord e sulla dashboard.
///
/// Nota bene: se è in corso un reset settimanale (`weekly_reset_pending`), i sondaggi vengono
/// lasciati aperti temporaneamente in attesa che il sistema calcoli i vincitori, per poi riaprirli
/// per la nuova settimana. Il campo `closes_at` viene preservato per indicare la chiusura.
pub fn close_expired_polls(db: &Database, now: DateTime<Tz>) -> DbResult<Vec<PollRow>> {
    if weekly_reset_pending(db, now)? {
        return Ok(Vec::new());
    }
    let now_ts = now.timestamp_millis() as f64 / 1000.0;
    let expired_ids: Vec<i64> = db.with_conn(|connection| -> DbResult<Vec<i64>> {
        connection
            .prepare("SELECT id FROM polls WHERE active=1 AND closes_at IS NOT NULL AND closes_at<=?")?
            .query_map([now_ts], |row| row.get(0))?
            .collect()
    })?;
    let mut closed = Vec::new();
    for poll_id in expired_ids {
        let updated = db.transaction(|tx| -> DbResult<usize> {
            Ok(tx.execute(
                "UPDATE polls SET active=0
                 WHERE id=? AND active=1 AND closes_at IS NOT NULL AND closes_at<=?",
                params![poll_id, now_ts],
            )?)
        });
        match updated {
            Ok(0) => continue,
            Ok(_) => {}
            Err(e) => {
                error!("Impossibile chiudere automaticamente il sondaggio {poll_id}: {e}");
                continue;
            }
        }
        if let Some(poll)        = find_poll(db, poll_id)? {
            closed.push(poll);
        }
    }
    Ok(closed)
}

/// Assegna una scadenza predefinita (il prossimo reset settimanale) ai sondaggi attivi
/// che ne sono sprovvisti (`closes_at IS NULL`).
pub fn ensure_closes_at(db: &Database, poll_id: i64, now: DateTime<Tz>) -> DbResult<()> {
    let closing_time = schedule::poll_closing_time(db, now)?;
    db.transaction(|tx| -> DbResult<()> {
        tx.execute(
            "UPDATE polls SET closes_at=? WHERE id=? AND active=1 AND closes_at IS NULL",
            params![closing_time, poll_id],
        )?;
        Ok(())
    })
}

// --- messaggio nel canale: claim, ID messaggio e thread --------------------------------------

/// Durata temporale del "claim" (60 secondi): quando un processo del bot sta pubblicando
/// o aggiornando il messaggio di un sondaggio, inserisce un blocco temporaneo (claim) nel database.
/// Questo evita che due processi concorrenti pubblichino lo stesso sondaggio due volte per errore.
pub const MESSAGE_CLAIM_SECONDS: f64 = 60.0;

/// Tenta di prenotare (fare il "claim") la creazione del messaggio per un sondaggio.
/// Restituisce `true` se la prenotazione è riuscita, oppure `false` se un altro processo
/// ha già prenotato la creazione e il blocco non è ancora scaduto.
pub fn try_claim_poll_message(db: &Database, poll_id: i64, now_ts: f64) -> DbResult<bool> {
    db.transaction(|tx| -> DbResult<bool> {
        let claimed_until: Option<f64> = tx
            .query_row("SELECT expires_at FROM poll_message_claims WHERE poll_id=?", [poll_id], |row| row.get(0))
            .optional()?;
        if claimed_until.is_some_and(|expires_at| expires_at > now_ts) {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO poll_message_claims(poll_id, expires_at) VALUES(?, ?)
             ON CONFLICT(poll_id) DO UPDATE SET expires_at=excluded.expires_at",
            params![poll_id, now_ts + MESSAGE_CLAIM_SECONDS],
        )?;
        Ok(true)
    })
}

/// Rilascia anticipatamente il claim sul messaggio del sondaggio.
pub fn release_poll_message_claim(db: &Database, poll_id: i64) -> DbResult<()> {
    db.execute("DELETE FROM poll_message_claims WHERE poll_id=?", [poll_id])?;
    Ok(())
}

/// Salva nel database l'ID del messaggio Discord associato a un sondaggio.
pub fn set_poll_message_id(db: &Database, poll_id: i64, message_id: u64) -> DbResult<()> {
    db.execute("UPDATE polls SET message_id=? WHERE id=?", params![message_id.to_string(), poll_id])?;
    Ok(())
}

/// Salva nel database l'ID del thread di discussione associato a un sondaggio.
pub fn set_poll_thread_id(db: &Database, poll_id: i64, thread_id: u64) -> DbResult<()> {
    db.execute("UPDATE polls SET thread_id=? WHERE id=?", params![thread_id.to_string(), poll_id])?;
    Ok(())
}

// --- refresh differito -----------------------------------------------------------------------

/// Sistema di "debouncing" (aggiornamento differito). Raggruppa molteplici richieste di aggiornamento
/// ravvicinate nel tempo per lo stesso elemento (ad esempio un sondaggio che riceve molti voti in pochi secondi)
/// in un'unica esecuzione effettiva dopo che è trascorso un certo intervallo (`REFRESH_DEBOUNCE_SECONDS`).
/// In questo modo si evitano chiamate superflue e sovraccarichi verso le API di Discord.
#[derive(Clone, Default)]
pub struct RefreshScheduler {
    pending: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
}

impl RefreshScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, JoinHandle<()>>> {
        self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Pianifica l'esecuzione di una funzione di aggiornamento (`refresh`) associata a una determinata chiave (`key`),
    /// a meno che non ce ne sia già una in attesa. Se non è attivo un runtime asincrono di Tokio, non fa nulla
    /// e restituisce `false`.
    pub fn schedule<F, Fut, E>(&self, key: impl Into<String>, refresh: F) -> bool
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: Display,
    {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            debug!("Aggiornamento differito non pianificato: runtime tokio non attivo.");
            return false;
        };
        let key = key.into();
        let mut pending = self.pending();
        if pending.get(&key).is_some_and(|task| !task.is_finished()) {
            return true;
        }
        let this = self.clone();
        let task_key = key.clone();
        let task = runtime.spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(REFRESH_DEBOUNCE_SECONDS)).await;
            this.pending().remove(&task_key);
            if let Err(e) = refresh().await {
                error!("Aggiornamento differito {task_key} non riuscito: {e}");
            }
        });
        pending.insert(key, task);
        true
    }

    /// Metodo di comodo per pianificare l'aggiornamento specifico di un sondaggio (`poll_id`).
    pub fn schedule_poll_refresh<F, Fut, E>(&self, poll_id: i64, refresh: F) -> bool
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: Display,
    {
        self.schedule(format!("poll:{poll_id}"), refresh)
    }

    /// Annulla immediatamente tutti gli aggiornamenti attualmente in sospeso.
    pub fn cancel_all(&self) {
        for (_, task) in self.pending().drain() {
            task.abort();
        }
    }

    /// Restituisce il numero di aggiornamenti attualmente in attesa di essere eseguiti.
    pub fn pending_count(&self) -> usize {
        self.pending().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use chrono::{Duration as ChronoDuration, TimeZone};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn now() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()
    }

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn count(db: &Database, sql: &str) -> i64 {
        db.with_conn(|c| c.query_row(sql, [], |row| row.get(0)).unwrap())
    }

    /// Sondaggio di prova con le opzioni indicate: `(ID sondaggio, ID opzioni)`.
    fn poll_with_options(db: &Database, labels: &[&str]) -> (i64, Vec<i64>) {
        let existing = count(db, "SELECT COUNT(*) FROM polls");
        db.execute(
            "INSERT INTO polls(name, active, closes_at) VALUES(?, 1, ?)",
            params![format!("Test {existing}"), now().timestamp() as f64 + 3600.0],
        )
        .unwrap();
        let poll_id = db.with_conn(|c| c.last_insert_rowid());
        let ids = labels
            .iter()
            .map(|label| {
                db.execute("INSERT INTO options(poll_id,label) VALUES(?,?)", params![poll_id, label]).unwrap();
                db.with_conn(|c| c.last_insert_rowid())
            })
            .collect();
        (poll_id, ids)
    }

    // --- voti ------------------------------------------------------------------------------

    #[test]
    fn set_user_votes_replaces_votes_and_respects_the_limit() {
        // Stesso caso di tests/test_features.py::test_set_user_votes_replaces_votes_and_respects_the_limit.
        let db = db();
        let (poll_id, ids) = poll_with_options(&db, &["A", "B", "C"]);
        assert_eq!(set_user_votes(&db, 5, poll_id, &[ids[0], ids[1]], now()).unwrap(), (true, "✅ Voti aggiornati.".into()));
        assert!(set_user_votes(&db, 5, poll_id, &[ids[1], ids[2]], now()).unwrap().0);
        let labels: Vec<String> = db.with_conn(|c| user_votes(c, poll_id, 5).unwrap()).into_iter().map(|v| v.1).collect();
        assert_eq!(labels, ["B", "C"]);
        let too_many = set_user_votes(&db, 5, poll_id, &ids, now()).unwrap();
        assert_eq!(too_many, (false, "⚠️ Puoi scegliere al massimo 2 opzioni.".into()));
        assert!(vote_summary(&db, poll_id, 5).unwrap().contains("I tuoi voti"));
    }

    #[test]
    fn set_user_votes_reports_invalid_requests() {
        let db = db();
        let (poll_id, ids) = poll_with_options(&db, &["A", "B"]);
        assert_eq!(set_user_votes(&db, 5, 999, &[1], now()).unwrap().1, "❌ Questo sondaggio non esiste più.");
        assert_eq!(set_user_votes(&db, 5, poll_id, &[ids[0], 9999], now()).unwrap().1, "❌ Alcune opzioni non esistono più: riapri il menu.");
        assert_eq!(set_user_votes(&db, 5, poll_id, &[], now()).unwrap(), (false, "ℹ️ Nessuna modifica ai tuoi voti.".into()));
        set_user_votes(&db, 5, poll_id, &[ids[0]], now()).unwrap();
        assert_eq!(set_user_votes(&db, 5, poll_id, &[ids[0]], now()).unwrap().1, "ℹ️ Nessuna modifica ai tuoi voti.");
        // Doppioni nell'elenco non contano come voti in più.
        assert!(set_user_votes(&db, 6, poll_id, &[ids[0], ids[0], ids[1]], now()).unwrap().0);
        // Un sondaggio chiuso non accetta voti.
        db.execute("UPDATE polls SET active=0 WHERE id=?", [poll_id]).unwrap();
        assert_eq!(set_user_votes(&db, 7, poll_id, &[ids[0]], now()).unwrap().1, "⚠️ Questo sondaggio non è attivo.");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM votes WHERE user_id=7"), 0);
    }

    #[test]
    fn configurable_vote_limit() {
        let db = db();
        let (poll_id, ids) = poll_with_options(&db, &["A", "B", "C"]);
        db.set_setting("max_votes", 3).unwrap();
        assert!(set_user_votes(&db, 8, poll_id, &ids, now()).unwrap().0);
    }

    #[test]
    fn toggle_adds_removes_and_enforces_the_limit() {
        let db = db();
        let (poll_id, ids) = poll_with_options(&db, &["A", "B", "C"]);
        let toggle = |user: i64, option: i64| toggle_vote(&db, user, option, now()).unwrap();
        assert_eq!(toggle(1, ids[0]), ToggleResult { changed: true, message: "✅ Voto registrato.".into(), poll_id: Some(poll_id) });
        assert!(toggle(1, ids[1]).changed);
        let full = toggle(1, ids[2]);
        assert!(!full.changed);
        assert_eq!(full.message, "⚠️ Hai già usato i tuoi 2 voti: togline uno o usa **📋 I miei voti** per cambiarli.");
        let removed = toggle(1, ids[0]);
        assert_eq!((removed.changed, removed.message.as_str()), (true, "🗑️ Voto rimosso."));
        assert!(toggle(1, ids[2]).changed, "dopo averne tolto uno c'è di nuovo posto");
        assert_eq!(toggle(1, 9999), ToggleResult { changed: false, message: "❌ Questa opzione non esiste più.".into(), poll_id: None });
        db.execute("UPDATE polls SET active=0", []).unwrap();
        assert_eq!(toggle(2, ids[0]), ToggleResult { changed: false, message: "⚠️ Questo sondaggio non è attivo.".into(), poll_id: Some(poll_id) });
    }

    #[test]
    fn vote_activity_counts_once_per_day_and_not_on_removal() {
        // Stesso caso di tests/test_features.py::test_vote_activity_counts_once_per_day_and_not_on_removal.
        let db = db();
        let (_, ids) = poll_with_options(&db, &["A", "B"]);
        for _ in 0..3 {
            toggle_vote(&db, 7, ids[0], now()).unwrap(); // aggiunge, toglie, aggiunge
        }
        assert_eq!(count(&db, "SELECT activity_count FROM weekly_activity WHERE user_id=7 AND week=1"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM community_activity WHERE action='vote'"), 2, "ogni voto aggiunto è registrato");
        // Il giorno dopo il voto conta di nuovo come attività.
        let tomorrow = now() + ChronoDuration::days(1);
        toggle_vote(&db, 7, ids[0], tomorrow).unwrap(); // toglie
        toggle_vote(&db, 7, ids[0], tomorrow).unwrap(); // aggiunge
        assert_eq!(count(&db, "SELECT activity_count FROM weekly_activity WHERE user_id=7 AND week=1"), 2);
    }

    #[test]
    fn changing_votes_often_unlocks_the_indecisive_badge() {
        let db = db();
        let (_, ids) = poll_with_options(&db, &["A"]);
        let badges = |db: &Database| count(db, "SELECT COUNT(*) FROM user_hidden_badges WHERE user_id=9 AND badge_key='indecisive'");
        toggle_vote(&db, 9, ids[0], now()).unwrap(); // voto: non è una modifica
        for removal in 1..=7 {
            toggle_vote(&db, 9, ids[0], now()).unwrap(); // toglie (modifica `removal`)
            assert_eq!(badges(&db), i64::from(removal > 5), "dopo {removal} rimozioni");
            toggle_vote(&db, 9, ids[0], now()).unwrap(); // rimette
        }
        assert_eq!(count(&db, "SELECT change_count FROM vote_change_counts WHERE user_id=9"), 7);
    }

    #[test]
    fn vote_summary_pluralises_and_never_goes_negative() {
        let db = db();
        let (poll_id, ids) = poll_with_options(&db, &["A", "B"]);
        assert_eq!(vote_summary(&db, poll_id, 1).unwrap(), "🗳️ I tuoi voti: nessuno · `2` voti rimasti");
        toggle_vote(&db, 1, ids[0], now()).unwrap();
        assert_eq!(vote_summary(&db, poll_id, 1).unwrap(), "🗳️ I tuoi voti: **A** · `1` voto rimasto");
        toggle_vote(&db, 1, ids[1], now()).unwrap();
        assert_eq!(vote_summary(&db, poll_id, 1).unwrap(), "🗳️ I tuoi voti: **A**, **B** · `0` voti rimasti");
        db.set_setting("max_votes", 1).unwrap(); // limite abbassato dopo i voti
        assert!(vote_summary(&db, poll_id, 1).unwrap().ends_with("`0` voti rimasti"));
    }

    // --- scadenze --------------------------------------------------------------------------

    #[test]
    fn expired_poll_is_closed_and_keeps_its_deadline() {
        // Stesso caso di tests/test_review_and_poll_expiration.py.
        let db = db();
        db.execute("INSERT INTO polls(name, active, closes_at) VALUES('Expired poll', 1, ?)", [now().timestamp() as f64 - 1.0]).unwrap();
        let poll_id = db.with_conn(|c| c.last_insert_rowid());
        db.execute("INSERT INTO options(poll_id, label) VALUES(?, 'Option')", [poll_id]).unwrap();

        let closed = close_expired_polls(&db, now()).unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!((closed[0].id, closed[0].is_active()), (poll_id, false));
        assert!(closed[0].closes_at.is_some(), "closes_at resta: segna «chiuso per scadenza»");
        let poll = find_poll(&db, poll_id).unwrap().unwrap();
        let buttons = poll_buttons(&poll, &poll_options(&db, poll_id).unwrap());
        assert!(buttons[0].disabled, "il pulsante dell'opzione è disabilitato");
        assert!(close_expired_polls(&db, now()).unwrap().is_empty(), "già chiuso: nulla da fare");
    }

    #[test]
    fn polls_without_deadline_staff_closed_or_in_the_future_are_left_alone() {
        let db = db();
        let ts = now().timestamp() as f64;
        db.execute_batch(&format!(
            "INSERT INTO polls(id,name,active,closes_at) VALUES
                (1,'senza scadenza',1,NULL),(2,'futuro',1,{}),(3,'chiuso dallo staff',0,{}),(4,'scaduto',1,{});",
            ts + 60.0,
            ts - 60.0,
            ts - 60.0
        ))
        .unwrap();
        let closed: Vec<i64> = close_expired_polls(&db, now()).unwrap().iter().map(|poll| poll.id).collect();
        assert_eq!(closed, [4]);
        assert_eq!(count(&db, "SELECT active FROM polls WHERE id=2"), 1);
    }

    #[test]
    fn polls_stay_open_while_a_reset_is_pending() {
        // Stesso caso di tests/test_weekly_cycle.py::test_polls_are_not_closed_while_a_reset_is_pending.
        let db = db();
        db.execute("INSERT INTO polls(name, active, closes_at) VALUES('Pending reset', 1, ?)", [now().timestamp() as f64 - 60.0]).unwrap();
        let last_scheduled = schedule::last_scheduled_reset(&db, now()).unwrap();
        let last_week = last_scheduled - ChronoDuration::days(7);
        db.set_setting("last_reset", schedule::isoformat(last_week)).unwrap();
        assert!(weekly_reset_pending(&db, now()).unwrap());
        assert!(close_expired_polls(&db, now()).unwrap().is_empty());
        assert_eq!(count(&db, "SELECT active FROM polls"), 1);
        // Reset fatto: ora il sondaggio può chiudersi.
        db.set_setting("last_reset", schedule::isoformat(last_scheduled)).unwrap();
        assert_eq!(close_expired_polls(&db, now()).unwrap().len(), 1);
    }

    #[test]
    fn ensure_closes_at_only_fills_missing_deadlines_of_active_polls() {
        let db = db();
        db.execute_batch("INSERT INTO polls(id,name,active,closes_at) VALUES (1,'a',1,NULL),(2,'b',1,123),(3,'c',0,NULL);").unwrap();
        for id in 1..=3 {
            ensure_closes_at(&db, id, now()).unwrap();
        }
        let next_reset = TZ.with_ymd_and_hms(2026, 11, 16, 12, 0, 0).unwrap().timestamp() as f64;
        let closes_at = |id: i64| -> Option<f64> {
            db.with_conn(|c| c.query_row("SELECT closes_at FROM polls WHERE id=?", [id], |r| r.get(0)).unwrap())
        };
        assert_eq!(closes_at(1), Some(next_reset));
        assert_eq!(closes_at(2), Some(123.0));
        assert_eq!(closes_at(3), None, "i sondaggi chiusi non ricevono una scadenza");
    }

    // --- claim e ID dei messaggi -----------------------------------------------------------

    #[test]
    fn message_claim_blocks_others_until_it_expires() {
        let db = db();
        db.execute("INSERT INTO polls(id,name) VALUES(1,'p')", []).unwrap();
        assert!(try_claim_poll_message(&db, 1, 1000.0).unwrap());
        assert!(!try_claim_poll_message(&db, 1, 1030.0).unwrap(), "claim ancora valido");
        assert!(try_claim_poll_message(&db, 1, 1061.0).unwrap(), "claim scaduto: si può riprendere");
        release_poll_message_claim(&db, 1).unwrap();
        assert!(try_claim_poll_message(&db, 1, 1062.0).unwrap(), "rilasciato: subito disponibile");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM poll_message_claims"), 1);
    }

    #[test]
    fn message_and_thread_ids_are_stored_as_text() {
        let db = db();
        db.execute("INSERT INTO polls(id,name) VALUES(1,'p')", []).unwrap();
        set_poll_message_id(&db, 1, 1_234_567_890_123_456_789).unwrap();
        set_poll_thread_id(&db, 1, 987).unwrap();
        let poll = find_poll(&db, 1).unwrap().unwrap();
        assert_eq!((poll.message_id.as_deref(), poll.thread_id.as_deref()), (Some("1234567890123456789"), Some("987")));
        assert_eq!(find_poll(&db, 99).unwrap(), None);
    }

    // --- layout e custom_id ----------------------------------------------------------------

    fn poll_row(id: i64, active: i64) -> PollRow {
        PollRow {
            id,
            name: "Test".into(),
            emoji: None,
            color: None,
            position: None,
            active: Some(active),
            is_community: None,
            message_id: None,
            thread_id: None,
            closes_at: None,
        }
    }

    fn option(id: i64, label: &str) -> PollOption {
        PollOption { id, label: label.into(), suggestion_id: None, tipo: None, link: None, n: 0 }
    }

    #[test]
    fn few_options_get_one_button_each_plus_my_votes() {
        let options = [option(10, "A"), option(11, &"x".repeat(100))];
        let buttons = poll_buttons(&poll_row(3, 1), &options);
        assert_eq!(buttons.len(), 3);
        assert_eq!((buttons[0].custom_id.as_str(), buttons[0].label.as_str(), buttons[0].kind), ("vote:10", "A", ButtonKind::Secondary));
        assert_eq!(buttons[1].label.chars().count(), 80, "etichette troncate a 80");
        let my_votes = &buttons[2];
        assert_eq!((my_votes.custom_id.as_str(), my_votes.label.as_str(), my_votes.emoji, my_votes.kind), ("myvotes:3", "I miei voti", Some("📋"), ButtonKind::Primary));
        assert!(buttons.iter().all(|b| !b.disabled));
    }

    #[test]
    fn many_options_collapse_into_a_single_vote_button() {
        let options: Vec<PollOption> = (1..=11).map(|id| option(id, "O")).collect();
        let buttons = poll_buttons(&poll_row(3, 1), &options);
        assert_eq!(buttons.len(), 1);
        assert_eq!((buttons[0].label.as_str(), buttons[0].emoji), ("Vota", Some("🗳️")));
        // Il confine: 10 opzioni hanno ancora un pulsante ciascuna.
        assert_eq!(poll_buttons(&poll_row(3, 1), &options[..10]).len(), 11);
    }

    #[test]
    fn closed_polls_and_empty_polls_disable_their_buttons() {
        let options = [option(10, "A")];
        assert!(poll_buttons(&poll_row(3, 0), &options).iter().all(|b| b.disabled));
        let empty = poll_buttons(&poll_row(3, 1), &[]);
        assert_eq!(empty.len(), 1);
        assert!(empty[0].disabled, "senza opzioni «I miei voti» è disabilitato");
        let mut null_active = poll_row(3, 1);
        null_active.active = None;
        assert!(poll_buttons(&null_active, &options).iter().all(|b| b.disabled), "active NULL = non attivo");
    }

    #[test]
    fn view_is_built_from_the_database_in_rows_of_five() {
        let db = db();
        let (poll_id, _) = poll_with_options(&db, &["1", "2", "3", "4", "5", "6", "7"]);
        let poll = find_poll(&db, poll_id).unwrap().unwrap();
        let rows = serde_json::to_value(build_view(&db, &poll).unwrap()).unwrap();
        let sizes: Vec<usize> = rows.as_array().unwrap().iter().map(|row| row["components"].as_array().unwrap().len()).collect();
        assert_eq!(sizes, [5, 3], "7 opzioni + «I miei voti» = 8 pulsanti");
    }

    #[test]
    fn custom_ids_round_trip_and_reject_garbage() {
        assert_eq!(PollComponent::parse(&vote_custom_id(42)), Some(PollComponent::Vote { option_id: 42 }));
        assert_eq!(PollComponent::parse(&my_votes_custom_id(7)), Some(PollComponent::MyVotes { poll_id: 7 }));
        assert_eq!(PollComponent::parse(&my_votes_select_custom_id(7)), Some(PollComponent::MyVotesSelect { poll_id: 7 }));
        for garbage in ["vote:", "vote:abc", "vote:12x", "vote:-1", "vote:+3", "vote: 3", "vote:1:2", "myvotes:", "review:approve:1", "", "VOTE:1", "vote:99999999999999999999", "vote:\u{663}"] {
            assert_eq!(PollComponent::parse(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn my_votes_menu_preselects_votes_and_limits_choices() {
        let db = db();
        let (poll_id, ids) = poll_with_options(&db, &["A", "B", "C"]);
        db.execute("UPDATE options SET suggestion_id=NULL", []).unwrap();
        db.execute("INSERT INTO suggestions(id,week,norm_name,name,tipo) VALUES(1,1,'x','X','Gioco')", []).unwrap();
        db.execute("UPDATE options SET suggestion_id=1 WHERE id=?", [ids[1]]).unwrap();
        toggle_vote(&db, 5, ids[0], now()).unwrap();
        let menu = serde_json::to_value(my_votes_menu(&db, poll_id, 5).unwrap()).unwrap();
        assert_eq!(menu["custom_id"], my_votes_select_custom_id(poll_id));
        assert_eq!(menu["placeholder"], "Scegli le tue opzioni preferite");
        assert_eq!((menu["min_values"].as_u64(), menu["max_values"].as_u64()), (Some(0), Some(2)), "max = limite voti (2) con 3 opzioni");
        let options = menu["options"].as_array().unwrap();
        assert_eq!(options.len(), 3);
        assert_eq!((options[0]["label"].as_str(), options[0]["value"].as_str(), options[0]["default"].as_bool()), (Some("A"), Some(ids[0].to_string().as_str()), Some(true)));
        assert_eq!(options[1]["default"], false);
        assert_eq!(options[1]["description"], "Gioco", "il tipo della proposta collegata");
        assert!(options[0]["description"].is_null());
        // Con una sola opzione il massimo scende a 1; con il limite a 5 resta il numero di opzioni.
        let (single, _) = poll_with_options(&db, &["Solo"]);
        assert_eq!(serde_json::to_value(my_votes_menu(&db, single, 5).unwrap()).unwrap()["max_values"], 1);
        db.set_setting("max_votes", 5).unwrap();
        assert_eq!(serde_json::to_value(my_votes_menu(&db, poll_id, 5).unwrap()).unwrap()["max_values"], 3);
    }

    // --- refresh differito -----------------------------------------------------------------

    fn counting_refresh(count: &Arc<AtomicUsize>) -> impl FnOnce() -> std::future::Ready<Result<(), String>> + Send + 'static {
        let count = Arc::clone(count);
        move || {
            count.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(()))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn close_refreshes_of_the_same_key_run_once() {
        let scheduler = RefreshScheduler::new();
        let runs = Arc::new(AtomicUsize::new(0));
        for _ in 0..5 {
            assert!(scheduler.schedule_poll_refresh(1, counting_refresh(&runs)));
        }
        assert_eq!(scheduler.pending_count(), 1);
        tokio::time::sleep(Duration::from_millis(1900)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 0, "prima dei 2 secondi niente");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(scheduler.pending_count(), 0);
        scheduler.schedule_poll_refresh(1, counting_refresh(&runs));
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "dopo l'esecuzione se ne può pianificare un altro");
    }

    #[tokio::test(start_paused = true)]
    async fn different_keys_are_independent_and_changes_during_a_refresh_schedule_another() {
        let scheduler = RefreshScheduler::new();
        let runs = Arc::new(AtomicUsize::new(0));
        scheduler.schedule_poll_refresh(1, counting_refresh(&runs));
        scheduler.schedule_poll_refresh(2, counting_refresh(&runs));
        assert_eq!(scheduler.pending_count(), 2);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2);

        // Un refresh lento: mentre gira, una nuova modifica ne pianifica un secondo.
        let slow_runs = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&slow_runs);
        scheduler.schedule("lento", move || async move {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<(), String>(())
        });
        tokio::time::sleep(Duration::from_millis(2100)).await; // il refresh lento è partito
        let counter = Arc::clone(&slow_runs);
        scheduler.schedule("lento", move || async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<(), String>(())
        });
        tokio::time::sleep(Duration::from_secs(15)).await;
        assert_eq!(slow_runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn failing_refresh_is_logged_not_fatal_and_cancel_all_stops_pending_ones() {
        let scheduler = RefreshScheduler::new();
        scheduler.schedule("rotto", || async { Err::<(), String>("Discord non risponde".into()) });
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(scheduler.pending_count(), 0);

        let runs = Arc::new(AtomicUsize::new(0));
        scheduler.schedule_poll_refresh(1, counting_refresh(&runs));
        scheduler.cancel_all();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert_eq!(scheduler.pending_count(), 0);
    }

    #[test]
    fn scheduling_outside_a_runtime_does_nothing() {
        let scheduler = RefreshScheduler::new();
        let runs = Arc::new(AtomicUsize::new(0));
        assert!(!scheduler.schedule_poll_refresh(1, counting_refresh(&runs)));
        assert_eq!(scheduler.pending_count(), 0);
    }
}
