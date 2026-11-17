use std::cell::RefCell;
use std::sync::{Mutex, Once};

use axum::http::Request;
use chrono::TimeZone;
use tower::ServiceExt;

use super::*;
use crate::discord_api::testing::FakeApi;
use crate::live_service::LiveController;
use crate::notify::Notifier;
use crate::notify::testing::FakeSender;
use crate::overlay::LiveSessionInfo;
use crate::polls::RefreshScheduler;
use crate::publisher::Publisher;
use crate::site_auth::tests::{FakeDirectory, FakeOAuth};
use crate::site_auth::{AuthConfig, GuildFacts, SiteMember};
use crate::site_backend::DiscordBackend;

fn golden() -> Value {
    serde_json::from_str(include_str!("../golden/site_tools_expected.json")).unwrap()
}

// --- finti -------------------------------------------------------------------------------------

type TestBackend = DiscordBackend<FakeSender, FakeApi>;

/// Il backend vero per audit e revisioni; le operazioni su Discord si registrano soltanto.
struct RecordingBackend {
    inner: TestBackend,
    calls: Arc<Mutex<Vec<Value>>>,
    poll_error: Mutex<Option<String>>,
    dashboard_error: Mutex<Option<String>>,
}

impl SiteBackend for RecordingBackend {
    fn audit(&self, entry: AuditEntry) {
        self.inner.audit(entry);
    }
    fn channel_kind(&self, guild_id: u64, channel_id: u64) -> BoxFuture<'_, Result<crate::discord_api::ChannelKind, crate::discord_api::ApiError>> {
        self.inner.channel_kind(guild_id, channel_id)
    }
    fn send_embed(&self, channel_id: u64, embed: crate::cards::EmbedSpec) -> BoxFuture<'_, Result<u64, crate::discord_api::ApiError>> {
        self.inner.send_embed(channel_id, embed)
    }
    fn apply_review(&self, actor_id: i64, suggestion_id: i64, action: String, reason: String) -> BoxFuture<'_, crate::review::ReviewOutcome> {
        self.inner.apply_review(actor_id, suggestion_id, action, reason)
    }
    fn refresh_poll(&self, poll_id: i64) -> BoxFuture<'_, Result<(), String>> {
        self.calls.lock().unwrap().push(json!(["refresh_poll", [poll_id], {}]));
        let error = self.poll_error.lock().unwrap().clone();
        Box::pin(async move { error.map_or(Ok(()), Err) })
    }
    fn refresh_dashboard(&self) -> BoxFuture<'_, Result<(), String>> {
        self.calls.lock().unwrap().push(json!(["refresh_dashboard", [], {}]));
        let error = self.dashboard_error.lock().unwrap().clone();
        Box::pin(async move { error.map_or(Ok(()), Err) })
    }
    fn notify_users(&self, user_ids: Vec<u64>, text: String) -> BoxFuture<'_, ()> {
        self.calls.lock().unwrap().push(json!(["notify", [user_ids, text], {}]));
        Box::pin(async {})
    }
    fn publish_activity(&self, text: String) -> BoxFuture<'_, ()> {
        self.calls.lock().unwrap().push(json!(["activity", [text], {}]));
        Box::pin(async {})
    }
    fn staff_log(&self, text: String) {
        self.calls.lock().unwrap().push(json!(["staff_log", [text], {}]));
    }
}

struct FakeLive {
    state: Arc<LiveState>,
    calls: Arc<Mutex<Vec<Value>>>,
    finish_result: Mutex<Option<i64>>,
    start_succeeds: Mutex<bool>,
}

impl LiveController for FakeLive {
    fn start(&self, _guild_id: u64, _channel_id: u64, _actor: u64) -> BoxFuture<'_, StartOutcome> {
        unimplemented!("non serve alle route del sito")
    }
    fn stop(&self, _actor: u64) -> BoxFuture<'_, Option<i64>> {
        unimplemented!("non serve alle route del sito")
    }
    fn standard_voice_channel_of(&self, _guild_id: u64, _user_id: u64) -> BoxFuture<'_, Option<u64>> {
        unimplemented!("non serve alle route del sito")
    }
    fn finish(&self, reason: &'static str) -> BoxFuture<'_, Option<i64>> {
        self.calls.lock().unwrap().push(json!(["finish_event", [reason], {}]));
        self.state.set_session(None);
        let result = self.finish_result.lock().unwrap().take();
        Box::pin(async move { result })
    }
    fn start_titled(&self, guild_id: u64, channel_id: u64, title: String) -> BoxFuture<'_, StartOutcome> {
        self.calls.lock().unwrap().push(json!(["start_event", [null, channel_id, title], {}]));
        let started = *self.start_succeeds.lock().unwrap();
        if started {
            self.state.set_session(Some(LiveSessionInfo { title, guild_id, channel_id, channel_name: "Sala".into(), started_at: String::new() }));
        }
        Box::pin(async move { if started { StartOutcome::Started { event_id: 1, link: String::new() } } else { StartOutcome::ConnectFailed } })
    }
}

