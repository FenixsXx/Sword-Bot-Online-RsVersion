import { apiRequest } from "./api.js";
import { $, escapeHtml } from "./dom.js";
import { formatDateTime, formatNumber, formatSeconds, pluralize } from "./format.js";
import { latestOnly } from "./state.js";
import { runBusy, showError } from "./ui.js";

const WEEKDAYS = ["Lun", "Mar", "Mer", "Gio", "Ven", "Sab", "Dom"];
const COMPONENTS = [
  { key: "partecipazione", label: "Partecipazione", max: 35, className: "purple-bar" },
  { key: "continuità", label: "Continuità", max: 25, className: "green-bar" },
  { key: "varietà", label: "Varietà", max: 20, className: "orange-bar" },
  { key: "coinvolgimento", label: "Coinvolgimento", max: 15, className: "blue-bar" },
  { key: "eventi", label: "Eventi", max: 10, className: "gold-bar" },
];
const ACTION_LABELS = {
  proposal: "Proposte",
  support: "Sostegni",
  vote: "Voti",
  event_join: "Ingressi eventi",
};
/** Quali sezioni ricaricare per ogni topic ricevuto in tempo reale. */
const TOPIC_SECTIONS = {
  proposals: ["overview", "health", "heatmap", "accuracy"],
  events: ["overview", "health", "heatmap", "events"],
  polls: ["health", "heatmap"],
};

const period = () => Number($("#stats-period").value) || 30;
let lastHistoryQuery = "";

/* ---------- Rendering ---------- */

function renderOverview(data) {
  $("#stats-week-label").textContent = `Ballottaggio · settimana #${data.current_week}`;
  $("#stats-selected").textContent = data.selected_proposals;
  const pending = pluralize(data.pending_proposals, "approvata in attesa", "approvate in attesa");
  const live = data.active_events
    ? pluralize(data.active_events, "evento live in corso", "eventi live in corso")
    : pluralize(data.completed_events, "evento live concluso", "eventi live conclusi");
  $("#stats-week-detail").innerHTML = `— <b>${escapeHtml(pending)}</b> <span>${escapeHtml(live)}</span>`;
}

function renderHealth(data) {
  const metrics = data.metrics;
  $("#stats-score").textContent = `${data.score}/100`;
  const trend = $("#stats-score-trend");
  const comparison = `vs ${escapeHtml(data.previous_score)} nei ${escapeHtml(data.period_days)} giorni precedenti`;
  if (data.trend === 0) {
    trend.className = "trend neutral";
    trend.innerHTML = `— <b>Stabile</b> <span>${comparison}</span>`;
  } else {
    const up = data.trend > 0;
    trend.className = `trend ${up ? "positive" : "negative"}`;
    trend.innerHTML = `${up ? "↗" : "↘"} <b>${up ? "+" : "−"}${escapeHtml(Math.abs(data.trend))} punti</b> <span>${comparison}</span>`;
  }
  $("#stats-active-users").textContent = metrics.active_users;
  $("#stats-active-detail").innerHTML = `— <b>${escapeHtml(pluralize(metrics.active_days, "giorno attivo", "giorni attivi"))}</b> <span>${escapeHtml(pluralize(metrics.events, "evento concluso", "eventi conclusi"))}</span>`;
  $("#stats-components").innerHTML = COMPONENTS.map(({ key, label, max, className }) => {
    const value = Number(metrics.components?.[key] || 0);
    const width = Math.min(100, value / max * 100);
    return `<div class="category-average-row">
      <span class="category-average-name">${label}</span>
      <div class="category-average-track" role="img" aria-label="${label}: ${escapeHtml(value)} su ${max} punti">
        <span class="category-average-fill ${className}" style="width:${width}%"></span>
      </div>
      <span class="category-average-value">${escapeHtml(value)}/${max}<small>punti</small></span>
    </div>`;
  }).join("");
  $("#stats-methodology").textContent = data.methodology;
}

