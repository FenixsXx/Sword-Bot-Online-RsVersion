import { fillChannelSelect } from "./admin.js";
import { apiRequest } from "./api.js";
import { $, $$, escapeHtml, safeHttpUrl } from "./dom.js";
import { latestOnly } from "./state.js";
import { confirmDialog, runBusy, showError, showToast } from "./ui.js";

const nextToolsLoad = latestOnly();
let toolsState = null;
let liveFormDirty = false;

function proposalCard(proposal) {
  const id = escapeHtml(proposal.id);
  const reasonId = `proposal-reason-${id}`;
  const reviewActions = proposal.review_status === "pending"
    ? `<button type="button" class="button button-ghost" data-proposal-review="${id}" data-action="review">Prendi in carico</button>`
    : proposal.review_status === "reviewing"
      ? `<label class="visually-hidden" for="${reasonId}">Motivo della decisione sulla proposta #${id} (obbligatorio per rifiutare)</label><input id="${reasonId}" class="proposal-reason" data-reason-for="${id}" maxlength="200" placeholder="Motivo (obbligatorio per rifiutare)" /><button type="button" class="button button-primary" data-proposal-review="${id}" data-action="approve">Approva</button><button type="button" class="button button-ghost" data-proposal-review="${id}" data-action="reject">Rifiuta</button>`
      : "";
  const ballotAction = proposal.status === "picked"
    ? `<button type="button" class="button button-ghost" data-proposal-unpick="${id}">Rimuovi dal ballottaggio</button>`
    : proposal.review_status === "approved"
      ? `<button type="button" class="button button-primary" data-proposal-choose="${id}">Aggiungi al ballottaggio</button>`
      : "";
  const link = safeHttpUrl(proposal.link);
  return `<article class="poll-card tool-card" data-proposal-card="${id}">
    <div class="poll-card-header"><h4>#${id} · ${escapeHtml(proposal.name)}</h4><span class="category">${escapeHtml(proposal.type)} · ${escapeHtml(proposal.supporters)} sostegni</span></div>
    <p>Revisione: <strong>${escapeHtml(proposal.review_status)}</strong> · Stato: <strong>${escapeHtml(proposal.status)}</strong>${link ? ` · <a href="${escapeHtml(link)}" target="_blank" rel="noopener noreferrer">Apri link</a>` : ""}</p>
    <div class="admin-actions">${reviewActions}${ballotAction}</div>
  </article>`;
}

function renderProposals(proposals) {
  const root = $("#proposal-tools");
  // Conserva i motivi già scritti e il focus quando la lista viene ridisegnata.
  const reasons = new Map($$("[data-reason-for]", root).map((input) => [input.dataset.reasonFor, input.value]));
  const focusedReason = root.contains(document.activeElement) ? document.activeElement.dataset?.reasonFor : undefined;
  root.innerHTML = proposals.length
    ? proposals.map(proposalCard).join("")
    : '<p class="empty-state">Nessuna proposta questa settimana.</p>';
  for (const input of $$("[data-reason-for]", root)) {
    if (reasons.has(input.dataset.reasonFor)) input.value = reasons.get(input.dataset.reasonFor);
    if (input.dataset.reasonFor === focusedReason) input.focus();
  }
}

function renderToolsState(data, { live }) {
  toolsState = data;
  renderProposals(data.proposals);

  const active = data.events.active;
  $("#live-event-status").textContent = active ? `Attivo · ${data.events.channel_name}` : "Non attivo";
  $("#live-event-start").classList.toggle("hidden", active);
  $("#live-event-stop").classList.toggle("hidden", !active);
  if (!(live && liveFormDirty) || active) {
    $("#live-event-title").value = data.events.title || "🎮 Game Night";
    fillChannelSelect($("#live-event-channel"), data.voice_channels, data.events.channel_id, "");
    liveFormDirty = false;
  }
  const isAdmin = data.principal.is_admin;
  $("#event-report-tools").innerHTML = data.events.reports.length
    ? data.events.reports.map((report) => {
      const id = escapeHtml(report.id);
      return `<article class="tool-row">
        <span><strong>#${id} · ${escapeHtml(report.title)}</strong><small>${escapeHtml(report.started_at)} · ${escapeHtml(report.participant_count)} partecipanti, picco ${escapeHtml(report.peak_participants)}</small></span>
        ${isAdmin ? `<button type="button" class="row-action" data-delete-event="${id}" aria-label="Elimina report evento ${id}" title="Elimina report evento">×</button>` : ""}
      </article>`;
    }).join("")
    : '<p class="empty-state">Nessun report evento live.</p>';

  $("#backup-tools").classList.toggle("hidden", !isAdmin);
  if (data.backups) {
    const backups = data.backups;
    $("#backup-status").textContent = `Giornalieri ${backups.daily_count}/14 · ultimo ${backups.latest_daily || "nessuno"} · settimanali ${backups.weekly_count}/8 · ultimo ${backups.latest_weekly || "nessuno"}`;
  }
}

export async function loadToolsState({ live = false } = {}) {
  const turn = nextToolsLoad();
  const view = $("#tools-view");
  view.setAttribute("aria-busy", "true");
  try {
    const data = await apiRequest("/api/site/tools/state");
    if (turn.isCurrent()) renderToolsState(data, { live });
  } finally {
    if (turn.isCurrent()) view.removeAttribute("aria-busy");
  }
}

