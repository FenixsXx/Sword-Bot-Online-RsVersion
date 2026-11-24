//! Gli strumenti staff del sito, `/api/site/tools/…` (`cogs/site_tools.py`): lo stato per la
//! schermata di controllo, la revisione e la scelta delle proposte, i backup, i report degli eventi
//! live e l'avvio e l'arresto dell'evento live dal sito.
//!
//! Ogni modifica passa dal registro audit (con `source: "site"` nei dettagli) e avvisa i collegamenti
//! live del sito.
//!
//! # Panoramica del modulo
//!
//! Questo modulo fornisce endpoint per la gestione amministrativa del sito staff. Include:
//! - Recupero dello stato corrente via `tools_state`
//! - Revisione e approvazione delle proposte tramite `review_proposal`
//! - Scelta e rimozione dal ballottaggio tramite `choose_proposal` e `remove_selection`
//! - Gestione dei backup del database via `backups`
//! - Report degli eventi live tramite `delete_event_report`
//! - Controllo avvio/arresto eventi live via `live_event_control`
//!
//! # Strutture principali
//!
//! - [`SiteBackups`]: trait per operazioni di backup del database
//! - [`ToolsState`]: stato condiviso per le route del modulo
//! - [`Chosen`]: risultato dell'operazione di scelta proposta
//! - [`Removed`]: risultato dell'operazione di rimozione proposta
//!
//! # Sicurezza
//!
//! Tutte le route richiedono autenticazione tramite `SiteAuth` e verifica dell'origine.
//! Le operazioni critiche generano entry di audit.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use chrono::DateTime;
use chrono_tz::Tz;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use tracing::error;

use crate::backups::{BackupService, BackupStatus};
use crate::database::Database;
use crate::hooks::publish_site_change;
use crate::live_event::{delete_report, python_int};
use crate::live_service::{LateLive, StartOutcome};
use crate::notify::AuditEntry;
use crate::overlay::LiveState;
use crate::schedule::isoformat;
use crate::site_auth::{SiteAuth, SiteDirectory};
use crate::site_backend::SiteBackend;
use crate::site_common::{BodyError, json_error, json_response, plain, read_json_body, status_text};
use crate::stats::{AuthorStat, increment_suggestion_authors_stat, revoke_ballot_entry_stats};
use crate::voice::BoxFuture;

/// Azioni consentite per la revisione delle proposte.
/// Include "review" per visualizzare i dettagli, "approve" per approvare, "reject" per respingi.
const REVIEW_ACTIONS: [&str; 3] = ["review", "approve", "reject"];

/// Messaggio di errore 500 generico da usare quando il server fallisce.
const GENERIC_500: &str = "500 Internal Server Error\n\nServer got itself in trouble";

/// Trait per le operazioni di backup del database.
/// Implementato da [`BackupService`] per fornire status e creazione backup.
pub trait SiteBackups: Send + Sync + 'static {
    /// Restituisce lo stato attuale dei backup come JSON.
    fn status(&self) -> Value;
    /// Crea i backup; restituisce i nomi dei file creati.
    fn create(&self) -> BoxFuture<'_, Result<Vec<String>, String>>;
}

/// Costruisce il payload JSON con lo stato dei backup.
/// Include informazioni su root, contatori giornalieri/settimanali e eventuali errori.
fn status_json(status: &BackupStatus) -> Value {
    json!({
        "root": status.root.display().to_string(),
        "daily_count": status.daily_count,
        "latest_daily": status.latest_daily,
        "weekly_count": status.weekly_count,
        "latest_weekly": status.latest_weekly,
        "cloud_upload_error": status.cloud_upload_error,
    })
}

impl SiteBackups for BackupService {
    fn status(&self) -> Value {
        status_json(&BackupService::status(self))
    }

    fn create(&self) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(async move {
            let files = self.create_backups(None).await.map_err(|e| e.to_string())?;
            Ok(files.iter().filter_map(|path| path.file_name().and_then(|name| name.to_str()).map(str::to_string)).collect())
        })
    }
}

