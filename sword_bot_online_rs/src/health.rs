//! Endpoint di salute per il monitoraggio dell'hosting, senza autenticazione
//! (equivalente di `cogs/health.py`).
//!
//! L'endpoint `/health` permette di verificare se il bot e il database funzionano correttamente.
//! Questo è importante per gli strumenti di monitoraggio (es. uptime monitor) che controllano
//! lo stato del servizio. L'endpoint non richiede autenticazione perché è pubblico e accessibile.
//!
//! La risposta include informazioni come:
//! - `status`: "ok" se tutto funziona, "degraded" se c'è un problema
//! - `bot_ready`: se il bot Discord è connesso e pronto
//! - `latency_ms`: la latenza media del bot in millisecondi
//! - `database`: se il database SQLite è accessibile
//! - `uptime_seconds`: quanto tempo il bot è stato attivo
//!
//! Usa una sola query semplice `SELECT 1` per verificare il database, senza caricare dati pesanti.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use poise::serenity_prelude::ShardManager;
use serde_json::json;
use tracing::error;

use crate::database::Database;
/* NO_STORE è una costante che definisce l'intestazione HTTP "Cache-Control: no-store".
   Questo impedisce ai browser e ai proxy di memorizzare in cache la risposta dell'endpoint
   di salute, assicurandosi che ogni richiesta venga processata correttamente dal server. */
use crate::web::NO_STORE;

/// Interfaccia astratta per ispezionare lo stato del bot Discord.
/// Questa interfaccia consente di verificare se il bot è pronto e della latenza,
/// separando la logica di monitoraggio dal codice specifico del bot.
pub trait BotProbe: Send + Sync {
    /// Verifica se il bot è connesso e pronto a ricevere comandi.
    /// Equivalente a `bot.is_ready()` in Python.
    fn is_ready(&self) -> bool;

    /// Restituisce la latenza media dell'heartbeat del bot in millisecondi.
    /// L'heartbeat è un "pulsazione" inviata periodicamente a Discord per mantenere la connessione viva.
    /// Restituisce `None` se non è stato ancora effettuato alcun heartbeat.
    /// Equivalente a `bot.latency` in Python.
    fn latency(&self) -> Pin<Box<dyn Future<Output = Option<Duration>> + Send + '_>>;
}

/// Struttura che tiene traccia dello stato del bot Discord.
/// Viene condivisa tra il client Discord (che gestisce la connessione) e il server HTTP
/// (che espone l'endpoint di salute). Usa tipi thread-safe perché può essere modificata
/// da più thread contemporaneamente.
#[derive(Default)]
pub struct BotStatus {
    /// Bandiera che indica se il bot è pronto (true) o chiuso (false).
    /// Usa AtomicBool per operazioni thread-safe senza lock.
    ready: AtomicBool,
    /// Il gestore dei "shards" (partizioni) del bot Discord.
    /// Viene inizializzato una volta e condiviso condiviso in modo sicuro.
    shard_manager: OnceLock<Arc<ShardManager>>,
}

impl BotStatus {
    /// Crea una nuova istanza di BotStatus pronta per essere condivisa.
    /// Restituisce un Arc (puntatore condiviso) per permettere al codice di
    /// utilizzarlo in più thread senza copiarlo.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Deve essere chiamato quando il client Discord ha completato l'avvio.
    /// Questo metodo viene invocato dall'evento `ready` di Discord, che indica
    /// che il bot è connesso e pronto a ricevere comandi.
    /// Imposta a `true` il flag `ready` e memorizza il shard_manager per la misurazione della latenza.
    pub fn mark_ready(&self, shard_manager: Arc<ShardManager>) {
        let _ = self.shard_manager.set(shard_manager);
        self.ready.store(true, Ordering::Release);
    }

