use std::time::Instant as StdInstant;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message as WsMessage};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::*;
use crate::database::Database;
use crate::site_auth::tests::{FakeDirectory, FakeOAuth};
use crate::site_auth::{AuthConfig, DirectoryError, GuildFacts, SiteMember};

fn golden() -> Value {
    serde_json::from_str(include_str!("../golden/site_events_expected.json")).unwrap()
}

const FAR: &str = "2099-01-01T00:00:00+00:00";

struct Env {
    db: Database,
    directory: FakeDirectory,
    auth: Arc<SiteAuth>,
}

fn env() -> Env {
    let db = Database::open_in_memory().unwrap();
    db.set_setting("guild_id", "100").unwrap();
    db.set_setting("staff_roles", "777").unwrap();
    let directory = FakeDirectory::default();
    directory.0.lock().unwrap().guild = Some(GuildFacts { owner_id: 1 });
    directory.add(1, false, &[]);
    directory.add(2, false, &[777]);
    directory.add(3, false, &[999]);
    let auth = Arc::new(SiteAuth::new(db.clone(), Arc::new(directory.clone()), Arc::new(FakeOAuth::default()), AuthConfig::default(), None));
    let e = Env { db, directory, auth };
    e.session("s1", 1, FAR);
    e.session("s2", 2, FAR);
    e
}

impl Env {
    fn session(&self, id: &str, user: i64, expires: &str) {
        self.db
            .execute(
                "INSERT OR REPLACE INTO web_sessions(session_id,user_id,display_name,created_at,expires_at) VALUES(?,?,'x','2027-01-01T00:00:00+00:00',?)",
                rusqlite::params![id, user, expires],
            )
            .unwrap();
    }

    async fn serve(&self, config: HubConfig) -> (std::net::SocketAddr, Arc<SiteEventHub>) {
        let hub = SiteEventHub::new(self.auth.clone(), config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = hub.routes();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (address, hub)
    }
}

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, PartialEq)]
enum Event {
    Text(String),
    Close(u16, String),
    Dropped,
}