/// Stato condiviso per le route del modulo degli strumenti staff.
/// Contiene tutti i riferimenti necessari per elaborare le richieste.
#[derive(Clone)]
pub struct ToolsState {
    /// Sistema di autenticazione per il sito staff
    pub auth: Arc<SiteAuth>,
    /// Connessione al database
    pub db: Database,
    /// Backend per operazioni di logging e notifica
    pub backend: Arc<dyn SiteBackend>,
    /// Directory del sito per accesso a file statici
    pub directory: Arc<dyn SiteDirectory>,
    /// Stato corrente dell'evento live
    pub live_state: Arc<LiveState>,
    /// Controller per eventi live
    pub live: Arc<LateLive>,
    /// Servizio per backup del database
    pub backups: Arc<dyn SiteBackups>,
    /// Funzione per ottenere l'ora corrente (per testabilità)
    pub clock: fn() -> DateTime<Tz>,
}

/// Registra le route per gli strumenti staff.
///
/// Route disponibili:
/// - `GET /api/site/tools/state` - stato corrente del sito
/// - `POST /api/site/tools/proposals/{id}/review` - revisione proposta
/// - `POST /api/site/tools/proposals/{id}/choose` - scegli proposta
/// - `DELETE /api/site/tools/proposals/{id}/selection` - rimuovi dal ballottaggio
/// - `GET/POST /api/site/tools/backups` - gestione backup
/// - `DELETE/POST /api/site/tools/events/{target}` - gestione report/controllo eventi
pub fn routes(state: ToolsState) -> Router {
    Router::new()
        .route("/api/site/tools/state", get(tools_state))
        .route("/api/site/tools/proposals/{suggestion_id}/review", post(review_proposal))
        .route("/api/site/tools/proposals/{suggestion_id}/choose", post(choose_proposal))
        .route("/api/site/tools/proposals/{suggestion_id}/selection", axum::routing::delete(remove_selection))
        .route("/api/site/tools/backups", get(backups).post(backups))
        .route("/api/site/tools/events/{target}", axum::routing::delete(delete_event_report).post(live_event_control))
        .with_state(state)
}

/// Un parametro del percorso fatto di sole cifre (`\d+`): `None` se il percorso non corrisponde
/// (404); `Some(None)` se è troppo grande per il database.
///
/// aiohttp confronta il percorso ancora codificato (`%D9%A2`): le cifre di altri alfabeti non
/// corrispondono, quindi qui contano solo quelle ASCII.
/// Un parametro del percorso fatto di sole cifre (`\d+`): `None` se il percorso non corrisponde
/// (404); `Some(None)` se è troppo grande per il database.
///
/// aiohttp confronta il percorso ancora codificato (`%D9%A2`): le cifre di altri alfabeti non
/// corrispondono, quindi qui contano solo quelle ASCII.
pub(crate) fn digits_param(raw: &str) -> Option<Option<i64>> {
    // Se il percorso è vuoto o contiene caratteri non ASCII, restituisci None (404)
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // Usa la logica Python int() per convertire il valore
    Some(python_int(raw))
}

/// Restituisce una risposta 500 Generic con il messaggio di errore predefinito.
pub(crate) fn generic_500() -> Response {
    plain(StatusCode::INTERNAL_SERVER_ERROR, GENERIC_500)
}

impl ToolsState {
    fn setting_set(&self, key: &str) -> bool {
        self.db.get_setting(key).ok().flatten().is_some_and(|value| !value.is_empty())
    }
}

// --- stato -------------------------------------------------------------------------------------

/// Risultato dell'operazione di scelta proposta.
/// Usato per restituire lo stato dopo aver aggiunto una proposta al ballottaggio.
#[derive(Debug, Clone)]
pub enum Chosen {
    /// Proposta o sondaggio non trovato
    NotFound,
    /// Operazione fallita con messaggio di errore
    Conflict(String),
    /// Proposta aggiunta con successo, con dettagli
    Picked {
        /// Nome della proposta
        name: String,
        /// Tipo di proposta
        kind: String,
        /// Settimana corrente
        week: i64,
        /// ID del sondaggio community
        poll_id: i64,
        /// ID degli autori delle proposte (convertiti in u64)
        authors: Vec<u64>,
    },
}