struct FakeBackups {
    calls: Arc<Mutex<Vec<Value>>>,
    error: Mutex<Option<String>>,
}

impl SiteBackups for FakeBackups {
    fn status(&self) -> Value {
        golden_backup_status()
    }
    fn create(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        self.calls.lock().unwrap().push(json!(["create_backups", [], {}]));
        let error = self.error.lock().unwrap().clone();
        Box::pin(async move { error.map_or_else(|| Ok(vec!["daily-2027-03-05.db".to_string(), "weekly-2027-03-05.db".to_string()]), Err) })
    }
}

fn golden_backup_status() -> Value {
    json!({"root": "/backup", "daily_count": 2, "latest_daily": "daily-2027-03-04.db", "weekly_count": 1, "latest_weekly": "weekly-2027-03-01.db", "cloud_upload_error": null})
}

thread_local! {
    static TOPICS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn capture_topics() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| crate::hooks::add_site_change_listener(Arc::new(|topic| TOPICS.with(|topics| topics.borrow_mut().push(topic.to_string())))));
}

fn rome_clock() -> DateTime<Tz> {
    crate::config::TZ.with_ymd_and_hms(2027, 3, 5, 21, 0, 0).unwrap()
}

struct Env {
    db: Database,
    backend: Arc<RecordingBackend>,
    live: Arc<FakeLive>,
    live_state: Arc<LiveState>,
    backups: Arc<FakeBackups>,
    directory: FakeDirectory,
    calls: Arc<Mutex<Vec<Value>>>,
    state: ToolsState,
}

