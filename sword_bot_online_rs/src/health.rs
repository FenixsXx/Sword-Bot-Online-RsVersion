//! Endpoint di salute per il monitoraggio dell'hosting, senza autenticazione
//! (equivalente di `cogs/health.py`).
//!
//! Restituisce solo informazioni non sensibili e costa una sola query `SELECT 1`.

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
use crate::web::NO_STORE;

/// Quello che `/health` deve sapere del bot (in Python: `bot.is_ready()` e `bot.latency`).
pub trait BotProbe: Send + Sync {
    fn is_ready(&self) -> bool;
    /// Latenza media dell'heartbeat; `None` finché non c'è un heartbeat misurato.
    fn latency(&self) -> Pin<Box<dyn Future<Output = Option<Duration>> + Send + '_>>;
}

/// Stato del bot condiviso tra il client Discord e il server HTTP.
#[derive(Default)]
pub struct BotStatus {
    ready: AtomicBool,
    shard_manager: OnceLock<Arc<ShardManager>>,
}

impl BotStatus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Da chiamare quando il client ha completato l'avvio (evento ready).
    pub fn mark_ready(&self, shard_manager: Arc<ShardManager>) {
        let _ = self.shard_manager.set(shard_manager);
        self.ready.store(true, Ordering::Release);
    }

    /// Da chiamare quando il client si ferma (equivalente di `bot.is_closed()`).
    pub fn mark_closed(&self) {
        self.ready.store(false, Ordering::Release);
    }
}

impl BotProbe for BotStatus {
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn latency(&self) -> Pin<Box<dyn Future<Output = Option<Duration>> + Send + '_>> {
        Box::pin(async move {
            let manager = self.shard_manager.get()?;
            let runners = manager.runners.lock().await;
            let latencies: Vec<Duration> = runners.values().filter_map(|runner| runner.latency).collect();
            if latencies.is_empty() {
                return None;
            }
            Some(latencies.iter().sum::<Duration>() / latencies.len() as u32)
        })
    }
}

type DatabaseCheck = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Dipendenze dell'endpoint, clonabili a basso costo.
#[derive(Clone)]
pub struct HealthState {
    bot: Arc<dyn BotProbe>,
    database_check: DatabaseCheck,
    started_at: Instant,
}

impl HealthState {
    pub fn new(bot: Arc<dyn BotProbe>, db: Database, started_at: Instant) -> Self {
        let database_check: DatabaseCheck = Arc::new(move || {
            db.with_conn(|connection| connection.query_row("SELECT 1", [], |row| row.get::<_, i64>(0)))
                .map(|_| ())
                .map_err(|error| error.to_string())
        });
        Self { bot, database_check, started_at }
    }

    /// Sostituisce il controllo del database (per simulare un guasto nei test).
    #[cfg(test)]
    fn with_database_check(mut self, check: DatabaseCheck) -> Self {
        self.database_check = check;
        self
    }
}

/// `Some(ms)` arrotondati, `None` se la latenza non è ancora nota.
fn latency_ms(latency: Option<Duration>) -> Option<u64> {
    latency.map(|value| (value.as_secs_f64() * 1000.0).round() as u64)
}

async fn health(State(state): State<HealthState>) -> impl IntoResponse {
    let database = match (state.database_check)() {
        Ok(()) => "ok",
        Err(message) => {
            error!("Controllo database di /health non riuscito: {message}");
            "error"
        }
    };
    let ready = state.bot.is_ready();
    let healthy = ready && database == "ok";
    let latency = latency_ms(state.bot.latency().await);
    let body = json!({
        "status": if healthy { "ok" } else { "degraded" },
        "bot_ready": ready,
        "latency_ms": latency,
        "database": database,
        "uptime_seconds": state.started_at.elapsed().as_secs(),
    });
    let status = if healthy { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, [(header::CACHE_CONTROL, NO_STORE)], Json(body))
}

/// Registra `GET /health` (equivalente di `register_health_routes`).
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

    /// Bot finto: pronto o no, con latenza in microsecondi (0 = non misurata).
    struct FakeBot {
        ready: AtomicBool,
        latency_micros: AtomicU64,
    }

    impl FakeBot {
        fn new(ready: bool, latency_ms: u64) -> Arc<Self> {
            Arc::new(Self {
                ready: AtomicBool::new(ready),
                latency_micros: AtomicU64::new(latency_ms * 1000),
            })
        }
    }

    impl BotProbe for FakeBot {
        fn is_ready(&self) -> bool {
            self.ready.load(Ordering::SeqCst)
        }
        fn latency(&self) -> Pin<Box<dyn Future<Output = Option<Duration>> + Send + '_>> {
            Box::pin(async move {
                match self.latency_micros.load(Ordering::SeqCst) {
                    0 => None,
                    micros => Some(Duration::from_micros(micros)),
                }
            })
        }
    }

    async fn get_health(state: HealthState) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
        let response = routes(state)
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.unwrap();
        (parts.status, parts.headers, serde_json::from_slice(&bytes).unwrap())
    }

    fn state(bot: Arc<FakeBot>) -> HealthState {
        HealthState::new(bot, Database::open_in_memory().unwrap(), Instant::now())
    }

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

    #[tokio::test]
    async fn ready_bot_without_heartbeat_has_null_latency() {
        let (status, _, body) = get_health(state(FakeBot::new(true, 0))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["latency_ms"].is_null());
    }

    #[tokio::test]
    async fn database_error_is_degraded_and_not_cached() {
        let state = state(FakeBot::new(true, 42)).with_database_check(Arc::new(|| Err("database chiuso".into())));
        let (status, headers, body) = get_health(state).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "degraded");
        assert_eq!(body["database"], "error");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn health_needs_no_authentication() {
        let (status, _, _) = get_health(state(FakeBot::new(true, 1))).await;
        assert!(status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn served_over_a_real_tcp_socket() {
        let config = crate::web::HttpConfig {
            host: "127.0.0.1".into(),
            port: 0, // porta effimera scelta dal sistema
            public_url: "https://sfo.example.com".into(),
            has_public_api_token: false,
            site_dir: std::path::PathBuf::new(),
        };
        let (address, task) = crate::web::serve(&config, routes(state(FakeBot::new(true, 42))))
            .await
            .expect("il server deve partire");
        let response = reqwest::get(format!("http://{address}/health")).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["latency_ms"], 42);
        let missing = reqwest::get(format!("http://{address}/non-esiste")).await.unwrap();
        assert_eq!(missing.status(), 404);
        task.abort();
    }

    #[test]
    fn latency_is_rounded_to_milliseconds() {
        assert_eq!(latency_ms(None), None);
        assert_eq!(latency_ms(Some(Duration::from_micros(41_600))), Some(42));
        assert_eq!(latency_ms(Some(Duration::from_micros(41_400))), Some(41));
    }
}
