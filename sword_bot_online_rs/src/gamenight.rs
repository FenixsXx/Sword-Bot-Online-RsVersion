//! Dal vincitore del sondaggio all'evento Discord programmato (equivalente di `sfo/gamenight.py`).
//!
//! Questo modulo gestisce il flusso completo che trasforma il vincitore di un sondaggio
//! settimanale in un vero e proprio "Evento Programmato" (Scheduled Event) di Discord,
//! nel canale vocale dedicato alla Game Night.
//!
//! FLUSSO PRINCIPALE:
//! 1. Dopo il reset settimanale, il sistema calcola il vincitore del sondaggio.
//! 2. Viene inviato un messaggio nel canale staff con un pulsante "📅 Crea evento".
//! 3. Quando lo staff clicca il pulsante, si apre una maschera (Modal) che chiede
//!    data/ora di inizio e una descrizione facoltativa.
//! 4. Alla conferma, il bot crea l'evento Discord nel canale vocale Game Night.
//! 5. L'evento viene automaticamente aggiunto all'elenco per il "tracciamento live"
//!    (stesso meccanismo usato dal comando `/evento programma`).
//!
//! DIFFERENZA CHIAVE RISPETTO A PYTHON:
//! In Python la vista (View) mantiene in memoria il vincitore. Qui, poiché il bot
//! può riavviarsi, il pulsante e la maschera portano la settimana nel `custom_id`
//! (es. `gamenight:create:7`) e il vincitore viene riletto dal database
//! dalla chiave `week_winner:<settimana>`. Questo garantisce che il pulsante
//! funzioni anche dopo un riavvio del bot.
//!
//! MODULI COLLEGATI:
//! - `reset`: calcola il vincitore e chiama `offer_event_creation`.
//! - `interactions`: gestisce il clic sul pulsante e l'invio della maschera,
//!   delegando qui la logica (`create_button_clicked`, `event_modal_submitted`).
//!
//! CONCETTI CHIAVE:
//! - `EventStart`: Incapsula sia l'orario "da muro" (quello scritto dall'utente, es. 21/03/2027 02:30)
//!   che l'istante assoluto (UTC) calcolato nel fuso orario. Fondamentale per gestire
//!   correttamente i cambi di ora legale (vedi commenti su `parse_event_start`).
//! - `StoredWinner`: Il vincitore serializzato nel database (chiave `week_winner:<n>`).
//! - `scheduled_event_optins`: Lista di ID di eventi Discord che hanno il tracciamento live attivo.
//! - `week_event_created:<n>`: Flag che impedisce di creare due eventi per lo stesso vincitore.

#![allow(dead_code)]

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use chrono_tz::Tz;
use poise::serenity_prelude::{ButtonStyle, CreateActionRow, CreateButton, CreateInputText, CreateModal, InputTextStyle, ReactionType};
use serde_json::{Value, json};
use tracing::{error, warn};

use crate::cards::EmbedSpec;
use crate::config::COLOR_PRESTIGE;
use crate::database::{Database, DbResult};
use crate::discord_api::{ChannelKind, DiscordApi, MessagePayload, NewScheduledEvent};
use crate::flows::Services;
use crate::formatting::{python_json, truncate_chars};
use crate::hooks::publish_site_change;
use crate::interactions::{InteractionIo, IoError, Reply};
use crate::notify::{AuditEntry, DiscordSender};
use crate::schedule::from_wall_clock;
use crate::stats::Winner;

/// Formato di data e ora accettato dalla maschera (modal) per l'inserimento manuale.
/// Corrisponde a `DATE_FORMAT` nel codice Python: giorno/mese/anno ora:minuti (24h).
/// Esempio: "21/11/2026 21:00".
pub const DATE_FORMAT: &str = "%d/%m/%Y %H:%M";

/// Durata standard prevista per una Game Night: 2 ore.
/// Usata per calcolare automaticamente l'orario di fine evento dall'orario di inizio.
const EVENT_DURATION_HOURS: i64 = 2;

/// Prefisso per il `custom_id` del pulsante "Crea evento" inviato allo staff.
/// Formato completo: `gamenight:create:<settimana>`.
pub const CREATE_BUTTON_PREFIX: &str = "gamenight:create:";

/// Prefisso per il `custom_id` della maschera (Modal) di creazione evento.
/// Formato completo: `gamenight:modal:<settimana>`.
pub const EVENT_MODAL_PREFIX: &str = "gamenight:modal:";

/// Nome del campo di input nella maschera per la data/ora di inizio.
pub const MODAL_START_FIELD: &str = "start";

/// Nome del campo di input nella maschera per la descrizione facoltativa.
pub const MODAL_DESCRIPTION_FIELD: &str = "description";

/// Alias per `Result<T, IoError>`: risultato di un'interazione con l'utente Discord.
type Io<T> = Result<T, IoError>;

// --- date ------------------------------------------------------------------------------------

/// Calcola la data/ora predefinita per la prossima Game Night: il venerdì successivo alle 21:00.
///
/// LOGICA (identica a Python):
/// 1. Si lavora sull'"orologio da muro" (wall clock) del fuso orario configurato,
///    NON su UTC. Questo significa che "venerdì alle 21:00" è inteso nel fuso locale.
/// 2. `local.weekday().num_days_from_monday()` restituisce 0=Lunedì ... 6=Domenica.
///    Il venerdì è 4. La formula `(4 - giorno_corrente).rem_euclid(7)` calcola
///    quanti giorni mancano al prossimo venerdì (0 se oggi è venerdì).
/// 3. Se l'orario calcolato è già passato rispetto a `now` (es. sono le 22:00 di venerdì),
///    si aggiungono 7 giorni per andare al venerdì successivo.
/// 4. `from_wall_clock` converte il `NaiveDateTime` locale in un `DateTime<Tz>` consapevole
///    del fuso orario (gestendo automaticamente l'ora legale).
///
/// ARGOMENTI:
/// - `now`: Il momento attuale nel fuso orario del bot (es. Europe/Rome).
///
/// RITORNA:
/// Un `DateTime<Tz>` che rappresenta il prossimo venerdì alle 21:00 (o quello dopo se già passato).
pub fn default_event_start(now: DateTime<Tz>) -> DateTime<Tz> {
    // Parte "naive" (senza fuso) dell'orario corrente nel fuso del bot.
    let local = now.naive_local();
    // Giorni da aggiungere per raggiungere il prossimo venerdì (4 = venerdì, 0 = lunedì).
    let days_ahead = (4 - i64::from(local.weekday().num_days_from_monday())).rem_euclid(7);
    // Orario fisso: 21:00:00.
    let at_nine = NaiveTime::from_hms_opt(21, 0, 0).expect("ora valida");
    // Candidato: data di oggi + giorni avanti, alle 21:00.
    let mut candidate = (local.date() + Duration::days(days_ahead)).and_time(at_nine);
    // Se il candidato è nel passato (o uguale a ora), slitta di una settimana.
    if candidate <= local {
        candidate += Duration::days(7);
    }
    // Converte da "orologio da muro" naive a DateTime consapevole del fuso.
    from_wall_clock(candidate)
}

