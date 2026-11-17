//! Backup del database (equivalente della parte logica di `cogs/backups.py`): snapshot SQLite
//! giornalieri e settimanali con controllo d'integrità, rotazione e copia cifrata su un bucket
//! S3-compatibile (Cloudflare R2).
//!
//! Differenze da Python:
//! * lo snapshot parte dalla connessione già aperta del bot (come l'export redatto), non da una
//!   seconda connessione al file;
//! * senza boto3, la richiesta `PutObject` si firma qui (AWS Signature V4, con il corpo firmato) e
//!   si invia con `reqwest`; la firma è verificata contro quella calcolata da botocore;
//! * la cartella predefinita è `backups` nella cartella da cui si avvia il bot.
// Collegato ai comandi `/backup` e al ciclo giornaliero in `main`.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveTime, Utc};
use chrono_tz::Tz;
use fernet::Fernet;
use hmac::{Hmac, KeyInit, Mac};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tracing::{error, info};

use crate::database::Database;
use crate::schedule::{from_wall_clock, now as rome_now};

pub const DAILY_RETENTION: usize = 14;
pub const WEEKLY_RETENTION: usize = 8;
/// L'ora del backup automatico (Europe/Rome).
pub const DAILY_HOUR: u32 = 3;
pub const DAILY_MINUTE: u32 = 15;

/// Errore di un backup locale.
#[derive(Debug)]
pub struct BackupError(pub String);

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BackupError {}

impl From<std::io::Error> for BackupError {
    fn from(error: std::io::Error) -> Self {
        BackupError(error.to_string())
    }
}

impl From<rusqlite::Error> for BackupError {
    fn from(error: rusqlite::Error) -> Self {
        BackupError(error.to_string())
    }
}

// --- snapshot locali -------------------------------------------------------------------------

/// Scrive una copia coerente del database in `destination`: la crea in un file temporaneo
/// accanto, ne verifica l'integrità e solo allora la sposta al posto definitivo.
pub fn write_snapshot(db: &Database, destination: &Path) -> Result<(), BackupError> {
    let directory = destination.parent().ok_or_else(|| BackupError("percorso del backup senza cartella".into()))?;
    std::fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    // Il file temporaneo sparisce da solo se qualcosa va storto prima dello spostamento.
    let temporary = tempfile::Builder::new().prefix(".sword-backup-").suffix(".tmp").tempfile_in(directory)?.into_temp_path();
    {
        let mut snapshot = Connection::open(&temporary)?;
        db.with_conn(|source| -> rusqlite::Result<()> {
            let backup = rusqlite::backup::Backup::new(source, &mut snapshot)?;
            backup.run_to_completion(100, Duration::ZERO, None)
        })?;
        let check: String = snapshot.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if check != "ok" {
            return Err(BackupError(format!("Controllo integrità backup non riuscito: {check}")));
        }
        // Un solo file autosufficiente, senza giornale da portarsi dietro.
        snapshot.pragma_update(None, "journal_mode", "DELETE")?;
        snapshot.close().map_err(|(_, e)| e)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    temporary.persist(destination).map_err(|e| BackupError(e.error.to_string()))?;
    Ok(())
}

/// Tiene solo i `retention` file più recenti (il nome contiene la data) che iniziano per `prefix`.
pub fn prune(directory: &Path, prefix: &str, retention: usize) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(prefix) && name.ends_with(".db"))
        .collect();
    names.sort_by(|a, b| b.cmp(a));
    for name in names.into_iter().skip(retention) {
        if let Err(e) = std::fs::remove_file(directory.join(&name)) {
            error!("Impossibile eliminare il vecchio backup {name}: {e}");
        }
    }
}

/// Nomi dei file `.db` che iniziano per `prefix`, dal più recente.
fn listing(directory: &Path, prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(prefix) && name.ends_with(".db"))
        .collect();
    names.sort_by(|a, b| b.cmp(a));
    names
}

/// Riepilogo dei backup presenti (`backup_status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupStatus {
    pub root: PathBuf,
    pub daily_count: usize,
    pub latest_daily: Option<String>,
    pub weekly_count: usize,
    pub latest_weekly: Option<String>,
    pub cloud_upload_error: Option<String>,
}

// --- copia cifrata sul cloud -----------------------------------------------------------------

/// Parametri del bucket (variabili `BACKUP_S3_*` e `BACKUP_ENCRYPTION_KEY`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudConfig {
    pub bucket: String,
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub encryption_key: String,
    pub region: String,
    pub prefix: String,
}

