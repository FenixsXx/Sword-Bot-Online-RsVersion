//! Statistiche utenti: punti, livelli, streak, badge segreti e record (equivalente di `sfo/stats.py`).
//!
//! # Panoramica del modulo
//!
//! Questo modulo gestisce le statistiche degli utenti in modo completo.
//! È il corrispettivo Rust di `sfo/stats.py` in Python e viene utilizzato
//! per tracciare attività, premi e record nel bot Discord.
//!
//! # Perché Rust è diverso da Python qui
//!
//! Tutte le funzioni devono funzionare all'interno di una transazione SQL
//! (vedi `Database::transaction` in `database.rs`). Per questo:
//!
//! - **Connessione**: Le funzioni prendono `&Connection`, che può essere
//!   una transazione (`Transaction`) o una connessione normale (`Connection`).
//!   In Rust, `Transaction` implementa lo stesso tratto di `Connection`.
//!
//! - **Orologio**: In Python si usa `datetime.now(TZ)`, in Rust l'orologio
//!   è un parametro esplicito `now`. Questo permette test deterministi
//!   e gestisce meglio i fusi orari.
//!
//! - **Tipo sicuro**: Gli enum in Rust non sono stringhe casuali. Se in Python
//!   `"badgium"` solleva `ValueError`, in Rust non compila affatto.
//!   Questo previene errori a runtime.
//!
//! # Cosa tiene traccia
//!
//! - **Badge segreti**: Achievement nascosti (notte, indeciso, influencer)
//! - **Contatori**: Quanti voti, proposte, sostegni ha dato/ricevuto
//! - **Streak**: Giorni/settimane consecutivi di attività
//! - **Livelli**: Classifica contributore (Nuovo, Bronzo, Argento, Oro, Platino, Re)
//! - **Record**: Statistiche della community (proposta più votata, streak più lungo, ecc.)
//!
//! # Verrà usato da
//!
//! - `sfo-site/src/proposals.rs` - per ipertesi di proposizioni e voti
//! - `sfo-site/src/suggestions.rs` - per tracciare sostegni e premi
//! - `web.rs` - per il reset settimanale
//! - `interactions.rs` - per visualizzare statistiche utente
#![allow(dead_code)]

use chrono::DateTime;
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::config::{AUTHOR_STAT_POINTS, CONTRIBUTOR_LEVELS, HIDDEN_BADGES};
use crate::database::DbResult;
use crate::formatting::python_json;
use crate::queries::{poll_options, suggestion_supporter_counts, suggestion_supporter_ranking};
use crate::schedule::isoformat;
use crate::seasonal::{record_season_activity, season_for_date};

// --- badge segreti ---------------------------------------------------------------------------
// I badge segreti sono achievement nascosti che gli utenti possono sbloccare.
// Non vengono mostrati in modo evidente, ma rivelati quando si ottiene il nome giusto.

/// Badge segreti che gli utenti possono sbloccare in modo nascosto.
///
/// Ogni badge ha una chiave e un nome visualizzato. Vengono sbloccati
/// in contesti specifici:
/// - `NightOwl`: attività notturna (dettagli in `config.rs`)
/// - `Indecisive`: interazioni con la funzione indecisiva
/// - `Influencer`: 3 settimane consecutive di "ballot entries"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenBadge {
    /// Notte stellata: sbloccato quando l'utente è attivo alle notti.
    /// La chiave nel database è `"night_owl"`.
    NightOwl,
    /// Indeciso: sbloccato quando si usa la funzione "indeciso".
    /// La chiave nel database è `"indecisive"`.
    Indecisive,
    /// Influencer: sbloccato quando ci si mette 3 settimane consecutive
    /// nel ballottaggio (o "ballot entries").
    /// La chiave nel database è `"influencer"`.
    Influencer,
}

impl HiddenBadge {
    /// Restituisce la chiave univoca del badge (es. `"night_owl"`).
    ///
    /// Questa chiave è usata per salvare il badge nel database
    /// e deve coincidere con l'entry in `config.rs::HIDDEN_BADGES`.
    ///
    /// # Esempio
    ///
    /// ```text
    /// HiddenBadge::NightOwl.key() → "night_owl"
    /// ```
    pub const fn key(self) -> &'static str {
        match self {
            HiddenBadge::NightOwl => "night_owl",
            HiddenBadge::Indecisive => "indecisive",
            HiddenBadge::Influencer => "influencer",
        }
    }

    /// Nome con emoji mostrato nel profilo utente.
    ///
    /// Il nome è configurato in `config.rs::HIDDEN_BADGES`, un array di
    /// coppie `(chiave, nome)`. Ad esempio: `("night_owl", "🦉 Nottataccia")`.
    ///
    /// # Panico di sicurezza
    ///
    /// Se il badge non è in `HIDDEN_BADGES`, il programma si blocca con un panic.
    /// Questo è intenzionale: un badge mancante è un bug, non un caso normale.
    pub fn name(self) -> &'static str {
        badge_name(self.key()).expect("ogni badge è in config::HIDDEN_BADGES")
    }
}

/// Trova il nome visualizzato di un badge dato la sua chiave.
///
/// Cerca nell'array `HIDDEN_BADGES` di `config.rs` e restituisce
/// il nome se trovato, altrimenti `None`.
fn badge_name(key: &str) -> Option<&'static str> {
    HIDDEN_BADGES.iter().find(|(badge_key, _)| *badge_key == key).map(|(_, name)| *name)
}

/// Sblocca un badge segreto per un utente.
///
/// Inserisce una riga nella tabella `user_hidden_badges` con:
/// - `user_id`: l'ID dell'utente
/// - `badge_key`: la chiave del badge (es. `"night_owl"`)
/// - `unlocked_at`: timestamp di quando è stato sbloccato
///
/// Usa `INSERT OR IGNORE` per evitare duplicati: se l'utente ha già
/// il badge, non viene inserito nuovamente.
///
/// # Ritorno
///
/// - `true` se il badge è stato appena sbloccato (nuovo per l'utente)
/// - `false` se l'utente lo aveva già
///
/// # Esempio
///
/// ```text
/// let nuovo = unlock_hidden_badge(&db, 12345, HiddenBadge::Influencer, now)?;
/// if nuovo { println!("Hai sbloccato un badge segreto!"); }
/// ```
pub fn unlock_hidden_badge(
    connection: &Connection,
    user_id: i64,
    badge: HiddenBadge,
    now: DateTime<Tz>,
) -> DbResult<bool> {
    let inserted = connection.execute(
        "INSERT OR IGNORE INTO user_hidden_badges(user_id,badge_key,unlocked_at) VALUES(?,?,?)",
        params![user_id, badge.key(), isoformat(now)],
    )?;
    Ok(inserted > 0)
}

/// Controlla se l'utente ha 3 settimane consecutive nel ballottaggio.
///
/// Per "nel ballottaggio" si intende che l'utente abbia scritto almeno
/// una proposta con opzioni che possono essere votate.
///
/// # Parametri
///
/// - `user_id`: ID dell'utente da controllare
/// - `week`: settimana di riferimento (controlla anche le due settimane precedenti)
///
/// # Logica
///
/// 1. Prepara le settimane da `week-2` a `week` (3 settimane)
/// 2. Se tutte e 3 sono presenti, l'utente ha lo streak
/// 3. Se manca anche una sola, non ha lo streak
///
/// # Esempio
///
/// ```text
/// // Utente con proposte alle settimane 5, 6, 7
/// has_three_week_ballot_streak(db, 123, 7) → true
///
/// // Utente con proposte alle settimane 5, 7 (manca la 6)
/// has_three_week_ballot_streak(db, 123, 7) → false
/// ```
pub fn has_three_week_ballot_streak(connection: &Connection, user_id: i64, week: i64) -> DbResult<bool> {
    let weeks: Vec<i64> = connection
        .prepare(
            "SELECT week FROM user_ballot_weeks
             WHERE user_id=? AND week BETWEEN ? AND ?
             ORDER BY week DESC LIMIT 3",
        )?
        .query_map(params![user_id, week - 2, week], |row| row.get(0))?
        .collect::<DbResult<_>>()?;
    Ok(weeks == [week, week - 1, week - 2])
}

