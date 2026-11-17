//! Ripristina un backup SQLite dopo aver arrestato Sword Bot Online
//!
//! ```text
//! restore_backup <BACKUP> [--database <DB>] [--preview]
//! ```
//!
//! `BACKUP` è un `.db` locale oppure un `.enc` cifrato con Fernet (richiede
//! `BACKUP_ENCRYPTION_KEY`). Prima di sostituire il database ne salva una copia
//! `pre-restore-<data>.db` in `BACKUP_DIR` (default: `backups/` accanto al database).

use std::collections::BTreeSet;
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{CommandFactory, Parser};
use fernet::Fernet;
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Serialize, Serializer};
use tempfile::{Builder, TempPath};

const REQUIRED_TABLES: [&str; 3] = ["settings", "polls", "history"];
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

// --- errori ----------------------------------------------------------------------------------

/// Equivalente di `(OSError, sqlite3.Error, ValueError)` del `main` Python.
#[derive(Debug)]
enum RestoreError {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    Value(String),
}

type Result<T> = std::result::Result<T, RestoreError>;

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreError::Io(error) => write!(f, "{error}"),
            RestoreError::Sqlite(error) => write!(f, "{error}"),
            RestoreError::Value(message) => f.write_str(message),
        }
    }
}

impl From<io::Error> for RestoreError {
    fn from(error: io::Error) -> Self {
        RestoreError::Io(error)
    }
}

impl From<rusqlite::Error> for RestoreError {
    fn from(error: rusqlite::Error) -> Self {
        RestoreError::Sqlite(error)
    }
}

fn value_error(message: impl Into<String>) -> RestoreError {
    RestoreError::Value(message.into())
}

// --- configurazione ---------------------------------------------------------------------------

/// Variabili d'ambiente lette una volta sola, così i test non toccano l'ambiente del processo.
#[derive(Debug, Clone, Default)]
struct Settings {
    /// `BACKUP_ENCRYPTION_KEY`: chiave Fernet per i file `.enc`.
    encryption_key: Option<String>,
    /// `BACKUP_DIR`: dove salvare la copia `pre-restore-*.db`.
    backup_dir: Option<PathBuf>,
}

impl Settings {
    fn from_env() -> Self {
        Self {
            encryption_key: env::var("BACKUP_ENCRYPTION_KEY").ok(),
            backup_dir: env::var_os("BACKUP_DIR").map(PathBuf::from),
        }
    }
}

// --- percorsi ---------------------------------------------------------------------------------

/// `Path.expanduser()`: sostituisce un `~` iniziale con la cartella dell'utente.
fn expand_user(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    match env::var_os("HOME").or_else(|| env::var_os("USERPROFILE")) {
        Some(home) => PathBuf::from(home).join(rest),
        None => path.to_path_buf(),
    }
}

/// Su Windows `canonicalize` restituisce percorsi `\\?\C:\...`: meglio quelli normali.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

/// `Path.resolve(strict=True)`: assoluto, senza `..`/link, e deve esistere.
fn resolve_existing(path: &Path) -> Result<PathBuf> {
    Ok(strip_verbatim_prefix(fs::canonicalize(expand_user(path))?))
}

/// `Path.resolve()`: come sopra, ma il file può non esistere ancora.
fn resolve(path: &Path) -> Result<PathBuf> {
    let path = expand_user(path);
    match fs::canonicalize(&path) {
        Ok(resolved) => Ok(strip_verbatim_prefix(resolved)),
        Err(_) => Ok(std::path::absolute(&path)?),
    }
}

/// File accessori che SQLite crea accanto a un database (anche aprendolo in sola lettura).
const SQLITE_SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Su Windows antivirus e indicizzatore possono tenere aperto per un attimo un file appena
/// scritto: errori 5 (accesso negato) e 32 (file in uso) sono transitori, si riprova.
fn is_transient_windows_error(error: &io::Error) -> bool {
    cfg!(windows) && matches!(error.raw_os_error(), Some(5 | 32))
}

fn retry_transient<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempts_left = 10;
    loop {
        match operation() {
            Err(error) if attempts_left > 0 && is_transient_windows_error(&error) => {
                attempts_left -= 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            other => return other,
        }
    }
}