/// Funzione ausiliaria: legge all'inizio di una stringa fino a `max_digits` cifre ASCII consecutive.
/// Usata dal parser manuale della data per estrarre giorno, mese, anno, ora, minuti.
///
/// RITORNA: `Some((valore_parsato, resto_della_stringa))` oppure `None` se non ci sono cifre.
fn take_number(text: &str, max_digits: usize) -> Option<(u32, &str)> {
    // Conta quante cifre ASCII consecutive ci sono all'inizio (massimo max_digits).
    let digits = text.bytes().take(max_digits).take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    // Converte le cifre trovate in numero e restituisce il resto della stringa.
    Some((text[..digits].parse().ok()?, &text[digits..]))
}

/// Parser manuale per il formato `GG/MM/AAAA HH:MM` (accetta anche cifre singole e spazi multipli).
///
/// MOTIVAZIONE: Non usiamo `NaiveDateTime::parse_from_str` perché:
/// - Python `datetime.strptime` è permissivo: accetta "1/1/2027 9:5" (cifre singole)
///   e spazi multipli tra data e ora. Il parser standard di Rust è rigido.
/// - Vogliamo replicare esattamente lo stesso comportamento permissivo di Python
///   per compatibilità con gli input che gli utenti potrebbero aver già usato.
///
/// FORMATO ACCETTATO:
/// - Giorno: 1 o 2 cifre (1-31)
/// - Separatore obbligatorio: '/'
/// - Mese: 1 o 2 cifre (1-12)
/// - Separatore obbligatorio: '/'
/// - Anno: ESATTAMENTE 4 cifre
/// - Almeno UNO spazio (o tab, newline)
/// - Ora: 1 o 2 cifre (0-23)
/// - Separatore obbligatorio: ':'
/// - Minuti: 1 o 2 cifre (0-59)
/// - Nient'altro dopo (stringa finita)
///
/// RITORNA: `Some(NaiveDateTime)` se il parsing riesce e i valori sono validi, altrimenti `None`.
fn parse_naive(text: &str) -> Option<NaiveDateTime> {
    // Estrae giorno (max 2 cifre).
    let (day, rest) = take_number(text, 2)?;
    // Deve seguire '/'.
    let rest = rest.strip_prefix('/')?;
    // Estrae mese (max 2 cifre).
    let (month, rest) = take_number(rest, 2)?;
    // Deve seguire '/'.
    let rest = rest.strip_prefix('/')?;
    // Estrae anno: ESATTAMENTE 4 cifre ASCII consecutive.
    let year_digits = rest.bytes().take(4).take_while(u8::is_ascii_digit).count();
    if year_digits != 4 {
        return None;
    }
    let year: i32 = rest[..4].parse().ok()?;
    let rest = &rest[4..];
    // Deve esserci almeno uno spazio/whitespace tra data e ora.
    let after_spaces = rest.trim_start_matches(char::is_whitespace);
    if after_spaces.len() == rest.len() {
        return None; // serve almeno uno spazio
    }
    // Estrae ora (max 2 cifre).
    let (hour, rest) = take_number(after_spaces, 2)?;
    // Deve seguire ':'.
    let rest = rest.strip_prefix(':')?;
    // Estrae minuti (max 2 cifre).
    let (minute, rest) = take_number(rest, 2)?;
    // Validazione finale: niente caratteri extra, range validi per ogni campo.
    if !rest.is_empty()
        || !(1..=31).contains(&day)
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || year < 1
    {
        return None;
    }
    // Costruisce il NaiveDateTime (ancora senza fuso orario).
    NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)
}

/// Struttura che rappresenta l'inizio di un evento, mantenendo DUE rappresentazioni temporali:
///
/// 1. `wall` (NaiveDateTime): L'orario "da muro" (wall clock) esattamente come scritto dall'utente.
///    Esempio: "28/03/2027 02:30" — questa è l'ora che apparirà nei log, nell'audit,
///    nel messaggio di conferma allo staff. È l'orario "umano".
///
/// 2. `instant` (DateTime<Tz>): L'istante assoluto (timestamp UTC) calcolato interpretando
///    `wall` nel fuso orario configurato (es. Europe/Rome). Questo è ciò che Discord
///    riceve realmente per programmare l'evento.
///
/// PERCHÉ SERVONO ENTRAMBI (GESTIONE ORA LEGALE):
/// Nella notte del passaggio all'ora legale (es. 28 marzo 2027 in Italia), l'orario 02:30
/// "non esiste" sull'orologio da muro (si salta dalle 02:00 alle 03:00).
/// - Python (`datetime.strptime` + `astimezone`) mantiene l'orario SCRITTO (02:30)
///   con l'offset DI PRIMA del salto (CET, UTC+1), producendo l'istante 01:30 UTC.
/// - Se rileggessimo l'istante nel fuso, direbbe 03:30 (CEST, UTC+2).
/// - I testi (audit, log staff, messaggi) DEVONO mostrare l'orario scritto (02:30),
///   non quello ricalcolato. Per questo conserviamo `wall` separatamente.
///
/// Questo approccio "wall clock first" garantisce coerenza totale con l'implementazione Python.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventStart {
    /// L'orario da muro (quello scritto dall'utente, es. 2027-03-28 02:30).
    wall: NaiveDateTime,
    /// L'istante assoluto nel fuso del bot (es. 2027-03-28 01:30 UTC per Europe/Rome).
    instant: DateTime<Tz>,
}

