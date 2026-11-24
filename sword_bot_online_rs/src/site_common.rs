//! Helper condivisi dalle route del sito staff e dell'API pubblica (`cogs/site_common.py`).
//!
//! Questo modulo contiene funzioni "utilità" (helper) usate in molti punti del sito web.
//! Sono operazioni di base come: creare risposte JSON, gestire i cookie, confrontare testi in modo sicuro,
//! leggere i parametri degli indirizzi web (query string).
//!
//! ### Cos'è un "Cookie"?
//!
//! Un cookie è un piccolo frammento di testo che il sito web chiede al tuo browser di conservare.
//! Funziona come un "bigliettino" che il browser rimanda al sito a ogni visita successiva.
//! Qui li usiamo per:
//! - Mantenere l'utente collegato (sessione).
//! - Proteggere il login da contraffazioni (CSRF token).
//!
//! ### Cos'è "Cache-Control: no-store"?
//!
//! È un'intestazione (header) HTTP che dice al browser: "Non salvare una copia di questa risposta".
//! Lo usiamo su tutte le risposte che contengono dati sensibili o che cambiano spesso (es. stato della sessione,
//! risultati di una ricerca), per evitare che il browser mostri dati vecchi o che qualcuno li legga dalla cache.

use std::collections::HashSet;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::formatting::python_json;
use crate::live_event::python_int;

/// `Cache-Control` delle risposte che non vanno mai memorizzate.
/// "no-store" = il browser non deve salvare nulla su disco né in memoria per questa risposta.
pub const NO_STORE: &str = "no-store";

