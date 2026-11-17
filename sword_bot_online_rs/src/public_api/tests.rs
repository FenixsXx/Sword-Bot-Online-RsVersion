use std::cell::Cell;

use axum::http::Request;
use chrono::TimeZone;
use tower::ServiceExt;

use super::*;
use crate::site_auth::tests::FakeDirectory;

fn golden() -> &'static Value {
    static GOLDEN: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    GOLDEN.get_or_init(|| serde_json::from_str(include_str!("../golden/public_api_expected.json")).unwrap())
}

thread_local! {
    static NOW: Cell<i64> = const { Cell::new(0) };
}

fn clock() -> DateTime<Tz> {
    crate::config::TZ.timestamp_opt(NOW.get(), 0).unwrap()
}

fn default_now() -> i64 {
    crate::config::TZ.with_ymd_and_hms(2027, 3, 5, 21, 0, 0).unwrap().timestamp()
}

const NAMES: [(u64, &str); 4] = [(101, "Ada Lovelace"), (102, "Èlena 🎮"), (103, ""), (104, "Dino")];

fn state(case: &Value) -> PublicApiState {
    let db = Database::open_in_memory().unwrap();
    for statement in golden()["seed"].as_array().unwrap() {
        db.execute_batch(statement.as_str().unwrap()).unwrap();
    }
    let setup = &case["setup"];
    for statement in setup["sql"].as_array().into_iter().flatten() {
        db.execute_batch(statement.as_str().unwrap()).unwrap();
    }
    for (key, value) in setup["settings"].as_object().into_iter().flatten() {
        db.set_setting(key, value.as_str().unwrap()).unwrap();
    }
    let directory = FakeDirectory::default();
    if setup.get("no_guild").is_none() {
        let mut guard = directory.0.lock().unwrap();
        for (id, name) in NAMES {
            guard.names.insert(id, if id == 103 { "N".repeat(150) } else { name.to_string() });
        }
    }
    NOW.set(match setup["now"].as_str() {
        Some(text) => DateTime::parse_from_rfc3339(text).unwrap().timestamp(),
        None => default_now(),
    });
    let data = Arc::new(PlatformData::new(db.clone(), Arc::new(directory), None));
    PublicApiState::new(db, data, case["request"]["token"].as_str().unwrap_or(""), clock)
}

#[tokio::test]
async fn public_api_matches_the_python_implementation() {
    let golden = golden();
    assert_eq!(COMMUNITY_HTML, golden["template"].as_str().unwrap());
    let scenarios = golden["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 307);
    for case in scenarios {
        let label = case["label"].as_str().unwrap();
        let request = &case["request"];
        let mut builder = Request::builder().method(request["method"].as_str().unwrap()).uri(request["path"].as_str().unwrap());
        for (name, value) in request["headers"].as_object().unwrap() {
            builder = builder.header(name.as_str(), value.as_str().unwrap().as_bytes());
        }
        let response = routes(state(case)).oneshot(builder.body(axum::body::Body::empty()).unwrap()).await.unwrap();
        let (parts, body) = response.into_parts();
        let text = String::from_utf8(axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap();
        let status = u64::from(parts.status.as_u16());
        assert_eq!(status, case["status"].as_u64().unwrap(), "{label}: stato ({text})");
        if status == 405 || status == 404 {
            continue;
        }
        let mut seen = serde_json::Map::new();
        for name in ["content-type", "cache-control", "www-authenticate", "content-security-policy", "referrer-policy", "x-content-type-options"] {
            if let Some(value) = parts.headers.get(name) {
                seen.insert(name.to_string(), json!(value.to_str().unwrap()));
            }
        }
        let expected: serde_json::Map<String, Value> = case["headers"].as_object().unwrap().iter().map(|(name, value)| (name.to_lowercase(), value.clone())).collect();
        assert_eq!(Value::Object(seen), Value::Object(expected), "{label}: intestazioni");
        assert_eq!(text, case["body"].as_str().unwrap(), "{label}: corpo");
    }
}

#[test]
fn tokens_must_be_long_and_without_outer_spaces() {
    let db = Database::open_in_memory().unwrap();
    let data = Arc::new(PlatformData::new(db.clone(), Arc::new(FakeDirectory::default()), None));
    let token = |raw: &str| PublicApiState::new(db.clone(), data.clone(), raw, clock).token.to_string();
    assert_eq!(token(&"t".repeat(32)), "t".repeat(32));
    assert_eq!(token(&"é".repeat(32)), "é".repeat(32));
    assert_eq!(token(&"t".repeat(31)), "");
    assert_eq!(token(&format!(" {}", "t".repeat(40))), "");
    assert_eq!(token(&format!("{}\n", "t".repeat(40))), "");
    assert_eq!(token(""), "");
}

#[test]
fn iso_datetimes_read_like_python() {
    let micros = |text: &str| parse_iso_datetime(text);
    assert_eq!(micros("1970-01-01T00:00:01+00:00"), Some((1_000_000, true)));
    assert_eq!(micros("1970-01-01T01:00:00+01:00"), Some((0, true)));
    assert_eq!(micros("1970-01-01 00:00:00.5"), Some((500_000, false)));
    assert_eq!(micros("1970-01-02"), Some((86_400_000_000, false)));
    assert_eq!(micros("19700102T000000Z"), Some((86_400_000_000, true)));
    for invalid in ["", "ieri", "1970-01-01T25:00:00", "1970-13-01", "1970-01-01T"] {
        assert_eq!(micros(invalid), None, "{invalid:?}");
    }
}