impl EventStart {
    /// Crea un `EventStart` da un `NaiveDateTime` "da muro", calcolando l'istante nel fuso.
    fn new(wall: NaiveDateTime) -> Self {
        Self { wall, instant: from_wall_clock(wall) }
    }

    /// Restituisce l'istante assoluto (DateTime<Tz>) da passare a Discord per l'evento.
    pub fn instant(&self) -> DateTime<Tz> {
        self.instant
    }

    /// Formato per visualizzazione umana: `GG/MM/AAAA HH:MM` (es. "28/03/2027 02:30").
    /// Usa l'orario DA MURO (`wall`), non quello ricalcolato dal fuso.
    pub fn display(&self) -> String {
        self.wall.format(DATE_FORMAT).to_string()
    }

    /// Formato ISO 8601 stile Python `datetime.isoformat()`: `2027-03-05T21:00:00+01:00`.
    /// Include l'offset calcolato dalla differenza tra `wall` e `instant` in UTC.
    /// Utile per log strutturati e audit trail.
    pub fn isoformat(&self) -> String {
        // Differenza in minuti tra orario da muro e UTC (l'offset del fuso in quel momento).
        let offset = (self.wall - self.instant.naive_utc()).num_minutes();
        let sign = if offset < 0 { '-' } else { '+' };
        // Formatta come YYYY-MM-DDTHH:MM:SS±HH:MM
        format!("{}{sign}{:02}:{:02}", self.wall.format("%Y-%m-%dT%H:%M:%S"), offset.abs() / 60, offset.abs() % 60)
    }

    /// Calcola l'istante di fine evento aggiungendo `hours` ore SULL'OROLOGIO DA MURO.
    /// Equivalente a `start + timedelta(hours=...)` in Python: l'addizione avviene
    /// sul `NaiveDateTime` (wall clock), poi si riconverte nel fuso.
    /// Questo preserva la logica "2 ore dopo sull'orologio da muro" anche attraversando
    /// il cambio d'ora (es. inizio 02:30 inesistente -> fine 04:30 ora legale).
    pub fn end_instant(&self, hours: i64) -> DateTime<Tz> {
        from_wall_clock(self.wall + Duration::hours(hours))
    }
}

/// Valida e converte una stringa di data/ora inserita dallo staff in un `EventStart`.
///
/// REGOLE DI VALIDAZIONE (identiche a Python):
/// 1. Il formato deve essere parsabile da `parse_naive` (GG/MM/AAAA HH:MM con varianti permissive).
/// 2. La data/ora deve essere NEL FUTURO di almeno 5 minuti rispetto a `now`
///    (confrontando gli orologi da muro nello stesso fuso).
/// 3. La data/ora non può essere oltre 60 giorni nel futuro.
///
/// GLI ERRORI RESTITUITI SONO I MESSAGGI MOSTRATI ALL'UTENTE nel modal di Discord.
///
/// ARGOMENTI:
/// - `text`: La stringa inserita dall'utente nel campo "Data e ora" del modal.
/// - `now`: Il momento attuale nel fuso del bot (per il confronto "nel futuro").
///
/// RITORNA: `Ok(EventStart)` se valido, `Err(String)` con messaggio di errore user-friendly.
pub fn parse_event_start(text: &str, now: DateTime<Tz>) -> Result<EventStart, String> {
    // Tenta il parsing permissivo.
    let Some(start) = parse_naive(text.trim()) else {
        return Err("Formato non valido: usa GG/MM/AAAA HH:MM, es. 21/11/2026 21:00.".to_string());
    };
    // Confronta due orari "da muro" nello stesso fuso: `now.naive_local()` vs `start`.
    // Questo replica esattamente il comportamento di Python che confronta due `datetime`
    // nello stesso timezone (wall clock vs wall clock).
    let local_now = now.naive_local();
    if start <= local_now + Duration::minutes(5) {
        return Err("La data deve essere nel futuro (almeno tra 5 minuti).".to_string());
    }
    if start > local_now + Duration::days(60) {
        return Err("La data è troppo lontana: massimo 60 giorni.".to_string());
    }
    Ok(EventStart::new(start))
}

// --- vincitore -------------------------------------------------------------------------------

/// Rappresenta il vincitore di una settimana memorizzato nel database
/// sotto la chiave `week_winner:<numero_settimana>`.
///
/// Viene serializzato come JSON con i campi:
/// - `suggestion_id`: ID della proposta vincitrice (opzionale, per collegamento DB).
/// - `name`: Nome del gioco/proposta (es. "Among Us").
/// - `votes`: Numero di voti ricevuti.
/// - `tie_break`: Motivo dello spareggio se c'è stato pareggio (es. "più sostenitori").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWinner {
    pub suggestion_id: Option<i64>,
    pub name: String,
    pub votes: i64,
    pub tie_break: Option<String>,
}

/// Legge il vincitore memorizzato per una settimana specifica dal database.
///
/// La chiave nel database è `week_winner:<week>` (es. `week_winner:7`).
/// Il valore è una stringa JSON. La lettura è difensiva: se il JSON è malformato,
/// mancano campi obbligatori o il tipo non è oggetto, ritorna `None` invece di fallire.
///
/// ARGOMENTI:
/// - `db`: Connessione al database.
/// - `week`: Numero della settimana (es. 7).
///
/// RITORNA: `Ok(Some(StoredWinner))` se trovato e valido, `Ok(None)` altrimenti.
pub fn winner_for_week(db: &Database, week: i64) -> DbResult<Option<StoredWinner>> {
    // Legge l'impostazione grezza.
    let Some(raw) = db.get_setting(&format!("week_winner:{week}"))?.filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    // Tenta il parsing JSON; se non è un oggetto, ignora.
    let Ok(Value::Object(data)) = serde_json::from_str::<Value>(&raw) else {
        return Ok(None);
    };
    // Estrae i campi obbligatori (name e votes). Se mancano, dato corrotto -> None.
    let (Some(name), Some(votes)) = (data.get("name").and_then(Value::as_str), data.get("votes").and_then(Value::as_i64)) else {
        return Ok(None);
    };
    // Ricostruisce la struttura tipizzata.
    Ok(Some(StoredWinner {
        suggestion_id: data.get("suggestion_id").and_then(Value::as_i64),
        name: name.to_string(),
        votes,
        tie_break: data.get("tie_break").and_then(Value::as_str).map(str::to_string),
    }))
}

