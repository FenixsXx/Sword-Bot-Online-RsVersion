//! Calendario del reset settimanale e scadenza dei sondaggi (equivalente di `sfo/schedule.py`).
//!
//! Come in Python, i calcoli sono sull'orologio da muro (`timedelta(days=7)` mantiene l'ora
//! locale anche a cavallo del cambio dell'ora legale). Un'ora locale che non esiste (salto in
//! avanti) usa l'offset precedente al cambio e una ambigua (ora che si ripete) la prima
//! occorrenza: sono le stesse regole di `zoneinfo`.
// Verrà usato da reset, promemoria e sondaggi.
#![allow(dead_code)]

use chrono::{DateTime, Datelike, Duration, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;

use crate::config::TZ;
use crate::database::{self, Connected, DbResult};
use crate::settings;

/// Adesso, in ora italiana.
pub fn now() -> DateTime<Tz> {
    Utc::now().with_timezone(&TZ)
}

/// Come `datetime.isoformat()` di Python: `2026-11-09T12:00:00+01:00`, con i microsecondi
/// (`.123456`) solo se non sono zero. È il formato salvato nel database (`last_reset`, audit…).
pub fn isoformat(value: DateTime<Tz>) -> String {
    let micros = value.timestamp_subsec_micros();
    let base = value.format("%Y-%m-%dT%H:%M:%S");
    let offset = value.format("%:z");
    if micros == 0 { format!("{base}{offset}") } else { format!("{base}.{micros:06}{offset}") }
}

/// Orologio da muro -> istante, con le regole di `zoneinfo` (fold=0) per ore inesistenti/ambigue.
pub fn from_wall_clock(naive: NaiveDateTime) -> DateTime<Tz> {
    if let Some(resolved) = TZ.from_local_datetime(&naive).earliest() {
        return resolved;
    }
    // Ora inesistente: vale l'offset in vigore prima del salto.
    let offset_before = TZ
        .from_local_datetime(&(naive - Duration::hours(3)))
        .earliest()
        .map_or(0, |before| before.offset().fix().local_minus_utc());
    TZ.from_utc_datetime(&(naive - Duration::seconds(i64::from(offset_before))))
}

/// `value + delta` come lo fa Python con `timedelta` su un orario con fuso: sull'orologio da muro,
/// quindi 3 giorni e mezzo prima restano le stesse ore locali anche a cavallo del cambio dell'ora legale.
pub fn shift_wall_clock(value: DateTime<Tz>, delta: Duration) -> DateTime<Tz> {
    from_wall_clock(value.naive_local() + delta)
}

/// Giorno e ora del reset settimanale (configurabili da /config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetSchedule {
    /// 0 = lunedì … 6 = domenica.
    weekday: u32,
    hour: u32,
}

impl ResetSchedule {
    pub fn new(weekday: u32, hour: u32) -> Option<Self> {
        (weekday <= 6 && hour <= 23).then_some(Self { weekday, hour })
    }

    /// Giorno e ora attualmente configurati (con i predefiniti di `config`).
    pub fn from_settings(db: &impl Connected) -> DbResult<Self> {
        let weekday = settings::reset_weekday(db)? as u32;
        let hour = settings::reset_hour(db)? as u32;
        Ok(Self::new(weekday, hour).expect("le impostazioni sono già limitate a 0..=6 e 0..=23"))
    }

    fn last_wall_clock(self, now: NaiveDateTime) -> NaiveDateTime {
        let days_back = (i64::from(now.weekday().num_days_from_monday()) - i64::from(self.weekday)).rem_euclid(7);
        let candidate = (now.date() - Duration::days(days_back))
            .and_hms_opt(self.hour, 0, 0)
            .expect("ora validata in `new`");
        if candidate > now { candidate - Duration::days(7) } else { candidate }
    }

    fn next_wall_clock(self, now: NaiveDateTime) -> NaiveDateTime {
        let target = self.last_wall_clock(now);
        if target <= now { target + Duration::days(7) } else { target }
    }

