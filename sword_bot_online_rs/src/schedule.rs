//! Calendario del reset settimanale e scadenza dei sondaggi (equivalente di `sfo/schedule.py`).
//!
//! Come in Python, i calcoli sono sull'orologio da muro (`timedelta(days=7)` mantiene l'ora
//! locale anche a cavallo del cambio dell'ora legale). Un'ora locale che non esiste (salto in
//! avanti) usa l'offset precedente al cambio e una ambigua (ora che si ripete) la prima
//! occorrenza: sono le stesse regole di `zoneinfo`.
//!
//! # Panoramica del modulo
//!
//! Questo modulo gestisce la logica del "reset settimanale" che è il meccanismo principale
//! per ripristinare le statistiche degli utenti ogni settimana. Il reset avviene una volta
//! a settimana in un orario configurabile (es. ogni lunedì alle 12:00).
//!
//! # Concetti chiave
//!
//! - **Reset settimanale**: un evento ricorrente che ripristina punti, livelli e streak
//!   degli utenti ogni settimana. Simile a un "limbo" settimanale.
//! - **Orologio da muro (wall clock)**: l'ora locale non UTC, ma con l'offset corretto per il
//!   fuso orario (es. +01:00 per Roma in inverno, +02:00 in estate).
//! - **Cambio dell'ora legale**: una volta ogni 6 mesi, l'ora passa da inverno a estate (o viceversa).
//!   Questo modulo gestisce automaticamente questi salti.
//! - **Sondaggi**: i sondaggi aperti vengono chiusi automaticamente al prossimo reset.
//!
//! # Strutture principali
//!
//! - `ResetSchedule`: contiene il giorno e l'orario del reset settimanale.
//!   - 0 = lunedì, 1 = martedì, ..., 6 = domenica
//!   - hour = 0-23 (es. 12 = mezzogiorno)
//!
//! # Funzioni di utilità
//!
//! - `now()`: restituisce l'ora corrente in ora italiana.
//! - `isoformat()`: converte un timestamp in formato stringa ISO 8601 per il database.
//! - `weekly_reset_pending()`: verifica se deve essere eseguito un reset.
//!
//! # Verrà usato da reset, promemoria e sondaggi.
#![allow(dead_code)]

