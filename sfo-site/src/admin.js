/*
 * Questo file gestisce la logica del pannello di amministrazione del sito.
 *
 * Concetti base di JavaScript usati qui:
 * - Import: Importiamo funzioni da altri file (API, gestione DOM, stato) per organizzare il codice.
 * - Funzioni: Blocchi di codice che eseguono compiti specifici (es. 'renderWelcomePreview').
 * - Asincronia (async/await): Poiché il sito deve parlare con il server (il "backend" in Rust),
 *   usiamo 'async' per le funzioni che richiedono tempo, attendendo i dati con 'await'.
 * - Eventi: Il codice reagisce alle azioni dell'utente (click, input, submit sui moduli)
 *   collegando funzioni agli elementi HTML (es. l'invio di un modulo chiama 'submitSettings').
 * - Manipolazione DOM: Cerchiamo elementi HTML nel sito ($("#ID")) e modifichiamo il loro
 *   contenuto o aspetto (es. cambiare testo, attivare checkbox, costruire tabelle).
 */

import { apiRequest } from "./api.js";
import { $, $$, element, uniqueId } from "./dom.js";
import { latestOnly, state } from "./state.js";
import { confirmDialog, runBusy, showError, showToast } from "./ui.js";

const nextAdminLoad = latestOnly();
const ADMIN_FORMS = ["#welcome-config-form", "#poll-settings-form", "#server-config-form"];
// Moduli o sondaggi modificati dall'utente: un aggiornamento live non li sovrascrive.
const dirtyForms = new Set();
const dirtyPolls = new Set();

/* ---------- Anteprima benvenuto ---------- */

function renderWelcomePreview() {
  const root = $("#welcome-preview");
  const serverName = state.adminState?.server_name || "Sword Friends Online";
  const memberCount = Number(state.adminState?.member_count || 1455) + 1;
  const values = {
    "{user}": "Rita",
    "{server}": serverName,
    "{member_count}": String(memberCount),
    "{ordinal}": `${memberCount.toLocaleString("it-IT")}°`,
  };
  const renderTemplate = (value) => Object.entries(values).reduce(
    (result, [token, replacement]) => result.split(token).join(replacement),
    value,
  );
  const header = element("div", { className: "welcome-preview-head" });
  const avatar = element("div", { className: "welcome-preview-avatar", text: "R" });
  const title = element("h4", { className: "welcome-preview-title", text: renderTemplate($("#welcome-title").value) });
  header.append(avatar, title);
  const body = element("div", { className: "welcome-preview-body", text: renderTemplate($("#welcome-body").value) });
  const nodes = [header, body];
  const gifUrl = $("#welcome-gif").value.trim();
  if (gifUrl.startsWith("https://")) {
    const gif = element("img", { className: "welcome-preview-gif" });
    gif.src = gifUrl;
    gif.alt = "GIF del messaggio di benvenuto";
    gif.referrerPolicy = "no-referrer";
    nodes.push(gif);
  }
  nodes.push(element("div", { className: "welcome-preview-footer", text: "Buona permanenza!" }));
  root.replaceChildren(...nodes);
}

/* ---------- Select ---------- */

export function fillChannelSelect(select, channels, selectedId, prefix = "#") {
  const placeholder = element("option", { text: "Seleziona un canale" });
  placeholder.value = "";
  const options = [placeholder];
  for (const channel of channels) {
    const option = element("option", { text: `${prefix}${channel.name}` });
    option.value = channel.id;
    options.push(option);
  }
  select.replaceChildren(...options);
  select.value = selectedId || "";
}

function fillRoleSelect(select, roles, selectedIds) {
  const selected = new Set(selectedIds);
  select.replaceChildren(...roles.map((role) => {
    const option = element("option", { text: `@${role.name}` });
    option.value = role.id;
    option.selected = selected.has(role.id);
    return option;
  }));
}

/* ---------- Sondaggi ---------- */

function makePollField(labelText, value, className = "") {
  const wrapper = element("div", { className: `field ${className}`.trim() });
  const input = document.createElement("input");
  input.id = uniqueId("poll-field");
  input.value = value ?? "";
  const label = element("label", { text: labelText });
  label.htmlFor = input.id;
  wrapper.append(label, input);
  return { wrapper, input };
}

function makeCheckbox(labelText, checked, dataName) {
  const label = element("label", { className: "checkbox-label" });
  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = Boolean(checked);
  input.dataset[dataName] = "";
  label.append(input, element("span", { text: labelText }));
  return label;
}

