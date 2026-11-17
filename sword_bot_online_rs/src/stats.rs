//! Statistiche utenti: punti, livelli, streak, badge segreti e record (equivalente di `sfo/stats.py`).
//!
//! Differenze di forma rispetto a Python, tutte per poter funzionare dentro `Database::transaction`:
//! * le funzioni prendono `&Connection` (dentro una transazione si passa la `Transaction`, che
//!   si comporta da `Connection`);
//! * l'orologio (`datetime.now(TZ)`) è un parametro `now`;
//! * le statistiche e i badge sono enum invece di stringhe: un nome non valido, che in Python
//!   sollevava `ValueError`, qui non compila.
// Verrà usato da proposte, voto, reset e profili.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenBadge {
    NightOwl,
    Indecisive,
    Influencer,
}

impl HiddenBadge {
    pub const fn key(self) -> &'static str {
        match self {
            HiddenBadge::NightOwl => "night_owl",
            HiddenBadge::Indecisive => "indecisive",
            HiddenBadge::Influencer => "influencer",
        }
    }

    /// Nome con emoji mostrato nel profilo (da `config::HIDDEN_BADGES`).
    pub fn name(self) -> &'static str {
        badge_name(self.key()).expect("ogni badge è in config::HIDDEN_BADGES")
    }
}

fn badge_name(key: &str) -> Option<&'static str> {
    HIDDEN_BADGES.iter().find(|(badge_key, _)| *badge_key == key).map(|(_, name)| *name)
}

/// Sblocca un badge segreto. `true` se è nuovo, `false` se l'utente lo aveva già.
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

/// L'utente ha avuto una proposta nel ballottaggio per tre settimane consecutive fino a `week`.
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

/// Nomi dei badge sbloccati, dal più vecchio (le chiavi sconosciute si saltano).
pub fn user_hidden_badge_names(connection: &Connection, user_id: i64) -> DbResult<Vec<&'static str>> {
    let keys: Vec<String> = connection
        .prepare("SELECT badge_key FROM user_hidden_badges WHERE user_id=? ORDER BY unlocked_at, badge_key")?
        .query_map([user_id], |row| row.get(0))?
        .collect::<DbResult<_>>()?;
    Ok(keys.iter().filter_map(|key| badge_name(key)).collect())
}

// --- contatori e attività --------------------------------------------------------------------

/// Colonne di `user_stats` che si possono incrementare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserStat {
    ProposalsSubmitted,
    SupportsGiven,
    SupportsReceived,
    BallotEntries,
    WinningProposals,
}

impl UserStat {
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

/// Le statistiche che premiano gli autori di una proposta (`config::AUTHOR_STAT_POINTS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorStat {
    SupportsReceived,
    BallotEntries,
    WinningProposals,
}

impl AuthorStat {
    const fn user_stat(self) -> UserStat {
        match self {
            AuthorStat::SupportsReceived => UserStat::SupportsReceived,
            AuthorStat::BallotEntries => UserStat::BallotEntries,
            AuthorStat::WinningProposals => UserStat::WinningProposals,
        }
    }

    /// Punti settimanali dati agli autori per questa statistica.
    pub fn points(self) -> i64 {
        let column = self.user_stat().column();
        AUTHOR_STAT_POINTS
            .iter()
            .find(|(name, _)| *name == column)
            .map(|(_, points)| *points)
            .expect("ogni statistica degli autori è in config::AUTHOR_STAT_POINTS")
    }
}

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

/// Registra un'attività dell'utente nella settimana: contatore, punti, stagione e streak.
///
/// Lo streak conta le settimane consecutive con almeno un'attività; `weekly_streak_records`
/// ricorda ogni volta che l'utente batte il proprio record personale.
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

/// Annulla i punti "ballottaggio" quando una proposta esce dal ballottaggio.
///
/// Senza questo, scegliere e togliere più volte la stessa proposta accumulerebbe +5 punti a ogni
/// giro per i suoi autori.
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

/// Assegna una statistica (e i suoi punti) a tutti gli autori di una proposta.
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

/// I contatori di `user_stats` di un utente.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserStats {
    pub proposals_submitted: i64,
    pub supports_given: i64,
    pub supports_received: i64,
    pub ballot_entries: i64,
    pub winning_proposals: i64,
}

pub fn contributor_points(stats: &UserStats) -> i64 {
    stats.proposals_submitted + 2 * stats.supports_received + 5 * stats.ballot_entries + 10 * stats.winning_proposals
}

/// Livello più alto raggiunto con `points` punti.
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

/// `(livello attuale, prossimo livello, minimo del livello attuale, minimo del prossimo)`.
/// Al livello massimo il prossimo è `"livello massimo"` con i due minimi uguali.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributorRow {
    pub user_id: i64,
    pub points: i64,
    pub level: &'static str,
    pub winning_proposals: i64,
    pub supports_received: i64,
}

/// Classifica dei contributori (solo chi ha punti): per punti, poi vittorie, poi sostegni
/// ricevuti, poi ID.
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

fn json_conversion_error(error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

/// Contributori della settimana: chi ha più punti (anche a pari merito). Il premio si calcola una
/// sola volta: le chiamate successive restituiscono quello già registrato.
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

/// Aggiorna un record della community solo se `metric` è positivo e supera quello esistente.
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

/// Ricalcola i record della community: proposta più sostenuta, più votata, streak più lungo,
/// più vittorie.
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

/// Azzera lo streak di chi non è stato attivo nella settimana che si chiude.
pub fn finalize_week_streaks(connection: &Connection, week: i64) -> DbResult<()> {
    connection.execute(
        "UPDATE user_streaks SET current_streak=0 WHERE current_streak>0 AND last_active_week<?",
        [week],
    )?;
    Ok(())
}

// --- vincitore della settimana ---------------------------------------------------------------

/// Una proposta nel ballottaggio con i suoi voti e i dati di spareggio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BallotResult {
    pub suggestion_id: i64,
    pub name: String,
    pub votes: i64,
    pub supporters: i64,
    /// Primo momento in cui la proposta è stata inviata (0 se sconosciuto).
    pub first_ts: i64,
}

/// Opzioni del ballottaggio community in ordine di classifica.
///
/// Spareggio esplicito: a parità di voti vince la proposta con più sostenitori; se resta la
/// parità, quella inviata per prima.
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

/// Il vincitore della settimana e l'eventuale criterio di spareggio usato.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Winner {
    pub result: BallotResult,
    pub tie_break: Option<&'static str>,
}

/// Vincitore unico della settimana; `None` se non ci sono opzioni o nessuno ha votato.
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

/// Registra la vittoria per gli autori della proposta vincente: una sola volta per autore e
/// settimana (`INSERT OR IGNORE`), quindi richiamarla non raddoppia punti e statistiche.
///
/// `awarded_at` è il momento a cui attribuire l'attività nella stagione (default: `now`).
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
