use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Mutex;

use axum::body::Body;
use axum::http::Request;
use chrono::TimeZone;
use tower::ServiceExt;

use super::*;
use crate::site_common::{constant_time_equals, json_error};

fn golden() -> Value {
    serde_json::from_str(include_str!("../golden/site_auth_expected.json")).unwrap()
}

// --- orologio e gettoni finti (uno per test) -----------------------------------------------------

thread_local! {
    static NOW: RefCell<DateTime<Utc>> = RefCell::new(Utc.with_ymd_and_hms(2027, 3, 5, 12, 0, 0).unwrap());
    static COUNTER: Cell<u32> = const { Cell::new(0) };
}

fn fake_now() -> DateTime<Utc> {
    NOW.with(|now| *now.borrow())
}

fn fake_token() -> String {
    COUNTER.with(|counter| {
        counter.set(counter.get() + 1);
        format!("token-{}", counter.get())
    })
}

// --- directory e OAuth finti ---------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct DirectoryState {
    pub guild: Option<GuildFacts>,
    pub members: HashMap<u64, SiteMember>,
    pub errors: HashMap<u64, DirectoryError>,
    /// I nomi dalla cache dei membri (`cached_member_name`).
    pub names: HashMap<u64, String>,
    /// I canali vocali del server.
    pub voice_channels: Vec<(u64, String)>,
}

#[derive(Clone, Default)]
pub(crate) struct FakeDirectory(pub Arc<Mutex<DirectoryState>>);

impl FakeDirectory {
    pub(crate) fn add(&self, id: u64, administrator: bool, roles: &[u64]) {
        self.0.lock().unwrap().members.insert(id, SiteMember { display_name: format!("utente-{id}"), administrator, role_ids: roles.to_vec() });
    }
}

impl SiteDirectory for FakeDirectory {
    fn guild(&self, _guild_id: u64) -> Option<GuildFacts> {
        self.0.lock().unwrap().guild.clone()
    }

    fn cached_member_name(&self, _guild_id: u64, user_id: u64) -> Option<String> {
        self.0.lock().unwrap().names.get(&user_id).cloned()
    }

    fn voice_channels(&self, _guild_id: u64) -> Vec<(u64, String)> {
        self.0.lock().unwrap().voice_channels.clone()
    }

    fn member(&self, _guild_id: u64, user_id: u64) -> BoxFuture<'_, Result<Option<SiteMember>, DirectoryError>> {
        let state = self.0.lock().unwrap();
        let result = match state.errors.get(&user_id) {
            Some(error) => Err(error.clone()),
            None => Ok(state.members.get(&user_id).cloned()),
        };
        Box::pin(async move { result })
    }
}

#[derive(Default)]
pub(crate) struct OAuthState {
    pub token: Option<Result<OAuthReply, OAuthError>>,
    pub profile: Option<Result<OAuthReply, OAuthError>>,
    pub calls: Vec<Value>,
}

#[derive(Clone, Default)]
pub(crate) struct FakeOAuth(pub Arc<Mutex<OAuthState>>);

impl OAuthProvider for FakeOAuth {
    fn exchange_code<'a>(&'a self, request: TokenRequest<'a>) -> BoxFuture<'a, Result<OAuthReply, OAuthError>> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(json!(["post", {
            "client_id": request.client_id,
            "client_secret": request.client_secret,
            "grant_type": "authorization_code",
            "code": request.code,
            "redirect_uri": request.redirect_uri,
        }]));
        let reply = state.token.clone().expect("risposta dello scambio non preparata");
        Box::pin(async move { reply })
    }

    fn fetch_profile<'a>(&'a self, access_token: &'a str) -> BoxFuture<'a, Result<OAuthReply, OAuthError>> {
        let mut state = self.0.lock().unwrap();
        state.calls.push(json!(["get", format!("Bearer {access_token}")]));
        let reply = state.profile.clone().expect("risposta del profilo non preparata");
        Box::pin(async move { reply })
    }
}

