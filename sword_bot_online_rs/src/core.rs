//! Avvio del database (parte di dati di `core.initialize` in `sfo/core.py`).
//!
//! In Python `initialize` azzera anche lo stato in memoria di sondaggi, log staff e dashboard
//! (lock e refresh pendenti): in Rust nascono vuoti con i rispettivi moduli, quindi qui non serve.
// `initialize` è usato da `main`; il resto servirà ai moduli successivi.
#![allow(dead_code)]

use chrono::DateTime;
use chrono_tz::Tz;

use crate::database::{Database, DbResult};
use crate::{schedule, seasonal};

/// Applica manutenzione, allinea le scadenze dei sondaggi e crea i dati di partenza.
///
/// `now` è l'istante di avvio in ora italiana (`schedule::now()`); si passa dall'esterno per
/// poter testare qualsiasi data. Il database deve essere già aperto (`Database::open`).
pub fn initialize(db: &Database, now: DateTime<Tz>) -> DbResult<()> {
    db.startup_maintenance()?;

    // Calcolato prima della transazione: `Database` non è rientrante (il lock è già preso dentro).
    let next_close = schedule::poll_closing_time(db, now)?;
    let today = now.date_naive();
    let today_text = today.to_string();
    db.transaction(|tx| -> DbResult<()> {
        // I sondaggi attivi chiudono al prossimo reset: allinea anche le scadenze
        // create con la vecchia regola "pubblicazione + 7 giorni".
        tx.execute(
            "UPDATE polls SET closes_at=? WHERE active=1 AND (closes_at IS NULL OR closes_at<?)",
            [next_close, next_close],
        )?;
        for key in ["stats_since", "awards_since"] {
            tx.execute(
                "INSERT OR IGNORE INTO settings(key,value) VALUES(?,?)",
                [key, today_text.as_str()],
            )?;
        }
        seasonal::ensure_season(tx, today)?;
        Ok(())
    })?;
    db.seed_defaults()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use chrono::TimeZone;

    fn now() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap() // mercoledì
    }

    fn next_reset() -> f64 {
        TZ.with_ymd_and_hms(2026, 11, 16, 12, 0, 0).unwrap().timestamp() as f64
    }

    fn closes_at(db: &Database, name: &str) -> Option<f64> {
        db.with_conn(|c| {
            c.query_row("SELECT closes_at FROM polls WHERE name=?", [name], |row| row.get(0)).unwrap()
        })
    }

    #[test]
    fn fresh_database_gets_defaults_dates_and_season() {
        let db = Database::open_in_memory().unwrap();
        initialize(&db, now()).unwrap();
        assert_eq!(db.get_setting("stats_since").unwrap().as_deref(), Some("2026-11-11"));
        assert_eq!(db.get_setting("awards_since").unwrap().as_deref(), Some("2026-11-11"));
        let seasons: Vec<String> = db.with_conn(|c| {
            c.prepare("SELECT season_key FROM seasons").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        });
        assert_eq!(seasons, ["autunno-2026"]);
        let polls: i64 = db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM polls", [], |r| r.get(0)).unwrap());
        assert_eq!(polls, 5);
    }

    #[test]
    fn active_polls_are_aligned_to_the_next_reset() {
        let db = Database::open_in_memory().unwrap();
        let far_future = next_reset() + 100_000.0;
        db.execute("INSERT INTO polls(name,active,closes_at) VALUES('senza scadenza',1,NULL)", []).unwrap();
        db.execute("INSERT INTO polls(name,active,closes_at) VALUES('scadenza vecchia',1,1000)", []).unwrap();
        db.execute("INSERT INTO polls(name,active,closes_at) VALUES('scadenza lontana',1,?)", [far_future]).unwrap();
        db.execute("INSERT INTO polls(name,active,closes_at) VALUES('chiuso dallo staff',0,NULL)", []).unwrap();

        initialize(&db, now()).unwrap();

        assert_eq!(closes_at(&db, "senza scadenza"), Some(next_reset()));
        assert_eq!(closes_at(&db, "scadenza vecchia"), Some(next_reset()));
        assert_eq!(closes_at(&db, "scadenza lontana"), Some(far_future), "non si accorcia");
        assert_eq!(closes_at(&db, "chiuso dallo staff"), None, "i sondaggi chiusi restano chiusi");
    }

    #[test]
    fn initialize_is_idempotent_and_keeps_existing_since_dates() {
        let db = Database::open_in_memory().unwrap();
        db.set_setting("stats_since", "2025-01-01").unwrap();
        initialize(&db, now()).unwrap();
        initialize(&db, now()).unwrap();
        assert_eq!(db.get_setting("stats_since").unwrap().as_deref(), Some("2025-01-01"));
        assert_eq!(db.get_setting("awards_since").unwrap().as_deref(), Some("2026-11-11"));
        let polls: i64 = db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM polls", [], |r| r.get(0)).unwrap());
        assert_eq!(polls, 5, "il seed non si ripete");
    }
}
