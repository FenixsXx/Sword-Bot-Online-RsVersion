//! Server HTTP del bot: configurazione e avvio (parte di `LiveEventCog.cog_load` in Python).
//!
//! Le route dei vari moduli (health, overlay, API pubblica, sito staff) vengono unite in un
//! unico `Router` e servite dallo stesso listener, come nell'app aiohttp originale.
// Alcuni campi serviranno alle route che porteremo nelle prossime iterazioni.
#![allow(dead_code)]

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Valore di `Cache-Control` per le risposte che non vanno mai memorizzate.
pub const NO_STORE: &str = "no-store";

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 8765;

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    /// Origine HTTPS pubblica (`EVENT_OVERLAY_PUBLIC_URL`, senza `/` finale).
    pub public_url: String,
    pub has_public_api_token: bool,
    pub site_dir: PathBuf,
}

impl HttpConfig {
    /// Legge `EVENT_OVERLAY_*`, `PUBLIC_API_TOKEN` e `SFO_SITE_DIR`. I valori non validi sono già
    /// stati scartati da `config::load_environment`; qui si usano solo i predefiniti.
    pub fn from_env() -> Self {
        let var = |name: &str| env::var(name).unwrap_or_default().trim().to_string();
        let site_dir = match var("SFO_SITE_DIR") {
            dir if dir.is_empty() => PathBuf::from("sfo-site").join("dist"),
            dir => PathBuf::from(dir),
        };
        Self {
            host: env::var("EVENT_OVERLAY_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string()),
            port: var("EVENT_OVERLAY_PORT").parse().ok().filter(|p| *p != 0).unwrap_or(DEFAULT_PORT),
            public_url: var("EVENT_OVERLAY_PUBLIC_URL").trim_end_matches('/').to_string(),
            has_public_api_token: !var("PUBLIC_API_TOKEN").is_empty(),
            site_dir,
        }
    }

    /// Il server parte solo se serve a qualcosa: overlay pubblico, API o sito community.
    pub fn should_start(&self) -> bool {
        !self.public_url.is_empty() || self.has_public_api_token || self.site_dir.join("index.html").is_file()
    }
}

/// Avvia il server HTTP. Restituisce indirizzo effettivo e task, oppure `None` se non è
/// configurato o la porta non è disponibile (in tal caso il bot continua senza web, come in Python).
pub async fn serve(config: &HttpConfig, router: Router) -> Option<(SocketAddr, JoinHandle<()>)> {
    if !config.should_start() {
        warn!(
            "Server HTTP per overlay, sito community, API e SFO non avviato: \
             configura EVENT_OVERLAY_PUBLIC_URL, PUBLIC_API_TOKEN o SFO_SITE_DIR."
        );
        return None;
    }
    let listener = match TcpListener::bind((config.host.as_str(), config.port)).await {
        Ok(listener) => listener,
        Err(e) => {
            error!("Impossibile avviare l'overlay su {}:{}: {e}", config.host, config.port);
            return None;
        }
    };
    let address = listener.local_addr().ok()?;
    info!("Overlay eventi in ascolto su {address}.");
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            error!("Il server HTTP si è fermato: {e}");
        }
    });
    Some((address, task))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(public_url: &str, token: bool, site_dir: PathBuf) -> HttpConfig {
        HttpConfig {
            host: "127.0.0.1".into(),
            port: 0,
            public_url: public_url.into(),
            has_public_api_token: token,
            site_dir,
        }
    }

    #[test]
    fn server_starts_only_when_something_is_configured() {
        let missing = std::env::temp_dir().join("sfo-sito-inesistente");
        assert!(!config("", false, missing.clone()).should_start());
        assert!(config("https://sfo.example.com", false, missing.clone()).should_start());
        assert!(config("", true, missing).should_start());

        let dir = std::env::temp_dir().join(format!("sfo-web-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!config("", false, dir.clone()).should_start(), "serve index.html");
        std::fs::write(dir.join("index.html"), "<html></html>").unwrap();
        assert!(config("", false, dir.clone()).should_start());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn unconfigured_server_is_not_started() {
        let missing = std::env::temp_dir().join("sfo-sito-inesistente");
        assert!(serve(&config("", false, missing), Router::new()).await.is_none());
    }

    #[tokio::test]
    async fn busy_port_does_not_crash() {
        let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = config("https://sfo.example.com", false, PathBuf::new());
        config.port = taken.local_addr().unwrap().port();
        assert!(serve(&config, Router::new()).await.is_none());
    }
}