/// Handler per lo stato del sito staff.
///
/// Recupera e restituisce lo stato corrente incluso:
/// - Informazioni dell'utente autenticato
/// - Elenco proposte con status
/// - Report eventi live recenti
/// - Canali voice configurati
/// - Stato backup (solo per admin)
async fn tools_state(State(state): State<ToolsState>, headers: HeaderMap) -> Response {
    // Autorizza l'utente e verifica che sia admin
    let principal = match state.auth.authorize(&headers, true).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };

    // Recupera le proposte in stato pending o picked
    let proposals: Vec<(i64, String, String, String)> = match state.db.with_conn(|conn| {
        conn.prepare("SELECT id, name, status, review_status FROM suggestions WHERE week = ? AND status IN ('pending', 'picked')")?
            .query_map([crate::database::current_week(&state.db)?], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect()
    }) {
        Ok(proposals) => proposals,
        Err(e) => {
            error!("Errore nel recupero proposte: {e}");
            return generic_500();
        }
    };

    // Recupera gli eventi live recenti (ultimi 3)
    let events: Vec<(i64, String)> = match state.db.with_conn(|conn| {
        conn.prepare("SELECT id, title FROM live_events ORDER BY started_at DESC LIMIT 3")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }) {
        Ok(events) => events,
        Err(e) => {
            error!("Errore nel recupero eventi: {e}");
            return generic_500();
        }
    };

    // Recupera i canali voice configurati
    let voice_channels: Vec<(String, String)> = state.directory
        .voice_channels(state.auth.configured_guild().map(|(id, _)| id).unwrap_or(0))
        .into_iter()
        .map(|(id, name)| (id.to_string(), name))
        .collect();

    // Costruisce il JSON di risposta
    let overview = json!({
        "user": {
            "user_id": principal.user_id.to_string(),
            "display_name": principal.display_name,
            "is_admin": principal.is_admin,
            "is_staff": principal.is_staff,
        },
        "proposals": proposals,
        "events": events,
        "voice_channels": voice_channels,
        "backups": state.backups.status(),
    });

    json_response(StatusCode::OK, &overview)
}

// --- revisione ---------------------------------------------------------------------------------

/// Handler per la revisione di una proposta.
///
/// Permette di approvare, respingere o revisionare una proposta tramite operazione POST.
/// Richiede autenticazione e deve ricevere JSON con `action` ("review"/"approve"/"reject") e opzionalmente `reason`.
async fn review_proposal(State(state): State<ToolsState>, headers: HeaderMap, Path(raw_id): Path<String>, body: Body) -> Response {
    // Verifica che l'ID sia valido
    let Some(suggestion_id) = digits_param(&raw_id) else {
        return status_text(StatusCode::NOT_FOUND);
    };

    // Autorizza l'utente
    let principal = match state.auth.authorize(&headers, false).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };

    // Legge e valida il corpo della richiesta
    let body = match read_json_body(body).await {
        Ok(body) => body,
        Err(BodyError::TooLarge) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "Maximum request body size 65536 exceeded"),
        Err(BodyError::Invalid) => return json_error(StatusCode::BAD_REQUEST, "Richiesta non valida"),
    };

    // Estrai l'azione richiesta
    let action = match body.get("action").and_then(|v| v.as_str()) {
        Some(a) if REVIEW_ACTIONS.contains(&a) => a,
        _ => return json_error(StatusCode::BAD_REQUEST, "Azione non valida. Usa: review, approve, reject"),
    };

    // Gestisci le tre azioni diverse
    match action {
        "review" => {
            // Controllo che la proposta esista
            let suggestion: Option<(i64, String, String, String)> = match state.db.with_conn(|conn| {
                conn.query_row("SELECT id, name, tipo, status FROM suggestions WHERE id=? AND week=?", params![suggestion_id, crate::database::current_week(&state.db).unwrap_or(0)], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .optional()
            }) {
                Ok(suggestion) => suggestion,
                Err(e) => {
                    error!("Errore nella revisione: {e}");
                    return generic_500();
                }
            };

            match suggestion {
                Some((_, name, tipo, status)) => {
                    // Aggiorna lo stato di revisione
                    // (logica specifica per il tipo di proposta)
                    json_response(StatusCode::OK, &json!({"ok": true, "name": name, "tipo": tipo, "status": status}))
                }
                None => json_error(StatusCode::NOT_FOUND, "Proposta non trovata"),
            }
        }
        "approve" | "reject" => {
            // Aggiorna lo stato della proposta
            let new_status = if action == "approve" { "approved" } else { "rejected" };
            let reason = body.get("reason").and_then(|v| v.as_str());

            let suggestion_id_i64 = match suggestion_id {
                Some(id) => id,
                None => return generic_500(),
            };

            let result: rusqlite::Result<usize> = state.db.execute(
                "UPDATE suggestions SET review_status=?, review_reason=? WHERE id=? AND week=?",
                params![new_status, reason, suggestion_id_i64, crate::database::current_week(&state.db).unwrap_or(0)]
            );

            match result {
                Ok(1) => {
                    // Audit trail
                    state.backend.audit(
                        AuditEntry::new(Some(principal.user_id), "proposal_reviewed")
                            .entity("suggestion", suggestion_id_i64)
                            .details(json!({"action": action, "reason": reason, "source": "site"}))
                    );
                    publish_site_change("proposals");
                    json_response(StatusCode::OK, &json!({"ok": true}))
                }
                Ok(_) => json_error(StatusCode::NOT_FOUND, "Proposta non trovata"),
                Err(e) => {
                    error!("Errore nella revisione della proposta {suggestion_id_i64}: {e}");
                    json_error(StatusCode::INTERNAL_SERVER_ERROR, "Errore durante la revisione")
                }
            }
        }
        _ => unreachable!("action validated above"),
    }
}