/// `mode`: «admin», «staff» (la stessa persona senza il permesso di amministratore) o «none».
fn env(mode: &str, with_controller: bool) -> Env {
    capture_topics();
    TOPICS.with(|topics| topics.borrow_mut().clear());
    let db = Database::open_in_memory().unwrap();
    for statement in golden()["seed"].as_array().unwrap() {
        db.execute_batch(statement.as_str().unwrap()).unwrap();
    }
    db.set_setting("staff_roles", "777").unwrap();
    db.execute(
        "INSERT INTO web_sessions(session_id,user_id,display_name,created_at,expires_at) VALUES('s1',42,'Staff','2027-01-01T00:00:00+00:00','2099-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();
    let directory = FakeDirectory::default();
    {
        let mut guard = directory.0.lock().unwrap();
        guard.guild = Some(GuildFacts { owner_id: 1 });
        guard.members.insert(42, SiteMember { display_name: "Staff".into(), administrator: mode == "admin", role_ids: vec![777] });
        guard.voice_channels = vec![(555, "Sala Eventi".into()), (556, "Sala é Due".into())];
    }
    let auth = Arc::new(SiteAuth::new(db.clone(), Arc::new(directory.clone()), Arc::new(FakeOAuth::default()), AuthConfig::default(), None));
    let api = FakeApi::default();
    let publisher = Arc::new(Publisher::new(api, db.clone()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(RecordingBackend {
        inner: DiscordBackend { db: db.clone(), notifier: Notifier::new(db.clone(), FakeSender::default()), publisher, scheduler: RefreshScheduler::new() },
        calls: calls.clone(),
        poll_error: Mutex::new(None),
        dashboard_error: Mutex::new(None),
    });
    let live_state = Arc::new(LiveState::default());
    let live = Arc::new(FakeLive { state: live_state.clone(), calls: calls.clone(), finish_result: Mutex::new(None), start_succeeds: Mutex::new(true) });
    let late = Arc::new(LateLive::default());
    if with_controller {
        late.set(live.clone());
    }
    let backups = Arc::new(FakeBackups { calls: calls.clone(), error: Mutex::new(None) });
    let state = ToolsState {
        auth,
        db: db.clone(),
        backend: backend.clone(),
        directory: Arc::new(directory.clone()),
        live_state: live_state.clone(),
        live: late,
        backups: backups.clone(),
        clock: rome_clock,
    };
    Env { db, backend, live, live_state, backups, directory, calls, state }
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
        "suggestions": table(db, "SELECT id,status,picked_by,picked_at,review_status FROM suggestions ORDER BY id"),
        "options": table(db, "SELECT poll_id,label,suggestion_id FROM options ORDER BY id"),
        "user_stats": table(db, "SELECT * FROM user_stats ORDER BY user_id"),
        "weekly_activity": table(db, "SELECT * FROM weekly_activity ORDER BY user_id,week"),
        "season_activity": table(db, "SELECT * FROM season_activity ORDER BY user_id,week"),
        "ballot_weeks": table(db, "SELECT * FROM user_ballot_weeks ORDER BY user_id,week"),
        "badges": table(db, "SELECT user_id,badge_key,unlocked_at FROM user_hidden_badges ORDER BY user_id,badge_key"),
        "audit": table(db, "SELECT actor_id,action,entity_type,entity_id,reason,details FROM staff_audit ORDER BY id"),
        "events": table(db, "SELECT id,title,ended_at FROM live_events ORDER BY id"),
    })
}

async fn call(e: &Env, method: &str, path: &str, body: Option<String>, authenticated: bool) -> (u16, axum::http::HeaderMap, String) {
    let mut builder = Request::builder().method(method).uri(path);
    if authenticated {
        builder = builder.header("Cookie", "sfo_site_session=s1");
    }
    let request = builder.body(axum::body::Body::from(body.unwrap_or_default())).unwrap();
    let response = routes(e.state.clone()).oneshot(request).await.unwrap();
    let (parts, response_body) = response.into_parts();
    (parts.status.as_u16(), parts.headers, String::from_utf8(axum::body::to_bytes(response_body, usize::MAX).await.unwrap().to_vec()).unwrap())
}

// --- le route ----------------------------------------------------------------------------------

#[tokio::test]
async fn tool_routes_match_the_python_implementation() {
    let golden = golden();
    let scenarios = golden["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 111);
    let base = "/api/site/tools";
    for case in scenarios {
        let label = case["label"].as_str().unwrap();
        let mode = if label.contains("non_autenticat") {
            "none"
        } else if label.contains("solo_staff") || label == "stato_staff" {
            "staff"
        } else {
            "admin"
        };
        let e = env(mode, label != "evento_senza_cog" && label != "evento_stop_senza_cog");
        let mut method = "GET";
        let uri;
        let mut body: Option<String> = None;
        let set_week = |week: &str| e.db.set_setting("week", week).unwrap();
        let session = |title: &str| e.live_state.set_session(Some(LiveSessionInfo { title: title.into(), guild_id: 100, channel_id: 555, channel_name: "Sala Eventi".into(), started_at: String::new() }));
        let review = |id: i64, payload: Value| (format!("{base}/proposals/{id}/review"), Some(payload.to_string()));
        match label {
            // Un identificativo che non è fatto di cifre non corrisponde a nessuna route: 404 prima di tutto.
            "revisione_id_non_numerico_non_autenticata" => (uri, body, method) = (format!("{base}/proposals/abc/review"), Some(json!({"action": "approve"}).to_string()), "POST"),
            "scelta_id_non_numerico_non_autenticata" => (uri, method) = (format!("{base}/proposals/abc/choose"), "POST"),
            l if l.starts_with("selezione_id_non_numerico_") => (uri, method) = (format!("{base}/proposals/abc/selection"), "DELETE"),
            l if l.starts_with("stato_") => {
                uri = format!("{base}/state");
                match l {
                    "stato_evento_attivo" => session("Serata 🎮"),
                    "stato_senza_server" => e.directory.0.lock().unwrap().voice_channels.clear(),
                    "stato_settimana_vuota" => set_week("99"),
                    _ => {}
                }
            }
            "revisione_approva" => (uri, body, method) = (review(2, json!({"action": "approve"})).0, review(2, json!({"action": "approve"})).1, "POST"),
            "revisione_rifiuta_con_motivo" => (uri, body, method) = (review(2, json!(0)).0, Some(json!({"action": "reject", "reason": "troppo simile"}).to_string()), "POST"),
            "revisione_rifiuta_senza_motivo" => (uri, body, method) = (review(2, json!(0)).0, Some(json!({"action": "reject"}).to_string()), "POST"),
            "revisione_rifiuta_motivo_vuoto" => (uri, body, method) = (review(2, json!(0)).0, Some(json!({"action": "reject", "reason": "   "}).to_string()), "POST"),
            "revisione_rimetti" => (uri, body, method) = (review(1, json!(0)).0, Some(json!({"action": "review"}).to_string()), "POST"),
            "revisione_stato_sbagliato" => (uri, body, method) = (review(1, json!(0)).0, Some(json!({"action": "approve"}).to_string()), "POST"),
            "revisione_non_trovata" => (uri, body, method) = (review(99, json!(0)).0, Some(json!({"action": "approve"}).to_string()), "POST"),
            "revisione_vecchia_settimana" => (uri, body, method) = (review(5, json!(0)).0, Some(json!({"action": "approve"}).to_string()), "POST"),
            "revisione_azione_non_valida" => (uri, body, method) = (review(2, json!(0)).0, Some(json!({"action": "boh"}).to_string()), "POST"),
            "revisione_azione_non_testo" => (uri, body, method) = (review(2, json!(0)).0, Some(json!({"action": 5}).to_string()), "POST"),
            "revisione_azione_assente" => (uri, body, method) = (review(2, json!(0)).0, Some("{}".into()), "POST"),
            "revisione_motivo_non_testo" => (uri, body, method) = (review(2, json!(0)).0, Some(json!({"action": "approve", "reason": 5}).to_string()), "POST"),
            "revisione_motivo_200" => (uri, body, method) = (review(2, json!(0)).0, Some(json!({"action": "reject", "reason": "x".repeat(200)}).to_string()), "POST"),
            "revisione_motivo_201" => (uri, body, method) = (review(2, json!(0)).0, Some(json!({"action": "reject", "reason": "x".repeat(201)}).to_string()), "POST"),
            "revisione_non_oggetto" => (uri, body, method) = (review(2, json!(0)).0, Some("[1]".into()), "POST"),
            "revisione_json_non_valido" => (uri, body, method) = (review(2, json!(0)).0, Some("x".into()), "POST"),
            "revisione_corpo_vuoto" => (uri, body, method) = (review(2, json!(0)).0, Some(String::new()), "POST"),
            "revisione_non_autenticata" => (uri, body, method) = (review(2, json!(0)).0, Some(json!({"action": "approve"}).to_string()), "POST"),
            "revisione_id_non_numerico" => (uri, body, method) = (format!("{base}/proposals/abc/review"), Some(json!({"action": "approve"}).to_string()), "POST"),
            "revisione_id_negativo" => (uri, body, method) = (format!("{base}/proposals/-1/review"), Some(json!({"action": "approve"}).to_string()), "POST"),
            "revisione_id_enorme" => (uri, body, method) = (format!("{base}/proposals/99999999999999999999/review"), Some(json!({"action": "approve"}).to_string()), "POST"),
            l if l.starts_with("scelta_") => {
                method = "POST";
                let target = match l {
                    "scelta_ok_due_autori" => 6,
                    "scelta_ok_senza_autori" => 7,
                    "scelta_non_approvata" => 2,
                    "scelta_rifiutata" => 4,
                    "scelta_gia_nel_ballottaggio" => 3,
                    "scelta_non_trovata" => 99,
                    "scelta_vecchia_settimana" => 5,
                    _ => 1,
                };
                uri = format!("{base}/proposals/{target}/choose");
                match l {
                    "scelta_senza_canale_sondaggi" => e.db.execute("DELETE FROM settings WHERE key='poll_channel'", []).map(|_| ()).unwrap(),
                    "scelta_senza_canale_staff" => e.db.set_setting("staff_channel", "").unwrap(),
                    "scelta_senza_sondaggio_community" => e.db.execute("UPDATE polls SET active=0", []).map(|_| ()).unwrap(),
                    "scelta_slot_pieni" => e.db.set_setting("slots", "1").unwrap(),
                    "scelta_slot_pieni_zero" => e.db.set_setting("slots", "0").unwrap(),
                    "scelta_slot_non_validi" => e.db.set_setting("slots", "molti").unwrap(),
                    "scelta_slot_con_spazi" => e.db.set_setting("slots", " 3 ").unwrap(),
                    "scelta_slot_assenti" => e.db.execute("DELETE FROM settings WHERE key='slots'", []).map(|_| ()).unwrap(),
                    "scelta_errore_database" => e.db.execute_batch("CREATE TRIGGER fail_options BEFORE INSERT ON options BEGIN SELECT RAISE(ABORT,'boom'); END").unwrap(),
                    "scelta_aggiornamento_sondaggio_fallito" => *e.backend.poll_error.lock().unwrap() = Some("discord".into()),
                    "scelta_aggiornamento_dashboard_negato" => *e.backend.dashboard_error.lock().unwrap() = Some("403".into()),
                    "scelta_terza_candidata_con_tre_slot" => {
                        e.db.set_setting("slots", "2").unwrap();
                        e.db.execute("INSERT INTO options(poll_id,label,suggestion_id) VALUES(1,'Extra',7)", []).unwrap();
                    }
                    "scelta_seconda_scelta_stesso_autore" => {
                        e.db.execute("INSERT INTO user_stats(user_id,ballot_entries) VALUES(101,1)", []).unwrap();
                    }
                    _ => {}
                }
            }
            l if l.starts_with("rimozione_") => {
                method = "DELETE";
                let target = match l {
                    "rimozione_non_nel_ballottaggio" => 1,
                    "rimozione_non_trovata" => 99,
                    _ => 3,
                };
                uri = format!("{base}/proposals/{target}/selection");
                match l {
                    "rimozione_senza_canali" => e.db.execute("DELETE FROM settings WHERE key='staff_channel'", []).map(|_| ()).unwrap(),
                    "rimozione_con_statistiche" => {
                        e.db.execute("INSERT INTO user_stats(user_id,ballot_entries) VALUES(101,3)", []).unwrap();
                        e.db.execute("INSERT INTO weekly_activity(user_id,week,activity_count,points) VALUES(101,10,2,12)", []).unwrap();
                    }
                    "rimozione_senza_opzione" => e.db.execute("DELETE FROM options", []).map(|_| ()).unwrap(),
                    "rimozione_aggiornamento_fallito" => *e.backend.poll_error.lock().unwrap() = Some("discord".into()),
                    "rimozione_errore_database" => e.db.execute_batch("CREATE TRIGGER fail_options BEFORE DELETE ON options BEGIN SELECT RAISE(ABORT,'boom'); END").unwrap(),
                    _ => {}
                }
            }
            l if l.starts_with("backup_") => {
                uri = format!("{base}/backups");
                method = match l {
                    "backup_creazione" | "backup_solo_staff_creazione" | "backup_errore_disco" | "backup_errore_runtime" | "backup_errore_database" => "POST",
                    "backup_metodo_non_ammesso" => "PUT",
                    _ => "GET",
                };
                if l.starts_with("backup_errore") {
                    *e.backups.error.lock().unwrap() = Some("errore".into());
                }
            }
            l if l.starts_with("report_evento_") => {
                method = "DELETE";
                let target = match l {
                    "report_evento_attivo" => "3",
                    "report_evento_inesistente" => "99",
                    l if l.starts_with("report_evento_id_non_numerico") => "abc",
                    _ => "1",
                };
                uri = format!("{base}/events/{target}");
                if l == "report_evento_errore_database" {
                    e.db.execute_batch("CREATE TRIGGER fail_events_delete BEFORE DELETE ON live_events BEGIN SELECT RAISE(ABORT,'boom'); END").unwrap();
                }
            }
            l if l.starts_with("evento_") => {
                method = "POST";
                let action = if l.starts_with("evento_stop") { "stop" } else if l.starts_with("evento_azione_sconosciuta") { "pausa" } else { "start" };
                uri = format!("{base}/events/{action}");
                let channel = |value: Value| Some(json!({"channel_id": value}).to_string());
                body = match l {
                    "evento_start_ok" | "evento_non_autenticato" | "evento_senza_cog" | "evento_start_gia_attivo" | "evento_start_fallito" | "evento_start_senza_server" => channel(json!(555)),
                    "evento_start_canale_testo" => channel(json!(600)),
                    "evento_start_canale_stringa" => channel(json!("556")),
                    "evento_start_canale_decimale" => channel(json!(555.9)),
                    "evento_start_canale_spazi" => channel(json!(" 555 ")),
                    "evento_start_canale_vero" => channel(json!(true)),
                    "evento_start_canale_testo_non_numerico" => channel(json!("abc")),
                    "evento_start_canale_null" => channel(json!(null)),
                    "evento_start_canale_assente" => Some("{}".into()),
                    "evento_start_canale_lista" => channel(json!([555])),
                    "evento_start_non_oggetto" => Some("[555]".into()),
                    "evento_start_json_non_valido" => Some("x".into()),
                    "evento_start_canale_inesistente" => channel(json!(999)),
                    l if l.starts_with("evento_start_titolo_") => {
                        let title = match l {
                            "evento_start_titolo_personalizzato" => json!("  Torneo  "),
                            "evento_start_titolo_vuoto" => json!("   "),
                            "evento_start_titolo_100" => json!("t".repeat(100)),
                            "evento_start_titolo_101" => json!("t".repeat(101)),
                            "evento_start_titolo_spazi_oltre_100" => json!(format!("     ok{}", " ".repeat(200))),
                            "evento_start_titolo_non_testo" => json!(5),
                            "evento_start_titolo_null" => json!(null),
                            other => panic!("titolo sconosciuto: {other}"),
                        };
                        Some(json!({"channel_id": 555, "title": title}).to_string())
                    }
                    _ => None,
                };
                match l {
                    "evento_stop_ok" => {
                        *e.live.finish_result.lock().unwrap() = Some(7);
                        session("x");
                    }
                    "evento_start_gia_attivo" => session("x"),
                    "evento_start_fallito" => *e.live.start_succeeds.lock().unwrap() = false,
                    "evento_start_senza_server" => e.directory.0.lock().unwrap().voice_channels.clear(),
                    _ => {}
                }
            }
            other => panic!("scenario sconosciuto: {other}"),
        }

        let (status, headers, text) = call(&e, method, &uri, body, mode != "none").await;

        assert_eq!(u64::from(status), case["status"].as_u64().unwrap(), "{label}: stato");
        assert_eq!(headers.get("cache-control").and_then(|value| value.to_str().ok()), case["headers"]["Cache-Control"].as_str(), "{label}: cache");
        if case["headers"].get("Content-Type").is_some() && status != 405 {
            assert_eq!(headers["content-type"], case["headers"]["Content-Type"].as_str().unwrap(), "{label}: tipo");
        }
        if label != "backup_metodo_non_ammesso" && status != 405 {
            assert_eq!(text, case["body"].as_str().unwrap(), "{label}: corpo");
        }
        // Le operazioni su Discord e sui servizi, nell'ordine (senza gli effetti interni della revisione).
        let expected_calls: Vec<Value> = case["calls"].as_array().unwrap().iter().filter(|call| !call[0].as_str().unwrap().starts_with("review_")).cloned().collect();
        let normalized: Vec<Value> = expected_calls.iter().map(|call| call.clone()).collect();
        assert_eq!(json!(e.calls.lock().unwrap().clone()), json!(normalized), "{label}: chiamate");
        assert_eq!(json!(TOPICS.with(|topics| topics.borrow().clone())), case["topics"], "{label}: argomenti");
        let mut db = dump(&e.db);
        let mut expected_db = case["db"].clone();
        // La revisione scrive righe di audit come in Python; le altre tabelle non cambiano.
        if label.starts_with("revisione_") {
            db["audit"] = json!(db["audit"].as_array().unwrap().iter().map(|row| json!([row[0], row[1], row[2], row[3]])).collect::<Vec<_>>());
            expected_db["audit"] = json!(expected_db["audit"].as_array().unwrap().iter().map(|row| json!([row[0], row[1], row[2], row[3]])).collect::<Vec<_>>());
        }
        assert_eq!(db, expected_db, "{label}: database");
    }
}

#[test]
fn digit_parameters_follow_the_route_patterns() {
    assert_eq!(digits_param("12"), Some(Some(12)));
    assert_eq!(digits_param("007"), Some(Some(7)));
    assert_eq!(digits_param("٣"), None);
    assert_eq!(digits_param("99999999999999999999"), Some(None));
    for text in ["", "abc", "-1", "+1", "1 ", "1.5", "1_0"] {
        assert_eq!(digits_param(text), None, "{text:?}");
    }
}

#[test]
fn backup_status_has_the_keys_python_uses() {
    let status = BackupStatus {
        root: std::path::PathBuf::from("/backup"),
        daily_count: 2,
        latest_daily: Some("daily-2027-03-04.db".into()),
        weekly_count: 1,
        latest_weekly: Some("weekly-2027-03-01.db".into()),
        cloud_upload_error: None,
    };
    let value = status_json(&status);
    assert_eq!(value, golden_backup_status());
    assert_eq!(value.as_object().unwrap().keys().collect::<Vec<_>>(), golden_backup_status().as_object().unwrap().keys().collect::<Vec<_>>());
}
