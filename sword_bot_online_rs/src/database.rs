//! Connessione SQLite condivisa, migrazioni dello schema e impostazioni
//! (equivalente di `sfo/database.py`).
//!
//! Come in Python, la connessione è una sola ed è condivisa da tutto il bot. È dietro un
//! `Mutex` (un lucchetto per la sicurezza tra thread): usa `with_conn` o `transaction`
//! per blocchi **brevi e sincroni**. Non tenere MAI il lock attraverso un `.await`
//! (bloccante per l'intero bot). Il lock **non è rientrante**: dentro `with_conn` o
//! `transaction` usa la connessione passata, mai altri metodi di `Database` (deadlock!).
//!
//! Differenza da `sqlite3` di Python: qui ogni operazione SQL è in autocommit
//! (rusqlite non apre transazioni implicite). Per operazioni multiple atomiche
//! (tutto o niente) usa `Database::transaction`.
// Molte funzioni servono solo ai moduli che porteremo nelle prossime iterazioni.
#![allow(dead_code)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use tracing::{info, warn};

pub type DbResult<T> = rusqlite::Result<T>;

/// Schema storico, già con tutte le migrazioni ad-hoc precedenti al versionamento.
const MIGRATION_1_SQL: &str = include_str!("sql/migration_1.sql");
/// Tabelle per revisione rapida, attività di voto giornaliera e promemoria.
const MIGRATION_2_SQL: &str = include_str!("sql/migration_2.sql");

type Migration = (&'static str, fn(&Connection) -> DbResult<()>);

/// Ogni migrazione gira una sola volta, in ordine; `PRAGMA user_version` ricorda l'ultima
/// applicata. Per cambiare lo schema aggiungi una funzione in fondo alla lista: non
/// modificare quelle già pubblicate.
const MIGRATIONS: [Migration; 2] = [
    ("migration_1_legacy_schema", migration_1_legacy_schema),
    ("migration_2_feature_tables", migration_2_feature_tables),
];

/// Qualcosa da cui si può ottenere una connessione: il `Database` condiviso (prende il lock) oppure
/// una `Connection`/`Transaction` già in mano (nessun lock).
///
/// Le query scritte come `fn(db: &impl Connected, ...)` funzionano quindi sia da sole sia dentro
/// `Database::transaction`, dove il lock è già preso e chiamare un metodo di `Database` darebbe
/// deadlock: lì si passa la `Transaction`.
pub trait Connected {
    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T;
}

impl Connected for Database {
    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        Database::with_conn(self, f)
    }
}

impl Connected for Connection {
    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        f(self)
    }
}

impl Connected for Transaction<'_> {
    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        f(self)
    }
}

/// Valore di un'impostazione (equivalente di `get_setting` senza predefinito).
pub fn setting(source: &impl Connected, key: &str) -> DbResult<Option<String>> {
    source.with_conn(|connection| setting_in(connection, key))
}

/// Valore di un'impostazione, o `default` se manca.
pub fn setting_or(source: &impl Connected, key: &str, default: &str) -> DbResult<String> {
    Ok(setting(source, key)?.unwrap_or_else(|| default.to_string()))
}

/// Settimana corrente (equivalente di `current_week()`): un valore non numerico è un errore.
pub fn current_week(source: &impl Connected) -> DbResult<i64> {
    source.with_conn(current_week_in)
}

fn setting_in(connection: &Connection, key: &str) -> DbResult<Option<String>> {
    connection
        .query_row("SELECT value FROM settings WHERE key=?", [key], |row| row.get(0))
        .optional()
        .map(Option::flatten)
}