    /// Deve essere chiamato quando il client Discord si chiude o si disconnette.
    /// Equivalente a `bot.is_closed()` in Python.
    /// Imposta a `false` il flag `ready` per indicare che il bot non è più disponibile.
    pub fn mark_closed(&self) {
        self.ready.store(false, Ordering::Release);
    }
}

impl BotProbe for BotStatus {
    /// Legge e restituisce lo stato di prontezza del bot.
    /// Usa `Ordering::Acquire` per assicurarsi che legga il valore scritto da `mark_ready`.
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Misura la latenza media del bot calcolando la media delle latenze di tutti i runners (shard).
    /// Ogni shard ha un proprio runner che misura la latenza verso Discord.
    /// Se non ci sono latenze misurabili, restituisce `None`.
    fn latency(&self) -> Pin<Box<dyn Future<Output = Option<Duration>> + Send + '_>> {
        Box::pin(async move {
            // Ottiene il shard_manager, altrimenti restituisce None
            let manager = self.shard_manager.get()?;
            // Prende il lock sui runners (struttura dati che contiene le informazioni di latenza)
            let runners = manager.runners.lock().await;
            // Raccoglie le latenze da tutti i runners che le hanno
            let latencies: Vec<Duration> = runners.values().filter_map(|runner| runner.latency).collect();
            if latencies.is_empty() {
                return None;
            }
            // Calcola la media aritmetica delle latenze
            Some(latencies.iter().sum::<Duration>() / latencies.len() as u32)
        })
    }
}

/// Tipo di funzione per il controllo del database.
/// Rappresenta una funzione che verifica se il database è accessibile.
/// Restituisce Ok(()) se tutto va bene, Err(String) con un messaggio di errore altrimenti.
type DatabaseCheck = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Stato condiviso per l'endpoint di salute.
/// Contiene tutte le dipendenze necessarie per rispondere al health check.
/// È clonabile perché viene spesso condiviso tra le richieste HTTP.
#[derive(Clone)]
pub struct HealthState {
    /// Interfaccia per verificare lo stato del bot Discord
    bot: Arc<dyn BotProbe>,
    /// Funzione per controllare se il database è accessibile
    database_check: DatabaseCheck,
    /// Timestamp di avvio del bot, usato per calcolare l'uptime
    started_at: Instant,
}

impl HealthState {
    /// Crea una nuova instance di HealthState con tutte le dipendenze.
    /// - `bot`: interfaccia per verificare lo stato del bot
    /// - `db`:connessione al database SQLite
    /// - `started_at`: momento di avvio del bot per calcolare l'uptime
    pub fn new(bot: Arc<dyn BotProbe>, db: Database, started_at: Instant) -> Self {
        // Crea una funzione di controllo del database che esegue una semplice query SELECT 1
        // Questa è efficiente e verifica solo che il database sia accessibile
        let database_check: DatabaseCheck = Arc::new(move || {
            db.with_conn(|connection| connection.query_row("SELECT 1", [], |row| row.get::<_, i64>(0)))
                .map(|_| ())
                .map_err(|error| error.to_string())
        });
        Self { bot, database_check, started_at }
    }

    /// Sostituisce il controllo del database con una funzione custom.
    /// Usato nei test per simulare errori del database senza compromettere il database reale.
    #[cfg(test)]
    fn with_database_check(mut self, check: DatabaseCheck) -> Self {
        self.database_check = check;
        self
    }
}

/// Converte un Duration in millisecondi arrotondati.
/// Usato per formattare la latenza nel JSON di risposta.
/// - `Some(Duration)`: converte in millisecondi e arrotonda al numero intero più vicino
/// - `None`: restituisce `None` (nessuna latenza misurata)
fn latency_ms(latency: Option<Duration>) -> Option<u64> {
    latency.map(|value| (value.as_secs_f64() * 1000.0).round() as u64)
}

