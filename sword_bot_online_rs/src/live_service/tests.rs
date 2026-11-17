use std::cell::RefCell;

use chrono::TimeZone;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::*;
use crate::config::TZ;
use crate::discord_api::testing::FakeApi;
use crate::notify::testing::FakeSender;
use crate::overlay::PublicUrl;
use crate::overlay_web::{Outgoing, Registration};
use crate::publisher::Publisher;
use crate::voice::fake::FakeGateway;

type TestService = LiveService<FakeSender, FakeApi, FakeGateway>;

fn golden() -> Value {
    serde_json::from_str(include_str!("../golden/live_service_expected.json")).unwrap()
}

// --- orologi finti (uno per test: i test girano ciascuno sul proprio thread) -------------------

thread_local! {
    static WALL: RefCell<Option<DateTime<Tz>>> = const { RefCell::new(None) };
    static MONO: RefCell<Option<Instant>> = const { RefCell::new(None) };
}

fn fake_wall() -> DateTime<Tz> {
    WALL.with(|wall| wall.borrow().expect("orologio non impostato"))
}

fn fake_mono() -> Instant {
    MONO.with(|mono| mono.borrow().expect("orologio monotono non impostato"))
}

fn at(hour: u32, minute: u32, second: u32) -> DateTime<Tz> {
    TZ.with_ymd_and_hms(2027, 3, 5, hour, minute, second).unwrap()
}

fn set_wall(value: DateTime<Tz>) {
    WALL.with(|wall| *wall.borrow_mut() = Some(value));
}

fn set_mono(value: Instant) {
    MONO.with(|mono| *mono.borrow_mut() = Some(value));
}

// --- ambiente ----------------------------------------------------------------------------------

struct Env {
    service: Arc<TestService>,
    api: FakeApi,
    gateway: FakeGateway,
    db: Database,
    live: Arc<LiveState>,
    base: Instant,
}

struct Options {
    public_url: &'static str,
    overlay_running: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { public_url: "https://overlay.example.it", overlay_running: true }
    }
}

fn env() -> Env {
    env_with(Options::default())
}

fn env_with(options: Options) -> Env {
    set_wall(at(21, 0, 0));
    let base = Instant::now();
    set_mono(base);
    let db = Database::open_in_memory().unwrap();
    let api = FakeApi::default();
    let notifier = Notifier::new(db.clone(), FakeSender::default());
    let publisher = Arc::new(Publisher::new(api.clone(), db.clone()));
    let gateway = FakeGateway::default();
    gateway.add_channel(1, 555, "Sala Eventi", true);
    gateway.set_members(&[10, 11]);
    api.set_channel_kind(555, ChannelKind::Voice);
    api.set_channel_kind(556, ChannelKind::Stage);
    let live = Arc::new(LiveState::new(PublicUrl::parse(options.public_url)).with_token_generator(|| "token-fisso".to_string()));
    let service = Arc::new(LiveService::new(db.clone(), notifier, publisher, gateway.clone(), live.clone(), options.overlay_running).with_clocks(fake_wall, fake_mono));
    Env { service, api, gateway, db, live, base }
}

impl Env {
    fn request(&self, actor: Option<u64>, scheduled: Option<u64>, title: &str) -> StartRequest {
        StartRequest { guild_id: 1, channel_id: 555, title: title.to_string(), scheduled_event_id: scheduled, actor }
    }

    async fn start_manual(&self) -> StartOutcome {
        self.service.start_event(self.request(Some(42), None, DEFAULT_TITLE)).await
    }

    fn staff_channel(&self, kind: ChannelKind) {
        self.db.set_setting("staff_channel", 600).unwrap();
        self.api.set_channel_kind(600, kind);
    }

    fn staff_messages(&self) -> Vec<(Option<String>, Option<String>)> {
        self.api
            .messages_in(600)
            .into_iter()
            .map(|message| (message.content, message.embeds.first().and_then(|embed| embed["title"].as_str()).map(str::to_string)))
            .collect()
    }

    /// Un collegamento overlay finto: tutto quello che il servizio gli manda.
    fn overlay_client(&self) -> (Registration, mpsc::Receiver<Outgoing>) {
        let (sender, receiver) = mpsc::channel(512);
        (self.live.hub.register(sender), receiver)
    }
}

fn table(db: &Database, sql: &str) -> Value {
    db.with_conn(|c| {
        let mut statement = c.prepare(sql).unwrap();
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                Ok((0..columns)
                    .map(|i| match row.get_ref(i).unwrap() {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(n) => json!(n),
                        rusqlite::types::ValueRef::Real(f) => json!(f),
                        rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                        rusqlite::types::ValueRef::Blob(_) => Value::Null,
                    })
                    .collect::<Vec<_>>())
            })
            .unwrap();
        Value::Array(rows.map(|row| Value::Array(row.unwrap())).collect())
    })
}