/// Legge la configurazione cloud: `Ok(None)` se non è stato impostato nulla, un errore (il messaggio
/// mostrato allo staff) se è incompleta o l'indirizzo non è HTTPS.
pub fn cloud_config(env: &dyn Fn(&str) -> Option<String>) -> Result<Option<CloudConfig>, String> {
    const KEYS: [&str; 5] =
        ["BACKUP_S3_BUCKET", "BACKUP_S3_ENDPOINT_URL", "BACKUP_S3_ACCESS_KEY_ID", "BACKUP_S3_SECRET_ACCESS_KEY", "BACKUP_ENCRYPTION_KEY"];
    let values: Vec<String> = KEYS.iter().map(|key| env(key).unwrap_or_default().trim().to_string()).collect();
    if values.iter().all(String::is_empty) {
        return Ok(None);
    }
    let missing: Vec<&str> = KEYS.iter().zip(&values).filter(|(_, value)| value.is_empty()).map(|(key, _)| *key).collect();
    if !missing.is_empty() {
        return Err(format!("Configurazione backup R2 incompleta: {}", missing.join(", ")));
    }
    if !values[1].starts_with("https://") {
        return Err("BACKUP_S3_ENDPOINT_URL deve iniziare con https://".to_string());
    }
    Ok(Some(CloudConfig {
        bucket: values[0].clone(),
        endpoint: values[1].clone(),
        access_key_id: values[2].clone(),
        secret_access_key: values[3].clone(),
        encryption_key: values[4].clone(),
        region: env("BACKUP_S3_REGION").unwrap_or_else(|| "auto".to_string()),
        prefix: env("BACKUP_S3_PREFIX").unwrap_or_else(|| "sfo-backups".to_string()).trim_matches('/').to_string(),
    }))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accetta chiavi di qualsiasi lunghezza");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Codifica percentuale di un percorso: si lasciano `A-Za-z0-9-_.~` e `/`, come botocore per S3.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Una richiesta `PutObject` pronta da inviare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedPut {
    pub url: String,
    /// Intestazioni da inviare, con `Authorization` ed escluso `Host` (lo aggiunge il client).
    pub headers: Vec<(String, String)>,
    pub authorization: String,
}

/// Firma con AWS Signature V4 un `PutObject` in stile percorso (`endpoint/bucket/chiave`).
///
/// Si firmano `content-type`, `host`, `x-amz-content-sha256`, `x-amz-date` e
/// `x-amz-meta-encryption`; il corpo è firmato (l'hash è nell'intestazione).
pub fn sign_put(config: &CloudConfig, key: &str, body: &[u8], now: DateTime<Utc>) -> Result<SignedPut, String> {
    const CONTENT_TYPE: &str = "application/octet-stream";
    const META_ENCRYPTION: &str = "fernet";
    let endpoint = url::Url::parse(&config.endpoint).map_err(|e| format!("indirizzo non valido: {e}"))?;
    let host = match (endpoint.host_str(), endpoint.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        _ => return Err("indirizzo senza host".to_string()),
    };
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let payload_hash = hex(&Sha256::digest(body));
    let path = format!("/{}/{}", encode_path(&config.bucket), encode_path(key));
    let signed_headers = "content-type;host;x-amz-content-sha256;x-amz-date;x-amz-meta-encryption";
    let canonical = format!(
        "PUT\n{path}\n\ncontent-type:{CONTENT_TYPE}\nhost:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\nx-amz-meta-encryption:{META_ENCRYPTION}\n\n{signed_headers}\n{payload_hash}"
    );
    let scope = format!("{date}/{}/s3/aws4_request", config.region);
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&Sha256::digest(canonical.as_bytes())));
    let mut signing_key = hmac_sha256(format!("AWS4{}", config.secret_access_key).as_bytes(), date.as_bytes());
    for part in [config.region.as_str(), "s3", "aws4_request"] {
        signing_key = hmac_sha256(&signing_key, part.as_bytes());
    }
    let signature = hex(&hmac_sha256(&signing_key, to_sign.as_bytes()));
    let authorization = format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", config.access_key_id);
    let scheme = endpoint.scheme();
    Ok(SignedPut {
        url: format!("{scheme}://{host}{path}"),
        headers: vec![
            ("content-type".into(), CONTENT_TYPE.into()),
            ("x-amz-content-sha256".into(), payload_hash),
            ("x-amz-date".into(), amz_date),
            ("x-amz-meta-encryption".into(), META_ENCRYPTION.into()),
            ("authorization".into(), authorization.clone()),
        ],
        authorization,
    })
}

