use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Mutex, Once};

use axum::http::Request;
use chrono::TimeZone;
use tower::ServiceExt;

use super::*;
use crate::discord_api::testing::FakeApi;
use crate::notify::testing::FakeSender;
use crate::notify::Notifier;
use crate::polls::RefreshScheduler;
use crate::publisher::Publisher;
use crate::site_auth::tests::{FakeDirectory, FakeOAuth};
use crate::site_auth::{AuthConfig, GuildFacts};
use crate::site_backend::DiscordBackend;

fn golden() -> Value {
    serde_json::from_str(include_str!("../golden/site_reports_expected.json")).unwrap()
}

// --- validazione -------------------------------------------------------------------------------

#[test]
fn constants_match_python() {
    let constants = &golden()["constants"];
    let mut categories: Vec<&str> = REPORT_CATEGORIES.to_vec();
    categories.sort_unstable();
    assert_eq!(json!(categories), constants["categories"]);
    assert_eq!(json!(REPORT_DURATIONS), constants["durations"]);
    assert_eq!(REPORT_COLUMNS, constants["columns"].as_str().unwrap());
}

#[test]
fn validation_matches_python() {
    let golden = golden();
    let cases = golden["validation"].as_array().unwrap();
    assert_eq!(cases.len(), 179);
    for case in cases {
        let label = case["label"].as_str().unwrap();
        let result = validate_report(&case["payload"]);
        match (&result, case["error"].as_str(), case["values"].as_object()) {
            (Ok(values), None, Some(expected)) => {
                assert_eq!(&Value::Object(values.clone()), &Value::Object(expected.clone()), "{label}");
                // Anche l'ordine dei campi è quello di Python.
                assert_eq!(values.keys().collect::<Vec<_>>(), expected.keys().collect::<Vec<_>>(), "{label}: ordine");
            }
            (Err(Invalid::Message(message)), Some(expected), None) => assert_eq!(message, expected, "{label}"),
            (Err(Invalid::Crash), Some("ECCEZIONE TypeError"), None) => {}
            other => panic!("{label}: risultato inatteso {other:?}"),
        }
    }
}

#[test]
fn uuids_are_read_like_python() {
    let canonical = "123e4567-e89b-12d3-a456-426614174000";
    for text in [canonical.to_string(), canonical.to_uppercase(), canonical.replace('-', ""), format!("{{{canonical}}}"), format!("urn:uuid:{canonical}"), format!("{{{}}}", canonical.to_uppercase())] {
        assert_eq!(parse_uuid(&text).as_deref(), Some(canonical), "{text}");
    }
    for text in ["", "non-un-uuid", "123e4567-e89b-12d3-a456-42661417400", "123e4567-e89b-12d3-a456-4266141740000", "zz3e4567-e89b-12d3-a456-426614174000"] {
        assert_eq!(parse_uuid(text), None, "{text}");
    }
}

#[test]
fn python_str_and_repr_cover_the_values_a_row_can_hold() {
    assert_eq!(py_str(&json!(null)), "None");
    assert_eq!(py_str(&json!(true)), "True");
    assert_eq!(py_str(&json!(false)), "False");
    assert_eq!(py_str(&json!(3.5)), "3.5");
    assert_eq!(py_str(&json!(2.0)), "2.0");
    assert_eq!(py_str(&json!(7)), "7");
    assert_eq!(py_str(&json!("testo")), "testo");
    assert_eq!(py_str(&json!([1, "a", null, true])), "[1, 'a', None, True]");
    assert_eq!(py_str(&json!({"a": [1.5]})), "{'a': [1.5]}");
    assert_eq!(py_str(&json!(["l'uno"])), "[\"l'uno\"]");
    assert_eq!(py_strip(" \u{1c}ciao\t\n "), "ciao");
}

// --- le route ----------------------------------------------------------------------------------

type Plan = Result<StoreReply, StoreError>;

#[derive(Default)]
struct FakeStore {
    plan: Mutex<VecDeque<Plan>>,
    calls: Mutex<Vec<Value>>,
}

