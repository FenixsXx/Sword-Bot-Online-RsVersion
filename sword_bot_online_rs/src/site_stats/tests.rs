use std::cell::Cell;

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

use super::*;
use crate::database::Database;
use crate::site_auth::tests::{FakeDirectory, FakeOAuth};
use crate::site_auth::{AuthConfig, GuildFacts, SiteAuth};

fn golden() -> Value {
    serde_json::from_str(include_str!("../golden/site_stats_expected.json")).unwrap()
}

/// 2027-04-10 12:00:00 (ora di Roma), l'istante «adesso» del golden.
const NOW: i64 = 1_807_351_200;

thread_local! {
    static MONO: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
}

fn fake_mono() -> std::time::Instant {
    MONO.with(|mono| mono.get().expect("orologio non impostato"))
}

struct Env {
    db: Database,
    data: Arc<PlatformData>,
    auth: Arc<SiteAuth>,
    base: std::time::Instant,
}

fn seed(db: &Database) {
    db.execute_batch(include_str!("../golden/site_stats_seed.sql")).unwrap();
}

fn env() -> Env {
    let base = std::time::Instant::now();
    MONO.with(|mono| mono.set(Some(base)));
    let db = Database::open_in_memory().unwrap();
    db.set_setting("guild_id", "100").unwrap();
    db.set_setting("week", "10").unwrap();
    seed(&db);
    let directory = FakeDirectory::default();
    {
        let mut state = directory.0.lock().unwrap();
        state.guild = Some(GuildFacts { owner_id: 1 });
        state.names.insert(1, "Alice".into());
        state.names.insert(2, "B".repeat(130));
        state.names.insert(3, "Carlo Ünï".into());
    }
    directory.add(1, true, &[]);
    db.execute(
        "INSERT INTO web_sessions(session_id,user_id,display_name,created_at,expires_at) VALUES('s1',1,'x','2027-01-01T00:00:00+00:00','2099-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();
    let data = Arc::new(PlatformData::new(db.clone(), Arc::new(directory.clone()), None).with_clocks(|| NOW, fake_mono));
    let auth = Arc::new(SiteAuth::new(db.clone(), Arc::new(directory), Arc::new(FakeOAuth::default()), AuthConfig::default(), None));
    Env { db, data, auth, base }
}

// --- le letture ---------------------------------------------------------------------------------

#[test]
fn reads_match_the_python_implementation() {
    let golden = golden();
    let e = env();
    let functions = golden["functions"].as_array().unwrap();
    assert_eq!(functions.len(), 41);
    for case in functions {
        let label = case["label"].as_str().unwrap();
        let args = case["args"][0].as_array().unwrap();
        let result = if label.starts_with("salute_") {
            e.data.community_health(args.first().and_then(Value::as_i64))
        } else if label.starts_with("mappa_") {
            e.data.activity_heatmap(args[0].as_i64().unwrap())
        } else if label.starts_with("ricerca_") {
            e.data.historical_search(args[0].as_str().unwrap(), args[1].as_i64().unwrap())
        } else if label.starts_with("eventi_") {
            e.data.event_analytics(args[0].as_i64().unwrap())
        } else {
            match label {
                "precisione" => e.data.staff_selection_accuracy(),
                "panoramica" => e.data.overview(),
                "record" => e.data.records(),
                other => panic!("lettura sconosciuta: {other}"),
            }
        };
        match (result, case.get("value"), case.get("error")) {
            (Ok(value), Some(expected), None) => assert_eq!(&value, expected, "{label}"),
            (Err(DataError::Value(message)), None, Some(expected)) => assert_eq!(message, expected[1].as_str().unwrap(), "{label}"),
            (other, ..) => panic!("{label}: risultato inatteso {other:?}"),
        }
    }
}

#[test]
fn period_weeks_match_python() {
    for case in golden()["period_weeks"].as_array().unwrap() {
        let (week, days, back) = (case[0].as_i64().unwrap(), case[1].as_i64().unwrap(), case[2].as_i64().unwrap());
        assert_eq!(json!(period_weeks_pair(week, days, back)), case[3], "{case}");
    }
}

fn period_weeks_pair(week: i64, days: i64, back: i64) -> [i64; 2] {
    let (first, last) = crate::platform_data::period_weeks(week, days, back);
    [first, last]
}

#[test]
fn reads_are_cached_for_fifteen_seconds() {
    let golden = golden();
    let cache = golden["cache"].as_array().unwrap();
    let expected = |label: &str| cache.iter().find(|entry| entry[0] == label).unwrap_or_else(|| panic!("golden senza {label}"))[1].clone();
    let e = env();
    let audit = |e: &Env| e.data.overview().unwrap()["audit_entries"].clone();
    let add_audit = |e: &Env, action: &str| e.db.execute("INSERT INTO staff_audit(occurred_at,actor_id,action) VALUES('x',1,?)", [action]).unwrap();
    assert_eq!(audit(&e), json!(3));
    add_audit(&e, "nuova");
    assert_eq!(audit(&e), expected("subito_dopo"));
    MONO.with(|mono| mono.set(Some(e.base + Duration::from_secs_f64(14.9))));
    assert_eq!(audit(&e), expected("dopo_14_9"));
    MONO.with(|mono| mono.set(Some(e.base + Duration::from_secs(15))));
    assert_eq!(audit(&e), expected("dopo_15_0"));
    add_audit(&e, "ancora");
    assert_eq!(audit(&e), expected("ancora_in_cache"));
    e.data.clear_read_cache();
    assert_eq!(audit(&e), expected("dopo_clear"));

    // Chiavi diverse: senza argomenti e con 30 giorni sono due letture distinte.
    e.data.clear_read_cache();
    let proposals = |value: Value| value["metrics"]["proposals"].clone();
    let a = proposals(e.data.community_health(None).unwrap());
    e.db.execute("INSERT INTO community_activity(action,occurred_at) VALUES('proposal',?)", [NOW - 86400]).unwrap();
    let b = proposals(e.data.community_health(Some(30)).unwrap());
    let d = proposals(e.data.community_health(None).unwrap());
    assert_eq!(json!([a, b, b.clone(), d]), expected("chiavi"));
    // Quello che si riceve è una copia: cambiarla non altera la cache.
    let mut copy = e.data.overview().unwrap();
    copy["audit_entries"] = json!(-1);
    assert_ne!(e.data.overview().unwrap()["audit_entries"], json!(-1));
}

// --- le route -----------------------------------------------------------------------------------

async fn get(e: &Env, path: &str, authenticated: bool) -> (u16, axum::http::HeaderMap, String) {
    let mut builder = Request::builder().uri(path);
    if authenticated {
        builder = builder.header("Cookie", "sfo_site_session=s1");
    }
    let state = StatsState { auth: e.auth.clone(), data: e.data.clone() };
    let response = routes(state).oneshot(builder.body(Body::empty()).unwrap()).await.unwrap();
    let (parts, body) = response.into_parts();
    (parts.status.as_u16(), parts.headers, String::from_utf8(axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap())
}

#[tokio::test]
async fn stats_routes_match_the_python_implementation() {
    let golden = golden();
    let routes_golden = golden["routes"].as_array().unwrap();
    assert_eq!(routes_golden.len(), 49);
    let e = env();
    for case in routes_golden {
        let label = case["label"].as_str().unwrap();
        let path = case["path"].as_str().unwrap();
        // Le modifiche del golden alla base di dati fra una richiesta e l'altra.
        match label {
            "panoramica_in_cache" => {
                e.db.execute("INSERT INTO staff_audit(occurred_at,actor_id,action) VALUES('x',1,'dopo')", []).unwrap();
            }
            "panoramica_dopo_notifica" => e.data.clear_read_cache(),
            "database_in_errore_panoramica" => {
                e.data.clear_read_cache();
                e.db.execute_batch("ALTER TABLE community_records RENAME TO community_records_off; ALTER TABLE live_events RENAME TO live_events_off").unwrap();
            }
            "istantanea_non_oggetto" => {
                e.db.execute_batch("ALTER TABLE live_events_off RENAME TO live_events; ALTER TABLE community_records_off RENAME TO community_records").unwrap();
                e.data.clear_read_cache();
                e.db.execute("INSERT INTO history(week,closed_at,data) VALUES(11,'x','[1,2]')", []).unwrap();
            }
            "voti_non_numerici" => {
                e.db.execute("DELETE FROM history WHERE week=11", []).unwrap();
                e.data.clear_read_cache();
                let data = json!({"polls": [{"is_community": true, "options": [{"label": "a", "votes": "x"}, {"label": "b", "votes": 2}]}], "suggestions": []});
                e.db.execute("INSERT INTO history(week,closed_at,data) VALUES(12,'x',?)", [data.to_string()]).unwrap();
            }
            "picked_by_non_valido" => {
                e.db.execute("DELETE FROM history WHERE week=12", []).unwrap();
                e.data.clear_read_cache();
                let data = json!({"polls": [], "suggestions": [{"name": "a", "status": "picked", "picked_by": "abc"}]});
                e.db.execute("INSERT INTO history(week,closed_at,data) VALUES(13,'x',?)", [data.to_string()]).unwrap();
            }
            _ => {}
        }
        let (status, headers, body) = get(&e, path, !label.starts_with("non_autenticato")).await;
        assert_eq!(u64::from(status), case["status"].as_u64().unwrap(), "{label}: stato");
        assert_eq!(body, case["body"].as_str().unwrap(), "{label}: corpo");
        assert_eq!(headers.get("cache-control").and_then(|value| value.to_str().ok()), case["headers"]["Cache-Control"].as_str(), "{label}: cache");
        if label != "istantanea_non_oggetto" {
            assert_eq!(headers["content-type"], case["headers"]["Content-Type"].as_str().unwrap(), "{label}: tipo");
        }
    }
}

#[tokio::test]
async fn the_bot_clearing_the_cache_is_what_makes_new_data_appear() {
    let e = env();
    let before = get(&e, "/api/site/stats/overview", true).await.2;
    e.db.execute("INSERT INTO staff_audit(occurred_at,actor_id,action) VALUES('x',1,'nuova')", []).unwrap();
    assert_eq!(get(&e, "/api/site/stats/overview", true).await.2, before);
    e.data.clear_read_cache();
    assert_ne!(get(&e, "/api/site/stats/overview", true).await.2, before);
}

#[test]
fn route_constants_match_python() {
    let constants = &golden()["constants"];
    assert_eq!(READ_CACHE_TTL.as_secs(), constants["ttl"].as_u64().unwrap());
    assert_eq!(json!([DAYS_RANGE.0, DAYS_RANGE.1]), constants["days"]);
    assert_eq!(json!([EVENTS_LIMIT_RANGE.0, EVENTS_LIMIT_RANGE.1]), constants["events"]);
    assert_eq!(json!([HISTORY_LIMIT_RANGE.0, HISTORY_LIMIT_RANGE.1]), constants["history"]);
    assert_eq!(json!([HISTORY_QUERY_LENGTH.0, HISTORY_QUERY_LENGTH.1]), constants["query"]);
}

use crate::platform_data::READ_CACHE_TTL;
use std::time::Duration;