function renderHeatmap(data) {
  const counts = new Map(data.cells.map((cell) => [`${cell.weekday}-${cell.hour}`, Number(cell.count)]));
  const max = Math.max(0, ...counts.values());
  const total = [...counts.values()].reduce((sum, value) => sum + value, 0);
  const rows = WEEKDAYS.map((day, weekday) => {
    const cells = Array.from({ length: 24 }, (_, hour) => {
      const count = counts.get(`${weekday}-${hour}`) || 0;
      const alpha = count && max ? (0.18 + 0.82 * count / max).toFixed(2) : 0;
      const style = count ? ` style="background:rgba(137,109,233,${alpha})"` : "";
      const label = `${day} ${String(hour).padStart(2, "0")}:00 · ${pluralize(count, "azione", "azioni")}`;
      return `<span class="heatmap-cell"${style} title="${escapeHtml(label)}"></span>`;
    }).join("");
    return `<span class="heatmap-label">${day}</span>${cells}`;
  }).join("");
  const hours = Array.from({ length: 24 }, (_, hour) => `<span class="heatmap-hour">${hour % 6 === 0 ? hour : ""}</span>`).join("");
  let peak = null;
  for (const cell of data.cells) {
    if (!peak || cell.count > peak.count) peak = cell;
  }
  const summary = peak
    ? `Picco: ${WEEKDAYS[peak.weekday]} alle ${String(peak.hour).padStart(2, "0")}:00 con ${pluralize(peak.count, "azione", "azioni")}`
    : "Nessuna attività registrata nel periodo";
  const heatmap = $("#stats-heatmap");
  heatmap.setAttribute("aria-label", `Attività per giorno e ora negli ultimi ${data.period_days} giorni. ${summary}.`);
  heatmap.innerHTML = `${rows}<span></span>${hours}`;
  $("#stats-heatmap-summary").textContent = `${summary} · ${pluralize(total, "azione", "azioni")} in totale`;
  const actions = Object.entries(data.actions || {});
  $("#stats-actions").innerHTML = actions.length
    ? actions.map(([action, count]) => `<span><i class="legend-dot"></i> ${escapeHtml(ACTION_LABELS[action] || action)} <b>${escapeHtml(count)}</b></span>`).join("")
    : "<span>Nessuna azione registrata</span>";
  $("#stats-heatmap-note").textContent = data.limitations;
}

function emptyRow(colspan, text) {
  return `<tr><td colspan="${colspan}" class="empty-state">${escapeHtml(text)}</td></tr>`;
}

function renderAccuracy(data) {
  $("#stats-accuracy-note").textContent = data.definition;
  $("#stats-accuracy").innerHTML = data.staff.length
    ? data.staff.map((row) => `<tr>
        <td data-label="Staff">${escapeHtml(row.name)}</td>
        <td data-label="Selezionate">${escapeHtml(row.selected)}</td>
        <td data-label="Vincitrici">${escapeHtml(row.won)}</td>
        <td class="new-count" data-label="Precisione">${escapeHtml(formatNumber(row.accuracy_percent))}%</td>
      </tr>`).join("")
    : emptyRow(4, "Nessuna selezione archiviata.");
}

function renderEvents(data) {
  $("#stats-events-note").textContent = data.limitations;
  $("#stats-events").innerHTML = data.reports.length
    ? data.reports.map((report) => {
      const speaker = report.top_speakers[0];
      const speakerLabel = speaker
        ? `${speaker.name} · ${formatSeconds(speaker.speaking_seconds_estimated)} (${formatNumber(speaker.share_percent)}%)`
        : "—";
      return `<tr>
        <td data-label="Evento">${escapeHtml(report.title)}</td>
        <td class="date-cell" data-label="Inizio">${escapeHtml(formatDateTime(report.started_at))}</td>
        <td data-label="Durata">${escapeHtml(formatSeconds(report.duration_seconds))}</td>
        <td data-label="Partecipanti">${escapeHtml(report.participant_count)}</td>
        <td data-label="Picco">${escapeHtml(report.peak_participants)}</td>
        <td data-label="Presenza media">${escapeHtml(formatSeconds(report.average_presence_seconds))}</td>
        <td data-label="Più attivo al microfono">${escapeHtml(speakerLabel)}</td>
      </tr>`;
    }).join("")
    : emptyRow(7, "Nessun evento live concluso.");
}

function historyBadge(item) {
  if (item.won) return '<span class="category green-cat">Vincitrice</span>';
  if (item.status === "picked" || item.selected_by || item.selected_at) return '<span class="category">Nel ballottaggio</span>';
  return '<span class="category blue-cat">Proposta</span>';
}

