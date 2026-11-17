//! Impostazioni modificabili da /config, con i valori predefiniti di `config`
//! (equivalente di `sfo/settings.py`).
// Verranno usate dai moduli di sondaggi, proposte e reset.
#![allow(dead_code)]

use crate::config;
use crate::database::{self, Connected, DbResult};

pub const WEEKDAY_NAMES: [&str; 7] = [
    "lunedì",
    "martedì",
    "mercoledì",
    "giovedì",
    "venerdì",
    "sabato",
    "domenica",
];

/// Legge un intero dalle impostazioni; se manca, non è numerico o è fuori da
/// `minimum..=maximum` restituisce `default`.
fn int_setting(db: &impl Connected, key: &str, default: i64, minimum: i64, maximum: i64) -> DbResult<i64> {
    let Some(raw) = database::setting(db, key)? else {
        return Ok(default);
    };
    Ok(match raw.trim().parse::<i64>() {
        Ok(value) if (minimum..=maximum).contains(&value) => value,
        _ => default,
    })
}

pub fn max_votes(db: &impl Connected) -> DbResult<i64> {
    int_setting(db, "max_votes", config::MAX_VOTES, 1, 10)
}

pub fn max_proposals(db: &impl Connected) -> DbResult<i64> {
    int_setting(db, "max_proposals", config::MAX_PROPOSALS, 1, 20)
}

pub fn cooldown_seconds(db: &impl Connected) -> DbResult<i64> {
    int_setting(db, "cooldown_seconds", config::COOLDOWN_SECONDS, 0, 3600)
}

pub fn ballot_slots(db: &impl Connected) -> DbResult<i64> {
    int_setting(db, "slots", config::DEFAULT_SLOTS, 1, 25)
}

pub fn reset_weekday(db: &impl Connected) -> DbResult<i64> {
    int_setting(db, "reset_weekday", config::DEFAULT_RESET_WEEKDAY, 0, 6)
}

pub fn reset_hour(db: &impl Connected) -> DbResult<i64> {
    int_setting(db, "reset_hour", config::DEFAULT_RESET_HOUR, 0, 23)
}

pub fn reminder_enabled(db: &impl Connected) -> DbResult<bool> {
    Ok(database::setting_or(db, "reset_reminder_enabled", "1")? == "1")
}

pub fn reset_label(db: &impl Connected) -> DbResult<String> {
    let weekday = reset_weekday(db)? as usize; // già limitato a 0..=6
    Ok(format!("ogni {} alle {:02}:00", WEEKDAY_NAMES[weekday], reset_hour(db)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;

    #[test]
    fn defaults_apply_when_settings_are_missing() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(max_votes(&db).unwrap(), 2);
        assert_eq!(max_proposals(&db).unwrap(), 3);
        assert_eq!(cooldown_seconds(&db).unwrap(), 30);
        assert_eq!(ballot_slots(&db).unwrap(), 5);
        assert!(reminder_enabled(&db).unwrap());
        assert_eq!(reset_label(&db).unwrap(), "ogni lunedì alle 12:00");
    }

    #[test]
    fn valid_values_are_used_and_invalid_ones_fall_back() {
        let db = Database::open_in_memory().unwrap();
        db.set_setting("max_votes", 5).unwrap();
        assert_eq!(max_votes(&db).unwrap(), 5);
        db.set_setting("max_votes", 11).unwrap(); // sopra il massimo
        assert_eq!(max_votes(&db).unwrap(), 2);
        db.set_setting("max_votes", "tanti").unwrap(); // non numerico
        assert_eq!(max_votes(&db).unwrap(), 2);
        db.set_setting("cooldown_seconds", 0).unwrap(); // 0 è ammesso qui
        assert_eq!(cooldown_seconds(&db).unwrap(), 0);
    }

    #[test]
    fn reset_label_follows_weekday_and_hour() {
        let db = Database::open_in_memory().unwrap();
        db.set_setting("reset_weekday", 6).unwrap();
        db.set_setting("reset_hour", 9).unwrap();
        assert_eq!(reset_label(&db).unwrap(), "ogni domenica alle 09:00");
        db.set_setting("reset_reminder_enabled", "0").unwrap();
        assert!(!reminder_enabled(&db).unwrap());
    }
}