fn dump(db: &Database) -> Value {
    json!({
        "events": table(db, "SELECT id,guild_id,scheduled_event_id,title,channel_id,channel_name,started_at,last_seen_at,ended_at,peak_participants,participant_count,end_reason FROM live_events ORDER BY id"),
        "participants": table(db, "SELECT event_id,user_id,joined_at,participation_seconds,speaking_seconds FROM live_event_participants ORDER BY rowid"),
        "activity": table(db, "SELECT action,occurred_at FROM community_activity WHERE action='event_join' ORDER BY id"),
        "audit": table(db, "SELECT actor_id,action,entity_type,entity_id,reason,details FROM staff_audit ORDER BY id"),
        "guild_setting": table(db, "SELECT value FROM settings WHERE key='guild_id'"),
    })
}

/// I numeri decimali si confrontano con una tolleranza: Python somma secondi monotoni grandi
/// (~1000 s) e accumula rumore dell'ordine di 1e-13.
fn assert_close(actual: &Value, expected: &Value, what: &str) {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) if a.is_f64() || b.is_f64() => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            assert!((a - b).abs() < 1e-9, "{what}: {a} contro {b}");
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{what}: lunghezza");
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                assert_close(x, y, &format!("{what}[{index}]"));
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(a.keys().collect::<std::collections::BTreeSet<_>>(), b.keys().collect::<std::collections::BTreeSet<_>>(), "{what}: chiavi");
            for (key, x) in a {
                assert_close(x, &b[key], &format!("{what}.{key}"));
            }
        }
        _ => assert_eq!(actual, expected, "{what}"),
    }
}

fn golden_sends(case: &Value) -> Vec<(Option<String>, Option<String>)> {
    case["sends"]
        .as_array()
        .map(|sends| {
            sends
                .iter()
                .map(|send| match send {
                    // Gli avvii programmati registrano solo il testo; i report anche l'embed.
                    Value::String(text) => (Some(text.clone()), None),
                    other => (other["content"].as_str().map(str::to_string), other["embed"]["title"].as_str().map(str::to_string)),
                })
                .collect()
        })
        .unwrap_or_default()
}

// --- avvio -------------------------------------------------------------------------------------

#[tokio::test]
async fn start_events_match_the_python_implementation() {
    let golden = golden();
    let cases = golden["start"].as_array().unwrap();
    assert_eq!(cases.len(), 21);
    for case in cases {
        let label = case["label"].as_str().unwrap();
        let interactive = !case["out"].is_null();
        let options = match label {
            "non_configurato_senza_dominio" | "non_configurato_automatico" => Options { public_url: "", ..Options::default() },
            "non_configurato_errore" => Options { public_url: "http://x", ..Options::default() },
            "non_configurato_senza_server" => Options { overlay_running: false, ..Options::default() },
            _ => Options::default(),
        };
        let e = env_with(options);
        let (scheduled, title) = match label {
            "gia_attivo_automatico_altro_evento" => (Some(901), DEFAULT_TITLE.to_string()),
            "avvio_automatico_staff_ok" => (Some(900), "Torneo Among Us".to_string()),
            "avvio_manuale_titolo_lungo" => (None, "T".repeat(150)),
            l if l.ends_with("_automatico") || l.starts_with("gia_attivo_automatico") || l == "gia_attivo_manuale_con_id_uguale" => (Some(900), DEFAULT_TITLE.to_string()),
            _ => (None, DEFAULT_TITLE.to_string()),
        };
        match label {
            l if l.starts_with("gia_attivo") => {
                let first = e.service.start_event(e.request(None, Some(900), DEFAULT_TITLE)).await;
                assert!(matches!(first, StartOutcome::Started { .. }), "{label}: {first:?}");
                e.gateway.clear_calls();
            }
            l if l.starts_with("senza_membro_bot") => e.gateway.set_bot_in_guild(false),
            l if l.starts_with("senza_permesso_connect") => e.gateway.add_channel(1, 555, "Sala Eventi", false),
            l if l.starts_with("connessione_fallita") => e.gateway.fail_connect(VoiceError::Failed("rete".into())),
            "ascolto_fallito" => e.gateway.fail_listen(VoiceError::Failed("opus".into())),
            l if l.starts_with("report_non_creato") => {
                e.db.execute_batch("CREATE TRIGGER fail_insert BEFORE INSERT ON live_events BEGIN SELECT RAISE(ABORT,'boom'); END").unwrap();
            }
            "avvio_manuale_canale_vuoto" => e.gateway.set_members(&[]),
            "avvio_automatico_staff_ok" => e.staff_channel(ChannelKind::Text),
            _ => {}
        }
        let outcome = e.service.start_event(e.request(interactive.then_some(42), scheduled, &title)).await;

        let succeeded = matches!(outcome, StartOutcome::Started { .. } | StartOutcome::AlreadyRunning);
        assert_eq!(succeeded, case["result"].as_bool().unwrap(), "{label}: esito {outcome:?}");
        if let Some(followup) = case["out"].as_array().and_then(|out| out.iter().find(|entry| entry[0] == "followup")) {
            assert_eq!(outcome.message(555), followup[1].as_str().unwrap(), "{label}: messaggio");
        }
        assert_eq!(e.gateway.calls(), case["calls"].as_array().unwrap().iter().map(|call| call.as_str().unwrap().to_string()).collect::<Vec<_>>(), "{label}: chiamate alla voce");
        assert_eq!(e.service.is_active(), !case["session"].is_null(), "{label}: sessione");
        if !label.starts_with("gia_attivo") {
            assert_eq!(dump(&e.db), case["db"], "{label}: database");
        }
        assert_eq!(e.staff_messages(), golden_sends(case), "{label}: messaggi nel canale staff");
        // L'overlay vede la sessione solo se è partita davvero.
        assert_eq!(e.live.session().is_some(), !case["session"].is_null(), "{label}: stato condiviso");
    }
}

