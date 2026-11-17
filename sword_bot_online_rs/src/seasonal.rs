//! Stagioni della community: attività, classifiche e campioni.
#![allow(dead_code)]

use chrono::{Datelike, NaiveDate};
use rusqlite::{Connection, params};

use crate::database::DbResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Season {
    pub key: String,
    pub label: String,
    /// Data ISO (inclusa).
    pub starts_at: String,
    /// Data ISO (esclusa).
    pub ends_at: String,
}

fn date(year: i32, month: u32) -> String {
    NaiveDate::from_ymd_opt(year, month, 1)
        .expect("primo del mese sempre valido")
        .to_string()
}

pub fn season_for_date(day: NaiveDate) -> Season {
    let (month, year) = (day.month(), day.year());
    if month == 12 || month <= 2 {
        // L'inverno attraversa il capodanno: gennaio e febbraio appartengono a quello iniziato prima.
        let start_year = if month == 12 { year } else { year - 1 };
        return Season {
            key: format!("inverno-{start_year}"),
            label: format!("Inverno {start_year}/{:02}", (start_year + 1) % 100),
            starts_at: date(start_year, 12),
            ends_at: date(start_year + 1, 3),
        };
    }
    let (name, key, starts_month, ends) = match month {
        3..=5 => ("Primavera", "primavera", 3, (year, 6)),
        6..=8 => ("Estate", "estate", 6, (year, 9)),
        _ => ("Autunno", "autunno", 9, (year, 12)),
    };
    Season {
        key: format!("{key}-{year}"),
        label: format!("{name} {year}"),
        starts_at: date(year, starts_month),
        ends_at: date(ends.0, ends.1),
    }
}

/// Registra la stagione corrente se non esiste ancora e la restituisce.
pub fn ensure_season(connection: &Connection, day: NaiveDate) -> DbResult<Season> {
    let season = season_for_date(day);
    connection.execute(
        "INSERT OR IGNORE INTO seasons(season_key, label, starts_at, ends_at) VALUES(?,?,?,?)",
        params![season.key, season.label, season.starts_at, season.ends_at],
    )?;
    Ok(season)
}

/// Registra un'attività dell'utente nella stagione che contiene `day`: un'attività in più e
/// `points` punti in più nella riga (stagione, utente, settimana).
pub fn record_season_activity(
    connection: &Connection,
    user_id: i64,
    week: i64,
    points: i64,
    day: NaiveDate,
) -> DbResult<()> {
    let season = ensure_season(connection, day)?;
    connection.execute(
        "INSERT INTO season_activity
         (season_key, user_id, week, activity_count, points)
         VALUES(?,?,?,1,?)
         ON CONFLICT(season_key, user_id, week) DO UPDATE SET
             activity_count=season_activity.activity_count+1,
             points=season_activity.points+excluded.points",
        params![season.key, user_id, week, points],
    )?;
    Ok(())
}

/// Riga della classifica di una stagione.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeasonLeaderRow {
    pub user_id: i64,
    pub activities: i64,
    pub points: i64,
}

/// Classifica di una stagione (solo chi ha punti): per punti, poi attività, poi ID.
pub fn season_leaderboard_rows(connection: &Connection, season_key: &str, limit: i64) -> DbResult<Vec<SeasonLeaderRow>> {
    connection
        .prepare(
            "SELECT user_id, SUM(activity_count) AS activities,
                    SUM(points) AS points
             FROM season_activity
             WHERE season_key=?
             GROUP BY user_id
             HAVING SUM(points)>0
             ORDER BY points DESC, activities DESC, user_id
             LIMIT ?",
        )?
        .query_map(params![season_key, limit], |row| {
            Ok(SeasonLeaderRow { user_id: row.get(0)?, activities: row.get(1)?, points: row.get(2)? })
        })?
        .collect()
}

/// Punti di un utente in una stagione.
pub fn season_points_for_user(connection: &Connection, season_key: &str, user_id: i64) -> DbResult<i64> {
    connection.query_row(
        "SELECT COALESCE(SUM(points), 0) FROM season_activity WHERE season_key=? AND user_id=?",
        params![season_key, user_id],
        |row| row.get(0),
    )
}

/// Il campione di una stagione conclusa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeasonChampion {
    pub season_key: String,
    pub label: String,
    pub user_id: i64,
    pub points: i64,
}

