//! Server HTTP del bot: configurazione e avvio.
//!
//! Questo modulo si occupa di gestire il "cuore" del server web del bot.
//! In informatica, un server HTTP è un programma che "ascolta" le richieste provenienti da internet
//! (o da una rete locale) e risponde inviando dati, come pagine web o informazioni tecniche.
//!
//! Immagina il bot come un ufficio: il server HTTP è lo sportello dove arrivano le persone
//! (i browser o altre applicazioni) per chiedere informazioni.
//!
//! Le route (i "percorsi") dei vari moduli (come lo stato del bot, l'overlay per le dirette,
//! le API pubbliche e il sito per lo staff) vengono unite in un unico `Router`.
//! Un `Router` è come un centralino che smista le richieste in arrivo all'ufficio giusto
//! in base all'indirizzo richiesto (es. /health, /api, ecc.).

#![allow(dead_code)]

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Valore di `Cache-Control` per le risposte che non vanno mai memorizzate nel browser.
/// In HTTP, il "Cache-Control" dice al browser se può salvare una copia della risposta per il futuro.
/// "no-store" significa: "non salvare nulla, chiedimi sempre i dati aggiornati".
pub const NO_STORE: &str = "no-store";

/// L'indirizzo IP predefinito su cui il server ascolta.
/// "127.0.0.1" (localhost) significa che il server è accessibile solo dallo stesso computer su cui gira il bot.
const DEFAULT_HOST: &str = "127.0.0.1";
/// La porta di rete predefinita (un numero che identifica il servizio web del bot).
const DEFAULT_PORT: u16 = 8765;

/// Configurazione tecnica del server HTTP.
/// Contiene i parametri necessari per sapere dove e come attivare le funzionalità web.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// L'indirizzo (IP) dove il server accetta connessioni.
    pub host: String,
    /// Il numero della porta (es. 80 per il web standard, qui 8765).
    pub port: u16,
    /// L'indirizzo internet (URL) pubblico dell'overlay (es. https://miobot.it/overlay).
    /// L'overlay è una pagina web speciale che viene mostrata sopra i video durante le dirette (es. su Twitch).
    pub public_url: String,
    /// Indica se è presente una chiave segreta (Token) per le API pubbliche.
    /// Le API (Application Programming Interface) sono porte di servizio che permettono ad altri programmi
    /// di parlare con il bot in modo automatico.
    pub has_public_api_token: bool,
    /// La cartella sul computer che contiene i file del sito web (HTML, CSS, Immagini).
    pub site_dir: PathBuf,
}

impl HttpConfig {
    /// Crea una nuova configurazione leggendo le "variabili d'ambiente".
    /// Le variabili d'ambiente sono impostazioni salvate nel sistema operativo o in un file .env
    /// che permettono di personalizzare il comportamento del bot senza cambiare il codice.
    pub fn from_env() -> Self {
        // Funzione di utilità per leggere una variabile o usare un testo vuoto se manca.
        let var = |name: &str| env::var(name).unwrap_or_default().trim().to_string();

        // Determina la cartella del sito web. Se non specificata, usa "sfo-site/dist".
        let site_dir = match var("SFO_SITE_DIR") {
            dir if dir.is_empty() => PathBuf::from("sfo-site").join("dist"),
            dir => PathBuf::from(dir),
        };

        Self {
            // Legge l'host, o usa il valore predefinito 127.0.0.1.
            host: env::var("EVENT_OVERLAY_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string()),
            // Legge la porta, prova a trasformarla in numero, se fallisce usa 8765.
            port: var("EVENT_OVERLAY_PORT").parse().ok().filter(|p| *p != 0).unwrap_or(DEFAULT_PORT),
            // Rimuove eventuali slash finali dall'URL pubblico per evitare errori di formattazione.
            public_url: var("EVENT_OVERLAY_PUBLIC_URL").trim_end_matches('/').to_string(),
            // Controlla se è stato impostato un token segreto per le API.
            has_public_api_token: !var("PUBLIC_API_TOKEN").is_empty(),
            site_dir,
        }
    }

    /// Decide se il server deve effettivamente partire.
    /// Non ha senso accendere il server se non abbiamo nulla da mostrare (niente overlay, niente API e niente sito).
    pub fn should_start(&self) -> bool {
        !self.public_url.is_empty() || self.has_public_api_token || self.site_dir.join("index.html").is_file()
    }
}

/// Questa funzione avvia materialmente il server HTTP.
/// Prende la configurazione (`config`) e l'elenco dei percorsi gestiti (`router`).
///
/// Restituisce:
/// - L'indirizzo dove è effettivamente partito e un "task" (un processo leggero che tiene vivo il server).
/// - `None` (nulla) se il server non deve partire o se c'è un errore (es. porta già occupata da un altro programma).
pub async fn serve(config: &HttpConfig, router: Router) -> Option<(SocketAddr, JoinHandle<()>)> {
    // Se non c'è motivo di partire, avvisa nei log e si ferma.
    if !config.should_start() {
        warn!(
            "Server HTTP per overlay, sito community, API e SFO non avviato: \
             configura EVENT_OVERLAY_PUBLIC_URL, PUBLIC_API_TOKEN o SFO_SITE_DIR."
        );
        return None;
    }

    // Prova a "legarsi" (bind) all'indirizzo e alla porta scelti.
    // È come cercare di aprire un ufficio a un certo indirizzo stradale.
    let listener = match TcpListener::bind((config.host.as_str(), config.port)).await {
        Ok(listener) => listener,
        Err(e) => {
            // Se la porta è già usata da un altro programma (es. un altro bot o un server web), dà errore.
            error!("Impossibile avviare il server web su {}:{}: {e}", config.host, config.port);
            return None;
        }
    };

    // Recupera l'indirizzo finale (potrebbe essere cambiato se avevamo messo porta 0).
    let address = listener.local_addr().ok()?;
    info!("Server web del bot in ascolto su {address}.");

    // "Spawna" il server: lo lancia in background in modo che il bot possa continuare
    // a fare altre cose (come rispondere ai messaggi su Discord) mentre il server web gira.
    let task = tokio::spawn(async move {
        // Axum è la libreria che gestisce effettivamente le richieste HTTP.
        if let Err(e) = axum::serve(listener, router).await {
            error!("Il server HTTP si è fermato improvvisamente: {e}");
        }
    });

    Some((address, task))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Funzione di supporto per i test: crea una configurazione finta.
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
        // Non deve partire se tutto è vuoto.
        assert!(!config("", false, missing.clone()).should_start());
        // Deve partire se c'è l'URL dell'overlay.
        assert!(config("https://sfo.example.com", false, missing.clone()).should_start());
        // Deve partire se c'è il token API.
        assert!(config("", true, missing).should_start());

        // Test per il sito: deve esserci il file index.html.
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
        // Occupiamo una porta a caso.
        let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = config("https://sfo.example.com", false, PathBuf::new());
        config.port = taken.local_addr().unwrap().port();
        // Il server dovrebbe accorgersi che la porta è occupata e restituire None invece di crashare.
        assert!(serve(&config, Router::new()).await.is_none());
    }
}
