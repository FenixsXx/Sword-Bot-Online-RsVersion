import { apiRequest } from "./api.js";
import { $, $$, escapeHtml } from "./dom.js";
import { dateKey, formatDate, formatDuration, formatNumber, startOfWeek } from "./format.js";
import { latestOnly, state } from "./state.js";
import { closeDialog, confirmDialog, openDialog, runBusy, showError, showToast } from "./ui.js";

// Categorie e icone per i report.
const CATEGORY_ICONS = { Minigiochi: "🎮", Anime: "🌸", Film: "🎬", Quiz: "🧠", Disegno: "🎨", Musica: "🎵" };
const CATEGORY_CLASSES = { Anime: "orange-cat", Film: "green-cat", Quiz: "blue-cat", Disegno: "pink-cat", Musica: "gold-cat" };
const CATEGORY_BARS = {
  Minigiochi: "purple-bar",
  Anime: "orange-bar",
  Film: "green-bar",
  Quiz: "blue-bar",
  Disegno: "pink-bar",
  Musica: "gold-bar",
};

const nextReportsLoad = latestOnly();
const deletingReports = new Set();

// Calcoli e statistiche per i report.
/** Qui vengono computate le metriche mensili, i trend e i confronti tra periodi. */

/* ---------- Calcoli ---------- */

function compareReports(a, b) {
  return `${b.date}T${b.time}`.localeCompare(`${a.date}T${a.time}`)
    || String(b.created_at || "").localeCompare(String(a.created_at || ""));
}

