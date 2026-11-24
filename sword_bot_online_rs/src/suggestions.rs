//! Proposte della community: registrazione, sostegno, selezione staff (equivalente di
//! `sfo/suggestions.py`).
//!
//! Le funzioni che in Python usano `with transaction()` qui prendono `&Database` e aprono la
//! transazione da sole; l'orologio (`datetime.now(TZ)`, `time.time()`) è il parametro `now`.
//! Un errore del database nelle operazioni "da utente" (`register_*`) viene loggato e
//! trasformato in un messaggio generico, come in Python; nelle operazioni staff
//! (`pick`/`unpick`/`merge`) si propaga come `Err`, come il `sqlite3.Error` sollevato in Python.
// Verrà usato dai comandi, dai pulsanti di proposta e dal sito.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv6Addr};

use chrono::{DateTime, Timelike};
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, Row, params};
use tracing::error;

use crate::config::{DEFAULT_TIPI, SIMILAR_SUGGESTION_THRESHOLD};
use crate::database::{self, Connected, Database, DbResult};
use crate::formatting::{casefold, normalize};
use crate::queries::{CurrentSuggestion, suggestion_supporter_counts};
use crate::schedule::{self, isoformat};
use crate::settings;
use crate::stats::{
    AuthorStat, HiddenBadge, UserStat, increment_suggestion_authors_stat, increment_user_stat,
    record_user_activity, revoke_ballot_entry_stats, unlock_hidden_badge,
};

// --- somiglianza tra nomi --------------------------------------------------------------------

/// `SequenceMatcher(None, first, second).ratio()` di `difflib`: `2 * M / T`, dove `M` è il numero
/// di caratteri in comune trovati cercando ricorsivamente la sottostringa comune più lunga
/// (a parità, la prima in `first` e poi in `second`) e `T` la somma delle lunghezze.
///
/// Non implementa l'euristica `autojunk` di Python (che scatta da 200 caratteri): i nomi
/// normalizzati confrontati qui ne hanno al massimo 80.
pub fn sequence_ratio(first: &str, second: &str) -> f64 {
    let a: Vec<char> = first.chars().collect();
    let b: Vec<char> = second.chars().collect();
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    2.0 * matching_characters(&a, &b) as f64 / total as f64
}

fn matching_characters(a: &[char], b: &[char]) -> usize {
    let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
    for (index, ch) in b.iter().enumerate() {
        b2j.entry(*ch).or_default().push(index);
    }
    let mut matched = 0;
    let mut pending = vec![(0, a.len(), 0, b.len())];
    while let Some((alo, ahi, blo, bhi)) = pending.pop() {
        let (i, j, size) = longest_match(a, &b2j, alo, ahi, blo, bhi);
        if size == 0 {
            continue;
        }
        matched += size;
        if alo < i && blo < j {
            pending.push((alo, i, blo, j));
        }
        if i + size < ahi && j + size < bhi {
            pending.push((i + size, ahi, j + size, bhi));
        }
    }
    matched
}

/// Sottostringa comune più lunga di `a[alo..ahi]` e `b[blo..bhi]`: `(inizio in a, inizio in b, lunghezza)`.
fn longest_match(
    a: &[char],
    b2j: &HashMap<char, Vec<usize>>,
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let (mut best_i, mut best_j, mut best_size) = (alo, blo, 0);
    // j2len[j] = lunghezza del match che finisce in a[i-1] e b[j].
    let mut j2len: HashMap<usize, usize> = HashMap::new();
    for (i, ch) in a.iter().enumerate().take(ahi).skip(alo) {
        let mut new_j2len: HashMap<usize, usize> = HashMap::new();
        for &j in b2j.get(ch).map_or(&[][..], Vec::as_slice) {
            if j < blo {
                continue;
            }
            if j >= bhi {
                break;
            }
            let k = j.checked_sub(1).and_then(|previous| j2len.get(&previous)).copied().unwrap_or(0) + 1;
            new_j2len.insert(j, k);
            if k > best_size {
                (best_i, best_j, best_size) = (i + 1 - k, j + 1 - k, k);
            }
        }
        j2len = new_j2len;
    }
    (best_i, best_j, best_size)
}

/// Somiglianza tra due nomi normalizzati, da 0 a 1.
pub fn similarity(first: &str, second: &str) -> f64 {
    if first.is_empty() || second.is_empty() {
        return 0.0;
    }
    if first == second {
        return 1.0;
    }
    let (shorter, longer) = if second.chars().count() < first.chars().count() { (second, first) } else { (first, second) };
    let (short_len, long_len) = (shorter.chars().count(), longer.chars().count());
    // "jackbox" dentro "jackboxpartypack": quasi certamente la stessa proposta.
    if short_len >= 4 && longer.contains(shorter) {
        return f64::max(0.85, short_len as f64 / long_len as f64);
    }
    sequence_ratio(first, second)
}

/// Una proposta della settimana con nome simile.
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarSuggestion {
    pub id: i64,
    pub name: String,
    pub norm_name: String,
    pub tipo: String,
    pub review_status: String,
    pub status: Option<String>,
    pub score: f64,
}

/// Proposte della settimana con nome simile (esclusi i duplicati esatti e le rifiutate), le più
/// simili per prime.
pub fn similar_suggestions(
    db: &impl Connected,
    name: &str,
    tipo: Option<&str>,
    limit: usize,
    exclude_id: Option<i64>,
) -> DbResult<Vec<SimilarSuggestion>> {
    let norm = normalize(name);
    if norm.is_empty() {
        return Ok(Vec::new());
    }
    let week = database::current_week(db)?;
    let candidates: Vec<(i64, String, String, String, String, Option<String>)> = db.with_conn(|connection| {
        connection
            .prepare(
                "SELECT id, name, norm_name, tipo, review_status, status
                 FROM suggestions
                 WHERE week=? AND review_status<>'rejected'",
            )?
            .query_map([week], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?))
            })?
            .collect::<DbResult<_>>()
    })?;
    let mut scored = Vec::new();
    for (id, name, norm_name, row_tipo, review_status, status) in candidates {
        if Some(id) == exclude_id {
            continue;
        }
        if norm_name == norm && tipo.is_none_or(|wanted| casefold(&row_tipo) == casefold(wanted)) {
            continue; // duplicato esatto: lo gestisce già register_suggestion
        }
        let score = similarity(&norm, &norm_name);
        if score >= SIMILAR_SUGGESTION_THRESHOLD {
            scored.push(SimilarSuggestion { id, name, norm_name, tipo: row_tipo, review_status, status, score });
        }
    }
    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    scored.truncate(limit);
    Ok(scored)
}

// --- notifiche private -----------------------------------------------------------------------

/// Imposta le notifiche private (DM) per un utente.
pub fn set_dm_notifications(db: &Database, user_id: i64, enabled: bool) -> DbResult<()> {
    db.transaction(|tx| -> DbResult<()> {
        tx.execute(
            "INSERT INTO user_notification_preferences(user_id, dm_enabled) VALUES(?,?)
             ON CONFLICT(user_id) DO UPDATE SET dm_enabled=excluded.dm_enabled",
            params![user_id, i64::from(enabled)],
        )?;
        Ok(())
    })
}

/// L'utente ha già scelto se ricevere i DM (a prescindere dalla scelta).
pub fn has_notification_preference(db: &impl Connected, user_id: i64) -> DbResult<bool> {
    db.with_conn(|connection| {
        connection
            .query_row("SELECT 1 FROM user_notification_preferences WHERE user_id=?", [user_id], |_| Ok(()))
            .optional()
            .map(|found| found.is_some())
    })
}

// --- raccomandazione staff -------------------------------------------------------------------

/// Punteggio con cui lo staff viene consigliato su cosa scegliere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recommendation {
    pub score: i64,
    pub supporters: i64,
    pub waiting_days: i64,
    pub won_recently: bool,
}

/// Consiglia una proposta tra `suggestions`: più sostenitori e più attesa (fino a 14 giorni) alzano
/// il punteggio, aver vinto nelle ultime 3 settimane lo abbassa di 25. `now` è un timestamp Unix.
///
/// Restituisce `(ID consigliato, punteggi per ID)`; `None` e mappa vuota senza proposte.
pub fn staff_recommendation(
    db: &impl Connected,
    suggestions: &[CurrentSuggestion],
    now: i64,
) -> DbResult<(Option<i64>, HashMap<i64, Recommendation>)> {
    if suggestions.is_empty() {
        return Ok((None, HashMap::new()));
    }
    let week = database::current_week(db)?;
    let ids: Vec<i64> = suggestions.iter().map(|suggestion| suggestion.id).collect();
    let supporter_counts = suggestion_supporter_counts(db, week, &ids)?;
    let recent_winners: HashSet<String> = db.with_conn(|connection| -> DbResult<Vec<String>> {
        connection
            .prepare("SELECT name FROM user_winning_proposals WHERE week>=?")?
            .query_map([week - 3], |row| row.get::<_, String>(0))?
            .collect()
    })?
    .iter()
    .map(|name| normalize(name))
    .collect();

    let mut scored = HashMap::new();
    for suggestion in suggestions {
        let waiting_days = match suggestion.created_ts {
            Some(created) if created != 0 => (now - created).div_euclid(86400).clamp(0, 14),
            _ => 0,
        };
        let won_recently = recent_winners.contains(&suggestion.norm_name);
        let supporters = supporter_counts.get(&suggestion.id).copied().unwrap_or(0);
        let score = supporters * 10 + waiting_days - if won_recently { 25 } else { 0 };
        scored.insert(suggestion.id, Recommendation { score, supporters, waiting_days, won_recently });
    }
    let recommendation = scored
        .iter()
        .max_by_key(|(id, entry)| (entry.score, entry.supporters, entry.waiting_days, -**id))
        .map(|(id, _)| *id);
    Ok((recommendation, scored))
}