    /// Ultimo reset programmato.
    pub fn last_scheduled_reset(self, now: DateTime<Tz>) -> DateTime<Tz> {
        from_wall_clock(self.last_wall_clock(now.naive_local()))
    }

    pub fn next_scheduled_reset(self, now: DateTime<Tz>) -> DateTime<Tz> {
        from_wall_clock(self.next_wall_clock(now.naive_local()))
    }

    /// Momento del promemoria: 24 ore (di orologio) prima del prossimo reset.
    pub fn reminder_time(self, now: DateTime<Tz>) -> DateTime<Tz> {
        from_wall_clock(self.next_wall_clock(now.naive_local()) - Duration::days(1))
    }

    /// I sondaggi restano aperti fino al prossimo reset settimanale (timestamp Unix).
    pub fn poll_closing_time(self, now: DateTime<Tz>) -> f64 {
        self.next_scheduled_reset(now).timestamp() as f64
    }
}

pub fn last_scheduled_reset(db: &impl Connected, now: DateTime<Tz>) -> DbResult<DateTime<Tz>> {
    Ok(ResetSchedule::from_settings(db)?.last_scheduled_reset(now))
}

pub fn next_scheduled_reset(db: &impl Connected, now: DateTime<Tz>) -> DbResult<DateTime<Tz>> {
    Ok(ResetSchedule::from_settings(db)?.next_scheduled_reset(now))
}

pub fn reminder_time(db: &impl Connected, now: DateTime<Tz>) -> DbResult<DateTime<Tz>> {
    Ok(ResetSchedule::from_settings(db)?.reminder_time(now))
}

pub fn poll_closing_time(db: &impl Connected, now: DateTime<Tz>) -> DbResult<f64> {
    Ok(ResetSchedule::from_settings(db)?.poll_closing_time(now))
}

