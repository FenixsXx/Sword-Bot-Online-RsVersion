use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::http::Request;
use chrono::TimeZone;
use tower::ServiceExt;

use super::*;
use crate::discord_api::testing::FakeApi;
use crate::notify::Notifier;
use crate::notify::testing::FakeSender;
use crate::polls::RefreshScheduler;
use crate::publisher::Publisher;
use crate::site_auth::tests::{FakeDirectory, FakeOAuth};
use crate::site_auth::{AuthConfig, DirectoryError, GuildFacts, SiteMember};

/// Il golden si legge una volta sola: pesa qualche megabyte.
fn golden() -> &'static Value {
    static GOLDEN: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    GOLDEN.get_or_init(|| serde_json::from_str(include_str!("../golden/site_admin_expected.json")).unwrap())
}

type Calls = Arc<Mutex<Vec<Value>>>;
type Errors = Arc<Mutex<HashMap<String, String>>>;

fn api_error(kind: &str) -> ApiError {
    match kind {
        "forbidden" => ApiError::Forbidden,
        "notfound" => ApiError::NotFound,
        _ => ApiError::Http("boom".into()),
    }
}

/// I canali testuali del server di prova.
const TEXT_CHANNELS: [u64; 5] = [900, 910, 911, 912, 913];

// --- finti -------------------------------------------------------------------------------------

type TestBackend = DiscordBackend<FakeSender, FakeApi>;

struct RecordingBackend {
    inner: TestBackend,
    calls: Calls,
    errors: Errors,
}

impl RecordingBackend {
    fn error(&self, name: &str) -> Option<String> {
        self.errors.lock().unwrap().get(name).cloned()
    }
}

impl SiteBackend for RecordingBackend {
    fn audit(&self, entry: AuditEntry) {
        self.inner.audit(entry);
    }
    fn channel_kind(&self, _guild_id: u64, channel_id: u64) -> BoxFuture<'_, Result<ChannelKind, ApiError>> {
        let result = match channel_id {
            id if TEXT_CHANNELS.contains(&id) => Ok(ChannelKind::Text),
            901 => Ok(ChannelKind::Voice),
            902 => Err(ApiError::Forbidden),
            _ => Err(ApiError::NotFound),
        };
        Box::pin(async move { result })
    }
    fn send_embed(&self, _channel_id: u64, _embed: crate::cards::EmbedSpec) -> BoxFuture<'_, Result<u64, ApiError>> {
        unimplemented!("non serve alle route di amministrazione")
    }
    fn apply_review(&self, _actor_id: i64, _suggestion_id: i64, _action: String, _reason: String) -> BoxFuture<'_, crate::review::ReviewOutcome> {
        unimplemented!("non serve alle route di amministrazione")
    }
    fn refresh_poll(&self, poll_id: i64) -> BoxFuture<'_, Result<(), String>> {
        self.calls.lock().unwrap().push(json!(["refresh_poll", [poll_id]]));
        let error = self.error("poll");
        Box::pin(async move { error.map_or(Ok(()), |_| Err("discord".to_string())) })
    }
    fn refresh_dashboard(&self) -> BoxFuture<'_, Result<(), String>> {
        self.calls.lock().unwrap().push(json!(["refresh_dashboard", []]));
        let error = self.error("dashboard");
        Box::pin(async move { error.map_or(Ok(()), |_| Err("discord".to_string())) })
    }
    fn notify_users(&self, _user_ids: Vec<u64>, _text: String) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn publish_activity(&self, _text: String) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
    fn staff_log(&self, _text: String) {}
}

/// Le operazioni su Discord: si registrano e, a richiesta, falliscono.
struct FakeOps {
    calls: Calls,
    errors: Errors,
    next_message: Mutex<u64>,
}

impl AdminOps for FakeOps {
    fn refresh_all(&self) -> BoxFuture<'_, Result<(), String>> {
        self.calls.lock().unwrap().push(json!(["refresh_all", []]));
        let error = self.errors.lock().unwrap().get("all").cloned();
        Box::pin(async move { error.map_or(Ok(()), |_| Err("discord".to_string())) })
    }

    fn send_message(&self, channel_id: u64, payload: MessagePayload) -> BoxFuture<'_, Result<u64, ApiError>> {
        let embed = payload.embeds.as_ref().and_then(|embeds| embeds.first()).map(|embed| serde_json::to_value(embed).unwrap());
        self.calls.lock().unwrap().push(json!(["send", [channel_id, payload.content, embed]]));
        let result = match self.errors.lock().unwrap().get("send") {
            Some(kind) => Err(api_error(kind)),
            None => {
                let mut next = self.next_message.lock().unwrap();
                let id = *next;
                *next += 1;
                Ok(id)
            }
        };
        Box::pin(async move { result })
    }

    fn delete_message(&self, channel_id: u64, message_id: u64) -> BoxFuture<'_, Result<(), ApiError>> {
        // Python non prova nemmeno a togliere un messaggio da un canale che non riesce a trovare.
        let result = if TEXT_CHANNELS.contains(&channel_id) {
            self.calls.lock().unwrap().push(json!(["delete_message", [channel_id, message_id]]));
            match self.errors.lock().unwrap().get("delete") {
                Some(kind) => Err(api_error(kind)),
                None => Ok(()),
            }
        } else if channel_id == 902 {
            Err(ApiError::Forbidden)
        } else {
            Err(ApiError::NotFound)
        };
        Box::pin(async move { result })
    }

    fn lock_poll(&self, _poll_id: i64) -> BoxFuture<'_, PollGuard> {
        Box::pin(async { Box::new(()) as PollGuard })
    }

    fn forget_poll_lock(&self, _poll_id: i64) {}
}

