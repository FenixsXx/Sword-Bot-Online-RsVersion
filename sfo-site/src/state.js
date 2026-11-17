/** Stato condiviso in memoria. Viene azzerato al logout o alla scadenza della sessione. */
export const state = {
  session: null,
  currentView: "overview",
  reports: [],
  weekOffset: 0,
  editingReportId: null,
  adminState: null,
  selectedPollId: null,
};

let sessionEpoch = 0;

export function resetState() {
  // Le risposte di richieste avviate prima del reset verranno ignorate.
  sessionEpoch += 1;
  state.session = null;
  state.currentView = "overview";
  state.reports = [];
  state.weekOffset = 0;
  state.editingReportId = null;
  state.adminState = null;
  state.selectedPollId = null;
}

/**
 * Restituisce una funzione che, a ogni chiamata, apre un nuovo "turno":
 * `isCurrent()` resta vero solo per l'ultima richiesta avviata (e solo finché
 * la sessione non viene azzerata), così le risposte in ritardo sono ignorate.
 */
export function latestOnly() {
  let generation = 0;
  return () => {
    generation += 1;
    const mine = generation;
    const epoch = sessionEpoch;
    return { isCurrent: () => mine === generation && epoch === sessionEpoch };
  };
}