/// Memorizza (serializza) il vincitore della settimana nel database.
///
/// Viene chiamato dal reset settimanale dopo aver calcolato il vincitore.
/// La chiave è `week_winner:<week>`. Il valore è JSON compatto (stile Python `json.dumps`
/// senza spazi, ottenuto tramite `python_json`).
///
/// ARGOMENTI:
/// - `db`: Connessione al database.
/// - `week`: Numero della settimana.
/// - `winner`: Struttura `Winner` risultante dal ballottaggio (contiene nome, voti, spareggio).
pub fn remember_winner(db: &Database, week: i64, winner: &Winner) -> DbResult<()> {
    let data = json!({
        "suggestion_id": winner.result.suggestion_id,
        "name": winner.result.name,
        "votes": winner.result.votes,
        "tie_break": winner.tie_break,
    });
    db.set_setting(&format!("week_winner:{week}"), python_json(&data))
}

/// Recupera il nome della proposta collegata a un evento programmato Discord.
///
/// Quando si crea l'evento, salviamo anche `scheduled_event_proposal:<event_id>` = nome proposta.
/// Questo serve per i report audit e per mostrare nel log staff a cosa si riferisce l'evento.
pub fn event_proposal_name(db: &Database, scheduled_event_id: u64) -> DbResult<Option<String>> {
    db.get_setting(&format!("scheduled_event_proposal:{scheduled_event_id}"))
}

/// Aggiunge un evento programmato all'elenco di quelli con "tracciamento live" automatico.
///
/// L'elenco è salvato nell'impostazione `scheduled_event_optins` come array JSON di ID evento (u64).
/// Gli eventi in questa lista vengono monitorati automaticamente per presenza/parlato
/// (stesso meccanismo del comando `/evento programma`).
///
/// LOGICA:
/// 1. Legge l'array esistente (o parte da vuoto se corrotto/assente).
/// 2. Aggiunge il nuovo ID.
/// 3. Ordina e deduplica (così l'elenco resta canonico e pulito).
/// 4. Risalva come JSON compatto.
///
/// Questo pattern "leggi-modifica-scrivi" è sicuro perché le operazioni sul database
/// sono serializzate tramite transazioni SQLite (single-writer).
fn opt_in_live_tracking(db: &Database, scheduled_event_id: u64) -> DbResult<()> {
    let mut values: Vec<u64> = db
        .get_setting_or("scheduled_event_optins", "[]")
        .ok()
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| serde_json::from_str::<Vec<u64>>(&raw).ok())
        .unwrap_or_default();
    values.push(scheduled_event_id);
    values.sort_unstable();
    values.dedup();
    db.set_setting("scheduled_event_optins", python_json(&json!(values)))
}

// --- messaggio per lo staff ------------------------------------------------------------------

/// Costruisce il `custom_id` per il pulsante "Crea evento" di una data settimana.
/// Formato: `gamenight:create:<week>`.
pub fn create_button_id(week: i64) -> String {
    format!("{CREATE_BUTTON_PREFIX}{week}")
}

/// Costruisce il `custom_id` per la maschera (Modal) di una data settimana.
/// Formato: `gamenight:modal:<week>`.
pub fn event_modal_id(week: i64) -> String {
    format!("{EVENT_MODAL_PREFIX}{week}")
}

/// Estrae il numero di settimana da una stringa che dovrebbe contenere SOLO cifre ASCII.
/// Ritorna `None` se la stringa è vuota o contiene non-cifre.
fn parse_week(rest: &str) -> Option<i64> {
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())).then(|| rest.parse().ok()).flatten()
}

/// Decodifica la settimana dal `custom_id` del pulsante "Crea evento".
/// Esempio input: "gamenight:create:7" -> output: Some(7).
pub fn parse_create_button(custom_id: &str) -> Option<i64> {
    parse_week(custom_id.strip_prefix(CREATE_BUTTON_PREFIX)?)
}

/// Decodifica la settimana dal `custom_id` della maschera inviata.
/// Esempio input: "gamenight:modal:7" -> output: Some(7).
pub fn parse_event_modal(custom_id: &str) -> Option<i64> {
    parse_week(custom_id.strip_prefix(EVENT_MODAL_PREFIX)?)
}

/// Crea il pulsante Discord "📅 Crea evento" per una data settimana.
/// Stile: Primary (blu), con emoji calendario.
pub fn create_button(week: i64) -> CreateButton {
    CreateButton::new(create_button_id(week)).label("Crea evento").emoji(ReactionType::Unicode("📅".into())).style(ButtonStyle::Primary)
}

/// Costruisce l'Embed (messaggio grafico Discord) che annuncia il vincitore allo staff
/// e invita a creare l'evento Game Night.
///
/// CONTENUTO:
/// - Titolo: "🏆 Vincitore della settimana #N: <nome>" (tronco a 256 caratteri per sicurezza Discord).
/// - Descrizione: "`X voti`" + eventuale riga "Spareggio: <motivo>." + chiamata all'azione.
/// - Colore: `COLOR_PRESTIGE` (oro/prestigio per i vincitori).
pub fn offer_embed(week: i64, name: &str, votes: i64, tie_break: Option<&str>) -> EmbedSpec {
    let title = truncate_chars(&format!("🏆 Vincitore della settimana #{week}: {name}"), 256);
    let mut description = format!("`{votes} voti`");
    if let Some(reason) = tie_break.filter(|reason| !reason.is_empty()) {
        description.push_str(&format!("\nSpareggio: {reason}."));
    }
    description.push_str("\n\nVuoi programmare la Game Night? Il bot creerà l'evento Discord e avvierà in automatico il tracciamento live.");
    EmbedSpec::new(title, description, COLOR_PRESTIGE)
}

