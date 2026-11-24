//! Avvio del database e manutenzione iniziale (corrisponde a `core.initialize` in `sfo/core.py`).
//!
//! In Python la funzione `initialize` si occupava anche di azzerare lo stato in memoria
//! relativo ai sondaggi attivi, al registro delle azioni staff e all'interfaccia dashboard.
//! In Rust, grazie alle garanzie sul ciclo di vita della memoria e sulla concorrenza,
//! questi moduli nascono già puliti nei rispettivi contesti, pertanto qui ci concentriamo
//! esclusivamente sulla manutenzione persistente del database SQLite e sull'allineamento dei dati.

// `allow(dead_code)` evita warning da parte del compilatore per funzioni integrate
// che verranno richiamate dai moduli di livello superiore (es. bot Discord e web server).
#![allow(dead_code)]

use chrono::DateTime;
use chrono_tz::Tz;

use crate::database::{Database, DbResult};
use crate::{schedule, seasonal};

/// Esegue le procedure di manutenzione iniziale del database, allinea le scadenze dei sondaggi
/// e garantisce la presenza delle impostazioni predefinite di sistema.
///
/// # Parametri
/// - `db`: Riferimento condiviso al database (`&Database`).
/// - `now`: Data e ora correnti con fuso orario italiano (`DateTime<Tz>`). Passare questo parametro
///   dall'esterno rende l'inizializzazione deterministica e facilmente testabile.
///
/// # Dettagli di Funzionamento e Concorrenza
/// 1. Chiama `startup_maintenance()` per applicare la pulizia dei record obsoleti e le ultime migrazioni.
/// 2. Calcola la data del prossimo reset (`next_close`) **prima** di aprire una transazione.
///    Questo passaggio è fondamentale in Rust: `Database` gestisce la concurrency tramite Mutex e le transazioni
///    mantengono un lock esclusivo sulla connessione. Calcolare il reset all'interno della closure della transazione
///    causerebbe un errore di rientranza (deadlock).
/// 3. Avvia una transazione atomica per:
///    - Aggiornare tutti i sondaggi attivi privi di data o con scadenza antecedente al prossimo reset.
///    - Salvare la data di partenza delle statistiche (`stats_since`) e dei premi (`awards_since`) se non presenti.
///    - Assicurare l'esistenza della stagione corrente tramite il modulo `seasonal`.
/// 4. Inserisce i dati predefiniti di base con `seed_defaults()`.
pub fn initialize(db: &Database, now: DateTime<Tz>) -> DbResult<()> {
    // Manutenzione iniziale (es. rimozione sessioni ed eventi scaduti).
    db.startup_maintenance()?;

    // Calcolo preventivo della data del prossimo reset dei sondaggi.
    let next_close = schedule::poll_closing_time(db, now)?;
    let today = now.date_naive();
    let today_text = today.to_string();

    // Transazione atomica per l'aggiornamento coerente delle impostazioni iniziali.
    db.transaction(|tx| -> DbResult<()> {
        // I sondaggi ancora attivi devono chiudere al prossimo reset settimanale.
        // Questo aggiornamento corregge anche eventuali scadenze impostate in precedenza
        // con la vecchia regola fissa dei 7 giorni dalla creazione.
        tx.execute(
            "UPDATE polls SET closes_at=? WHERE active=1 AND (closes_at IS NULL OR closes_at<?)",
            [next_close, next_close],
        )?;

        // Imposta le date di inizio per statistiche e premi se il database è nuovo.
        for key in ["stats_since", "awards_since"] {
            tx.execute(
                "INSERT OR IGNORE INTO settings(key,value) VALUES(?,?)",
                [key, today_text.as_str()],
            )?;
        }

        // Crea o verifica l'esistenza della stagione legata alla data attuale (es. "autunno-2026").
        seasonal::ensure_season(tx, today)?;
        Ok(())
    })?;

    // Inserisce i sondaggi e le opzioni base di default se il database è privo di dati.
    db.seed_defaults()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use chrono::TimeZone;

    /// Genera una data di test fissa (Mercoledì 11 Novembre 2026 alle 18:00 ora italiana).
    fn now() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap() // Mercoledì
    }

    /// Calcola il timestamp del prossimo reset atteso (Lunedì 16 Novembre 2026 alle 12:00).
    fn next_reset() -> f64 {
        TZ.with_ymd_and_hms(2026, 11, 16, 12, 0, 0).unwrap().timestamp() as f64
    }

    /// Helper di test per estrarre il timestamp di chiusura di un sondaggio dal suo nome.
    fn closes_at(db: &Database, name: &str) -> Option<f64> {
        db.with_conn(|c| {
            c.query_row("SELECT closes_at FROM polls WHERE name=?", [name], |row| row.get(0)).unwrap()
        })
    }

    #[test]
    /// Verifica che un database appena creato riceva i valori predefiniti, la stagione e le date iniziali.
    fn fresh_database_gets_defaults_dates_and_season() {
        let db = Database::open_in_memory().unwrap();
        initialize(&db, now()).unwrap();

        // Verifica il salvataggio della data iniziale
        assert_eq!(db.get_setting("stats_since").unwrap().as_deref(), Some("2026-11-11"));
        assert_eq!(db.get_setting("awards_since").unwrap().as_deref(), Some("2026-11-11"));

        // Verifica l'inserimento della stagione corrente
        let seasons: Vec<String> = db.with_conn(|c| {
            c.prepare("SELECT season_key FROM seasons").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        });
        assert_eq!(seasons, ["autunno-2026"]);

        // Verifica l'inserimento dei sondaggi predefiniti
        let polls: i64 = db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM polls", [], |r| r.get(0)).unwrap());
        assert_eq!(polls, 5);
    }

    #[test]
    /// Verifica che i sondaggi attivi vengano allineati alla data del prossimo reset settimanale.
    fn active_polls_are_aligned_to_the_next_reset() {
        let db = Database::open_in_memory().unwrap();
        let far_future = next_reset() + 100_000.0;

        // Inserimento di vari scenari di sondaggi
        db.execute("INSERT INTO polls(name,active,closes_at) VALUES('senza scadenza',1,NULL)", []).unwrap();
        db.execute("INSERT INTO polls(name,active,closes_at) VALUES('scadenza vecchia',1,1000)", []).unwrap();
        db.execute("INSERT INTO polls(name,active,closes_at) VALUES('scadenza lontana',1,?)", [far_future]).unwrap();
        db.execute("INSERT INTO polls(name,active,closes_at) VALUES('chiuso dallo staff',0,NULL)", []).unwrap();

        initialize(&db, now()).unwrap();

        // Esito atteso: i sondaggi senza scadenza o vecchi vengono portati al prossimo reset.
        assert_eq!(closes_at(&db, "senza scadenza"), Some(next_reset()));
        assert_eq!(closes_at(&db, "scadenza vecchia"), Some(next_reset()));
        // Le scadenze già programmate più lontane non vengono accorciate.
        assert_eq!(closes_at(&db, "scadenza lontana"), Some(far_future), "non si accorcia");
        // I sondaggi disattivati mantengono lo stato disattivato senza modificare la chiusura.
        assert_eq!(closes_at(&db, "chiuso dallo staff"), None, "i sondaggi chiusi restano chiusi");
    }

    #[test]
    /// Verifica che l'inizializzazione sia idempotente (eseguibile più volte senza alterare i dati esistenti).
    fn initialize_is_idempotent_and_keeps_existing_since_dates() {
        let db = Database::open_in_memory().unwrap();
        // Imposta una data precedente personalizzata
        db.set_setting("stats_since", "2025-01-01").unwrap();

        // Esegue due volte l'inizializzazione
        initialize(&db, now()).unwrap();
        initialize(&db, now()).unwrap();

        // La data precedente non deve essere sovrascritta
        assert_eq!(db.get_setting("stats_since").unwrap().as_deref(), Some("2025-01-01"));
        assert_eq!(db.get_setting("awards_since").unwrap().as_deref(), Some("2026-11-11"));

        // I dati iniziali di seed non devono essere duplicati
        let polls: i64 = db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM polls", [], |r| r.get(0)).unwrap());
        assert_eq!(polls, 5, "il seed non si ripete");
    }
}