#[tokio::test]
async fn staff_is_told_about_automatic_starts_like_python() {
    let golden = golden();
    for case in golden["notify"].as_array().unwrap() {
        let label = case["label"].as_str().unwrap();
        let e = env();
        e.gateway.set_members(&[10]);
        match label {
            "nessun_canale_staff" => {}
            "id_non_valido" => e.db.set_setting("staff_channel", "abc").unwrap(),
            "id_con_spazi" => {
                e.db.set_setting("staff_channel", " 600 ").unwrap();
                e.api.set_channel_kind(600, ChannelKind::Text);
            }
            // Canale che non esiste o non si può recuperare: per Discord è un errore, per noi un canale non testuale.
            "canale_non_trovato" | "recupero_vietato" | "recupero_http" => e.db.set_setting("staff_channel", 600).unwrap(),
            "recuperato_ok" | "invio_ok" => e.staff_channel(ChannelKind::Text),
            "canale_vocale" => e.staff_channel(ChannelKind::Voice),
            "invio_vietato" => {
                e.staff_channel(ChannelKind::Text);
                e.api.fail_send(Some(crate::discord_api::ApiError::Forbidden));
            }
            other => panic!("caso sconosciuto: {other}"),
        }
        let outcome = e.service.start_event(e.request(None, Some(900), "Serata *Giochi*")).await;
        assert!(matches!(outcome, StartOutcome::Started { .. }), "{label}");
        assert_eq!(e.staff_messages(), golden_sends(case), "{label}: messaggi");
        assert_eq!(dump(&e.db)["audit"], case["audit"], "{label}: registro audit");
    }
}

// --- fine --------------------------------------------------------------------------------------