function renderPollCard(poll) {
  const pollId = String(poll.id);
  const card = element("article", { className: "poll-card" });
  card.dataset.pollCard = pollId;

  const header = element("div", { className: "poll-card-header" });
  const title = element("h4", { text: `${poll.emoji} ${poll.name}${poll.is_community ? " · Community" : ""}` });
  const headerActions = element("div", { className: "admin-actions" });
  const selectButton = element("button", {
    className: "button button-ghost",
    text: state.selectedPollId === poll.id ? "Selezionato per prova" : "Seleziona",
  });
  selectButton.type = "button";
  selectButton.dataset.pollSelect = pollId;
  selectButton.setAttribute("aria-pressed", String(state.selectedPollId === poll.id));
  const deleteButton = element("button", {
    className: "row-action",
    text: "×",
    attrs: { "aria-label": `Elimina sondaggio ${poll.name}`, title: "Elimina sondaggio" },
  });
  deleteButton.type = "button";
  deleteButton.dataset.pollDelete = pollId;
  headerActions.append(selectButton, deleteButton);
  header.append(title, headerActions);

  const editGrid = element("div", { className: "poll-edit-grid" });
  const nameField = makePollField("Nome", poll.name, "poll-name-field");
  nameField.input.maxLength = 100;
  nameField.input.dataset.pollName = "";
  const emojiField = makePollField("Emoji", poll.emoji, "poll-emoji-field");
  emojiField.input.maxLength = 32;
  emojiField.input.dataset.pollEmoji = "";
  const positionField = makePollField("Ordine", poll.position, "poll-position-field");
  positionField.input.type = "number";
  positionField.input.min = "0";
  positionField.input.max = "999";
  positionField.input.dataset.pollPosition = "";
  const colorField = makePollField("Colore", "", "poll-color-field");
  colorField.input.type = "color";
  colorField.input.value = /^#[0-9a-f]{6}$/i.test(poll.color) ? poll.color : "#5865f2";
  colorField.input.dataset.pollColor = "";
  const saveButton = element("button", { className: "button button-primary", text: "Salva" });
  saveButton.type = "button";
  saveButton.dataset.pollSave = pollId;
  editGrid.append(
    nameField.wrapper,
    emojiField.wrapper,
    positionField.wrapper,
    colorField.wrapper,
    makeCheckbox("Community", poll.is_community, "pollCommunity"),
    makeCheckbox("Attivo", poll.active, "pollActive"),
    saveButton,
  );

  const optionsTitle = element("strong", { text: `Opzioni (${poll.options.length}/25)` });
  const options = element("div", { className: "poll-options" });
  for (const option of poll.options) {
    const row = element("div", { className: "poll-option-row" });
    const label = element("span", { text: `${option.label} · ${option.votes} voti` });
    const remove = element("button", {
      className: "row-action",
      text: "×",
      attrs: { "aria-label": `Rimuovi opzione ${option.label}`, title: "Rimuovi opzione" },
    });
    remove.type = "button";
    remove.dataset.optionDelete = String(option.id);
    remove.dataset.pollId = pollId;
    remove.dataset.optionLabel = option.label;
    row.append(label, remove);
    options.append(row);
  }

  const addForm = element("form", { className: "poll-option-add" });
  addForm.dataset.optionAdd = pollId;
  const addInput = document.createElement("input");
  addInput.id = uniqueId("poll-option-new");
  addInput.maxLength = 80;
  addInput.required = true;
  addInput.placeholder = "Nuova opzione";
  const addLabel = element("label", { className: "visually-hidden", text: `Nuova opzione per ${poll.name}` });
  addLabel.htmlFor = addInput.id;
  const addButton = element("button", { className: "button button-ghost", text: "Aggiungi" });
  addButton.type = "submit";
  addForm.append(addLabel, addInput, addButton);
  card.append(header, editGrid, optionsTitle, options, addForm);
  return card;
}

function renderPollList(polls) {
  dirtyPolls.clear();
  const cards = polls.map(renderPollCard);
  if (!cards.length) cards.push(element("p", { className: "empty-state", text: "Nessun sondaggio configurato." }));
  $("#poll-list").replaceChildren(...cards);
}