const CLOUD_FAILED: &str = "Caricamento cifrato dei backup su R2 non riuscito";
const BAD_KEY: &str = "SDK S3 mancante o BACKUP_ENCRYPTION_KEY non è una chiave Fernet valida";

/// Cifra con Fernet e carica i file su `<prefisso>/<cartella>/<nome>.enc`. Restituisce le chiavi
/// degli oggetti caricati. Gli errori sono i messaggi mostrati allo staff.
pub async fn upload_encrypted_backups(
    client: &reqwest::Client,
    config: &CloudConfig,
    paths: &[PathBuf],
    now: fn() -> DateTime<Utc>,
) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let cipher = Fernet::new(&config.encryption_key).ok_or_else(|| BAD_KEY.to_string())?;
    let mut uploaded = Vec::new();
    for path in paths {
        let folder = path.parent().and_then(Path::file_name).and_then(|name| name.to_str()).unwrap_or_default();
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        let object_key = format!("{}/{folder}/{name}.enc", config.prefix);
        let content = std::fs::read(path).map_err(|e| format!("lettura del backup {name}: {e}"))?;
        let body = cipher.encrypt(&content).into_bytes();
        let signed = sign_put(config, &object_key, &body, now()).map_err(|e| {
            error!("Firma della richiesta S3 non riuscita: {e}");
            CLOUD_FAILED.to_string()
        })?;
        let mut request = client.put(&signed.url).body(body);
        for (header, value) in &signed.headers {
            request = request.header(header.as_str(), value.as_str());
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => uploaded.push(object_key),
            Ok(response) => {
                error!("Il bucket ha rifiutato {object_key}: {}", response.status());
                return Err(CLOUD_FAILED.to_string());
            }
            Err(e) => {
                error!("Caricamento di {object_key} non riuscito: {e}");
                return Err(CLOUD_FAILED.to_string());
            }
        }
    }
    Ok(uploaded)
}

// --- servizio --------------------------------------------------------------------------------

/// Il prossimo momento del backup automatico (le 03:15 di Roma) dopo `now`.
pub fn next_daily_run(now: DateTime<Tz>) -> DateTime<Tz> {
    let at = NaiveTime::from_hms_opt(DAILY_HOUR, DAILY_MINUTE, 0).expect("ora valida");
    let today = now.date_naive().and_time(at);
    let candidate = from_wall_clock(today);
    if candidate > now { candidate } else { from_wall_clock(today + chrono::Duration::days(1)) }
}

pub struct BackupService {
    db: Database,
    root: PathBuf,
    clock: fn() -> DateTime<Tz>,
    cloud_clock: fn() -> DateTime<Utc>,
    env: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    http: reqwest::Client,
    /// Un solo backup alla volta (`BACKUP_LOCK`).
    lock: tokio::sync::Mutex<()>,
    last_cloud_error: Mutex<Option<String>>,
}

impl BackupService {
    /// Con la cartella di `BACKUP_DIR` (o `./backups`) e le variabili d'ambiente del processo.
    pub fn new(db: Database) -> Self {
        let env: Arc<dyn Fn(&str) -> Option<String> + Send + Sync> = Arc::new(|key| std::env::var(key).ok());
        let root = backup_root(&*env);
        Self::with(db, root, rome_now, Utc::now, env)
    }