/// Restituisce i nomi dei badge segreti sbloccati da un utente.
///
/// Legge la tabella `user_hidden_badges`, filtra le chiavi sconosciute
/// (non presenti in `config.rs::HIDDEN_BADGES`) e restituisce i nomi
/// ordinati dal più vecchio al più recente.
///
/// # Ordine
///
/// I badge sono ordinati per `unlocked_at` (quando sono stati sbloccati),
/// poi per `badge_key` (per determinismo).
///
/// # Esempio
///
/// ```text
/// let nomi = user_hidden_badge_names(&db, 12345)?;
/// // Restituisce: ["🦉 Nottataccia", "👑 Influencer"]
/// ```
pub fn user_hidden_badge_names(connection: &Connection, user_id: i64) -> DbResult<Vec<&'static str>> {
    let keys: Vec<String> = connection
        .prepare("SELECT badge_key FROM user_hidden_badges WHERE user_id=? ORDER BY unlocked_at, badge_key")?
        .query_map([user_id], |row| row.get(0))?
        .collect::<DbResult<_>>()?;
    Ok(keys.iter().filter_map(|key| badge_name(key)).collect())
}

// --- contatori e attività --------------------------------------------------------------------
// Qui trovi le funzioni per tracciare le azioni degli utenti e il loro
// progresso nel tempo.

/// Colonne della tabella `user_stats` che si possono incrementare.
///
/// Ogni colonna conta un tipo specifico di azione:
/// - `ProposalsSubmitted`: quante proposte ha pubblicato l'utente
/// - `SupportsGiven`: quanti sostegni ha dato (cliccato su proposte)
/// - `SupportsReceived`: quanti sostegni ha ricevuto (gli altri hanno dato)
/// - `BallotEntries`: quante volte ha scritto opzioni nel ballottaggio settimanale
/// - `WinningProposals`: quante proposte ha vinto
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserStat {
    ProposalsSubmitted,
    SupportsGiven,
    SupportsReceived,
    BallotEntries,
    WinningProposals,
}

impl UserStat {
    /// Restituisce il nome della colonna nel database.
    ///
    /// Mappa l'enum a una stringa che corrisponde al nome della colonna
    /// nella tabella `user_stats`.
    ///
    /// # Esempio
    ///
    /// ```text
    /// UserStat::ProposalsSubmitted.column() → "proposals_submitted"
    /// ```
    const fn column(self) -> &'static str {
        match self {
            UserStat::ProposalsSubmitted => "proposals_submitted",
            UserStat::SupportsGiven => "supports_given",
            UserStat::SupportsReceived => "supports_received",
            UserStat::BallotEntries => "ballot_entries",
            UserStat::WinningProposals => "winning_proposals",
        }
    }
}

/// Statistiche che vengono premiate agli autori delle proposte.
///
/// Gli autori ricevono punti per le loro proposte vincenti o ricevute.
/// I punti sono configurabili in `config.rs::AUTHOR_STAT_POINTS`.
///
/// # Esempio di configurazione
///
/// ```text
/// AUTHOR_STAT_POINTS = [
///     ("supports_received", 2),  // +2 punti per ogni sostegno ricevuto
///     ("ballot_entries", 5),     // +5 punti per ogni voce nel ballottaggio
///     ("winning_proposals", 10), // +10 punti per ogni proposta vinta
/// ]
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorStat {
    SupportsReceived,
    BallotEntries,
    WinningProposals,
}

impl AuthorStat {
    /// Mappa l'enum alla sua controparte `UserStat`.
    ///
    /// Ogni `AuthorStat` ha una colonna corrispondente in `user_stats`.
    fn user_stat(self) -> UserStat {
        match self {
            AuthorStat::SupportsReceived => UserStat::SupportsReceived,
            AuthorStat::BallotEntries => UserStat::BallotEntries,
            AuthorStat::WinningProposals => UserStat::WinningProposals,
        }
    }

    /// Punti settimanali che l'autore riceve per questa statistica.
    ///
    /// Legge il valore da `config.rs::AUTHOR_STAT_POINTS`. Se manca
    /// la configurazione, il programma si blocca (un bug).
    ///
    /// # Esempio
    ///
    /// ```text
    /// AuthorStat::BallotEntries.points() → 5
    /// // Significa: ogni volta che scrivi una voce nel ballottaggio,
    /// // ricevi 5 punti di contribuzione
    /// ```
    pub fn points(self) -> i64 {
        let column = self.user_stat().column();
        AUTHOR_STAT_POINTS
            .iter()
            .find(|(name, _)| *name == column)
            .map(|(_, points)| *points)
            .expect("ogni statistica degli autori è in config::AUTHOR_STAT_POINTS")
    }
}

/// Incrementa un contatore della tabella `user_stats`.
///
/// Usa un upsert: se l'utente non esiste, lo inserisce; se esiste,
/// incrementa di 1.
///
/// # Sicurezza SQL
///
/// Anche se usa `format!` per inserire il nome della colonna, è sicuro
/// perché `UserStat::column()` restituisce stringhe da una lista chiusa
/// (es. `"proposals_submitted"`). Non è possibile impersonare SQL.
///
/// # Esempio
///
/// ```text
/// increment_user_stat(&db, 12345, UserStat::SupportsReceived)?;
/// // Se l'utente non esiste, crea:
/// // INSERT INTO user_stats(user_id, supports_received) VALUES(12345, 1)
/// // Se esiste già:
/// // UPDATE user_stats SET supports_received = supports_received + 1 WHERE user_id = 12345
/// ```
pub fn increment_user_stat(connection: &Connection, user_id: i64, stat: UserStat) -> DbResult<()> {
    let column = stat.column(); // nome di colonna da una lista chiusa: sicuro da interpolare
    connection.execute(
        &format!(
            "INSERT INTO user_stats(user_id, {column}) VALUES(?, 1)
             ON CONFLICT(user_id) DO UPDATE SET {column}=user_stats.{column}+1"
        ),
        [user_id],
    )?;
    Ok(())
}