function renderAdminSkeleton() {
  const skeletonCard = (sizes, property) => {
    const card = element("div", { className: "skeleton-card", attrs: { "aria-hidden": "true" } });
    for (const size of sizes) {
      const line = element("span", { className: "skeleton-line" });
      line.style[property] = size;
      card.append(line);
    }
    return card;
  };
  $("#poll-list").replaceChildren(...Array.from({ length: 3 }, () => skeletonCard(["55%", "90%", "72%", "38%"], "width")));
  $("#welcome-preview").replaceChildren(skeletonCard(["18px", "70px", "180px"], "height"));
}

/* ---------- Stato ---------- */

function applyAdminState(data, { live }) {
  state.adminState = data;
  const isAdmin = data.principal.is_admin;
  $("#admin-only-notice").classList.toggle("hidden", isAdmin);
  $("#manage-view .admin-tabs").classList.toggle("hidden", !isAdmin);
  if (!isAdmin) {
    $$("[role=tabpanel]", $("#manage-view")).forEach((panel) => panel.classList.add("hidden"));
  } else {
    selectTab($("#manage-view [role=tab][aria-selected=true]") || $("#manage-view [role=tab]"));
  }
  const keep = (selector) => live && dirtyForms.has(selector);
  const { settings, channels } = data;
  if (!keep("#welcome-config-form")) {
    fillChannelSelect($("#welcome-channel"), channels, settings.channel_id);
    $("#welcome-enabled").checked = settings.enabled;
    $("#welcome-title").value = settings.title;
    $("#welcome-body").value = settings.body;
    $("#welcome-gif").value = settings.gif_url;
    dirtyForms.delete("#welcome-config-form");
  }
  if (!keep("#poll-settings-form")) {
    fillChannelSelect($("#poll-channel"), channels, settings.poll_channel_id);
    $("#poll-slots").value = String(settings.slots);
    dirtyForms.delete("#poll-settings-form");
  }
  if (!keep("#server-config-form")) {
    fillChannelSelect($("#staff-channel"), channels, settings.staff_channel_id);
    fillChannelSelect($("#event-report-channel"), channels, settings.event_report_channel_id);
    fillChannelSelect($("#activity-channel"), channels, settings.activity_channel_id);
    fillRoleSelect($("#staff-roles"), data.roles, settings.staff_role_ids);
    $("#activity-feed-enabled").checked = settings.activity_feed_enabled;
    $("#proposal-types").value = settings.tipi;
    dirtyForms.delete("#server-config-form");
  }
  if (!data.polls.some((poll) => poll.id === state.selectedPollId)) {
    state.selectedPollId = data.polls[0]?.id || null;
  }
  const pollList = $("#poll-list");
  const typing = pollList.contains(document.activeElement) && document.activeElement.matches("input, textarea, select");
  const editingPolls = live && (dirtyPolls.size > 0 || typing);
  if (!editingPolls) renderPollList(data.polls);
  renderWelcomePreview();
}

/**
 * Carica la configurazione. Le risposte superate da una richiesta più recente
 * vengono ignorate. Con `live` non sovrascrive i moduli in modifica.
 */
export async function loadAdminState({ live = false } = {}) {
  const turn = nextAdminLoad();
  const view = $("#manage-view");
  view.setAttribute("aria-busy", "true");
  if (!state.adminState) renderAdminSkeleton();
  try {
    const data = await apiRequest("/api/site/admin/state");
    if (turn.isCurrent()) applyAdminState(data, { live });
  } finally {
    if (turn.isCurrent()) view.removeAttribute("aria-busy");
  }
}

export function resetAdmin() {
  dirtyForms.clear();
  dirtyPolls.clear();
  $("#manage-view").removeAttribute("aria-busy");
  $("#poll-list").replaceChildren();
  $("#welcome-preview").replaceChildren();
}

/* ---------- Schede ---------- */

function selectTab(tab, { focus = false } = {}) {
  if (!tab) return;
  for (const item of $$("#manage-view [role=tab]")) {
    const selected = item === tab;
    item.classList.toggle("active", selected);
    item.setAttribute("aria-selected", String(selected));
    item.tabIndex = selected ? 0 : -1;
    $(`#${item.getAttribute("aria-controls")}`).classList.toggle("hidden", !selected);
  }
  if (focus) tab.focus();
}