    pub fn with(
        db: Database,
        root: PathBuf,
        clock: fn() -> DateTime<Tz>,
        cloud_clock: fn() -> DateTime<Utc>,
        env: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    ) -> Self {
        Self { db, root, clock, cloud_clock, env, http: reqwest::Client::new(), lock: tokio::sync::Mutex::new(()), last_cloud_error: Mutex::new(None) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Esito dell'ultimo caricamento cloud: `None` se riuscito o non configurato.
    pub fn last_cloud_error(&self) -> Option<String> {
        self.last_cloud_error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    fn set_cloud_error(&self, value: Option<String>) {
        *self.last_cloud_error.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
    }

    /// Crea lo snapshot giornaliero (e il lunedì quello settimanale), ruota i vecchi e prova a
    /// caricarli cifrati. Un errore del caricamento non fa fallire il backup: le copie locali sono
    /// valide e l'errore resta in [`Self::last_cloud_error`].
    pub async fn create_backups(&self, now: Option<DateTime<Tz>>) -> Result<Vec<PathBuf>, BackupError> {
        let moment = now.unwrap_or_else(|| (self.clock)());
        let _guard = self.lock.lock().await;
        let daily_dir = self.root.join("daily");
        let daily_file = daily_dir.join(format!("daily-{}.db", moment.date_naive().format("%Y-%m-%d")));
        let mut created = vec![daily_file.clone()];
        let mut jobs = vec![(daily_file, daily_dir, "daily-", DAILY_RETENTION)];
        if moment.weekday() == chrono::Weekday::Mon {
            let week = moment.iso_week();
            let weekly_dir = self.root.join("weekly");
            let weekly_file = weekly_dir.join(format!("weekly-{}-W{:02}.db", week.year(), week.week()));
            created.push(weekly_file.clone());
            jobs.push((weekly_file, weekly_dir, "weekly-", WEEKLY_RETENTION));
        }
        for (file, directory, prefix, retention) in jobs {
            let db = self.db.clone();
            let target = file.clone();
            tokio::task::spawn_blocking(move || write_snapshot(&db, &target)).await.map_err(|e| BackupError(e.to_string()))??;
            prune(&directory, prefix, retention);
        }

        let outcome = match cloud_config(&*self.env) {
            Err(message) => Err(message),
            Ok(None) => Ok(Vec::new()),
            Ok(Some(config)) => upload_encrypted_backups(&self.http, &config, &created, self.cloud_clock).await,
        };
        match outcome {
            Ok(uploaded) => {
                self.set_cloud_error(None);
                if !uploaded.is_empty() {
                    info!("Backup cifrati caricati su R2: {}", uploaded.join(", "));
                }
            }
            Err(message) => {
                error!("Backup locali creati, ma il caricamento su R2 non è riuscito: {message}");
                self.set_cloud_error(Some(message));
            }
        }
        Ok(created)
    }

    pub fn status(&self) -> BackupStatus {
        let daily = listing(&self.root.join("daily"), "daily-");
        let weekly = listing(&self.root.join("weekly"), "weekly-");
        BackupStatus {
            root: self.root.clone(),
            daily_count: daily.len(),
            latest_daily: daily.first().cloned(),
            weekly_count: weekly.len(),
            latest_weekly: weekly.first().cloned(),
            cloud_upload_error: self.last_cloud_error(),
        }
    }

    /// Il backup automatico delle 03:15 (ora di Roma), ogni giorno. Un errore si registra e il
    /// ciclo continua.
    pub fn start_daily_loop(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let now = (service.clock)();
                let wait = (next_daily_run(now) - now).to_std().unwrap_or(Duration::from_secs(60));
                tokio::time::sleep(wait).await;
                match service.create_backups(None).await {
                    Ok(created) => info!("Backup SQLite/R2 completato: {}", created.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")),
                    Err(e) => error!("Backup automatico SQLite/R2 non riuscito: {e}"),
                }
            }
        })
    }
}

/// `BACKUP_DIR` (con `~` espanso) oppure `./backups`.
pub fn backup_root(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    match env("BACKUP_DIR").filter(|value| !value.is_empty()) {
        Some(configured) => {
            if let Some(rest) = configured.strip_prefix('~') {
                if let Some(home) = env("HOME").or_else(|| env("USERPROFILE")) {
                    return PathBuf::from(home).join(rest.trim_start_matches(['/', '\\']));
                }
            }
            PathBuf::from(configured)
        }
        None => std::env::current_dir().unwrap_or_default().join("backups"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use chrono::TimeZone;
    use serde_json::{Value, json};
    use std::collections::HashMap;

    fn golden() -> Value {
        serde_json::from_str(include_str!("golden/backups_expected.json")).unwrap()
    }

    fn fixture_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        // Il golden è stato generato il 2026-11-11: le stagioni del fixture esistono già.
        crate::core::initialize(&db, TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()).unwrap();
        db.execute_batch(include_str!("golden/cards_fixture.sql")).unwrap();
        db
    }

    fn cloud_clock() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2027, 3, 1, 3, 15, 0).unwrap()
    }

