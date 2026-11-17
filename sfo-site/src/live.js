import { apiRequest } from "./api.js";
import { $ } from "./dom.js";

/**
 * Connessione in tempo reale a /api/site/events.
 * Il server invia {"type":"ready"} all'apertura e {"type":"change","topic":"..."}
 * quando cambiano i dati; il client si riconnette con backoff esponenziale.
 */

const MIN_DELAY_MS = 1000;
const MAX_DELAY_MS = 30000;
const SESSION_EXPIRED_CODE = 4401;

let socket = null;
let wanted = false;
let attempt = 0;
let retryTimer = null;
let connectedBefore = false;
let handlers = { onChange: () => {}, onResync: () => {} };
let listenersReady = false;

const INDICATOR = {
  live: { label: "Live", title: "Aggiornamenti in tempo reale attivi" },
  connecting: { label: "Connessione…", title: "Connessione agli aggiornamenti in tempo reale" },
  offline: { label: "Offline", title: "Aggiornamenti in tempo reale non disponibili" },
};

function setIndicator(name, detail = "") {
  const indicator = $("#live-indicator");
  const { label, title } = INDICATOR[name];
  indicator.dataset.state = name;
  indicator.querySelector("[data-live-label]").textContent = label;
  indicator.title = detail ? `${title}: ${detail}` : title;
}

function socketUrl() {
  const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${protocol}//${window.location.host}/api/site/events`;
}

function connect() {
  clearTimeout(retryTimer);
  retryTimer = null;
  if (!wanted || socket) return;
  setIndicator("connecting");
  let ws;
  try {
    ws = new WebSocket(socketUrl());
  } catch (error) {
    console.error("Connessione live non riuscita.", error);
    scheduleReconnect();
    return;
  }
  socket = ws;
  ws.addEventListener("message", (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }
    if (message?.type === "ready") {
      attempt = 0;
      setIndicator("live");
      // Dopo una riconnessione potremmo aver perso degli aggiornamenti.
      if (connectedBefore) handlers.onResync();
      connectedBefore = true;
    } else if (message?.type === "change" && typeof message.topic === "string") {
      handlers.onChange(message.topic);
    }
  });
  ws.addEventListener("close", (event) => {
    if (socket !== ws) return;
    socket = null;
    if (!wanted) return;
    scheduleReconnect(event.code === SESSION_EXPIRED_CODE);
  });
}

function scheduleReconnect(immediateSessionCheck = false) {
  const base = Math.min(MAX_DELAY_MS, MIN_DELAY_MS * 2 ** attempt);
  attempt += 1;
  const delay = immediateSessionCheck ? 0 : Math.round(base * (0.75 + Math.random() * 0.5));
  setIndicator("offline", `nuovo tentativo tra ${Math.max(1, Math.round(delay / 1000))} s`);
  clearTimeout(retryTimer);
  retryTimer = setTimeout(async () => {
    retryTimer = null;
    if (!wanted) return;
    // Un rifiuto dell'handshake non espone il codice HTTP: verifichiamo la
    // sessione, così un 401 riporta al login invece di riprovare all'infinito.
    try {
      await apiRequest("/api/site/auth/session");
    } catch (error) {
      if (error.status === 401) return;
    }
    if (wanted && !retryTimer) connect();
  }, delay);
}

function reconnectNow() {
  if (!wanted || socket) return;
  attempt = 0;
  connect();
}

export function startLive(nextHandlers) {
  handlers = { ...handlers, ...nextHandlers };
  wanted = true;
  attempt = 0;
  connectedBefore = false;
  $("#live-indicator").classList.remove("hidden");
  if (!listenersReady) {
    listenersReady = true;
    window.addEventListener("online", reconnectNow);
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "visible") reconnectNow();
    });
  }
  connect();
}

export function stopLive() {
  wanted = false;
  clearTimeout(retryTimer);
  retryTimer = null;
  const current = socket;
  socket = null;
  if (current && current.readyState <= WebSocket.OPEN) current.close(1000, "Logout");
  $("#live-indicator").classList.add("hidden");
}