fn current_week_in(connection: &Connection) -> DbResult<i64> {
    let raw = setting_in(connection, "week")?.unwrap_or_else(|| "1".to_string());
    raw.trim().parse::<i64>().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

/// Maniglia clonabile sulla connessione condivisa.
#[derive(Clone)]
pub struct Database {
    connection: Arc<Mutex<Connection>>,
    path: Arc<str>,
}

impl Database {
    /// Apre il database e applica le migrazioni mancanti.
    pub fn open(path: &str) -> DbResult<Self> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection, path)
    }

    /// Database temporaneo in memoria (per i test).
    pub fn open_in_memory() -> DbResult<Self> {
        Self::from_connection(Connection::open_in_memory()?, ":memory:")
    }

    fn from_connection(connection: Connection, path: &str) -> DbResult<Self> {
        connection.pragma_update(None, "foreign_keys", "ON")?;
        // `journal_mode` restituisce una riga: va letta con `pragma_update_and_check`.
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
        // In modalità WAL, `synchronous = NORMAL` garantisce la piena consistenza riducendo i blocchi I/O su disco.
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        // Tabelle temporanee e indici di ordinamento in RAM anziché su file temporanei su disco.
        connection.pragma_update(None, "temp_store", "MEMORY")?;
        connection.busy_timeout(Duration::from_millis(1000))?;
        migrate(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path: Arc::from(path),
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // Un panic mentre il lock è preso non deve rendere il database inutilizzabile.
        self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Esegue `f` con la connessione. Il blocco NON deve contenere `.await`.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        f(&self.lock())
    }

    /// Unico modo di aprire una transazione di scrittura multi-statement.
    ///
    /// Apre con `BEGIN IMMEDIATE`, salva se `f` restituisce `Ok` e annulla se restituisce `Err`
    /// (o va in panic). Il blocco NON deve contenere `.await`.
    pub fn transaction<T, E: From<rusqlite::Error>>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        let connection = self.lock();
        let tx = Transaction::new_unchecked(&connection, TransactionBehavior::Immediate)?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }

    /// Esegue una scrittura (autocommit). Equivalente di `q()` in Python.
    pub fn execute(&self, sql: &str, params: impl rusqlite::Params) -> DbResult<usize> {
        self.lock().execute(sql, params)
    }

    /// Esegue uno o più statement SQL senza parametri (autocommit per ciascuno).
    pub fn execute_batch(&self, sql: &str) -> DbResult<()> {
        self.lock().execute_batch(sql)
    }

    // --- Gestione delle impostazioni nel database ---------------------------------------------------

    /// Recupera il valore di una impostazione dal database.
    /// Restituisce `Some(String)` se l'impostazione esiste, `None` altrimenti.
    pub fn get_setting(&self, key: &str) -> DbResult<Option<String>> {
        setting_in(&self.lock(), key)
    }

    /// Recupera il valore di una impostazione, fornendo un valore di default se non esiste.
    pub fn get_setting_or(&self, key: &str, default: &str) -> DbResult<String> {
        Ok(self.get_setting(key)?.unwrap_or_else(|| default.to_string()))
    }

    /// Salva o aggiorna un'impostazione nel database.
    /// Se la chiave esiste, il valore viene aggiornato; altrimenti, viene inserita una nuova riga.
    pub fn set_setting(&self, key: &str, value: impl ToString) -> DbResult<()> {
        self.execute(
            "INSERT INTO settings(key,value) VALUES(?,?) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value.to_string()],
        )?;
        Ok(())
    }

    /// Recupera il numero della settimana corrente dal database, convertendolo in `i64`.
    /// Se il valore non è un numero valido, restituisce un errore.
    pub fn current_week(&self) -> DbResult<i64> {
        current_week_in(&self.lock())
    }

    // --- Manutenzione e inizializzazione del database -----------------------------------------------

    /// Esegue operazioni di pulizia e controllo leggere ogni volta che il bot si avvia.
    /// Queste non sono migrazioni di schema, ma aggiustamenti dello stato.
    pub fn startup_maintenance(&self) -> DbResult<()> {
        self.with_conn(startup_maintenance)
    }

    /// Popola il database con i sondaggi predefiniti e le relative opzioni, se il database è vuoto.
    /// Questo assicura che il bot abbia dati di partenza validi al primo avvio.
    pub fn seed_defaults(&self) -> DbResult<()> {
        self.transaction(|tx| -> DbResult<()> {
            let polls: i64 = tx.query_row("SELECT COUNT(*) FROM polls", [], |row| row.get(0))?;
            if polls > 0 {
                return Ok(()); // Non ripopolare se ci sono già sondaggi
            }
            // Lista di sondaggi predefiniti da creare: (nome, emoji, colore, è_community, opzioni).
            const SEED: [(&str, &str, &str, i64, &[&str]); 5] = [
                ("Giochi gratis da scaricare", "🎮", "#5865F2", 0, &["Among Us", "Supermarket", "Fortnite"]),
                ("Giochi da Discord", "💬", "#9B6BDF", 0, &["Poker Night", "Chess", "Putt Party"]),
                (
                    "Giochi con link/codice",
                    "🔗",
                    "#2FB5A0",
                    0,
                    &[
                        "Skribbl.io",
                        "Gartic Phone",
                        "Codenames",
                        "Make It Meme",
                        "AMQ",
                        "Bomb Party",
                        "Jackbox",
                        "Kahoot",
                    ],
                ),
                ("Giochi senza link", "🎲", "#E0A030", 0, &["Hunger Games", "Jeopardy"]),
                ("Proposte della community", "🏆", "#D95F6A", 1, &[]),
            ];
            // Inserisce ogni sondaggio e le sue opzioni associate nel database.
            for (position, (name, emoji, color, community, options)) in SEED.iter().enumerate() {
                tx.execute(
                    "INSERT INTO polls(name,emoji,color,position,is_community) VALUES(?,?,?,?,?)",
                    params![name, emoji, color, position as i64, community],
                )?;
                let poll_id = tx.last_insert_rowid(); // Prende l'ID del sondaggio appena inserito
                for label in *options {
                    tx.execute("INSERT INTO options(poll_id,label) VALUES(?,?)", params![poll_id, label])?;
                }
            }
            Ok(())
        })
    }
}