/// Invia allo staff il messaggio con il vincitore e il pulsante "Crea evento".
///
/// FLUSSO:
/// 1. Se non c'è vincitore, esce subito (nessun messaggio).
/// 2. Memorizza SEMPRE il vincitore nel database (`remember_winner`), anche se
///    l'invio del messaggio fallisse o il canale staff non fosse configurato.
///    Questo garantisce che il pulsante funzioni dopo un riavvio.
/// 3. Legge l'impostazione `staff_channel` (ID canale Discord).
/// 4. Se il canale è configurato e valido, invia l'Embed con il pulsante.
/// 5. Logga gli errori ma non propaga l'errore (fire-and-forget per il messaggio staff).
///
/// ARGOMENTI:
/// - `db`: Database per salvare il vincitore e leggere la configurazione.
/// - `api`: Interfaccia per inviare messaggi su Discord.
/// - `week`: Numero della settimana.
/// - `winner`: Opzionale, il vincitore calcolato dal reset.
pub async fn offer_event_creation<A: DiscordApi>(db: &Database, api: &A, week: i64, winner: Option<&Winner>) {
    let Some(winner) = winner else {
        return;
    };
    // SEMPRE ricorda il vincitore: serve per far funzionare il pulsante dopo riavvio.
    if let Err(e) = remember_winner(db, week, winner) {
        error!("Impossibile ricordare il vincitore della settimana {week}: {e}");
    }
    // Legge il canale staff configurato.
    let channel = db.get_setting("staff_channel").ok().flatten().and_then(|raw| raw.trim().parse::<u64>().ok()).filter(|id| *id != 0);
    let Some(channel) = channel else {
        return; // Canale non configurato: il vincitore è salvato, ma niente messaggio.
    };
    // Costruisce embed + pulsante e invia.
    let embed = offer_embed(week, &winner.result.name, winner.result.votes, winner.tie_break).build();
    let payload = MessagePayload::embed(embed).with_components(vec![CreateActionRow::Buttons(vec![create_button(week)])]);
    if let Err(e) = api.send_message(channel, payload).await {
        error!("Impossibile proporre la creazione dell'evento allo staff nel canale {channel}: {e}");
    }
}

// --- maschera --------------------------------------------------------------------------------

/// Crea la maschera (Modal) Discord per chiedere data/ora e descrizione dell'evento.
///
/// CAMPI:
/// 1. "Data e ora (GG/MM/AAAA HH:MM)" - InputText Short, obbligatorio, 16 caratteri
///    (esattamente il formato "GG/MM/AAAA HH:MM"), pre-riempito con il venerdì successivo alle 21:00.
/// 2. "Descrizione (facoltativa)" - InputText Paragraph, opzionale, max 500 caratteri.
///
/// Il `custom_id` della maschera include la settimana: `gamenight:modal:<week>`.
pub fn event_modal(week: i64, now: DateTime<Tz>) -> CreateModal {
    CreateModal::new(event_modal_id(week), "Crea l'evento della Game Night").components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Data e ora (GG/MM/AAAA HH:MM)", MODAL_START_FIELD)
                .min_length(16)
                .max_length(16)
                .value(default_event_start(now).format(DATE_FORMAT).to_string()),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Paragraph, "Descrizione (facoltativa)", MODAL_DESCRIPTION_FIELD)
                .required(false)
                .max_length(500),
        ),
    ])
}

/// Gestisce il clic sul pulsante "Crea evento" da parte dello staff.
///
/// CONTROLLI DI SICUREZZA:
/// 1. Solo lo staff può cliccare (`is_staff` passato dal caller in `interactions`).
/// 2. Un solo evento per vincitore: controlla `week_event_created:<week>` nel database.
///    Se già esiste, risponde "ℹ️ L'evento per questo vincitore è già stato creato.".
/// 3. Verifica che il vincitore esista ancora nel database (`winner_for_week`).
///
/// SE TUTTO OK: Apre la maschera (`event_modal`) per chiedere data/ora.
///
/// ARGOMENTI:
/// - `db`: Database per i controlli.
/// - `io`: Interfaccia per rispondere all'interazione (mostrare modal o messaggio).
/// - `week`: Settimana estratta dal `custom_id` del pulsante.
/// - `is_staff`: Booleano, vero se l'utente ha permessi staff.
/// - `now`: Orario corrente per il valore predefinito del modal.
pub async fn create_button_clicked<I: InteractionIo>(db: &Database, io: &I, week: i64, is_staff: bool, now: DateTime<Tz>) -> Io<()> {
    // Controllo permessi: solo staff.
    if !is_staff {
        return io.respond(Reply::text("⚠️ Solo lo staff può creare l'evento.")).await;
    }
    // Controllo duplicati: un evento per vincitore.
    let already = db.get_setting(&format!("week_event_created:{week}")).ok().flatten().is_some_and(|value| !value.is_empty());
    if already {
        return io.respond(Reply::text("ℹ️ L'evento per questo vincitore è già stato creato.")).await;
    }
    // Verifica che il vincitore esista e apri il modal.
    match winner_for_week(db, week) {
        Ok(Some(_)) => io.show_modal(event_modal(week, now)).await,
        Ok(None) => io.respond(Reply::text("❌ Vincitore non trovato per questa settimana.")).await,
        Err(e) => {
            error!("Impossibile leggere il vincitore della settimana {week}: {e}");
            io.respond(Reply::text("❌ Vincitore non trovato per questa settimana.")).await
        }
    }
}