export function resetTools() {
  toolsState = null;
  liveFormDirty = false;
  $("#tools-view").removeAttribute("aria-busy");
  $("#proposal-tools").innerHTML = '<p class="empty-state">Caricamento…</p>';
  $("#event-report-tools").replaceChildren();
}

function proposalName(id) {
  const proposal = toolsState?.proposals.find((item) => String(item.id) === String(id));
  return proposal ? `#${proposal.id} “${proposal.name}”` : `#${id}`;
}

async function handleProposalClick(event) {
  const review = event.target.closest("[data-proposal-review]");
  const choose = event.target.closest("[data-proposal-choose]");
  const unpick = event.target.closest("[data-proposal-unpick]");
  const button = review || choose || unpick;
  if (!button || button.disabled) return;
  const card = button.closest("[data-proposal-card]");
  const controls = $$("button, input", card);
  const act = (path, options) => runBusy(controls, async () => {
    await apiRequest(path, options);
    await loadToolsState();
    showToast("Proposta aggiornata.");
  });
  try {
    if (review) {
      const id = review.dataset.proposalReview;
      const action = review.dataset.action;
      const reasonInput = card.querySelector(`[data-reason-for="${CSS.escape(id)}"]`);
      const reason = reasonInput?.value.trim() || "";
      if (action === "reject") {
        if (!reason) {
          reasonInput.setCustomValidity("Inserisci il motivo del rifiuto.");
          reasonInput.reportValidity();
          reasonInput.setCustomValidity("");
          return;
        }
        const confirmed = await confirmDialog({
          overline: "RIFIUTA PROPOSTA",
          title: "Rifiutare la proposta?",
          message: `La proposta ${proposalName(id)} verrà rifiutata con il motivo: “${reason}”.`,
          confirmLabel: "Rifiuta proposta",
        });
        if (!confirmed) return;
      }
      await act(`/api/site/tools/proposals/${encodeURIComponent(id)}/review`, {
        method: "POST",
        body: { action, reason },
      });
    } else if (choose) {
      await act(`/api/site/tools/proposals/${encodeURIComponent(choose.dataset.proposalChoose)}/choose`, {
        method: "POST",
        body: {},
      });
    } else {
      const id = unpick.dataset.proposalUnpick;
      const confirmed = await confirmDialog({
        overline: "BALLOTTAGGIO",
        title: "Rimuovere dal ballottaggio?",
        message: `La proposta ${proposalName(id)} non sarà più tra le scelte del ballottaggio.`,
        confirmLabel: "Rimuovi",
      });
      if (!confirmed) return;
      await act(`/api/site/tools/proposals/${encodeURIComponent(id)}/selection`, { method: "DELETE" });
    }
  } catch (error) {
    showError(error);
  }
}

async function handleEventReportClick(event) {
  const button = event.target.closest("[data-delete-event]");
  if (!button || button.disabled) return;
  const id = button.dataset.deleteEvent;
  const confirmed = await confirmDialog({
    overline: "ELIMINA REPORT EVENTO",
    title: "Eliminare il report evento?",
    message: `Il report evento #${id} e i relativi dati di partecipazione verranno eliminati.`,
    confirmLabel: "Elimina report",
  });
  if (!confirmed) return;
  try {
    await runBusy(button, async () => {
      await apiRequest(`/api/site/tools/events/${encodeURIComponent(id)}`, { method: "DELETE" });
      await loadToolsState();
      showToast("Report evento eliminato.");
    });
  } catch (error) {
    showError(error);
  }
}

export function initTools() {
  const refresh = $("#tools-refresh");
  refresh.addEventListener("click", () => runBusy(refresh, () => loadToolsState().catch(showError)));
  $("#proposal-tools").addEventListener("click", handleProposalClick);
  $("#event-report-tools").addEventListener("click", handleEventReportClick);

  const liveForm = $("#live-event-form");
  const markDirty = () => { liveFormDirty = true; };
  liveForm.addEventListener("input", markDirty);
  liveForm.addEventListener("change", markDirty);
  liveForm.addEventListener("submit", (event) => {
    event.preventDefault();
    runBusy(liveForm, async () => {
      try {
        await apiRequest("/api/site/tools/events/start", {
          method: "POST",
          body: {
            title: $("#live-event-title").value.trim(),
            channel_id: $("#live-event-channel").value,
          },
        });
        liveFormDirty = false;
        await loadToolsState();
        showToast("Evento live avviato.");
      } catch (error) {
        showError(error);
      }
    });
  });
  $("#live-event-stop").addEventListener("click", () => runBusy(liveForm, async () => {
    try {
      await apiRequest("/api/site/tools/events/stop", { method: "POST", body: {} });
      await loadToolsState();
      showToast("Evento live terminato.");
    } catch (error) {
      showError(error);
    }
  }));
  $("#create-backup").addEventListener("click", (event) => runBusy(event.currentTarget, async () => {
    try {
      await apiRequest("/api/site/tools/backups", { method: "POST", body: {} });
      await loadToolsState();
      showToast("Backup completato.");
    } catch (error) {
      showError(error);
    }
  }));
}