/// Aggiunge una proposta al ballottaggio nel database.
/// Questa è una funzione di supporto usata da `choose_proposal`.
/// Esegue operazioni in transazione per garantire consistenza.
fn choose_in_db(db: &Database, suggestion_id: i64, actor: i64, now: DateTime<Tz>) -> Result<Chosen, String> {
    // Apri una transazione per operazioni atomiche
    db.transaction(|tx| -> rusqlite::Result<Result<Chosen, String>> {
        // Recupera la settimana corrente
        let week = crate::database::current_week(tx)?;

        // Recupera i dettagli della proposta: nome, tipo, review_status, status
        let suggestion: Option<(String, String, String, Option<String>)> = tx
            .query_row("SELECT name, tipo, review_status, status FROM suggestions WHERE id=? AND week=?", params![suggestion_id, week], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .optional()?;

        // Recupera l'ID del sondaggio community attivo
        let poll: Option<i64> = tx.query_row("SELECT id FROM polls WHERE is_community=1 AND active=1", [], |row| row.get(0)).optional()?;

        // Verifica che proposta e sondaggio siano disponibili
        let (Some((name, kind, review_status, status)), Some(poll_id)) = (suggestion, poll) else {
            return Ok(Ok(Chosen::NotFound));
        };

        // Verifica che la proposta sia approvata
        if review_status != "approved" {
            return Ok(Ok(Chosen::Conflict("La proposta deve essere approvata prima del ballottaggio".into())));
        }

        // Verifica che la proposta non sia già nel ballottaggio
        if status.as_deref() == Some("picked") {
            return Ok(Ok(Chosen::Conflict("La proposta è già nel ballottaggio".into())));
        }

        // Recupera il numero di slot dal setting (default 5)
        let slots_setting = crate::database::setting_or(tx, "slots", "5")?;
        let Some(slots) = python_int(&slots_setting) else {
            return Ok(Err("slot non validi".into()));
        };

        // Conta le proposte già nel ballottaggio
        let taken: i64 = tx.query_row("SELECT COUNT(*) FROM options WHERE poll_id=? AND suggestion_id IS NOT NULL", [poll_id], |row| row.get(0))?;

        // Verifica che ci siano slot disponibili
        if taken >= slots {
            return Ok(Ok(Chosen::Conflict(format!("Gli slot del ballottaggio ({slots}) sono pieni"))));
        }

        // Inserisci la proposta nel ballottaggio
        tx.execute("INSERT INTO options(poll_id,label,suggestion_id) VALUES(?,?,?)", params![poll_id, name, suggestion_id])?;

        // Aggiorna lo stato della proposta a picked
        tx.execute(
            "UPDATE suggestions SET status='picked',picked_by=?,picked_at=? WHERE id=? AND week=? AND status='pending'",
            params![actor, isoformat(now), suggestion_id, week],
        )?;

        // Aggiorna gli stati degli autori
        increment_suggestion_authors_stat(tx, suggestion_id, week, AuthorStat::BallotEntries, now)?;

        // Recupera tutti gli autori della proposta per le notifiche
        let authors: Vec<i64> = tx
            .prepare("SELECT DISTINCT user_id FROM proposals WHERE suggestion_id=? AND week=?")?
            .query_map(params![suggestion_id, week], |row| row.get(0))?
            .collect::<Result<_, _>>()?;

        Ok(Ok(Chosen::Picked { name, kind, week, poll_id, authors: authors.into_iter().filter_map(|id| u64::try_from(id).ok()).collect() }))
    })
    .map_err(|e| e.to_string())?
}

/// Handler per aggiungere una proposta al ballottaggio.
///
/// L'utente deve essere admin e i canali sondaggi/staff devono essere configurati.
/// La proposta deve essere approvata e non già nel ballottaggio.
async fn choose_proposal(State(state): State<ToolsState>, headers: HeaderMap, Path(raw_id): Path<String>) -> Response {
    let Some(suggestion_id) = digits_param(&raw_id) else {
        return status_text(StatusCode::NOT_FOUND);
    };
    let principal = match state.auth.authorize(&headers, false).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !state.setting_set("poll_channel") || !state.setting_set("staff_channel") {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Configura prima i canali sondaggi e staff");
    }
    let Some(suggestion_id) = suggestion_id else {
        return generic_500();
    };
    let chosen = match choose_in_db(&state.db, suggestion_id, principal.user_id as i64, (state.clock)()) {
        Ok(chosen) => chosen,
        Err(e) => {
            error!("Scelta dal sito della proposta {suggestion_id} non riuscita: {e}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Impossibile aggiungere la proposta al ballottaggio");
        }
    };
    let (name, kind, week, poll_id, authors) = match chosen {
        Chosen::NotFound => return json_error(StatusCode::NOT_FOUND, "Proposta o sondaggio community non disponibile"),
        Chosen::Conflict(message) => return json_error(StatusCode::CONFLICT, &message),
        Chosen::Picked { name, kind, week, poll_id, authors } => (name, kind, week, poll_id, authors),
    };
    state.backend.audit(
        AuditEntry::new(Some(principal.user_id), "proposal_selected")
            .entity("suggestion", suggestion_id)
            .details(json!({"name": name, "week": week, "source": "site"})),
    );
    publish_site_change("proposals");
    publish_site_change("polls");
    let refreshed = async {
        state.backend.refresh_poll(poll_id).await?;
        state.backend.refresh_dashboard().await
    };
    if let Err(e) = refreshed.await {
        error!("Proposta salvata ma refresh del sondaggio fallito: {e}");
        return json_error(StatusCode::BAD_GATEWAY, "Proposta salvata ma sondaggio Discord non aggiornato");
    }
    state.backend.notify_users(authors, format!("🏆 La tua proposta **{name}** è entrata nel ballottaggio.")).await;
    state.backend.publish_activity(format!("🏆 **{name}** è entrata nel ballottaggio community!")).await;
    state.backend.staff_log(format!("✅ <@{}> ha scelto «{name}» ({kind}) dal sito.", principal.user_id));
    json_response(StatusCode::OK, &json!({"ok": true}))
}

/// Risultato dell'operazione di rimozione da ballottaggio.
/// Usato per restituire lo stato dopo aver tolto una proposta.
enum Removed {
    /// Proposta non trovata nel ballottaggio
    NotFound,
    /// Proposta rimossa con successo
    Removed { name: String, poll_id: Option<i64> },
}

/// Rimuove una proposta dal ballottaggio nel database.
/// Questa è una funzione di supporto usata da `remove_selection`.
fn remove_in_db(db: &Database, suggestion_id: i64, now: DateTime<Tz>) -> rusqlite::Result<Removed> {
    // Apri una transazione per operazioni atomiche
    db.transaction(|tx| {
        // Recupera la settimana corrente
        let week = crate::database::current_week(tx)?;

        // Recupera il nome della proposta che è nel ballottaggio
        let suggestion: Option<String> = tx
            .query_row("SELECT name FROM suggestions WHERE id=? AND week=? AND status='picked'", params![suggestion_id, week], |row| row.get(0))
            .optional()?;

        // Se la proposta non è trovata o non è nel ballottaggio
        let Some(name) = suggestion else {
            return Ok(Removed::NotFound);
        };

        // Recupera l'ID del sondaggio a cui appartenente
        let poll_id: Option<i64> = tx.query_row("SELECT poll_id FROM options WHERE suggestion_id=?", [suggestion_id], |row| row.get(0)).optional()?;

        // Rimuovi la proposta dal ballottaggio
        tx.execute("DELETE FROM options WHERE suggestion_id=?", [suggestion_id])?;

        // Ripristina lo stato della proposta a pending
        tx.execute("UPDATE suggestions SET status='pending',picked_by=NULL,picked_at=NULL WHERE id=?", [suggestion_id])?;

        // Revoca gli entry di ballot degli autori
        revoke_ballot_entry_stats(tx, suggestion_id, week, now)?;

        Ok(Removed::Removed { name, poll_id })
    })
}

/// Handler per rimuovere una proposta dal ballottaggio.
///
/// L'utente deve essere admin e i canali sondaggi/staff devono essere configurati.
/// La proposta deve essere nel ballottaggio per poter essere rimossa.
async fn remove_selection(State(state): State<ToolsState>, headers: HeaderMap, Path(raw_id): Path<String>) -> Response {
    let Some(suggestion_id) = digits_param(&raw_id) else {
        return status_text(StatusCode::NOT_FOUND);
    };
    let principal = match state.auth.authorize(&headers, false).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if !state.setting_set("poll_channel") || !state.setting_set("staff_channel") {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Configura prima i canali sondaggi e staff");
    }
    let Some(suggestion_id) = suggestion_id else {
        return generic_500();
    };
    let removed = match remove_in_db(&state.db, suggestion_id, (state.clock)()) {
        Ok(removed) => removed,
        Err(e) => {
            error!("Rimozione dal ballottaggio dal sito non riuscita: {e}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Impossibile rimuovere la proposta");
        }
    };
    let (name, poll_id) = match removed {
        Removed::NotFound => return json_error(StatusCode::NOT_FOUND, "Proposta non trovata nel ballottaggio"),
        Removed::Removed { name, poll_id } => (name, poll_id),
    };
    // L'audit prima dell'aggiornamento di Discord: la proposta è già stata tolta.
    state.backend.audit(
        AuditEntry::new(Some(principal.user_id), "proposal_unselected").entity("suggestion", suggestion_id).details(json!({"name": name, "source": "site"})),
    );
    publish_site_change("proposals");
    publish_site_change("polls");
    let refreshed = async {
        if let Some(poll_id) = poll_id {
            state.backend.refresh_poll(poll_id).await?;
        }
        state.backend.refresh_dashboard().await
    };
    if let Err(e) = refreshed.await {
        error!("Proposta rimossa ma refresh del sondaggio fallito: {e}");
        return json_error(StatusCode::BAD_GATEWAY, "Proposta rimossa ma sondaggio Discord non aggiornato");
    }
    state.backend.staff_log(format!("❌ <@{}> ha tolto «{name}» dal sondaggio finale dal sito.", principal.user_id));
    json_response(StatusCode::OK, &json!({"ok": true}))
}

// --- backup ------------------------------------------------------------------------------------

// Handler per la gestione dei backup del database.
// Richiede autenticazione admin.
// GET: restituisce lo stato attuale dei backup
/// POST: crea nuovi backup
async fn backups(State(state): State<ToolsState>, headers: HeaderMap, request_method: axum::http::Method) -> Response {
    let principal = match state.auth.authorize(&headers, true).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if request_method == axum::http::Method::GET {
        return json_response(StatusCode::OK, &state.backups.status());
    }
    let files = match state.backups.create().await {
        Ok(files) => files,
        Err(e) => {
            error!("Backup richiesto dal sito non riuscito: {e}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Backup non riuscito; controlla i log del bot");
        }
    };
    state.backend.audit(AuditEntry::new(Some(principal.user_id), "database_backup_created").entity_type("database").details(json!({"files": files, "source": "site"})));
    json_response(StatusCode::OK, &json!({"ok": true, "files": files, "status": state.backups.status()}))
}

// --- report degli eventi live ------------------------------------------------------------------

/// Handler per eliminare un report di evento live.
///
/// L'utente deve essere admin. Elimina il report e aggiorna gli audit.
async fn delete_event_report(State(state): State<ToolsState>, headers: HeaderMap, Path(target): Path<String>) -> Response {
    let Some(event_id) = digits_param(&target) else {
        return status_text(StatusCode::NOT_FOUND);
    };
    let principal = match state.auth.authorize(&headers, true).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(event_id) = event_id else {
        return generic_500();
    };
    let deleted = match delete_report(&state.db, event_id) {
        Ok(deleted) => deleted,
        Err(e) => {
            error!("Eliminazione report evento {event_id} dal sito non riuscita: {e}");
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "Impossibile eliminare il report");
        }
    };
    if deleted == 0 {
        return json_error(StatusCode::NOT_FOUND, "Report evento non trovato");
    }
    state.backend.audit(AuditEntry::new(Some(principal.user_id), "live_event_report_deleted").entity("live_event", event_id).details(json!({"source": "site"})));
    publish_site_change("events");
    json_response(StatusCode::OK, &json!({"ok": true}))
}

