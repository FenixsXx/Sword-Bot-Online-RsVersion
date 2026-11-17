import { $, $$ } from "./dom.js";

/* ---------- Tema ---------- */

function applyTheme(theme) {
  const isDark = theme === "dark";
  document.documentElement.dataset.theme = isDark ? "dark" : "light";
  $$("[data-theme-toggle]").forEach((button) => {
    button.setAttribute("aria-pressed", String(isDark));
    button.setAttribute("aria-label", isDark ? "Passa al tema chiaro" : "Passa al tema scuro");
    button.querySelector("[data-theme-label]").textContent = `Tema ${isDark ? "chiaro" : "scuro"}`;
  });
}

export function initTheme() {
  let savedTheme;
  try {
    savedTheme = localStorage.getItem("sfo-theme");
  } catch (error) {
    console.error("Impossibile leggere il tema salvato.", error);
  }
  const prefersDark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  applyTheme(savedTheme || (prefersDark ? "dark" : "light"));
  $$("[data-theme-toggle]").forEach((button) => button.addEventListener("click", () => {
    const theme = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
    applyTheme(theme);
    try {
      localStorage.setItem("sfo-theme", theme);
    } catch (error) {
      console.error("Impossibile salvare il tema selezionato.", error);
    }
  }));
}

/* ---------- Toast ed errori ---------- */

let toastTimer = null;

export function showToast(message, duration = 3000, type = "info") {
  const toast = $("#toast");
  toast.textContent = message;
  toast.className = `toast show ${type}`;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toast.classList.remove("show");
  }, duration);
}

export function hideToast() {
  clearTimeout(toastTimer);
  $("#toast").className = "toast";
}

export function showError(error) {
  // La sessione scaduta ha già riportato al login con il suo messaggio.
  if (error?.sessionExpired) return;
  console.error(error);
  showToast(error?.message || "Operazione non riuscita.", 4000, "error");
}

/* ---------- Operazioni in corso ---------- */

/**
 * Disabilita pulsanti e moduli mentre `task` è in corso. Se uno dei controlli
 * è già occupato, l'operazione non viene ripetuta (doppio clic, invii multipli).
 */
export async function runBusy(targets, task) {
  const list = (Array.isArray(targets) ? targets : [targets]).filter(Boolean);
  const controls = list.flatMap((target) => (target instanceof HTMLFormElement ? [...target.elements] : [target]));
  if (controls.some((control) => control.dataset.busy === "true")) return undefined;
  const focused = document.activeElement;
  const previous = controls.map((control) => control.disabled);
  controls.forEach((control) => {
    control.disabled = true;
    control.dataset.busy = "true";
  });
  list.forEach((target) => target.setAttribute("aria-busy", "true"));
  try {
    return await task();
  } finally {
    controls.forEach((control, index) => {
      control.disabled = previous[index];
      delete control.dataset.busy;
    });
    list.forEach((target) => target.removeAttribute("aria-busy"));
    if (focused instanceof HTMLElement && controls.includes(focused) && focused.isConnected && document.activeElement === document.body) {
      focused.focus();
    }
  }
}

/* ---------- Finestre modali ---------- */

const FOCUSABLE = [
  "a[href]",
  "button:not([disabled])",
  "input:not([disabled]):not([type=hidden])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  "[tabindex]:not([tabindex='-1'])",
].join(",");
const dialogStack = [];

function focusableIn(root) {
  return $$(FOCUSABLE, root).filter((item) => !item.closest(".hidden") && item.getClientRects().length > 0);
}

function setBackgroundInert(value) {
  for (const id of ["app-view", "login-view"]) {
    const root = document.getElementById(id);
    if (root) root.inert = value;
  }
}

export function isDialogOpen(backdrop) {
  return dialogStack.some((entry) => entry.backdrop === backdrop);
}

/** Apre una modale: blocca il focus al suo interno e ricorda chi l'ha aperta. */
export function openDialog(backdrop, { initialFocus = null, onRequestClose = null } = {}) {
  if (isDialogOpen(backdrop)) return;
  const opener = document.activeElement instanceof HTMLElement && document.activeElement !== document.body
    ? document.activeElement
    : null;
  dialogStack.push({ backdrop, opener, onRequestClose });
  backdrop.classList.remove("hidden");
  setBackgroundInert(true);
  (initialFocus || focusableIn(backdrop)[0])?.focus();
}

/** Chiude una modale e riporta il focus sull'elemento che l'aveva aperta. */
export function closeDialog(backdrop) {
  const index = dialogStack.findIndex((entry) => entry.backdrop === backdrop);
  if (index === -1) return;
  const [entry] = dialogStack.splice(index, 1);
  backdrop.classList.add("hidden");
  if (!dialogStack.length) setBackgroundInert(false);
  const opener = entry.opener;
  if (opener?.isConnected && !opener.closest(".hidden")) opener.focus();
}

function requestClose(entry) {
  if (entry.onRequestClose) entry.onRequestClose();
  else closeDialog(entry.backdrop);
}

document.addEventListener("keydown", (event) => {
  const top = dialogStack.at(-1);
  if (!top) return;
  if (event.key === "Escape") {
    event.preventDefault();
    requestClose(top);
    return;
  }
  if (event.key !== "Tab") return;
  const items = focusableIn(top.backdrop);
  if (!items.length) {
    event.preventDefault();
    return;
  }
  const first = items[0];
  const last = items.at(-1);
  if (!top.backdrop.contains(document.activeElement)) {
    event.preventDefault();
    first.focus();
  } else if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
});

document.addEventListener("focusin", (event) => {
  const top = dialogStack.at(-1);
  if (top && !top.backdrop.contains(event.target)) focusableIn(top.backdrop)[0]?.focus();
});

/* ---------- Conferma azioni distruttive ---------- */

let settleConfirm = null;

/** Chiede conferma con la modale del sito. Restituisce una Promise<boolean>. */
export function confirmDialog({ title, message, confirmLabel = "Conferma", overline = "CONFERMA" }) {
  const modal = $("#confirm-modal");
  settleConfirm?.(false);
  $("#confirm-overline").textContent = overline;
  $("#confirm-title").textContent = title;
  $("#confirm-message").textContent = message;
  $("#confirm-accept").textContent = confirmLabel;
  return new Promise((resolve) => {
    const settle = (value) => {
      if (settleConfirm !== settle) return;
      settleConfirm = null;
      closeDialog(modal);
      resolve(value);
    };
    settleConfirm = settle;
    openDialog(modal, {
      initialFocus: $("#confirm-cancel"),
      onRequestClose: () => settle(false),
    });
  });
}

export function initConfirmDialog() {
  const modal = $("#confirm-modal");
  $("#confirm-accept").addEventListener("click", () => settleConfirm?.(true));
  $$("[data-confirm-cancel]", modal).forEach((button) => button.addEventListener("click", () => settleConfirm?.(false)));
  modal.addEventListener("click", (event) => {
    if (event.target === modal) settleConfirm?.(false);
  });
}

/** Chiude ogni modale aperta (logout o sessione scaduta). */
export function closeAllDialogs() {
  settleConfirm?.(false);
  for (const entry of [...dialogStack].reverse()) requestClose(entry);
}