/// Handler per l'endpoint HTTP GET /health.
/// Risponde con lo stato di salute del bot e del database.
async fn health(State(state): State<HealthState>) -> impl IntoResponse {
    // Verifica lo stato del database eseguendo la query di controllo
    let database = match (state.database_check)() {
        Ok(()) => "ok",
        Err(message) => {
            // Logga l'errore per il debug, ma non blocca l'intero handler
            error!("Controllo database di /health non riuscito: {message}");
            "error"
        }
    };

    // Verifica se il bot è pronto
    let ready = state.bot.is_ready();

    // L'istanza è sana solo se il bot è pronto E il database è accessibile
    let healthy = ready && database == "ok";

    // Misura la latenza del bot (potrebbe essere None se non ancora calcolata)
    let latency = latency_ms(state.bot.latency().await);

    // Costruisce il corpo della risposta JSON con tutte le informazioni di stato
    let body = json!({
        "status": if healthy { "ok" } else { "degraded" },
        "bot_ready": ready,
        "latency_ms": latency,
        "database": database,
        "uptime_seconds": state.started_at.elapsed().as_secs(),
    });

    // Restituisce lo status HTTP appropriato:
    // - 200 OK se tutto va bene
    // - 503 Service Unavailable se c'è un problema
    // - Intestazione Cache-Control: no-store per non memorizzare in cache
    let status = if healthy { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, [(header::CACHE_CONTROL, NO_STORE)], Json(body))
}