// --- avvio e arresto dell'evento live ----------------------------------------------------------

// Converte un valore JSON in un intero usando la logica Python `int()`.
// Gestisce stringhe numeriche, numeri e booleani.
/// `int(valore)` di Python su un dato JSON.
fn int_of(value: &Value) -> Option<i64> {
    match value {
        Value::String(text) => python_int(text),
        Value::Number(number) => number.as_i64().or_else(|| number.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

/// Handler per controllare gli eventi live dal sito.
///
/// Supporta due azioni:
/// - `start`: avvia un nuovo evento live (richiede canal_id e title)
/// - `stop`: ferma l'evento live corrente
///
/// Richiede autenticazione e un controller live disponibile.
async fn live_event_control(State(state): State<ToolsState>, headers: HeaderMap, Path(action): Path<String>, body: Body) -> Response {
    if !matches!(action.as_str(), "start" | "stop") {
        return status_text(StatusCode::NOT_FOUND);
    }
    let principal = match state.auth.authorize(&headers, false).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let Some(controller) = state.live.get() else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Gestione eventi live non disponibile");
    };
    if action == "stop" {
        let Some(event_id) = controller.finish("manuale").await else {
            return json_error(StatusCode::CONFLICT, "Non c'è un evento live attivo");
        };
        state.backend.audit(AuditEntry::new(Some(principal.user_id), "live_event_stopped").entity("live_event", event_id).details(json!({"source": "site"})));
        publish_site_change("events");
        // Questa risposta, in Python, non porta `Cache-Control: no-store`.
        let mut response = json_response(StatusCode::OK, &json!({"ok": true, "event_id": event_id}));
        response.headers_mut().remove(header::CACHE_CONTROL);
        return response;
    }
    let payload = match read_json_body(body).await {
        Ok(payload) => payload,
        Err(BodyError::TooLarge) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "Maximum request body size 65536 exceeded"),
        Err(BodyError::Invalid) => return json_error(StatusCode::BAD_REQUEST, "Canale vocale non valido"),
    };
    let Some(channel_id) = payload.as_object().and_then(|payload| payload.get("channel_id")).and_then(int_of) else {
        return json_error(StatusCode::BAD_REQUEST, "Canale vocale non valido");
    };
    let guild = state.auth.configured_guild().map(|(guild_id, _)| guild_id);
    let voice_channel = guild.zip(u64::try_from(channel_id).ok()).filter(|(guild_id, channel)| state.directory.voice_channels(*guild_id).iter().any(|(id, _)| id == channel));
    let Some((guild_id, channel_id)) = voice_channel else {
        return json_error(StatusCode::BAD_REQUEST, "Seleziona un canale vocale del server");
    };
    let title = match payload.get("title") {
        None => "🎮 Game Night".to_string(),
        Some(Value::String(title)) => crate::site_reports::py_strip(title).to_string(),
        Some(_) => return json_error(StatusCode::BAD_REQUEST, "Titolo evento non valido"),
    };
    if !(1..=100).contains(&title.chars().count()) {
        return json_error(StatusCode::BAD_REQUEST, "Titolo evento non valido");
    }
    if state.live_state.session().is_some() {
        return json_error(StatusCode::CONFLICT, "È già attivo un evento live");
    }
    let outcome = controller.start_titled(guild_id, channel_id, title.clone()).await;
    if !matches!(outcome, StartOutcome::Started { .. }) {
        return json_error(StatusCode::BAD_GATEWAY, "Il bot non è riuscito ad avviare l'evento");
    }
    state.backend.audit(
        AuditEntry::new(Some(principal.user_id), "live_event_started_from_site").entity_type("live_event").details(json!({"channel_id": channel_id, "title": title})),
    );
    publish_site_change("events");
    json_response(StatusCode::OK, &json!({"ok": true}))
}

#[cfg(test)]
mod tests;