// --- sostegno --------------------------------------------------------------------------------

/// Risultato dell'operazione di sostegno a una proposta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupportResult {
    pub message: String,
    pub success: bool,
    /// Numero di sostenitori se è stato appena raggiunto un traguardo (25, o multipli di 10 da 10).
    pub milestone: Option<i64>,
}

impl SupportResult {
    fn failure(message: impl Into<String>) -> Self {
        Self { message: message.into(), success: false, milestone: None }
    }
}

/// Aggiunge il sostegno di `user_id` a una proposta approvata della settimana.
pub fn register_suggestion_support(
    db: &Database,
    user_id: i64,
    suggestion_id: i64,
    now: DateTime<Tz>,
) -> SupportResult {
    let occurred_at = now.timestamp();
    let outcome = db.transaction(|tx| -> DbResult<SupportResult> {
        let week = database::current_week(tx)?;
        let suggestion: Option<(String, String)> = tx
            .query_row(
                "SELECT name, review_status FROM suggestions WHERE id=? AND week=?",
                [suggestion_id, week],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((name, review_status)) = suggestion else {
            return Ok(SupportResult::failure("❌ Questa proposta non è più disponibile."));
        };
        if review_status != "approved" {
            return Ok(SupportResult::failure(
                "⚠️ Questa proposta deve essere approvata dallo staff prima del supporto.",
            ));
        }
        let already_proposed = tx
            .query_row(
                "SELECT 1 FROM proposals WHERE suggestion_id=? AND user_id=? AND week=? LIMIT 1",
                params![suggestion_id, user_id, week],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if already_proposed {
            return Ok(SupportResult::failure(
                "⚠️ Hai già proposto questa attività: il tuo sostegno è già conteggiato.",
            ));
        }
        let already_supported = tx
            .query_row(
                "SELECT 1 FROM suggestion_supports WHERE suggestion_id=? AND user_id=?",
                [suggestion_id, user_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if already_supported {
            return Ok(SupportResult::failure("⚠️ Hai già supportato questa proposta."));
        }
        let supporters = |tx: &rusqlite::Transaction<'_>| -> DbResult<i64> {
            Ok(suggestion_supporter_counts(tx, week, &[suggestion_id])?.get(&suggestion_id).copied().unwrap_or(0))
        };
        let previous_supporters = supporters(tx)?;
        tx.execute(
            "INSERT INTO suggestion_supports(suggestion_id,user_id,week,created_at) VALUES(?,?,?,?)",
            params![suggestion_id, user_id, week, occurred_at],
        )?;
        tx.execute("INSERT INTO community_activity(action,occurred_at) VALUES('support',?)", [occurred_at])?;
        increment_user_stat(tx, user_id, UserStat::SupportsGiven)?;
        record_user_activity(tx, user_id, week, 0, now)?;
        increment_suggestion_authors_stat(tx, suggestion_id, week, AuthorStat::SupportsReceived, now)?;
        let new_supporters = supporters(tx)?;
        let milestone = (new_supporters > previous_supporters
            && (new_supporters == 25 || (new_supporters >= 10 && new_supporters % 10 == 0)))
        .then_some(new_supporters);
        Ok(SupportResult { message: format!("✅ Hai supportato **{name}**. Grazie!"), success: true, milestone })
    });
    outcome.unwrap_or_else(|e| {
        error!("Errore SQLite durante il supporto di una proposta: {e}");
        SupportResult::failure("❌ Non è stato possibile registrare il supporto. Riprova tra poco.")
    })
}

// --- registrazione ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterResult {
    pub message: String,
    pub success: bool,
    /// ID della proposta creata (assente se si è unita a una esistente o in caso di errore).
    pub created_id: Option<i64>,
}

impl RegisterResult {
    fn failure(message: impl Into<String>) -> Self {
        Self { message: message.into(), success: false, created_id: None }
    }
}

/// Verifica se una etichetta DNS è valida: lettere, cifre e trattini (1-63 caratteri).
fn is_dns_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

/// Nome di host valido: indirizzo IP, o etichette DNS (lettere, cifre, trattini) separate da punti,
/// con un punto finale facoltativo.
fn is_valid_hostname(hostname: &str) -> bool {
    if hostname.parse::<IpAddr>().is_ok() {
        return true;
    }
    let name = hostname.strip_suffix('.').unwrap_or(hostname);
    !name.is_empty() && name.split('.').all(is_dns_label)
}

/// Il link è accettabile? Stesse regole di `urlparse` in Python: schema `http`/`https`, host
/// presente e valido, nessuna credenziale (`utente:password@`), porta numerica tra 0 e 65535.
pub fn is_valid_link(link: &str) -> bool {
    // urlsplit scarta i caratteri di controllo e gli spazi iniziali, e tab e a capo ovunque.
    let cleaned: String = link
        .trim_start_matches(|ch: char| ch <= ' ')
        .chars()
        .filter(|ch| !matches!(ch, '\t' | '\r' | '\n'))
        .collect();

    // Schema: testo prima del primo ':' se inizia con una lettera ASCII ed è fatto di lettere,
    // cifre, '+', '-', '.'.
    let (scheme, rest) = match cleaned.find(':') {
        Some(colon)
            if colon > 0
                && cleaned.as_bytes()[0].is_ascii_alphabetic()
                && cleaned[..colon].bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) =>
        {
            (cleaned[..colon].to_ascii_lowercase(), &cleaned[colon + 1..])
        }
        _ => (String::new(), cleaned.as_str()),
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let Some(after_slashes) = rest.strip_prefix("//") else {
        return false; // nessun netloc
    };
    let netloc = &after_slashes[..after_slashes.find(['/', '?', '#']).unwrap_or(after_slashes.len())];
    if netloc.is_empty() {
        return false;
    }
    if netloc.contains('[') != netloc.contains(']') {
        return false; // parentesi quadre non bilanciate
    }
    // Credenziali: qualsiasi `@` nel netloc significa utente/password.
    if netloc.contains('@') {
        return false;
    }
    let hostinfo = netloc;
    let (hostname, port, is_ip_literal) = if let Some(open) = hostinfo.find('[') {
        // Host IPv6 tra parentesi: deve essere un indirizzo IPv6 valido.
        let bracketed = &hostinfo[open + 1..];
        let close = bracketed.find(']').unwrap_or(bracketed.len());
        let host = &bracketed[..close];
        // IPv6, con zona facoltativa (`fe80::1%eth0`) come `ipaddress.ip_address` di Python.
        let (address, zone) = host.split_once('%').map_or((host, None), |(address, zone)| (address, Some(zone)));
        if address.parse::<Ipv6Addr>().is_err() || zone.is_some_and(|zone| zone.is_empty() || zone.contains('%')) {
            return false;
        }
        let after = &bracketed[(close + 1).min(bracketed.len())..];
        (host, after.split_once(':').map(|(_, port)| port), true)
    } else {
        match hostinfo.split_once(':') {
            Some((host, port)) => (host, Some(port), false),
            None => (hostinfo, None, false),
        }
    };
    // `parsed.port` solleva ValueError se la porta non è un numero tra 0 e 65535.
    if let Some(port) = port.filter(|port| !port.is_empty()) {
        match port.parse::<u32>() {
            Ok(number) if port.bytes().all(|b| b.is_ascii_digit()) && number <= 65535 => {}
            _ => return false,
        }
    }
    !hostname.is_empty() && (is_ip_literal || is_valid_hostname(hostname))
}

/// Passi possibili durante la registrazione di una proposta.
enum RegisterStep {
    Reject(String),
    Registered { used_before: i64, created_id: Option<i64> },
}

/// Registra una proposta (nuova o in aggiunta a una uguale già presente) e aggiorna statistiche e
/// attività. `now` è l'istante dell'invio, in ora italiana.
pub fn register_suggestion(
    db: &Database,
    user_id: i64,
    nome: &str,
    tipo: &str,
    link: Option<&str>,
    now: DateTime<Tz>,
) -> RegisterResult {
    let nome = nome.trim();
    let tipo = tipo.trim();
    let link = link.map(str::trim).filter(|link| !link.is_empty());
    let name_length = nome.chars().count();
    if !(2..=80).contains(&name_length) {
        return RegisterResult::failure("❌ Il nome deve contenere da 2 a 80 caratteri.");
    }
    let configured = match database::setting(db, "tipi") {
        Ok(setting) => setting.filter(|text| !text.is_empty()).unwrap_or_else(|| DEFAULT_TIPI.to_string()),
        Err(e) => {
            error!("Impossibile leggere i tipi di proposta: {e}");
            return RegisterResult::failure("❌ Non è stato possibile registrare la proposta. Riprova tra poco.");
        }
    };
    let tipi: Vec<&str> = configured.split(',').map(str::trim).collect();
    let Some(matched) = tipi.iter().find(|candidate| candidate.to_lowercase() == tipo.to_lowercase()) else {
        return RegisterResult::failure(format!("❌ Tipo non valido. Scegli tra: {}.", tipi.join(", ")));
    };
    let norm = normalize(nome);
    if norm.is_empty() {
        return RegisterResult::failure("❌ Il nome deve contenere lettere o numeri.");
    }
    if let Some(link) = link {
        if !is_valid_link(link) {
            return RegisterResult::failure("❌ Il link non sembra valido (deve iniziare con http).");
        }
    }

    let timestamp = now.timestamp();
    let step = db.transaction(|tx| -> DbResult<RegisterStep> {
        let week = database::current_week(tx)?;
        let (used, last_ts): (i64, Option<i64>) = tx.query_row(
            "SELECT COUNT(*), MAX(ts) FROM proposals WHERE user_id=? AND week=?",
            [user_id, week],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let max_proposals = settings::max_proposals(tx)?;
        if used >= max_proposals {
            let retry_at = schedule::next_scheduled_reset(tx, now)?.timestamp();
            return Ok(RegisterStep::Reject(format!(
                "⚠️ Hai già fatto {max_proposals} proposte questa settimana. Puoi riprovare dopo <t:{retry_at}:R>."
            )));
        }
        let cooldown = settings::cooldown_seconds(tx)?;
        if last_ts.is_some_and(|last| last != 0 && timestamp - last < cooldown) {
            return Ok(RegisterStep::Reject("⚠️ Aspetta qualche secondo prima di una nuova proposta.".into()));
        }

        let existing: Option<(i64, Option<String>, String)> = tx
            .query_row(
                "SELECT id, link, review_status FROM suggestions WHERE week=? AND norm_name=? AND tipo=?",
                params![week, norm, matched],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let (suggestion_id, created_id) = match existing {
            Some((_, _, review_status)) if review_status == "rejected" => {
                return Ok(RegisterStep::Reject("❌ Questa proposta è stata rifiutata dallo staff.".into()));
            }
            Some((id, existing_link, _)) => {
                if let (Some(link), None | Some("")) = (link, existing_link.as_deref()) {
                    tx.execute("UPDATE suggestions SET link=? WHERE id=?", params![link, id])?;
                }
                (id, None)
            }
            None => {
                tx.execute(
                    "INSERT INTO suggestions (week,norm_name,name,tipo,link,review_status)
                     VALUES(?,?,?,?,?,'pending')",
                    params![week, norm, nome, matched, link],
                )?;
                let id = tx.last_insert_rowid();
                (id, Some(id))
            }
        };
        tx.execute(
            "INSERT INTO proposals(suggestion_id,user_id,week,ts,raw_name,raw_link) VALUES(?,?,?,?,?,?)",
            params![suggestion_id, user_id, week, timestamp, nome, link],
        )?;
        tx.execute("INSERT INTO community_activity(action,occurred_at) VALUES('proposal',?)", [timestamp])?;
        if (2..5).contains(&now.hour()) {
            unlock_hidden_badge(tx, user_id, HiddenBadge::NightOwl, now)?;
        }
        increment_user_stat(tx, user_id, UserStat::ProposalsSubmitted)?;
        record_user_activity(tx, user_id, week, 1, now)?;
        Ok(RegisterStep::Registered { used_before: used, created_id })
    });
    match step {
        Ok(RegisterStep::Reject(message)) => RegisterResult::failure(message),
        Ok(RegisterStep::Registered { used_before, created_id }) => {
            let remaining = settings::max_proposals(db).map_or(0, |max| max - used_before - 1);
            RegisterResult {
                message: format!(
                    "✅ Proposta registrata: **{nome}** ({matched}). Te ne restano `{remaining}` questa settimana."
                ),
                success: true,
                created_id,
            }
        }
        Err(e) => {
            error!("Errore SQLite durante la registrazione di una proposta: {e}");
            RegisterResult::failure("❌ Non è stato possibile registrare la proposta. Riprova tra poco.")
        }
    }
}

// --- selezione staff -------------------------------------------------------------------------

/// Riga completa di `suggestions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestionRow {
    pub id: i64,
    pub week: i64,
    pub norm_name: String,
    pub name: String,
    pub tipo: String,
    pub link: Option<String>,
    /// `pending` o `picked`.
    pub status: Option<String>,
    pub review_status: String,
    pub picked_by: Option<i64>,
    pub picked_at: Option<String>,
}

impl SuggestionRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            week: row.get("week")?,
            norm_name: row.get("norm_name")?,
            name: row.get("name")?,
            tipo: row.get("tipo")?,
            link: row.get("link")?,
            status: row.get("status")?,
            review_status: row.get("review_status")?,
            picked_by: row.get("picked_by")?,
            picked_at: row.get("picked_at")?,
        })
    }

    fn is_picked(&self) -> bool {
        self.status.as_deref() == Some("picked")
    }
}