impl ReportStore for FakeStore {
    fn request<'a>(&'a self, call: StoreCall<'a>) -> BoxFuture<'a, Result<StoreReply, StoreError>> {
        let params: Map<String, Value> = call.params.iter().map(|(key, value)| ((*key).to_string(), json!(value))).collect();
        self.calls.lock().unwrap().push(json!([call.method, params, call.payload, call.prefer]));
        let reply = self.plan.lock().unwrap().pop_front().expect("risposta di Supabase non preparata");
        Box::pin(async move { reply })
    }
}

thread_local! {
    static TOPICS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn capture_topics() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| crate::hooks::add_site_change_listener(Arc::new(|topic| TOPICS.with(|topics| topics.borrow_mut().push(topic.to_string())))));
}

fn rome_clock() -> chrono::DateTime<chrono_tz::Tz> {
    crate::config::TZ.with_ymd_and_hms(2027, 3, 5, 21, 0, 0).unwrap()
}

struct Env {
    db: Database,
    api: FakeApi,
    store: Arc<FakeStore>,
    state: ReportsState,
}

const REPORT_ID: &str = "123e4567-e89b-12d3-a456-426614174000";

fn env_with(url: &str, key: &str) -> Env {
    capture_topics();
    TOPICS.with(|topics| topics.borrow_mut().clear());
    let db = Database::open_in_memory().unwrap();
    db.set_setting("guild_id", "100").unwrap();
    db.execute(
        "INSERT INTO web_sessions(session_id,user_id,display_name,created_at,expires_at) VALUES('s1',42,'Staff','2027-01-01T00:00:00+00:00','2099-01-01T00:00:00+00:00')",
        [],
    )
    .unwrap();
    let directory = FakeDirectory::default();
    directory.0.lock().unwrap().guild = Some(GuildFacts { owner_id: 1 });
    directory.add(42, true, &[]);
    let auth = Arc::new(SiteAuth::new(db.clone(), Arc::new(directory), Arc::new(FakeOAuth::default()), AuthConfig::default(), None));
    let api = FakeApi::default();
    let publisher = Arc::new(Publisher::new(api.clone(), db.clone()));
    let backend: Arc<dyn SiteBackend> =
        Arc::new(DiscordBackend { db: db.clone(), notifier: Notifier::new(db.clone(), FakeSender::default()), publisher, scheduler: RefreshScheduler::new() });
    let store = Arc::new(FakeStore::default());
    let state = ReportsState {
        auth,
        db: db.clone(),
        backend,
        store: store.clone(),
        supabase_url: url.to_string(),
        service_key: key.to_string(),
        fallback_guild_id: None,
        send_lock: Arc::new(tokio::sync::Mutex::new(())),
        clock: rome_clock,
    };
    Env { db, api, store, state }
}