#[tokio::test]
async fn finishing_an_event_matches_the_python_implementation() {
    let golden = golden();
    let cases = golden["finish"].as_array().unwrap();
    assert_eq!(cases.len(), 17);
    for case in cases {
        let label = case["label"].as_str().unwrap();
        let e = env();
        e.gateway.set_members(&[10, 11]);
        if label == "fine_senza_sessione" {
            assert_eq!(e.service.finish_event("manuale").await, None);
            assert_eq!(dump(&e.db), case["db"]);
            continue;
        }
        assert!(matches!(e.start_manual().await, StartOutcome::Started { .. }), "{label}");
        let connection = e.gateway.connection();
        let mut reason = "manuale";
        let mut clients = Vec::new();
        match label {
            "fine_con_parlato" => lock(&e.service.session).as_mut().unwrap().speaking = vec![(10, 125.5), (12, 30.25)],
            "fine_con_picco_maggiore" => connection.set_members(&[10, 11, 12, 13, 14]),
            "fine_con_overlay" | "fine_con_errore_database" => clients = (0..if label == "fine_con_overlay" { 2 } else { 1 }).map(|_| e.overlay_client()).collect(),
            "fine_disconnessione" => reason = "disconnessione",
            "fine_errore_disconnessione" => *connection.disconnect_error.lock().unwrap() = Some(VoiceError::Failed("http".into())),
            "fine_arresto_bot" => reason = "arresto_bot",
            "fine_con_report_nel_canale_staff" => {
                e.staff_channel(ChannelKind::Text);
                lock(&e.service.session).as_mut().unwrap().speaking = vec![(10, 90.0)];
            }
            "fine_con_report_automatico" => {
                e.staff_channel(ChannelKind::Text);
                reason = "completato";
            }
            "report_canale_mancante" | "report_recupero_vietato" | "report_canale_vocale" => {
                e.db.set_setting("staff_channel", 600).unwrap();
                if label == "report_canale_vocale" {
                    e.api.set_channel_kind(600, ChannelKind::Voice);
                }
            }
            "report_id_non_valido" => e.db.set_setting("staff_channel", "abc").unwrap(),
            "report_invio_vietato" => {
                e.staff_channel(ChannelKind::Text);
                e.api.fail_send(Some(crate::discord_api::ApiError::Forbidden));
            }
            "fine_manuale" | "report_senza_canale_staff" => {}
            other => panic!("caso sconosciuto: {other}"),
        }
        if label == "fine_con_errore_database" {
            e.db.execute_batch("CREATE TRIGGER fail_finish BEFORE UPDATE OF ended_at ON live_events BEGIN SELECT RAISE(ABORT,'boom'); END").unwrap();
        }
        e.gateway.clear_calls();
        set_wall(if label == "fine_arresto_bot" { at(21, 0, 0) } else { at(21, 40, 0) });

        let result = e.service.finish_event(reason).await;

        assert_eq!(result, case["result"].as_i64(), "{label}");
        assert_eq!(e.gateway.calls(), case["calls"].as_array().unwrap().iter().map(|call| call.as_str().unwrap().to_string()).collect::<Vec<_>>(), "{label}: chiamate alla voce");
        assert!(!e.service.is_active() && e.live.session().is_none(), "{label}: sessione chiusa");
        assert_eq!(dump(&e.db), case["db"], "{label}: database");
        assert_eq!(e.staff_messages(), golden_sends(case), "{label}: report nel canale staff");
        assert_eq!(e.live.hub.client_count(), case["clients_left"].as_u64().unwrap() as usize, "{label}: overlay rimasti");
        // Gli overlay collegati ricevono «evento terminato» e la chiusura.
        let expected_clients = case["clients"].as_array().unwrap();
        assert_eq!(clients.len(), expected_clients.len(), "{label}");
        for (_registration, mut inbox) in clients {
            assert_eq!(inbox.try_recv().unwrap(), Outgoing::Text(json!({"type": "status", "active": false}).to_string()), "{label}");
            assert_eq!(inbox.try_recv().unwrap(), Outgoing::Close(1000, "Evento terminato"), "{label}");
        }
    }
}

// --- presenze e serata intera ------------------------------------------------------------------

#[tokio::test]
async fn a_whole_evening_goes_through_the_service_like_python() {
    let live_core: Value = serde_json::from_str(include_str!("../golden/live_core_expected.json")).unwrap();
    let steps = live_core["evening"].as_array().unwrap();
    let step = |name: &str| steps.iter().find(|s| s[0] == name).unwrap_or_else(|| panic!("passo {name}"));
    let e = env();
    e.gateway.add_channel(1, 555, "vocale", true);
    e.gateway.set_members(&[10, 11, 12]);
    assert!(matches!(e.start_manual().await, StartOutcome::Started { event_id: 1, .. }));
    let connection = e.gateway.connection();
    let service = &e.service;

    set_wall(at(21, 10, 30));
    connection.set_members(&[10, 12]);
    service.on_voice_state_update(1, 11, false, Some(555), None);
    set_wall(at(21, 20, 0));
    connection.set_members(&[10, 12, 13]);
    service.on_voice_state_update(1, 13, false, None, Some(555));
    set_wall(at(21, 25, 0));
    connection.set_members(&[10, 12, 13, 14]);
    service.on_voice_state_update(1, 14, false, None, Some(555));
    set_wall(at(21, 30, 0));
    connection.set_members(&[10, 12, 13, 14, 11]);
    service.on_voice_state_update(1, 11, false, None, Some(555));
    // Ignorati: un bot, un altro server, nessun cambio di canale, un altro canale.
    service.on_voice_state_update(1, 98, true, None, Some(555));
    service.on_voice_state_update(2, 77, false, None, Some(555));
    service.on_voice_state_update(1, 10, false, Some(555), Some(555));
    service.on_voice_state_update(1, 15, false, Some(600), Some(601));
    let after_presences = step("dopo_presenze");
    assert_eq!(dump(&e.db)["participants"], after_presences[1]["participants"]);
    assert_eq!(dump(&e.db)["activity"], after_presences[1]["activity"]);
    assert_eq!(lock(&service.session).as_ref().unwrap().peak, after_presences[2].as_i64().unwrap());

    // Quindici secondi dopo l'ultimo salvataggio il ciclo salva il parlato maturato.
    lock(&service.session).as_mut().unwrap().speaking = vec![(10, 125.5), (12, 30.25)];
    set_wall(at(21, 40, 0));
    service.tick(e.base + Duration::from_secs(16), at(21, 40, 0)).await.unwrap();
    let after_progress = step("dopo_progresso");
    assert_eq!(dump(&e.db)["participants"], after_progress[1]["participants"]);
    assert_eq!(dump(&e.db)["events"][0][7], after_progress[1]["events"][0][7], "ultimo istante visto");

    // Fine alle 22:30:10 con tre persone presenti.
    lock(&service.session).as_mut().unwrap().speaking = vec![(10, 140.0), (12, 30.25), (15, 3.0)];
    connection.set_members(&[10, 12, 13]);
    set_wall(at(22, 30, 10));
    // Un secondo avvio mentre l'evento è in corso viene rifiutato.
    assert_eq!(e.start_manual().await, StartOutcome::AlreadyActive);
    assert_eq!(service.finish_event("manuale").await, Some(1));
    let finished = step("fine");
    let db = dump(&e.db);
    assert_eq!(db["participants"], finished[2]["participants"]);
    assert_eq!(db["events"], finished[2]["events"]);
    assert_eq!(db["activity"], finished[2]["activity"]);
    assert_eq!(service.finish_event("manuale").await, None);
}