fn find_suggestion(connection: &Connection, id: i64, week: i64) -> DbResult<Option<SuggestionRow>> {
    connection
        .query_row("SELECT * FROM suggestions WHERE id=? AND week=?", [id, week], SuggestionRow::from_row)
        .optional()
}

/// Quando una proposta con lo stesso nome e tipo era già stata scelta in una settimana precedente.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorPick {
    pub picked_at: Option<String>,
}

/// Esito di [`pick_suggestion_in_db`]: `error` è `None` se la proposta è entrata nel ballottaggio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickOutcome {
    pub error: Option<String>,
    /// La proposta così com'era prima della selezione.
    pub suggestion: Option<SuggestionRow>,
    pub poll_id: Option<i64>,
    pub prior: Option<PriorPick>,
}

/// Inserisce una proposta approvata nel ballottaggio community.
pub fn pick_suggestion_in_db(
    db: &Database,
    suggestion_id: i64,
    actor_id: i64,
    now: DateTime<Tz>,
) -> DbResult<PickOutcome> {
    db.transaction(|tx| -> DbResult<PickOutcome> {
        let week = database::current_week(tx)?;
        let suggestion = find_suggestion(tx, suggestion_id, week)?;
        let community: Option<i64> = tx
            .query_row("SELECT id FROM polls WHERE is_community=1 AND active=1", [], |row| row.get(0))
            .optional()?;
        let (Some(suggestion), Some(poll_id)) = (suggestion.clone(), community) else {
            return Ok(PickOutcome {
                error: Some("❌ Proposta o sondaggio community non disponibili.".into()),
                suggestion,
                poll_id: None,
                prior: None,
            });
        };
        let refuse = |message: String, suggestion: SuggestionRow| PickOutcome {
            error: Some(message),
            suggestion: Some(suggestion),
            poll_id: Some(poll_id),
            prior: None,
        };
        if suggestion.review_status != "approved" {
            return Ok(refuse("⚠️ La proposta deve essere approvata prima del ballottaggio.".into(), suggestion));
        }
        if suggestion.is_picked() {
            return Ok(refuse("⚠️ Questa proposta è già nel ballottaggio.".into(), suggestion));
        }
        let slots = settings::ballot_slots(tx)?;
        let taken: i64 = tx.query_row(
            "SELECT COUNT(*) FROM options WHERE poll_id=? AND suggestion_id IS NOT NULL",
            [poll_id],
            |row| row.get(0),
        )?;
        if taken >= slots {
            return Ok(refuse(
                format!("⚠️ Gli slot ({slots}) sono pieni: togli una proposta prima di continuare."),
                suggestion,
            ));
        }
        let prior = tx
            .query_row(
                "SELECT picked_at FROM picked_suggestions
                 WHERE norm_name=? AND tipo=? AND week<?
                 ORDER BY week DESC LIMIT 1",
                params![suggestion.norm_name, suggestion.tipo, week],
                |row| Ok(PriorPick { picked_at: row.get(0)? }),
            )
            .optional()?;
        tx.execute(
            "INSERT INTO options(poll_id,label,suggestion_id) VALUES(?,?,?)",
            params![poll_id, suggestion.name, suggestion.id],
        )?;
        tx.execute(
            "UPDATE suggestions SET status='picked', picked_by=?, picked_at=?
             WHERE id=? AND week=? AND status='pending'",
            params![actor_id, isoformat(now), suggestion.id, week],
        )?;
        increment_suggestion_authors_stat(tx, suggestion.id, week, AuthorStat::BallotEntries, now)?;
        Ok(PickOutcome { error: None, suggestion: Some(suggestion), poll_id: Some(poll_id), prior })
    })
}

/// Toglie una proposta dal ballottaggio. Restituisce la proposta (com'era) e il sondaggio da cui è
/// uscita, o `None` se non era nel ballottaggio.
pub fn unpick_suggestion(
    db: &Database,
    suggestion_id: i64,
    now: DateTime<Tz>,
) -> DbResult<Option<(SuggestionRow, Option<i64>)>> {
    db.transaction(|tx| -> DbResult<Option<(SuggestionRow, Option<i64>)>> {
        let week = database::current_week(tx)?;
        let Some(suggestion) = tx
            .query_row(
                "SELECT * FROM suggestions WHERE id=? AND week=? AND status='picked'",
                [suggestion_id, week],
                SuggestionRow::from_row,
            )
            .optional()?
        else {
            return Ok(None);
        };
        let poll_id: Option<i64> = tx
            .query_row("SELECT poll_id FROM options WHERE suggestion_id=?", [suggestion_id], |row| row.get(0))
            .optional()?;
        tx.execute("DELETE FROM options WHERE suggestion_id=?", [suggestion_id])?;
        tx.execute(
            "UPDATE suggestions SET status='pending', picked_by=NULL, picked_at=NULL WHERE id=? AND week=?",
            [suggestion_id, week],
        )?;
        revoke_ballot_entry_stats(tx, suggestion_id, week, now)?;
        Ok(Some((suggestion, poll_id)))
    })
}

/// Esito di [`merge_suggestions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    pub ok: bool,
    pub message: String,
    pub keep: Option<SuggestionRow>,
    pub duplicate: Option<SuggestionRow>,
}