/// Registra un'attività dell'utente per la settimana corrente.
///
/// Aggiorna 4 tabelle in modo transazionale:
/// 1. `weekly_activity`: contatore e punti di questa settimana
/// 2. `season_activity`: totali stagionale (estate/inverno)
/// 3. `user_streaks`: streak corrente e migliore
/// 4. `weekly_streak_records`: record di streak per settimana
///
/// # Streak: cos'è?
///
/// Lo streak è il numero di settimane CONSECUTIVE con attività.
/// - Se manca una settimana, lo streak ricomincia da 1
/// - Il migliore (best_streak) non si resetta mai
///
/// # Esempio
///
/// ```text
/// // Settimana 5: attività
/// record_user_activity(&db, user_id, 5, 10, now)?; // streak = 1
///
/// // Settimana 6: nuova attività
/// record_user_activity(&db, user_id, 6, 5, now)?;  // streak = 2
///
/// // Settimana 7: mancata attività (chiama con 0 punti?)
/// // streak = 1 (rispetto alla settimana 5, 6 sono "interrotte")
/// ```
pub fn record_user_activity(
    connection: &Connection,
    user_id: i64,
    week: i64,
    points: i64,
    occurred_at: DateTime<Tz>,
) -> DbResult<()> {
    connection.execute(
        "INSERT INTO weekly_activity(user_id, week, activity_count, points)
         VALUES(?,?,1,?)
         ON CONFLICT(user_id, week) DO UPDATE SET
             activity_count=weekly_activity.activity_count+1,
             points=weekly_activity.points+excluded.points",
        params![user_id, week, points],
    )?;
    record_season_activity(connection, user_id, week, points, occurred_at.date_naive())?;

    let streak: Option<(i64, i64, Option<i64>)> = connection
        .query_row(
            "SELECT current_streak, best_streak, last_active_week FROM user_streaks WHERE user_id=?",
            [user_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if streak.is_some_and(|(_, _, last)| last == Some(week)) {
        return Ok(()); // già attivo questa settimana: lo streak non cambia
    }
    let next_streak = match streak {
        Some((current, _, last)) if last == Some(week - 1) => current + 1,
        _ => 1,
    };
    let previous_best = streak.map_or(0, |(_, best, _)| best);
    let best_streak = if streak.is_some() { previous_best.max(next_streak) } else { 1 };
    connection.execute(
        "INSERT INTO user_streaks(user_id,current_streak,best_streak,last_active_week)
         VALUES(?,?,?,?)
         ON CONFLICT(user_id) DO UPDATE SET
             current_streak=excluded.current_streak,
             best_streak=excluded.best_streak,
             last_active_week=excluded.last_active_week",
        params![user_id, next_streak, best_streak, week],
    )?;
    if next_streak > previous_best {
        connection.execute(
            "INSERT OR IGNORE INTO weekly_streak_records(user_id,week,streak) VALUES(?,?,?)",
            params![user_id, week, next_streak],
        )?;
    }
    Ok(())
}

/// Autori distinti di una proposta in una settimana.
fn suggestion_authors(connection: &Connection, suggestion_id: i64, week: i64) -> DbResult<Vec<i64>> {
    connection
        .prepare("SELECT DISTINCT user_id FROM proposals WHERE suggestion_id=? AND week=?")?
        .query_map([suggestion_id, week], |row| row.get(0))?
        .collect()
}

/// Rimuove i punti "ballottaggio" quando una proposta esce dal ballottaggio.
///
/// Questo è necessario perché i punti vengono assegnati quando un autore
/// scrive una proposta, ma se la proposta viene rimossa dal ballottaggio
/// (es. per troppi voti neri), quei punti vanno sottratti.
///
/// # Chiama quando
///
/// - Rimuovere un'autoproposta dal ballottaggio
/// - Eliminare una proposta non approvata
/// - Spostare una proposta fuori dal ballottaggio per errori
///
/// # Cosa fa
///
/// 1. Trova tutti gli autori della proposta
/// 2. Sottrae 1 a `ballot_entries` in `user_stats`
/// 3. Sottrae i punti da `weekly_activity`
/// 4. Sottrae i punti da `season_activity`
/// 5. Usa `MAX(0, ...)` per non andare sotto zero
///
/// # Esempio
///
/// ```text
/// // Prima: autore ha ballot_entries = 3, points = 15
/// revoke_ballot_entry_stats(&db, proposta_id, settimana, now)?;
/// // Dopo: ballot_entries = 2, points = 10
/// ```
pub fn revoke_ballot_entry_stats(
    connection: &Connection,
    suggestion_id: i64,
    week: i64,
    now: DateTime<Tz>,
) -> DbResult<()> {
    let points = AuthorStat::BallotEntries.points();
    let season_key = season_for_date(now.date_naive()).key;
    for user_id in suggestion_authors(connection, suggestion_id, week)? {
        connection.execute(
            "UPDATE user_stats SET ballot_entries=MAX(0, ballot_entries-1) WHERE user_id=?",
            [user_id],
        )?;
        connection.execute(
            "UPDATE weekly_activity SET points=MAX(0, points-?) WHERE user_id=? AND week=?",
            params![points, user_id, week],
        )?;
        connection.execute(
            "UPDATE season_activity SET points=MAX(0, points-?)
             WHERE season_key=? AND user_id=? AND week=?",
            params![points, season_key, user_id, week],
        )?;
    }
    Ok(())
}

/// Assegna una statistica e i punti corrispondenti a tutti gli autori di una proposta.
///
/// Questa è la funzione chiave per premiare chi scrive proposte.
/// Viene chiamata quando:
/// - Un utente riceve un sostegno
/// - Un'autoproposta va nel ballottaggio
/// - Una proposta vince la settimana
///
/// # Azioni effettuate per ogni autore
///
/// 1. Se è `BallotEntries`: controlla se basta per il badge Influencer
/// 2. Incrementa il contatore in `user_stats`
/// 3. Registra l'attività (per lo streak)
///
/// # Esempio
///
/// ```text
/// increment_suggestion_authors_stat(&db, proposta_id, 5, AuthorStat::SupportsReceived, now)?;
/// // Ogni autore riceve +2 punti e +1 a supports_received
/// ```
pub fn increment_suggestion_authors_stat(
    connection: &Connection,
    suggestion_id: i64,
    week: i64,
    stat: AuthorStat,
    now: DateTime<Tz>,
) -> DbResult<()> {
    for user_id in suggestion_authors(connection, suggestion_id, week)? {
        if stat == AuthorStat::BallotEntries {
            connection.execute(
                "INSERT OR IGNORE INTO user_ballot_weeks(user_id,week) VALUES(?,?)",
                params![user_id, week],
            )?;
            if has_three_week_ballot_streak(connection, user_id, week)? {
                unlock_hidden_badge(connection, user_id, HiddenBadge::Influencer, now)?;
            }
        }
        increment_user_stat(connection, user_id, stat.user_stat())?;
        record_user_activity(connection, user_id, week, stat.points(), now)?;
    }
    Ok(())
}

// --- livelli e classifica --------------------------------------------------------------------
// Qui trovi le funzioni per i livelli di contributore e la classifica.

/// I contatori di `user_stats` di un utente, in un'unica struttura.
///
/// Rappresenta lo "sketch" delle statistiche di un utente, utile per
/// calcolare il livello e i punti. I campi sono copi direttamente
/// dalla tabella `user_stats`.
///
/// # Esempio
///
/// ```text
/// let stats = UserStats {
///     proposals_submitted: 5,
///     supports_received: 10,
///     supports_given: 3,
///     ballot_entries: 2,
///     winning_proposals: 1,
/// };
/// let punti = contributor_points(&stats); // 5 + 20 + 10 = 35
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserStats {
    pub proposals_submitted: i64,
    pub supports_given: i64,
    pub supports_received: i64,
    pub ballot_entries: i64,
    pub winning_proposals: i64,
}

/// Calcola i punti totali di contribuzione.
///
/// La formula premia gli sostegni ricevuti e le vittorie di più
/// degli sostegni dati, incentivando la creazione di proposte
/// di qualità.
///
/// # Formula
///
/// ```text
/// punti = proposals_submitted
///       + 2 * supports_received
///       + 5 * ballot_entries
///       + 10 * winning_proposals
/// ```
///
/// # Esempio
///
/// ```text
/// UserStats { proposals_submitted: 5, supports_received: 10, ... }
/// punti = 5 + 2*10 = 25 punti
/// ```
pub fn contributor_points(stats: &UserStats) -> i64 {
    stats.proposals_submitted + 2 * stats.supports_received + 5 * stats.ballot_entries + 10 * stats.winning_proposals
}

/// Trova il livello più alto raggiunto dai punti.
///
/// I livelli sono definiti in `config.rs::CONTRIBUTOR_LEVELS`.
/// Ogni livello ha un nome con emoji (es. "🥉 Bronzo").
///
/// # Classificazione
///
/// ```text
/// 0-9 punti:   "🌱 Nuovo"
/// 10-24 punti: "🥉 Bronzo"
/// 25-49 punti: "🥈 Argento"
/// 50-99 punti: "🥇 Oro"
/// 100-199:     "💎 Platino"
/// 200+:        "👑 Leggenda"
/// ```
pub fn contributor_level(points: i64) -> &'static str {
    let mut level = CONTRIBUTOR_LEVELS[0].1;
    for (minimum, name) in CONTRIBUTOR_LEVELS {
        if points < i64::from(minimum) {
            break;
        }
        level = name;
    }
    level
}

/// Restituisce il progresso verso il prossimo livello.
///
/// Utile per mostrare una barra di progresso nel profilo utente.
///
/// # Ritorno
///
/// Una tupla: `(livello_attuale, prossimo_livello, min_attuale, min_prossimo)`
///
/// - `livello_attuale`: nome del livello corrente (es. "🥉 Bronzo")
/// - `prossimo_livello`: nome del livello successivo (es. "🥈 Argento")
/// - `min_attuale`: punti minimi per il livello attuale (es. 10)
/// - `min_prossimo`: punti minimi per il prossimo livello (es. 25)
///
/// # Esempio
///
/// ```text
/// contributor_level_progress(15) → ("🥉 Bronzo", "🥈 Argento", 10, 25)
/// // L'utente è al livello Bronzo (min 10), serve arrivare a 25 per Argento
///
/// contributor_level_progress(200) → ("👑 Leggenda", "livello massimo", 200, 200)
/// // Al massimo: prossimo è "livello massimo", minimo uguale
/// ```
pub fn contributor_level_progress(points: i64) -> (&'static str, &'static str, i64, i64) {
    let mut level = CONTRIBUTOR_LEVELS[0].1;
    let mut current_minimum = 0;
    for (minimum, name) in CONTRIBUTOR_LEVELS {
        let minimum = i64::from(minimum);
        if points < minimum {
            return (level, name, current_minimum, minimum);
        }
        level = name;
        current_minimum = minimum;
    }
    (level, "livello massimo", current_minimum, current_minimum)
}

/// Riga della classifica dei contributori.
///
/// Contiene i dati necessari per mostrare una riga in classifica:
/// - `user_id`: per menzionare l'utente
/// - `points`: punti totali (per ordinamento)
/// - `level`: livello attuale
/// - `winning_proposals`: vittorie (per spareggio)
/// - `supports_received`: sostegni ricevuti (per spareggio)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributorRow {
    pub user_id: i64,
    pub points: i64,
    pub level: &'static str,
    pub winning_proposals: i64,
    pub supports_received: i64,
}