fn env() -> Env {
    env_with("https://sb.example.co", "chiave-segreta")
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

fn rows(text: &str) -> Plan {
    if text.is_empty() {
        return Ok(StoreReply::Rows(json!([])));
    }
    serde_json::from_str(text).map(StoreReply::Rows).map_err(|_| StoreError::Unreachable)
}

fn failed() -> Plan {
    Ok(StoreReply::Failed)
}

fn unreachable() -> Plan {
    Err(StoreError::Unreachable)
}

fn row() -> Value {
    golden()["row"].clone()
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

/// La parte di un embed che conta (quella che Python mette in `to_dict()`).
fn embed_view(message: &crate::discord_api::testing::StoredMessage) -> Value {
    let embed = &message.embeds[0];
    json!({"footer": embed["footer"], "fields": embed["fields"], "color": embed["color"], "title": embed["title"]})
}

fn base_payload() -> String {
    golden()["validation"][0]["payload"].to_string()
}

#[tokio::test]
async fn report_routes_match_the_python_implementation() {
    let golden = golden();
    let scenarios = golden["routes"].as_array().unwrap();
    assert_eq!(scenarios.len(), 70);
    let ok = |text: &str| rows(text);
    let valid = base_payload();
    let one_row = json!([row()]).to_string();
    let only_id = json!([{"id": REPORT_ID}]).to_string();
    for case in scenarios {
        let label = case["label"].as_str().unwrap();
        if let Some(configuration) = label.strip_prefix("configurazione_") {
            let e = match configuration {
                "senza_url" => env_with("", "k"),
                "url_http" => env_with("http://sb.example.co", "k"),
                "senza_chiave" => env_with("https://sb.example.co", ""),
                other => panic!("caso sconosciuto: {other}"),
            };
            let (status, headers, body) = call(&e, "GET", "/api/site/reports", None, true).await;
            assert_eq!(u64::from(status), case["status"].as_u64().unwrap(), "{label}");
            assert_eq!(body, case["body"].as_str().unwrap(), "{label}");
            assert_eq!(headers["cache-control"], "no-store");
            continue;
        }
        let e = env();
        let path = |suffix: &str| format!("/api/site/reports{suffix}");
        let id_path = path(&format!("/{REPORT_ID}"));
        let publish_path = path(&format!("/{REPORT_ID}/publish"));
        let (mut method, mut uri, mut body, mut plan): (&str, String, Option<String>, Vec<Plan>) = ("GET", path(""), None, vec![]);
        let channel = |kind: ChannelKind| {
            e.db.set_setting("event_report_channel", 700).unwrap();
            e.api.set_channel_kind(700, kind);
        };
        match label {
            "elenco_non_autenticato" => {}
            "elenco" | "elenco_limite_1000" => plan = vec![ok(&one_row)],
            "elenco_paginato" => (uri, plan) = (path("?offset=20&limit=5"), vec![ok(&one_row)]),
            "elenco_spazi" => (uri, plan) = (path("?offset=%205%20&limit=%2010"), vec![ok(&one_row)]),
            "elenco_offset_negativo" => uri = path("?offset=-1"),
            "elenco_limite_zero" => uri = path("?limit=0"),
            "elenco_limite_1001" => uri = path("?limit=1001"),
            "elenco_testo" => uri = path("?limit=x"),
            "elenco_vuoto_offset" => uri = path("?offset="),
            "elenco_supabase_http_errore" => plan = vec![failed()],
            "elenco_supabase_corpo_vuoto" => plan = vec![ok("")],
            "elenco_supabase_rete" | "elenco_supabase_timeout" | "elenco_supabase_json_non_valido" => plan = vec![unreachable()],
            "elenco_supabase_oggetto" => plan = vec![ok("{\"a\": 1}")],
            "crea_non_autenticato" => (method, body) = ("POST", Some(valid.clone())),
            "crea_ok" => (method, body, plan) = ("POST", Some(valid.clone()), vec![ok(&one_row)]),
            "crea_json_non_valido" => (method, body) = ("POST", Some("non json".into())),
            "crea_corpo_vuoto" => (method, body) = ("POST", Some(String::new())),
            "crea_non_oggetto" => (method, body) = ("POST", Some("[1]".into())),
            "crea_validazione" => {
                let mut payload: Value = serde_json::from_str(&valid).unwrap();
                payload["duration"] = json!(7);
                (method, body) = ("POST", Some(payload.to_string()));
            }
            "crea_supabase_errore" => (method, body, plan) = ("POST", Some(valid.clone()), vec![failed()]),
            "crea_supabase_vuoto" | "crea_supabase_lista_vuota" => (method, body, plan) = ("POST", Some(valid.clone()), vec![ok(if label.ends_with("vuoto") { "" } else { "[]" })]),
            "crea_supabase_rete" => (method, body, plan) = ("POST", Some(valid.clone()), vec![unreachable()]),
            "crea_corpo_troppo_grande" => (method, body) = ("POST", Some(json!({"notes": "x".repeat(70000)}).to_string())),
            "aggiorna_non_autenticato" => (method, uri, body) = ("PUT", id_path.clone(), Some(valid.clone())),
            "aggiorna_ok" | "aggiorna_non_trovato" | "aggiorna_supabase_errore" | "aggiorna_supabase_rete" => {
                plan = vec![match label {
                    "aggiorna_ok" => ok(&only_id),
                    "aggiorna_non_trovato" => ok("[]"),
                    "aggiorna_supabase_errore" => failed(),
                    _ => unreachable(),
                }];
                (method, uri, body) = ("PUT", id_path.clone(), Some(valid.clone()));
            }
            "aggiorna_id_maiuscolo" => (method, uri, body, plan) = ("PUT", path(&format!("/{}", REPORT_ID.to_uppercase())), Some(valid.clone()), vec![ok(&only_id)]),
            "aggiorna_id_senza_trattini" => (method, uri, body, plan) = ("PUT", path(&format!("/{}", REPORT_ID.replace('-', ""))), Some(valid.clone()), vec![ok(&only_id)]),
            "aggiorna_id_con_graffe" => (method, uri, body, plan) = ("PUT", path(&format!("/%7B{REPORT_ID}%7D")), Some(valid.clone()), vec![ok(&only_id)]),
            "aggiorna_id_urn" => (method, uri, body, plan) = ("PUT", path(&format!("/urn:uuid:{REPORT_ID}")), Some(valid.clone()), vec![ok(&only_id)]),
            "aggiorna_id_non_valido" => (method, uri, body) = ("PUT", path("/non-un-uuid"), Some(valid.clone())),
            "aggiorna_json_non_valido" => (method, uri, body) = ("PUT", id_path.clone(), Some("x".into())),
            "aggiorna_validazione" => {
                let mut payload: Value = serde_json::from_str(&valid).unwrap();
                payload["category"] = json!("Altro");
                (method, uri, body) = ("PUT", id_path.clone(), Some(payload.to_string()));
            }
            "elimina_non_autenticato" => (method, uri) = ("DELETE", id_path.clone()),
            "elimina_ok" => (method, uri, plan) = ("DELETE", id_path.clone(), vec![ok(&only_id)]),
            "elimina_con_notifica_locale" | "elimina_errore_database_locale" => {
                e.db.execute("INSERT INTO site_report_notifications(report_id,message_id,sent_at) VALUES(?,?,?)", params![REPORT_ID, "1", "x"]).unwrap();
                if label.ends_with("locale") && label.contains("errore") {
                    e.db.execute_batch("CREATE TRIGGER fail_delete_notification BEFORE DELETE ON site_report_notifications BEGIN SELECT RAISE(ABORT,'boom'); END").unwrap();
                }
                (method, uri, plan) = ("DELETE", id_path.clone(), vec![ok(&only_id)]);
            }
            "elimina_id_non_valido" => (method, uri) = ("DELETE", path("/zzz")),
            "elimina_non_trovato" => (method, uri, plan) = ("DELETE", id_path.clone(), vec![ok("[]")]),
            "elimina_supabase_errore" => (method, uri, plan) = ("DELETE", id_path.clone(), vec![failed()]),
            "elimina_supabase_rete" => (method, uri, plan) = ("DELETE", id_path.clone(), vec![unreachable()]),
            "pubblica_non_autenticato" => (method, uri) = ("POST", publish_path.clone()),
            "pubblica_id_non_valido" => (method, uri) = ("POST", path("/x/publish")),
            "pubblica_senza_canale" => (method, uri) = ("POST", publish_path.clone()),
            "pubblica_ok" | "pubblica_canale_recuperato" => {
                channel(ChannelKind::Text);
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok(&one_row)]);
            }
            "pubblica_report_con_campi_vuoti" => {
                channel(ChannelKind::Text);
                let data = json!([{"id": REPORT_ID, "category": null, "event_time": null, "clip_recorded": false, "flop": true, "new_member_names": "", "notes": "", "name": ""}]).to_string();
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok(&data)]);
            }
            "pubblica_report_minimo" => {
                channel(ChannelKind::Text);
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok(&only_id)]);
            }
            "pubblica_report_lungo" => {
                channel(ChannelKind::Text);
                let mut data = row();
                for (key, value) in [("name", "N".repeat(300)), ("notes", "n".repeat(2000)), ("responsible", "r".repeat(2000)), ("new_member_names", "m".repeat(1500)), ("clip_registrar", String::new()), ("event_time", "21:00:00".into())] {
                    data[key] = json!(value);
                }
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok(&json!([data]).to_string())]);
            }
            "pubblica_report_con_tipi_strani" => {
                channel(ChannelKind::Text);
                let mut data = row();
                for (key, value) in [("attendees", json!(3.5)), ("duration", json!(null)), ("new_members", json!(true)), ("event_date", json!(null)), ("responsible", json!(7))] {
                    data[key] = value;
                }
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok(&json!([data]).to_string())]);
            }
            "pubblica_report_non_trovato" => {
                channel(ChannelKind::Text);
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok("[]")]);
            }
            "pubblica_supabase_errore" => {
                channel(ChannelKind::Text);
                (method, uri, plan) = ("POST", publish_path.clone(), vec![failed()]);
            }
            "pubblica_supabase_rete" => {
                channel(ChannelKind::Text);
                (method, uri, plan) = ("POST", publish_path.clone(), vec![unreachable()]);
            }
            "pubblica_gia_inviato" => {
                channel(ChannelKind::Text);
                e.db.execute("INSERT INTO site_report_notifications(report_id,message_id,sent_at) VALUES(?,?,?)", params![REPORT_ID, "5", "x"]).unwrap();
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok(&one_row)]);
            }
            "pubblica_canale_non_valido" => {
                e.db.set_setting("event_report_channel", "abc").unwrap();
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok(&one_row)]);
            }
            "pubblica_canale_non_trovato" | "pubblica_canale_vietato" => {
                e.db.set_setting("event_report_channel", 700).unwrap();
                e.api.fail_channel(700, if label.ends_with("vietato") { ApiError::Forbidden } else { ApiError::NotFound });
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok(&one_row)]);
            }
            "pubblica_canale_vocale" => {
                channel(ChannelKind::Voice);
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok(&one_row)]);
            }
            "pubblica_invio_vietato" | "pubblica_invio_http" => {
                channel(ChannelKind::Text);
                e.api.fail_send(Some(if label.ends_with("vietato") { ApiError::Forbidden } else { ApiError::Http("500".into()) }));
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok(&one_row)]);
            }
            "pubblica_registrazione_fallita" => {
                channel(ChannelKind::Text);
                e.db.execute_batch("CREATE TRIGGER fail_notification BEFORE INSERT ON site_report_notifications BEGIN SELECT RAISE(ABORT,'boom'); END").unwrap();
                (method, uri, plan) = ("POST", publish_path.clone(), vec![ok(&one_row)]);
            }
            other => panic!("scenario sconosciuto: {other}"),
        }
        e.store.plan.lock().unwrap().extend(plan);
        let (status, headers, text) = call(&e, method, &uri, body, !label.contains("non_autenticato")).await;

        assert_eq!(u64::from(status), case["status"].as_u64().unwrap(), "{label}: stato");
        if label == "crea_corpo_troppo_grande" {
            continue;
        }
        assert_eq!(headers["cache-control"], "no-store", "{label}");
        assert_eq!(headers["content-type"], case["headers"]["Content-Type"].as_str().unwrap(), "{label}: tipo");
        assert_eq!(text, case["body"].as_str().unwrap(), "{label}: corpo");
        // Le richieste a Supabase (metodo, parametri, corpo e preferenza).
        let expected_calls: Vec<Value> = case["calls"].as_array().unwrap().iter().map(|call| json!([call[1], call[3], call[4], call[5]["Prefer"]])).collect();
        assert_eq!(json!(e.store.calls.lock().unwrap().clone()), json!(expected_calls), "{label}: chiamate a Supabase");
        assert_eq!(table(&e.db, "SELECT actor_id,action,entity_type,entity_id,reason,details FROM staff_audit ORDER BY id"), case["audit"], "{label}: audit");
        assert_eq!(json!(TOPICS.with(|topics| topics.borrow().clone())), case["topics"], "{label}: argomenti");
        // Il finto di Python numera i messaggi da 9001, quello di Rust da 1001.
        let expected_notifications = case["notifications"].to_string().replace("\"9001\"", "\"1001\"");
        assert_eq!(table(&e.db, "SELECT report_id,message_id,sent_at FROM site_report_notifications").to_string(), expected_notifications, "{label}: notifiche");
        let sent: Vec<Value> = e.api.messages_in(700).iter().map(embed_view).collect();
        let expected_sent: Vec<Value> = case["sent"].as_array().unwrap().iter().map(|sent| sent["embed"].clone()).collect();
        assert_eq!(json!(sent), json!(expected_sent), "{label}: embed pubblicato");
    }
}