function initTabs() {
  const tabs = $$("#manage-view [role=tab]");
  tabs.forEach((tab) => tab.addEventListener("click", () => selectTab(tab)));
  $("#manage-view [role=tablist]").addEventListener("keydown", (event) => {
    const index = tabs.indexOf(document.activeElement);
    if (index === -1) return;
    const targets = {
      ArrowRight: tabs[(index + 1) % tabs.length],
      ArrowLeft: tabs[(index - 1 + tabs.length) % tabs.length],
      Home: tabs[0],
      End: tabs.at(-1),
    };
    const target = targets[event.key];
    if (!target) return;
    event.preventDefault();
    selectTab(target, { focus: true });
  });
}

/* ---------- Azioni ---------- */

async function submitSettings(form, body, message) {
  await runBusy(form, async () => {
    try {
      await apiRequest("/api/site/admin/settings", { method: "PUT", body });
      dirtyForms.delete(`#${form.id}`);
      await loadAdminState();
      showToast(message);
    } catch (error) {
      showError(error);
    }
  });
}

function pollPayload(card) {
  return {
    name: card.querySelector("[data-poll-name]").value.trim(),
    emoji: card.querySelector("[data-poll-emoji]").value.trim(),
    position: Number(card.querySelector("[data-poll-position]").value),
    color: card.querySelector("[data-poll-color]").value,
    is_community: card.querySelector("[data-poll-community]").checked,
    active: card.querySelector("[data-poll-active]").checked,
  };
}

async function handlePollListClick(event) {
  const select = event.target.closest("[data-poll-select]");
  if (select) {
    state.selectedPollId = Number(select.dataset.pollSelect);
    for (const button of $$("[data-poll-select]", $("#poll-list"))) {
      const selected = Number(button.dataset.pollSelect) === state.selectedPollId;
      button.textContent = selected ? "Selezionato per prova" : "Seleziona";
      button.setAttribute("aria-pressed", String(selected));
    }
    return;
  }
  const save = event.target.closest("[data-poll-save]");
  const remove = event.target.closest("[data-poll-delete]");
  const removeOption = event.target.closest("[data-option-delete]");
  try {
    if (save) {
      const card = save.closest("[data-poll-card]");
      await runBusy(save, async () => {
        await apiRequest(`/api/site/admin/polls/${encodeURIComponent(save.dataset.pollSave)}`, {
          method: "PUT",
          body: pollPayload(card),
        });
        dirtyPolls.delete(save.dataset.pollSave);
        await loadAdminState();
        showToast("Sondaggio aggiornato.");
      });
    } else if (remove) {
      const pollId = Number(remove.dataset.pollDelete);
      const poll = state.adminState?.polls.find((item) => item.id === pollId);
      if (!poll) return;
      const confirmed = await confirmDialog({
        overline: "ELIMINA SONDAGGIO",
        title: "Eliminare il sondaggio?",
        message: `Il sondaggio “${poll.name}” e tutte le sue opzioni verranno eliminati.`,
        confirmLabel: "Elimina sondaggio",
      });
      if (!confirmed) return;
      await runBusy(remove, async () => {
        await apiRequest(`/api/site/admin/polls/${encodeURIComponent(pollId)}`, { method: "DELETE" });
        if (state.selectedPollId === pollId) state.selectedPollId = null;
        await loadAdminState();
        showToast("Sondaggio eliminato.");
      });
    } else if (removeOption) {
      const confirmed = await confirmDialog({
        overline: "RIMUOVI OPZIONE",
        title: "Rimuovere l'opzione?",
        message: `L'opzione “${removeOption.dataset.optionLabel}” e i suoi voti verranno rimossi dal sondaggio.`,
        confirmLabel: "Rimuovi opzione",
      });
      if (!confirmed) return;
      await runBusy(removeOption, async () => {
        const { pollId, optionDelete } = removeOption.dataset;
        await apiRequest(`/api/site/admin/polls/${encodeURIComponent(pollId)}/options/${encodeURIComponent(optionDelete)}`, { method: "DELETE" });
        await loadAdminState();
        showToast("Opzione rimossa.");
      });
    }
  } catch (error) {
    showError(error);
  }
}

async function handleOptionAdd(event) {
  const form = event.target.closest("[data-option-add]");
  if (!form) return;
  event.preventDefault();
  const input = form.querySelector("input");
  await runBusy(form, async () => {
    try {
      await apiRequest(`/api/site/admin/polls/${encodeURIComponent(form.dataset.optionAdd)}/options`, {
        method: "POST",
        body: { label: input.value.trim() },
      });
      dirtyPolls.delete(form.dataset.optionAdd);
      await loadAdminState();
      showToast("Opzione aggiunta e sondaggio aggiornato.");
    } catch (error) {
      showError(error);
    }
  });
}

