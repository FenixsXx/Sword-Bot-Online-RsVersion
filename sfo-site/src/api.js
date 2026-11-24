// Comunicazione con il backend Rust.
// Questo modulo gestisce le richieste API verso il server Rust back-end.

/*
 * Concetti chiave:
 * - Fetch: È la funzione nativa di JavaScript usata per "chiedere" dati al server.
 *   Pensa a `apiRequest` come a un corriere: parte dal browser (il tuo sito),
 *   porta un messaggio al server Rust e torna con una risposta (solitamente in formato JSON).
 * - Asincronia: Il browser non si blocca mentre aspetta la risposta dal server.
 *   Usiamo 'async' per dire "questo compito richiede tempo" e 'await' per "metti in pausa questa
 *   funzione finché il server non risponde".
 */

let unauthorizedHandler = null;

/** Registra la funzione chiamata quando il server risponde 401 (sessione scaduta). */
export function onUnauthorized(handler) {
  unauthorizedHandler = handler;
}

/**
 * Richiesta JSON same-origin verso le API del bot.
 * `allowUnauthorized` evita il ritorno al login per le chiamate che si
 * aspettano un 401 (verifica iniziale della sessione, logout).
 */
export async function apiRequest(path, { method = "GET", body, allowUnauthorized = false } = {}) {
  const response = await fetch(path, {
    method,
    credentials: "same-origin",
    headers: body !== undefined ? { "Content-Type": "application/json" } : {},
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
  });
  const result = await response.json().catch(() => ({}));
  if (!response.ok) {
    const error = new Error(result.error || `Errore HTTP ${response.status}`);
    error.status = response.status;
    if (response.status === 401 && !allowUnauthorized) {
      error.sessionExpired = true;
      unauthorizedHandler?.(error);
    }
    throw error;
  }
  return result;
}
