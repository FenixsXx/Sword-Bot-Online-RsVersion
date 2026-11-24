//! Impostazioni modificabili da /config, con i valori predefiniti di `config`
//! (equivalente di `sfo/settings.py`).
//!
//! Questo modulo gestisce le impostazioni configurabili del bot. Ogni impostazione
//! ha:
//! - Una chiave nel database (es. `"max_votes"`)
//! - Un valore predefinito (definito in `config.rs`)
//! - Un range di valori validi (min, max)
//!
//! # Come funziona
//!
//! Quando un utente usa `/config`, il valore viene validato e salvato nel database.
//! Quando il bot ha bisogno del valore, lo legge dal database. Se il valore
//! manca, non è numerico, o è fuori range, usa il valore predefinito.
//!
//! # Impostazioni disponibili
//!
//! - `max_votes`: Quanti voti per sondaggio (default: 2, range: 1-10)
//! - `max_proposals`: Quante proposte aperte per utente (default: 3, range: 1-20)
//! - `cooldown_seconds`: Secondi tra un voto e l'altro (default: 30, range: 0-3600)
//! - `slots`: Quanti slot nel ballottaggio (default: 5, range: 1-25)
//! - `reset_weekday`: Giorno del reset settimanale (default: 0=lunedì, range: 0-6)
//! - `reset_hour`: Ora del reset (default: 12, range: 0-23)
//! - `reset_reminder_enabled`: Se inviare promemoria (default: 1=sì, "1"/"0")
//!
//! # Verranno usate dai moduli di sondaggi, proposte e reset.
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

/// Legge un intero dal database con validazione e fallback.
///
/// Questa è la funzione centrale che tutte le altre usano. Gestisce:
/// - Valore mancante nel database → restituisce `default`
/// - Valore non numerico (es. `"tanti"`) → restituisce `default`
/// - Valore fuori range (es. 11 quando max è 10) → restituisce `default`
/// - Valore valido → restituisce il valore
///
/// # Parametri
///
/// - `db`: Connessione al database
/// - `key`: Chiave della impostazione (es. `"max_votes"`)
/// - `default`: Valore predefinito da usare se c'è un problema
/// - `minimum`: Valore minimo accettabile (incluso)
/// - `maximum`: Valore massimo accettabile (incluso)
///
/// # Esempio
///
/// ```text
/// // Database ha max_votes = "5"
/// int_setting(db, "max_votes", 2, 1, 10) → Ok(5)
///
/// // Database ha max_votes = "11" (troppo alto)
/// int_setting(db, "max_votes", 2, 1, 10) → Ok(2)
///
/// // Database non ha max_votes
/// int_setting(db, "max_votes", 2, 1, 10) → Ok(2)
/// ```
fn int_setting(db: &impl Connected, key: &str, default: i64, minimum: i64, maximum: i64) -> DbResult<i64> {
    let Some(raw) = database::setting(db, key)? else {
        return Ok(default);
    };
    Ok(match raw.trim().parse::<i64>() {
        Ok(value) if (minimum..=maximum).contains(&value) => value,
        _ => default,
    })
}

/// Massimo numero di voti per sondaggio per utente.
    ///
    /// Default: 2 (da config). Range: 1-10.
    /// Usato per limitare quanti voti può dare un utente in un sondaggio.
    pub fn max_votes(db: &impl Connected) -> DbResult<i64> {
        int_setting(db, "max_votes", config::MAX_VOTES, 1, 10)
    }

    /// Massimo numero di proposte aperte simultaneamente per utente.
    ///
    /// Default: 3 (da config). Range: 1-20.
    /// Impedisce a un utente di aprire troppe proposte alla volta.
    pub fn max_proposals(db: &impl Connected) -> DbResult<i64> {
        int_setting(db, "max_proposals", config::MAX_PROPOSALS, 1, 20)
    }

    /// Cooldown in secondi tra un voto e l'altro per lo stesso utente.
    ///
    /// Default: 30 secondi (da config). Range: 0-3600 (1 ora).
    /// 0 = nessun cooldown. Usato per evitare spam di voti.
    pub fn cooldown_seconds(db: &impl Connected) -> DbResult<i64> {
        int_setting(db, "cooldown_seconds", config::COOLDOWN_SECONDS, 0, 3600)
    }

    /// Numero di slot (opzioni) nel ballottaggio settimanale.
    ///
    /// Default: 5 (da config). Range: 1-25.
    /// Il limite 25 è il massimo di opzioni per un sondaggio Discord.
    pub fn ballot_slots(db: &impl Connected) -> DbResult<i64> {
        int_setting(db, "slots", config::DEFAULT_SLOTS, 1, 25)
    }

    /// Giorno della settimana del reset (0=lunedì ... 6=domenica).
    ///
    /// Default: 0=lunedì (da config). Range: 0-6.
    /// Determina quando avviene il reset settimanale.
    pub fn reset_weekday(db: &impl Connected) -> DbResult<i64> {
        int_setting(db, "reset_weekday", config::DEFAULT_RESET_WEEKDAY, 0, 6)
    }

    /// Ora del giorno del reset (0-23).
    ///
    /// Default: 12=mezzogiorno (da config). Range: 0-23.
    /// Determina l'ora esatta del reset settimanale.
    pub fn reset_hour(db: &impl Connected) -> DbResult<i64> {
        int_setting(db, "reset_hour", config::DEFAULT_RESET_HOUR, 0, 23)
    }

    /// Se il promemoria del reset è attivo.
    ///
    /// Default: true (1). Salvato come "1" o "0" nel database.
    /// Se true, il bot invia un promemoria 24 ore prima del reset.
    pub fn reminder_enabled(db: &impl Connected) -> DbResult<bool> {
        Ok(database::setting_or(db, "reset_reminder_enabled", "1")? == "1")
    }

    /// Etichetta leggibile dell'orario del reset per messaggi e embed.
    ///
    /// Combina `reset_weekday` e `reset_hour` in una stringa italiana.
    /// Esempio: "ogni lunedì alle 12:00" o "ogni domenica alle 09:00"
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
