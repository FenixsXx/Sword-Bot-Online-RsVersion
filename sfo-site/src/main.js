import "../styles.css";
import { initAdmin, loadAdminState, resetAdmin } from "./admin.js";
import { apiRequest, onUnauthorized } from "./api.js";
import { $, $$ } from "./dom.js";
import { dateKey } from "./format.js";
import { startLive, stopLive } from "./live.js";
import { initReports, loadReports, renderReports } from "./reports.js";
import { resetState, state } from "./state.js";
import { initStats, loadStats, reloadStatsForTopic, resetStats } from "./stats.js";
import { initTools, loadToolsState, resetTools } from "./tools.js";
import { closeAllDialogs, hideToast, initConfirmDialog, initTheme, runBusy, showError } from "./ui.js";

// Navigazione tra le diverse viste dell'applicazione.
const REPORT_VIEWS = new Set(["overview", "reports", "analytics"]);
const VIEW_LOADERS = {
  manage: (options) => loadAdminState(options),
  tools: (options) => loadToolsState(options),
  stats: () => loadStats(),
};
const pendingViewLoads = new Map();
let reportsStale = false;

/* ---------- Navigazione ---------- */

function showView(view) {
  state.currentView = view;
  $$(".page-view").forEach((page) => page.classList.toggle("hidden", page.id !== `${view}-view`));
  $$(".nav-item").forEach((item) => {
    const active = item.dataset.view === view;
    item.classList.toggle("active", active);
    if (active) item.setAttribute("aria-current", "page");
    else item.removeAttribute("aria-current");
  });
}

function trackViewLoad(view, promise) {
  const buttons = $$(`.nav-item[data-view="${view}"]`);
  buttons.forEach((button) => button.setAttribute("aria-busy", "true"));
  pendingViewLoads.set(view, promise);
  promise
    .catch(showError)
    .finally(() => {
      pendingViewLoads.delete(view);
      buttons.forEach((button) => button.removeAttribute("aria-busy"));
    });
}

/** Carica i dati della vista; i clic ripetuti durante il caricamento sono ignorati. */
function loadView(view) {
  if (pendingViewLoads.has(view)) return;
  if (REPORT_VIEWS.has(view)) {
    if (!reportsStale) return;
    reportsStale = false;
    trackViewLoad(view, loadReports());
    return;
  }
  const loader = VIEW_LOADERS[view];
  if (loader) trackViewLoad(view, loader());
}

function navigate(view) {
  showView(view);
  loadView(view);
}

/* ---------- Aggiornamenti in tempo reale ---------- */

function logLiveError(error) {
  if (!error?.sessionExpired) console.error("Aggiornamento in tempo reale non riuscito.", error);
}

function handleLiveChange(topic) {
  const view = state.currentView;
  if (topic === "reports") {
    if (REPORT_VIEWS.has(view)) loadReports().catch(logLiveError);
    else reportsStale = true;
    return;
  }
  if (view === "manage" && (topic === "settings" || topic === "polls")) {
    loadAdminState({ live: true }).catch(logLiveError);
  } else if (view === "tools" && ["proposals", "events", "settings"].includes(topic)) {
    loadToolsState({ live: true }).catch(logLiveError);
  } else if (view === "stats") {
    reloadStatsForTopic(topic).catch(logLiveError);
  }
}

function handleLiveResync() {
  const view = state.currentView;
  if (REPORT_VIEWS.has(view)) {
    loadReports().catch(logLiveError);
    return;
  }
  reportsStale = true;
  VIEW_LOADERS[view]?.({ live: true }).catch(logLiveError);
}

/* ---------- Sessione ---------- */

function enterApp(session) {
  state.session = session;
  $("#login-error").textContent = "";
  $("#login-view").classList.add("hidden");
  $("#app-view").classList.remove("hidden");
  const identity = $("#staff-identity");
  const indicator = identity.querySelector("i");
  identity.replaceChildren(
    ...(indicator ? [indicator] : []),
    document.createTextNode(session?.display_name || "Staff online"),
  );
  $$('[data-view="manage"]').forEach((item) => item.classList.toggle("hidden", !session?.is_admin));
  reportsStale = false;
  loadReports().catch(showError);
  startLive({ onChange: handleLiveChange, onResync: handleLiveResync });
}

/** Torna al login azzerando tutti i dati in memoria. */
function showLogin(message = "") {
  stopLive();
  closeAllDialogs();
  hideToast();
  resetState();
  resetAdmin();
  resetTools();
  resetStats();
  pendingViewLoads.clear();
  reportsStale = false;
  renderReports();
  showView("overview");
  $("#app-view").classList.add("hidden");
  $("#login-view").classList.remove("hidden");
  $("#login-error").textContent = message;
  const loginButton = $("#login-form [type=submit]");
  loginButton.disabled = false;
  loginButton.focus();
}

function handleSessionExpired() {
  if (!state.session) return;
  showLogin("La sessione è scaduta. Accedi di nuovo con Discord.");
}

async function logout(event) {
  await runBusy(event.currentTarget, async () => {
    try {
      await apiRequest("/api/site/auth/logout", { method: "POST", allowUnauthorized: true });
    } catch (error) {
      showError(error);
    }
  });
  showLogin();
}

/* ---------- Avvio ---------- */

initTheme();
initConfirmDialog();
initReports();
initAdmin();
initTools();
initStats();
onUnauthorized(handleSessionExpired);

$$(".nav-item").forEach((item) => item.addEventListener("click", () => navigate(item.dataset.view)));
$$("[data-view-target]").forEach((button) => button.addEventListener("click", () => navigate(button.dataset.viewTarget)));
$("#logout-button").addEventListener("click", logout);
$("#login-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const submitButton = event.submitter || $("#login-form [type=submit]");
  if (submitButton.disabled) return;
  submitButton.disabled = true;
  window.location.assign("/staff/auth/login");
});

const loginError = new URLSearchParams(window.location.search).get("login_error");
if (loginError) {
  const messages = {
    discord: "Accesso Discord non riuscito. Riprova.",
    forbidden: "Il tuo account non ha i permessi richiesti nel server Discord.",
    unavailable: "Discord non è raggiungibile al momento.",
  };
  $("#login-error").textContent = messages[loginError] || "Accesso non riuscito.";
  window.history.replaceState({}, "", window.location.pathname);
}

apiRequest("/api/site/auth/session", { allowUnauthorized: true })
  .then((session) => enterApp(session))
  .catch((error) => {
    if (error.status !== 401) {
      $("#login-error").textContent = error.message || "Impossibile verificare la sessione.";
    }
  });

let renderedDay = dateKey(new Date());
setInterval(() => {
  const today = dateKey(new Date());
  if (today === renderedDay) return;
  renderedDay = today;
  if (state.session) renderReports();
}, 60000);