/// Restituisce la classifica dei contributori (chi ha più punti).
///
/// Legge tutti gli utenti da `user_stats`, calcola i punti per ognuno,
/// filtra chi ha 0 punti, ordina e limita i risultati.
///
/// # Ordinamento (criteri di spareggio)
///
/// 1. **Punti totali** (decrescente) - chi ha più punti sta sopra
/// 2. **Proposte vinte** (decrescente) - a parità, chi ha vinto di più
/// 3. **Sostegni ricevuti** (decrescente) - a parità, chi ha più sostegni
/// 4. **user_id** (crescente) - a parità assoluta, chi ha ID minore
///
/// # Esempio
///
/// ```text
/// // Top 10
/// let top10 = contributor_leaderboard_rows(&db, 10)?;
/// for (i, row) in top10.iter().enumerate() {
///     println!("{}. {} - {} - {}", i+1, row.user_id, row.level, row.points);
/// }
/// ```
pub fn contributor_leaderboard_rows(connection: &Connection, limit: usize) -> DbResult<Vec<ContributorRow>> {
    let all: Vec<(i64, UserStats)> = connection
        .prepare(
            "SELECT user_id, proposals_submitted, supports_given, supports_received,
                    ballot_entries, winning_proposals
             FROM user_stats",
        )?
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                UserStats {
                    proposals_submitted: row.get(1)?,
                    supports_given: row.get(2)?,
                    supports_received: row.get(3)?,
                    ballot_entries: row.get(4)?,
                    winning_proposals: row.get(5)?,
                },
            ))
        })?
        .collect::<DbResult<_>>()?;
    let mut scored: Vec<ContributorRow> = all
        .into_iter()
        .map(|(user_id, stats)| {
            let points = contributor_points(&stats);
            ContributorRow {
                user_id,
                points,
                level: contributor_level(points),
                winning_proposals: stats.winning_proposals,
                supports_received: stats.supports_received,
            }
        })
        .filter(|row| row.points > 0)
        .collect();
    scored.sort_by_key(|row| (-row.points, -row.winning_proposals, -row.supports_received, row.user_id));
    scored.truncate(limit);
    Ok(scored)
}

// --- premi, record e streak settimanali ------------------------------------------------------
// Qui trovi le funzioni per i premi settimanali e i record della community.

