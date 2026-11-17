//! Helper condivisi dalle route del sito staff e dell'API pubblica (`cogs/site_common.py`).

use std::collections::HashSet;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::formatting::python_json;
use crate::live_event::python_int;

/// `Cache-Control` delle risposte che non vanno mai memorizzate.
pub const NO_STORE: &str = "no-store";

/// `json.dumps(valore)` di Python: separatori `, ` e `: ` e caratteri non ASCII come `\uXXXX`
/// (coppie surrogate comprese).
pub fn python_json_ascii(value: &Value) -> String {
    let mut ascii = String::new();
    for c in python_json(value).chars() {
        if c.is_ascii() {
            ascii.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                ascii.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    ascii
}

/// Risposta JSON (`web.json_response`) che non si memorizza in cache.
pub fn json_response(status: StatusCode, value: &Value) -> Response {
    let mut response = (status, python_json_ascii(value)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    response
}

/// Risposta JSON di errore, mai memorizzata in cache (`json_error`).
pub fn json_error(status: StatusCode, message: &str) -> Response {
    json_response(status, &json!({"error": message}))
}

/// Risposta di solo testo, come le `web.HTTP*` di aiohttp (`text/plain; charset=utf-8`).
pub fn plain(status: StatusCode, text: &str) -> Response {
    (status, text.to_string()).into_response()
}

/// Il corpo che aiohttp mette in una risposta senza testo: `<codice>: <motivo>`.
pub fn status_text(status: StatusCode) -> Response {
    plain(status, &format!("{}: {}", status.as_u16(), status.canonical_reason().unwrap_or("")))
}

/// Reindirizzamento come `web.HTTPFound` (302) o `web.HTTPPermanentRedirect` (308).
pub fn redirect(status: StatusCode, location: &str) -> Response {
    let mut response = status_text(status);
    response.headers_mut().insert(header::LOCATION, HeaderValue::from_bytes(location.as_bytes()).unwrap_or_else(|_| HeaderValue::from_static("/")));
    response
}

/// Aggiunge un'intestazione `Set-Cookie`.
pub fn add_cookie(response: &mut Response, cookie: &str) {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

/// `response.set_cookie(nome, valore, max_age, httponly, secure, samesite="Lax", path)` di aiohttp:
/// gli attributi escono in ordine alfabetico.
pub fn set_cookie(name: &str, value: &str, max_age: u64, secure: bool, path: &str) -> String {
    let mut cookie = format!("{name}={value}; HttpOnly; Max-Age={max_age}; Path={path}; SameSite=Lax");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// `response.del_cookie(nome, path)` di aiohttp.
pub fn delete_cookie(name: &str, path: &str) -> String {
    format!("{name}=\"\"; expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path={path}")
}

/// Il valore di un cookie della richiesta (`request.cookies`): se compare più volte vale l'ultimo,
/// le virgolette attorno al valore si tolgono.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for header in headers.get_all(header::COOKIE) {
        let Ok(raw) = header.to_str() else { continue };
        for part in raw.split(';') {
            let Some((key, value)) = part.split_once('=') else { continue };
            if key.trim() == name {
                let value = value.trim();
                let value = value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(value);
                found = Some(value.to_string());
            }
        }
    }
    found
}

/// I ruoli staff come stringhe non vuote (`staff_role_values`).
pub fn staff_role_values(raw: &str) -> Vec<&str> {
    raw.split(',').filter(|role| !role.is_empty()).collect()
}

/// Gli ID numerici dei ruoli staff; `None` se la configurazione è corrotta (`ValueError` in Python).
pub fn staff_role_ids(raw: &str) -> Option<HashSet<i64>> {
    staff_role_values(raw).into_iter().map(python_int).collect()
}

/// Confronto a tempo costante (`constant_time_equals`): sui byte UTF-8, quindi anche con testo non ASCII.
pub fn constant_time_equals(supplied: &str, expected: &str) -> bool {
    let (a, b) = (supplied.as_bytes(), expected.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |difference, (x, y)| difference | (x ^ y)) == 0
}

/// I parametri della query: per ogni nome vale il primo (`request.query.get`).
pub fn query_params(raw: Option<String>) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    for (key, value) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        params.entry(key.into_owned()).or_insert_with(|| value.into_owned());
    }
    params
}

/// Perché il corpo di una richiesta non si è potuto leggere come JSON.
#[derive(Debug, PartialEq, Eq)]
pub enum BodyError {
    /// Oltre i 64 KB che il server accetta (`client_max_size`).
    TooLarge,
    /// Non è JSON.
    Invalid,
}

/// Il corpo di una richiesta come JSON (`await request.json()`), con il limite di 64 KB.
pub async fn read_json_body(body: axum::body::Body) -> Result<Value, BodyError> {
    let bytes = axum::body::to_bytes(body, 64 * 1024).await.map_err(|_| BodyError::TooLarge)?;
    serde_json::from_slice(&bytes).map_err(|_| BodyError::Invalid)
}