function getMonthlyStats(sourceReports, now = new Date()) {
  const currentEnd = dateKey(now);
  const currentStart = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}-01`;
  const previousMonth = new Date(now.getFullYear(), now.getMonth() - 1, 1);
  const previousStart = dateKey(previousMonth);
  const previousEnd = dateKey(new Date(now.getFullYear(), now.getMonth(), 0));
  const summarize = (start, end) => {
    const periodReports = sourceReports.filter((report) => report.date >= start && report.date <= end);
    return {
      events: periodReports.length,
      newMembers: periodReports.reduce((sum, report) => sum + Number(report.newMembers), 0),
      averageDuration: periodReports.length
        ? periodReports.reduce((sum, report) => sum + Number(report.duration), 0) / periodReports.length
        : 0,
    };
  };
  return {
    previousLabel: previousMonth.toLocaleDateString("it-IT", { month: "long" }),
    current: summarize(currentStart, currentEnd),
    previous: summarize(previousStart, previousEnd),
  };
}

/** Riempie un indicatore `.trend` confrontando due valori (usato anche dalle statistiche). */
export function renderTrend(trend, current, previous, previousLabel, { duration = false, unit = "", currentLabel = "questo mese" } = {}) {
  const format = (value) => escapeHtml(duration ? formatDuration(Math.round(value)) : `${value}${unit}`);
  const label = escapeHtml(previousLabel);
  const comparison = `vs ${format(previous)} ${label}`;
  if (current === 0 && previous === 0) {
    trend.className = "trend neutral";
    trend.innerHTML = `— <b>Nessun dato</b> <span>né ${escapeHtml(currentLabel)} né ${label}</span>`;
    return;
  }
  if (duration && current === 0) {
    trend.className = "trend neutral";
    trend.innerHTML = `— <b>Nessun evento</b> <span>${escapeHtml(currentLabel)} · ${format(previous)} ${label}</span>`;
    return;
  }
  if (previous === 0) {
    trend.className = `trend ${duration ? "neutral" : "positive"}`;
    trend.innerHTML = duration
      ? `— <b>Nessun confronto</b> <span>nessun evento ${label}</span>`
      : `↗ <b>+${escapeHtml(current)}</b> <span>vs 0 ${label}</span>`;
    return;
  }
  const difference = current - previous;
  if (difference === 0 || (duration && Math.abs(difference) < 0.5)) {
    trend.className = "trend neutral";
    trend.innerHTML = `— <b>Stabile</b> <span>${comparison}</span>`;
    return;
  }
  const percentage = Math.round(Math.abs(difference / previous) * 100);
  const changeLabel = percentage ? `${percentage}%` : "&lt;1%";
  const direction = difference > 0 ? "↗" : "↘";
  trend.className = `trend ${duration ? "neutral" : difference > 0 ? "positive" : "negative"}`;
  trend.innerHTML = `${direction} <b>${difference > 0 ? "+" : "−"}${changeLabel}</b> <span>${comparison}</span>`;
}

/* ---------- Tabelle ---------- */

function reportRow(report, detailed = false) {
  const icon = CATEGORY_ICONS[report.category] || "✦";
  const name = escapeHtml(report.name);
  const notes = escapeHtml(report.notes);
  const category = escapeHtml(report.category);
  const id = escapeHtml(report.id || "");
  const names = escapeHtml(report.names);
  const dateLabel = escapeHtml(formatDate(report.date, report.time));
  const attendees = escapeHtml(report.attendees);
  const newMembers = escapeHtml(report.newMembers);
  const duration = escapeHtml(formatDuration(Number(report.duration)));
  return `<tr>
    <td data-label="Evento"><div class="event-cell"><span class="event-emoji category-icon category-${category.toLowerCase()}">${icon}</span><div><strong>${name}</strong>${!detailed && notes ? `<small class="table-note">${notes}</small>` : ""}</div></div></td>
    ${detailed ? `<td data-label="Categoria"><span class="category ${CATEGORY_CLASSES[report.category] || ""}">${category}</span></td>` : ""}
    <td class="date-cell" data-label="Data e ora">${dateLabel}</td><td data-label="Presenze">${attendees}</td><td class="new-count" data-label="Nuovi membri"${names ? ` title="${names}"` : ""}>+${newMembers}${detailed && names ? `<small class="table-note">${names}</small>` : ""}</td><td data-label="Durata">${duration}</td>
    ${detailed ? `<td data-label="Responsabile">${escapeHtml(report.responsible) || "—"}</td><td data-label="Clip social">${report.clipRecorded ? escapeHtml(report.clipRegistrar) || "Sì" : "No"}</td><td data-label="Flop">${report.flop ? "Sì" : "No"}</td><td class="notes-cell" data-label="Note">${notes || "—"}</td>` : ""}
    <td data-label="Azioni"><div class="report-actions"><button type="button" class="row-action edit-report-action" data-edit-report="${id}" aria-label="Modifica report ${name}" title="Modifica"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="m16 4 4 4M4 20l4-.8L19 8a2.1 2.1 0 0 0-3-3L5 16l-1 4Z" /></svg></button><button type="button" class="row-action" data-delete-report="${id}" aria-label="Elimina report ${name}" title="Elimina">×</button></div></td>
  </tr>`;
}

function updateQuickSelects() {
  const unique = (key) => [...new Set(state.reports.map((report) => report[key]).filter(Boolean))].sort();
  const options = (key) => unique(key).map((value) => `<option value="${escapeHtml(value)}"></option>`).join("");
  $("#event-name-options").innerHTML = options("name");
  $("#responsible-options").innerHTML = options("responsible");
  $("#registrar-options").innerHTML = options("clipRegistrar");
}

/* ---------- Panoramica e insights ---------- */

function renderCategoryLegend() {
  const grouped = state.reports.reduce((result, report) => {
    const item = result[report.category] || { total: 0, count: 0 };
    item.total += Number(report.attendees);
    item.count += 1;
    result[report.category] = item;
    return result;
  }, {});
  const entries = Object.entries(CATEGORY_BARS)
    .map(([category, className]) => {
      const item = grouped[category];
      return { category, className, average: item ? item.total / item.count : 0, count: item?.count || 0 };
    })
    .sort((a, b) => b.average - a.average || a.category.localeCompare(b.category, "it"));
  const maxAverage = Math.max(0, ...entries.map((item) => item.average));
  $("#category-legend").innerHTML = entries.map(({ category, className, average, count }) => {
    const width = maxAverage ? average / maxAverage * 100 : 0;
    const averageLabel = count ? formatNumber(average) : "—";
    const eventLabel = count === 1 ? "1 evento" : `${count} eventi`;
    return `<div class="category-average-row">
      <span class="category-average-name">${category}</span>
      <div class="category-average-track" role="img" aria-label="${category}: ${count ? `${formatNumber(average)} presenze medie per evento, ${eventLabel}` : "nessun evento registrato"}">
        <span class="category-average-fill ${className}" style="width:${width}%"></span>
      </div>
      <span class="category-average-value">${averageLabel}<small>${count ? eventLabel : "nessun evento"}</small></span>
    </div>`;
  }).join("");
}

function renderLeaderboard() {
  const grouped = Object.values(state.reports.reduce((result, report) => {
    const label = String(report.name).trim();
    const key = label.toLocaleLowerCase("it-IT");
    const item = result[key] || { name: label, variants: {}, total: 0, count: 0 };
    item.variants[label] = (item.variants[label] || 0) + 1;
    if (item.variants[label] > item.variants[item.name]) item.name = label;
    item.total += Number(report.attendees);
    item.count += 1;
    result[key] = item;
    return result;
  }, {}));
  const top = grouped
    .map((event) => ({ ...event, average: event.total / event.count }))
    .sort((a, b) => b.average - a.average || b.count - a.count || a.name.localeCompare(b.name, "it"))
    .slice(0, 3);
  $("#top-events").innerHTML = top.map((event, index) => {
    const eventLabel = event.count === 1 ? "1 evento" : `${event.count} eventi`;
    return `<div class="leaderboard-row" title="${escapeHtml(event.name)}: ${formatNumber(event.average)} presenze medie su ${eventLabel}"><b>${index + 1}</b><span>${escapeHtml(event.name)}</span><strong>${formatNumber(event.average)} di media</strong></div>`;
  }).join("") || "<p>Nessun evento disponibile.</p>";
}

function renderBestMoment() {
  const days = ["Domenica", "Lunedì", "Martedì", "Mercoledì", "Giovedì", "Venerdì", "Sabato"];
  const slotFor = (hour) => hour < 6 ? "notte" : hour < 12 ? "mattina" : hour < 18 ? "pomeriggio" : "sera";
  const slots = state.reports.reduce((result, report) => {
    const date = new Date(`${report.date}T${report.time}`);
    const slot = slotFor(date.getHours());
    const key = `${date.getDay()}-${slot}`;
    const item = result[key] || { day: days[date.getDay()], slot, total: 0, count: 0 };
    item.total += Number(report.attendees);
    item.count += 1;
    result[key] = item;
    return result;
  }, {});
  const best = Object.values(slots)
    .map((item) => ({ ...item, average: item.total / item.count }))
    .sort((a, b) => b.average - a.average || b.count - a.count)[0];
  if (!best) {
    $("#best-moment").textContent = "—";
    $("#best-moment-detail").textContent = "Inserisci più report per calcolare il momento con più presenze.";
    return;
  }
  const eventLabel = best.count === 1 ? "1 evento" : `${best.count} eventi`;
  $("#best-moment").textContent = `${best.day} ${best.slot}`;
  $("#best-moment-detail").innerHTML = `È il momento con più presenze medie: <b>${formatNumber(best.average)} partecipanti</b> su ${eventLabel}.`;
}

function chartMax(value) {
  const step = Math.max(1, Math.ceil(value / 4));
  const magnitude = 10 ** Math.floor(Math.log10(step));
  const niceStep = [1, 2, 3, 4, 5, 6, 8, 10].map((factor) => factor * magnitude).find((candidate) => candidate >= step);
  return Math.max(4, niceStep * 4);
}

function renderActivityChart() {
  const monday = startOfWeek(new Date());
  monday.setDate(monday.getDate() + state.weekOffset * 7);
  const values = Array.from({ length: 7 }, (_, index) => {
    const date = new Date(monday);
    date.setDate(monday.getDate() + index);
    const dayReports = state.reports.filter((report) => report.date === dateKey(date));
    return {
      date,
      events: dayReports.length,
      attendees: dayReports.reduce((sum, report) => sum + Number(report.attendees), 0),
      newMembers: dayReports.reduce((sum, report) => sum + Number(report.newMembers), 0),
    };
  });
  const max = chartMax(Math.max(...values.map((item) => item.attendees)));
  const xFor = (index) => (index + 0.5) * (620 / 7);
  const yFor = (value) => 190 - (value / max) * 190;
  const pathFor = (key) => values.map((item, index) => `${index ? "L" : "M"}${xFor(index).toFixed(2)},${yFor(item[key]).toFixed(2)}`).join(" ");
  const line = pathFor("attendees");
  $("#chart-line").setAttribute("d", line);
  $("#chart-fill").setAttribute("d", `${line} L${xFor(6).toFixed(2)},190 L${xFor(0).toFixed(2)},190 Z`);
  $("#chart-line-new").setAttribute("d", pathFor("newMembers"));
  const dayLabel = (item) => item.date.toLocaleDateString("it-IT", { weekday: "long", day: "2-digit", month: "long" });
  const eventsLabel = (item) => item.events === 1 ? "1 evento" : `${item.events} eventi`;
  $("#chart-points").innerHTML = values.map((item, index) => `<button type="button" class="chart-point" data-day-index="${index}" style="left:${(xFor(index) / 620 * 100).toFixed(3)}%;top:${(yFor(item.attendees) / 190 * 100).toFixed(3)}%" aria-label="${escapeHtml(`${dayLabel(item)}: ${item.attendees} presenze, ${item.newMembers} nuovi membri, ${eventsLabel(item)}`)}"></button>`).join("");
  const tooltip = $("#chart-tooltip");
  const showDayTooltip = (index, point) => {
    const item = values[index];
    $("#chart-tooltip-day").textContent = dayLabel(item);
    $("#chart-tooltip-attendees").textContent = `${item.attendees} presenze · ${eventsLabel(item)}`;
    $("#chart-tooltip-new-members").textContent = `${item.newMembers} nuovi membri`;
    tooltip.classList.remove("hidden");
    const pointRect = point.getBoundingClientRect();
    const chartRect = $("#chart-area").getBoundingClientRect();
    const left = pointRect.left + pointRect.width / 2 - chartRect.left;
    const top = pointRect.top + pointRect.height / 2 - chartRect.top;
    const tooltipLeft = Math.max(0, Math.min(left - tooltip.offsetWidth / 2, chartRect.width - tooltip.offsetWidth));
    const tooltipTop = top - tooltip.offsetHeight - 14 >= 0
      ? top - tooltip.offsetHeight - 14
      : Math.min(top + 14, chartRect.height - tooltip.offsetHeight);
    tooltip.style.left = `${tooltipLeft}px`;
    tooltip.style.top = `${Math.max(0, tooltipTop)}px`;
  };
  const hideDayTooltip = () => tooltip.classList.add("hidden");
  hideDayTooltip();
  $$("#chart-points .chart-point").forEach((point) => {
    const index = Number(point.dataset.dayIndex);
    point.addEventListener("pointerenter", () => showDayTooltip(index, point));
    point.addEventListener("pointerleave", hideDayTooltip);
    point.addEventListener("click", () => showDayTooltip(index, point));
    point.addEventListener("focus", () => showDayTooltip(index, point));
    point.addEventListener("blur", hideDayTooltip);
  });
  $("#chart-x-labels").innerHTML = values.map((item) => `<span>${escapeHtml(item.date.toLocaleDateString("it-IT", { weekday: "short", day: "2-digit" }))}</span>`).join("");
  $("#chart-y-labels").innerHTML = [max, max * .75, max * .5, max * .25, 0].map((value) => `<span>${escapeHtml(value)}</span>`).join("");
  $("#chart-attendees").textContent = values.reduce((sum, item) => sum + item.attendees, 0);
  $("#chart-new-members").textContent = values.reduce((sum, item) => sum + item.newMembers, 0);
}

function renderWeekLabel() {
  const monday = startOfWeek(new Date());
  monday.setDate(monday.getDate() + state.weekOffset * 7);
  const sunday = new Date(monday);
  sunday.setDate(monday.getDate() + 6);
  const format = (date) => date.toLocaleDateString("it-IT", { day: "2-digit", month: "short", year: "numeric" });
  $("#week-label").textContent = `SETTIMANA ${format(monday)} — ${format(sunday)}`.toUpperCase();
}

function renderDayAverages() {
  const names = ["Dom", "Lun", "Mar", "Mer", "Gio", "Ven", "Sab"];
  const values = names.map((name, day) => {
    const matching = state.reports.filter((report) => new Date(`${report.date}T${report.time}`).getDay() === day);
    return { name, value: matching.length ? matching.reduce((sum, report) => sum + Number(report.attendees), 0) / matching.length : 0 };
  });
  const max = Math.max(1, ...values.map((item) => item.value));
  $("#day-bars").innerHTML = values.slice(1).concat(values[0]).map((item) => `<div class="${item.value === max ? "best" : ""}"><span>${item.name}</span><i style="height:${Math.max(8, item.value / max * 92)}%"><b>${Math.round(item.value)}</b></i></div>`).join("");
}

export function debounce(fn, delayMs = 150) {
  let timeoutId;
  return (...args) => {
    clearTimeout(timeoutId);
    timeoutId = setTimeout(() => fn(...args), delayMs);
  };
}

export function renderReportsTable() {
  const { reports } = state;
  const query = $("#report-search").value.trim().toLowerCase();
  const category = $("#report-filter").value || "all";
  const filtered = reports.filter((report) => {
    const haystack = [report.name, report.responsible, report.notes, report.names, report.clipRegistrar].join(" ").toLowerCase();
    return haystack.includes(query) && (category === "all" || report.category === category);
  });
  $("#all-reports").innerHTML = filtered.map((report) => reportRow(report, true)).join("")
    || `<tr><td colspan="11" class="empty-state">Nessun report trovato corrispondente alla ricerca.</td></tr>`;
}

export function renderReports() {
  const { reports } = state;
  $("#recent-reports").innerHTML = reports.slice(0, 4).map((report) => reportRow(report)).join("");
  renderReportsTable();
  const average = reports.length ? Math.round(reports.reduce((sum, report) => sum + Number(report.duration), 0) / reports.length) : 0;
  const monthly = getMonthlyStats(reports);
  const previousLabel = `a ${monthly.previousLabel}`;
  $("#total-events").textContent = monthly.current.events;
  $("#total-new-members").textContent = monthly.current.newMembers;
  $("#average-duration").textContent = monthly.current.events ? formatDuration(Math.round(monthly.current.averageDuration)) : "—";
  renderTrend($("#events-trend"), monthly.current.events, monthly.previous.events, previousLabel);
  renderTrend($("#members-trend"), monthly.current.newMembers, monthly.previous.newMembers, previousLabel);
  renderTrend($("#duration-trend"), monthly.current.averageDuration, monthly.previous.averageDuration, previousLabel, { duration: true });
  renderCategoryLegend();
  renderLeaderboard();
  renderBestMoment();
  $("#insight-average-duration").textContent = reports.length ? formatDuration(average) : "—";
  renderActivityChart();
  renderDayAverages();
  renderWeekLabel();
  updateQuickSelects();
}

/* ---------- Caricamento ---------- */

export async function loadReports() {
  const turn = nextReportsLoad();
  const pageSize = 1000;
  const data = [];
  for (let offset = 0; ; ) {
    const page = await apiRequest(`/api/site/reports?offset=${offset}&limit=${pageSize}`);
    if (!turn.isCurrent()) return;
    if (!Array.isArray(page)) throw new Error("Il server non ha restituito un elenco di report valido.");
    if (page.length === 0) break;
    data.push(...page);
    offset += page.length;
  }
  state.reports = data.map((report) => ({
    ...report,
    date: report.event_date,
    time: String(report.event_time || "").slice(0, 5),
    newMembers: report.new_members,
    names: report.new_member_names,
    clipRecorded: report.clip_recorded,
    clipRegistrar: report.clip_registrar,
  })).sort(compareReports);
  renderReports();
}

/* ---------- Modale report ---------- */

function toggleRegistrarField(visible) {
  $("#registrar-field").classList.toggle("hidden", !visible);
  $("#event-registrar").required = visible;
}

export function openReportModal(report = null) {
  state.editingReportId = report?.id || null;
  $("#report-form").reset();
  $("#modal-overline").textContent = report ? "MODIFICA ATTIVITÀ" : "NUOVA ATTIVITÀ";
  $("#modal-title").textContent = report ? "Modifica il report" : "Registra un evento";
  $("#modal-description").textContent = report ? "Aggiorna i dettagli dell'evento." : "Aggiungi i dettagli dell'evento appena concluso.";
  $("#report-submit-label").textContent = report ? "Salva modifiche" : "Salva report";
  if (report) {
    $("#event-name").value = report.name;
    $("#event-category").value = report.category;
    $("#event-date").value = report.date;
    $("#event-time").value = report.time;
    $("#event-duration").value = String(report.duration);
    $("#event-attendees").value = report.attendees;
    $("#event-new").value = report.newMembers;
    $("#event-names").value = report.names || "";
    $("#event-responsible").value = report.responsible || "";
    $("#event-notes").value = report.notes || "";
    $("#event-clip").checked = Boolean(report.clipRecorded);
    $("#event-registrar").value = report.clipRegistrar || "";
    $("#event-flop").checked = Boolean(report.flop);
  } else {
    $("#event-date").value = dateKey(new Date());
    $("#event-time").value = "20:00";
  }
  toggleRegistrarField($("#event-clip").checked);
  openDialog($("#report-modal"), { initialFocus: $("#event-name"), onRequestClose: closeReportModal });
}

export function closeReportModal() {
  closeDialog($("#report-modal"));
  state.editingReportId = null;
}

function readReportForm() {
  return {
    name: $("#event-name").value.trim(),
    category: $("#event-category").value,
    event_date: $("#event-date").value,
    event_time: $("#event-time").value,
    duration: Number($("#event-duration").value),
    attendees: Number($("#event-attendees").value),
    new_members: Number($("#event-new").value),
    new_member_names: $("#event-names").value.trim(),
    responsible: $("#event-responsible").value.trim(),
    notes: $("#event-notes").value.trim(),
    clip_recorded: $("#event-clip").checked,
    clip_registrar: $("#event-clip").checked ? $("#event-registrar").value.trim() : "",
    flop: $("#event-flop").checked,
  };
}

async function submitReport(event) {
  event.preventDefault();
  const form = event.currentTarget;
  const payload = readReportForm();
  const newMembersInput = $("#event-new");
  if (payload.new_members > payload.attendees) {
    newMembersInput.setCustomValidity("I nuovi membri non possono superare le presenze.");
    newMembersInput.reportValidity();
    newMembersInput.setCustomValidity("");
    return;
  }
  const editingId = state.editingReportId;
  await runBusy(form, async () => {
    let warning = "";
    try {
      if (editingId) {
        await apiRequest(`/api/site/reports/${encodeURIComponent(editingId)}`, { method: "PUT", body: payload });
      } else {
        const created = await apiRequest("/api/site/reports", { method: "POST", body: payload });
        try {
          await apiRequest(`/api/site/reports/${encodeURIComponent(created.id)}/publish`, { method: "POST", body: {} });
        } catch (error) {
          if (error.sessionExpired) throw error;
          console.error("Report salvato ma non pubblicato su Discord.", error);
          warning = "Report salvato, ma non inviato su Discord. Verifica il canale report e la configurazione del bot.";
        }
      }
      closeReportModal();
      form.reset();
      showToast(warning || (editingId ? "Modifiche salvate." : "Evento salvato e inviato su Discord."), warning ? 6000 : 3000, warning ? "info" : "success");
      await loadReports();
    } catch (error) {
      showError(error);
    }
  });
}

async function deleteReport(button) {
  const report = state.reports.find((item) => item.id === button.dataset.deleteReport);
  if (!report || deletingReports.has(report.id)) return;
  const confirmed = await confirmDialog({
    overline: "ELIMINA REPORT",
    title: "Eliminare il report?",
    message: `Il report “${report.name}” verrà eliminato definitivamente.`,
    confirmLabel: "Elimina report",
  });
  if (!confirmed) return;
  deletingReports.add(report.id);
  try {
    await runBusy(button, async () => {
      await apiRequest(`/api/site/reports/${encodeURIComponent(report.id)}`, { method: "DELETE" });
      showToast("Report eliminato.", 3000, "success");
      await loadReports();
    });
  } catch (error) {
    showError(error);
  } finally {
    deletingReports.delete(report.id);
  }
}

/* ---------- Eventi ---------- */

export function initReports() {
  $$("[data-open-report]").forEach((button) => button.addEventListener("click", () => openReportModal()));
  $$("[data-close-modal]").forEach((button) => button.addEventListener("click", closeReportModal));
  $("#report-modal").addEventListener("click", (event) => {
    if (event.target.id === "report-modal") closeReportModal();
  });
  $("#report-form").addEventListener("submit", submitReport);
  $("#event-clip").addEventListener("change", (event) => toggleRegistrarField(event.target.checked));
  $("#report-search").addEventListener("input", debounce(renderReportsTable, 150));
  $("#report-filter").addEventListener("change", renderReportsTable);
  $("[data-week-prev]").addEventListener("click", () => {
    state.weekOffset -= 1;
    renderActivityChart();
    renderWeekLabel();
  });
  $("[data-week-next]").addEventListener("click", () => {
    state.weekOffset += 1;
    renderActivityChart();
    renderWeekLabel();
  });
  for (const table of ["#recent-reports", "#all-reports"]) {
    $(table).addEventListener("click", (event) => {
      const editButton = event.target.closest("[data-edit-report]");
      if (editButton?.dataset.editReport) {
        const report = state.reports.find((item) => item.id === editButton.dataset.editReport);
        if (report) openReportModal(report);
        return;
      }
      const deleteButton = event.target.closest("[data-delete-report]");
      if (deleteButton?.dataset.deleteReport) deleteReport(deleteButton);
    });
  }
}