/// Campioni delle stagioni concluse (`ends_at <= today`), dalla più recente. In caso di parità in
/// vetta ci sono più campioni per la stessa stagione. Dopo `limit` campioni si smette di iniziare
/// nuove stagioni (le parità della stagione in corso si completano).
pub fn completed_season_champions(connection: &Connection, today: NaiveDate, limit: usize) -> DbResult<Vec<SeasonChampion>> {
    let rows: Vec<(String, String, i64, i64)> = connection
        .prepare(
            "SELECT s.season_key, s.label, a.user_id, SUM(a.points) AS points
             FROM seasons s
             JOIN season_activity a ON a.season_key=s.season_key
             WHERE s.ends_at<=?
             GROUP BY s.season_key, a.user_id
             HAVING SUM(a.points)>0
             ORDER BY s.starts_at DESC, points DESC, a.user_id",
        )?
        .query_map([today.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
        .collect::<DbResult<_>>()?;
    let mut champions: Vec<SeasonChampion> = Vec::new();
    let mut current_key: Option<String> = None;
    let mut top_points = 0;
    for (season_key, label, user_id, points) in rows {
        if current_key.as_deref() != Some(season_key.as_str()) {
            if champions.len() >= limit {
                break;
            }
            current_key = Some(season_key.clone());
            top_points = points;
        }
        if points != top_points {
            continue;
        }
        champions.push(SeasonChampion { season_key, label, user_id, points });
    }
    Ok(champions)
}

/// Etichette delle stagioni vinte da `user_id`, dalla più recente (al massimo `limit`).
pub fn season_champion_titles(connection: &Connection, user_id: i64, today: NaiveDate, limit: usize) -> DbResult<Vec<String>> {
    Ok(completed_season_champions(connection, today, 50)?
        .into_iter()
        .filter(|champion| champion.user_id == user_id)
        .map(|champion| champion.label)
        .take(limit)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn winter_spans_the_new_year() {
        let december = season_for_date(day(2026, 12, 15));
        assert_eq!(december.key, "inverno-2026");
        assert_eq!(december.label, "Inverno 2026/27");
        assert_eq!((december.starts_at.as_str(), december.ends_at.as_str()), ("2026-12-01", "2027-03-01"));
        // Gennaio e febbraio appartengono all'inverno iniziato l'anno prima.
        assert_eq!(season_for_date(day(2027, 1, 10)), december);
        assert_eq!(season_for_date(day(2027, 2, 28)), december);
    }

    #[test]
    fn other_seasons_follow_calendar_quarters() {
        let spring = season_for_date(day(2026, 3, 1));
        assert_eq!((spring.key.as_str(), spring.label.as_str()), ("primavera-2026", "Primavera 2026"));
        assert_eq!((spring.starts_at.as_str(), spring.ends_at.as_str()), ("2026-03-01", "2026-06-01"));
        assert_eq!(season_for_date(day(2026, 5, 31)), spring);

        let summer = season_for_date(day(2026, 6, 1));
        assert_eq!((summer.key.as_str(), summer.label.as_str()), ("estate-2026", "Estate 2026"));
        assert_eq!(summer.ends_at, "2026-09-01");
        assert_eq!(season_for_date(day(2026, 8, 31)), summer);

        let autumn = season_for_date(day(2026, 11, 11));
        assert_eq!((autumn.key.as_str(), autumn.label.as_str()), ("autunno-2026", "Autunno 2026"));
        assert_eq!((autumn.starts_at.as_str(), autumn.ends_at.as_str()), ("2026-09-01", "2026-12-01"));
    }

    #[test]
    fn century_boundary_label_uses_two_digits() {
        assert_eq!(season_for_date(day(2099, 12, 1)).label, "Inverno 2099/00");
    }

    #[test]
    fn season_activity_accumulates_per_user_and_week() {
        let db = crate::database::Database::open_in_memory().unwrap();
        db.with_conn(|c| {
            record_season_activity(c, 5, 3, 2, day(2026, 11, 11)).unwrap();
            record_season_activity(c, 5, 3, 5, day(2026, 11, 12)).unwrap();
            record_season_activity(c, 5, 4, 1, day(2026, 11, 20)).unwrap();
            record_season_activity(c, 6, 3, 0, day(2026, 11, 11)).unwrap();
        });
        let rows: Vec<(String, i64, i64, i64, i64)> = db.with_conn(|c| {
            c.prepare("SELECT season_key,user_id,week,activity_count,points FROM season_activity ORDER BY user_id, week")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        });
        let key = "autunno-2026".to_string();
        assert_eq!(rows, [(key.clone(), 5, 3, 2, 7), (key.clone(), 5, 4, 1, 1), (key, 6, 3, 1, 0)]);
    }

    #[test]
    fn leaderboard_points_and_champions_follow_the_python_rules() {
        let db = crate::database::Database::open_in_memory().unwrap();
        db.execute_batch(
            "INSERT INTO seasons VALUES
                ('inverno-2024','Inverno 2024/25','2024-12-01','2025-03-01'),
                ('primavera-2025','Primavera 2025','2025-03-01','2025-06-01'),
                ('estate-2025','Estate 2025','2025-06-01','2025-09-01'),
                ('autunno-2026','Autunno 2026','2026-09-01','2026-12-01');
             INSERT INTO season_activity(season_key,user_id,week,activity_count,points) VALUES
                ('inverno-2024',1,1,2,10),('inverno-2024',2,1,1,10),('inverno-2024',3,1,1,4),
                ('primavera-2025',2,5,3,20),('primavera-2025',1,5,1,5),
                ('estate-2025',3,9,1,0),
                ('autunno-2026',1,40,4,7),('autunno-2026',2,40,1,7),('autunno-2026',2,41,2,3);",
        )
        .unwrap();
        db.with_conn(|c| {
            // Classifica: a pari punti prima chi ha più attività, poi l'ID; 0 punti esclusi.
            let rows = season_leaderboard_rows(c, "autunno-2026", 10).unwrap();
            assert_eq!(rows.iter().map(|r| (r.user_id, r.activities, r.points)).collect::<Vec<_>>(), [(2, 3, 10), (1, 4, 7)]);
            assert_eq!(season_leaderboard_rows(c, "estate-2025", 10).unwrap(), []);
            assert_eq!(season_leaderboard_rows(c, "autunno-2026", 1).unwrap().len(), 1);
            assert_eq!(season_points_for_user(c, "autunno-2026", 2).unwrap(), 10);
            assert_eq!(season_points_for_user(c, "autunno-2026", 99).unwrap(), 0);

            let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
            let champions = completed_season_champions(c, today, 8).unwrap();
            // Primavera: l'utente 2; inverno: parità 1 e 2 a 10 punti; estate: nessuno (0 punti); autunno non concluso.
            assert_eq!(
                champions.iter().map(|c| (c.season_key.as_str(), c.user_id, c.points)).collect::<Vec<_>>(),
                [("primavera-2025", 2, 20), ("inverno-2024", 1, 10), ("inverno-2024", 2, 10)]
            );
            // Con limit=1 la prima stagione è completa e la successiva non inizia.
            assert_eq!(completed_season_champions(c, today, 1).unwrap().len(), 1);
            assert_eq!(season_champion_titles(c, 2, today, 3).unwrap(), ["Primavera 2025", "Inverno 2024/25"]);
            assert_eq!(season_champion_titles(c, 2, today, 1).unwrap(), ["Primavera 2025"]);
            assert_eq!(season_champion_titles(c, 1, today, 3).unwrap(), ["Inverno 2024/25"]);
            assert!(season_champion_titles(c, 3, today, 3).unwrap().is_empty());
            // L'autunno 2026 diventa "concluso" a dicembre.
            let december = NaiveDate::from_ymd_opt(2026, 12, 1).unwrap();
            assert_eq!(season_champion_titles(c, 2, december, 3).unwrap()[0], "Autunno 2026");
        });
    }

    #[test]
    fn ensure_season_inserts_once() {
        let db = crate::database::Database::open_in_memory().unwrap();
        for _ in 0..2 {
            db.with_conn(|connection| ensure_season(connection, day(2026, 11, 11))).unwrap();
        }
        let count: i64 = db.with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM seasons WHERE season_key='autunno-2026'", [], |r| r.get(0)).unwrap()
        });
        assert_eq!(count, 1);
    }
}