/// Unisce la proposta duplicata in quella principale: proposte, opzioni e sostegni passano alla
/// principale (chi aveva sostenuto entrambe resta contato una volta) e la duplicata viene eliminata.
pub fn merge_suggestions(db: &Database, keep_id: i64, duplicate_id: i64) -> DbResult<MergeOutcome> {
    db.transaction(|tx| -> DbResult<MergeOutcome> {
        let week = database::current_week(tx)?;
        let keep = find_suggestion(tx, keep_id, week)?;
        let duplicate = find_suggestion(tx, duplicate_id, week)?;
        let refuse = |message: &str, keep: Option<SuggestionRow>, duplicate: Option<SuggestionRow>| MergeOutcome {
            ok: false,
            message: message.to_string(),
            keep,
            duplicate,
        };
        let (Some(keep), Some(duplicate)) = (keep.clone(), duplicate.clone()) else {
            return Ok(refuse("❌ Scegli due proposte diverse di questa settimana.", keep, duplicate));
        };
        if keep.id == duplicate.id {
            return Ok(refuse("❌ Scegli due proposte diverse di questa settimana.", Some(keep), Some(duplicate)));
        }
        if keep.tipo != duplicate.tipo {
            return Ok(refuse("❌ Non puoi unire proposte di tipo diverso.", Some(keep), Some(duplicate)));
        }
        if duplicate.is_picked() {
            return Ok(refuse("⚠️ Prima togli la duplicata dal ballottaggio.", Some(keep), Some(duplicate)));
        }
        tx.execute("UPDATE proposals SET suggestion_id=? WHERE suggestion_id=?", [keep.id, duplicate.id])?;
        tx.execute("UPDATE options SET suggestion_id=? WHERE suggestion_id=?", [keep.id, duplicate.id])?;
        // Sposta i sostegni prima di cancellare la duplicata: il DELETE li eliminerebbe in cascata.
        tx.execute(
            "INSERT OR IGNORE INTO suggestion_supports(suggestion_id,user_id,week,created_at)
             SELECT ?, user_id, week, created_at FROM suggestion_supports
             WHERE suggestion_id=?",
            [keep.id, duplicate.id],
        )?;
        if keep.link.as_deref().is_none_or(str::is_empty) {
            if let Some(link) = duplicate.link.as_deref().filter(|link| !link.is_empty()) {
                tx.execute("UPDATE suggestions SET link=? WHERE id=?", params![link, keep.id])?;
            }
        }
        tx.execute("DELETE FROM suggestions WHERE id=?", [duplicate.id])?;
        Ok(MergeOutcome {
            ok: true,
            message: format!("✅ Unite: «{}» → «{}».", duplicate.name, keep.name),
            keep: Some(keep),
            duplicate: Some(duplicate),
        })
    })
}