/// Converte un errore di parsing JSON in errore rusqlite.
///
/// Usato quando leggiamo i JSON salvati nel database per controllare
/// se sono validi. Se il JSON è corrotto, restituisce un errore SQL.
fn json_conversion_error(error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

/// Registra i contributori della settimana (chi ha più punti).
///
/// Calcola una sola volta: le chiamate successive restituiscono
/// il risultato già salvato in `weekly_awards`.
///
/// # Logica
///
/// 1. Controlla se c'è già un premio per questa settimana in `weekly_awards`
/// 2. Se sì, lo restituisce (idempotente)
/// 3. Se no, calcola chi ha più punti in `weekly_activity`
/// 4. Salva il risultato come JSON in `weekly_awards`
/// 5. Restituisce (vincitori, punti)
///
/// # Gestione JSON
///
/// Il JSON salvato è compatibile con Python: array di numeri o stringhe.
/// Esempio: `"[10, 30]"` o `"[\"11\", 22]"`.
///
/// # Esempio
///
/// ```text
/// let (vincitori, punti) = record_weekly_contributor(&db, 5, now)?;
/// // vincitori = [12345, 67890], punti = 50
/// ```
pub fn record_weekly_contributor(
    connection: &Connection,
    week: i64,
    now: DateTime<Tz>,
) -> DbResult<(Vec<i64>, i64)> {
    let existing: Option<(String, i64)> = connection
        .query_row("SELECT user_ids, points FROM weekly_awards WHERE week=?", [week], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?;
    if let Some((user_ids, points)) = existing {
        let values: Vec<Value> = serde_json::from_str(&user_ids).map_err(json_conversion_error)?;
        let winners = values
            .iter()
            .map(|value| match value {
                Value::Number(number) => number.as_i64(),
                Value::String(text) => text.trim().parse().ok(), // `int("123")` di Python
                _ => None,
            })
            .collect::<Option<Vec<i64>>>()
            .ok_or_else(|| json_conversion_error(std::fmt::Error))?;
        return Ok((winners, points));
    }

    let top: Option<i64> =
        connection.query_row("SELECT MAX(points) FROM weekly_activity WHERE week=?", [week], |row| row.get(0))?;
    let points = top.unwrap_or(0);
    let winners: Vec<i64> = if points == 0 {
        Vec::new()
    } else {
        connection
            .prepare("SELECT user_id FROM weekly_activity WHERE week=? AND points=? ORDER BY user_id")?
            .query_map(params![week, points], |row| row.get(0))?
            .collect::<DbResult<_>>()?
    };
    connection.execute(
        "INSERT INTO weekly_awards(week,user_ids,points,recorded_at) VALUES(?,?,?,?)",
        params![week, python_json(&json!(winners)), points, isoformat(now)],
    )?;
    Ok((winners, points))
}

/// Aggiorna un record della community se il nuovo valore è migliore.
///
/// I record sono salvati in `community_records` e hanno:
/// - `record_key`: identificatore (es. "most_supported", "longest_streak")
/// - `metric`: valore numerico
/// - `label`: descrizione leggibile (es. "Proposta X", "<@12345>")
/// - `week`: settimana in cui è stato stabilito
/// - `user_id`: utente associato (opzionale)
///
/// # Regola
///
/// Aggiorna solo se `metric > 0` e supera il record esistente.
/// A parità, il primo record vince (non si sovrascrive).
///
/// # Esempio
///
/// ```text
/// // Nuovo record per proposta più sostenuta
/// update_community_record(&db, "most_supported", 50, "Among Us", 4, None)?;
/// ```
pub fn update_community_record(
    connection: &Connection,
    record_key: &str,
    metric: i64,
    label: &str,
    week: i64,
    user_id: Option<i64>,
) -> DbResult<()> {
    if metric <= 0 {
        return Ok(());
    }
    let existing: Option<i64> = connection
        .query_row("SELECT metric FROM community_records WHERE record_key=?", [record_key], |row| row.get(0))
        .optional()?;
    if existing.is_some_and(|current| current >= metric) {
        return Ok(());
    }
    connection.execute(
        "INSERT INTO community_records(record_key,metric,label,week,user_id)
         VALUES(?,?,?,?,?)
         ON CONFLICT(record_key) DO UPDATE SET
             metric=excluded.metric,
             label=excluded.label,
             week=excluded.week,
             user_id=excluded.user_id",
        params![record_key, metric, label, week, user_id],
    )?;
    Ok(())
}

/// Ricalcola tutti i record della community per la settimana.
///
/// Chiama le funzioni per ogni tipo di record:
/// 1. `most_supported`: proposta con più sostenitori
/// 2. `most_voted`: opzione con più voti (nei sondaggi community)
/// 3. `longest_streak`: utente con lo streak più lungo
/// 4. `most_wins`: utente con più proposte vincenti
///
/// # Note
///
/// - `most_voted` si basa sui sondaggi community (`polls.is_community = 1`)
/// - Usa `max()` di Python per i backup: a parità, vince il primo
/// - Gli altri record usano i dati aggregati
///
/// # Esempio
///
/// ```text
/// update_community_records(&db, 5)?;
/// // Aggiorna tutti i record con dati della settimana 5
/// ```
pub fn update_community_records(connection: &Connection, week: i64) -> DbResult<()> {
    if let Some(top) = suggestion_supporter_ranking(connection, week, 1, false)?.first() {
        update_community_record(connection, "most_supported", top.supporters, &top.name, week, None)?;
    }

    let community_poll_ids: Vec<i64> = connection
        .prepare("SELECT id FROM polls WHERE is_community=1 AND active=1")?
        .query_map([], |row| row.get(0))?
        .collect::<DbResult<_>>()?;
    let mut top_option = None;
    for poll_id in community_poll_ids {
        for option in poll_options(connection, poll_id)? {
            // `max()` di Python tiene il primo tra i pari: si sostituisce solo con un valore maggiore.
            if top_option.as_ref().is_none_or(|best: &crate::queries::PollOption| option.n > best.n) {
                top_option = Some(option);
            }
        }
    }
    if let Some(option) = top_option {
        update_community_record(connection, "most_voted", option.n, &option.label, week, None)?;
    }

    let streak: Option<(i64, i64)> = connection
        .query_row(
            "SELECT user_id, best_streak FROM user_streaks ORDER BY best_streak DESC, user_id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((user_id, best_streak)) = streak {
        update_community_record(connection, "longest_streak", best_streak, &format!("<@{user_id}>"), week, Some(user_id))?;
    }

    let wins: Option<(i64, i64)> = connection
        .query_row(
            "SELECT user_id, winning_proposals FROM user_stats ORDER BY winning_proposals DESC, user_id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((user_id, winning_proposals)) = wins {
        update_community_record(connection, "most_wins", winning_proposals, &format!("<@{user_id}>"), week, Some(user_id))?;
    }
    Ok(())
}

/// Azzera gli streak di chi non ha fatto attività nella settimana appena chiusa.
///
/// Questa funzione è chiamata alla fine di ogni settimana (dopo il reset).
/// Gli utenti che non hanno attività in `weekly_activity` per la settimana
/// precedente (`week - 1`, cioè appena conclusa) vedranno il loro `current_streak`
/// azzerato a 0.
///
/// L'`best_streak` (migliore record personale) resta intatto.
///
/// # Esempio
///
/// ```text
/// // Settimana 5 appena chiusa
/// finalize_week_streaks(&db, 5)?;
/// // Utenti non attivi nella settimana 4 hanno streak = 0
/// ```
pub fn finalize_week_streaks(connection: &Connection, week: i64) -> DbResult<()> {
    connection.execute(
        "UPDATE user_streaks SET current_streak=0 WHERE current_streak>0 AND last_active_week<?",
        [week],
    )?;
    Ok(())
}

// --- vincitore della settimana ---------------------------------------------------------------
// Qui trovi le strutture e funzioni per determinare chi vince la settimana.

/// Una proposta nel ballottaggio con risultati e dati di spareggio.
///
/// Rappresenta una singola opzione nel ballottaggio community.
///
/// # Campi
///
/// - `suggestion_id`: ID unico della proposta
/// - `name`: nome visualizzato (es. "Among Us")
/// - `votes`: numero di voti espressi
/// - `supporters`: numero di unici sostenitori (persone diverse)
/// - `first_ts`: timestamp della prima proposta (0 se sconosciuto, per spareggio)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BallotResult {
    pub suggestion_id: i64,
    pub name: String,
    pub votes: i64,
    pub supporters: i64,
    /// Primo momento in cui la proposta è stata inviata (0 se sconosciuto).
    pub first_ts: i64,
}

/// Risultati del ballottaggio community in ordine di classifica.
///
/// Legge le opzioni dei sondaggi community e conta i voti e i sostenitori.
///
/// # Spareggio esplicito
///
/// Quando due proposte hanno lo stesso numero di voti, il "tie-breaker" è:
/// 1. Maggior numero di sostenitori (persone diverse che hanno dato un voto)
/// 2. Se ancora pareggio, la proposta con timestamp più basso (prima inviata)
///
/// # Esempio
///
/// ```text
/// let risultati = community_ballot_results(&db, 5)?;
/// // prima = BallotResult { name: "Among Us", votes: 10, supporters: 8, ... }
/// ```
pub fn community_ballot_results(connection: &Connection, week: i64) -> DbResult<Vec<BallotResult>> {
    let rows: Vec<(i64, String, i64, Option<i64>)> = connection
        .prepare(
            "SELECT o.suggestion_id, o.label AS name, COUNT(v.user_id) AS votes,
                    (SELECT MIN(ts) FROM proposals pr
                     WHERE pr.suggestion_id=o.suggestion_id AND pr.week=?) AS first_ts
             FROM options o
             JOIN polls p ON p.id=o.poll_id
             LEFT JOIN votes v ON v.option_id=o.id
             WHERE p.is_community=1 AND p.active=1 AND o.suggestion_id IS NOT NULL
             GROUP BY o.id",
        )?
        .query_map([week], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
        .collect::<DbResult<_>>()?;
    let ids: Vec<i64> = rows.iter().map(|row| row.0).collect();
    let supporters = suggestion_supporter_counts(connection, week, &ids)?;
    let mut results: Vec<BallotResult> = rows
        .into_iter()
        .map(|(suggestion_id, name, votes, first_ts)| BallotResult {
            suggestion_id,
            name,
            votes,
            supporters: supporters.get(&suggestion_id).copied().unwrap_or(0),
            first_ts: first_ts.unwrap_or(0),
        })
        .collect();
    results.sort_by_key(|item| (-item.votes, -item.supporters, item.first_ts, item.suggestion_id));
    Ok(results)
}

/// Dati del vincitore della settimana.
///
/// Contiene la proposta vincente e un messaggio facoltativo se c'è
/// stato bisogno di uno spareggio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Winner {
    /// Risultato (dati della proposta)
    pub result: BallotResult,
    /// Messaggio di spareggio (es. "vince con più sostenitori"), None se non serve
    pub tie_break: Option<&'static str>,
}

/// Determina il vincitore unico della settimana.
///
/// Restituisce `None` se:
/// - Non ci sono sondaggi community attivi
/// - Nessuno ha votato
///
/// Se c'è un pareggio (pari voti), calcola il messaggio di spareggio.
///
/// # Esempio
///
/// ```text
/// if let Some(vincitore) = weekly_winner(&db, 5)? {
///     println!("Ha vinto: {}", vincitore.result.name);
/// }
/// ```
pub fn weekly_winner(connection: &Connection, week: i64) -> DbResult<Option<Winner>> {
    let results = community_ballot_results(connection, week)?;
    let Some(first) = results.first() else {
        return Ok(None);
    };
    if first.votes == 0 {
        return Ok(None);
    }
    let mut tie_break = None;
    if let Some(second) = results.get(1).filter(|second| second.votes == first.votes) {
        tie_break = Some(if second.supporters != first.supporters {
            "a parità di voti vince la proposta con più sostenitori"
        } else {
            "a parità di voti e sostenitori vince la proposta inviata per prima"
        });
    }
    Ok(Some(Winner { result: first.clone(), tie_break }))
}

/// Registra le statistiche di vittoria per gli autori della proposta vincente.
///
/// Viene chiamata dopo `weekly_winner`. Per ogni autore:
/// 1. Inserisce in `user_winning_proposals` (con `INSERT OR IGNORE`)
/// 2. Se nuovo (non già registrato), incrementa `winning_proposals` e aggiunge punti
///
/// # Idempotenza
///
/// `INSERT OR IGNORE` evita duplicati: chiamare due volte non raddoppia i punti.
///
/// # Parametri
///
/// - `awarded_at`: quando attribuire l'attività (default `now`). Utile per
///   backfilling: se calcoli il vincitore dopo la settimana, puoi usare
///   la data di fine settimana come `awarded_at`.
///
/// # Esempio
///
/// ```text
/// let Some(vincitore) = record_weekly_winner_stats(&db, 5, now, None)?;
/// // Gli autori hanno ora +10 punti e +1 a winning_proposals
/// ```
pub fn record_weekly_winner_stats(
    connection: &Connection,
    week: i64,
    now: DateTime<Tz>,
    awarded_at: Option<DateTime<Tz>>,
) -> DbResult<Option<Winner>> {
    let Some(winner) = weekly_winner(connection, week)? else {
        return Ok(None);
    };
    for user_id in suggestion_authors(connection, winner.result.suggestion_id, week)? {
        let inserted = connection.execute(
            "INSERT OR IGNORE INTO user_winning_proposals
             (user_id, week, suggestion_id, name, votes, recorded_at)
             VALUES(?,?,?,?,?,?)",
            params![
                user_id,
                week,
                winner.result.suggestion_id,
                winner.result.name,
                winner.result.votes,
                isoformat(now)
            ],
        )?;
        if inserted > 0 {
            increment_user_stat(connection, user_id, UserStat::WinningProposals)?;
            record_user_activity(
                connection,
                user_id,
                week,
                AuthorStat::WinningProposals.points(),
                awarded_at.unwrap_or(now),
            )?;
        }
    }
    Ok(Some(winner))
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
        Database::open_in_memory().unwrap()
    }

    fn one<T: rusqlite::types::FromSql>(db: &Database, sql: &str) -> T {
        db.with_conn(|c| c.query_row(sql, [], |row| row.get(0)).unwrap())
    }

    fn activity(db: &Database, user: i64, week: i64) -> (i64, i64) {
        db.with_conn(|c| {
            c.query_row(
                "SELECT activity_count, points FROM weekly_activity WHERE user_id=? AND week=?",
                [user, week],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        })
    }

    fn streak(db: &Database, user: i64) -> (i64, i64, Option<i64>) {
        db.with_conn(|c| {
            c.query_row(
                "SELECT current_streak, best_streak, last_active_week FROM user_streaks WHERE user_id=?",
                [user],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        })
    }

    fn records(db: &Database, user: i64) -> Vec<(i64, i64)> {
        db.with_conn(|c| {
            c.prepare("SELECT week, streak FROM weekly_streak_records WHERE user_id=? ORDER BY week")
                .unwrap()
                .query_map([user], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        })
    }

    // --- badge -----------------------------------------------------------------------------

    #[test]
    fn hidden_badges_unlock_once_and_list_in_order() {
        let db = db();
        db.with_conn(|c| {
            assert!(unlock_hidden_badge(c, 1, HiddenBadge::Influencer, now()).unwrap());
            assert!(!unlock_hidden_badge(c, 1, HiddenBadge::Influencer, now()).unwrap(), "già sbloccato");
            assert!(unlock_hidden_badge(c, 1, HiddenBadge::NightOwl, now()).unwrap());
            // Stesso istante di sblocco: a parità decide la chiave (influencer < night_owl).
            assert_eq!(user_hidden_badge_names(c, 1).unwrap(), ["📣 Influencer", "🌙 Nottataccia"]);
            assert!(user_hidden_badge_names(c, 2).unwrap().is_empty());
            // Un badge con chiave sconosciuta nel database viene ignorato.
            c.execute("INSERT INTO user_hidden_badges VALUES(1,'vecchio_badge','x')", []).unwrap();
            assert_eq!(user_hidden_badge_names(c, 1).unwrap().len(), 2);
        });
        assert_eq!(HiddenBadge::Indecisive.name(), "🔄 Indeciso");
    }

    #[test]
    fn three_consecutive_ballot_weeks_are_detected() {
        let db = db();
        db.with_conn(|c| {
            for week in [5, 6] {
                c.execute("INSERT INTO user_ballot_weeks VALUES(1,?)", [week]).unwrap();
            }
            assert!(!has_three_week_ballot_streak(c, 1, 7).unwrap());
            c.execute("INSERT INTO user_ballot_weeks VALUES(1,7)", []).unwrap();
            assert!(has_three_week_ballot_streak(c, 1, 7).unwrap());
            assert!(!has_three_week_ballot_streak(c, 1, 8).unwrap(), "manca la settimana 8");
            c.execute("INSERT INTO user_ballot_weeks VALUES(2,5),(2,7),(2,8)", []).unwrap();
            assert!(!has_three_week_ballot_streak(c, 2, 8).unwrap(), "buco in mezzo");
        });
    }

    // --- attività e streak -----------------------------------------------------------------

    #[test]
    fn user_stats_increment_per_column() {
        let db = db();
        db.with_conn(|c| {
            increment_user_stat(c, 1, UserStat::SupportsGiven).unwrap();
            increment_user_stat(c, 1, UserStat::SupportsGiven).unwrap();
            increment_user_stat(c, 1, UserStat::ProposalsSubmitted).unwrap();
        });
        let row: (i64, i64, i64) = db.with_conn(|c| {
            c.query_row(
                "SELECT proposals_submitted, supports_given, winning_proposals FROM user_stats WHERE user_id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        });
        assert_eq!(row, (1, 2, 0));
    }

    #[test]
    fn activity_accumulates_counts_points_and_season() {
        let db = db();
        db.with_conn(|c| {
            record_user_activity(c, 7, 3, 1, now()).unwrap();
            record_user_activity(c, 7, 3, 2, now()).unwrap();
        });
        assert_eq!(activity(&db, 7, 3), (2, 3));
        let season: (i64, i64) = db.with_conn(|c| {
            c.query_row(
                "SELECT activity_count, points FROM season_activity WHERE season_key='autunno-2026' AND user_id=7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        });
        assert_eq!(season, (2, 3));
    }

    #[test]
    fn streak_grows_with_consecutive_weeks_and_resets_after_a_gap() {
        let db = db();
        db.with_conn(|c| {
            record_user_activity(c, 1, 10, 0, now()).unwrap();
            record_user_activity(c, 1, 10, 0, now()).unwrap(); // stessa settimana: nulla cambia
        });
        assert_eq!(streak(&db, 1), (1, 1, Some(10)));
        assert_eq!(records(&db, 1), [(10, 1)]);

        db.with_conn(|c| {
            record_user_activity(c, 1, 11, 0, now()).unwrap();
            record_user_activity(c, 1, 12, 0, now()).unwrap();
        });
        assert_eq!(streak(&db, 1), (3, 3, Some(12)));
        assert_eq!(records(&db, 1), [(10, 1), (11, 2), (12, 3)], "un record a ogni nuovo massimo");

        db.with_conn(|c| record_user_activity(c, 1, 15, 0, now()).unwrap()); // salta 13 e 14
        assert_eq!(streak(&db, 1), (1, 3, Some(15)), "streak riparte, il migliore resta");
        assert_eq!(records(&db, 1).len(), 3, "nessun nuovo record");
    }

    #[test]
    fn finalize_week_streaks_zeroes_inactive_users_only() {
        let db = db();
        db.with_conn(|c| {
            record_user_activity(c, 1, 9, 0, now()).unwrap(); // inattivo nella 10
            record_user_activity(c, 2, 10, 0, now()).unwrap(); // attivo nella 10
            finalize_week_streaks(c, 10).unwrap();
        });
        assert_eq!(streak(&db, 1).0, 0);
        assert_eq!(streak(&db, 1).1, 1, "il migliore non si tocca");
        assert_eq!(streak(&db, 2).0, 1);
    }

    // --- autori e ballottaggio -------------------------------------------------------------

    fn suggestion_with_authors(db: &Database, id: i64, week: i64, authors: &[i64]) {
        db.execute(
            "INSERT INTO suggestions(id,week,norm_name,name,tipo,review_status) VALUES(?,?,?,?, 'Gioco','approved')",
            rusqlite::params![id, week, format!("n{id}"), format!("Proposta {id}")],
        )
        .unwrap();
        for (index, author) in authors.iter().enumerate() {
            db.execute(
                "INSERT INTO proposals(suggestion_id,user_id,week,ts) VALUES(?,?,?,?)",
                rusqlite::params![id, author, week, 100 + index as i64],
            )
            .unwrap();
        }
    }

    #[test]
    fn author_stats_award_points_to_every_distinct_author() {
        let db = db();
        suggestion_with_authors(&db, 1, 3, &[10, 11]);
        db.execute("INSERT INTO proposals(suggestion_id,user_id,week,ts) VALUES(1,10,3,999)", []).unwrap(); // doppione
        db.with_conn(|c| increment_suggestion_authors_stat(c, 1, 3, AuthorStat::SupportsReceived, now()).unwrap());
        assert_eq!(activity(&db, 10, 3), (1, 2), "+2 punti per un sostegno ricevuto, una sola volta");
        assert_eq!(activity(&db, 11, 3), (1, 2));
        assert_eq!(one::<i64>(&db, "SELECT supports_received FROM user_stats WHERE user_id=10"), 1);
    }

    #[test]
    fn removing_from_ballot_revokes_ballot_points() {
        // Stesso caso di tests/test_weekly_cycle.py::test_removing_from_ballot_revokes_ballot_points.
        let db = db();
        suggestion_with_authors(&db, 1, 3, &[1]);
        db.with_conn(|c| record_user_activity(c, 1, 3, 1, now()).unwrap()); // la proposta iniziale
        let snapshot = |db: &Database| {
            (
                one::<i64>(db, "SELECT COALESCE((SELECT ballot_entries FROM user_stats WHERE user_id=1),0)"),
                activity(db, 1, 3).1,
            )
        };
        let before = snapshot(&db);
        db.with_conn(|c| increment_suggestion_authors_stat(c, 1, 3, AuthorStat::BallotEntries, now()).unwrap());
        assert_eq!(snapshot(&db), (before.0 + 1, before.1 + 5));
        db.with_conn(|c| revoke_ballot_entry_stats(c, 1, 3, now()).unwrap());
        assert_eq!(snapshot(&db), before);
        let season_points: i64 = one(&db, "SELECT points FROM season_activity WHERE user_id=1 AND week=3");
        assert_eq!(season_points, 1, "anche i punti stagione tornano a quelli iniziali");
        // Un secondo annullamento non porta i valori sotto zero.
        db.with_conn(|c| revoke_ballot_entry_stats(c, 1, 3, now()).unwrap());
        assert_eq!(snapshot(&db), (0, 0));
    }

    #[test]
    fn three_ballot_weeks_in_a_row_unlock_the_influencer_badge() {
        let db = db();
        for week in 5..=7 {
            suggestion_with_authors(&db, week, week, &[42]);
            db.with_conn(|c| increment_suggestion_authors_stat(c, week, week, AuthorStat::BallotEntries, now()).unwrap());
            let unlocked = one::<i64>(&db, "SELECT COUNT(*) FROM user_hidden_badges WHERE user_id=42");
            assert_eq!(unlocked, i64::from(week == 7), "settimana {week}");
        }
    }

    // --- livelli ---------------------------------------------------------------------------

    #[test]
    fn points_and_levels_follow_the_python_rules() {
        let stats = UserStats { proposals_submitted: 3, supports_received: 4, ballot_entries: 2, winning_proposals: 1, ..Default::default() };
        assert_eq!(contributor_points(&stats), 3 + 8 + 10 + 10);
        assert_eq!(contributor_points(&UserStats { supports_given: 99, ..Default::default() }), 0, "i sostegni dati non contano");

        let expected = [
            (0, "🌱 Nuovo"),
            (9, "🌱 Nuovo"),
            (10, "🥉 Bronzo"),
            (24, "🥉 Bronzo"),
            (25, "🥈 Argento"),
            (50, "🥇 Oro"),
            (100, "💎 Platino"),
            (199, "💎 Platino"),
            (200, "👑 Leggenda"),
            (5000, "👑 Leggenda"),
        ];
        for (points, level) in expected {
            assert_eq!(contributor_level(points), level, "{points} punti");
        }
        assert_eq!(contributor_level(-5), "🌱 Nuovo");
    }

    #[test]
    fn level_progress_points_at_the_next_threshold() {
        assert_eq!(contributor_level_progress(0), ("🌱 Nuovo", "🥉 Bronzo", 0, 10));
        assert_eq!(contributor_level_progress(10), ("🥉 Bronzo", "🥈 Argento", 10, 25));
        assert_eq!(contributor_level_progress(24), ("🥉 Bronzo", "🥈 Argento", 10, 25));
        assert_eq!(contributor_level_progress(199), ("💎 Platino", "👑 Leggenda", 100, 200));
        assert_eq!(contributor_level_progress(200), ("👑 Leggenda", "livello massimo", 200, 200));
    }

    #[test]
    fn leaderboard_orders_by_points_wins_supports_then_id() {
        let db = db();
        db.execute_batch(
            "INSERT INTO user_stats(user_id,proposals_submitted,supports_received,ballot_entries,winning_proposals) VALUES
                (1, 10, 0, 0, 0),   -- 10 punti
                (2, 0, 0, 0, 1),    -- 10 punti, 1 vittoria
                (3, 6, 2, 0, 0),    -- 10 punti, 2 sostegni
                (4, 10, 0, 0, 0),   -- 10 punti, come 1: decide l'ID
                (5, 0, 0, 0, 0),    -- 0 punti: escluso
                (6, 30, 0, 0, 0);   -- 30 punti: primo",
        )
        .unwrap();
        let rows = db.with_conn(|c| contributor_leaderboard_rows(c, 10).unwrap());
        assert_eq!(rows.iter().map(|r| r.user_id).collect::<Vec<_>>(), [6, 2, 3, 1, 4]);
        assert_eq!(rows[0].level, "🥈 Argento");
        assert_eq!(db.with_conn(|c| contributor_leaderboard_rows(c, 2).unwrap()).len(), 2);
    }

    // --- premi e record --------------------------------------------------------------------

    #[test]
    fn weekly_contributor_takes_the_top_scorers_once() {
        let db = db();
        db.execute_batch(
            "INSERT INTO weekly_activity(user_id,week,activity_count,points) VALUES
                (30,5,1,12),(10,5,1,12),(20,5,1,7),(10,6,1,99);",
        )
        .unwrap();
        let (winners, points) = db.with_conn(|c| record_weekly_contributor(c, 5, now()).unwrap());
        assert_eq!((winners, points), (vec![10, 30], 12), "a pari merito, in ordine di ID");
        let stored: String = one(&db, "SELECT user_ids FROM weekly_awards WHERE week=5");
        assert_eq!(stored, "[10, 30]", "stesso testo di json.dumps");
        // Dopo la registrazione i nuovi punti non cambiano il premio.
        db.execute("UPDATE weekly_activity SET points=100 WHERE user_id=20 AND week=5", []).unwrap();
        assert_eq!(db.with_conn(|c| record_weekly_contributor(c, 5, now()).unwrap()), (vec![10, 30], 12));
    }

    #[test]
    fn weekly_contributor_with_no_points_records_an_empty_award() {
        let db = db();
        assert_eq!(db.with_conn(|c| record_weekly_contributor(c, 5, now()).unwrap()), (vec![], 0));
        assert_eq!(one::<String>(&db, "SELECT user_ids FROM weekly_awards WHERE week=5"), "[]");
    }

    #[test]
    fn weekly_contributor_reads_awards_written_by_python() {
        let db = db();
        db.execute("INSERT INTO weekly_awards(week,user_ids,points,recorded_at) VALUES(4,'[\"11\", 22]',5,'x')", []).unwrap();
        assert_eq!(db.with_conn(|c| record_weekly_contributor(c, 4, now()).unwrap()), (vec![11, 22], 5));
        db.execute("INSERT INTO weekly_awards(week,user_ids,points,recorded_at) VALUES(3,'non json',5,'x')", []).unwrap();
        assert!(db.with_conn(|c| record_weekly_contributor(c, 3, now())).is_err());
    }

    #[test]
    fn community_records_only_move_up() {
        let db = db();
        let record = |db: &Database| -> Option<(i64, String, i64, Option<i64>)> {
            db.with_conn(|c| {
                c.query_row(
                    "SELECT metric,label,week,user_id FROM community_records WHERE record_key='k'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
                .unwrap()
            })
        };
        db.with_conn(|c| update_community_record(c, "k", 0, "zero", 1, None).unwrap());
        assert_eq!(record(&db), None, "metrica 0: nessun record");
        db.with_conn(|c| update_community_record(c, "k", 5, "A", 1, Some(9)).unwrap());
        assert_eq!(record(&db), Some((5, "A".into(), 1, Some(9))));
        db.with_conn(|c| update_community_record(c, "k", 5, "B", 2, None).unwrap());
        assert_eq!(record(&db).unwrap().1, "A", "parità: resta il primo");
        db.with_conn(|c| update_community_record(c, "k", 6, "C", 3, None).unwrap());
        assert_eq!(record(&db), Some((6, "C".into(), 3, None)));
    }

    #[test]
    fn community_records_are_derived_from_supporters_votes_streaks_and_wins() {
        let db = db();
        db.seed_defaults().unwrap();
        suggestion_with_authors(&db, 1, 4, &[10, 11]);
        db.execute("UPDATE suggestions SET name='Among Us' WHERE id=1", []).unwrap();
        db.execute("INSERT INTO suggestion_supports VALUES(1,12,4,0)", []).unwrap();
        let community: i64 = one(&db, "SELECT id FROM polls WHERE is_community=1");
        db.execute("INSERT INTO options(id,poll_id,label,suggestion_id) VALUES(100,?,'Among Us',1)", [community]).unwrap();
        db.execute_batch("INSERT INTO votes VALUES(100,1),(100,2);").unwrap();
        db.execute_batch(
            "INSERT INTO user_streaks VALUES(7,2,5,4),(8,1,5,4),(9,0,2,3);
             INSERT INTO user_stats(user_id,winning_proposals) VALUES(7,1),(8,3),(9,3);",
        )
        .unwrap();
        db.with_conn(|c| update_community_records(c, 4).unwrap());
        let rows: Vec<(String, i64, String, i64, Option<i64>)> = db.with_conn(|c| {
            c.prepare("SELECT record_key,metric,label,week,user_id FROM community_records ORDER BY record_key")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        });
        assert_eq!(
            rows,
            [
                ("longest_streak".to_string(), 5, "<@7>".to_string(), 4, Some(7)), // pari: l'ID minore
                ("most_supported".to_string(), 3, "Among Us".to_string(), 4, None),
                ("most_voted".to_string(), 2, "Among Us".to_string(), 4, None),
                ("most_wins".to_string(), 3, "<@8>".to_string(), 4, Some(8)),
            ]
        );
    }

    // --- vincitore -------------------------------------------------------------------------

    fn ballot_with(db: &Database, entries: &[(i64, &[i64])]) {
        // (ID proposta, votanti)
        db.seed_defaults().unwrap();
        let community: i64 = one(db, "SELECT id FROM polls WHERE is_community=1");
        for (suggestion_id, voters) in entries {
            db.execute(
                "INSERT INTO options(id,poll_id,label,suggestion_id) VALUES(?,?,?,?)",
                rusqlite::params![1000 + suggestion_id, community, format!("Proposta {suggestion_id}"), suggestion_id],
            )
            .unwrap();
            for voter in *voters {
                db.execute("INSERT INTO votes VALUES(?,?)", [1000 + suggestion_id, *voter]).unwrap();
            }
        }
    }

    #[test]
    fn tie_break_prefers_more_supporters() {
        // Stesso caso di tests/test_features.py::test_tie_break_prefers_more_supporters.
        let db = db();
        suggestion_with_authors(&db, 1, 3, &[1]);
        suggestion_with_authors(&db, 2, 3, &[2]);
        db.execute("INSERT INTO suggestion_supports VALUES(2,30,3,0)", []).unwrap();
        ballot_with(&db, &[(1, &[101]), (2, &[102])]);
        let winner = db.with_conn(|c| weekly_winner(c, 3).unwrap()).unwrap();
        assert_eq!(winner.result.name, "Proposta 2");
        assert!(winner.tie_break.unwrap().contains("sostenitori"));
    }

    #[test]
    fn tie_break_falls_back_to_the_first_submission_and_then_votes_win() {
        let db = db();
        suggestion_with_authors(&db, 1, 3, &[1]);
        suggestion_with_authors(&db, 2, 3, &[2]);
        db.execute("UPDATE proposals SET ts=50 WHERE suggestion_id=2", []).unwrap(); // la 2 è arrivata prima
        ballot_with(&db, &[(1, &[101]), (2, &[102])]);
        let winner = db.with_conn(|c| weekly_winner(c, 3).unwrap()).unwrap();
        assert_eq!(winner.result.suggestion_id, 2);
        assert_eq!(winner.tie_break, Some("a parità di voti e sostenitori vince la proposta inviata per prima"));

        db.execute("INSERT INTO votes VALUES(1001,103)", []).unwrap(); // ora la 1 ha più voti
        let winner = db.with_conn(|c| weekly_winner(c, 3).unwrap()).unwrap();
        assert_eq!((winner.result.suggestion_id, winner.tie_break), (1, None), "nessuno spareggio");
    }

    #[test]
    fn no_winner_without_options_or_votes() {
        let db = db();
        assert_eq!(db.with_conn(|c| weekly_winner(c, 3).unwrap()), None);
        suggestion_with_authors(&db, 1, 3, &[1]);
        ballot_with(&db, &[(1, &[])]);
        assert_eq!(db.with_conn(|c| weekly_winner(c, 3).unwrap()), None, "nessun voto");
    }

    #[test]
    fn winner_stats_are_awarded_once_per_author_and_week() {
        let db = db();
        suggestion_with_authors(&db, 1, 3, &[10, 11]);
        ballot_with(&db, &[(1, &[101, 102])]);
        let winner = db.with_conn(|c| record_weekly_winner_stats(c, 3, now(), None).unwrap()).unwrap();
        assert_eq!(winner.result.votes, 2);
        db.with_conn(|c| record_weekly_winner_stats(c, 3, now(), None).unwrap());
        for user in [10, 11] {
            assert_eq!(
                one::<i64>(&db, &format!("SELECT winning_proposals FROM user_stats WHERE user_id={user}")),
                1,
                "la seconda chiamata non raddoppia"
            );
            assert_eq!(activity(&db, user, 3), (1, 10), "+10 punti una volta sola");
        }
        assert_eq!(one::<i64>(&db, "SELECT COUNT(*) FROM user_winning_proposals"), 2);
    }

    #[test]
    fn winner_stats_do_nothing_without_a_winner() {
        let db = db();
        assert_eq!(db.with_conn(|c| record_weekly_winner_stats(c, 3, now(), None).unwrap()), None);
        assert_eq!(one::<i64>(&db, "SELECT COUNT(*) FROM user_winning_proposals"), 0);
    }
}