    fn fixed_clock() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2027, 3, 8, 3, 15, 0).unwrap()
    }

    type Env = Arc<Mutex<HashMap<String, String>>>;

    fn service(db: Database, root: PathBuf) -> (BackupService, Env) {
        let vars: Env = Arc::default();
        let lookup = Arc::clone(&vars);
        let env: Arc<dyn Fn(&str) -> Option<String> + Send + Sync> = Arc::new(move |key| lookup.lock().unwrap().get(key).cloned());
        (BackupService::with(db, root, fixed_clock, cloud_clock, env), vars)
    }

    fn names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory).map(|d| d.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        names.sort();
        names
    }

    // --- firma ---------------------------------------------------------------------------------

    /// La firma coincide con quella calcolata da botocore per la stessa richiesta (4 casi: chiave
    /// semplice, chiave con spazio/`+`/`=`, indirizzo con porta e regioni diverse).
    #[test]
    fn signatures_match_botocore() {
        for case in golden()["signatures"].as_array().unwrap() {
            let config = CloudConfig {
                bucket: case["bucket"].as_str().unwrap().into(),
                endpoint: case["endpoint"].as_str().unwrap().into(),
                access_key_id: "AKIDEXAMPLE".into(),
                secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
                encryption_key: String::new(),
                region: case["region"].as_str().unwrap().into(),
                prefix: String::new(),
            };
            let length = case["body_len"].as_u64().unwrap() as usize;
            let mut body: Vec<u8> = (0..=255u8).cycle().take(length / 256 * 256).collect();
            body.extend(std::iter::repeat_n(0u8, length % 256));
            let signed = sign_put(&config, case["key"].as_str().unwrap(), &body, cloud_clock()).unwrap();
            let header = |name: &str| signed.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str()).unwrap();
            assert_eq!(signed.authorization, case["authorization"].as_str().unwrap(), "{}", case["key"]);
            assert_eq!(header("x-amz-date"), case["date"].as_str().unwrap());
            assert_eq!(header("x-amz-content-sha256"), case["content_sha256"].as_str().unwrap());
            assert_eq!(signed.url, case["url"].as_str().unwrap());
        }
    }

    // --- snapshot e rotazione ------------------------------------------------------------------

    /// 92 giorni di backup: stessi file giornalieri e settimanali (anche a cavallo dell'anno ISO,
    /// con la settimana 53), stessa rotazione a 14 e 8 file, stesso stato e snapshot valido.
    #[tokio::test]
    async fn rotation_matches_python() {
        let golden = golden();
        let root = tempfile::tempdir().unwrap();
        let (service, _) = service(fixture_db(), root.path().to_path_buf());
        let normalize = |mut status: Value| {
            status["root"] = json!("<ROOT>");
            status
        };
        let as_json = |status: BackupStatus| {
            json!({
                "root": "<ROOT>", "daily_count": status.daily_count, "latest_daily": status.latest_daily,
                "weekly_count": status.weekly_count, "latest_weekly": status.latest_weekly, "cloud_upload_error": status.cloud_upload_error,
            })
        };
        assert_eq!(as_json(service.status()), normalize(golden["status_empty"].clone()));

        let start = NaiveDateWrapper::start();
        let mut runs = Vec::new();
        for (offset, expected) in golden["runs"].as_array().unwrap().iter().enumerate() {
            let day = start + chrono::Duration::days(offset as i64);
            let moment = TZ.from_local_datetime(&day.and_hms_opt(3, 15, 0).unwrap()).earliest().unwrap();
            let created = service.create_backups(Some(moment)).await.unwrap();
            let listed: Vec<String> =
                created.iter().map(|p| format!("{}/{}", p.parent().unwrap().file_name().unwrap().to_string_lossy(), p.file_name().unwrap().to_string_lossy())).collect();
            runs.push(json!([day.format("%Y-%m-%d").to_string(), listed]));
            assert_eq!(runs.last().unwrap(), expected, "giorno {day}");
            if let Some(checkpoint) = golden["checkpoints"].get(day.format("%Y-%m-%d").to_string()) {
                assert_eq!(json!(names(&root.path().join("daily"))), checkpoint["daily"], "giornalieri {day}");
                assert_eq!(json!(names(&root.path().join("weekly"))), checkpoint["weekly"], "settimanali {day}");
                assert_eq!(as_json(service.status()), normalize(checkpoint["status"].clone()), "stato {day}");
            }
        }
        assert_eq!(runs.len(), golden["runs"].as_array().unwrap().len());

        // L'ultimo snapshot è un database valido e completo (stesse tabelle e dati di Python).
        let latest = root.path().join("daily").join(service.status().latest_daily.unwrap());
        let snapshot = Connection::open(&latest).unwrap();
        let integrity: String = snapshot.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(json!(integrity), golden["snapshot"]["integrity"]);
        let tables: Vec<String> = snapshot
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name<>'sqlite_sequence' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(json!(tables), golden["snapshot"]["tables"]);
        let count = |table: &str| snapshot.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!((json!(count("suggestions")), json!(count("settings"))), (golden["snapshot"]["suggestions"].clone(), golden["snapshot"]["settings"].clone()));
        // Nessun file temporaneo lasciato nelle cartelle.
        for folder in ["daily", "weekly"] {
            assert!(names(&root.path().join(folder)).iter().all(|n| !n.starts_with('.')), "{folder}");
        }
        assert_eq!(json!(Vec::<String>::new()), golden["no_temp_files_left"]);
    }

    /// Il giorno dopo la data del nome conta l'ora di Roma, non UTC: alle 00:30 locali del lunedì
    /// si crea già la copia settimanale di quel lunedì.
    #[tokio::test]
    async fn file_names_follow_the_rome_calendar_day() {
        let root = tempfile::tempdir().unwrap();
        let (service, _) = service(fixture_db(), root.path().to_path_buf());
        let created = service.create_backups(Some(TZ.with_ymd_and_hms(2027, 3, 1, 0, 30, 0).unwrap())).await.unwrap();
        let names: Vec<String> = created.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["daily-2027-03-01.db", "weekly-2027-W09.db"]);
    }

    #[tokio::test]
    async fn a_failing_snapshot_is_an_error_and_leaves_no_files() {
        let blocker = tempfile::tempdir().unwrap();
        let file = blocker.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let (service, _) = service(fixture_db(), file.join("sotto"));
        assert!(service.create_backups(None).await.is_err());
        assert_eq!(names(blocker.path()), ["file"]);
    }

    #[test]
    fn pruning_keeps_the_newest_files_by_name() {
        let directory = tempfile::tempdir().unwrap();
        for day in 1..=20 {
            std::fs::write(directory.path().join(format!("daily-2027-03-{day:02}.db")), "x").unwrap();
        }
        std::fs::write(directory.path().join("note.txt"), "x").unwrap();
        prune(directory.path(), "daily-", 14);
        let left = names(directory.path());
        assert_eq!(left.len(), 15);
        assert!(left.contains(&"daily-2027-03-20.db".to_string()) && left.contains(&"daily-2027-03-07.db".to_string()));
        assert!(!left.contains(&"daily-2027-03-06.db".to_string()));
        assert!(left.contains(&"note.txt".to_string()));
    }

    #[test]
    fn the_daily_run_is_at_0315_rome_time() {
        let at = |y, mo, d, h, mi| TZ.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap();
        assert_eq!(next_daily_run(at(2027, 3, 3, 1, 0)), at(2027, 3, 3, 3, 15));
        assert_eq!(next_daily_run(at(2027, 3, 3, 3, 15)), at(2027, 3, 4, 3, 15), "alle 03:15 esatte si aspetta il giorno dopo");
        assert_eq!(next_daily_run(at(2027, 3, 3, 23, 59)), at(2027, 3, 4, 3, 15));
        // Cambio all'ora legale (28 marzo 2027): le 03:15 esistono comunque.
        let next = next_daily_run(at(2027, 3, 27, 12, 0));
        assert_eq!((next.format("%Y-%m-%d %H:%M").to_string(), next.offset().to_string()), ("2027-03-28 03:15".to_string(), "CEST".to_string()));
    }

    #[test]
    fn backup_directory_defaults_to_a_backups_folder_and_expands_the_home() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_string());
        assert!(backup_root(&env(&[])).ends_with("backups"));
        assert_eq!(backup_root(&env(&[("BACKUP_DIR", "/dati/bk")])), PathBuf::from("/dati/bk"));
        assert_eq!(backup_root(&env(&[("BACKUP_DIR", "~/bk"), ("HOME", "/home/u")])), PathBuf::from("/home/u").join("bk"));
        assert!(backup_root(&env(&[("BACKUP_DIR", "")])).ends_with("backups"));
    }

    // --- cloud ---------------------------------------------------------------------------------

    fn cloud_env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
    }

    /// Gli 8 casi di configurazione cloud di Python: stessa configurazione letta (valori ripuliti
    /// dagli spazi), stessi messaggi d'errore e stesso `last_cloud_upload_error` dopo un backup.
    #[tokio::test]
    async fn cloud_configuration_and_failures_match_python() {
        let golden = golden();
        let key = golden["fernet_key"].as_str().unwrap();
        let full = [
            ("BACKUP_S3_BUCKET", "bk"),
            ("BACKUP_S3_ENDPOINT_URL", "https://127.0.0.1:1"),
            ("BACKUP_S3_ACCESS_KEY_ID", "AK"),
            ("BACKUP_S3_SECRET_ACCESS_KEY", "SK"),
            ("BACKUP_ENCRYPTION_KEY", key),
        ];
        let with = |changes: &[(&str, &str)]| {
            let mut map = cloud_env(&full);
            for (k, v) in changes {
                map.insert((*k).to_string(), (*v).to_string());
            }
            map
        };
        let spaced: HashMap<String, String> = full.iter().map(|(k, v)| ((*k).to_string(), format!("  {v}  "))).collect();
        let cases: Vec<(&str, HashMap<String, String>)> = vec![
            ("nessuna", HashMap::new()),
            ("solo_spazi", cloud_env(&[("BACKUP_S3_BUCKET", "  ")])),
            ("parziale", cloud_env(&[("BACKUP_S3_BUCKET", "bk"), ("BACKUP_S3_ACCESS_KEY_ID", "AK")])),
            ("endpoint_http", with(&[("BACKUP_S3_ENDPOINT_URL", "http://127.0.0.1:1")])),
            ("chiave_non_valida", with(&[("BACKUP_ENCRYPTION_KEY", "non-una-chiave")])),
            ("chiave_non_ascii", with(&[("BACKUP_ENCRYPTION_KEY", "chiavè")])),
            ("irraggiungibile", cloud_env(&full)),
            ("valori_con_spazi", spaced),
        ];
        let root = tempfile::tempdir().unwrap();
        let (service, vars) = service(fixture_db(), root.path().to_path_buf());
        for (case, (label, env)) in golden["cloud"].as_array().unwrap().iter().zip(cases) {
            assert_eq!(case["label"], label, "ordine dei casi");
            *vars.lock().unwrap() = env.clone();
            let lookup = |key: &str| env.get(key).cloned();
            match cloud_config(&lookup) {
                Ok(config) => {
                    let actual = config.map(|c| json!({
                        "BACKUP_S3_BUCKET": c.bucket, "BACKUP_S3_ENDPOINT_URL": c.endpoint, "BACKUP_S3_ACCESS_KEY_ID": c.access_key_id,
                        "BACKUP_S3_SECRET_ACCESS_KEY": c.secret_access_key, "BACKUP_ENCRYPTION_KEY": c.encryption_key,
                    }));
                    assert_eq!(actual.unwrap_or(Value::Null), case["config"], "{label}: configurazione");
                }
                Err(message) => assert_eq!(json!(message), case["error"], "{label}: errore"),
            }
            service.create_backups(Some(TZ.with_ymd_and_hms(2027, 3, 22, 3, 15, 0).unwrap())).await.unwrap();
            assert_eq!(service.last_cloud_error().map_or(Value::Null, Value::from), case["last_error"], "{label}: ultimo errore");
        }
    }

    #[test]
    fn prefix_and_region_come_from_the_environment() {
        let env = cloud_env(&[
            ("BACKUP_S3_BUCKET", "b"), ("BACKUP_S3_ENDPOINT_URL", "https://x"), ("BACKUP_S3_ACCESS_KEY_ID", "a"),
            ("BACKUP_S3_SECRET_ACCESS_KEY", "s"), ("BACKUP_ENCRYPTION_KEY", "k"), ("BACKUP_S3_PREFIX", "/mio/prefisso/"), ("BACKUP_S3_REGION", "eu-west-1"),
        ]);
        let config = cloud_config(&|key| env.get(key).cloned()).unwrap().unwrap();
        assert_eq!((config.prefix.as_str(), config.region.as_str()), ("mio/prefisso", "eu-west-1"));
        let defaults = cloud_config(&|key| cloud_env(&[
            ("BACKUP_S3_BUCKET", "b"), ("BACKUP_S3_ENDPOINT_URL", "https://x"), ("BACKUP_S3_ACCESS_KEY_ID", "a"),
            ("BACKUP_S3_SECRET_ACCESS_KEY", "s"), ("BACKUP_ENCRYPTION_KEY", "k"),
        ]).get(key).cloned()).unwrap().unwrap();
        assert_eq!((defaults.prefix.as_str(), defaults.region.as_str()), ("sfo-backups", "auto"));
    }

    /// Un finto bucket: registra ogni `PUT` e risponde con lo stato indicato.
    struct FakeBucket {
        address: std::net::SocketAddr,
        requests: Arc<Mutex<Vec<(String, Vec<(String, String)>, Vec<u8>)>>>,
    }

    async fn fake_bucket(status: axum::http::StatusCode) -> FakeBucket {
        use axum::{Router, body::Bytes, extract::State, http::{HeaderMap, Uri}, routing::put};
        type Log = Arc<Mutex<Vec<(String, Vec<(String, String)>, Vec<u8>)>>>;
        let requests: Log = Arc::default();
        let app = Router::new()
            .fallback(put(move |State(log): State<Log>, uri: Uri, headers: HeaderMap, body: Bytes| async move {
                let headers = headers.iter().map(|(n, v)| (n.to_string(), v.to_str().unwrap_or_default().to_string())).collect();
                log.lock().unwrap().push((uri.path().to_string(), headers, body.to_vec()));
                status
            }))
            .with_state(Arc::clone(&requests));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        FakeBucket { address, requests }
    }

    fn config_for(bucket: &FakeBucket, key: &str) -> CloudConfig {
        CloudConfig {
            bucket: "bk".into(),
            endpoint: format!("http://{}", bucket.address),
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            encryption_key: key.into(),
            region: "auto".into(),
            prefix: "sfo-backups".into(),
        }
    }

    #[tokio::test]
    async fn encrypted_backups_are_uploaded_signed_under_the_expected_keys() {
        let bucket = fake_bucket(axum::http::StatusCode::OK).await;
        let key = Fernet::generate_key();
        let config = config_for(&bucket, &key);
        let directory = tempfile::tempdir().unwrap();
        let daily = directory.path().join("daily");
        let weekly = directory.path().join("weekly");
        std::fs::create_dir_all(&daily).unwrap();
        std::fs::create_dir_all(&weekly).unwrap();
        std::fs::write(daily.join("daily-2027-03-01.db"), b"contenuto giornaliero").unwrap();
        std::fs::write(weekly.join("weekly-2027-W09.db"), b"contenuto settimanale").unwrap();

        let uploaded = upload_encrypted_backups(&reqwest::Client::new(), &config, &[daily.join("daily-2027-03-01.db"), weekly.join("weekly-2027-W09.db")], cloud_clock).await.unwrap();
        assert_eq!(uploaded, ["sfo-backups/daily/daily-2027-03-01.db.enc", "sfo-backups/weekly/weekly-2027-W09.db.enc"]);

        let requests = bucket.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let cipher = Fernet::new(&key).unwrap();
        for ((path, headers, body), (expected_key, plain)) in
            requests.iter().zip([("sfo-backups/daily/daily-2027-03-01.db.enc", &b"contenuto giornaliero"[..]), ("sfo-backups/weekly/weekly-2027-W09.db.enc", b"contenuto settimanale")])
        {
            assert_eq!(path, &format!("/bk/{expected_key}"));
            let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone()).unwrap_or_default();
            assert_eq!(header("content-type"), "application/octet-stream");
            assert_eq!(header("x-amz-meta-encryption"), "fernet");
            assert_eq!(header("x-amz-date"), "20270301T031500Z");
            // La firma è quella attesa per il corpo realmente inviato (che si può decifrare).
            let signed = sign_put(&config, expected_key, body, cloud_clock()).unwrap();
            assert_eq!(header("authorization"), signed.authorization);
            assert_eq!(header("x-amz-content-sha256"), hex(&Sha256::digest(body)));
            assert_eq!(cipher.decrypt(std::str::from_utf8(body).unwrap()).unwrap(), plain);
        }
    }

    #[tokio::test]
    async fn a_rejecting_bucket_or_a_bad_key_are_reported_with_the_python_messages() {
        let key = Fernet::generate_key();
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("daily").join("daily-2027-03-01.db");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"x").unwrap();

        let refusing = fake_bucket(axum::http::StatusCode::FORBIDDEN).await;
        let result = upload_encrypted_backups(&reqwest::Client::new(), &config_for(&refusing, &key), std::slice::from_ref(&file), cloud_clock).await;
        assert_eq!(result, Err("Caricamento cifrato dei backup su R2 non riuscito".to_string()));

        let accepting = fake_bucket(axum::http::StatusCode::OK).await;
        let result = upload_encrypted_backups(&reqwest::Client::new(), &config_for(&accepting, "non-una-chiave"), std::slice::from_ref(&file), cloud_clock).await;
        assert_eq!(result, Err("SDK S3 mancante o BACKUP_ENCRYPTION_KEY non è una chiave Fernet valida".to_string()));
        assert!(accepting.requests.lock().unwrap().is_empty(), "con la chiave sbagliata non parte nulla");

        // Nessun file: niente da caricare, nemmeno la chiave viene controllata (come in Python).
        assert_eq!(upload_encrypted_backups(&reqwest::Client::new(), &config_for(&accepting, "x"), &[], cloud_clock).await, Ok(Vec::new()));
    }

    /// Come helper per le date del golden (il primo giorno è il 21 dicembre 2026).
    struct NaiveDateWrapper;
    impl NaiveDateWrapper {
        fn start() -> chrono::NaiveDate {
            chrono::NaiveDate::from_ymd_opt(2026, 12, 21).unwrap()
        }
    }
}