struct Env {
    auth: Arc<SiteAuth>,
    directory: FakeDirectory,
    oauth: FakeOAuth,
    db: Database,
}

fn env_with(config: AuthConfig) -> Env {
    NOW.with(|now| *now.borrow_mut() = Utc.with_ymd_and_hms(2027, 3, 5, 12, 0, 0).unwrap());
    COUNTER.with(|counter| counter.set(0));
    let db = Database::open_in_memory().unwrap();
    db.set_setting("guild_id", "100").unwrap();
    let directory = FakeDirectory::default();
    directory.0.lock().unwrap().guild = Some(GuildFacts { owner_id: 1 });
    let oauth = FakeOAuth::default();
    let auth = SiteAuth::new(db.clone(), Arc::new(directory.clone()), Arc::new(oauth.clone()), config, None).with_sources(fake_now, fake_token);
    Env { auth: Arc::new(auth), directory, oauth, db }
}

fn configured() -> AuthConfig {
    AuthConfig::from_values(" client-1 ", "segreto", None, "https://overlay.example.it/")
}

fn env() -> Env {
    env_with(configured())
}

/// Le righe di una query come `[[valore, …], …]`.
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

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    cookies: Vec<String>,
    body: String,
}

async fn request(env: &Env, method: &str, path: &str, cookie: Option<&str>) -> Reply {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header("Cookie", cookie);
    }
    let response = env.auth.routes().oneshot(builder.body(Body::empty()).unwrap()).await.unwrap();
    let (parts, body) = response.into_parts();
    let cookies = parts.headers.get_all("set-cookie").iter().map(|value| value.to_str().unwrap().to_string()).collect();
    Reply { status: parts.status.as_u16(), headers: parts.headers, cookies, body: String::from_utf8(axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec()).unwrap() }
}

fn add_session(db: &Database, id: &str, user: i64, expires_at: &str) {
    db.execute(
        "INSERT INTO web_sessions(session_id,user_id,display_name,created_at,expires_at) VALUES(?,?,?,?,?)",
        params![id, user, "x", "2027-03-05T00:00:00+00:00", expires_at],
    )
    .unwrap();
}

