use super::*;
use crate::overlay::{PublicUrl, VoiceSource};
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message as WsMessage};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tower::ServiceExt;

const ORIGIN: &str = "https://overlay.example.it";
const TOKEN: &str = "token-di-prova";

fn golden() -> Value {
    serde_json::from_str(include_str!("../golden/overlay_expected.json")).unwrap()
}

#[derive(Debug)]
struct FixedSource {
    members: Vec<VoiceMember>,
    levels: HashMap<u64, f64>,
}

impl VoiceSource for FixedSource {
    fn members(&self) -> Vec<VoiceMember> {
        self.members.clone()
    }
    fn levels(&self) -> HashMap<u64, f64> {
        self.levels.clone()
    }
}

fn member(id: u64, name: &str) -> VoiceMember {
    VoiceMember { id, name: name.to_string(), avatar_url: format!("https://cdn.discordapp.com/avatars/{id}/a.png") }
}

fn session_info() -> LiveSessionInfo {
    let golden = golden();
    let meta = &golden["session_meta"];
    LiveSessionInfo {
        title: meta["title"].as_str().unwrap().into(),
        guild_id: 7,
        channel_id: 555,
        channel_name: meta["channel"].as_str().unwrap().into(),
        started_at: meta["started_at"].as_str().unwrap().into(),
    }
}

/// Il database con le statistiche e le serie usate dal golden.
fn seeded_db() -> Database {
    let db = Database::open_in_memory().unwrap();
    for row in golden()["stats_seed"].as_array().unwrap() {
        let v: Vec<i64> = row.as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect();
        db.execute(
            "INSERT INTO user_stats(user_id,proposals_submitted,supports_given,supports_received,ballot_entries,winning_proposals) VALUES(?,?,?,?,?,?)",
            rusqlite::params![v[0], v[1], v[2], v[3], v[4], v[5]],
        )
        .unwrap();
        db.execute("INSERT INTO user_streaks(user_id,current_streak,best_streak) VALUES(?,?,?)", rusqlite::params![v[0], v[6], v[6]]).unwrap();
    }
    db
}

fn state() -> OverlayState {
    let live = Arc::new(LiveState::new(PublicUrl::parse(ORIGIN)));
    let db = seeded_db();
    db.set_setting("guild_id", 7).unwrap();
    db.set_setting("live_event_token", TOKEN).unwrap();
    live.start_session(
        session_info(),
        Arc::new(FixedSource { members: vec![member(1, "Zeta"), member(2, "alpha")], levels: HashMap::from([(1, 0.5)]) }),
    );
    OverlayState { live, db, auth_timeout: Duration::from_millis(300) }
}

// --- stato dell'evento ---

#[test]
fn snapshots_match_python() {
    let state = state();
    let golden = golden();
    let snapshots = golden["snapshots"].as_array().unwrap();
    assert_eq!(snapshots.len(), 7);
    for case in snapshots {
        let label = case["label"].as_str().unwrap();
        let members: Vec<VoiceMember> = case["members"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| !m[2].as_bool().unwrap())
            .map(|m| member(m[0].as_u64().unwrap(), m[1].as_str().unwrap()))
            .collect();
        let levels: HashMap<u64, f64> =
            case["levels"].as_object().unwrap().iter().map(|(id, level)| (id.parse().unwrap(), level.as_f64().unwrap())).collect();
        let actual = if label == "nessuna_sessione" {
            state.live.set_session(None);
            let snapshot = current_snapshot(&state).unwrap();
            state.live.set_session(Some(session_info()));
            snapshot
        } else {
            event_snapshot(&state.db, &session_info(), &members, &levels).unwrap()
        };
        assert_eq!(actual, case["snapshot"], "{label}");
    }
}

#[test]
fn volume_rounding_matches_python() {
    for pair in golden()["rounding"].as_array().unwrap() {
        let (value, expected) = (pair[0].as_f64().unwrap(), pair[1].as_f64().unwrap());
        assert_eq!(round3(value), expected, "round({value}, 3)");
    }
}