// --- Funzioni di migrazione dello schema del database ------------------------------------------

/// Applica le migrazioni dello schema del database in ordine, se non sono già state applicate.
/// Ogni migrazione aggiorna `PRAGMA user_version` per tenere traccia dello stato.
pub fn migrate(connection: &Connection) -> DbResult<()> {
    // Recupera la versione attuale dello schema dal database.
    let current: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    // Itera su tutte le migrazioni definite.
    for (index, (name, migration)) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1; // La versione della migrazione corrente.
        if version <= current {
            continue; // Salta le migrazioni già applicate.
        }
        info!("Applico la migrazione del database {version} ({name}).");
        migration(connection)?; // Esegue la logica della migrazione.
        connection.pragma_update(None, "user_version", version)?; // Aggiorna la versione.
    }
    Ok(())
}

/// Recupera i nomi di tutte le colonne di una data tabella.
/// Usato durante le migrazioni per verificare la presenza di colonne.
fn table_columns(connection: &Connection, table: &str) -> DbResult<HashSet<String>> {
    // `table` è sempre una costante interna: nessun input esterno finisce qui (prevenzione SQL injection).
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement.query_map([], |row| row.get::<_, String>("name"))?;
    names.collect()
}

/// Verifica se la chiave esterna `options.suggestion_id` è configurata correttamente.
/// Controlla che punti a `suggestions(id)` e abbia `ON DELETE SET NULL`.
fn options_suggestion_fk_is_current(connection: &Connection) -> DbResult<bool> {
    let mut statement = connection.prepare("PRAGMA foreign_key_list(options)")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>("from")?, // Colonna locale
            row.get::<_, String>("table")?, // Tabella esterna
            row.get::<_, Option<String>>("to")?,  // Colonna esterna
            row.get::<_, String>("on_delete")?, // Azione in caso di cancellazione
        ))
    })?;
    for row in rows {
        let (from, table, to, on_delete) = row?;
        // Se la chiave esterna riguarda la colonna `suggestion_id`...
        if from == "suggestion_id" {
            // ...verifica che punti alla tabella `suggestions`, alla colonna `id` e che l'azione
            // `ON DELETE` sia `SET NULL` (ignora maiuscole/minuscole).
            return Ok(table == "suggestions"
                && to.as_deref() == Some("id")
                && on_delete.eq_ignore_ascii_case("SET NULL"));
        }
    }
    Ok(false) // Nessuna chiave esterna trovata per `suggestion_id` o non conforme.
}