/// La directory del server: i permessi arrivano dalla directory finta, il resto da `overview`.
#[derive(Clone)]
struct AdminDirectory {
    inner: FakeDirectory,
    overview: Arc<Mutex<Option<GuildOverview>>>,
}

impl SiteDirectory for AdminDirectory {
    fn guild(&self, guild_id: u64) -> Option<GuildFacts> {
        self.inner.guild(guild_id)
    }
    fn member(&self, guild_id: u64, user_id: u64) -> BoxFuture<'_, Result<Option<SiteMember>, DirectoryError>> {
        self.inner.member(guild_id, user_id)
    }
    fn guild_overview(&self, _guild_id: u64) -> Option<GuildOverview> {
        self.overview.lock().unwrap().clone()
    }
}

thread_local! {
    static TOPICS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn capture_topics() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| crate::hooks::add_site_change_listener(Arc::new(|topic| TOPICS.with(|topics| topics.borrow_mut().push(topic.to_string())))));
}

fn rome_clock() -> DateTime<Tz> {
    crate::config::TZ.with_ymd_and_hms(2027, 3, 5, 21, 0, 0).unwrap()
}

fn overview() -> GuildOverview {
    GuildOverview {
        name: "Sword Friends Online".into(),
        member_count: 42,
        text_channels: vec![(900, "sondaggi".into()), (910, "benvenuto".into()), (911, "staff".into()), (912, "report".into()), (913, "attività 🎉".into())],
        roles: vec![(555, "Éventi".into()), (777, "Staff".into()), (888, "Moderatori".into())],
        bot_avatar_url: "https://cdn.example.test/bot.png".into(),
    }
}

struct Env {
    db: Database,
    calls: Calls,
    errors: Errors,
    overview: Arc<Mutex<Option<GuildOverview>>>,
    state: AdminState,
}

/// `mode`: «admin», «staff» (la stessa persona senza il permesso di amministratore) o «none».
fn env(mode: &str) -> Env {
    capture_topics();
    TOPICS.with(|topics| topics.borrow_mut().clear());
    let db = Database::open_in_memory().unwrap();
    for statement in golden()["seed"].as_array().unwrap() {
        db.execute_batch(statement.as_str().unwrap()).unwrap();
    }
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
    }
    let overview_cell = Arc::new(Mutex::new(Some(overview())));
    let directory = AdminDirectory { inner: directory, overview: overview_cell.clone() };
    let auth = Arc::new(SiteAuth::new(db.clone(), Arc::new(directory.clone()), Arc::new(FakeOAuth::default()), AuthConfig::default(), None));
    let publisher = Arc::new(Publisher::new(FakeApi::default(), db.clone()));
    let calls: Calls = Arc::default();
    let errors: Errors = Arc::default();
    let backend = Arc::new(RecordingBackend {
        inner: DiscordBackend { db: db.clone(), notifier: Notifier::new(db.clone(), FakeSender::default()), publisher, scheduler: RefreshScheduler::new() },
        calls: calls.clone(),
        errors: errors.clone(),
    });
    let ops = Arc::new(FakeOps { calls: calls.clone(), errors: errors.clone(), next_message: Mutex::new(7001) });
    let state = AdminState {
        auth,
        db: db.clone(),
        backend,
        ops,
        directory: Arc::new(directory),
        clock: rome_clock,
        preview_delay: Duration::from_secs(PREVIEW_SECONDS),
    };
    Env { db, calls, errors, overview: overview_cell, state }
}