/// `true` se l'ultimo reset eseguito è precedente all'ultimo reset programmato.
///
/// `last_reset` è un ISO 8601 con offset (es. `2026-11-09T12:00:00+01:00`), come lo scrive Python.
pub fn weekly_reset_pending(db: &impl Connected, now: DateTime<Tz>) -> DbResult<bool> {
    let Some(raw) = database::setting(db, "last_reset")? else {
        return Ok(false);
    };
    let last = DateTime::parse_from_rfc3339(raw.trim())
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
        })?
        .with_timezone(&Utc);
    Ok(last < last_scheduled_reset(db, now)?.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    fn rome(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Tz> {
        TZ.with_ymd_and_hms(year, month, day, hour, minute, 0).unwrap()
    }

    /// (adesso, giorno, ora) -> (ultimo, prossimo, promemoria): timestamp calcolati da
    /// `sfo.schedule` in Python (Europe/Rome via zoneinfo) con gli stessi input.
    #[test]
    fn matches_the_python_implementation() {
        let cases = [
            (rome(2026, 11, 11, 18, 0), 0, 12, 1794222000, 1794826800, 1794740400),
            // Esattamente all'ora del reset: è già "ultimo", il prossimo è tra una settimana.
            (rome(2026, 11, 16, 12, 0), 0, 12, 1794826800, 1795431600, 1795345200),
            (rome(2026, 11, 16, 11, 59), 0, 12, 1794222000, 1794826800, 1794740400),
            // Fine dell'ora legale (25/10): il reset resta alle 12:00 locali.
            (rome(2026, 10, 24, 10, 0), 0, 12, 1792404000, 1793012400, 1792926000),
            (rome(2026, 10, 26, 13, 0), 0, 12, 1793012400, 1793617200, 1793530800),
            // Ora inesistente (29/03 02:00): offset precedente al salto.
            (rome(2026, 3, 28, 10, 0), 6, 2, 1774141200, 1774746000, 1774659600),
            // Ora ambigua (25/10 02:00): prima occorrenza.
            (rome(2026, 10, 24, 10, 0), 6, 2, 1792281600, 1792886400, 1792800000),
            (rome(2026, 11, 11, 18, 0), 6, 9, 1794124800, 1794729600, 1794643200),
            (rome(2026, 12, 31, 23, 30), 2, 0, 1798585200, 1799190000, 1799103600),
        ];
        for (now, weekday, hour, last, next, reminder) in cases {
            let schedule = ResetSchedule::new(weekday, hour).unwrap();
            assert_eq!(schedule.last_scheduled_reset(now).timestamp(), last, "last {now} wd={weekday} h={hour}");
            assert_eq!(schedule.next_scheduled_reset(now).timestamp(), next, "next {now} wd={weekday} h={hour}");
            assert_eq!(schedule.reminder_time(now).timestamp(), reminder, "reminder {now} wd={weekday} h={hour}");
            assert_eq!(schedule.poll_closing_time(now), next as f64);
        }
    }

    #[test]
    fn isoformat_matches_python_with_and_without_microseconds() {
        assert_eq!(isoformat(rome(2026, 11, 9, 12, 0)), "2026-11-09T12:00:00+01:00");
        assert_eq!(isoformat(rome(2026, 7, 1, 9, 5)), "2026-07-01T09:05:00+02:00", "ora legale");
        let with_micros = rome(2026, 11, 9, 12, 0) + Duration::microseconds(123_456);
        assert_eq!(isoformat(with_micros), "2026-11-09T12:00:00.123456+01:00");
        let tiny = rome(2026, 11, 9, 12, 0) + Duration::microseconds(5);
        assert_eq!(isoformat(tiny), "2026-11-09T12:00:00.000005+01:00", "microsecondi a 6 cifre");
        // Il valore scritto da Rust è leggibile dal controllo `weekly_reset_pending`.
        assert!(DateTime::parse_from_rfc3339(&isoformat(with_micros)).is_ok());
    }

    #[test]
    fn new_polls_close_at_the_next_weekly_reset() {
        // Stesso caso di tests/test_weekly_cycle.py: mercoledì -> lunedì successivo alle 12.
        let db = Database::open_in_memory().unwrap();
        let now = rome(2026, 11, 11, 18, 0);
        let expected = rome(2026, 11, 16, 12, 0);
        assert_eq!(poll_closing_time(&db, now).unwrap(), expected.timestamp() as f64);
    }

    #[test]
    fn schedule_follows_the_configured_weekday_and_hour() {
        let db = Database::open_in_memory().unwrap();
        db.set_setting("reset_weekday", 6).unwrap();
        db.set_setting("reset_hour", 9).unwrap();
        let next = next_scheduled_reset(&db, rome(2026, 11, 11, 18, 0)).unwrap();
        assert_eq!(next, rome(2026, 11, 15, 9, 0));
    }

    #[test]
    fn schedule_rejects_out_of_range_values() {
        assert!(ResetSchedule::new(7, 12).is_none());
        assert!(ResetSchedule::new(0, 24).is_none());
        assert!(ResetSchedule::new(6, 23).is_some());
    }

    #[test]
    fn weekly_reset_pending_compares_last_reset_with_the_schedule() {
        let db = Database::open_in_memory().unwrap();
        let now = rome(2026, 11, 11, 18, 0);
        // Nessun reset registrato: niente da recuperare.
        assert!(!weekly_reset_pending(&db, now).unwrap());

        let last_scheduled = last_scheduled_reset(&db, now).unwrap();
        db.set_setting("last_reset", last_scheduled.to_rfc3339()).unwrap();
        assert!(!weekly_reset_pending(&db, now).unwrap(), "reset di questa settimana già fatto");

        db.set_setting("last_reset", (last_scheduled - Duration::days(7)).to_rfc3339()).unwrap();
        assert!(weekly_reset_pending(&db, now).unwrap(), "reset della settimana scorsa");

        db.set_setting("last_reset", "non una data").unwrap();
        assert!(weekly_reset_pending(&db, now).is_err());
    }
}