#[test]
fn token_comparison_is_exact_and_ascii_only() {
    assert!(token_matches("abc-_9", "abc-_9"));
    assert!(!token_matches("abc-_8", "abc-_9"));
    assert!(!token_matches("abc", "abc-_9"));
    assert!(!token_matches("", "abc"));
    assert!(!token_matches("tokén", "token"));
    assert_eq!(supplied_token(r#"{"token":"x"}"#).as_deref(), Some("x"));
    assert_eq!(supplied_token(r#"{"altro":1}"#).as_deref(), Some(""));
    assert_eq!(supplied_token(r#"{"token":123}"#), None);
    for not_an_object in ["non json", "[1,2]", "null", r#""x""#] {
        assert_eq!(supplied_token(not_an_object).as_deref(), Some(""), "{not_an_object}");
    }
}

// --- la pagina ---

async fn get(state: &OverlayState, method: &str, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    let request = axum::http::Request::builder().method(method).uri(uri).body(axum::body::Body::empty()).unwrap();
    let response = routes(state.clone()).oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    (parts.status, parts.headers, axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec())
}

#[tokio::test]
async fn overlay_page_matches_python() {
    let state = state();
    for page in golden()["pages"].as_array().unwrap() {
        let label = page["label"].as_str().unwrap();
        let (method, path) = match label {
            "pagina" => ("GET", "/overlay/7"),
            "pagina_con_tema" => ("GET", "/overlay/7?tema=podcast"),
            "pagina_guild_diverso" => ("GET", "/overlay/8"),
            "pagina_guild_non_numerico" => ("GET", "/overlay/abc"),
            "pagina_con_spazi_e_segno" => ("GET", "/overlay/%20%2B007%20"),
            "pagina_cifre_unicode" => ("GET", "/overlay/%D9%A7"),
            "pagina_head" => ("HEAD", "/overlay/7"),
            "pagina_post" => ("POST", "/overlay/7"),
            other => panic!("caso sconosciuto: {other}"),
        };
        let (status, headers, body) = get(&state, method, path).await;
        let expected = page["headers"].as_object().unwrap();
        assert_eq!(u64::from(status.as_u16()), page["status"].as_u64().unwrap(), "{label}: stato");
        if label == "pagina_post" {
            continue;
        }
        if status != StatusCode::OK {
            assert_eq!(String::from_utf8(body).unwrap(), page["body"].as_str().unwrap(), "{label}: corpo");
            continue;
        }
        for name in ["Content-Type", "Cache-Control", "Content-Security-Policy", "Referrer-Policy", "X-Content-Type-Options"] {
            assert_eq!(headers.get(name).unwrap().to_str().unwrap(), expected[name].as_str().unwrap(), "{label}: {name}");
        }
        if method == "GET" {
            let digest: String = Sha256::digest(&body).iter().map(|byte| format!("{byte:02x}")).collect();
            assert_eq!(digest, page["sha256"].as_str().unwrap(), "{label}: contenuto della pagina");
            assert_eq!(body.len().to_string(), expected["Content-Length"].as_str().unwrap(), "{label}: lunghezza");
        }
    }
}

#[tokio::test]
async fn page_without_a_public_domain_has_an_empty_connect_origin() {
    let state = state();
    let live = Arc::new(LiveState::new(PublicUrl::default()));
    let state = OverlayState { live, ..state };
    let (_, headers, _) = get(&state, "GET", "/overlay/7").await;
    let policy = headers.get("content-security-policy").unwrap().to_str().unwrap();
    assert!(policy.contains("connect-src 'self' ; base-uri 'none'"), "{policy}");
}

async fn serve(state: &OverlayState) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = routes(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

#[tokio::test]
async fn head_request_has_the_length_but_no_body() {
    let address = serve(&state()).await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(b"HEAD /overlay/7 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8(raw).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let expected = golden()["pages"][0]["headers"]["Content-Length"].as_str().unwrap().to_string();
    assert!(head.to_lowercase().contains(&format!("content-length: {expected}")), "{head}");
    assert_eq!(body, "");
}

// --- il websocket ---

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, PartialEq)]
enum Event {
    Text(Value),
    Close(u16, String),
    Dropped,
}

/// `Err((stato, corpo))` quando il server risponde con un errore HTTP invece dell'upgrade.
async fn connect(address: std::net::SocketAddr, path: &str, origin: Option<&str>) -> Result<Client, (u16, String)> {
    let mut request = format!("ws://{address}{path}").into_client_request().unwrap();
    if let Some(origin) = origin {
        request.headers_mut().insert("Origin", origin.parse().unwrap());
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

async fn next_event(client: &mut Client) -> Event {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), client.next()).await.expect("nessun messaggio entro 5 secondi") {
            None | Some(Err(_)) => return Event::Dropped,
            Some(Ok(WsMessage::Text(text))) => return Event::Text(serde_json::from_str(text.as_str()).unwrap()),
            Some(Ok(WsMessage::Close(frame))) => {
                return frame.map_or(Event::Close(1005, String::new()), |frame| Event::Close(u16::from(frame.code), frame.reason.to_string()));
            }
            Some(Ok(_)) => {}
        }
    }
}

/// Aspetta che una condizione diventi vera (i contatori si aggiornano dopo la chiusura).
async fn eventually(what: &str, condition: impl Fn() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condizione non raggiunta: {what}");
}

fn token_message(token: &str) -> WsMessage {
    WsMessage::text(json!({ "token": token }).to_string())
}

#[tokio::test]
async fn websocket_scenarios_match_python() {
    let golden = golden();
    for case in golden["ws"].as_array().unwrap() {
        let label = case["label"].as_str().unwrap();
        let state = state();
        let address = serve(&state).await;
        let snapshot = current_snapshot(&state).unwrap();
        let mut path = "/ws/7";
        let mut origin = Some(ORIGIN);
        let mut first: Option<WsMessage> = Some(token_message(TOKEN));
        let mut slots = Vec::new();
        let mut expected = case.get("events").map(|events| {
            let event = &events[0];
            match event[0].as_str().unwrap() {
                "text" => Event::Text(event[1].clone()),
                "close" => Event::Close(event[1].as_u64().unwrap() as u16, event[2].as_str().unwrap().to_string()),
                _ => Event::Dropped,
            }
        });
        match label {
            "nessuna_sessione" => state.live.set_session(None),
            "guild_diverso" => path = "/ws/8",
            "guild_non_numerico" => path = "/ws/abc",
            "origine_mancante" => origin = None,
            "origine_sbagliata" => origin = Some("https://altro.example.it"),
            "origine_http" => origin = Some("http://overlay.example.it"),
            "troppi_client" => slots = (0..MAX_OVERLAY_CLIENTS).map(|_| state.live.hub.try_open(MAX_OVERLAY_CLIENTS).unwrap()).collect(),
            "ultimo_posto_libero" => slots = (0..MAX_OVERLAY_CLIENTS - 1).map(|_| state.live.hub.try_open(MAX_OVERLAY_CLIENTS).unwrap()).collect(),
            "token_valido" => {}
            "token_valido_guild_con_segno" => path = "/ws/+7",
            "token_sbagliato" => first = Some(token_message("sbagliato")),
            "token_vuoto" => first = Some(token_message("")),
            "token_non_stringa" => first = Some(WsMessage::text(r#"{"token":123}"#)),
            "token_mancante" => first = Some(WsMessage::text(r#"{"altro":1}"#)),
            "json_non_valido" => first = Some(WsMessage::text("non json")),
            "json_lista" => first = Some(WsMessage::text("[1,2]")),
            "json_null" => first = Some(WsMessage::text("null")),
            "json_stringa" => first = Some(WsMessage::text(r#""x""#)),
            "primo_messaggio_binario" => first = Some(WsMessage::binary(vec![0u8, 1])),
            "messaggio_troppo_grande" => first = Some(token_message(&"x".repeat(2000))),
            // Python solleva un TypeError e lascia cadere la connessione (1006): qui il rifiuto
            // è pulito, con lo stesso codice di ogni altra chiave sbagliata.
            "token_con_caratteri_non_ascii" => {
                first = Some(token_message("tokén"));
                expected = Some(Event::Close(1008, "Token non valido".into()));
            }
            // Python annulla la lettura e la connessione cade (1006) senza il messaggio
            // previsto dal codice; qui il messaggio arriva.
            "autenticazione_scaduta" => {
                first = None;
                expected = Some(Event::Close(1008, "Autenticazione scaduta".into()));
            }
            other => panic!("caso sconosciuto: {other}"),
        }
        let status = case["status"].as_u64().unwrap();
        let outcome = connect(address, path, origin).await;
        if status != 101 {
            let (code, body) = outcome.err().unwrap_or_else(|| panic!("{label}: il collegamento doveva essere rifiutato"));
            assert_eq!(u64::from(code), status, "{label}: stato");
            assert_eq!(body, case["http_body"].as_str().unwrap(), "{label}: corpo");
            continue;
        }
        let mut client = outcome.unwrap_or_else(|refused| panic!("{label}: rifiutato {refused:?}"));
        if let Some(message) = first {
            client.send(message).await.unwrap();
        }
        let event = next_event(&mut client).await;
        if let Some(Event::Text(_)) = &expected {
            assert_eq!(event, Event::Text(snapshot.clone()), "{label}: stato iniziale");
            assert_eq!(state.live.hub.client_count(), 1, "{label}: collegamento registrato");
        } else {
            assert_eq!(event, expected.unwrap(), "{label}");
            assert_eq!(state.live.hub.client_count(), 0, "{label}: non registrato");
        }
        drop(client);
        eventually(label, || state.live.hub.client_count() == 0 && state.live.hub.open_connections() == slots.len()).await;
    }
}

#[tokio::test]
async fn a_client_that_sends_a_close_frame_first_is_released() {
    let state = state();
    let address = serve(&state).await;
    let mut client = connect(address, "/ws/7", Some(ORIGIN)).await.unwrap();
    client.send(WsMessage::Close(None)).await.unwrap();
    drop(client);
    eventually("rilascio", || state.live.hub.open_connections() == 0 && state.live.hub.client_count() == 0).await;
}

#[tokio::test]
async fn state_is_published_to_every_client_and_the_loop_survives_an_empty_room() {
    let state = state();
    let address = serve(&state).await;
    let mut clients = Vec::new();
    for _ in 0..2 {
        let mut client = connect(address, "/ws/7", Some(ORIGIN)).await.unwrap();
        client.send(token_message(TOKEN)).await.unwrap();
        assert!(matches!(next_event(&mut client).await, Event::Text(_)));
        clients.push(client);
    }
    eventually("due client", || state.live.hub.client_count() == 2).await;
    assert_eq!(state.live.hub.open_connections(), 2);

    // Qualcuno parla: lo stato pubblicato cambia per tutti.
    state.live.start_session(
        session_info(),
        Arc::new(FixedSource { members: vec![member(1, "Zeta"), member(2, "alpha")], levels: HashMap::from([(2, 0.25)]) }),
    );
    publish_once(&state).unwrap();
    let expected = current_snapshot(&state).unwrap();
    assert_eq!(expected["main_speaker_id"], json!(2));
    for client in &mut clients {
        assert_eq!(next_event(client).await, Event::Text(expected.clone()));
    }

    // Senza evento non c'è niente da pubblicare; la revoca della chiave chiude tutti.
    state.live.set_session(None);
    publish_once(&state).unwrap();
    state.live.hub.close_all(1008, "Link overlay revocato");
    for client in &mut clients {
        assert_eq!(next_event(client).await, Event::Close(1008, "Link overlay revocato".into()));
    }
    eventually("svuotato", || state.live.hub.client_count() == 0 && state.live.hub.open_connections() == 0).await;
    publish_once(&state).unwrap();
}

#[tokio::test]
async fn ending_the_event_notifies_and_closes_the_clients() {
    let state = state();
    let address = serve(&state).await;
    let mut client = connect(address, "/ws/7", Some(ORIGIN)).await.unwrap();
    client.send(token_message(TOKEN)).await.unwrap();
    next_event(&mut client).await;
    eventually("registrato", || state.live.hub.client_count() == 1).await;
    state.live.hub.event_ended();
    assert_eq!(next_event(&mut client).await, Event::Text(json!({"type": "status", "active": false})));
    assert_eq!(next_event(&mut client).await, Event::Close(1000, "Evento terminato".into()));
    eventually("chiuso", || state.live.hub.open_connections() == 0).await;
}

#[tokio::test]
async fn a_new_token_locks_out_the_old_link() {
    let state = state();
    let address = serve(&state).await;
    state.db.set_setting("live_event_token", "nuova-chiave").unwrap();
    let mut stale = connect(address, "/ws/7", Some(ORIGIN)).await.unwrap();
    stale.send(token_message(TOKEN)).await.unwrap();
    assert_eq!(next_event(&mut stale).await, Event::Close(1008, "Token non valido".into()));
    let mut fresh = connect(address, "/ws/7", Some(ORIGIN)).await.unwrap();
    fresh.send(token_message("nuova-chiave")).await.unwrap();
    assert!(matches!(next_event(&mut fresh).await, Event::Text(_)));
}

#[test]
fn a_slow_client_is_dropped_instead_of_blocking_the_others() {
    let hub = Arc::new(OverlayHub::default());
    let (slow, mut slow_inbox) = mpsc::channel(1);
    let (fast, mut fast_inbox) = mpsc::channel(8);
    let _slow = hub.register(slow);
    let _fast = hub.register(fast);
    hub.broadcast("uno");
    assert_eq!(hub.client_count(), 2);
    hub.broadcast("due");
    assert_eq!(hub.client_count(), 1, "la coda del lento è piena");
    assert_eq!(slow_inbox.try_recv().unwrap(), Outgoing::Text("uno".into()));
    assert!(slow_inbox.try_recv().is_err());
    assert_eq!(fast_inbox.try_recv().unwrap(), Outgoing::Text("uno".into()));
    assert_eq!(fast_inbox.try_recv().unwrap(), Outgoing::Text("due".into()));
}

#[test]
fn connection_slots_are_limited_and_released() {
    let hub = Arc::new(OverlayHub::default());
    let slots: Vec<_> = (0..3).map(|_| hub.try_open(3).unwrap()).collect();
    assert!(hub.try_open(3).is_none());
    assert_eq!(hub.open_connections(), 3);
    drop(slots);
    assert_eq!(hub.open_connections(), 0);
    assert!(hub.try_open(3).is_some());
}