export function initAdmin() {
  initTabs();
  for (const selector of ADMIN_FORMS) {
    const markDirty = () => dirtyForms.add(selector);
    $(selector).addEventListener("input", markDirty);
    $(selector).addEventListener("change", markDirty);
  }
  const pollList = $("#poll-list");
  const markPollDirty = (event) => {
    const card = event.target.closest("[data-poll-card]");
    if (card) dirtyPolls.add(card.dataset.pollCard);
  };
  pollList.addEventListener("input", markPollDirty);
  pollList.addEventListener("change", markPollDirty);
  pollList.addEventListener("click", handlePollListClick);
  pollList.addEventListener("submit", handleOptionAdd);

  $("#welcome-config-form").addEventListener("submit", (event) => {
    event.preventDefault();
    submitSettings(event.currentTarget, {
      welcome_channel_id: $("#welcome-channel").value,
      welcome_enabled: $("#welcome-enabled").checked,
      welcome_title: $("#welcome-title").value,
      welcome_body: $("#welcome-body").value,
      welcome_gif: $("#welcome-gif").value.trim(),
    }, "Configurazione benvenuto salvata.");
  });
  $("#poll-settings-form").addEventListener("submit", (event) => {
    event.preventDefault();
    submitSettings(event.currentTarget, {
      poll_channel_id: $("#poll-channel").value,
      slots: Number($("#poll-slots").value),
    }, "Destinazione e slot salvati.");
  });
  $("#server-config-form").addEventListener("submit", (event) => {
    event.preventDefault();
    submitSettings(event.currentTarget, {
      staff_channel_id: $("#staff-channel").value,
      event_report_channel_id: $("#event-report-channel").value,
      activity_channel_id: $("#activity-channel").value,
      activity_feed_enabled: $("#activity-feed-enabled").checked,
      staff_role_ids: [...$("#staff-roles").selectedOptions].map((option) => option.value),
      tipi: $("#proposal-types").value,
    }, "Canali, ruoli e tipi di proposta salvati.");
  });

  $("#welcome-test").addEventListener("click", (event) => runBusy(event.currentTarget, async () => {
    try {
      await apiRequest("/api/site/admin/welcome/test", {
        method: "POST",
        body: {
          welcome_channel_id: $("#welcome-channel").value,
          welcome_title: $("#welcome-title").value,
          welcome_body: $("#welcome-body").value,
          welcome_gif: $("#welcome-gif").value.trim(),
        },
      });
      showToast("Prova inviata nel canale selezionato.");
    } catch (error) {
      showError(error);
    }
  }));
  ["#welcome-title", "#welcome-body", "#welcome-gif"].forEach((selector) => {
    $(selector).addEventListener("input", renderWelcomePreview);
  });

  $("#poll-create-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    runBusy(form, async () => {
      try {
        const options = $("#new-poll-options").value.split("\n").map((label) => label.trim()).filter(Boolean);
        const created = await apiRequest("/api/site/admin/polls", {
          method: "POST",
          body: {
            name: $("#new-poll-name").value.trim(),
            emoji: $("#new-poll-emoji").value.trim(),
            color: $("#new-poll-color").value,
            is_community: $("#new-poll-community").checked,
            options,
          },
        });
        state.selectedPollId = created.id;
        form.reset();
        $("#new-poll-emoji").value = "🎮";
        $("#new-poll-color").value = "#5865f2";
        await loadAdminState();
        showToast("Sondaggio creato e pubblicato.");
      } catch (error) {
        showError(error);
      }
    });
  });

  $("#polls-test").addEventListener("click", (event) => {
    if (!state.selectedPollId) {
      showToast("Seleziona prima un sondaggio.");
      return;
    }
    runBusy(event.currentTarget, async () => {
      try {
        await apiRequest("/api/site/admin/polls/test", { method: "POST", body: { poll_id: state.selectedPollId } });
        showToast("Anteprima inviata per 60 secondi nel canale sondaggi.");
      } catch (error) {
        showError(error);
      }
    });
  });
  $("#polls-publish").addEventListener("click", (event) => runBusy(event.currentTarget, async () => {
    try {
      await apiRequest("/api/site/admin/polls/publish", { method: "POST", body: {} });
      showToast("Sondaggi attivi pubblicati o aggiornati nel canale.");
    } catch (error) {
      showError(error);
    }
  }));
}