/// Rimette "in attesa" le proposte scelte di un sondaggio che sta per essere eliminato.
///
/// Va chiamata dentro la transazione che elimina il sondaggio (`connection` è la `Transaction`).
pub fn release_poll_suggestions(connection: &Connection, poll_id: i64, now: DateTime<Tz>) -> DbResult<()> {
    let week = database::current_week(connection)?;
    let suggestion_ids: Vec<i64> = connection
        .prepare("SELECT DISTINCT suggestion_id FROM options WHERE poll_id=? AND suggestion_id IS NOT NULL")?
        .query_map([poll_id], |row| row.get(0))?
        .collect::<DbResult<_>>()?;
    for suggestion_id in suggestion_ids {
        let released = connection.execute(
            "UPDATE suggestions SET status='pending', picked_by=NULL, picked_at=NULL
             WHERE id=? AND status='picked'",
            [suggestion_id],
        )?;
        if released > 0 {
            revoke_ballot_entry_stats(connection, suggestion_id, week, now)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use chrono::{Duration, TimeZone};

    /// Mercoledì 11/11/2026, 18:00: lontano dalla finestra "nottambuli" e dal reset.
    fn now() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()
    }

    fn db() -> Database {
        let db = Database::open_in_memory().unwrap();
        db.seed_defaults().unwrap();
        db
    }

    fn count(db: &Database, sql: &str) -> i64 {
        db.with_conn(|c| c.query_row(sql, [], |row| row.get(0)).unwrap())
    }

    /// Registra e approva (come `_approved` nei test Python).
    fn approved(db: &Database, user: i64, name: &str) -> i64 {
        let result = register_suggestion(db, user, name, "Gioco", None, now());
        assert!(result.success, "{}", result.message);
        let id = result.created_id.expect("proposta nuova");
        db.execute("UPDATE suggestions SET review_status='approved' WHERE id=?", [id]).unwrap();
        id
    }

    fn community_poll(db: &Database) -> i64 {
        count(db, "SELECT id FROM polls WHERE is_community=1 AND active=1")
    }

    // --- scenario completo contro Python ----------------------------------------------------

    fn json_of(value: rusqlite::types::ValueRef<'_>) -> serde_json::Value {
        use rusqlite::types::ValueRef;
        match value {
            ValueRef::Null => serde_json::Value::Null,
            ValueRef::Integer(number) => number.into(),
            ValueRef::Real(number) => number.into(),
            ValueRef::Text(text) => String::from_utf8(text.to_vec()).unwrap().into(),
            ValueRef::Blob(_) => panic!("blob inattesi"),
        }
    }

    fn dump(db: &Database, sql: &str) -> serde_json::Value {
        db.with_conn(|connection| {
            let mut statement = connection.prepare(sql).unwrap();
            let columns = statement.column_count();
            let rows: Vec<serde_json::Value> = statement
                .query_map([], |row| {
                    Ok(serde_json::Value::Array((0..columns).map(|index| json_of(row.get_ref(index).unwrap())).collect()))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            serde_json::Value::Array(rows)
        })
    }

    fn table(db: &Database, name: &str, columns: &str, order: &str, filter: &str) -> serde_json::Value {
        dump(db, &format!("SELECT {columns} FROM {name} {filter} ORDER BY {order}"))
    }

    /// Esegue lo stesso scenario di `golden_scenario.py` sul vero codice Python (registrazioni,
    /// sostegni, ballottaggio, voti, unioni, vincitore, premi, record, nuova settimana) e confronta
    /// i risultati e il contenuto di tutte le tabelle coinvolte.
    #[test]
    fn full_scenario_matches_the_python_implementation() {
        use crate::stats::{
            contributor_leaderboard_rows, finalize_week_streaks, record_weekly_contributor,
            record_weekly_winner_stats, update_community_records, weekly_winner,
        };
        use serde_json::{Value, json};

        let golden: Value = serde_json::from_str(include_str!("golden/suggestions_scenario.json")).unwrap();
        let db = Database::open_in_memory().unwrap();
        db.seed_defaults().unwrap(); // `core.initialize` di Python crea anche i sondaggi di partenza
        let mut log = serde_json::Map::new();
        let approve = |id: i64| db.execute("UPDATE suggestions SET review_status='approved' WHERE id=?", [id]).unwrap();
        let reg = |log: &mut serde_json::Map<String, Value>, label: &str, user: i64, name: &str, tipo: &str, link: Option<&str>| {
            let result = register_suggestion(&db, user, name, tipo, link, now());
            log.insert(label.into(), json!([result.message, result.success, result.created_id]));
            result.created_id
        };

        // --- registrazioni
        let among = reg(&mut log, "reg_among", 1, "Among Us", "Gioco", Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")).unwrap();
        reg(&mut log, "reg_among_again", 2, "among  us!", "gioco", None);
        let jack = reg(&mut log, "reg_jack", 3, "Jackbox Party", "Gioco", None).unwrap();
        let karaoke = reg(&mut log, "reg_karaoke", 4, "Karaoke Night", "Karaoke", None).unwrap();
        let jpp = reg(&mut log, "reg_jpp", 5, "Jackbox Party Pack", "Gioco", Some("https://example.com/jpp")).unwrap();
        let vr = reg(&mut log, "reg_vr", 6, "Among Us VR", "Gioco", None).unwrap();
        reg(&mut log, "reg_bad_type", 1, "Serie TV", "Serie", None);
        reg(&mut log, "reg_bad_link", 7, "Link rotto", "Film", Some("https://exa mple.com"));
        log.insert("ids".into(), json!([among, jack, karaoke, jpp, vr]));
        for id in [among, jack, karaoke, jpp, vr] {
            approve(id);
        }

        // --- sostegni
        for (label, user, target) in [
            ("sup_10_among", 10, among),
            ("sup_11_among", 11, among),
            ("sup_12_jack", 12, jack),
            ("sup_13_vr", 13, vr),
            ("sup_10_among_dup", 10, among),
            ("sup_1_own", 1, among),
            ("sup_missing", 14, 999),
        ] {
            let result = register_suggestion_support(&db, user, target, now());
            log.insert(label.into(), json!([result.message, result.success, result.milestone]));
        }

        // --- ballottaggio
        for (label, target) in [("pick_among", among), ("pick_jack", jack), ("pick_among_again", among)] {
            let outcome = pick_suggestion_in_db(&db, target, 99, now()).unwrap();
            log.insert(label.into(), json!([outcome.error, outcome.poll_id, outcome.prior.is_some()]));
        }
        let option_of = |suggestion_id: i64| -> i64 {
            db.with_conn(|c| c.query_row("SELECT id FROM options WHERE suggestion_id=?", [suggestion_id], |r| r.get(0)).unwrap())
        };
        let (option_among, option_jack) = (option_of(among), option_of(jack));
        for user in [100, 101, 102] {
            db.execute("INSERT INTO votes(option_id,user_id) VALUES(?,?)", [option_among, user]).unwrap();
        }
        for user in [100, 103, 104] {
            db.execute("INSERT INTO votes(option_id,user_id) VALUES(?,?)", [option_jack, user]).unwrap();
        }

        // --- unioni
        for (label, keep, duplicate) in [("merge_among_vr", among, vr), ("merge_jack_jpp", jack, jpp), ("merge_same", among, among)] {
            let outcome = merge_suggestions(&db, keep, duplicate).unwrap();
            log.insert(label.into(), json!([outcome.ok, outcome.message]));
        }

        // --- vincitore, premi, record
        let week = db.current_week().unwrap();
        let winner = db.with_conn(|c| weekly_winner(c, week).unwrap()).unwrap();
        log.insert(
            "winner".into(),
            json!({
                "suggestion_id": winner.result.suggestion_id,
                "name": winner.result.name,
                "votes": winner.result.votes,
                "supporters": winner.result.supporters,
                "tie_break": winner.tie_break,
            }),
        );
        db.with_conn(|c| record_weekly_winner_stats(c, week, now(), None).unwrap());
        db.with_conn(|c| record_weekly_winner_stats(c, week, now(), None).unwrap());
        let (winners, points) = db.with_conn(|c| record_weekly_contributor(c, week, now()).unwrap());
        log.insert("contributor".into(), json!([winners, points]));
        db.with_conn(|c| update_community_records(c, week).unwrap());
        let leaderboard = db.with_conn(|c| contributor_leaderboard_rows(c, 10).unwrap());
        log.insert(
            "leaderboard".into(),
            Value::Array(
                leaderboard
                    .iter()
                    .map(|r| json!([r.user_id, r.points, r.level, r.winning_proposals, r.supports_received]))
                    .collect(),
            ),
        );
        let similar = similar_suggestions(&db, "Among Us 2", Some("Gioco"), 3, None).unwrap();
        log.insert(
            "similar".into(),
            Value::Array(similar.iter().map(|s| json!([s.id, s.name, (s.score * 1e12).round() / 1e12])).collect()),
        );

        // --- uscita dal ballottaggio e fine settimana
        let removed = unpick_suggestion(&db, jack, now()).unwrap().unwrap();
        log.insert("unpick_jack".into(), json!([removed.0.id, removed.1]));
        log.insert("unpick_jack_again".into(), json!(unpick_suggestion(&db, jack, now()).unwrap().map(|r| r.0.id)));
        db.with_conn(|c| finalize_week_streaks(c, week + 1).unwrap());

        // --- nuova settimana
        db.set_setting("week", week + 1).unwrap();
        reg(&mut log, "reg_week2_user1", 1, "Nuova settimana", "Altro", None);
        reg(&mut log, "reg_week2_user8", 8, "Altra idea", "Film", None);

        // --- confronto
        for (label, expected) in golden["log"].as_object().unwrap() {
            assert_eq!(log.get(label), Some(expected), "esito «{label}»");
        }
        assert_eq!(log.len(), golden["log"].as_object().unwrap().len(), "stesse voci di log");

        let tables = [
            ("suggestions", dump(&db, "SELECT id,week,norm_name,name,tipo,link,status,review_status,picked_by FROM suggestions ORDER BY id")),
            ("proposals", table(&db, "proposals", "id,suggestion_id,user_id,week,raw_name,raw_link", "id", "")),
            ("suggestion_supports", table(&db, "suggestion_supports", "suggestion_id,user_id,week", "suggestion_id,user_id", "")),
            ("options", table(&db, "options", "id,poll_id,label,suggestion_id", "id", "")),
            ("votes", table(&db, "votes", "option_id,user_id", "option_id,user_id", "")),
            (
                "user_stats",
                table(&db, "user_stats", "user_id,proposals_submitted,supports_given,supports_received,ballot_entries,winning_proposals", "user_id", ""),
            ),
            ("weekly_activity", table(&db, "weekly_activity", "user_id,week,activity_count,points", "user_id,week", "")),
            ("season_activity", table(&db, "season_activity", "season_key,user_id,week,activity_count,points", "user_id,week", "")),
            ("user_streaks", table(&db, "user_streaks", "user_id,current_streak,best_streak,last_active_week", "user_id", "")),
            ("weekly_streak_records", table(&db, "weekly_streak_records", "user_id,week,streak", "user_id,week", "")),
            ("community_records", table(&db, "community_records", "record_key,metric,label,week,user_id", "record_key", "")),
            ("weekly_awards", table(&db, "weekly_awards", "week,user_ids,points", "week", "")),
            ("user_winning_proposals", table(&db, "user_winning_proposals", "user_id,week,suggestion_id,name,votes", "user_id,week", "")),
            ("user_ballot_weeks", table(&db, "user_ballot_weeks", "user_id,week", "user_id,week", "")),
            (
                "user_hidden_badges",
                table(&db, "user_hidden_badges", "user_id,badge_key", "user_id,badge_key", "WHERE badge_key<>'night_owl'"),
            ),
            ("community_activity", dump(&db, "SELECT action, COUNT(*) FROM community_activity GROUP BY action ORDER BY action")),
        ];
        for (name, actual) in &tables {
            assert_eq!(actual, &golden["tables"][*name], "tabella «{name}»");
        }
        assert_eq!(tables.len(), golden["tables"].as_object().unwrap().len(), "stesse tabelle");
    }

    // --- somiglianza -----------------------------------------------------------------------

    /// (a, b, SequenceMatcher.ratio() di Python, `_similarity` di Python).
    #[test]
    fn similarity_matches_difflib_and_the_python_rules() {
        let cases: [(&str, &str, Option<f64>, f64); 32] = [
            ("jackboxparty", "jackboxpartypack", Some(0.8571428571428571), 0.85),
            ("jackboxparty", "jackboxparty", Some(1.0), 1.0),
            ("amongus", "amongusvr", Some(0.875), 0.85),
            ("karaokeanni80", "jackboxparty", Some(0.24), 0.24),
            ("skribbl", "skribblio", Some(0.875), 0.85),
            ("codenames", "codename", Some(0.9411764705882353), 0.8888888888888888),
            ("gartic", "garticphone", Some(0.7058823529411765), 0.85),
            ("abc", "abd", Some(0.6666666666666666), 0.6666666666666666),
            ("abcd", "abcde", Some(0.8888888888888888), 0.85),
            ("", "abc", None, 0.0),
            ("a", "a", Some(1.0), 1.0),
            ("ab", "ba", Some(0.5), 0.5),
            ("pokernight", "pokernights", Some(0.9523809523809523), 0.9090909090909091),
            ("chess", "chessgame", Some(0.7142857142857143), 0.85),
            ("monopoly", "monopolio", Some(0.8235294117647058), 0.8235294117647058),
            ("hungergames", "hungergame", Some(0.9523809523809523), 0.9090909090909091),
            ("jeopardy", "jeopardyquiz", Some(0.8), 0.85),
            ("kahoot", "kahootquiz", Some(0.75), 0.85),
            ("wikipedia", "wikipediaa", Some(0.9473684210526315), 0.9),
            ("the", "thee", Some(0.8571428571428571), 0.8571428571428571),
            ("lorem", "ipsum", Some(0.2), 0.2),
            ("abcdefgh", "hgfedcba", Some(0.125), 0.125),
            ("aaaa", "aaab", Some(0.75), 0.75),
            ("abab", "baba", Some(0.75), 0.75),
            ("xyzxyz", "xyz", Some(0.6666666666666666), 0.6666666666666666),
            ("giochidatavolo", "giocodatavolo", Some(0.8888888888888888), 0.8888888888888888),
            ("mariokart", "mariokart8deluxe", Some(0.72), 0.85),
            ("fifa23", "fifa24", Some(0.8333333333333334), 0.8333333333333334),
            ("fortnite", "fortnight", Some(0.8235294117647058), 0.8235294117647058),
            ("bombparty", "bombpartyvr", Some(0.9), 0.85),
            ("makeitmeme", "makeitmeme2", Some(0.9523809523809523), 0.9090909090909091),
            ("amq", "amqpro", Some(0.6666666666666666), 0.6666666666666666),
        ];
        for (a, b, ratio, expected) in cases {
            if let Some(ratio) = ratio {
                assert_eq!(sequence_ratio(a, b), ratio, "ratio({a:?}, {b:?})");
            }
            assert_eq!(similarity(a, b), expected, "similarity({a:?}, {b:?})");
        }
    }

    #[test]
    fn similar_suggestions_catch_near_duplicates_but_not_exact_ones() {
        // Stesso caso di tests/test_features.py.
        let db = db();
        approved(&db, 1, "Jackbox Party");
        let similar = similar_suggestions(&db, "Jackbox Party Pack", Some("Gioco"), 3, None).unwrap();
        assert_eq!(similar.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Jackbox Party"]);
        assert!(similar[0].score >= 0.85);
        assert!(similar_suggestions(&db, "jackbox party!", Some("Gioco"), 3, None).unwrap().is_empty());
        assert!(similar_suggestions(&db, "Karaoke anni 80", Some("Gioco"), 3, None).unwrap().is_empty());
        // Senza tipo, anche un nome identico (di un altro tipo) è escluso solo se uguale: qui sì.
        assert!(similar_suggestions(&db, "jackbox party!", None, 3, None).unwrap().is_empty());
        // Un tipo diverso non è un duplicato esatto: viene segnalato come somigliante (score 1.0).
        let other_type = similar_suggestions(&db, "jackbox party", Some("Film"), 3, None).unwrap();
        assert_eq!(other_type.len(), 1);
        assert_eq!(other_type[0].score, 1.0);
        assert!(similar_suggestions(&db, "!!!", None, 3, None).unwrap().is_empty(), "nome senza lettere");
    }

    #[test]
    fn similar_suggestions_skip_rejected_excluded_and_limit_results() {
        let db = db();
        let first = approved(&db, 1, "Jackbox Party Pack");
        let second = approved(&db, 2, "Jackbox Party Pack 2");
        let third = approved(&db, 3, "Jackbox Party Pack 3");
        db.execute("UPDATE suggestions SET review_status='rejected' WHERE id=?", [third]).unwrap();
        let found = similar_suggestions(&db, "Jackbox Party", None, 5, None).unwrap();
        assert_eq!(found.iter().map(|s| s.id).collect::<Vec<_>>(), [first, second], "la rifiutata non compare");
        let excluded = similar_suggestions(&db, "Jackbox Party", None, 5, Some(first)).unwrap();
        assert_eq!(excluded.iter().map(|s| s.id).collect::<Vec<_>>(), [second]);
        assert_eq!(similar_suggestions(&db, "Jackbox Party", None, 1, None).unwrap().len(), 1);
    }

    // --- link ------------------------------------------------------------------------------

    #[test]
    fn link_validation_matches_urlparse_rules() {
        let valid = [
            "https://example.com",
            "http://example.com/path?q=1#frag",
            "HTTPS://Example.COM/",
            "https://sub.domain.example.co.uk:8080/x",
            "https://localhost",
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://[2001:db8::1]:8443/",
            "https://example.com./",
            "https://a-b.com",
            "https://999.1.1.1/",
            "https://example.com:/",
            "https://store.steampowered.com/app/570/Dota_2/",
            "https://example.com/path with spaces",
        ];
        for link in valid {
            assert!(is_valid_link(link), "dovrebbe essere valido: {link}");
        }
        let invalid = [
            "",
            "example.com",
            "ftp://example.com",
            "javascript:alert(1)",
            "https://",
            "https:///path",
            "https://user@example.com",
            "https://user:pw@example.com",
            "https://:pw@example.com",
            "https://exa mple.com",
            "https://-bad.com",
            "https://bad-.com",
            "https://a..b.com",
            "https://exa_mple.com",
            "https://example.com:abc/",
            "https://example.com:99999/",
            "https://example.com:-1/",
            "https://[::1/",
            "https://::1]/",
            "https://[not-an-ip]/",
            "https://:80/",
            "//example.com",
            "http:example.com",
            "https://exàmple.com",
        ];
        for link in invalid {
            assert!(!is_valid_link(link), "dovrebbe essere non valido: {link}");
        }
    }

    /// Altri casi, con l'esito che dà `register_suggestion` di Python.
    #[test]
    fn link_validation_matches_python_on_edge_cases() {
        let long_label = |n: usize| format!("https://{}.com", "a".repeat(n));
        let cases: Vec<(String, bool)> = vec![
            ("https://example.com:0/".into(), true),
            ("https://example.com:65535/".into(), true),
            ("https://example.com:65536/".into(), false),
            ("https://[::ffff:1.2.3.4]/".into(), true),
            ("https://[1.2.3.4]/".into(), false),
            ("https://1.2.3.4:80".into(), true),
            ("https://256.1.1.1".into(), true),
            ("https://a.b.c.d.e.f".into(), true),
            ("http://x".into(), true),
            ("https://-".into(), false),
            ("https://a.com\t/x".into(), true),
            ("https://a.com@b.com".into(), false),
            ("https://[::1]:abc/".into(), false),
            ("https://[::1]x/".into(), true),
            ("https://exa%6dple.com/".into(), false),
            ("https://example.com?x=1".into(), true),
            ("https://example.com#x".into(), true),
            (long_label(63), true),
            (long_label(64), false),
            ("https://a.com:8o/".into(), false),
            ("https://a.com:\u{663}/".into(), false),
            ("HtTp://A.com".into(), true),
            ("https://\u{e9}.com".into(), false),
            ("http://[fe80::1%eth0]/".into(), true),
            ("https://xn--bcher-kva.example/".into(), true),
            ("https://exa mple.com/ok".into(), false),
            (" https://spazio-iniziale.com".into(), true),
        ];
        for (link, expected) in cases {
            assert_eq!(is_valid_link(&link), expected, "link: {link:?}");
        }
    }

    // --- registrazione ---------------------------------------------------------------------

    #[test]
    fn register_validates_name_type_and_link_with_python_messages() {
        let db = db();
        let register = |nome: &str, tipo: &str, link: Option<&str>| register_suggestion(&db, 1, nome, tipo, link, now());
        assert_eq!(register("x", "Gioco", None).message, "❌ Il nome deve contenere da 2 a 80 caratteri.");
        assert_eq!(register(&"x".repeat(81), "Gioco", None).message, "❌ Il nome deve contenere da 2 a 80 caratteri.");
        assert_eq!(register("Among Us", "Serie", None).message, "❌ Tipo non valido. Scegli tra: Gioco, Film, Karaoke, Altro.");
        assert_eq!(register("!!!", "Gioco", None).message, "❌ Il nome deve contenere lettere o numeri.");
        assert_eq!(register("Among Us", "Gioco", Some("non-un-link")).message, "❌ Il link non sembra valido (deve iniziare con http).");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM suggestions"), 0, "nulla viene scritto sugli errori");
        // Lo spazio ai bordi non conta e il tipo non distingue le maiuscole.
        let ok = register("  Among Us  ", " gioco ", Some("   "));
        assert!(ok.success, "{}", ok.message);
        // 80 caratteri esatti e nome con 2 caratteri sono ammessi.
        assert!(register_suggestion(&db, 2, "ab", "Film", None, now()).success);
        assert!(register_suggestion(&db, 3, &"y".repeat(80), "Altro", None, now()).success);
    }

    #[test]
    fn register_creates_a_pending_suggestion_and_updates_stats() {
        let db = db();
        let result = register_suggestion(&db, 7, "  Jackbox Party ", "film", Some("https://example.com/x"), now());
        assert!(result.success);
        assert_eq!(result.message, "✅ Proposta registrata: **Jackbox Party** (Film). Te ne restano `2` questa settimana.");
        let id = result.created_id.unwrap();
        let row: (String, String, String, Option<String>, String, i64) = db.with_conn(|c| {
            c.query_row(
                "SELECT norm_name, name, tipo, link, review_status, week FROM suggestions WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap()
        });
        assert_eq!(
            row,
            ("jackboxparty".into(), "Jackbox Party".into(), "Film".into(), Some("https://example.com/x".into()), "pending".into(), 1)
        );
        let recorded = count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM proposals WHERE suggestion_id=1 AND user_id=7 AND raw_name='Jackbox Party' AND ts={}",
                now().timestamp()
            ),
        );
        assert_eq!(recorded, 1);
        assert_eq!(count(&db, "SELECT proposals_submitted FROM user_stats WHERE user_id=7"), 1);
        assert_eq!(count(&db, "SELECT points FROM weekly_activity WHERE user_id=7 AND week=1"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM community_activity WHERE action='proposal'"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM user_hidden_badges"), 0, "alle 18 niente nottambulo");
    }

    #[test]
    fn same_proposal_from_another_user_joins_the_existing_one_and_fills_the_link() {
        let db = db();
        let first = register_suggestion(&db, 1, "Among Us", "Gioco", None, now());
        let second = register_suggestion(&db, 2, "among  us!", "gioco", Some("https://example.com/au"), now());
        assert!(second.success);
        assert_eq!(second.created_id, None, "si è unita a quella esistente");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM suggestions"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM proposals WHERE suggestion_id=1"), 2);
        let link: Option<String> = db.with_conn(|c| c.query_row("SELECT link FROM suggestions", [], |r| r.get(0)).unwrap());
        assert_eq!(link.as_deref(), Some("https://example.com/au"), "il link mancante si completa");
        // Un terzo link non sovrascrive quello presente.
        register_suggestion(&db, 3, "Among Us", "Gioco", Some("https://example.com/other"), now());
        let link: Option<String> = db.with_conn(|c| c.query_row("SELECT link FROM suggestions", [], |r| r.get(0)).unwrap());
        assert_eq!(link.as_deref(), Some("https://example.com/au"));
        assert!(first.created_id.is_some());
        // Stesso nome ma tipo diverso: è un'altra proposta.
        assert!(register_suggestion(&db, 4, "Among Us", "Film", None, now()).created_id.is_some());
    }

    #[test]
    fn rejected_suggestions_cannot_be_proposed_again() {
        let db = db();
        let id = register_suggestion(&db, 1, "Monopoly", "Gioco", None, now()).created_id.unwrap();
        db.execute("UPDATE suggestions SET review_status='rejected' WHERE id=?", [id]).unwrap();
        let again = register_suggestion(&db, 2, "Monopoly", "Gioco", None, now());
        assert_eq!((again.success, again.message.as_str()), (false, "❌ Questa proposta è stata rifiutata dallo staff."));
        assert_eq!(count(&db, "SELECT COUNT(*) FROM proposals"), 1);
    }

    #[test]
    fn weekly_limit_and_cooldown_apply_per_user() {
        let db = db();
        let at = |seconds: i64| now() + Duration::seconds(seconds);
        assert!(register_suggestion(&db, 1, "Uno", "Gioco", None, at(0)).success);
        let too_soon = register_suggestion(&db, 1, "Due", "Gioco", None, at(10));
        assert_eq!(too_soon.message, "⚠️ Aspetta qualche secondo prima di una nuova proposta.");
        assert!(register_suggestion(&db, 2, "Altro utente", "Gioco", None, at(10)).success, "il cooldown è per utente");
        assert!(register_suggestion(&db, 1, "Due", "Gioco", None, at(31)).success, "dopo 30 secondi");
        let third = register_suggestion(&db, 1, "Tre", "Gioco", None, at(90));
        assert!(third.success);
        assert!(third.message.ends_with("Te ne restano `0` questa settimana."), "{}", third.message);
        let blocked = register_suggestion(&db, 1, "Quattro", "Gioco", None, at(200));
        assert!(!blocked.success);
        // Prossimo reset: lunedì 16/11/2026 alle 12:00.
        let reset = TZ.with_ymd_and_hms(2026, 11, 16, 12, 0, 0).unwrap().timestamp();
        assert_eq!(blocked.message, format!("⚠️ Hai già fatto 3 proposte questa settimana. Puoi riprovare dopo <t:{reset}:R>."));
        db.set_setting("max_proposals", 4).unwrap();
        assert!(register_suggestion(&db, 1, "Quattro", "Gioco", None, at(200)).success, "limite configurabile");
        db.set_setting("max_proposals", 3).unwrap();
        db.set_setting("week", 2).unwrap();
        assert!(register_suggestion(&db, 1, "Nuova settimana", "Gioco", None, at(300)).success, "il limite è per settimana");
    }

    #[test]
    fn cooldown_can_be_turned_off() {
        let db = db();
        db.set_setting("cooldown_seconds", 0).unwrap();
        assert!(register_suggestion(&db, 1, "Uno", "Gioco", None, now()).success);
        assert!(register_suggestion(&db, 1, "Due", "Gioco", None, now()).success);
    }

    #[test]
    fn proposals_between_2_and_5_in_the_morning_unlock_night_owl() {
        let db = db();
        for (user, hour) in [(1, 1), (2, 2), (3, 4), (4, 5)] {
            let at = TZ.with_ymd_and_hms(2026, 11, 11, hour, 59, 0).unwrap();
            assert!(register_suggestion(&db, user, &format!("Notte {user}"), "Gioco", None, at).success);
        }
        let owls: Vec<i64> = db.with_conn(|c| {
            c.prepare("SELECT user_id FROM user_hidden_badges WHERE badge_key='night_owl' ORDER BY user_id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        });
        assert_eq!(owls, [2, 3], "dalle 02:00 alle 04:59");
    }

    #[test]
    fn custom_types_come_from_settings() {
        let db = db();
        db.set_setting("tipi", "Gioco, Quiz ,Serata").unwrap();
        assert!(register_suggestion(&db, 1, "Trivia", "QUIZ", None, now()).message.contains("(Quiz)"));
        let error = register_suggestion(&db, 2, "Trivia 2", "Film", None, now());
        assert_eq!(error.message, "❌ Tipo non valido. Scegli tra: Gioco, Quiz, Serata.");
        db.set_setting("tipi", "").unwrap(); // vuoto: valgono i predefiniti
        assert!(register_suggestion(&db, 2, "Trivia 2", "Film", None, now()).success);
    }

    #[test]
    fn database_failure_is_reported_as_a_generic_error() {
        let db = db();
        db.execute("DROP TABLE community_activity", []).unwrap();
        let result = register_suggestion(&db, 1, "Among Us", "Gioco", None, now());
        assert_eq!(
            (result.success, result.message.as_str()),
            (false, "❌ Non è stato possibile registrare la proposta. Riprova tra poco.")
        );
        assert_eq!(count(&db, "SELECT COUNT(*) FROM suggestions"), 0, "la transazione è annullata");
    }

    // --- sostegno --------------------------------------------------------------------------

    #[test]
    fn support_has_clear_refusals_and_counts_once_per_user() {
        let db = db();
        let pending = register_suggestion(&db, 1, "Among Us", "Gioco", None, now()).created_id.unwrap();
        let support = |user: i64, id: i64| register_suggestion_support(&db, user, id, now());
        assert_eq!(support(2, 999).message, "❌ Questa proposta non è più disponibile.");
        assert_eq!(support(2, pending).message, "⚠️ Questa proposta deve essere approvata dallo staff prima del supporto.");
        db.execute("UPDATE suggestions SET review_status='approved' WHERE id=?", [pending]).unwrap();
        assert_eq!(support(1, pending).message, "⚠️ Hai già proposto questa attività: il tuo sostegno è già conteggiato.");
        let first = support(2, pending);
        assert_eq!((first.success, first.message.as_str()), (true, "✅ Hai supportato **Among Us**. Grazie!"));
        assert_eq!(support(2, pending).message, "⚠️ Hai già supportato questa proposta.");
        // Statistiche: chi sostiene, chi riceve il sostegno.
        assert_eq!(count(&db, "SELECT supports_given FROM user_stats WHERE user_id=2"), 1);
        assert_eq!(count(&db, "SELECT supports_received FROM user_stats WHERE user_id=1"), 1);
        assert_eq!(count(&db, "SELECT points FROM weekly_activity WHERE user_id=1 AND week=1"), 1 + 2, "proposta + sostegno ricevuto");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM community_activity WHERE action='support'"), 1);
        // Un'altra settimana: la proposta non è più disponibile.
        db.set_setting("week", 2).unwrap();
        assert_eq!(support(3, pending).message, "❌ Questa proposta non è più disponibile.");
    }

    #[test]
    fn support_reports_milestones_at_10_20_and_25_supporters() {
        let db = db();
        let id = approved(&db, 1, "Among Us"); // 1 sostenitore (l'autore)
        let mut milestones = Vec::new();
        for user in 100..130 {
            let result = register_suggestion_support(&db, user, id, now());
            assert!(result.success, "{}", result.message);
            if let Some(milestone) = result.milestone {
                milestones.push(milestone);
            }
        }
        assert_eq!(milestones, [10, 20, 25, 30]);
    }

    #[test]
    fn support_failure_is_reported_generically() {
        let db = db();
        let id = approved(&db, 1, "Among Us");
        db.execute("DROP TABLE community_activity", []).unwrap();
        let result = register_suggestion_support(&db, 2, id, now());
        assert_eq!(
            (result.success, result.message.as_str()),
            (false, "❌ Non è stato possibile registrare il supporto. Riprova tra poco.")
        );
        assert_eq!(count(&db, "SELECT COUNT(*) FROM suggestion_supports"), 0);
    }

    // --- ballottaggio ----------------------------------------------------------------------

    #[test]
    fn pick_and_unpick_helpers() {
        // Stesso caso di tests/test_features.py::test_pick_and_unpick_helpers.
        let db = db();
        let id = approved(&db, 1, "Among Us");
        let picked = pick_suggestion_in_db(&db, id, 99, now()).unwrap();
        assert_eq!(picked.error, None);
        assert_eq!(picked.suggestion.as_ref().unwrap().status.as_deref(), Some("pending"), "com'era prima");
        let poll_id = picked.poll_id.unwrap();
        assert_eq!(poll_id, community_poll(&db));

        let again = pick_suggestion_in_db(&db, id, 99, now()).unwrap();
        assert!(again.error.unwrap().contains("già nel ballottaggio"));

        let (removed, removed_poll) = unpick_suggestion(&db, id, now()).unwrap().unwrap();
        assert_eq!(removed.id, id);
        assert_eq!(removed_poll, Some(poll_id));
        assert_eq!(unpick_suggestion(&db, id, now()).unwrap(), None);
    }

    #[test]
    fn pick_records_who_when_and_awards_ballot_points() {
        let db = db();
        let id = approved(&db, 1, "Among Us");
        let before = count(&db, "SELECT points FROM weekly_activity WHERE user_id=1 AND week=1");
        pick_suggestion_in_db(&db, id, 99, now()).unwrap();
        let row: (String, i64, String) = db.with_conn(|c| {
            c.query_row("SELECT status, picked_by, picked_at FROM suggestions WHERE id=?", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap()
        });
        assert_eq!(row, ("picked".into(), 99, "2026-11-11T18:00:00+01:00".into()));
        assert_eq!(count(&db, "SELECT COUNT(*) FROM options WHERE suggestion_id=1"), 1);
        assert_eq!(count(&db, "SELECT ballot_entries FROM user_stats WHERE user_id=1"), 1);
        assert_eq!(count(&db, "SELECT points FROM weekly_activity WHERE user_id=1 AND week=1"), before + 5);
        // Togliere dal ballottaggio annulla tutto.
        unpick_suggestion(&db, id, now()).unwrap();
        assert_eq!(count(&db, "SELECT points FROM weekly_activity WHERE user_id=1 AND week=1"), before);
        assert_eq!(count(&db, "SELECT ballot_entries FROM user_stats WHERE user_id=1"), 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM options WHERE suggestion_id=1"), 0);
        let status: Option<String> = db.with_conn(|c| c.query_row("SELECT picked_by FROM suggestions WHERE id=1", [], |r| r.get(0)).unwrap());
        assert_eq!(status, None);
    }

    #[test]
    fn pick_refuses_unavailable_unapproved_and_full_ballots() {
        let db = db();
        let pending = register_suggestion(&db, 1, "Bozza", "Gioco", None, now()).created_id.unwrap();
        let outcome = pick_suggestion_in_db(&db, pending, 99, now()).unwrap();
        assert_eq!(outcome.error.as_deref(), Some("⚠️ La proposta deve essere approvata prima del ballottaggio."));
        assert_eq!(outcome.poll_id, Some(community_poll(&db)));

        let missing = pick_suggestion_in_db(&db, 999, 99, now()).unwrap();
        assert_eq!(missing.error.as_deref(), Some("❌ Proposta o sondaggio community non disponibili."));
        assert_eq!((missing.suggestion, missing.poll_id), (None, None));

        db.set_setting("slots", 2).unwrap();
        let ids: Vec<i64> = (1..=3).map(|n| approved(&db, 10 + n, &format!("Gioco {n}"))).collect();
        assert_eq!(pick_suggestion_in_db(&db, ids[0], 99, now()).unwrap().error, None);
        assert_eq!(pick_suggestion_in_db(&db, ids[1], 99, now()).unwrap().error, None);
        let full = pick_suggestion_in_db(&db, ids[2], 99, now()).unwrap();
        assert_eq!(full.error.as_deref(), Some("⚠️ Gli slot (2) sono pieni: togli una proposta prima di continuare."));
        assert_eq!(count(&db, "SELECT COUNT(*) FROM options WHERE suggestion_id IS NOT NULL"), 2);

        db.execute("UPDATE polls SET active=0 WHERE is_community=1", []).unwrap();
        let no_poll = pick_suggestion_in_db(&db, ids[2], 99, now()).unwrap();
        assert_eq!(no_poll.error.as_deref(), Some("❌ Proposta o sondaggio community non disponibili."));
        assert!(no_poll.suggestion.is_some(), "la proposta c'è, manca il sondaggio");
    }

    #[test]
    fn pick_reports_a_previous_selection_of_the_same_proposal() {
        let db = db();
        db.execute(
            "INSERT INTO picked_suggestions(original_suggestion_id,week,norm_name,name,tipo,picked_at)
             VALUES(500,0,'amongus','Among Us','Gioco','2026-01-05T12:00:00+01:00')",
            [],
        )
        .unwrap();
        let id = approved(&db, 1, "Among Us");
        let outcome = pick_suggestion_in_db(&db, id, 99, now()).unwrap();
        assert_eq!(outcome.prior, Some(PriorPick { picked_at: Some("2026-01-05T12:00:00+01:00".into()) }));
    }

    // --- unione ----------------------------------------------------------------------------

    #[test]
    fn merge_keeps_supporters_of_the_duplicate() {
        // Stesso caso di tests/test_weekly_cycle.py::test_merge_keeps_supporters_of_the_duplicate.
        let db = db();
        let kept = approved(&db, 1, "Jackbox Party");
        let duplicate = approved(&db, 2, "Jackbox Party Pack");
        assert!(register_suggestion_support(&db, 10, duplicate, now()).success);
        assert!(register_suggestion_support(&db, 11, duplicate, now()).success);
        assert!(register_suggestion_support(&db, 11, kept, now()).success);
        let outcome = merge_suggestions(&db, kept, duplicate).unwrap();
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(outcome.message, "✅ Unite: «Jackbox Party Pack» → «Jackbox Party».");
        let supporters: Vec<i64> = db.with_conn(|c| {
            c.prepare("SELECT user_id FROM suggestion_supports WHERE suggestion_id=? ORDER BY user_id")
                .unwrap()
                .query_map([kept], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        });
        assert_eq!(supporters, [10, 11]);
        let week = db.current_week().unwrap();
        // Autori 1 e 2 + sostenitori 10 e 11.
        let counts = crate::queries::suggestion_supporter_counts(&db, week, &[kept]).unwrap();
        assert_eq!(counts[&kept], 4);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM suggestions"), 1, "la duplicata è sparita");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM proposals WHERE suggestion_id=1"), 2);
    }

    #[test]
    fn merge_moves_the_link_and_the_option_and_refuses_bad_pairs() {
        let db = db();
        let kept = approved(&db, 1, "Jackbox");
        let duplicate = register_suggestion(&db, 2, "Jackbox Pack", "Gioco", Some("https://example.com/j"), now()).created_id.unwrap();
        db.execute("UPDATE suggestions SET review_status='approved' WHERE id=?", [duplicate]).unwrap();
        let film = register_suggestion(&db, 3, "Jackbox Film", "Film", None, now()).created_id.unwrap();

        let same = merge_suggestions(&db, kept, kept).unwrap();
        assert_eq!((same.ok, same.message.as_str()), (false, "❌ Scegli due proposte diverse di questa settimana."));
        let missing = merge_suggestions(&db, kept, 999).unwrap();
        assert_eq!(missing.message, "❌ Scegli due proposte diverse di questa settimana.");
        assert!(missing.keep.is_some() && missing.duplicate.is_none());
        assert_eq!(merge_suggestions(&db, kept, film).unwrap().message, "❌ Non puoi unire proposte di tipo diverso.");

        pick_suggestion_in_db(&db, duplicate, 99, now()).unwrap();
        assert_eq!(merge_suggestions(&db, kept, duplicate).unwrap().message, "⚠️ Prima togli la duplicata dal ballottaggio.");
        unpick_suggestion(&db, duplicate, now()).unwrap();
        // Un'opzione collegata alla duplicata passa alla principale.
        let poll = community_poll(&db);
        db.execute("INSERT INTO options(poll_id,label,suggestion_id) VALUES(?,?,?)", params![poll, "Jackbox Pack", duplicate]).unwrap();

        assert!(merge_suggestions(&db, kept, duplicate).unwrap().ok);
        let link: Option<String> = db.with_conn(|c| c.query_row("SELECT link FROM suggestions WHERE id=?", [kept], |r| r.get(0)).unwrap());
        assert_eq!(link.as_deref(), Some("https://example.com/j"), "link mancante preso dalla duplicata");
        assert_eq!(count(&db, &format!("SELECT COUNT(*) FROM options WHERE suggestion_id={kept}")), 1);
    }

    #[test]
    fn release_poll_suggestions_puts_picked_proposals_back_to_pending() {
        let db = db();
        let picked = approved(&db, 1, "Among Us");
        let other = approved(&db, 2, "Jackbox");
        pick_suggestion_in_db(&db, picked, 99, now()).unwrap();
        let poll = community_poll(&db);
        let ballot_points = |db: &Database| count(db, "SELECT COALESCE((SELECT points FROM weekly_activity WHERE user_id=1 AND week=1),0)");
        assert_eq!(ballot_points(&db), 1 + 5);
        db.transaction(|tx| -> DbResult<()> { release_poll_suggestions(tx, poll, now()) }).unwrap();
        let status = |db: &Database, id: i64| -> String {
            db.with_conn(|c| c.query_row("SELECT status FROM suggestions WHERE id=?", [id], |r| r.get(0)).unwrap())
        };
        assert_eq!(status(&db, picked), "pending");
        assert_eq!(status(&db, other), "pending");
        assert_eq!(ballot_points(&db), 1, "i punti ballottaggio vengono tolti");
        // Richiamarla non toglie di nuovo punti (la proposta non è più "picked").
        db.transaction(|tx| -> DbResult<()> { release_poll_suggestions(tx, poll, now()) }).unwrap();
        assert_eq!(ballot_points(&db), 1);
    }

    // --- raccomandazione -------------------------------------------------------------------

    fn current(id: i64, norm: &str, created_ts: Option<i64>) -> CurrentSuggestion {
        CurrentSuggestion {
            id,
            week: 1,
            norm_name: norm.into(),
            name: norm.into(),
            tipo: "Gioco".into(),
            link: None,
            status: Some("pending".into()),
            created_ts,
            latest_ts: created_ts,
        }
    }

    #[test]
    fn recommendation_weighs_supporters_waiting_time_and_recent_wins() {
        let db = db();
        let day = 86_400;
        let now_ts = 100 * day;
        // Proposta 1: 2 sostenitori (autore + 1), in attesa da 3 giorni; 2: 1 sostenitore, 20 giorni (max 14);
        // 3: come la 1 ma ha vinto di recente; 4: nessuna data di creazione.
        db.execute_batch(
            "INSERT INTO suggestions(id,week,norm_name,name,tipo,review_status) VALUES
                (1,1,'uno','Uno','Gioco','approved'),(2,1,'due','Due','Gioco','approved'),
                (3,1,'tre','Tre','Gioco','approved'),(4,1,'quattro','Quattro','Gioco','approved');
             INSERT INTO proposals(suggestion_id,user_id,week,ts) VALUES (1,10,1,0),(2,20,1,0),(3,30,1,0),(4,40,1,0);
             INSERT INTO suggestion_supports VALUES (1,11,1,0),(3,31,1,0);
             INSERT INTO user_winning_proposals VALUES (30,0,3,'Tre!',5,'x');",
        )
        .unwrap();
        let suggestions = [
            current(1, "uno", Some(now_ts - 3 * day)),
            current(2, "due", Some(now_ts - 20 * day)),
            current(3, "tre", Some(now_ts - 3 * day)),
            current(4, "quattro", None),
        ];
        let (recommended, scores) = staff_recommendation(&db, &suggestions, now_ts).unwrap();
        assert_eq!(scores[&1], Recommendation { score: 23, supporters: 2, waiting_days: 3, won_recently: false });
        assert_eq!(scores[&2], Recommendation { score: 24, supporters: 1, waiting_days: 14, won_recently: false });
        assert_eq!(scores[&3], Recommendation { score: -2, supporters: 2, waiting_days: 3, won_recently: true });
        assert_eq!(scores[&4], Recommendation { score: 10, supporters: 1, waiting_days: 0, won_recently: false });
        assert_eq!(recommended, Some(2));
        assert_eq!(staff_recommendation(&db, &[], now_ts).unwrap(), (None, HashMap::new()));
    }

    #[test]
    fn recommendation_ties_go_to_more_supporters_then_waiting_then_lowest_id() {
        let db = db();
        db.execute_batch(
            "INSERT INTO suggestions(id,week,norm_name,name,tipo,review_status) VALUES
                (1,1,'a','A','Gioco','approved'),(2,1,'b','B','Gioco','approved');
             INSERT INTO proposals(suggestion_id,user_id,week,ts) VALUES (1,10,1,0),(2,20,1,0);",
        )
        .unwrap();
        let suggestions = [current(1, "a", None), current(2, "b", None)];
        let (recommended, _) = staff_recommendation(&db, &suggestions, 0).unwrap();
        assert_eq!(recommended, Some(1), "parità totale: l'ID più basso");
    }

    // --- notifiche -------------------------------------------------------------------------

    #[test]
    fn dm_preferences_are_stored_and_updated() {
        let db = db();
        assert!(!has_notification_preference(&db, 5).unwrap());
        set_dm_notifications(&db, 5, true).unwrap();
        assert!(has_notification_preference(&db, 5).unwrap());
        assert_eq!(count(&db, "SELECT dm_enabled FROM user_notification_preferences WHERE user_id=5"), 1);
        set_dm_notifications(&db, 5, false).unwrap();
        assert_eq!(count(&db, "SELECT dm_enabled FROM user_notification_preferences WHERE user_id=5"), 0);
        assert!(has_notification_preference(&db, 5).unwrap(), "disattivate ma scelte");
    }
}