function renderHistory(data) {
  $("#stats-history-summary").textContent = `“${data.query}”: ${pluralize(data.appearances, "apparizione", "apparizioni")} · ${data.ballot_entries} nel ballottaggio · ${pluralize(data.wins, "vittoria", "vittorie")}`;
  $("#stats-history-results").innerHTML = data.timeline.length
    ? data.timeline.map((item) => `<article class="tool-row">
        <span><strong>${escapeHtml(item.name)}</strong><small>${escapeHtml(item.type)} · settimana #${escapeHtml(item.week)}${item.selected_at ? ` · scelta il ${escapeHtml(formatDateTime(item.selected_at))}` : ""}</small></span>
        ${historyBadge(item)}
      </article>`).join("")
    : '<p class="empty-state">Nessuna proposta trovata.</p>';
  $("#stats-history-note").textContent = data.limitations;
}

/* ---------- Caricamento ---------- */

const SECTIONS = {
  overview: { path: () => "/api/site/stats/overview", render: renderOverview },
  health: { path: () => `/api/site/stats/health?days=${period()}`, render: renderHealth },
  heatmap: { path: () => `/api/site/stats/heatmap?days=${period()}`, render: renderHeatmap },
  accuracy: { path: () => "/api/site/stats/accuracy", render: renderAccuracy },
  events: { path: () => "/api/site/stats/events?limit=20", render: renderEvents },
};
for (const section of Object.values(SECTIONS)) section.next = latestOnly();
const nextHistory = latestOnly();

async function loadSection(name) {
  const section = SECTIONS[name];
  const turn = section.next();
  const data = await apiRequest(section.path());
  if (turn.isCurrent()) section.render(data);
}

/** Carica le sezioni indicate (tutte per default); le risposte superate vengono ignorate. */
export async function loadStats(names = Object.keys(SECTIONS)) {
  const view = $("#stats-view");
  view.setAttribute("aria-busy", "true");
  try {
    const results = await Promise.allSettled(names.map(loadSection));
    const failure = results.find((result) => result.status === "rejected");
    if (failure) throw failure.reason;
  } finally {
    view.removeAttribute("aria-busy");
  }
}

async function searchHistory(query) {
  const turn = nextHistory();
  const params = new URLSearchParams({ q: query, limit: "50" });
  const data = await apiRequest(`/api/site/stats/history?${params}`);
  if (turn.isCurrent()) {
    lastHistoryQuery = query;
    renderHistory(data);
  }
}

/** Ricarica solo i dati delle statistiche toccati da un cambiamento. */
export function reloadStatsForTopic(topic) {
  const names = TOPIC_SECTIONS[topic];
  if (!names) return Promise.resolve();
  const tasks = [loadStats(names)];
  if (topic === "proposals" && lastHistoryQuery) tasks.push(searchHistory(lastHistoryQuery));
  return Promise.all(tasks);
}

export function resetStats() {
  lastHistoryQuery = "";
  $("#stats-view").removeAttribute("aria-busy");
  for (const selector of ["#stats-score", "#stats-active-users", "#stats-selected"]) $(selector).textContent = "—";
  for (const selector of ["#stats-components", "#stats-heatmap", "#stats-actions", "#stats-accuracy", "#stats-events", "#stats-history-results"]) {
    $(selector).replaceChildren();
  }
  $("#stats-history-summary").textContent = "";
  $("#stats-history-query").value = "";
}

export function initStats() {
  const refresh = $("#stats-refresh");
  refresh.addEventListener("click", () => runBusy(refresh, () => loadStats().catch(showError)));
  const periodSelect = $("#stats-period");
  periodSelect.addEventListener("change", () => loadStats(["health", "heatmap"]).catch(showError));
  const form = $("#stats-history-form");
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const query = $("#stats-history-query").value.trim();
    if (query.length < 2) {
      const input = $("#stats-history-query");
      input.setCustomValidity("Scrivi almeno 2 caratteri.");
      input.reportValidity();
      input.setCustomValidity("");
      return;
    }
    runBusy(form, () => searchHistory(query).catch(showError));
  });
}