use chrono::{DateTime, Datelike, Duration, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;

use crate::config::TZ;
use crate::database::{self, Connected, DbResult};
use crate::settings;

/// Restituisce l'ora corrente convertita nell'orario italiano (Europe/Rome).
    ///
    /// Questa funzione è fondamentale perché tutti i calcoli del reset e dei promemoria
    /// si basano sull'orologio da muro locale, non sull'ora UTC.
    ///
    /// # Esempio
    ///
    /// Se sono le 11:00 UTC, questa funzione restituisce 12:00 (ora italiana in inverno).
    /// Se sono le 10:00 UTC in estate, restituisce 12:00 (ora italiana in estate).
    ///
    /// # Nota tecnica
    ///
    /// `TZ` è il fuso orario Europe/Rome configurato in `config.rs`. Usa `chrono_tz`
    /// per gestire automaticamente il cambio orario.
pub fn now() -> DateTime<Tz> {
    Utc::now().with_timezone(&TZ)
}

/// Converte un timestamp in formato stringa ISO 8601.
    ///
    /// Questo è il formato che Python scriverebbe con `datetime.isoformat()`.
    /// Esempio: `2026-11-09T12:00:00+01:00` o `2026-11-09T12:00:00.123456+01:00`
    /// con microsecondi solo se diversi da zero.
    ///
    /// # Perché serve
    ///
    /// Il database salva le date in questo formato perché è leggibile da Python.
    /// Quando il bot legge un valore, lo converte nuovamente in `DateTime<Tz>`.
    ///
    /// # Formato specificato
    ///
    /// - `%Y-%m-%d` = anno-mese-giorno (es. 2026-11-09)
    /// - `%H:%M:%S` = ore:minuti:secondi (es. 12:00:00)
    /// - `%:z` = offset dalla UTC con i due punti (es. +01:00)
pub fn isoformat(value: DateTime<Tz>) -> String {
    let micros = value.timestamp_subsec_micros();
    let base = value.format("%Y-%m-%dT%H:%M:%S");
    let offset = value.format("%:z");
    if micros == 0 { format!("{base}{offset}") } else { format!("{base}.{micros:06}{offset}") }
}

/// Converte un orologio da muro (datetime senza fuso) in un istante con fuso.
    ///
    /// Questa funzione gestisce i casi particolari del cambio dell'ora legale:
    /// - **Ore inesistenti**: quando si "salta" in avanti (es. 02:00 diventa 03:00).
    ///   L'ora 02:00 esiste, quindi prendiamo l'offset prima del salto.
    /// - **Ore ambigue**: quando si "torna indietro" (es. 02:00 si ripete due volte).
    ///   Prendiamo la PRIMA occorrenza (quella con l'offset "di prima").
    ///
    /// # Regole zoneinfo
    ///
    /// Questo comporta le stesse regole della libreria Python `zoneinfo`:
    /// - fold=0: indica la prima occorrenza in un'ora ambigua
    /// - per ore inesistenti, usa l'offset valido prima del salto
    ///
    /// # Esempio pratico
    ///
    /// In Italia, il cambio all'estate avviene la notte del 26 marzo:
    /// - le 02:00-02:59 esistono, poi salto a 03:00
    /// - se provieni da `2026-03-29T01:30:00` e aggiungi 1 ora,
    ///   ottieni `2026-03-29T02:30:00` che esiste, quindi va bene
    /// - se provieni da `2026-03-28T03:30:00` e sottrarrai 1 ora,
    ///   ottieni `2026-03-29T02:30:00` che esiste
    /// - ma se il calcolo da solo produrrebbe un'ora inesistente,
    ///   la funzione la risolve correttamente
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

/// Aggiunge una durata a un timestamp, mantenendo l'orologio da muro.
    ///
    /// Questo è il comportamento identico a Python con `timedelta`:
    /// quando aggiungi 3 giorni e 12 ore a un'ora, l'ora locale non cambia
    /// anche se il calcolo attraversa il cambio dell'ora legale.
    ///
    /// # Esempio
    ///
    /// ```text
    /// Ora: lunedì 16:00 (inverno, UTC+1)
    /// + 7 giorni = lunedì successivo 16:00 (inverno, UTC+1)
    ///
    /// Se il reset è lunedì alle 12:00:
    /// - ultimo reset: lunedì 12:00
    /// - prossimo reset: lunedì successivo 12:00
    /// ```
    ///
    /// # Perché è importante
    ///
    /// Se usassimo solo l'aritmetica UTC, il reset potrebbe cambiare ora
    /// quando passiamo dall'inverno all'estate. Con l'orologio da muro,
    /// il reset è sempre alle stesse ore locali.
pub fn shift_wall_clock(value: DateTime<Tz>, delta: Duration) -> DateTime<Tz> {
    from_wall_clock(value.naive_local() + delta)
}

/// Giorno e ora del reset settimanale (configurabili da /config).
///
/// Il reset è un evento ricorrente che ripristina:
/// - I punti guadagnati durante la settimana
/// - I livelli degli utenti
/// - Gli streak di presenza
/// - Lo stato dei sondaggi
///
/// # Campi
///
/// - `weekday`: giorno della settimana (0=lunedì, 1=martedì, ..., 6=domenica)
/// - `hour`: ora del giorno in 24h (0-23, es. 12 = mezzogiorno)
///
/// # Esempio
///
/// ```text
/// ResetSchedule { weekday: 0, hour: 12 }
/// → Reset ogni lunedì alle 12:00
/// ```
///
/// Configurabile via comando `/config`:
/// - `reset_weekday`: giorno del reset
/// - `reset_hour`: ora del reset
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetSchedule {
    /// 0 = lunedì … 6 = domenica.
    weekday: u32,
    hour: u32,
}

impl ResetSchedule {
    /// Crea una nuova programmazione del reset, validando i parametri.
    ///
    /// # Validazione
    ///
    /// - `weekday` deve essere 0-6 (lunedì-domenica)
    /// - `hour` deve essere 0-23 (mezzanotte-mezzogiorno)
    ///
    /// # Ritorno
    ///
    /// - `Some(Self)` se i parametri sono validi
    /// - `None` se i parametri sono fuori range
    ///
    /// # Esempio
    ///
    /// ```text
    /// ResetSchedule::new(0, 12) → Some(ResetSchedule{weekday:0, hour:12}) // lunedì 12:00
    /// ResetSchedule::new(7, 12) → None // giorno 7 non esiste
    /// ResetSchedule::new(0, 24) → None // ora 24 non esiste
    /// ```
    pub fn new(weekday: u32, hour: u32) -> Option<Self> {
        (weekday <= 6 && hour <= 23).then_some(Self { weekday, hour })
    }

    /// Legge il reset corrente dalle impostazioni del database.
    ///
    /// Recupera i valori configurati tramite `/config`:
    /// - `reset_weekday`: giorno del reset (default: 0 = lunedì)
    /// - `reset_hour`: ora del reset (default: 12 = mezzogiorno)
    ///
    /// # Panic
    ///
    /// Panica se i valori nel database sono fuori range. Questo non dovrebbe
    /// mai succedere perché il comando `/config` valida gli input prima di salvare.
    ///
    /// # Errori possibili
    ///
    /// - Errore di database se non può leggere le impostazioni
pub fn from_settings(db: &impl Connected) -> DbResult<Self> {
        let weekday = settings::reset_weekday(db)? as u32;
        let hour = settings::reset_hour(db)? as u32;
        Ok(Self::new(weekday, hour).expect("le impostazioni sono già limitate a 0..=6 e 0..=23"))
    }

    /// Calcola l'ultimo reset programmato rispetto a un'ora data.
    ///
    /// Se siamo già passati l'orario del reset di oggi, ritorna l'orario
    /// di oggi. Altrimenti ritorna lo stesso orario di una settimana prima.
    ///
    /// # Algoritmo
    ///
    /// 1. Calcola quanti giorni devi andare indietro dal giorno odierno
    ///    per raggiungere il giorno del reset settimanale
    /// 2. Crea un orario con l'orario del reset
    /// 3. Se quell'orario è futuro rispetto a `now`, sottrai 7 giorni
    ///
    /// # Esempio
    ///
    /// ```text
    /// Reset: lunedì 12:00
    /// now = mercoledì 14:00
    /// → ultimo reset = lunedì 12:00 (già passato)
    ///
    /// now = lunedì 10:00 (prima del reset)
    /// → ultimo reset = lunedì 12:00 (settimana scorsa)
    /// ```
    fn last_wall_clock(self, now: NaiveDateTime) -> NaiveDateTime {
        let days_back = (i64::from(now.weekday().num_days_from_monday()) - i64::from(self.weekday)).rem_euclid(7);
        let candidate = (now.date() - Duration::days(days_back))
            .and_hms_opt(self.hour, 0, 0)
            .expect("ora validata in `new`");
        if candidate > now { candidate - Duration::days(7) } else { candidate }
    }

    /// Calcola il prossimo reset programmato rispetto a un'ora data.
    ///
    /// Ritorna l'orario del reset più vicino, che può essere oggi o
    /// una settimana in avanti.
    ///
    /// # Esempio
    ///
    /// ```text
    /// Reset: lunedì 12:00
    /// now = mercoledì 14:00
    /// → prossimo reset = lunedì 12:00 (tra 5 giorni)
    ///
    /// now = lunedì 10:00 (prima del reset)
    /// → prossimo reset = lunedì 12:00 (tra 2 ore)
    /// ```
    fn next_wall_clock(self, now: NaiveDateTime) -> NaiveDateTime {
        let target = self.last_wall_clock(now);
        if target <= now { target + Duration::days(7) } else { target }
    }

    /// Timestamp dell'ultimo reset che dovrebbe essere stato eseguito.
    ///
    /// Questo è l'orario del reset più recente che dovrebbe essere stato
    /// completato. Viene confrontato con `last_reset` nel database per verificare
    /// se c'è un reset pendente.
    ///
    /// # Usaggio
    ///
    /// La funzione `weekly_reset_pending()` confronta questo valore con il
    /// timestamp salvato in `last_reset`. Se il salvato è più vecchio,
    /// allora c'è un reset da fare.
    ///
    /// # Conversione
    ///
    /// Usa `from_wall_clock()` per gestire correttamente il cambio dell'ora legale.
    pub fn last_scheduled_reset(self, now: DateTime<Tz>) -> DateTime<Tz> {
        from_wall_clock(self.last_wall_clock(now.naive_local()))
    }

    /// Timestamp del prossimo reset che deve avvenire.
    ///
    /// Questo è l'orario del prossimo reset settimanale.
    /// Usato per:
    /// - Chiusura automatica dei sondaggi
    /// - Calcolo del promemoria
    /// - Aggiornamento del dashboard
    pub fn next_scheduled_reset(self, now: DateTime<Tz>) -> DateTime<Tz> {
        from_wall_clock(self.next_wall_clock(now.naive_local()))
    }

    /// Timestamp del promemoria prima del prossimo reset.
    ///
    /// Il promemoria viene inviato 24 ore prima del reset.
    /// Così gli utenti possono vedere i loro punti prima che vengano azzerati.
    ///
    /// # Esempio
    ///
    /// ```text
    /// Reset: lunedì 12:00
    /// now = domenica 10:00
    /// → promemoria = domenica 12:00
    ///
    /// now = lunedì 10:00 (giorno del reset)
    /// → promemoria = domenica 12:00 (già passato, nessun promemoria)
    /// ```
    pub fn reminder_time(self, now: DateTime<Tz>) -> DateTime<Tz> {
        from_wall_clock(self.next_wall_clock(now.naive_local()) - Duration::days(1))
    }

    /// Timestamp Unix di quando chiudere i sondaggi.
    ///
    /// I sondaggi sono chiusi al prossimo reset settimanale.
    /// Questo timestamp Unix lo usa Discord per le notifiche di chiusura.
    ///
    /// # Perché Unix timestamp?
    ///
    /// Discord usa formato Unix per le notifiche dei sondaggi.
    /// Esempio: `<t:1794826800:F>` mostra "lunedì 16 novembre 2026, ore 12:00:00".
    pub fn poll_closing_time(self, now: DateTime<Tz>) -> f64 {
        self.next_scheduled_reset(now).timestamp() as f64
    }
}

/// Funzioni di utilità pubbliche che combinano `from_settings` e i metodi dell'impl.
///
/// Queste funzioni sono un'alternativa più comoda da usare quando non hai
/// bisogno del `ResetSchedule` direttamente.
///
/// # Esempio d'uso
///
/// ```text
/// let next = next_scheduled_reset(&db, now)?;
/// let ts = next.timestamp();
/// // Usa ts per Discord o per salvare nel database
/// ```
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

/// Verifica se c'è un reset settimanale pendente da eseguire.
    ///
    /// Confronta l'ultimo reset registrato nel database con il prossimo reset
    /// che dovrebbe avvenire secondo la programmazione.
    ///
    /// # Logica
    ///
    /// ```text
    /// last_reset (dal DB)  <  next_scheduled_reset
    ///         │                        │
    ///     "2026-11-09"          "2026-11-16"
    ///         │                        │
    ///         └───  Settimana 1 ───────┘  → Pendente!
    /// ```
    ///
    /// Se `last_reset` è già aggiornato all'ultimo reset programmato,
    /// allora non c'è nulla da fare.
    ///
    /// # Formato del database
    ///
    /// `last_reset` è salvato in formato ISO 8601 con offset:
    /// `2026-11-09T12:00:00+01:00`
    /// Questo è il formato che Python scriverebbe con `datetime.isoformat()`.
    ///
    /// # Casi particolari
    ///
    /// - Se `last_reset` non esiste nel database → `Ok(false)`, nessun reset da fare
    /// - Se il valore nel database non è una data valida → errore
    /// - Se `last_reset` è più vecchio del prossimo reset → `Ok(true)`
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