async fn connect(address: std::net::SocketAddr, cookie: Option<&str>, headers: &[(&str, String)]) -> Result<Client, (u16, String)> {
    let mut request = format!("ws://{address}/api/site/events").into_client_request().unwrap();
    if let Some(cookie) = cookie {
        request.headers_mut().insert("Cookie", cookie.parse().unwrap());
    }
    for (name, value) in headers {
        request.headers_mut().insert(axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(), value.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((client, _)) => Ok(client),
        Err(WsError::Http(response)) => {
            let body = response.body().clone().unwrap_or_default();
            Err((response.status().as_u16(), String::from_utf8(body).unwrap()))
        }
        Err(other) => panic!("collegamento non riuscito: {other}"),
    }
}

async fn next_event(client: &mut Client, wait: Duration) -> Option<Event> {
    loop {
        match tokio::time::timeout(wait, client.next()).await {
            Err(_) => return None,
            Ok(None) | Ok(Some(Err(_))) => return Some(Event::Dropped),
            Ok(Some(Ok(WsMessage::Text(text)))) => return Some(Event::Text(text.to_string())),
            Ok(Some(Ok(WsMessage::Close(frame)))) => {
                return Some(frame.map_or(Event::Close(1005, String::new()), |frame| Event::Close(u16::from(frame.code), frame.reason.to_string())));
            }
            Ok(Some(Ok(_))) => {}
        }
    }
}

fn text(value: &str) -> Event {
    Event::Text(value.to_string())
}

async fn eventually(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..300 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condizione non raggiunta: {what}");
}

const READY: &str = "{\"type\": \"ready\"}";

// --- apertura ------------------------------------------------------------------------------------

#[tokio::test]
async fn refusals_match_the_python_implementation() {
    let golden = golden();
    let e = env();
    let (address, _hub) = e.serve(HubConfig::default()).await;
    for case in golden["handshake"].as_array().unwrap() {
        let label = case["label"].as_str().unwrap();
        let (cookie, headers): (Option<&str>, Vec<(&str, String)>) = match label {
            "senza_cookie" => (None, vec![]),
            "sessione_sconosciuta" => (Some("sfo_site_session=no"), vec![]),
            "origine_estranea" => (Some("sfo_site_session=s1"), vec![("Origin", "https://evil.example".into())]),
            "origine_null" => (Some("sfo_site_session=s1"), vec![("Origin", "null".into())]),
            other => panic!("caso sconosciuto: {other}"),
        };
        let (status, body) = connect(address, cookie, &headers).await.err().unwrap_or_else(|| panic!("{label}: doveva essere rifiutato"));
        assert_eq!(u64::from(status), case["status"].as_u64().unwrap(), "{label}: stato");
        assert_eq!(body, case["body"].as_str().unwrap(), "{label}: corpo");
    }
}

#[tokio::test]
async fn allowed_and_refused_origins_match_the_python_implementation() {
    let golden = golden();
    let e = env();
    let (address, hub) = e.serve(HubConfig::default()).await;
    let host = address.to_string();
    for case in golden["origins"].as_array().unwrap() {
        let label = case["label"].as_str().unwrap();
        let headers: Vec<(&str, String)> = match label {
            "senza_origine" => vec![],
            "origine_vuota" => vec![("Origin", String::new())],
            "stesso_host" => vec![("Origin", format!("http://{host}"))],
            "stesso_host_maiuscolo" => vec![("Origin", format!("HTTP://{}", host.to_uppercase()))],
            "stesso_host_https" => vec![("Origin", format!("https://{host}"))],
            "host_inoltrato" => vec![("Origin", "https://sito.example".into()), ("X-Forwarded-Host", "Sito.Example".into())],
            "host_inoltrato_con_elenco" => vec![("Origin", "https://sito.example".into()), ("X-Forwarded-Host", "sito.example, altro.example".into())],
            "host_inoltrato_secondo" => vec![("Origin", "https://altro.example".into()), ("X-Forwarded-Host", "sito.example, altro.example".into())],
            "origine_con_percorso" => vec![("Origin", format!("http://{host}/pagina"))],
            "origine_porta_diversa" => vec![("Origin", "http://127.0.0.1:1".into())],
            other => panic!("caso sconosciuto: {other}"),
        };
        match connect(address, Some("sfo_site_session=s1"), &headers).await {
            Ok(mut client) => {
                assert!(case["ok"].as_bool().unwrap(), "{label}: doveva essere rifiutato");
                assert_eq!(next_event(&mut client, Duration::from_secs(2)).await, Some(text(READY)), "{label}");
                drop(client);
                eventually(label, || hub.client_count() == 0).await;
            }
            Err((status, _)) => {
                assert!(!case["ok"].as_bool().unwrap(), "{label}: doveva essere ammesso");
                assert_eq!(u64::from(status), case["status"].as_u64().unwrap(), "{label}");
            }
        }
    }
}

#[tokio::test]
async fn the_number_of_live_connections_is_limited() {
    let capacity = golden()["capacity"].clone();
    let e = env();
    let (address, hub) = e.serve(HubConfig { max_clients: 2, ..HubConfig::default() }).await;
    let mut a = connect(address, Some("sfo_site_session=s1"), &[]).await.unwrap();
    let mut b = connect(address, Some("sfo_site_session=s2"), &[]).await.unwrap();
    assert_eq!(next_event(&mut a, Duration::from_secs(2)).await, Some(text(READY)));
    assert_eq!(next_event(&mut b, Duration::from_secs(2)).await, Some(text(READY)));
    assert_eq!(hub.client_count() as u64, capacity["count"].as_u64().unwrap());
    let (status, body) = connect(address, Some("sfo_site_session=s1"), &[]).await.err().unwrap();
    assert_eq!(u64::from(status), capacity["third"]["status"].as_u64().unwrap());
    assert_eq!(body, capacity["third"]["body"].as_str().unwrap());
    drop(a);
    eventually("posto liberato", || hub.client_count() as u64 == capacity["after_close"].as_u64().unwrap()).await;
    let mut c = connect(address, Some("sfo_site_session=s1"), &[]).await.unwrap();
    assert_eq!(next_event(&mut c, Duration::from_secs(2)).await, Some(text(READY)));
    assert_eq!(hub.client_count() as u64, capacity["count_again"].as_u64().unwrap());
    drop((b, c));
    eventually("tutto libero", || hub.client_count() == 0).await;
}

// --- pubblicazione -------------------------------------------------------------------------------

#[tokio::test]
async fn publications_are_coalesced_like_python() {
    let publish = golden()["publish"].clone();
    let e = env();
    let (address, hub) = e.serve(HubConfig { coalesce: Duration::from_millis(300), ..HubConfig::default() }).await;
    let mut first = connect(address, Some("sfo_site_session=s1"), &[]).await.unwrap();
    let mut second = connect(address, Some("sfo_site_session=s2"), &[]).await.unwrap();
    for client in [&mut first, &mut second] {
        assert_eq!(next_event(client, Duration::from_secs(2)).await, Some(text(READY)));
    }
    eventually("due client", || hub.client_count() == 2).await;
    let change = |topic: &str| text(&change_message(topic));

    // Una raffica: un solo messaggio per argomento (l'argomento sconosciuto si ignora).
    hub.publish("polls");
    for _ in 0..5 {
        hub.publish("polls");
    }
    hub.publish("proposals");
    hub.publish("sconosciuto");
    let burst_started = StdInstant::now();
    for client in [&mut first, &mut second] {
        assert_eq!(next_event(client, Duration::from_secs(2)).await, Some(change("polls")));
        assert_eq!(next_event(client, Duration::from_secs(2)).await, Some(change("proposals")));
        assert_eq!(next_event(client, Duration::from_millis(5)).await, None, "nessun messaggio in più");
    }
    assert_eq!(publish["burst"][0], json!([["text", "{\"type\": \"change\", \"topic\": \"polls\"}"], ["text", "{\"type\": \"change\", \"topic\": \"proposals\"}"]]));
    assert!(burst_started.elapsed() < Duration::from_millis(600));

    // Lo stesso argomento subito dopo: arriva solo quando è passato l'intervallo.
    let published = StdInstant::now();
    hub.publish("polls");
    assert_eq!(next_event(&mut first, Duration::from_millis(100)).await, None, "troppo presto");
    assert_eq!(publish["too_early"], json!([3, 3]), "anche in Python non è ancora arrivato");
    assert_eq!(next_event(&mut first, Duration::from_secs(2)).await, Some(change("polls")));
    assert_eq!(next_event(&mut second, Duration::from_secs(2)).await, Some(change("polls")));
    assert!(published.elapsed() >= Duration::from_millis(150), "l'intervallo è stato rispettato");

    // Passato l'intervallo, gli altri argomenti partono subito.
    tokio::time::sleep(Duration::from_millis(400)).await;
    hub.publish("events");
    hub.publish("reports");
    hub.publish("settings");
    for topic in ["events", "reports", "settings"] {
        assert_eq!(next_event(&mut first, Duration::from_secs(2)).await, Some(change(topic)));
    }
    assert_eq!(publish["topics"], json!([["text", change_message("events")], ["text", change_message("reports")], ["text", change_message("settings")]]));

    // Senza nessuno collegato non si mette niente in coda, e chi arriva dopo non trova arretrati.
    drop((first, second));
    eventually("nessuno", || hub.client_count() == 0).await;
    hub.publish("polls");
    assert!(hub.state().scheduled.is_empty(), "{:?}", publish["scheduled_without_clients"]);
    let mut late = connect(address, Some("sfo_site_session=s1"), &[]).await.unwrap();
    assert_eq!(next_event(&mut late, Duration::from_secs(2)).await, Some(text(READY)));
    assert_eq!(next_event(&mut late, Duration::from_millis(300)).await, None, "nessun arretrato");

    // Quello che il browser scrive si ignora; un messaggio troppo grande chiude il collegamento.
    late.send(WsMessage::text("ciao")).await.unwrap();
    assert_eq!(next_event(&mut late, Duration::from_millis(200)).await, None);
    assert_eq!(hub.client_count(), 1);
    late.send(WsMessage::text("x".repeat(2000))).await.unwrap();
    assert_eq!(next_event(&mut late, Duration::from_secs(2)).await, Some(Event::Close(1009, String::new())));
    eventually("chiuso", || hub.client_count() == 0).await;
}

#[test]
fn unknown_topics_and_a_missing_runtime_do_nothing() {
    let e = {
        // Fuori da un runtime non si può creare un database con client: basta un hub vuoto.
        let db = Database::open_in_memory().unwrap();
        let directory = FakeDirectory::default();
        let auth = Arc::new(SiteAuth::new(db, Arc::new(directory), Arc::new(FakeOAuth::default()), AuthConfig::default(), None));
        SiteEventHub::new(auth, HubConfig::default())
    };
    e.publish("polls");
    e.publish("inesistente");
    assert!(e.state().scheduled.is_empty());
    assert_eq!(json!(TOPICS), golden()["constants"]["topics"].as_array().map(|topics| {
        let mut sorted: Vec<&Value> = topics.iter().collect();
        sorted.sort_by_key(|topic| TOPICS.iter().position(|known| Some(*known) == topic.as_str()));
        json!(sorted)
    }).unwrap());
    assert_eq!(u64::from(SESSION_EXPIRED_CLOSE_CODE), golden()["constants"]["expired_code"].as_u64().unwrap());
}

#[tokio::test]
async fn a_slow_client_is_closed_instead_of_blocking_the_others() {
    let e = env();
    let hub = SiteEventHub::new(e.auth.clone(), HubConfig::default());
    let (slow_queue, mut slow_inbox) = mpsc::channel(1);
    let (slow_close, mut slow_close_requests) = watch::channel(None);
    let (fast_queue, mut fast_inbox) = mpsc::channel(8);
    let (fast_close, _fast_close_requests) = watch::channel(None);
    {
        let mut state = hub.state();
        state.clients.insert(1, super::Client { queue: slow_queue, close: slow_close });
        state.clients.insert(2, super::Client { queue: fast_queue, close: fast_close });
        state.scheduled.insert("polls".into());
    }
    hub.flush("polls");
    assert_eq!(hub.state().clients.len(), 2);
    hub.state().scheduled.insert("polls".into());
    hub.flush("polls");
    assert_eq!(hub.state().clients.len(), 1, "la coda piena fa chiudere il lento");
    slow_close_requests.changed().await.unwrap();
    assert_eq!(*slow_close_requests.borrow(), Some((1013, "Client troppo lento")));
    assert_eq!(slow_inbox.try_recv().unwrap(), change_message("polls"));
    assert_eq!(fast_inbox.try_recv().unwrap(), change_message("polls"));
    assert_eq!(fast_inbox.try_recv().unwrap(), change_message("polls"));
}

// --- sessione ------------------------------------------------------------------------------------

#[tokio::test]
async fn the_session_is_checked_again_while_a_client_stays_connected() {
    let golden = golden();
    let reval = &golden["revalidation"];
    let e = env();
    e.directory.add(4, false, &[777]);
    e.directory.add(5, false, &[777]);
    e.directory.add(6, false, &[777]);
    for (id, user) in [("s4", 4), ("s5", 5), ("s6", 6)] {
        e.session(id, user, FAR);
    }
    let (address, hub) = e.serve(HubConfig { revalidate: Duration::from_millis(300), ..HubConfig::default() }).await;
    let mut ok = connect(address, Some("sfo_site_session=s1"), &[]).await.unwrap();
    let mut deleted = connect(address, Some("sfo_site_session=s2"), &[]).await.unwrap();
    let mut demoted = connect(address, Some("sfo_site_session=s4"), &[]).await.unwrap();
    let mut errored = connect(address, Some("sfo_site_session=s5"), &[]).await.unwrap();
    let mut expired = connect(address, Some("sfo_site_session=s6"), &[]).await.unwrap();
    for client in [&mut ok, &mut deleted, &mut demoted, &mut errored, &mut expired] {
        assert_eq!(next_event(client, Duration::from_secs(2)).await, Some(text(READY)));
    }
    e.db.execute("DELETE FROM web_sessions WHERE session_id='s2'", []).unwrap();
    e.directory.add(4, false, &[999]);
    e.directory.0.lock().unwrap().errors.insert(5, DirectoryError::Http("500".into()));
    e.db.execute("UPDATE web_sessions SET expires_at='2000-01-01T00:00:00+00:00' WHERE session_id='s6'", []).unwrap();

    let closed = Event::Close(4401, "Sessione scaduta".into());
    assert_eq!(next_event(&mut deleted, Duration::from_secs(3)).await, Some(closed));
    assert_eq!(next_event(&mut demoted, Duration::from_secs(3)).await, Some(Event::Close(4401, "Sessione scaduta".into())));
    assert_eq!(next_event(&mut expired, Duration::from_secs(3)).await, Some(Event::Close(4401, "Sessione scaduta".into())));
    // Chi è ancora in regola (o per cui Discord non risponde) resta collegato.
    assert_eq!(next_event(&mut ok, Duration::from_millis(900)).await, None);
    assert_eq!(next_event(&mut errored, Duration::from_millis(100)).await, None);
    eventually("due rimasti", || hub.client_count() as u64 == reval["count"].as_u64().unwrap()).await;
    assert_eq!(golden["constants"]["expired_code"], 4401);
}

#[tokio::test]
async fn shutting_down_closes_everyone_and_drops_pending_messages() {
    let e = env();
    let (address, hub) = e.serve(HubConfig::default()).await;
    let mut a = connect(address, Some("sfo_site_session=s1"), &[]).await.unwrap();
    let mut b = connect(address, Some("sfo_site_session=s1"), &[]).await.unwrap();
    for client in [&mut a, &mut b] {
        assert_eq!(next_event(client, Duration::from_secs(2)).await, Some(text(READY)));
    }
    eventually("due client", || hub.client_count() == 2).await;
    hub.publish("polls");
    hub.shutdown();
    // Python ha lo stesso scenario, ma aiohttp manda 1000 invece del codice del codice: qui arriva 1001.
    for client in [&mut a, &mut b] {
        assert_eq!(next_event(client, Duration::from_secs(2)).await, Some(Event::Close(1001, "Server in arresto".into())));
    }
    eventually("vuoto", || hub.client_count() == 0).await;
    assert!(hub.state().scheduled.is_empty());
    assert_eq!(golden()["shutdown"]["count"], 0);
}

#[test]
fn same_origin_checks_the_host_and_the_forwarded_host() {
    let headers = |pairs: &[(&str, &str)]| {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(), value.parse().unwrap());
        }
        map
    };
    assert!(same_origin(&headers(&[("host", "sito.it")])), "senza Origin");
    assert!(same_origin(&headers(&[("host", "sito.it"), ("origin", "https://SITO.it")])));
    assert!(same_origin(&headers(&[("host", "sito.it:8443"), ("origin", "https://sito.it:8443/x/y?z")])));
    assert!(!same_origin(&headers(&[("host", "sito.it"), ("origin", "https://sito.it:8443")])));
    assert!(!same_origin(&headers(&[("host", "sito.it"), ("origin", "null")])));
    assert!(!same_origin(&headers(&[("host", "sito.it"), ("origin", "https://altro.it")])));
    assert!(same_origin(&headers(&[("host", "interno:8765"), ("x-forwarded-host", "Sito.IT, proxy"), ("origin", "https://sito.it")])));
    assert!(!same_origin(&headers(&[("host", "interno:8765"), ("x-forwarded-host", "sito.it, proxy"), ("origin", "https://proxy")])));
    let _ = SiteMember { display_name: String::new(), administrator: false, role_ids: vec![] };
}