// --- ciclo di pubblicazione --------------------------------------------------------------------

#[tokio::test]
async fn the_publish_loop_counts_speech_and_watches_the_connection_like_python() {
    let section = golden()["loop"].clone();
    let e = env();
    e.gateway.set_members(&[10, 11, 12]);
    for id in [10, 11, 12] {
        e.db.execute("INSERT OR REPLACE INTO user_stats(user_id) VALUES(?)", [id]).unwrap();
    }
    assert!(matches!(e.start_manual().await, StartOutcome::Started { .. }));
    lock(&e.service.session).as_mut().unwrap().audio_recovery_attempts = 3;
    let (_a, mut inbox_a) = e.overlay_client();
    let (_b, mut inbox_b) = e.overlay_client();
    let connection = e.gateway.connection();

    // (secondi trascorsi, cosa succede prima del passo): gli stessi passi del golden.
    type Hook = Box<dyn Fn(&crate::voice::fake::FakeConnection, Instant)>;
    let levels = |values: &'static [(u64, f64)], packet: bool| -> Hook {
        Box::new(move |connection, base| {
            connection.set_levels(values);
            if packet {
                *connection.last_packet.lock().unwrap() = Some(base + Duration::from_millis(100));
            }
        })
    };
    let script: Vec<(f64, Hook)> = vec![
        (0.25, levels(&[(10, 0.5)], true)),
        (0.25, levels(&[(10, 0.5), (11, 0.2)], false)),
        (0.25, levels(&[], false)),
        (3.0, levels(&[(12, 0.9)], false)),
        (0.0, levels(&[(12, 0.9)], false)),
        (0.25, levels(&[(10, 1.4)], false)),
        (10.0, levels(&[(10, 0.1)], false)),
        (1.0, levels(&[(10, 0.1)], false)),
        (4.0, levels(&[(11, 0.3)], false)),
        (0.25, levels(&[(11, 0.3)], false)),
        (0.25, Box::new(|c, _| c.connected.store(false, std::sync::atomic::Ordering::SeqCst))),
        (5.0, Box::new(|_, _| {})),
        (4.9, Box::new(|c, _| c.connected.store(true, std::sync::atomic::Ordering::SeqCst))),
        (0.25, Box::new(|c, _| c.connected.store(false, std::sync::atomic::Ordering::SeqCst))),
        (9.0, Box::new(|_, _| {})),
        (0.9, Box::new(|_, _| {})),
        (0.2, Box::new(|_, _| {})),
    ];
    let steps = section["steps"].as_array().unwrap();
    assert_eq!(steps.len(), script.len());
    let (mut sent_a, mut sent_b) = (0usize, 0usize);
    let mut last_a: Option<Value> = None;
    let mut failures = 0u32;
    let (mut elapsed_micros, start) = (0i64, at(21, 0, 0));
    for ((seconds, hook), expected) in script.iter().zip(steps) {
        let label = expected["after"].as_str().unwrap();
        elapsed_micros += (seconds * 1e6).round() as i64;
        hook(&connection, e.base);
        let wall = start + chrono::Duration::microseconds(elapsed_micros);
        set_wall(wall);
        e.service.loop_step(&mut failures, e.base + Duration::from_micros(elapsed_micros as u64), wall).await;

        while let Ok(message) = inbox_a.try_recv() {
            sent_a += 1;
            if let Outgoing::Text(text) = message {
                last_a = serde_json::from_str(&text).ok();
            }
        }
        while inbox_b.try_recv().is_ok() {
            sent_b += 1;
        }
        assert_eq!(json!([sent_a, sent_b]), expected["payloads"], "{label}: messaggi inviati");
        let guard = lock(&e.service.session);
        match guard.as_ref() {
            Some(session) => {
                let speaking: serde_json::Map<String, Value> = session.speaking.iter().map(|(id, seconds)| (id.to_string(), json!(seconds))).collect();
                assert_close(&Value::Object(speaking), &expected["session"]["speaking_seconds"], &format!("{label}: parlato"));
                let persisted: serde_json::Map<String, Value> = session.persisted.iter().map(|(id, seconds)| (id.to_string(), json!(seconds))).collect();
                assert_close(&Value::Object(persisted), &expected["persisted"], &format!("{label}: parlato salvato"));
                assert_eq!(session.disconnected_since.is_some(), expected["disconnected"].as_bool().unwrap(), "{label}: scollegato");
                assert_eq!(u64::from(session.audio_recovery_attempts), expected["session"]["audio_recovery_attempts"].as_u64().unwrap(), "{label}: tentativi audio");
            }
            None => assert!(expected["session"].is_null(), "{label}: l'evento doveva essere ancora attivo"),
        }
        drop(guard);
        assert_close(&table(&e.db, "SELECT id,last_seen_at,ended_at,end_reason FROM live_events"), &expected["events"], &format!("{label}: eventi"));
        assert_close(&table(&e.db, "SELECT event_id,user_id,speaking_seconds FROM live_event_participants ORDER BY user_id"), &expected["participants"], &format!("{label}: partecipanti"));
        if let Some(payload) = expected["last_payload"].as_object().filter(|payload| payload.get("type") == Some(&json!("event"))) {
            assert_close(last_a.as_ref().expect("nessuno stato pubblicato"), &Value::Object(payload.clone()), &format!("{label}: stato pubblicato"));
        }
    }
    assert!(!e.service.is_active());
    assert_eq!(dump(&e.db)["events"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn forty_failures_in_a_row_end_the_event_like_python() {
    let section = golden()["loop"]["errors"].clone();
    let e = env();
    e.gateway.set_members(&[10, 11]);
    assert!(matches!(e.start_manual().await, StartOutcome::Started { .. }));
    let (_client, mut inbox) = e.overlay_client();
    e.db.execute_batch("ALTER TABLE user_stats RENAME TO user_stats_off").unwrap();
    let steps = section["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 45);
    let mut failures = 0u32;
    let mut sent = 0usize;
    for (index, expected) in steps.iter().enumerate() {
        let tick = index as u64 + 1;
        let wall = at(21, 0, 0) + chrono::Duration::milliseconds(250 * tick as i64);
        set_wall(wall);
        e.service.loop_step(&mut failures, e.base + Duration::from_millis(250 * tick), wall).await;
        while inbox.try_recv().is_ok() {
            sent += 1;
        }
        assert_eq!(e.service.is_active(), expected["active"].as_bool().unwrap(), "dopo {tick} passi");
        assert_eq!(sent as u64, expected["payloads"].as_u64().unwrap(), "dopo {tick} passi: messaggi");
        assert_eq!(table(&e.db, "SELECT end_reason FROM live_events"), expected["events"], "dopo {tick} passi: motivo");
    }
    e.db.execute_batch("ALTER TABLE user_stats_off RENAME TO user_stats").unwrap();
    assert_close(&dump(&e.db)["events"], &section["final"]["db"]["events"], "evento chiuso per errori");
}

#[tokio::test]
async fn an_event_without_overlay_clients_publishes_nothing_but_still_counts() {
    let e = env();
    assert!(matches!(e.start_manual().await, StartOutcome::Started { .. }));
    e.gateway.connection().set_levels(&[(10, 0.4)]);
    let mut failures = 0;
    e.service.loop_step(&mut failures, e.base + Duration::from_millis(250), at(21, 0, 0)).await;
    assert_eq!(lock(&e.service.session).as_ref().unwrap().speaking, vec![(10, 0.25)]);
    // Senza evento il ciclo non fa nulla.
    e.service.finish_event("manuale").await;
    e.service.loop_step(&mut failures, e.base + Duration::from_secs(1), at(21, 0, 1)).await;
    assert_eq!(failures, 0);
}

// --- ascolto audio -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_audio_listener_is_restarted_with_a_growing_delay_like_python() {
    let cases = golden()["recovery"].as_array().unwrap().clone();
    let e = env();
    assert!(matches!(e.start_manual().await, StartOutcome::Started { .. }));
    let connection = e.gateway.connection();
    let as_dyn: Arc<dyn VoiceConnection> = connection.clone();
    connection.listening.store(false, std::sync::atomic::Ordering::SeqCst);
    for case in &cases {
        let label = case["label"].as_str().unwrap();
        match label {
            l if l.starts_with("tentativo_") => lock(&e.service.session).as_mut().unwrap().audio_recovery_attempts = l[10..].parse().unwrap(),
            "gia_in_ascolto" => connection.listening.store(true, std::sync::atomic::Ordering::SeqCst),
            "non_connesso" => {
                connection.listening.store(false, std::sync::atomic::Ordering::SeqCst);
                connection.connected.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            "ascolto_fallito" => {
                connection.connected.store(true, std::sync::atomic::Ordering::SeqCst);
                *connection.restart_error.lock().unwrap() = Some(VoiceError::Failed("opus".into()));
            }
            "altro_client" | "senza_sessione" => {}
            other => panic!("caso sconosciuto: {other}"),
        }
        e.gateway.clear_calls();
        let started = Instant::now();
        match label {
            "altro_client" => e.service.recover_audio_listener(e.gateway.detached_connection()).await,
            "senza_sessione" => {
                e.service.finish_event("manuale").await;
                e.service.recover_audio_listener(as_dyn.clone()).await;
            }
            _ => e.service.recover_audio_listener(as_dyn.clone()).await,
        }
        let slept = started.elapsed();
        match case.get("sleep").and_then(Value::as_u64) {
            Some(seconds) => assert_eq!(slept, Duration::from_secs(seconds), "{label}: attesa"),
            None if label == "gia_in_ascolto" || label == "non_connesso" || label == "ascolto_fallito" => {}
            None => assert_eq!(slept, Duration::ZERO, "{label}: nessuna attesa"),
        }
        if let Some(calls) = case["calls"].as_array() {
            assert_eq!(e.gateway.calls(), calls.iter().map(|call| call.as_str().unwrap().to_string()).collect::<Vec<_>>(), "{label}: chiamate");
        }
        if let (Some(attempts), true) = (case["attempts"].as_u64(), e.service.is_active()) {
            assert_eq!(u64::from(lock(&e.service.session).as_ref().unwrap().audio_recovery_attempts), attempts, "{label}: tentativi");
        }
    }
}

// --- eventi programmati di Discord -------------------------------------------------------------

fn event(id: u64, name: &str, status: EventStatus, channel: Option<u64>) -> ScheduledEventInfo {
    ScheduledEventInfo { id, name: name.to_string(), status, channel_id: channel }
}

#[tokio::test]
async fn scheduled_events_start_and_stop_the_event_like_python() {
    use EventStatus::{Active, Canceled, Completed, Scheduled};
    let golden = golden();
    let cases = golden["scheduled"].as_array().unwrap();
    assert_eq!(cases.len(), 21);
    for case in cases {
        let label = case["label"].as_str().unwrap();
        let e = env();
        e.gateway.set_members(&[10]);
        let optins: std::collections::BTreeSet<i64> = if label == "pronto" { [900, 902].into() } else { [900].into() };
        live_event::save_scheduled_event_optins(&e.db, &optins).unwrap();
        e.staff_channel(ChannelKind::Text);
        let session_event = match label {
            "avvio_gia_in_corso_stesso" | "aggiornamento_completato" | "aggiornamento_annullato" | "aggiornamento_programmato_annullato" | "aggiornamento_completato_non_era_attivo" | "eliminato" | "eliminato_altro_id" => Some(900),
            "avvio_gia_in_corso_altro" | "aggiornamento_completato_altro_id" => Some(555),
            _ => None,
        };
        if let Some(id) = session_event {
            assert!(matches!(e.service.start_event(e.request(None, Some(id), "in corso")).await, StartOutcome::Started { .. }), "{label}");
        }
        e.gateway.clear_calls();
        let messages_before = e.api.messages_in(600).len();

        let service = &e.service;
        match label {
            "avvio_programmato" => service.start_scheduled_event(1, &event(900, "Serata Among Us", Active, Some(555))).await,
            "avvio_non_abilitato" => service.start_scheduled_event(1, &event(901, "Altra", Active, Some(555))).await,
            "avvio_non_attivo" => service.start_scheduled_event(1, &event(900, "Serata", Scheduled, Some(555))).await,
            "avvio_in_stage" => service.start_scheduled_event(1, &event(900, "Serata", Active, Some(556))).await,
            "avvio_senza_canale" => service.start_scheduled_event(1, &event(900, "Serata", Active, None)).await,
            "avvio_titolo_lungo" => service.start_scheduled_event(1, &event(900, &"N".repeat(150), Active, Some(555))).await,
            "avvio_gia_in_corso_stesso" | "avvio_gia_in_corso_altro" => service.start_scheduled_event(1, &event(900, "Serata", Active, Some(555))).await,
            "avvio_gia_in_partenza" => {
                lock(&service.auto_starting).insert(900);
                service.start_scheduled_event(1, &event(900, "Serata", Active, Some(555))).await;
            }
            "aggiornamento_parte" => update(service, Scheduled, event(900, "Serata", Active, Some(555))).await,
            "aggiornamento_resta_attivo" => update(service, Active, event(900, "Serata", Active, Some(555))).await,
            "aggiornamento_completato" | "aggiornamento_completato_senza_sessione" | "aggiornamento_completato_altro_id" => update(service, Active, event(900, "Serata", Completed, Some(555))).await,
            "aggiornamento_annullato" => update(service, Active, event(900, "Serata", Canceled, Some(555))).await,
            "aggiornamento_programmato_annullato" => update(service, Scheduled, event(900, "Serata", Canceled, Some(555))).await,
            "aggiornamento_completato_non_era_attivo" => update(service, Scheduled, event(900, "Serata", Completed, Some(555))).await,
            "eliminato" | "eliminato_senza_sessione" => service.on_scheduled_event_delete(900).await,
            "eliminato_altro_id" => service.on_scheduled_event_delete(901).await,
            "pronto" => {
                e.api.set_scheduled_events(1, vec![event(900, "Serata", Active, Some(555)), event(901, "Non abilitata", Active, Some(555)), event(902, "Futura", Scheduled, Some(555))]);
                service.on_ready(&[1]).await;
            }
            other => panic!("caso sconosciuto: {other}"),
        }

        assert_eq!(e.gateway.calls(), case["calls"].as_array().unwrap().iter().map(|call| call.as_str().unwrap().to_string()).collect::<Vec<_>>(), "{label}: chiamate alla voce");
        assert_eq!(service.is_active(), !case["session"].is_null(), "{label}: sessione");
        // Il golden Python sostituisce la pubblicazione del report: qui contano i soli messaggi di testo
        // (il link OBS); i report (messaggi con un embed) sono provati nei casi di fine evento.
        let sent: Vec<String> = e.staff_messages().into_iter().skip(messages_before).filter_map(|(content, _)| content).collect();
        let expected: Vec<String> = case["sends"].as_array().unwrap().iter().map(|text| text.as_str().unwrap().to_string()).collect();
        assert_eq!(sent, expected, "{label}: messaggi allo staff");
        let db = dump(&e.db);
        assert_eq!(db["audit"].as_array().unwrap().iter().map(|row| row[1].clone()).collect::<Vec<_>>(), case["audit"].as_array().unwrap().iter().map(|row| row[1].clone()).collect::<Vec<_>>(), "{label}: registro audit");
        assert_eq!(db["events"].as_array().unwrap().iter().map(|row| row[11].clone()).collect::<Vec<_>>(), case["events"].as_array().unwrap().iter().map(|row| row[11].clone()).collect::<Vec<_>>(), "{label}: motivi di chiusura");
        let mut auto: Vec<u64> = lock(&service.auto_starting).iter().copied().collect();
        auto.sort_unstable();
        assert_eq!(json!(auto), case["auto_starting"], "{label}: avvii in corso");
    }

    async fn update(service: &TestService, before: EventStatus, after: ScheduledEventInfo) {
        service.note_event_status(after.id, before);
        service.on_scheduled_event_update(1, &after).await;
    }
}

#[tokio::test]
async fn updates_without_a_known_previous_state_still_start_and_finish() {
    let e = env();
    live_event::save_scheduled_event_optins(&e.db, &[900].into()).unwrap();
    // Nessuno stato precedente: un evento che risulta attivo parte comunque…
    e.service.on_scheduled_event_update(1, &event(900, "Serata", EventStatus::Active, Some(555))).await;
    assert!(e.service.is_active());
    // …e si chiude quando risulta concluso (lo stato attivo era stato visto poco fa).
    e.service.on_scheduled_event_update(1, &event(900, "Serata", EventStatus::Completed, Some(555))).await;
    assert!(!e.service.is_active());
    // Lo stesso aggiornamento ripetuto non riavvia nulla: prima era attivo, ora non lo è più.
    e.service.on_scheduled_event_update(1, &event(900, "Serata", EventStatus::Completed, Some(555))).await;
    assert!(!e.service.is_active());
}

// --- chiusura in ripresa e riavvio ---------------------------------------------------------------

#[tokio::test]
async fn stop_event_audits_who_stopped_it() {
    let e = env();
    assert_eq!(e.service.stop_event(77).await, None);
    assert!(matches!(e.start_manual().await, StartOutcome::Started { .. }));
    assert_eq!(e.service.stop_event(77).await, Some(1));
    let audit = dump(&e.db)["audit"].clone();
    let actions: Vec<_> = audit.as_array().unwrap().iter().map(|row| (row[0].clone(), row[1].clone())).collect();
    assert_eq!(
        actions,
        vec![(json!(42), json!("live_event_started")), (Value::Null, json!("live_event_finished")), (json!(77), json!("live_event_stopped"))]
    );
}

#[tokio::test]
async fn the_standard_voice_channel_of_a_person_excludes_stages() {
    let e = env();
    let controller: &dyn LiveController = e.service.as_ref();
    assert_eq!(controller.standard_voice_channel_of(1, 42).await, None, "non è in nessun canale");
    e.gateway.set_user_channel(1, 42, 556);
    assert_eq!(controller.standard_voice_channel_of(1, 42).await, None, "uno Stage non vale");
    e.gateway.set_user_channel(1, 42, 555);
    assert_eq!(controller.standard_voice_channel_of(1, 42).await, Some(555));
}