// --- Supabase vero -----------------------------------------------------------------------------

#[tokio::test]
async fn the_supabase_client_sends_the_headers_and_parameters_python_does() {
    use axum::extract::Query;
    use axum::http::HeaderMap as Headers;
    use std::collections::HashMap;
    let app = axum::Router::new().route(
        "/rest/v1/reports",
        axum::routing::any(|method: axum::http::Method, headers: Headers, Query(query): Query<HashMap<String, String>>, body: String| async move {
            let status = match query.get("select").map(String::as_str) {
                Some("errore") => 409,
                Some("vuoto") => 204,
                Some("rotto") => 200,
                _ => 200,
            };
            let payload = match query.get("select").map(String::as_str) {
                Some("vuoto") => String::new(),
                Some("rotto") => "non json".to_string(),
                Some("errore") => "duplicato".to_string(),
                _ => json!({
                    "method": method.as_str(),
                    "apikey": headers.get("apikey").and_then(|v| v.to_str().ok()),
                    "authorization": headers.get("authorization").and_then(|v| v.to_str().ok()),
                    "content_type": headers.get("content-type").and_then(|v| v.to_str().ok()),
                    "prefer": headers.get("prefer").and_then(|v| v.to_str().ok()),
                    "query": query,
                    "body": body,
                })
                .to_string(),
            };
            (axum::http::StatusCode::from_u16(status).unwrap(), payload)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let store = SupabaseStore::new(&format!("http://{address}/"), "chiave");
    let payload = json!({"name": "x"});
    let reply = store
        .request(StoreCall { method: "PATCH", params: vec![("id", "eq.1".into()), ("select", "id,name".into())], payload: Some(&payload), prefer: Some("return=representation") })
        .await;
    let Ok(StoreReply::Rows(echo)) = reply else { panic!("{reply:?}") };
    assert_eq!(echo["method"], "PATCH");
    assert_eq!(echo["apikey"], "chiave");
    assert_eq!(echo["authorization"], "Bearer chiave");
    assert_eq!(echo["content_type"], "application/json");
    assert_eq!(echo["prefer"], "return=representation");
    assert_eq!(echo["query"], json!({"id": "eq.1", "select": "id,name"}));
    assert_eq!(serde_json::from_str::<Value>(echo["body"].as_str().unwrap()).unwrap(), payload);
    let get = |select: &str| StoreCall { method: "GET", params: vec![("select", select.to_string())], payload: None, prefer: None };
    assert_eq!(store.request(get("errore")).await, Ok(StoreReply::Failed));
    assert_eq!(store.request(get("vuoto")).await, Ok(StoreReply::Rows(json!([]))));
    assert_eq!(store.request(get("rotto")).await, Err(StoreError::Unreachable));
    let nowhere = SupabaseStore::new("http://127.0.0.1:1", "chiave");
    assert_eq!(nowhere.request(get("id")).await, Err(StoreError::Unreachable));
}