/// Crea e registra le route per l'endpoint di salute.
/// - GET /health: chiama la funzione `health` per controllare lo stato del bot
///   l'argomento `state` viene passato automaticamente a ogni handler tramite l'estrattore `State`.
pub fn routes(state: HealthState) -> Router {
    Router::new().route("/health", get(health)).with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use std::sync::atomic::AtomicU64;
    use tower::ServiceExt;

    /// Bot finto per i test: permette di simulare diversi stati di prontezza e latenza.
    /// - `ready`: indica se il bot è pronto o meno
    /// - `latency_micros`: latenza in microsecondi (0 significa non misurata)
    struct FakeBot {
        ready: AtomicBool,
        latency_micros: AtomicU64,
    }

    impl FakeBot {
        /// Crea un nuovo bot finto con lo stato di prontezza e latenza specificati.
        /// La latenza viene passata in millisecondi e convertita in microsecondi internamente.
        fn new(ready: bool, latency_ms: u64) -> Arc<Self> {
            Arc::new(Self {
                ready: AtomicBool::new(ready),
                latency_micros: AtomicU64::new(latency_ms * 1000),
            })
        }
    }

    /// Implementa l'interfaccia BotProbe per il bot finto.
    /// Questo permette ai test di usare FakeBot al posto di BotStatus reale.
    impl BotProbe for FakeBot {
        /// Legge e restituisce lo stato di prontezza del bot finto.
        fn is_ready(&self) -> bool {
            self.ready.load(Ordering::SeqCst)
        }

        /// Restituisce la latenza simulata del bot.
        /// Se la latenza è 0, restituisce None (nessuna latenza misurata).
        /// Altrimenti converte i microsecondi in un Duration.
        fn latency(&self) -> Pin<Box<dyn Future<Output = Option<Duration>> + Send + '_>> {
            Box::pin(async move {
                match self.latency_micros.load(Ordering::SeqCst) {
                    0 => None,
                    micros => Some(Duration::from_micros(micros)),
                }
            })
        }
    }

    /// Funzione di utilità per eseguire una richiesta HTTP GET a /health
    /// e restituire status, headers e corpo JSON.
    async fn get_health(state: HealthState) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
        // Crea una richiesta HTTP GET a /health e la invia al router
        let response = routes(state)
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // Separates la risposta in parti (status, headers) e corpo
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        (parts.status, parts.headers, serde_json::from_slice(&bytes).unwrap())
    }

    /// Crea uno HealthState di test con un bot finto e un database in memoria.
    fn state(bot: Arc<FakeBot>) -> HealthState {
        HealthState::new(bot, Database::open_in_memory().unwrap(), Instant::now())
    }

    /// Test: bot pronto e database accessibile restituiscono status 200 OK.
    /// Verifica che tutti i campi della risposta siano corretti.
    #[tokio::test]
    async fn healthy_bot_and_database_report_ok() {
        let (status, headers, body) = get_health(state(FakeBot::new(true, 42))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["bot_ready", "database", "latency_ms", "status", "uptime_seconds"]);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["bot_ready"], true);
        assert_eq!(body["latency_ms"], 42);
        assert_eq!(body["database"], "ok");
        assert!(body["uptime_seconds"].is_u64());
    }

    /// Test: bot non pronto restituisce status 503 con "degraded".
    /// La latenza è null finché non c'è un heartbeat.
    #[tokio::test]
    async fn bot_not_ready_is_degraded_with_null_latency() {
        // Prima del primo heartbeat discord.py dà inf/nan: qui è `None`.
        let (status, _, body) = get_health(state(FakeBot::new(false, 0))).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "degraded");
        assert_eq!(body["bot_ready"], false);
        assert!(body["latency_ms"].is_null());
        assert_eq!(body["database"], "ok");
    }

    /// Test: bot pronto ma senza heartbeat ancora ha latenza null.
    /// Non è un errore: l'uptime monitor lo vede come OK.
    #[tokio::test]
    async fn ready_bot_without_heartbeat_has_null_latency() {
        let (status, _, body) = get_health(state(FakeBot::new(true, 0))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["latency_ms"].is_null());
    }

    /// Test: errore del database restituisce status 503 con "degraded".
    /// Verifica che la risposta non venga memorizzata in cache.
    #[tokio::test]
    async fn database_error_is_degraded_and_not_cached() {
        // Usa with_database_check per simulare un database chiuso
        let state = state(FakeBot::new(true, 42)).with_database_check(Arc::new(|| Err("database chiuso".into())));
        let (status, headers, body) = get_health(state).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "degraded");
        assert_eq!(body["database"], "error");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }

    /// Test: l'endpoint /health non richiede autenticazione.
    /// Chiama direttamente l'endpoint senza header Authorization.
    #[tokio::test]
    async fn health_needs_no_authentication() {
        let (status, _, _) = get_health(state(FakeBot::new(true, 1))).await;
        assert!(status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN);
    }

    /// Test: verifica che l'endpoint funzioni su una socket TCP reale.
    /// Usa reqwest per fare una richiesta HTTP reale e verifica la risposta.
    #[tokio::test]
    async fn served_over_a_real_tcp_socket() {
        let config = crate::web::HttpConfig {
            host: "127.0.0.1".into(),
            port: 0, // porta effimera scelta dal sistema
            public_url: "https://sfo.example.com".into(),
            has_public_api_token: false,
            site_dir: std::path::PathBuf::new(),
        };
        // Avvia il server HTTP in background
        let (address, task) = crate::web::serve(&config, routes(state(FakeBot::new(true, 42))))
            .await
            .expect("il server deve partire");

        // Fai una richiesta HTTP GET a /health
        let response = reqwest::get(format!("http://{address}/health")).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["latency_ms"], 42);

        // Verifica che un endpoint inesistente ritorni 404
        let missing = reqwest::get(format!("http://{address}/non-esiste")).await.unwrap();
        assert_eq!(missing.status(), 404);
        task.abort();
    }

    /// Test: verifica che la conversione della latenza in millisecondi funzioni correttamente.
    /// Controlla l'arrotondamento dei valori decimali.
    #[test]
    fn latency_is_rounded_to_milliseconds() {
        assert_eq!(latency_ms(None), None);
        assert_eq!(latency_ms(Some(Duration::from_micros(41_600))), Some(42));  // 41.6ms -> 42ms
        assert_eq!(latency_ms(Some(Duration::from_micros(41_400))), Some(41));  // 41.4ms -> 41ms
    }
}