/// Ricrea la tabella `options` per aggiungere o correggere la chiave esterna `ON DELETE SET NULL`.
/// Questo è necessario perché SQLite non permette di modificare direttamente le chiavi esterne.
fn rebuild_options_table(connection: &Connection) -> DbResult<()> {
    // Salva le definizioni degli indici esistenti sulla tabella `options` per ricrearli dopo.
    let option_indexes: Vec<String> = connection
        .prepare(
            "SELECT sql FROM sqlite_master \
             WHERE type='index' AND tbl_name='options' AND sql IS NOT NULL",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<DbResult<_>>()?;

    // Temporaneamente disattiva i controlli delle chiavi esterne per permettere la ricreazione della tabella.
    // Questo PRAGMA deve essere eseguito fuori da una transazione.
    connection.pragma_update(None, "foreign_keys", "OFF")?;

    // Esegue le operazioni di ricreazione della tabella all'interno di una transazione.
    let result = (|| -> DbResult<()> {
        let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
        // Conta le opzioni che fanno riferimento a `suggestion_id` inesistenti.
        let orphaned: i64 = tx.query_row(
            "SELECT COUNT(*) FROM options o \
             LEFT JOIN suggestions s ON s.id=o.suggestion_id \
             WHERE o.suggestion_id IS NOT NULL AND s.id IS NULL",
            [],
            |row| row.get(0),
        )?;
        if orphaned > 0 {
            warn!(
                "Trovate {orphaned} opzioni con suggestion_id inesistente; \
                 i riferimenti verranno azzerati durante la migrazione."
            );
        }
        // Passaggi per ricreare la tabella:
        // 1. Crea una nuova tabella `options_new` con la chiave esterna corretta.
        // 2. Copia i dati dalla vecchia `options` alla nuova, gestendo gli `suggestion_id` orfani.
        // 3. Elimina la vecchia tabella `options`.
        // 4. Rinomina `options_new` in `options`.
        tx.execute_batch(
            "CREATE TABLE options_new (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                poll_id INTEGER NOT NULL REFERENCES polls(id) ON DELETE CASCADE,
                label TEXT NOT NULL,
                suggestion_id INTEGER REFERENCES suggestions(id) ON DELETE SET NULL
            );
            INSERT INTO options_new(id, poll_id, label, suggestion_id)
                SELECT o.id, o.poll_id, o.label,
                       CASE WHEN s.id IS NOT NULL THEN o.suggestion_id ELSE NULL END
                FROM options o
                LEFT JOIN suggestions s ON s.id=o.suggestion_id;
            DROP TABLE options;
            ALTER TABLE options_new RENAME TO options;",
        )?;
        // Ricrea gli indici che erano stati salvati.
        for index_sql in &option_indexes {
            tx.execute_batch(index_sql)?;
        }
        tx.commit() // Commit della transazione.
    })();
    // `finally` di Python: riattiva le chiavi esterne, anche se la ricreazione è fallita.
    let restore = connection.pragma_update(None, "foreign_keys", "ON");
    result?;
    restore
}