/// Elimina `-wal`, `-shm` e `-journal` di `database` (se non esistono va bene).
fn remove_sqlite_sidecars(database: &Path) -> io::Result<()> {
    for suffix in SQLITE_SIDECARS {
        let mut sidecar = database.as_os_str().to_owned();
        sidecar.push(suffix);
        match retry_transient(|| fs::remove_file(&sidecar)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

/// File temporaneo di un database: alla fine sparisce con i suoi file accessori.
///
/// Aprire un database WAL anche solo in lettura lascia `-wal`/`-shm` accanto al file:
/// il semplice `TempPath` cancellerebbe il `.db` e lascerebbe quelli.
struct TempDb(Option<TempPath>);

impl TempDb {
    fn path(&self) -> &Path {
        self.0.as_ref().expect("il file temporaneo c'è finché non viene spostato")
    }

    /// Sposta il file su `destination`, sostituendolo se esiste (rename atomico).
    fn persist(mut self, destination: &Path) -> Result<()> {
        remove_sqlite_sidecars(self.path())?;
        let mut staged = self.0.take().expect("il file temporaneo c'è finché non viene spostato");
        let mut attempts_left = 10;
        loop {
            match staged.persist(destination) {
                Ok(()) => return Ok(()),
                Err(failure) if attempts_left > 0 && is_transient_windows_error(&failure.error) => {
                    attempts_left -= 1;
                    staged = failure.path;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(failure) => return Err(RestoreError::Io(failure.error)),
            }
        }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = remove_sqlite_sidecars(path);
        }
    }
}

fn file_name_ends_with(path: &Path, suffix: &str) -> bool {
    path.file_name().is_some_and(|name| name.to_string_lossy().ends_with(suffix))
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(()) // Windows non ha i permessi POSIX: i file temporanei sono già privati dell'utente.
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    restrict_to_owner(path, 0o700)
}

// --- SQLite -----------------------------------------------------------------------------------

fn open_read_only(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    Ok(connection)
}

fn open_read_write(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    Ok(connection)
}

/// Risultato di `PRAGMA integrity_check` (`"ok"` se il database è integro).
fn integrity_check(connection: &Connection) -> Result<Option<String>> {
    Ok(connection.query_row("PRAGMA integrity_check", [], |row| row.get(0)).optional()?)
}

fn table_names(connection: &Connection) -> Result<BTreeSet<String>> {
    let mut statement = connection.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
    let names = statement.query_map([], |row| row.get(0))?;
    Ok(names.collect::<rusqlite::Result<_>>()?)
}

fn validate_database(path: &Path) -> Result<()> {
    let connection = open_read_only(path)?;
    let result = integrity_check(&connection)?;
    if result.as_deref() != Some("ok") {
        return Err(RestoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            Some(format!(
                "Controllo integrità fallito: {}",
                result.as_deref().unwrap_or("nessun risultato")
            )),
        )));
    }
    let tables = table_names(&connection)?;
    let missing: Vec<&str> = REQUIRED_TABLES.into_iter().filter(|table| !tables.contains(*table)).collect();
    if !missing.is_empty() {
        return Err(value_error(format!(
            "Backup non compatibile, tabelle mancanti: {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

/// Copia coerente di `source` in `destination` con l'API di backup di SQLite.
fn copy_database(source_path: &Path, destination_path: &Path) -> Result<()> {
    let source = open_read_only(source_path)?;
    let mut destination = open_read_write(destination_path)?;
    {
        let backup = Backup::new(&source, &mut destination)?;
        backup.run_to_completion(256, Duration::ZERO, None)?;
    }
    if integrity_check(&destination)?.as_deref() != Some("ok") {
        return Err(value_error("La copia non ha superato integrity_check."));
    }
    Ok(())
}

/// Le sessioni web del backup non vanno mai riattivate: chi era collegato deve rifare il login.
fn clear_restored_sessions(path: &Path) -> Result<()> {
    let connection = open_read_write(path)?;
    let has_sessions: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='web_sessions'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if has_sessions.is_some() {
        connection.execute("DELETE FROM web_sessions", [])?;
    }
    if integrity_check(&connection)?.as_deref() != Some("ok") {
        return Err(value_error("Il database ripristinato non è integro."));
    }
    Ok(())
}

/// Conteggio righe per tabella, nell'ordine di creazione (come `sqlite_master`).
fn table_row_counts(path: &Path) -> Result<TableCounts> {
    let connection = open_read_only(path)?;
    let tables: Vec<String> = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut counts = Vec::with_capacity(tables.len());
    for table in tables {
        let quoted = table.replace('"', "\"\"");
        let count: i64 = connection.query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |row| row.get(0))?;
        counts.push((table, count));
    }
    Ok(TableCounts(counts))
}

/// Mappa `tabella -> righe` che mantiene l'ordine d'inserimento nel JSON (come il `dict` Python).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TableCounts(Vec<(String, i64)>);

impl Serialize for TableCounts {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().map(|(table, count)| (table, count)))
    }
}

// --- cifratura --------------------------------------------------------------------------------

fn decrypt_backup(backup_path: &Path, key: Option<&str>) -> Result<Vec<u8>> {
    let key = key.unwrap_or("");
    if key.is_empty() {
        return Err(value_error("BACKUP_ENCRYPTION_KEY è necessaria per leggere un file .enc"));
    }
    let wrong_key_or_file = || value_error("Chiave di cifratura errata o backup .enc non valido");
    let token = fs::read(backup_path)?;
    if !key.is_ascii() {
        return Err(wrong_key_or_file());
    }
    let fernet = Fernet::new(key.trim()).ok_or_else(wrong_key_or_file)?;
    let token = std::str::from_utf8(&token).map_err(|_| wrong_key_or_file())?;
    fernet.decrypt(token.trim()).map_err(|_| wrong_key_or_file())
}

/// Decifra `backup_path` in un file temporaneo `<prefisso>*.db` accanto al backup.
/// Il file sparisce da solo quando il `TempDb` viene rilasciato, anche in caso di errore.
fn decrypt_to_temp_file(backup_path: &Path, prefix: &str, key: Option<&str>) -> Result<TempDb> {
    let plain = decrypt_backup(backup_path, key)?;
    let directory = backup_path.parent().unwrap_or(Path::new("."));
    let mut file = Builder::new().prefix(prefix).suffix(".db").tempfile_in(directory)?;
    io::Write::write_all(&mut file, &plain)?;
    restrict_to_owner(file.path(), 0o600)?;
    // `into_temp_path` chiude il file: su Windows SQLite deve poterlo riaprire.
    Ok(TempDb(Some(file.into_temp_path())))
}

// --- anteprima e ripristino -------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct PreviewReport {
    backup: String,
    encrypted: bool,
    backup_size_bytes: u64,
    database: String,
    database_exists: bool,
    backup_tables: TableCounts,
    current_tables: TableCounts,
}

fn preview_restore(backup_path: &Path, database_path: &Path, settings: &Settings) -> Result<PreviewReport> {
    let backup_path = resolve_existing(backup_path)?;
    let database_path = resolve(database_path)?;
    if backup_path == database_path {
        return Err(value_error("Il backup e il database di destinazione coincidono."));
    }

    let encrypted = file_name_ends_with(&backup_path, ".enc");
    let temporary = if encrypted {
        Some(decrypt_to_temp_file(&backup_path, ".sword-preview-", settings.encryption_key.as_deref())?)
    } else {
        None
    };
    let inspect_path: &Path = temporary.as_ref().map_or(backup_path.as_path(), TempDb::path);

    validate_database(inspect_path)?;
    let backup_tables = table_row_counts(inspect_path)?;
    let database_exists = database_path.exists();
    let current_tables = if database_exists { table_row_counts(&database_path)? } else { TableCounts::default() };
    Ok(PreviewReport {
        backup: backup_path.display().to_string(),
        encrypted,
        backup_size_bytes: fs::metadata(&backup_path)?.len(),
        database: database_path.display().to_string(),
        database_exists,
        backup_tables,
        current_tables,
    })
    // `temporary` viene rilasciato qui: il file decifrato è cancellato.
}

/// Ripristina un backup non cifrato. Restituisce il percorso della copia `pre-restore-*.db`
/// (che esiste solo se il database di destinazione c'era già).
fn restore_plain_backup(backup_path: &Path, database_path: &Path, settings: &Settings) -> Result<PathBuf> {
    let backup_path = resolve_existing(backup_path)?;
    let database_path = resolve(database_path)?;
    if backup_path == database_path {
        return Err(value_error("Il backup e il database di destinazione coincidono."));
    }
    validate_database(&backup_path)?;
    let database_dir = database_path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(database_dir)?;

    let backup_root = match &settings.backup_dir {
        Some(directory) => expand_user(directory),
        None => database_dir.join("backups"),
    };
    create_private_dir(&backup_root)?;
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let pre_restore = backup_root.join(format!("pre-restore-{timestamp}.db"));
    if database_path.exists() {
        copy_database(&database_path, &pre_restore)?;
        restrict_to_owner(&pre_restore, 0o600)?;
    }

    // Il database nuovo si prepara in un file accanto alla destinazione e si sposta solo alla fine:
    // in caso di errore il database attuale resta intatto e il file temporaneo viene cancellato.
    let staged = TempDb(Some(
        Builder::new().prefix(".sword-restore-").suffix(".db").tempfile_in(database_dir)?.into_temp_path(),
    ));
    copy_database(&backup_path, staged.path())?;
    clear_restored_sessions(staged.path())?;
    validate_database(staged.path())?;
    restrict_to_owner(staged.path(), 0o600)?;
    // Accessori del database che stiamo per sostituire (WAL, e un eventuale journal rimasto da
    // un arresto brusco): applicati al file nuovo lo danneggerebbero. Il database è chiuso: il bot
    // va fermato prima.
    remove_sqlite_sidecars(&database_path)?;
    staged.persist(&database_path)?;
    Ok(pre_restore)
}

fn restore(backup_path: &Path, database_path: &Path, settings: &Settings) -> Result<PathBuf> {
    let backup_path = resolve_existing(backup_path)?;
    if !file_name_ends_with(&backup_path, ".enc") {
        return restore_plain_backup(&backup_path, database_path, settings);
    }
    let decrypted = decrypt_to_temp_file(&backup_path, ".sword-decrypted-", settings.encryption_key.as_deref())?;
    restore_plain_backup(decrypted.path(), database_path, settings)
    // `decrypted` viene rilasciato qui: il file decifrato è cancellato.
}

// --- riga di comando --------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "restore_backup", about = "Ripristina un backup SQLite dopo aver arrestato Sword Bot Online.")]
struct Args {
    /// Backup .db locale o .enc cifrato da R2
    backup: PathBuf,
    /// Database di destinazione (default: DB_PATH o sword_bot.db)
    #[arg(long, default_value_os_t = default_database())]
    database: PathBuf,
    /// Verifica il backup e mostra i conteggi senza modificare il database
    #[arg(long)]
    preview: bool,
}

fn default_database() -> PathBuf {
    env::var_os("DB_PATH").map_or_else(|| PathBuf::from("sword_bot.db"), PathBuf::from)
}

/// Come `parser.error(...)` di argparse: messaggio su stderr e codice di uscita 2.
fn fail(message: String) -> ! {
    Args::command().error(clap::error::ErrorKind::Io, message).exit()
}

fn main() -> ExitCode {
    let args = Args::parse();
    let settings = Settings::from_env();
    if args.preview {
        match preview_restore(&args.backup, &args.database, &settings) {
            Ok(report) => {
                println!("{}", serde_json::to_string_pretty(&report).expect("il report è serializzabile"));
                ExitCode::SUCCESS
            }
            Err(error) => fail(format!("Anteprima non riuscita: {error}")),
        }
    } else {
        match restore(&args.backup, &args.database, &settings) {
            Ok(pre_restore) => {
                let database = resolve(&args.database).unwrap_or_else(|_| args.database.clone());
                println!("Ripristino completato in {}.", database.display());
                if pre_restore.exists() {
                    println!("Backup pre-ripristino conservato in {}.", pre_restore.display());
                }
                ExitCode::SUCCESS
            }
            Err(error) => fail(format!("Ripristino non riuscito: {error}")),
        }
    }
}

// --- test -------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::TempDir;

    const MINIMAL_SCHEMA: &str = "CREATE TABLE settings(key TEXT, value TEXT);
         CREATE TABLE polls(id INTEGER, name TEXT);
         CREATE TABLE history(id INTEGER, data TEXT);";

    fn create_db(path: &Path, sql: &str) {
        let connection = Connection::open(path).unwrap();
        connection.execute_batch(sql).unwrap();
    }

    fn week_of(path: &Path) -> String {
        Connection::open(path)
            .unwrap()
            .query_row("SELECT value FROM settings WHERE key='week'", [], |row| row.get(0))
            .unwrap()
    }

    fn settings_with(key: Option<&str>, backup_dir: &Path) -> Settings {
        Settings { encryption_key: key.map(str::to_string), backup_dir: Some(backup_dir.to_path_buf()) }
    }

    fn leftover_temp_files(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".sword-"))
            .collect()
    }

    fn encrypted_copy(source: &Path, destination: &Path, key: &str) {
        let token = Fernet::new(key).unwrap().encrypt(&fs::read(source).unwrap());
        fs::write(destination, token).unwrap();
    }

    fn db_in(dir: &TempDir, name: &str, week: &str) -> PathBuf {
        let path = dir.path().join(name);
        create_db(&path, &format!("{MINIMAL_SCHEMA} INSERT INTO settings VALUES('week','{week}');"));
        path
    }

    #[test]
    fn preview_validates_backup_without_changing_destination() {
        let dir = TempDir::new().unwrap();
        let source = db_in(&dir, "backup.db", "12");
        let target = db_in(&dir, "current.db", "13");
        let original = fs::read(&target).unwrap();

        let report = preview_restore(&source, &target, &Settings::default()).unwrap();

        assert!(report.database_exists);
        assert!(!report.encrypted);
        assert_eq!(report.backup_tables.0[0], ("settings".to_string(), 1));
        assert_eq!(report.current_tables.0[0], ("settings".to_string(), 1));
        assert_eq!(fs::read(&target).unwrap(), original);
        assert!(leftover_temp_files(dir.path()).is_empty());
    }

    #[test]
    fn preview_report_is_json_with_python_key_order() {
        let dir = TempDir::new().unwrap();
        let source = db_in(&dir, "backup.db", "12");
        let report = preview_restore(&source, &dir.path().join("assente.db"), &Settings::default()).unwrap();
        assert!(!report.database_exists);
        let json = serde_json::to_string_pretty(&report).unwrap();
        let order: Vec<usize> = ["backup", "encrypted", "backup_size_bytes", "database\"", "database_exists", "backup_tables", "current_tables"]
            .iter()
            .map(|key| json.find(&format!("\"{}", key.trim_end_matches('"'))).unwrap_or_else(|| panic!("manca {key}")))
            .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "ordine chiavi: {json}");
        assert!(json.contains("\"current_tables\": {}"));
        assert!(json.contains("\"settings\": 1"));
    }

    #[test]
    fn encrypted_backup_is_previewed_and_restored_without_leftovers() {
        let dir = TempDir::new().unwrap();
        let plain = db_in(&dir, "daily-2026-11-09.db", "12");
        let key = Fernet::generate_key();
        let encrypted = dir.path().join("daily-2026-11-09.db.enc");
        encrypted_copy(&plain, &encrypted, &key);
        let backups = dir.path().join("pre");
        let settings = settings_with(Some(&key), &backups);

        let report = preview_restore(&encrypted, &dir.path().join("restored.db"), &settings).unwrap();
        assert!(report.encrypted);
        assert_eq!(report.backup_tables.0[0], ("settings".to_string(), 1));

        let target = dir.path().join("restored.db");
        restore(&encrypted, &target, &settings).unwrap();
        assert_eq!(week_of(&target), "12");
        assert_eq!(integrity_check(&open_read_only(&target).unwrap()).unwrap().as_deref(), Some("ok"));
        assert!(leftover_temp_files(dir.path()).is_empty());
    }

    #[test]
    fn empty_encrypted_database_round_trips() {
        let dir = TempDir::new().unwrap();
        let plain = dir.path().join("daily-empty.db");
        create_db(&plain, MINIMAL_SCHEMA);
        let key = Fernet::generate_key();
        let encrypted = dir.path().join("daily-empty.db.enc");
        encrypted_copy(&plain, &encrypted, &key);
        let target = dir.path().join("restored-empty.db");

        restore(&encrypted, &target, &settings_with(Some(&key), &dir.path().join("pre"))).unwrap();

        let counts = table_row_counts(&target).unwrap();
        assert_eq!(counts.0, [("settings".into(), 0), ("polls".into(), 0), ("history".into(), 0)]);
        assert!(leftover_temp_files(dir.path()).is_empty());
    }

    #[test]
    fn wrong_or_missing_key_is_a_clear_error_and_leaves_nothing_behind() {
        let dir = TempDir::new().unwrap();
        let plain = db_in(&dir, "b.db", "1");
        let encrypted = dir.path().join("b.db.enc");
        encrypted_copy(&plain, &encrypted, &Fernet::generate_key());
        let target = dir.path().join("t.db");

        let wrong = settings_with(Some(&Fernet::generate_key()), &dir.path().join("pre"));
        let error = restore(&encrypted, &target, &wrong).unwrap_err().to_string();
        assert_eq!(error, "Chiave di cifratura errata o backup .enc non valido");

        let missing = restore(&encrypted, &target, &Settings::default()).unwrap_err().to_string();
        assert_eq!(missing, "BACKUP_ENCRYPTION_KEY è necessaria per leggere un file .enc");

        let garbage_key = settings_with(Some("non-una-chiave-fernet"), &dir.path().join("pre"));
        assert!(restore(&encrypted, &target, &garbage_key).is_err());

        fs::write(&encrypted, b"non un token").unwrap();
        let corrupted = settings_with(Some(&Fernet::generate_key()), &dir.path().join("pre"));
        assert!(restore(&encrypted, &target, &corrupted).is_err());

        assert!(!target.exists());
        assert!(leftover_temp_files(dir.path()).is_empty());
    }

    #[test]
    fn restore_replaces_database_keeps_a_pre_restore_copy_and_clears_web_sessions() {
        let dir = TempDir::new().unwrap();
        let backup = dir.path().join("backup.db");
        create_db(
            &backup,
            &format!(
                "{MINIMAL_SCHEMA}
                 CREATE TABLE web_sessions(session_id TEXT, user_id INTEGER);
                 INSERT INTO settings VALUES('week','12');
                 INSERT INTO web_sessions VALUES('abc', 1);"
            ),
        );
        let target = db_in(&dir, "current.db", "13");
        // File accessori di un database WAL precedente: vanno eliminati.
        fs::write(dir.path().join("current.db-wal"), b"x").unwrap();
        fs::write(dir.path().join("current.db-shm"), b"x").unwrap();
        let backups = dir.path().join("safety");

        let pre_restore = restore(&backup, &target, &settings_with(None, &backups)).unwrap();

        assert_eq!(week_of(&target), "12");
        let sessions: i64 = Connection::open(&target).unwrap().query_row("SELECT COUNT(*) FROM web_sessions", [], |r| r.get(0)).unwrap();
        assert_eq!(sessions, 0, "le sessioni web non si ripristinano");
        assert!(pre_restore.starts_with(&backups));
        assert_eq!(week_of(&pre_restore), "13", "copia del database precedente");
        assert!(!dir.path().join("current.db-wal").exists());
        assert!(!dir.path().join("current.db-shm").exists());
        assert!(leftover_temp_files(dir.path()).is_empty());
    }

    #[test]
    fn restore_into_a_missing_database_makes_no_pre_restore_copy() {
        let dir = TempDir::new().unwrap();
        let backup = db_in(&dir, "backup.db", "5");
        let target = dir.path().join("nuova-cartella").join("new.db");

        let pre_restore = restore(&backup, &target, &settings_with(None, &dir.path().join("safety"))).unwrap();

        assert_eq!(week_of(&target), "5");
        assert!(!pre_restore.exists());
    }

    #[test]
    fn wal_mode_backup_is_restored() {
        // I database del bot usano WAL: anche i loro snapshot vanno ripristinati.
        let dir = TempDir::new().unwrap();
        let backup = dir.path().join("wal.db");
        create_db(&backup, &format!("PRAGMA journal_mode=WAL; {MINIMAL_SCHEMA} INSERT INTO settings VALUES('week','9');"));
        let target = dir.path().join("t.db");
        restore(&backup, &target, &settings_with(None, &dir.path().join("safety"))).unwrap();
        assert_eq!(week_of(&target), "9");
        assert!(leftover_temp_files(dir.path()).is_empty(), "{:?}", leftover_temp_files(dir.path()));
    }

    #[test]
    fn wal_mode_encrypted_backup_leaves_no_temporary_files_or_sidecars() {
        // Aprire un database WAL (anche in sola lettura) crea -wal/-shm accanto al file:
        // devono sparire insieme al temporaneo, sia in anteprima sia nel ripristino.
        let dir = TempDir::new().unwrap();
        let plain = dir.path().join("wal.db");
        create_db(&plain, &format!("PRAGMA journal_mode=WAL; {MINIMAL_SCHEMA} INSERT INTO settings VALUES('week','9');"));
        let key = Fernet::generate_key();
        let encrypted = dir.path().join("wal.db.enc");
        encrypted_copy(&plain, &encrypted, &key);
        let target = db_in(&dir, "t.db", "1");
        let settings = settings_with(Some(&key), &dir.path().join("safety"));

        preview_restore(&encrypted, &target, &settings).unwrap();
        assert!(leftover_temp_files(dir.path()).is_empty(), "{:?}", leftover_temp_files(dir.path()));

        restore(&encrypted, &target, &settings).unwrap();
        assert_eq!(week_of(&target), "9");
        assert!(leftover_temp_files(dir.path()).is_empty(), "{:?}", leftover_temp_files(dir.path()));
    }

    #[test]
    fn leftover_journal_of_the_replaced_database_is_removed() {
        // Un `-journal` che SQLite ignora (vuoto) non deve restare accanto al database nuovo.
        // (Un journal "caldo" vero blocca già la copia di sicurezza: va prima recuperato aprendo
        // il database con il bot, come in Python.)
        let dir = TempDir::new().unwrap();
        let backup = db_in(&dir, "b.db", "2");
        let target = db_in(&dir, "t.db", "1");
        fs::write(dir.path().join("t.db-journal"), b"").unwrap();
        restore(&backup, &target, &settings_with(None, &dir.path().join("safety"))).unwrap();
        assert!(!dir.path().join("t.db-journal").exists());
        assert_eq!(week_of(&target), "2");
    }

    #[test]
    fn incompatible_or_corrupt_backups_are_rejected_and_destination_is_untouched() {
        let dir = TempDir::new().unwrap();
        let target = db_in(&dir, "current.db", "13");
        let original = fs::read(&target).unwrap();
        let settings = settings_with(None, &dir.path().join("safety"));

        let incompatible = dir.path().join("incompatibile.db");
        create_db(&incompatible, "CREATE TABLE settings(key TEXT, value TEXT);");
        let error = restore(&incompatible, &target, &settings).unwrap_err().to_string();
        assert_eq!(error, "Backup non compatibile, tabelle mancanti: polls, history");

        let corrupt = dir.path().join("corrotto.db");
        fs::write(&corrupt, vec![7u8; 4096]).unwrap();
        assert!(restore(&corrupt, &target, &settings).is_err());

        let missing = dir.path().join("non-esiste.db");
        assert!(restore(&missing, &target, &settings).is_err());

        assert_eq!(fs::read(&target).unwrap(), original);
        assert!(leftover_temp_files(dir.path()).is_empty());
    }

    #[test]
    fn backup_and_destination_must_differ() {
        let dir = TempDir::new().unwrap();
        let database = db_in(&dir, "same.db", "1");
        let settings = settings_with(None, &dir.path().join("safety"));
        let error = restore(&database, &database, &settings).unwrap_err().to_string();
        assert_eq!(error, "Il backup e il database di destinazione coincidono.");
        let error = preview_restore(&database, &database, &settings).unwrap_err().to_string();
        assert_eq!(error, "Il backup e il database di destinazione coincidono.");
    }

    #[test]
    fn table_names_with_quotes_are_counted_safely() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("q.db");
        create_db(&path, "CREATE TABLE \"strana\"\"tabella\"(x); INSERT INTO \"strana\"\"tabella\" VALUES(1),(2);");
        assert_eq!(table_row_counts(&path).unwrap().0, [("strana\"tabella".to_string(), 2)]);
    }

    #[test]
    fn leftover_file_handles_are_closed() {
        // Sanity check Windows: dopo il ripristino il database si può cancellare (nessun handle aperto).
        let dir = TempDir::new().unwrap();
        let backup = db_in(&dir, "b.db", "1");
        let target = dir.path().join("t.db");
        restore(&backup, &target, &settings_with(None, &dir.path().join("safety"))).unwrap();
        drop(File::open(&target).unwrap());
        fs::remove_file(&target).unwrap();
    }
}