/// Prepara la scena come `apply_setup` del golden Python.
fn apply_setup(e: &Env, setup: &Value) {
    if let Some(settings) = setup.get("settings").and_then(Value::as_object) {
        for (key, value) in settings {
            match value {
                Value::Null => {
                    e.db.execute("DELETE FROM settings WHERE key=?", [key]).unwrap();
                }
                other => e.db.set_setting(key, other.as_str().unwrap()).unwrap(),
            }
        }
    }
    for statement in setup.get("sql").and_then(Value::as_array).into_iter().flatten() {
        e.db.execute_batch(statement.as_str().unwrap()).unwrap();
    }
    for (name, kind) in setup.get("errors").and_then(Value::as_object).into_iter().flatten() {
        e.errors.lock().unwrap().insert(name.clone(), kind.as_str().unwrap().to_string());
    }
    if setup.get("no_guild").is_some() {
        *e.overview.lock().unwrap() = None;
    }
    if let Some(count) = setup.get("member_count").and_then(Value::as_u64) {
        e.overview.lock().unwrap().as_mut().unwrap().member_count = count;
    }
    if setup.get("no_bot").is_some() {
        e.overview.lock().unwrap().as_mut().unwrap().bot_avatar_url = String::new();
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
        "polls": table(db, "SELECT id,name,emoji,color,position,active,is_community,message_id,thread_id,closes_at FROM polls ORDER BY id"),
        "options": table(db, "SELECT id,poll_id,label,suggestion_id FROM options ORDER BY id"),
        "votes": table(db, "SELECT * FROM votes ORDER BY option_id,user_id"),
        "suggestions": table(db, "SELECT id,status,picked_by,picked_at,review_status FROM suggestions ORDER BY id"),
        "user_stats": table(db, "SELECT * FROM user_stats ORDER BY user_id"),
        "weekly_activity": table(db, "SELECT * FROM weekly_activity ORDER BY user_id,week"),
        "season_activity": table(db, "SELECT * FROM season_activity ORDER BY user_id,week"),
        "settings": table(db, "SELECT key,value FROM settings WHERE key NOT IN ('last_reset','stats_since','awards_since','schema_version') ORDER BY key"),
        "audit": table(db, "SELECT actor_id,action,entity_type,entity_id,reason,details FROM staff_audit ORDER BY id"),
    })
}

async fn call(e: &Env, method: &str, path: &str, body: Option<String>, authenticated: bool) -> (u16, axum::http::HeaderMap, String) {
    let mut builder = Request::builder().method(method).uri(path);
    if authenticated {
        builder = builder.header("Cookie", "sfo_site_session=s1");
    }
    let request = builder.body(Body::from(body.unwrap_or_default())).unwrap();
    let response = routes(e.state.clone()).oneshot(request).await.unwrap();
    let (parts, response_body) = response.into_parts();
    (parts.status.as_u16(), parts.headers, String::from_utf8(axum::body::to_bytes(response_body, usize::MAX).await.unwrap().to_vec()).unwrap())
}

/// Un'incorporazione come la serializza Python (`embed.to_dict()`), senza i campi che serenity scrive
/// vuoti e Python omette.
fn normalize_embed(embed: Value) -> Value {
    match embed {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(key, value)| !value.is_null() && key != "type" && key != "flags" && !value.as_array().is_some_and(Vec::is_empty))
                .map(|(key, value)| (key, normalize_embed(value)))
                .collect(),
        ),
        other => other,
    }
}

fn normalize_calls(calls: &[Value]) -> Vec<Value> {
    calls
        .iter()
        .map(|call| {
            let mut call = call.clone();
            if call[0] == "send" {
                call[1][2] = normalize_embed(call[1][2].clone());
            }
            call
        })
        .collect()
}