/// Prima migrazione: crea lo schema iniziale del bot e aggiunge colonne necessarie.
fn migration_1_legacy_schema(connection: &Connection) -> DbResult<()> {
    connection.execute_batch(MIGRATION_1_SQL)?; // Esegue lo script SQL di base.

    // Aggiunge la colonna `thread_id` alla tabella `polls` se non esiste.
    let poll_columns = table_columns(connection, "polls")?;
    if !poll_columns.contains("thread_id") {
        connection.execute_batch("ALTER TABLE polls ADD COLUMN thread_id TEXT")?;
    }
    // Aggiunge la colonna `closes_at` alla tabella `polls` se non esiste.
    if !poll_columns.contains("closes_at") {
        connection.execute_batch("ALTER TABLE polls ADD COLUMN closes_at REAL")?;
    }
    // Aggiunge la colonna `review_status` alla tabella `suggestions` se non esiste.
    let suggestion_columns = table_columns(connection, "suggestions")?;
    if !suggestion_columns.contains("review_status") {
        connection.execute_batch(
            "ALTER TABLE suggestions ADD COLUMN review_status TEXT NOT NULL DEFAULT 'approved'",
        )?;
    }

    // Ricostruisce la tabella `options` se la chiave esterna `suggestion_id` non è corretta.
    if !options_suggestion_fk_is_current(connection)? {
        rebuild_options_table(connection)?;
    }

    // Aggiorna la tabella `picked_suggestions` rinominando `suggestion_id` in `original_suggestion_id`.
    let picked_columns = table_columns(connection, "picked_suggestions")?;
    if picked_columns.contains("suggestion_id") && !picked_columns.contains("original_suggestion_id") {
        let tx = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "CREATE TABLE picked_suggestions_new (
                original_suggestion_id INTEGER PRIMARY KEY,
                week INTEGER NOT NULL,
                norm_name TEXT NOT NULL,
                name TEXT NOT NULL,
                tipo TEXT NOT NULL,
                link TEXT,
                picked_by INTEGER,
                picked_at TEXT
            );
            INSERT INTO picked_suggestions_new
                (original_suggestion_id, week, norm_name, name, tipo, link, picked_by, picked_at)
                SELECT suggestion_id, week, norm_name, name, tipo, link, picked_by, picked_at
                FROM picked_suggestions;
            DROP TABLE picked_suggestions;
            ALTER TABLE picked_suggestions_new RENAME TO picked_suggestions;",
        )?;
        tx.commit()?;
    }

    // Crea o aggiorna gli indici per migliorare le performance delle query.
    connection.execute_batch(
        "DROP INDEX IF EXISTS picked_suggestions_name_idx;
        CREATE INDEX picked_suggestions_name_idx ON picked_suggestions(norm_name, tipo, week);
        CREATE INDEX IF NOT EXISTS suggestions_week_idx ON suggestions(week);
        CREATE UNIQUE INDEX IF NOT EXISTS suggestions_week_name_type_idx
            ON suggestions(week, norm_name, tipo);
        CREATE INDEX IF NOT EXISTS proposals_user_week_idx ON proposals(user_id, week);
        CREATE INDEX IF NOT EXISTS votes_user_idx ON votes(user_id);
        CREATE INDEX IF NOT EXISTS polls_community_active_idx ON polls(is_community, active);",
    )
}

/// Seconda migrazione: crea tabelle aggiuntive per nuove funzionalità.
fn migration_2_feature_tables(connection: &Connection) -> DbResult<()> {
    connection.execute_batch(MIGRATION_2_SQL)
}