/// `json.dumps(valore)` di Python: separatori `, ` e `: ` e caratteri non ASCII come `\uXXXX`
/// (coppie surrogate comprese).
/// Questa funzione serve a produrre JSON **identico** a quello che produrrebbe Python,
/// per compatibilità con eventuali script o test che confrontano l'output.
pub fn python_json_ascii(value: &Value) -> String {
    let mut ascii = String::new();
    // python_json produce JSON con caratteri Unicode veri; qui li convertiamo in escape \uXXXX.
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

/// Crea una risposta HTTP con corpo JSON e intestazioni corrette.
/// - Imposta `Content-Type: application/json; charset=utf-8`
/// - Imposta `Cache-Control: no-store` (non salvare in cache)
/// - Usa la serializzazione JSON compatibile con Python (`python_json_ascii`).
pub fn json_response(status: StatusCode, value: &Value) -> Response {
    let mut response = (status, python_json_ascii(value)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    response
}

/// Crea una risposta JSON di errore standardizzata: `{"error": "messaggio"}`.
/// Non viene mai memorizzata in cache.
pub fn json_error(status: StatusCode, message: &str) -> Response {
    json_response(status, &json!({"error": message}))
}

/// Crea una risposta di solo testo semplice (`text/plain; charset=utf-8`).
/// Simile alle risposte `web.HTTP*` della libreria Python `aiohttp`.
pub fn plain(status: StatusCode, text: &str) -> Response {
    (status, text.to_string()).into_response()
}

/// Il testo standard che aiohttp mette in una risposta senza corpo personalizzato:
/// "CODICE: MOTIVO" (es. "404: Not Found").
pub fn status_text(status: StatusCode) -> Response {
    plain(status, &format!("{}: {}", status.as_u16(), status.canonical_reason().unwrap_or("")))
}

/// Crea un reindirizzamento (redirect) HTTP.
/// `status` deve essere 302 (Found/temporaneo) o 308 (Permanent Redirect).
/// Impone l'intestazione `Location` con l'indirizzo di destinazione.
pub fn redirect(status: StatusCode, location: &str) -> Response {
    let mut response = status_text(status);
    response.headers_mut().insert(header::LOCATION, HeaderValue::from_bytes(location.as_bytes()).unwrap_or_else(|_| HeaderValue::from_static("/")));
    response
}

/// Aggiunge un'intestazione `Set-Cookie` a una risposta già esistente.
/// I cookie si "accodano": puoi chiamare questa funzione più volte per impostare più cookie.
pub fn add_cookie(response: &mut Response, cookie: &str) {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

/// Costruisce la stringa completa di un cookie, nello stesso formato che userebbe `aiohttp` in Python.
/// L'ordine degli attributi è alfabetico (come fa aiohttp).
/// Parametri:
/// - `name`: nome del cookie (es. "session_id")
/// - `value`: valore del cookie
/// - `max_age`: secondi di vita (es. 43200 = 12 ore)
/// - `secure`: true = il cookie viaggia solo su HTTPS
/// - `path`: percorso per cui è valido (es. "/" = tutto il sito)
/// Attributi fissi: `HttpOnly` (JavaScript non può leggerlo, sicurezza), `SameSite=Lax` (protezione CSRF base).
pub fn set_cookie(name: &str, value: &str, max_age: u64, secure: bool, path: &str) -> String {
    let mut cookie = format!("{name}={value}; HttpOnly; Max-Age={max_age}; Path={path}; SameSite=Lax");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Costruisce una stringa cookie che dice al browser di **cancellare** un cookie.
/// Lo fa impostando una data di scadenza nel passato (1 Gennaio 1970) e `Max-Age=0`.
pub fn delete_cookie(name: &str, path: &str) -> String {
    format!("{name}=\"\"; expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path={path}")
}

/// Legge il valore di un cookie dalla richiesta HTTP.
/// L'intestazione `Cookie` può contenere più cookie separati da punto e virgola.
/// Se lo stesso cookie compare più volte, vince l'ultimo (comportamento standard browser).
/// Rimuove eventuali virgolette attorno al valore.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for header in headers.get_all(header::COOKIE) {
        let Ok(raw) = header.to_str() else { continue };
        for part in raw.split(';') {
            let Some((key, value)) = part.split_once('=') else { continue };
            if key.trim() == name {
                let value = value.trim();
                // Tolgo le virgolette se ci sono (es. "valore" -> valore)
                let value = value.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(value);
                found = Some(value.to_string());
            }
        }
    }
    found
}

/// Estrae i nomi dei ruoli staff dalla stringa di configurazione (es. "123,456,789").
/// Ignora i vuoti. Restituisce una lista di stringhe.
pub fn staff_role_values(raw: &str) -> Vec<&str> {
    raw.split(',').filter(|role| !role.is_empty()).collect()
}

/// Converte i nomi dei ruoli staff in un `HashSet` di ID numerici (i64).
/// `python_int` gestisce stringhe come "123", "0x7B", ecc.
/// Restituisce `None` se anche solo un valore non è un numero valido (configurazione corrotta).
pub fn staff_role_ids(raw: &str) -> Option<HashSet<i64>> {
    staff_role_values(raw).into_iter().map(python_int).collect()
}

/// **Confronto a tempo costante** (Constant-time comparison).
/// Confronta due stringhe impiegando **sempre lo stesso tempo**, indipendentemente da quanto sono simili.
///
/// ### Perché serve?
/// Se confronti due password/token con `==` normale, il processore si ferma al primo carattere diverso.
/// Un attaccante può misurare i tempi di risposta e indovinare il token carattere per carattere
/// (attacco "timing attack").
/// Questa funzione elabora **tutti** i byte sempre, così il tempo non rivela informazioni.
///
/// Funziona sui byte UTF-8, quindi va bene anche per testo non-ASCII.
pub fn constant_time_equals(supplied: &str, expected: &str) -> bool {
    let (a, b) = (supplied.as_bytes(), expected.as_bytes());
    // Se le lunghezze sono diverse, sono sicuramente diverse (ma controlliamo tutto lo stesso per coerenza).
    if a.len() != b.len() {
        return false;
    }
    // XOR di ogni coppia di byte: se sono uguali, XOR = 0. Accumuliamo con OR.
    // Se alla fine difference == 0, tutti i byte erano uguali.
    a.iter().zip(b).fold(0u8, |difference, (x, y)| difference | (x ^ y)) == 0
}

/// Analizza la "query string" di un URL (la parte dopo il `?`).
/// Esempio: "?a=1&b=2&a=3" -> {"a": "1", "b": "2"} (prende il primo valore per chiavi duplicate).
/// Usa la crate `url` che segue lo standard WHATWG.
pub fn query_params(raw: Option<String>) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    for (key, value) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        params.entry(key.into_owned()).or_insert_with(|| value.into_owned());
    }
    params
}

/// Possibili errori quando si legge il corpo di una richiesta come JSON.
#[derive(Debug, PartialEq, Eq)]
pub enum BodyError {
    /// Il corpo supera i 64 KB che il server accetta (limite `client_max_size`).
    TooLarge,
    /// Il corpo non è JSON valido.
    Invalid,
}

/// Legge l'intero corpo di una richiesta HTTP e lo interpreta come JSON.
/// Limite massimo: 64 KB (65.536 byte). Oltre quello, errore `TooLarge`.
/// Se il JSON è malformato, errore `Invalid`.
pub async fn read_json_body(body: axum::body::Body) -> Result<Value, BodyError> {
    let bytes = axum::body::to_bytes(body, 64 * 1024).await.map_err(|_| BodyError::TooLarge)?;
    serde_json::from_slice(&bytes).map_err(|_| BodyError::Invalid)
}