fn error_view(response: Response) -> (u16, Value) {
    let status = response.status().as_u16();
    let body = futures_util::FutureExt::now_or_never(axum::body::to_bytes(response.into_body(), usize::MAX)).unwrap().unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

// --- configurazione e helper ---------------------------------------------------------------------

#[test]
fn configuration_matches_python() {
    let golden = golden();
    assert_eq!(SESSION_COOKIE, golden["constants"]["session_cookie"]);
    assert_eq!(STATE_COOKIE, golden["constants"]["state_cookie"]);
    assert_eq!(SESSION_SECONDS as f64 / 3600.0, golden["constants"]["session_hours"].as_f64().unwrap());
    assert_eq!(OAUTH_STATE_TTL_SECONDS, golden["constants"]["state_ttl"].as_f64().unwrap());
    let config = configured();
    let expected = &golden["config"];
    assert_eq!(config.client_id, expected["client_id"].as_str().unwrap());
    assert_eq!(config.client_secret, expected["client_secret"].as_str().unwrap());
    assert_eq!(config.redirect_uri, expected["redirect_uri"].as_str().unwrap());
    assert_eq!(config.secure_cookie, expected["secure_cookie"].as_bool().unwrap());
    assert_eq!(config.oauth_configured(), expected["configured"].as_bool().unwrap());
    // L'indirizzo di ritorno esplicito vale più del dominio; senza dominio né indirizzo il login non c'è.
    assert_eq!(AuthConfig::from_values("a", "b", Some(" https://x.it/cb ".into()), "https://overlay.example.it").redirect_uri, "https://x.it/cb");
    assert!(!AuthConfig::from_values("a", "b", None, "").oauth_configured());
    assert!(!AuthConfig::from_values("a", "b", Some("http://x.it/cb".into()), "").oauth_configured());
    assert!(!AuthConfig::from_values("", "b", None, "https://x.it").oauth_configured());
    assert!(!AuthConfig::from_values("a", "", None, "https://x.it").oauth_configured());
}

#[test]
fn helpers_match_python() {
    for case in golden()["helpers"]["equals"].as_array().unwrap() {
        assert_eq!(constant_time_equals(case[0].as_str().unwrap(), case[1].as_str().unwrap()), case[2].as_bool().unwrap(), "{case}");
    }
    let golden = golden();
    let [status, text, headers] = &golden["helpers"]["json_error"][0].as_array().unwrap()[..] else { panic!() };
    let response = json_error(StatusCode::from_u16(status.as_u64().unwrap() as u16).unwrap(), "Non è qui");
    assert_eq!(response.headers()["content-type"], headers["Content-Type"].as_str().unwrap());
    assert_eq!(response.headers()["cache-control"], headers["Cache-Control"].as_str().unwrap());
    let body = futures_util::FutureExt::now_or_never(axum::body::to_bytes(response.into_body(), usize::MAX)).unwrap().unwrap();
    assert_eq!(String::from_utf8(body.to_vec()).unwrap(), text.as_str().unwrap());
}

#[test]
fn oauth_query_is_normalised_like_yarl() {
    assert_eq!(authorize_query(&[("a", "https://x.it/p?q=1&r=2")]), "a=https://x.it/p?q%3D1%26r%3D2");
    assert_eq!(authorize_query(&[("a", "b c+d%41;e")]), "a=b+c%2Bd%2541%3Be");
    assert_eq!(authorize_query(&[("a", "é☃")]), "a=%C3%A9%E2%98%83");
}

// --- permessi ------------------------------------------------------------------------------------

#[tokio::test]
async fn member_permissions_match_python() {
    let golden = golden();
    let cases = golden["permissions"].as_array().unwrap();
    assert_eq!(cases.len(), 15);
    for case in cases {
        let label = case["label"].as_str().unwrap();
        let e = env();
        let user: u64 = match label {
            "proprietario" => {
                e.directory.add(1, false, &[]);
                1
            }
            "amministratore" | "ruoli_corrotti_ma_admin" => {
                e.directory.add(2, true, &[]);
                if label == "ruoli_corrotti_ma_admin" {
                    e.db.set_setting("staff_roles", "abc").unwrap();
                }
                2
            }
            "senza_ruoli" => {
                e.directory.add(3, false, &[300]);
                3
            }
            "ruolo_staff" | "ruolo_staff_con_spazi" | "ruolo_non_nell_elenco" | "ruoli_corrotti" => {
                let (roles, setting): (&[u64], &str) = match label {
                    "ruolo_staff" => (&[300, 777], "777"),
                    "ruolo_staff_con_spazi" => (&[8], "1, 8 ,,9"),
                    "ruolo_non_nell_elenco" => (&[5], "777,888"),
                    _ => (&[8], "8,abc"),
                };
                e.directory.add(4, false, roles);
                e.db.set_setting("staff_roles", setting).unwrap();
                4
            }
            "membro_non_in_cache_recuperato" => {
                e.directory.add(5, true, &[]);
                5
            }
            "membro_inesistente" => 6,
            "recupero_vietato" => {
                e.directory.0.lock().unwrap().errors.insert(7, DirectoryError::Forbidden);
                7
            }
            "recupero_http" => {
                e.directory.0.lock().unwrap().errors.insert(7, DirectoryError::Http("500".into()));
                7
            }
            "server_non_configurato" | "server_non_valido" | "server_sconosciuto" => {
                e.directory.add(1, false, &[]);
                match label {
                    "server_non_configurato" => e.db.execute("DELETE FROM settings WHERE key='guild_id'", []).map(|_| ()).unwrap(),
                    "server_non_valido" => e.db.set_setting("guild_id", "abc").unwrap(),
                    _ => e.directory.0.lock().unwrap().guild = None,
                }
                1
            }
            other => panic!("caso sconosciuto: {other}"),
        };
        match e.auth.member_permissions(user as i64).await {
            Ok(principal) => {
                assert_eq!(json!([principal.user_id, principal.display_name, principal.is_admin, principal.is_staff]), case["principal"], "{label}");
                assert!(case["error"].is_null(), "{label}");
            }
            Err(response) => {
                assert!(case["principal"].is_null(), "{label}");
                let (status, body) = error_view(response);
                assert_eq!(json!({"status": status, "body": body}), case["error"], "{label}");
            }
        }
    }
}

#[tokio::test]
async fn the_environment_guild_is_used_when_the_setting_is_missing() {
    let e = env();
    e.directory.add(1, false, &[]);
    e.db.execute("DELETE FROM settings WHERE key='guild_id'", []).unwrap();
    assert!(e.auth.member_permissions(1).await.is_err(), "senza impostazione né ambiente");
    let with_env = SiteAuth::new(e.db.clone(), Arc::new(e.directory.clone()), Arc::new(e.oauth.clone()), configured(), Some(100));
    assert!(with_env.member_permissions(1).await.is_ok());
    // Un utente con ID negativo non può essere nel server.
    assert!(with_env.member_permissions(-4).await.is_err());
}

// --- sessioni ------------------------------------------------------------------------------------

#[tokio::test]
async fn sessions_and_logout_match_python() {
    let golden = golden();
    let cases = golden["sessions"].as_array().unwrap();
    assert_eq!(cases.len(), 18);
    for case in cases {
        let label = case["label"].as_str().unwrap();
        let e = env();
        e.directory.add(1, false, &[]);
        e.directory.add(9, false, &[999]);
        e.db.set_setting("staff_roles", "777").unwrap();
        let session = |id: &str, user: i64, expires: &str| add_session(&e.db, id, user, expires);
        let (mut method, mut path, mut cookie) = ("GET", "/api/site/auth/session", Some("sfo_site_session=s1"));
        match label {
            "senza_cookie" => cookie = None,
            "cookie_vuoto" => cookie = Some("sfo_site_session="),
            "sessione_sconosciuta" => cookie = Some("sfo_site_session=nessuna"),
            "sessione_valida_proprietario" | "piu_cookie" | "cookie_tra_virgolette" => {
                session("s1", 1, "2027-03-05T20:00:00+00:00");
                if label == "piu_cookie" {
                    cookie = Some("altro=1; sfo_site_session=s1; x=2");
                }
                if label == "cookie_tra_virgolette" {
                    cookie = Some("sfo_site_session=\"s1\"");
                }
            }
            "sessione_che_scade_ora" => session("s1", 1, "2027-03-05T12:00:00+00:00"),
            "sessione_scaduta" => session("s1", 1, "2027-03-05T11:59:59+00:00"),
            "scadenza_non_valida" => session("s1", 1, "domani"),
            "scadenza_con_microsecondi" => session("s1", 1, "2027-03-05T12:00:00.000001+00:00"),
            "scadenza_con_altro_fuso" => session("s1", 1, "2027-03-05T14:00:00+02:00"),
            "scadenza_con_z" => session("s1", 1, "2027-03-05T13:00:00Z"),
            "utente_non_staff" => session("s1", 9, "2027-03-05T20:00:00+00:00"),
            "utente_uscito_dal_server" => session("s1", 77, "2027-03-05T20:00:00+00:00"),
            "altre_sessioni_restano" => {
                session("s1", 1, "2027-03-05T11:00:00+00:00");
                session("s2", 1, "2027-03-05T20:00:00+00:00");
            }
            "logout_senza_cookie" => (method, path, cookie) = ("POST", "/api/site/auth/logout", None),
            "logout_con_sessione" => {
                session("s1", 1, "2027-03-05T20:00:00+00:00");
                session("s2", 1, "2027-03-05T20:00:00+00:00");
                (method, path) = ("POST", "/api/site/auth/logout");
            }
            "logout_metodo_sbagliato" => (method, path, cookie) = ("GET", "/api/site/auth/logout", None),
            other => panic!("caso sconosciuto: {other}"),
        }
        let reply = request(&e, method, path, cookie).await;
        assert_eq!(u64::from(reply.status), case["status"].as_u64().unwrap(), "{label}: stato");
        if label == "logout_metodo_sbagliato" {
            continue;
        }
        assert_eq!(reply.headers["content-type"], case["headers"]["Content-Type"].as_str().unwrap(), "{label}");
        assert_eq!(reply.headers["cache-control"], "no-store", "{label}");
        assert_eq!(json!(reply.cookies), case["cookies"], "{label}: cookie");
        assert_eq!(reply.body, case["body"].as_str().unwrap(), "{label}: corpo");
        assert_eq!(table(&e.db, "SELECT session_id FROM web_sessions ORDER BY session_id"), case["sessions_left"], "{label}: sessioni rimaste");
    }
}

#[tokio::test]
async fn admin_only_routes_refuse_plain_staff() {
    let golden = golden();
    for case in golden["admin_only"].as_array().unwrap() {
        let label = case["label"].as_str().unwrap();
        let e = env();
        e.directory.add(2, true, &[]);
        e.directory.add(4, false, &[777]);
        e.db.set_setting("staff_roles", "777").unwrap();
        let user = match label {
            "admin" => 2,
            "solo_staff" => 4,
            _ => 77,
        };
        add_session(&e.db, "s1", user, "2027-03-05T20:00:00+00:00");
        let mut headers = HeaderMap::new();
        headers.insert("cookie", "sfo_site_session=s1".parse().unwrap());
        match e.auth.authorize(&headers, true).await {
            Ok(principal) => assert_eq!(json!([principal.user_id, principal.is_admin]), case["principal"], "{label}"),
            Err(response) => {
                let (status, body) = error_view(response);
                assert_eq!(json!([status, body]), case["error"], "{label}");
            }
        }
    }
}

// --- login ---------------------------------------------------------------------------------------

#[tokio::test]
async fn login_matches_python() {
    let golden = golden();
    for case in golden["login"].as_array().unwrap() {
        let label = case["label"].as_str().unwrap();
        let (client_id, secret, redirect) = match label {
            "non_configurato" => ("", "segreto", "https://overlay.example.it/staff/auth/callback"),
            "senza_segreto" => ("client-1", "", "https://overlay.example.it/staff/auth/callback"),
            "redirect_http" => ("client-1", "segreto", "http://overlay.example.it/staff/auth/callback"),
            "senza_redirect" => ("client-1", "segreto", ""),
            "redirect_con_query" => ("c1", "segreto", "https://x.it/cb?a=b&c=d e#f"),
            "caratteri_speciali" => ("id con spazio&=/", "segreto", "https://x.it/è/☃+%41"),
            "riservati" => ("c1", "segreto", "https://x.it/a;b,c!d*e'f(g)h$i~j@k:l"),
            _ => ("client-1", "segreto", "https://overlay.example.it/staff/auth/callback"),
        };
        let mut config = AuthConfig::from_values(client_id, secret, Some(redirect.to_string()), "");
        // «ok_senza_secure» è lo stesso login con il cookie senza `Secure`.
        if label == "ok_senza_secure" {
            config.secure_cookie = false;
        }
        let e = env_with(config);
        let now = fake_now().timestamp() as f64;
        if label == "ok" {
            for (state, age) in [("vecchio", 601.0), ("limite", 600.0), ("recente", 5.0)] {
                e.db.execute("INSERT INTO site_oauth_states(state,created_at) VALUES(?,?)", params![state, now - age]).unwrap();
            }
        }
        if label == "database_in_errore" {
            e.db.execute_batch("CREATE TRIGGER fail_state BEFORE INSERT ON site_oauth_states BEGIN SELECT RAISE(ABORT,'boom'); END").unwrap();
        }
        // Nel golden i login consecutivi usano gettoni numerati: si parte dallo stesso numero.
        let first_token = case["cookies"]
            .as_array()
            .and_then(|cookies| cookies.first())
            .and_then(Value::as_str)
            .and_then(|cookie| cookie.split("token-").nth(1))
            .and_then(|number| number.split(';').next())
            .and_then(|number| number.parse::<u32>().ok())
            .unwrap_or(1);
        COUNTER.with(|counter| counter.set(first_token - 1));
        let reply = request(&e, "GET", "/staff/auth/login", None).await;
        assert_eq!(u64::from(reply.status), case["status"].as_u64().unwrap(), "{label}: stato");
        assert_eq!(reply.body, case["body"].as_str().unwrap(), "{label}: corpo");
        assert_eq!(reply.headers["content-type"], case["headers"]["Content-Type"].as_str().unwrap(), "{label}");
        assert_eq!(reply.headers.get("location").map(|value| value.to_str().unwrap()), case["headers"]["Location"].as_str(), "{label}: Location");
        assert_eq!(json!(reply.cookies), case["cookies"], "{label}: cookie");
        if label == "ok" {
            assert_eq!(table(&e.db, "SELECT state,created_at FROM site_oauth_states ORDER BY state"), case["states"], "{label}: stati salvati");
        }
    }
}

// --- callback ------------------------------------------------------------------------------------

#[tokio::test]
async fn callback_matches_python() {
    let golden = golden();
    let cases = golden["callback"].as_array().unwrap();
    assert_eq!(cases.len(), 28);
    let ok_reply = |body: Value| -> Result<OAuthReply, OAuthError> { Ok((200, Some(body))) };
    for case in cases {
        let label = case["label"].as_str().unwrap();
        let e = if label == "non_configurato" { env_with(AuthConfig::from_values("", "segreto", None, "https://overlay.example.it")) } else { env() };
        e.directory.add(5, true, &[]);
        let mut state_cookie = Some("S1");
        let mut stored = true;
        let mut age = 1.0;
        let mut query = "?state=S1&code=c".to_string();
        let mut token: Result<OAuthReply, OAuthError> = ok_reply(json!({"access_token": "at"}));
        let mut profile: Result<OAuthReply, OAuthError> = ok_reply(json!({"id": "5"}));
        let mut trigger = false;
        match label {
            "non_configurato" => {}
            "senza_stato" => query = "?code=c".into(),
            "senza_cookie" => state_cookie = None,
            "cookie_vuoto" => state_cookie = Some(""),
            "stato_diverso" => query = "?state=S2&code=c".into(),
            "stato_non_salvato" => stored = false,
            "stato_scaduto" => age = 601.0,
            "stato_al_limite" => age = 600.0,
            "errore_di_discord" => query = "?state=S1&error=access_denied".into(),
            "senza_codice" => query = "?state=S1".into(),
            "codice_vuoto" => query = "?state=S1&code=".into(),
            "scambio_http_errore" => token = Ok((400, None)),
            "senza_access_token" => token = ok_reply(json!({})),
            "access_token_vuoto" => token = ok_reply(json!({"access_token": ""})),
            "profilo_http_errore" => profile = Ok((500, None)),
            "rete_scambio" => token = Err(OAuthError::Unavailable),
            "rete_profilo" | "timeout_profilo" => profile = Err(OAuthError::Unavailable),
            "json_non_valido" => token = Err(OAuthError::Unavailable),
            "profilo_senza_id" => profile = ok_reply(json!({})),
            "profilo_id_non_numerico" => profile = ok_reply(json!({"id": "abc"})),
            "profilo_id_intero" => profile = ok_reply(json!({"id": 5})),
            "profilo_id_none" => profile = ok_reply(json!({"id": null})),
            "utente_non_staff" => {
                e.directory.add(9, false, &[]);
                profile = ok_reply(json!({"id": "9"}));
            }
            "utente_fuori_dal_server" => profile = ok_reply(json!({"id": "77"})),
            "ok" => query = "?state=S1&code=c%20d".into(),
            "ok_con_sessioni_scadute" => {
                e.db.execute_batch(
                    "INSERT INTO web_sessions(session_id,user_id,display_name,created_at,expires_at) VALUES('vecchia',1,'x','a','2027-03-05T11:59:59+00:00'),('valida',1,'x','a','2027-03-05T12:00:01+00:00'),('al_limite',1,'x','a','2027-03-05T12:00:00+00:00')",
                )
                .unwrap();
            }
            "sessione_non_creata" => trigger = true,
            other => panic!("caso sconosciuto: {other}"),
        }
        if stored {
            e.db.execute("INSERT INTO site_oauth_states(state,created_at) VALUES('S1',?)", [fake_now().timestamp() as f64 - age]).unwrap();
        }
        if trigger {
            e.db.execute_batch("CREATE TRIGGER fail_session BEFORE INSERT ON web_sessions BEGIN SELECT RAISE(ABORT,'boom'); END").unwrap();
        }
        {
            let mut state = e.oauth.0.lock().unwrap();
            state.token = Some(token);
            state.profile = Some(profile);
        }
        let cookie = state_cookie.map(|value| format!("sfo_discord_oauth_state={value}"));
        let reply = request(&e, "GET", &format!("/staff/auth/callback{query}"), cookie.as_deref()).await;

        assert_eq!(u64::from(reply.status), case["status"].as_u64().unwrap(), "{label}: stato");
        assert_eq!(reply.headers.get("location").map(|value| value.to_str().unwrap()), case["headers"]["Location"].as_str(), "{label}: Location");
        assert_eq!(json!(reply.cookies), case["cookies"], "{label}: cookie");
        assert_eq!(reply.body, case["body"].as_str().unwrap(), "{label}: corpo");
        assert_eq!(table(&e.db, "SELECT state FROM site_oauth_states"), case["states"], "{label}: stati rimasti");
        assert_eq!(table(&e.db, "SELECT session_id,user_id,display_name,created_at,expires_at FROM web_sessions"), case["sessions"], "{label}: sessioni");
        // Le chiamate a Discord: lo scambio del codice e, se è andato bene, il profilo.
        let expected_calls: Vec<Value> = case["calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|call| call[0] != "session")
            .map(|call| match call[0].as_str().unwrap() {
                "post" => json!(["post", call[2]]),
                _ => json!(["get", "Bearer at"]),
            })
            .collect();
        let calls = e.oauth.0.lock().unwrap().calls.clone();
        assert_eq!(json!(calls), json!(expected_calls), "{label}: chiamate a Discord");
    }
}

#[tokio::test]
async fn an_oauth_state_can_be_used_only_once() {
    let golden = golden();
    let e = env();
    e.directory.add(5, true, &[]);
    {
        let mut state = e.oauth.0.lock().unwrap();
        state.token = Some(Ok((200, Some(json!({"access_token": "at"})))));
        state.profile = Some(Ok((200, Some(json!({"id": "5"})))));
    }
    e.db.execute("INSERT INTO site_oauth_states(state,created_at) VALUES('S1',?)", [fake_now().timestamp() as f64]).unwrap();
    let first = request(&e, "GET", "/staff/auth/callback?state=S1&code=c", Some("sfo_discord_oauth_state=S1")).await;
    let second = request(&e, "GET", "/staff/auth/callback?state=S1&code=c", Some("sfo_discord_oauth_state=S1")).await;
    assert_eq!(json!([first.status, second.status, second.body]), golden["state_reuse"]);
}

#[tokio::test]
async fn the_directory_is_unavailable_until_the_bot_is_ready() {
    let late = LateDirectory::default();
    assert_eq!(late.guild(1), None);
    assert_eq!(late.member(1, 2).await, Ok(None));
    let directory = FakeDirectory::default();
    directory.0.lock().unwrap().guild = Some(GuildFacts { owner_id: 9 });
    directory.add(2, true, &[]);
    late.set(Arc::new(directory));
    assert_eq!(late.guild(1), Some(GuildFacts { owner_id: 9 }));
    assert_eq!(late.member(1, 2).await.unwrap().map(|member| member.administrator), Some(true));
}