// --- le route ----------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn admin_routes_match_the_python_implementation() {
    let golden = golden();
    let scenarios = golden["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 597);
    assert_eq!(PREVIEW_SECONDS, 60);
    for case in scenarios {
        let label = case["label"].as_str().unwrap();
        let request = &case["request"];
        let mode = request["mode"].as_str().unwrap();
        let e = env(mode);
        apply_setup(&e, &case["setup"]);
        let body = request["body"].as_str().map(str::to_string);
        let (status, headers, text) = call(&e, request["method"].as_str().unwrap(), request["path"].as_str().unwrap(), body, mode != "none").await;

        assert_eq!(u64::from(status), case["status"].as_u64().unwrap(), "{label}: stato ({text})");
        assert_eq!(headers.get("cache-control").and_then(|value| value.to_str().ok()), case["headers"]["Cache-Control"].as_str(), "{label}: cache");
        if let Some(content_type) = case["headers"]["Content-Type"].as_str() {
            assert_eq!(headers["content-type"], content_type, "{label}: tipo");
        }
        if status == 413 {
            // aiohttp aggiunge la dimensione effettiva del corpo; qui basta il limite.
            assert!(case["body"].as_str().unwrap().starts_with(&text), "{label}: corpo");
        } else {
            assert_eq!(text, case["body"].as_str().unwrap(), "{label}: corpo");
        }

        // L'anteprima di un sondaggio sparisce dopo un minuto, e non prima.
        let mut expected_calls: Vec<Value> = case["calls"].as_array().unwrap().clone();
        if let Some(position) = expected_calls.iter().position(|call| call[0] == "sleep") {
            assert_eq!(expected_calls[position][1], json!([60]), "{label}: attesa");
            expected_calls.remove(position);
            let deleted = |calls: &Calls| calls.lock().unwrap().iter().any(|call| call[0] == "delete_message");
            // Il compito di eliminazione deve partire (e cominciare ad aspettare) prima di far scorrere il tempo.
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(Duration::from_secs(59)).await;
            tokio::task::yield_now().await;
            assert!(!deleted(&e.calls), "{label}: l'anteprima è sparita troppo presto");
            tokio::time::advance(Duration::from_secs(2)).await;
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(json!(normalize_calls(&e.calls.lock().unwrap())), json!(normalize_calls(&expected_calls)), "{label}: chiamate");
        assert_eq!(json!(TOPICS.with(|topics| topics.borrow().clone())), case["topics"], "{label}: argomenti");
        assert_eq!(dump(&e.db), case["db"], "{label}: database");
    }
}

// --- aiuti di Python ---------------------------------------------------------------------------

#[test]
fn int_arguments_follow_python_int() {
    assert_eq!(int_arg(&json!("910")), IntArg::Int(910));
    assert_eq!(int_arg(&json!(" 9_10 ")), IntArg::Int(910));
    assert_eq!(int_arg(&json!("٩١٠")), IntArg::Int(910));
    assert_eq!(int_arg(&json!(910.9)), IntArg::Int(910));
    assert_eq!(int_arg(&json!(-0.5)), IntArg::Int(0));
    assert_eq!(int_arg(&json!(true)), IntArg::Int(1));
    assert_eq!(int_arg(&json!(false)), IntArg::Int(0));
    assert_eq!(int_arg(&json!("99999999999999999999")), IntArg::Huge);
    assert_eq!(int_arg(&json!(10u64.pow(19))), IntArg::Huge);
    assert_eq!(int_arg(&json!(1e30)), IntArg::Huge);
    for invalid in [json!("abc"), json!(""), json!("1__0"), json!("_1"), json!("1_"), json!("9.1e2"), json!(null), json!([1]), json!({})] {
        assert_eq!(int_arg(&invalid), IntArg::Invalid, "{invalid}");
    }
}

#[test]
fn colors_accept_six_hex_digits_with_an_optional_hash() {
    for valid in ["#abcdef", "abcdef", "#ABCDEF", "#012345"] {
        assert!(valid_color(valid), "{valid}");
    }
    for invalid in ["#abc", "#abcdef0", "##abcdef", "#ggg000", "", "#", "#abcdef\n", " #abcdef", "#٠٠٠٠٠٠"] {
        assert!(!valid_color(invalid), "{invalid:?}");
    }
}

#[test]
fn guild_roles_are_ordered_like_discord_py() {
    let mut roles = [(5u16, 10u64), (1, 30), (1, 20), (5, 40)];
    roles.sort_by_key(|(position, id)| crate::site_auth::role_order(*position, *id));
    assert_eq!(roles, [(1, 30), (1, 20), (5, 40), (5, 10)]);
}

#[test]
fn gif_urls_are_checked_like_urlsplit() {
    assert_eq!(check_gif_url("https://cdn.example/a.gif"), GifCheck::Ok);
    assert_eq!(check_gif_url("HTTPS://cdn.example/a.gif"), GifCheck::Ok);
    assert_eq!(check_gif_url("https://@cdn.example/a.gif"), GifCheck::Ok);
    assert_eq!(check_gif_url("https://[::1]/a.gif"), GifCheck::Ok);
    assert_eq!(check_gif_url("https://u@cdn.example/a.gif"), GifCheck::Rejected);
    assert_eq!(check_gif_url("http://cdn.example/a.gif"), GifCheck::Rejected);
    assert_eq!(check_gif_url("https:///a.gif"), GifCheck::Rejected);
    assert_eq!(check_gif_url("https://[nope]/a.gif"), GifCheck::Crash);
    assert_eq!(check_gif_url("https://[::1/a.gif"), GifCheck::Crash);
}

#[test]
fn python_float_repr_matches_the_shortest_representation() {
    for (number, expected) in [
        (1.5, "1.5"),
        (1000.0, "1000.0"),
        (1e15, "1000000000000000.0"),
        (1e16, "1e+16"),
        (1e-4, "0.0001"),
        (1e-5, "1e-05"),
        (123456789.123, "123456789.123"),
        (-0.0, "-0.0"),
        (0.1 + 0.2, "0.30000000000000004"),
        (1.5e300, "1.5e+300"),
        (-2.5e-7, "-2.5e-07"),
    ] {
        assert_eq!(crate::site_reports::py_float_repr(number), expected, "{number}");
    }
}