/// Gestisce l'invio della maschera (Modal) con data/ora e descrizione.
/// Crea l'effettivo Evento Programmato (Scheduled Event) su Discord.
///
/// FLUSSO DETTAGLIATO:
/// 1. Legge e valida il campo "start" (data/ora) con `parse_event_start`.
///    Se invalido, risponde con errore nel modal (ephemeral).
/// 2. Verifica che l'interazione avvenga in un server (guild), non in DM.
/// 3. Rilegge il vincitore dal database (fonte di verità, non memoria).
/// 4. Legge la configurazione `game_night_channel` (canale vocale Game Night).
/// 5. Verifica che il canale esista e sia di tipo Voice o Stage.
/// 6. `io.defer()`: Riconosce l'interazione (Discord richiede risposta entro 3s).
/// 7. Costruisce la descrizione: se lo staff non ne ha data una, ne genera una
///    automatica con nome proposta, voti e settimana.
/// 8. Calcola l'orario di fine: `start + 2 ore` SULL'OROLOGIO DA MURO (vedi `EventStart::end_instant`).
/// 9. Crea l'evento Discord tramite `create_scheduled_event` con:
///    - Nome: "🎮 <nome proposta>" (tronco a 100 char).
///    - Orario inizio/fine: timestamp Unix (secondi) dall'`instant` assoluto.
///    - Canale: ID del canale vocale Game Night.
///    - Tipo: Stage se il canale è Stage, altrimenti Voice.
///    - Descrizione: troncata a 1000 char (limite Discord).
///    - Motivo: "Game Night creata da <utente>".
/// 10. Se creazione fallisce (es. permessi mancanti), risponde con errore nel followup.
/// 11. Se creazione riesce:
///     a. Aggiunge l'evento al tracciamento live (`opt_in_live_tracking`).
///     b. Salva `scheduled_event_proposal:<event_id>` = nome proposta (per audit).
///     c. Salva `week_event_created:<week>` = event_id (blocca duplicati).
///     d. Registra audit entry per lo staff log.
///     e. Pubblica cambiamento sito (`publish_site_change("events")`).
///     f. Risponde al followup con link all'evento e conferma tracciamento live.
///     g. Aggiorna il messaggio originale del vincitore (edit) con chi ha creato l'evento e link.
///     h. Logga nel canale staff testuale.
///
/// GESTIONE ERRORI:
/// - Ogni passo critico ha il suo `match`/`if let Err` per loggare senza far crashare il bot.
/// - Le operazioni di "salvataggio metadati" (11a-11c) sono in catena `and_then`:
///   se una fallisce, l'evento è già stato creato su Discord ma i metadati locali
///   potrebbero essere incompleti; l'errore viene loggato ma non mostrato all'utente.
///
/// ARGOMENTI:
/// - `services`: Contiene `db`, `publisher.api()`, `notifier` (accesso a tutto il necessario).
/// - `io`: Interfaccia per rispondere all'interazione (defer, followup, edit_source).
/// - `week`: Settimana dal `custom_id` della maschera.
/// - `now`: Orario corrente per la validazione "nel futuro".
pub async fn event_modal_submitted<S: DiscordSender, A: DiscordApi, I: InteractionIo>(
    services: &Services<S, A>,
    io: &I,
    week: i64,
    now: DateTime<Tz>,
) -> Io<()> {
    let db = &services.db;
    // 1. Valida data/ora inserita dallo staff.
    let start = match parse_event_start(&io.modal_field(MODAL_START_FIELD).unwrap_or_default(), now) {
        Ok(start) => start,
        Err(message) => return io.respond(Reply::text(format!("❌ {message}"))).await,
    };
    // 2. Deve essere in un server (guild).
    let Some(guild) = io.guild_id() else {
        return io.respond(Reply::text("❌ Usa questo bottone nel server.")).await;
    };
    // 3. Rilegge il vincitore (fonte di verità persistente).
    let winner = match winner_for_week(db, week) {
        Ok(Some(winner)) => winner,
        _ => return io.respond(Reply::text("❌ Vincitore non trovato per questa settimana.")).await,
    };
    // 4-5. Verifica canale Game Night configurato e di tipo Voice/Stage.
    let channel = db.get_setting("game_night_channel").ok().flatten().and_then(|raw| raw.trim().parse::<u64>().ok());
    let kind = match channel {
        Some(id) => services.publisher.api().channel_kind(guild, id).await.unwrap_or_else(|e| {
            warn!("Canale della Game Night {id} non leggibile: {e}");
            ChannelKind::Other
        }),
        None => ChannelKind::Other,
    };
    let (Some(channel), ChannelKind::Voice | ChannelKind::Stage) = (channel, kind) else {
        return io
            .respond(Reply::text("⚠️ Configura prima il canale vocale della Game Night con `/config canale_game_night`."))
            .await;
    };
    // 6. Defer: riconosciamo l'interazione (tempo per creare l'evento).
    io.defer().await?;

    // 7. Descrizione: personalizzata o generata automaticamente.
    let custom = io.modal_field(MODAL_DESCRIPTION_FIELD).unwrap_or_default();
    let description = match custom.trim() {
        "" => format!("La community ha scelto **{}** con {} voti nella settimana #{week}.", winner.name, winner.votes),
        text => text.to_string(),
    };
    // 8. Fine evento: 2 ore dopo sull'orologio da muro.
    let end = start.end_instant(EVENT_DURATION_HOURS);
    // 9. Crea l'evento su Discord.
    let event = NewScheduledEvent {
        name: truncate_chars(&format!("🎮 {}", winner.name), 100),
        start_unix: start.instant().timestamp(),
        end_unix: end.timestamp(),
        channel_id: channel,
        stage: kind == ChannelKind::Stage,
        description: truncate_chars(&description, 1000),
        reason: format!("Game Night creata da {}", io.user_name()),
    };
    let event_id = match services.publisher.api().create_scheduled_event(guild, event).await {
        Ok(id) => id,
        Err(e) => {
            error!("Creazione dell'evento Game Night non riuscita: {e}");
            return io
                .followup(Reply::text("❌ Discord non ha creato l'evento: controlla che il bot abbia il permesso «Gestisci eventi»."))
                .await;
        }
    };
    let url = format!("https://discord.com/events/{guild}/{event_id}");

    // 11. Salva metadati locali (audit, tracciamento live, blocco duplicati).
    if let Err(e) = opt_in_live_tracking(db, event_id)
        .and_then(|()| db.set_setting(&format!("scheduled_event_proposal:{event_id}"), &winner.name))
        .and_then(|()| db.set_setting(&format!("week_event_created:{week}"), event_id))
    {
        error!("Evento {event_id} creato, ma salvarne i dati non è riuscito: {e}");
    }
    // Audit per pannello staff.
    services.notifier.record_staff_audit(
        AuditEntry::new(u64::try_from(io.user_id()).ok(), "game_night_event_created")
            .entity("scheduled_event", event_id)
            .details(json!({"proposal": winner.name, "week": week, "start": start.isoformat()})),
    );
    // Notifica al sito web (dashboard eventi).
    publish_site_change("events");
    // 11f. Risposta followup all'utente che ha premuto il pulsante.
    io.followup(Reply::text(format!("✅ Evento creato: {url}\nIl tracciamento live partirà in automatico."))).await?;

    // 11g. Aggiorna il messaggio originale del vincitore (quello con il pulsante).
    let mention = format!("<@{}>", io.user_id());
    if let Err(e) = io.edit_source_message(format!("📅 Evento creato da {mention}: {url}")).await {
        error!("Impossibile aggiornare il messaggio del vincitore: {e}");
    }
    // 11h. Log testuale nel canale staff.
    services.notifier.staff_log(format!("📅 {mention} ha creato l'evento «{}» per il {}.", winner.name, start.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::discord_api::testing::{Call, FakeApi};
    use crate::schedule::isoformat;
    use crate::stats::BallotResult;
    use chrono::TimeZone;

    /// Helper per creare DateTime nel fuso Rome (usato nei test).
    fn rome(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Tz> {
        TZ.with_ymd_and_hms(year, month, day, hour, minute, 0).unwrap()
    }

    /// Carica i golden test dal file JSON (stessi casi di Python).
    fn golden() -> Value {
        serde_json::from_str(include_str!("golden/gamenight_expected.json")).unwrap()
    }

    /// Converte Result<String, String> in tupla (successo, testo) per confrontare con golden.
    fn outcome(result: &Result<String, String>) -> (bool, &str) {
        match result {
            Ok(text) => (true, text),
            Err(text) => (false, text),
        }
    }

    #[test]
    fn default_start_matches_python() {
        // Verifica che `default_event_start` produca gli stessi risultati di Python
        // per una serie di orari "now" diversi (golden test).
        for case in golden()["defaults"].as_array().unwrap() {
            let now = DateTime::parse_from_rfc3339(case[0].as_str().unwrap()).unwrap().with_timezone(&TZ);
            let start = default_event_start(now);
            assert_eq!(isoformat(start), case[1].as_str().unwrap(), "da {now}");
            assert_eq!(start.format(DATE_FORMAT).to_string(), case[2].as_str().unwrap());
        }
    }

    /// Test completo del parser `parse_event_start`: stessi input, stessi output/errore di Python.
    /// Include forme permesse da `strptime` (cifre singole, spazi multipli) e forme rifiutate.
    #[test]
    fn parsing_matches_python() {
        let golden = golden();
        let now = rome(2027, 2, 28, 18, 0);
        // Casi standard dal golden file.
        for case in golden["parsed"].as_array().unwrap() {
            let text = case[0].as_str().unwrap();
            let actual = parse_event_start(text, now).map(|start| start.isoformat());
            assert_eq!(outcome(&actual), (case[1].as_str().unwrap() == "ok", case[2].as_str().unwrap()), "«{text}»");
        }
        // Casi specifici per il cambio ora legale (DST) - verificano comportamento "wall clock".
        for (label, current, text) in [
            ("dst_sunday_morning", rome(2027, 3, 27, 23, 0), "28/03/2027 03:00"),
            ("dst_sunday_gap", rome(2027, 3, 27, 23, 0), "28/03/2027 02:30"),
            ("dst_5_minutes", rome(2027, 3, 28, 1, 56), "28/03/2027 03:00"),
            ("far_across_dst", rome(2027, 1, 28, 12, 0), "29/03/2027 12:00"),
            ("far_across_dst_edge", rome(2027, 1, 28, 12, 0), "29/03/2027 12:01"),
        ] {
            let expected = &golden[label];
            let actual = parse_event_start(text, current).map(|start| start.isoformat());
            assert_eq!(outcome(&actual), (expected[0].as_str().unwrap() == "ok", expected[1].as_str().unwrap()), "{label}");
        }
    }

    /// Verifica che l'orario "inesistente" (02:30 nella notte del cambio ora legale)
    /// venga mantenuto come scritto dall'utente (wall clock) e non "corretto" al ricalcolo.
    #[test]
    fn nonexistent_time_keeps_the_written_wall_clock() {
        // 28 marzo 2027 in Italia: alle 02:00 si salta alle 03:00 (ora legale).
        // L'utente scrive "28/03/2027 02:30" che sull'orologio da muro non esiste.
        let start = parse_event_start("28/03/2027 02:30", rome(2027, 3, 27, 23, 0)).unwrap();
        // L'ISO deve mostrare l'orario SCRITTO con offset DI PRIMA del salto (CET, +01:00).
        assert_eq!(start.isoformat(), "2027-03-28T02:30:00+01:00");
        // display() deve mostrare l'orario scritto.
        assert_eq!(start.display(), "28/03/2027 02:30");
        // L'istante assoluto (UTC) corrisponde alle 01:30 UTC (02:30 CET = 01:30 UTC).
        assert_eq!(start.instant().naive_utc().to_string(), "2027-03-28 01:30:00");
        // La fine (2 ore dopo sul wall clock: 02:30 + 2h = 04:30 wall clock)
        // 04:30 wall clock in ora legale (CEST, +02:00) = 02:30 UTC.
        assert_eq!(start.end_instant(2).naive_utc().to_string(), "2027-03-28 02:30:00");
    }

    /// Crea un database in memoria con canale staff configurato per i test.
    fn db_with_staff_channel() -> Database {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, rome(2027, 2, 28, 18, 0)).unwrap();
        db.set_setting("staff_channel", 600).unwrap();
        db
    }

    /// Helper per creare un Winner di test.
    fn winner(id: i64, name: &str, votes: i64, tie_break: Option<&'static str>) -> Winner {
        Winner { result: BallotResult { suggestion_id: id, name: name.to_string(), votes, supporters: 0, first_ts: 0 }, tie_break }
    }

    /// Test end-to-end di `offer_event_creation`: verifica messaggio staff, embed, pulsante,
    /// e che il vincitore venga memorizzato correttamente. Confronta con golden Python.
    #[tokio::test]
    async fn offer_matches_the_python_implementation() {
        let golden = golden();
        let db = db_with_staff_channel();
        let api = FakeApi::default();
        let cases: [(i64, Option<Winner>); 4] = [
            (7, Some(winner(1, "Among Us", 3, None))),
            (8, Some(winner(2, "Jackbox Party", 4, Some("più sostenitori")))),
            (9, Some(winner(3, &"N".repeat(300), 12, None))), // nome lunghissimo -> troncamento titolo
            (10, None), // nessun vincitore -> nessun messaggio
        ];
        let mut expected = golden["offers"].as_array().unwrap().iter();
        for (week, winner) in &cases {
            let before = api.calls().len();
            offer_event_creation(&db, &api, *week, winner.as_ref()).await;
            let case = expected.next().unwrap();
            let sent: Vec<Call> = api.calls()[before..].to_vec();
            let wanted = case[1].as_array().unwrap();
            assert_eq!(sent.len(), wanted.len(), "settimana {week}");
            for (call, wanted) in sent.iter().zip(wanted) {
                let Call::Send { channel, message } = call else { panic!("atteso un invio") };
                assert_eq!(json!(channel), wanted["channel"]);
                let message = api.message(*channel, *message).unwrap();
                let embed = &message.embeds[0];
                assert_eq!(embed["title"], wanted["title"]);
                assert_eq!(embed["description"], wanted["description"]);
                assert_eq!(embed["color"], wanted["color"]);
                assert!(embed.get("footer").is_none() && embed.get("timestamp").is_none());
                // Verifica pulsanti: custom_id, label, emoji, style.
                let buttons: Vec<Value> = message
                    .components
                    .iter()
                    .flat_map(|row| row["components"].as_array().unwrap().clone())
                    .map(|b| json!([b["custom_id"], b["label"], b["emoji"]["name"], b["style"]]))
                    .collect();
                assert_eq!(json!(buttons), wanted["buttons"]);
            }
            // Verifica che il vincitore sia stato salvato nel database (chiave week_winner).
            assert_eq!(db.get_setting(&format!("week_winner:{week}")).unwrap().map_or(Value::Null, Value::from), case[2], "vincitore ricordato");
        }
        // Caso extra: senza canale staff il vincitore si ricorda ma non parte messaggio.
        db.execute("DELETE FROM settings WHERE key='staff_channel'", []).unwrap();
        let before = api.calls().len();
        offer_event_creation(&db, &api, 11, Some(&winner(4, "Karaoke", 2, None))).await;
        let case = expected.next().unwrap();
        assert_eq!(api.calls().len(), before, "nessun messaggio senza canale staff");
        assert_eq!(db.get_setting("week_winner:11").unwrap().map(Value::from).unwrap(), case[2], "ma vincitore salvato");
        assert!(expected.next().is_none());
    }

    /// Verifica che `remember_winner` venga chiamato anche se l'invio del messaggio fallisce.
    #[tokio::test]
    async fn offer_survives_a_failing_channel() {
        let db = db_with_staff_channel();
        let api = FakeApi::default();
        api.fail_send(Some(crate::discord_api::ApiError::Forbidden));
        offer_event_creation(&db, &api, 7, Some(&winner(1, "Among Us", 3, None))).await;
        // Il vincitore deve essere salvato comunque.
        assert!(db.get_setting("week_winner:7").unwrap().is_some(), "il vincitore si ricorda comunque");
    }

    /// Test lettura difensiva di `winner_for_week`: gestisce JSON malformati,
    /// campi mancanti, tipi sbagliati senza fare panic.
    #[test]
    fn stored_winner_is_read_defensively() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(winner_for_week(&db, 7).unwrap(), None);
        for garbage in ["", "non json", "[]", "null", "{}", r#"{"name": "X"}"#, r#"{"votes": 1}"#] {
            db.set_setting("week_winner:7", garbage).unwrap();
            assert_eq!(winner_for_week(&db, 7).unwrap(), None, "{garbage}");
        }
        // JSON valido con tutti i campi.
        db.set_setting("week_winner:7", r#"{"suggestion_id": 1, "name": "Among Us", "votes": 3, "tie_break": "x"}"#).unwrap();
        let stored = winner_for_week(&db, 7).unwrap().unwrap();
        assert_eq!((stored.name.as_str(), stored.votes, stored.tie_break.as_deref(), stored.suggestion_id), ("Among Us", 3, Some("x"), Some(1)));
    }

    /// Verifica che la lista `scheduled_event_optins` rimanga ordinata e senza duplicati
    /// anche dopo inserzioni multiple e recupero da JSON corrotto.
    #[test]
    fn live_tracking_list_stays_sorted_and_unique() {
        let db = Database::open_in_memory().unwrap();
        opt_in_live_tracking(&db, 30).unwrap();
        opt_in_live_tracking(&db, 10).unwrap();
        opt_in_live_tracking(&db, 30).unwrap(); // duplicato
        assert_eq!(db.get_setting("scheduled_event_optins").unwrap().as_deref(), Some("[10, 30]"));
        // Se il JSON è corrotto, riparte da zero col nuovo valore.
        db.set_setting("scheduled_event_optins", "rotto").unwrap();
        opt_in_live_tracking(&db, 5).unwrap();
        assert_eq!(db.get_setting("scheduled_event_optins").unwrap().as_deref(), Some("[5]"));
    }
}