/// Operazioni di manutenzione eseguite all'avvio del bot.
/// Include la pulizia dei claim di messaggi scaduti e il backfill di proposte 'picked'.
fn startup_maintenance(connection: &Connection) -> DbResult<()> {
    // Controlla se esistono più sondaggi community attivi, il che dovrebbe essere un errore di configurazione.
    let duplicate_community_polls = connection
        .query_row(
            "SELECT 1 FROM polls \
             WHERE is_community=1 AND active=1 \
             GROUP BY is_community, active \
             HAVING COUNT(*) > 1",
            [],
            |_| Ok(()),
        )
        .optional()? // Tenta di ottenere il risultato, restituisce None se non ci sono righe.
        .is_some(); // Verifica se un duplicato è stato trovato.
    if duplicate_community_polls {
        warn!("Sono presenti più sondaggi community attivi; rimuovi i duplicati prima di crearne un altro.");
    } else {
        // Crea un indice unico per garantire che ci sia un solo sondaggio community attivo.
        connection.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS one_active_community_poll_idx \
             ON polls(is_community) WHERE is_community=1 AND active=1",
        )?;
    }
    // Calcola il timestamp Unix corrente.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0);
    // Elimina i "claim" sui messaggi dei sondaggi scaduti o non più associati a un sondaggio esistente.
    connection.execute(
        "DELETE FROM poll_message_claims WHERE expires_at<=? \
         OR poll_id NOT IN (SELECT id FROM polls)",
        [now],
    )?;
    // Assicura che tutte le proposte con stato 'picked' (selezionate) siano presenti in `picked_suggestions`.
    connection.execute_batch(
        "INSERT OR IGNORE INTO picked_suggestions \
         (original_suggestion_id, week, norm_name, name, tipo, link, picked_by, picked_at) \
         SELECT id, week, norm_name, name, tipo, link, picked_by, picked_at \
         FROM suggestions WHERE status='picked'",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Funzione helper per contare righe o valori numerici in una query di test.
    fn count(db: &Database, sql: &str) -> i64 {
        db.with_conn(|c| c.query_row(sql, [], |row| row.get(0)).unwrap())
    }

    /// Funzione helper per ottenere i nomi di tutte le tabelle nel database di test.
    fn table_names(db: &Database) -> HashSet<String> {
        db.with_conn(|c| {
            c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<DbResult<_>>()
                .unwrap()
        })
    }

    #[test]
    /// Verifica che un database appena creato abbia lo schema completo dopo le migrazioni.
    fn fresh_database_gets_the_full_schema() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(count(&db, "PRAGMA user_version"), 2, "La versione del DB dovrebbe essere 2 dopo le migrazioni.");
        let tables = table_names(&db);
        // Elenco delle tabelle che ci aspettiamo di trovare.
        for expected in [
            "settings", "polls", "options", "votes", "suggestions", "picked_suggestions", "history",
            "web_sessions", "live_events", "staff_audit", "suggestion_review_messages", "daily_vote_activity",
        ] {
            assert!(tables.contains(expected), "Tabella mancante nel database: {expected}");
        }
        assert_eq!(tables.len(), 31, "Il numero totale di tabelle dovrebbe corrispondere a quelle create dalle migrazioni.");
    }

    #[test]
    /// Testa che le migrazioni siano idempotenti (possono essere eseguite più volte senza problemi).
    /// E che il database possa essere riaperto mantenendo lo stato.
    fn migrate_is_idempotent_and_survives_reopening() {
        let dir = std::env::temp_dir().join(format!("sfo-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reopen.db");
        let path = path.to_str().unwrap();
        {
            let db = Database::open(path).unwrap();
            db.set_setting("week", 7).unwrap();
        }
        let db = Database::open(path).unwrap();
        migrate(&db.lock()).unwrap();
        assert_eq!(count(&db, "PRAGMA user_version"), 2, "La user_version dovrebbe rimanere 2.");
        assert_eq!(db.current_week().unwrap(), 7, "L'impostazione 'week' dovrebbe persistere.");
        drop(db);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    /// Testa il ciclo di vita delle impostazioni: lettura, scrittura, valori di default.
    fn settings_round_trip_with_defaults_and_upsert() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.get_setting("nope").unwrap(), None, "Un'impostazione inesistente dovrebbe restituire None.");
        assert_eq!(db.get_setting_or("nope", "x").unwrap(), "x", "Il default dovrebbe essere usato se l'impostazione manca.");
        assert_eq!(db.current_week().unwrap(), 1, "La settimana iniziale dovrebbe essere 1.");
        db.set_setting("k", "uno").unwrap();
        db.set_setting("k", 2).unwrap();
        assert_eq!(db.get_setting("k").unwrap().as_deref(), Some("2"), "L'impostazione dovrebbe essere aggiornata.");
        db.set_setting("week", "abc").unwrap();
        assert!(db.current_week().is_err(), "La conversione di 'abc' in numero di settimana dovrebbe fallire.");
    }

    #[test]
    /// Verifica che la funzione `seed_defaults` popoli il database solo se è vuoto.
    fn seed_defaults_populates_once() {
        let db = Database::open_in_memory().unwrap();
        db.seed_defaults().unwrap();
        db.seed_defaults().unwrap(); // Eseguita una seconda volta, non dovrebbe fare nulla.
        assert_eq!(count(&db, "SELECT COUNT(*) FROM polls"), 5, "Dovrebbero esserci 5 sondaggi iniziali.");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM options"), 16, "Dovrebbero esserci 16 opzioni totali.");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM polls WHERE is_community=1"), 1, "Dovrebbe esserci un solo sondaggio community.");
    }

    #[test]
    /// Verifica che le transazioni facciano il commit su successo e il rollback su errore.
    fn transaction_commits_on_ok_and_rolls_back_on_err() {
        let db = Database::open_in_memory().unwrap();
        // Transazione di successo: dovrebbe salvare 'a'='1'.
        db.transaction(|tx| -> DbResult<()> {
            tx.execute("INSERT INTO settings(key,value) VALUES('a','1')", [])?;
            Ok(())
        })
        .unwrap();
        // Transazione fallita: 'b'='2' non dovrebbe essere salvato.
        let failed = db.transaction(|tx| -> DbResult<()> {
            tx.execute("INSERT INTO settings(key,value) VALUES('b','2')", [])?;
            Err(rusqlite::Error::QueryReturnedNoRows)
        });
        assert!(failed.is_err(), "La transazione dovrebbe fallire.");
        assert_eq!(db.get_setting("a").unwrap().as_deref(), Some("1"), "Il valore 'a' dovrebbe essere stato committato.");
        assert_eq!(db.get_setting("b").unwrap(), None, "Il valore 'b' non dovrebbe essere stato committato a causa del rollback.");
    }

    #[test]
    /// Testa l'applicazione delle chiavi esterne (foreign keys) e il comportamento `ON DELETE CASCADE`/`SET NULL`.
    fn foreign_keys_are_enforced() {
        let db = Database::open_in_memory().unwrap();
        db.seed_defaults().unwrap();
        db.execute("INSERT INTO votes(option_id,user_id) VALUES(1,42)", []).unwrap();
        assert!(db.execute("INSERT INTO votes(option_id,user_id) VALUES(9999,42)", []).is_err(), "Inserire un voto per un'option_id inesistente dovrebbe fallire.");
        db.execute("DELETE FROM options WHERE id=1", []).unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM votes"), 0, "`ON DELETE CASCADE`: i voti associati all'opzione eliminata dovrebbero essere spariti.");
    }

    #[test]
    /// Verifica le operazioni di manutenzione all'avvio: pulizia dei claim e backfill delle proposte.
    fn startup_maintenance_cleans_claims_backfills_picks_and_guards_duplicates() {
        let db = Database::open_in_memory().unwrap();
        db.execute("INSERT INTO polls(id,name,is_community) VALUES(1,'a',1)", []).unwrap();
        db.execute("INSERT INTO poll_message_claims(poll_id,expires_at) VALUES(1, 0)", []).unwrap();
        db.execute("INSERT INTO poll_message_claims(poll_id,expires_at) VALUES(1000, 9999999999)", []).unwrap();
        db.execute(
            "INSERT INTO suggestions(id,week,norm_name,name,tipo,status) VALUES(5,1,'among us','Among Us','Gioco','picked')",
            [],
        )
        .unwrap();
        db.startup_maintenance().unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM poll_message_claims"), 0, "I claim scaduti o orfani dovrebbero essere stati puliti.");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM picked_suggestions WHERE original_suggestion_id=5"), 1, "La proposta 'picked' dovrebbe essere stata backfillata.");
        // L'indice univoco impedisce un secondo sondaggio community attivo.
        assert!(db.execute("INSERT INTO polls(id,name,is_community) VALUES(2,'b',1)", []).is_err(), "Non dovrebbero essere permessi sondaggi community duplicati attivi.");
    }

    #[test]
    /// Verifica che la presenza di sondaggi community duplicati attivi generi solo un warning, non un errore bloccante.
    fn duplicate_active_community_polls_only_warn() {
        let db = Database::open_in_memory().unwrap();
        db.execute("INSERT INTO polls(name,is_community) VALUES('a',1)", []).unwrap();
        db.execute("INSERT INTO polls(name,is_community) VALUES('b',1)", []).unwrap();
        db.startup_maintenance().unwrap();
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM sqlite_master WHERE name='one_active_community_poll_idx'"),
            0, "L'indice unico per i sondaggi community non dovrebbe essere creato in caso di duplicati."
        );
    }

    #[test]
    /// Testa l'aggiornamento di un database legacy a una versione più recente.
    fn legacy_database_is_upgraded_in_place() {
        // Crea uno schema di database molto vecchio per simulare l'aggiornamento.
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE polls (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE NOT NULL,
                    emoji TEXT DEFAULT '🎮', color TEXT DEFAULT '#5865F2', position INTEGER DEFAULT 0,
                    active INTEGER DEFAULT 1, is_community INTEGER DEFAULT 0, message_id TEXT);
                 CREATE TABLE options (id INTEGER PRIMARY KEY AUTOINCREMENT, poll_id INTEGER NOT NULL
                    REFERENCES polls(id) ON DELETE CASCADE, label TEXT NOT NULL, suggestion_id INTEGER);
                 CREATE TABLE suggestions (id INTEGER PRIMARY KEY AUTOINCREMENT, week INTEGER NOT NULL,
                    norm_name TEXT NOT NULL, name TEXT NOT NULL, tipo TEXT NOT NULL, link TEXT,
                    status TEXT DEFAULT 'pending', picked_by INTEGER, picked_at TEXT);
                 CREATE TABLE picked_suggestions (suggestion_id INTEGER PRIMARY KEY, week INTEGER NOT NULL,
                    norm_name TEXT NOT NULL, name TEXT NOT NULL, tipo TEXT NOT NULL, link TEXT,
                    picked_by INTEGER, picked_at TEXT);
                 INSERT INTO polls(id,name) VALUES(1,'p');
                 INSERT INTO suggestions(id,week,norm_name,name,tipo) VALUES(10,1,'x','X','Gioco');
                 INSERT INTO options(poll_id,label,suggestion_id) VALUES(1,'valida',10);
                 INSERT INTO options(poll_id,label,suggestion_id) VALUES(1,'orfana',999);
                 INSERT INTO picked_suggestions VALUES(10,1,'x','X','Gioco',NULL,5,'2024-01-01');",
            )
            .unwrap();
        connection.pragma_update(None, "foreign_keys", "ON").unwrap();

        migrate(&connection).unwrap(); // Applica le migrazioni al database legacy.

        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 2, "La versione del DB dovrebbe essere aggiornata a 2.");
        assert!(options_suggestion_fk_is_current(&connection).unwrap(), "La chiave esterna per le opzioni dovrebbe essere stata corretta.");
        let foreign_keys: i64 = connection.pragma_query_value(None, "foreign_keys", |r| r.get(0)).unwrap();
        assert_eq!(foreign_keys, 1, "Le chiavi esterne dovrebbero tornare attive.");
        let labels: Vec<(String, Option<i64>)> = connection
            .prepare("SELECT label, suggestion_id FROM options ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<DbResult<_>>()
            .unwrap();
        assert_eq!(labels, vec![("valida".into(), Some(10)), ("orfana".into(), None)], "Gli suggestion_id orfani dovrebbero essere NULL.");
        let picked: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM picked_suggestions WHERE original_suggestion_id=10 AND picked_by=5",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(picked, 1, "La proposta 'picked' dovrebbe essere stata migrata correttamente.");
        assert!(table_columns(&connection, "polls").unwrap().contains("closes_at"), "La colonna 'closes_at' dovrebbe essere stata aggiunta alla tabella polls.");
        assert!(table_columns(&connection, "suggestions").unwrap().contains("review_status"), "La colonna 'review_status' dovrebbe essere stata aggiunta alla tabella suggestions.");
    }

    /// Esporta il database creato da Rust per il confronto di schema con Python.
    /// Questo test è ignorato per impostazione predefinita e viene eseguito solo manualmente
    /// per verificare la parità dello schema con la versione Python.
    #[test]
    #[ignore]
    fn dump_schema_for_python_parity() {
        let path = std::env::var("SFO_DUMP_DB").expect("imposta SFO_DUMP_DB");
        let _ = std::fs::remove_file(&path);
        Database::open(&path).unwrap();
    }
}
