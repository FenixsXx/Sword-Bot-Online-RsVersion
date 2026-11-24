//! L'evento live: avvio, fine, presenze, ciclo di pubblicazione e ascoltatori degli eventi Discord.
//!
//! Questo modulo gestisce il ciclo di vita di un evento "Live" (es. una sessione di gioco o discussione):
//! - Avvio: Connette il bot al canale vocale, prepara il database e avvia il monitoraggio dell'audio.
//! - Ciclo di vita (tick): Aggiorna costantemente (ogni 250ms) il tempo che ogni utente passa parlando,
//!   gestisce le riconnessioni se il bot cade e salva periodicamente i dati sul database.
//! - Overlay: Pubblica in tempo reale lo stato dell'evento (chi sta parlando, chi è presente) su una pagina web.
//! - Fine: Chiude l'evento, calcola le statistiche finali (chi ha parlato di più, picchi di presenze) e pubblica un report.
//!
//! È il cuore orchestratore: coordina [`crate::voice`] per l'audio, [`crate::live_event`] per la persistenza dati
//! e [`crate::overlay_web`] per la visualizzazione.
//!
//! CONCETTI CHIAVE PER NON PROGRAMMATORI:
//! - **Arc (Atomic Reference Counting)**: Un puntatore intelligente che permette a più parti del codice di
//!   condividere lo stesso dato in modo sicuro. Quando l'ultimo Arc viene distrutto, il dato viene liberato
//!   automaticamente. È come un "contatore di riferimenti atomico" (thread-safe).
//! - **Mutex (Mutual Exclusion)**: Un "lucchetto" che permette a un solo thread alla volta di accedere a un dato.
//!   In Rust, il Mutex "avvolge" il dato e devi fare `lock()` per accedervi; il lock viene rilasciato
//!   automaticamente quando esce dallo scope (RAII).
//! - **BoxFuture**: Un "futuro" (operazione asincrona) messo in un Box (heap allocation). Serve perché
//!   i trait non possono restituire tipi di dimensioni sconosciute; il Box rende la dimensione fissa.
//! - **Trait**: Simile a un'interfaccia in altri linguaggi: definisce un contratto di metodi che un tipo deve implementare.
//! - **async/await**: Rust usa "futures" per le operazioni asincrone. `async fn` restituisce un Future che
//!   viene eseguito solo quando viene "awaitato". Questo permette di fare altre cose mentre si aspetta I/O.
//! - **tokio::sync::Mutex**: Versione asincrona di Mutex per usare `await` dentro la sezione critica.
//! - **Instant vs DateTime**: `Instant` è un orologio monotono (non va indietro, serve per misurare intervalli).
//!   `DateTime<Tz>` è un orologio "a muro" con fuso orario (serve per timestamp umani).
//! - **Voice Gateway/Connection**: Astrazioni per la connessione vocale Discord (tramite songbird). Il gateway
//!   crea le connessioni; la connection gestisce l'audio in entrata/uscita per un canale specifico.
//! - **Overlay (Browser Source OBS)**: Una pagina web che mostra in tempo reale chi sta parlando. Viene
//!   aggiunta come "Browser Source" in OBS Studio per lo streaming.
//! - **Scheduled Event (Evento Programmato)**: Evento Discord nativo (calendario) che può far partire
//!   automaticamente la modalità live quando inizia.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::DateTime;
use chrono_tz::Tz;
use serde_json::json;
use tokio::time::{Instant, MissedTickBehavior, interval};
use tracing::{error, info, warn};

use crate::database::Database;
use crate::discord_api::{ChannelKind, DiscordApi, EventStatus, MessagePayload, ScheduledEventInfo};
use crate::live_event::{self, NewEvent};
use crate::notify::{AuditEntry, DiscordSender, Notifier};
use crate::overlay::{LiveSessionInfo, LiveState, VoiceSource};
use crate::overlay_web;
use crate::publisher::Publisher;
use crate::schedule::isoformat;
use crate::voice::{BoxFuture, VoiceConnection, VoiceError, VoiceGateway};

/// Quanto può restare scollegato il bot prima che l'evento finisca (discord.py si riconnette da solo
/// dopo brevi interruzioni).
///
/// CONCETTO: `Duration` è un tipo della standard library per rappresentare intervalli di tempo.
/// `Duration::from_secs(10)` crea una durata di 10 secondi. È preferibile a numeri "magici"
/// perché rende il codice auto-documentante e evita errori di unità (millisecondi vs secondi).
const VOICE_DISCONNECT_GRACE: Duration = Duration::from_secs(10);
/// Ogni quanto si salva il parlato maturato.
///
/// Salviamo sul database ogni 15 secondi per bilanciare: troppi scritture = lento, troppo poche = rischio perdita dati.
const PERSIST_EVERY: Duration = Duration::from_secs(15);
/// Ogni quanto gira il ciclo.
///
/// 250ms = 4 volte al secondo. Questo è il "tick" del loop principale. Ogni tick aggiorna i contatori
/// di parlato, controlla la connessione e manda aggiornamenti all'overlay web.
const TICK: Duration = Duration::from_millis(250);
/// Dopo quanti errori di fila nel ciclo l'evento viene chiuso.
///
/// 40 errori * 250ms = 10 secondi di errori continui prima di arrendersi. Questo evita che un
/// problema temporaneo (es. lag di rete) chiuda l'evento, ma protegge da loop infiniti rotti.
const MAX_FAILURES: u32 = 40;
/// Il titolo di un evento avviato a mano.
///
/// `&str` è un "string slice": un riferimento a una stringa (spesso un letterale nel codice compilato).
/// È diverso da `String` che è una stringa allocata sull'heap e modificabile.
pub const DEFAULT_TITLE: &str = "🎮 Game Night";

/// Cosa serve per avviare un evento.
///
/// `#[derive(Debug, Clone)]` dice al compilatore di generare automaticamente:
/// - `Debug`: permette di stampare la struct con `{:?}` per il debug
/// - `Clone`: permette di duplicare la struct (copia profonda dei campi)
///
/// `Option<u64>` significa "o un u64, o niente". È il modo Rust di evitare i `null`:
/// devi gestire esplicitamente il caso `None`, prevenendo errori a runtime.
#[derive(Debug, Clone)]
pub struct StartRequest {
    pub guild_id: u64,              // ID univoco del server Discord (chiamato "guild" in Discord API)
    pub channel_id: u64,            // ID del canale vocale dove avviare l'evento
    pub title: String,              // Titolo dell'evento (String = stringa owned, modificabile, sull'heap)
    /// L'evento Discord che lo ha fatto partire (avvio automatico).
    pub scheduled_event_id: Option<u64>,
    /// Chi lo ha avviato a mano; `None` per l'avvio automatico.
    pub actor: Option<u64>,
}

/// Com'è andato un avvio.
///
/// `enum` in Rust è un "tipo somma" (tagged union): può essere UNA delle varianti elencate.
/// A differenza di C/C++, le varianti possono portare dati (es. `Started { event_id, link }`).
/// `PartialEq, Eq` permettono di confrontare due StartOutcome con `==` (derivato automaticamente).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// Partito: l'ID del report e il link per OBS.
    ///
    /// Le parentesi graffe definiscono una variante "struct-like" con campi nominati.
    Started { event_id: i64, link: String },
    /// Lo stesso evento programmato era già in corso: niente da fare.
    AlreadyRunning,
    NotConfigured,
    AlreadyActive,
    NoBotMember,
    NoConnectPermission,
    /// Il collegamento o l'avvio dell'ascolto sono falliti.
    ConnectFailed,
    /// Questa versione del bot non sa ancora collegarsi ai canali vocali.
    VoiceUnavailable,
    ReportFailed,
}

pub const VOICE_UNAVAILABLE: &str = "⚠️ Il collegamento al canale vocale e il rilevamento degli speaker non sono ancora disponibili in questa versione del bot.";

impl StartOutcome {
    /// La risposta da mostrare a chi ha avviato l'evento a mano.
    ///
    /// `match` in Rust è "pattern matching": controlla quale variante dell'enum è e estrae i dati.
    /// È **esaustivo**: il compilatore ti obbliga a gestire TUTTE le varianti, niente casi dimenticati.
    /// `..` ignora i campi rimanenti della variante (qui `event_id` che non serve nel messaggio).
    /// `format!` è una macro (riconoscilibile dal `!`) che crea una String interpolando le variabili.
    /// `.to_string()` converte un `&str` (string slice) in `String` (owned).
    pub fn message(&self, channel_id: u64) -> String {
        match self {
            StartOutcome::Started { link, .. } => format!(
                "✅ Modalità evento live attiva in <#{channel_id}>.\nOBS Browser Source: {link}\n\
                 Il link è una chiave di accesso: condividilo solo con chi deve vedere l'overlay. \
                 L'audio viene analizzato per stimare gli speaker, ma non viene salvato. \
                 Il report con presenza e parlato stimato è consultabile dallo staff \
                 ed eliminabile con `/evento elimina-rapporto`."
            ),
            StartOutcome::NotConfigured => "❌ Overlay non configurato correttamente. Verifica `EVENT_OVERLAY_PUBLIC_URL`, `EVENT_OVERLAY_HOST` e `EVENT_OVERLAY_PORT`; serve un dominio HTTPS con reverse proxy.".to_string(),
            StartOutcome::AlreadyActive | StartOutcome::AlreadyRunning => "⚠️ È già attivo un evento live. Usa `/evento termina` prima di avviarne un altro.".to_string(),
            StartOutcome::NoBotMember => "❌ Non riesco a recuperare il membro del bot in questo server.".to_string(),
            StartOutcome::NoConnectPermission => "❌ Il bot non ha il permesso di connettersi a quel canale vocale.".to_string(),
            StartOutcome::ConnectFailed => "❌ Non sono riuscito a collegarmi o avviare il rilevamento speaker. Controlla permessi, libreria voice-receive e log.".to_string(),
            StartOutcome::VoiceUnavailable => VOICE_UNAVAILABLE.to_string(),
            StartOutcome::ReportFailed => "❌ Non sono riuscito a creare il report evento.".to_string(),
        }
    }
}

/// Quello che i comandi `/evento avvia|collega|termina` chiedono all'evento live.
///
/// `trait` in Rust definisce un **contratto** (simile a un'interfaccia in Java/TypeScript).
/// Chi implementa questo trait deve fornire tutti questi metodi.
///
/// `Send + Sync` sono **marker trait** (trait senza metodi) che dicono al compilatore:
/// - `Send`: il tipo può essere spostato tra thread in sicurezza
/// - `Sync`: il tipo può essere riferito da più thread contemporaneamente (`&T` è Send)
/// Sono necessari perché `LiveService` viene condiviso tra task asincroni (tokio).
///
/// `BoxFuture<'_, StartOutcome>` è un **type alias** per `Pin<Box<dyn Future<Output = StartOutcome> + Send + '_>>`.
/// Serve perché i trait non possono restituire `impl Future` direttamente (dimensione sconosciuta).
/// Il `Box` alloca sul heap e rende la dimensione fissa. Il `Pin` impedisce spostamenti in memoria
/// necessari per i future auto-referenziali. La lifetime `'_` lega il future a `&self`.
pub trait LiveController: Send + Sync {
    /// Avvia un evento a mano (`start_event` con un'interazione).
    fn start(&self, guild_id: u64, channel_id: u64, actor: u64) -> BoxFuture<'_, StartOutcome>;
    /// Termina l'evento in corso e lo registra nel registro audit; `None` se non ce n'è uno.
    fn stop(&self, actor: u64) -> BoxFuture<'_, Option<i64>>;
    /// Il canale vocale standard (non Stage) in cui si trova una persona.
    fn standard_voice_channel_of(&self, guild_id: u64, user_id: u64) -> BoxFuture<'_, Option<u64>>;
    /// Chiude l'evento in corso con il motivo indicato (senza scrivere nell'audit).
    fn finish(&self, reason: &'static str) -> BoxFuture<'_, Option<i64>>;
    /// Avvia un evento con un titolo, come fa il sito (senza interazione: parte come l'avvio automatico).
    fn start_titled(&self, guild_id: u64, channel_id: u64, title: String) -> BoxFuture<'_, StartOutcome>;
}

/// Il controller collegato a bot avviato: il server HTTP parte prima del gateway.
///
/// Pattern di **inizializzazione differita** (lazy initialization): il server HTTP (overlay_web)
/// parte all'avvio del programma, ma il `LiveService` (che implementa `LiveController`) viene
/// creato dopo. `LateLive` permette al server HTTP di "aspettare" il controller.
///
/// `OnceLock` (std::sync::OnceLock) è una cella che può essere scritta **una sola volta**.
/// È thread-safe e non richiede locking per la lettura dopo l'inizializzazione.
/// `Arc<dyn LiveController>`: puntatore intelligente con reference counting a un **trait object**.
/// Il `dyn` indica "dynamic dispatch": il tipo concreto è noto solo a runtime.
/// Questo permette polimorfismo (come `interface` in Go o `base class*` in C++).
#[derive(Default)]
pub struct LateLive {
    inner: std::sync::OnceLock<Arc<dyn LiveController>>,
}

impl LateLive {
    pub fn set(&self, controller: Arc<dyn LiveController>) {
        let _ = self.inner.set(controller);
    }

    /// Il controller, se il bot è già pronto.
    pub fn get(&self) -> Option<&Arc<dyn LiveController>> {
        self.inner.get()
    }
}

/// L'evento in corso (in Python il dizionario `session`).
///
/// Questa struct raccoglie tutto lo stato mutabile di un evento live attivo.
/// È privata al modulo (`struct` senza `pub`) perché è un dettaglio implementativo.
///
/// Nota sui tipi:
/// - `Arc<dyn VoiceConnection>`: trait object per la connessione vocale (polimorfismo runtime)
/// - `Vec<(u64, f64)>`: vettore di tuple (user_id, secondi_parlato). `Vec` è un array dinamico sull'heap.
/// - `Option<Instant>`: `Instant` è un timestamp monotono; `Option` gestisce "ancora non impostato".
struct Session {
    event_id: i64,                              // ID univoco del report evento nel database
    guild_id: u64,                              // Server Discord (guild)
    channel_id: u64,                            // Canale vocale dell'evento
    connection: Arc<dyn VoiceConnection>,       // Connessione vocale condivisa (Arc = reference counted)
    title: String,                              // Titolo dell'evento
    scheduled_event_id: Option<u64>,            // ID evento Discord programmato (se avvio automatico)
    /// Secondi di parlato per utente, nell'ordine in cui sono comparsi.
    speaking: Vec<(u64, f64)>,                  // (user_id, secondi_totali_parlato) - stato corrente
    /// Quanto di `speaking` è già nel database.
    persisted: Vec<(u64, f64)>,                 // Copia di `speaking` all'ultimo salvataggio DB
    audio_recovery_attempts: u32,               // Contatore tentativi riavvio listener audio (backoff esponenziale)
    last_audio_packet_at: Option<Instant>,      // Ultimo pacchetto audio ricevuto (per rilevare silenzio/rottura)
    last_sample: Instant,                       // Ultimo tick del ciclo (per calcolare `elapsed`)
    last_persist: Instant,                      // Ultimo salvataggio su database
    peak: i64,                                  // Picco massimo di utenti contemporanei nell'evento
    disconnected_since: Option<Instant>,        // Da quando il bot è disconnesso (None = connesso)
}

/// Cosa fare dopo aver aggiornato i contatori di un passo del ciclo.
///
/// Enum che rappresenta le **azioni possibili** dopo un tick del ciclo principale.
/// È un "tipo somma" come StartOutcome. Ogni variante porta i dati necessari per l'azione.
///
/// - `Idle`: niente di speciale, continua il loop
/// - `Finish(&'static str)`: chiudi l'evento con il motivo dato (string literal statica)
/// - `Publish { ... }`: manda aggiornamento all'overlay web con i livelli audio correnti
enum Next {
    Idle,
    Finish(&'static str),
    Publish { connection: Arc<dyn VoiceConnection>, levels: HashMap<u64, f64> },
}

pub struct LiveService<S: DiscordSender, A: DiscordApi, G: VoiceGateway> {
    /// Connessione al database SQLite (condivisa, thread-safe tramite connection pool interno).
    db: Database,
    /// Notifiche (audit log, messaggi staff) - generica su `DiscordSender` per testabilità.
    notifier: Notifier<S>,
    /// Publisher per inviare messaggi/embed su Discord - wrappa l'API Discord.
    publisher: Arc<Publisher<A>>,
    /// Gateway vocale: astrazione per creare connessioni voice (songbird).
    gateway: G,
    /// Stato condiviso dell'overlay web (token, sessioni, WebSocket hub).
    live: Arc<LiveState>,
    /// Il server HTTP dell'overlay è in ascolto (`self.runner` in Python).
    overlay_running: bool,
    /// Funzione per ottenere l'ora "a muro" con fuso orario (per timestamp umani, DB).
    /// `fn() -> DateTime<Tz>` è un **function pointer** (puntatore a funzione), non una closure.
    /// Questo permette di iniettare un orologio finto nei test (`with_clocks`).
    clock: fn() -> DateTime<Tz>,
    /// Funzione per ottenere l'istante monotono (per misurare intervalli, timeout).
    monotonic: fn() -> Instant,
    /// Serializza avvio e fine (`_session_lock`).
    /// `tokio::sync::Mutex` è la versione **asincrona** di Mutex: si usa `lock().await`.
    /// Protegge le operazioni che non devono sovrapporsi (es. avvio + fine simultanei).
    session_lock: tokio::sync::Mutex<()>,
    /// Sessione corrente, se c'è. `Mutex<Option<Session>>` = lock + optionalità.
    /// `std::sync::Mutex` (sync) perché l'accesso è veloce (solo lettura/scrittura struct).
    session: Mutex<Option<Session>>,
    /// Eventi programmati in corso di avvio (`_auto_starting_events`).
    /// `HashSet<u64>` = insieme di ID evento per evitare avvii duplicati simultanei.
    auto_starting: Mutex<HashSet<u64>>,
    /// L'ultimo stato noto di ogni evento programmato: gli aggiornamenti di Discord portano solo il
    /// nuovo stato, mentre discord.py passa anche quello precedente.
    /// `HashMap<u64, EventStatus>` mappa event_id -> stato noto.
    event_status: Mutex<HashMap<u64, EventStatus>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Helper per acquisire un lock `std::sync::Mutex` gestendo il "mutex poisoning".
    //
    // In Rust, se un thread panica mentre tiene un lock, il mutex diventa "avvelenato" (poisoned).
    // `lock()` restituisce `Err(PoisonError)` in quel caso. `PoisonError::into_inner()`
    // recupera comunque il dato (il lock viene rilasciato), permettendo di continuare.
    // Questo pattern evita che un panic in un thread blocchi tutto il programma.
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl<S: DiscordSender, A: DiscordApi, G: VoiceGateway> LiveService<S, A, G> {
    pub fn new(
        db: Database,
        notifier: Notifier<S>,
        publisher: Arc<Publisher<A>>,
        gateway: G,
        live: Arc<LiveState>,
        overlay_running: bool,
    ) -> Self {
        // Costruttore che inizializza il service con i componenti dipendenti.
        //
        // `Self` è un alias per `LiveService<S, A, G>`. È equivalente a scrivere il nome completo.
        //
        // I campi inizializzati:
        // - `session_lock`: mutex vuoto `Mutex::new(())` usato come "binary semaphore" per serializzare
        //   operazioni critiche (avvio/fine evento). `()` è il "unit type" di Rust (valore vuoto).
        // - `session`: `Mutex::new(None)` = nessuna sessione attiva all'avvio
        // - `auto_starting`: `HashSet::new()` = insieme vuoto di eventi già in avvio
        // - `event_status`: `HashMap::new()` = mappa vuota stato eventi
        Self {
            db,
            notifier,
            publisher,
            gateway,
            live,
            overlay_running,
            clock: crate::schedule::now,
            monotonic: Instant::now,
            session_lock: tokio::sync::Mutex::new(()),
            session: Mutex::new(None),
            auto_starting: Mutex::new(HashSet::new()),
            event_status: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_clocks(mut self, clock: fn() -> DateTime<Tz>, monotonic: fn() -> Instant) -> Self {
        self.clock = clock;
        self.monotonic = monotonic;
        self
    }

    fn api(&self) -> &A {
        self.publisher.api()
    }

    /// C'è un evento in corso.
    ///
    /// Controlla se `session` contiene `Some(Session)`, cioè se c'è un evento attivo.
    /// `.is_some()` è un metodo su `Option` che restituisce `true` se c'è un valore.
    pub fn is_active(&self) -> bool {
        lock(&self.session).is_some()
    }

    /// Verifica se l'overlay è pronto per l'uso.
    ///
    /// Tre condizioni:
    /// 1. `overlay_running`: il server HTTP dell'overlay è in esecuzione
    /// 2. `public_url.is_empty()`: l'URL è stato configurato (serve HTTPS con reverse proxy)
    /// 3. `configuration_error.is_none()`: non ci sono errori di validazione dell'URL
    fn overlay_ready(&self) -> bool {
        let url = &self.live.public_url;
        self.overlay_running && !url.public_url.is_empty() && url.configuration_error.is_none()
    }

    /// Il link dell'overlay (tema classico) con la chiave attuale.
    ///
    /// Genera un URL sicuro per l'OBS Browser Source. La chiave (token) è un segreto
    /// che permette l'accesso all'overlay e differenzia le sessioni.
    ///
    /// `Option::and_then` è come `flatMap` in altri linguaggi: se il primo Option è `Some`,
    /// chiama la funzione con il valore, altrimenti restituisce `None`.
    /// `unwrap_or_else` recupera il valore o chiama la funzione con il fallback.
    fn overlay_link(&self, guild_id: u64) -> String {
        let link = live_event::overlay_token_with(&self.db, self.live.token_generator)
            .map_err(|e| e.to_string())
            .and_then(|token| live_event::overlay_link(&self.live.public_url.public_url, guild_id, "classic", &token).map_err(|e| e.to_string()));
        link.unwrap_or_else(|e| {
            error!("Impossibile preparare il link dell'overlay: {e}");
            String::new()
        })
    }

    // --- avvio ---------------------------------------------------------------------------------

    /// Avvia un evento live (modalità Game Night) su Discord.
    ///
    /// Flow completo:
    /// 1. Verifica che l'overlay sia configurato correttamente
    /// 2. Prende un lock per serializzare avvio/fine (evita race condition)
    /// 3. Verifica che non ci sia già un evento in corso
    /// 4. Verifica che il bot sia nel server e abbia permessi
    /// 5. Connessione al canale vocale (songbird/WebRTC)
    /// 6. Creazione record nel database
    /// 7. Inizializzazione della sessione e avvio overlay
    /// 8. Notifica allo staff
    ///
    /// Ritorna `StartOutcome` che indica esito e messaggio di stato.
    pub async fn start_event(&self, request: StartRequest) -> StartOutcome {
        let StartRequest { guild_id, channel_id, title, scheduled_event_id, actor } = request;
        // Step 1: Verifica overlay configurato
        if !self.overlay_ready() {
            error!("Overlay non configurato correttamente: l'evento live non può partire.");
            return StartOutcome::NotConfigured;
        }
        // Step 2: Lock per evitare concorrenza (RAII: il lock si rilascia automaticamente all'uscite)
        let _guard = self.session_lock.lock().await;
        // Step 3: Controlla se c'è già un evento
        if let Some(active) = lock(&self.session).as_ref() {
            // Lo stesso evento programmato già in corso non è un errore.
            if scheduled_event_id.is_some() && active.scheduled_event_id == scheduled_event_id {
                return StartOutcome::AlreadyRunning;
            }
            warn!("Evento {title} ignorato: è già attivo un altro evento.");
            return StartOutcome::AlreadyActive;
        }
        // Step 4: Verifica bot nel guild con permessi
        if !self.gateway.bot_in_guild(guild_id) {
            error!("Non riesco a recuperare il membro del bot nel server {guild_id}.");
            return StartOutcome::NoBotMember;
        }
        let Some(channel) = self.gateway.channel_info(guild_id, channel_id).filter(|channel| channel.can_connect) else {
            error!("Avvio evento negato: nessun permesso connect nel canale {channel_id}.");
            return StartOutcome::NoConnectPermission;
        };
        // Step 5: Connessione vocale (richiede songbird + voice crate)
        let connection = match self.gateway.connect(guild_id, channel_id).await {
            Ok(connection) => connection,
            Err(VoiceError::Unavailable) => return StartOutcome::VoiceUnavailable,
            Err(VoiceError::Failed(reason)) => {
                error!("Impossibile avviare la modalità evento nel canale vocale {channel_id}: {reason}");
                return StartOutcome::ConnectFailed;
            }
        };
        // Step 6: Crea record nel database
        let started_at = (self.clock)();
        let members: Vec<u64> = connection.members().iter().map(|member| member.id).collect();
        let new_event = NewEvent { guild_id, scheduled_event_id, title: &title, channel_id, channel_name: &channel.name, started_at };
        let event_id = match live_event::create_event_rows(&self.db, &new_event, &members) {
            Ok(event_id) => event_id,
            Err(e) => {
                // Pulizia in caso di errore: ferma la connessione
                connection.stop_listening();
                if connection.is_connected() {
                    if let Err(e) = connection.disconnect().await {
                        error!("Impossibile disconnettere il bot dopo l'errore del report: {e}");
                    }
                }
                error!("Impossibile creare il report dell'evento per il canale {channel_id}: {e}");
                return StartOutcome::ReportFailed;
            }
        };
        // Salva guild_id per eventuali chiamate successive
        if let Err(e) = self.db.set_setting("guild_id", guild_id) {
            error!("Impossibile salvare l'ID del server dell'evento: {e}");
        }
        // Step 7: Inizializza sessione in memoria e overlay
        let now = (self.monotonic)();
        *lock(&self.session) = Some(Session {
            event_id,
            guild_id,
            channel_id,
            connection: connection.clone(),
            title: title.clone(),
            scheduled_event_id,
            speaking: Vec::new(),
            persisted: Vec::new(),
            audio_recovery_attempts: 0,
            last_audio_packet_at: connection.last_packet_at(),
            last_sample: now,
            last_persist: now,
            peak: members.len() as i64,
            disconnected_since: None,
        });
        self.live.start_session(
            LiveSessionInfo { title: title.clone(), guild_id, channel_id, channel_name: channel.name.clone(), started_at: isoformat(started_at) },
            connection as Arc<dyn VoiceSource>,
        );
        // Step 8: Notifica (e genera link per OBS)
        let link = self.overlay_link(guild_id);
        match actor {
            None => {
                // Evento avviato da sistema/sito (es. evento programmato Discord)
                info!("Evento programmato {scheduled_event_id:?} avviato nel canale vocale {channel_id}.");
                // Un evento avviato dal sito non ha un evento programmato: l'ID resta vuoto.
                let entry = AuditEntry::new(None, "scheduled_live_event_started").details(json!({"title": title, "channel_id": channel_id}));
                let entry = match scheduled_event_id {
                    Some(id) => entry.entity("scheduled_event", id),
                    None => entry.entity_type("scheduled_event"),
                };
                self.notifier.record_staff_audit(entry);
                self.notify_staff_event_started(&title, channel_id, guild_id, &link).await;
            }
            Some(actor) => {
                // Evento avviato manualmente da uno staff (comando /evento avvia)
                self.notifier.record_staff_audit(
                    AuditEntry::new(Some(actor), "live_event_started")
                        .entity("live_event", event_id)
                        .details(json!({"title": title, "channel_id": channel_id})),
                );
            }
        }
        StartOutcome::Started { event_id, link }
    }

    /// Il canale staff, se è configurato e testuale. I motivi per cui non lo è si registrano.
    ///
    /// `async fn` = funzione asincrona. Restituisce un `Future` che deve essere `await`ata.
    /// `Option<u64>` = restituisce l'ID del canale se valido, `None` altrimenti.
    /// `&str` = string slice (riferimento a stringa), passato per referenza per evitare copia.
    ///
    /// Flusso:
    /// 1. Legge impostazione `staff_channel` dal DB
    /// 2. Verifica che non sia vuoto
    /// 3. Converte da stringa/Python int a u64 (compatibilità con Python esistente)
    /// 4. Verifica che sia un canale testuale (non voice, non stage)
    async fn staff_text_channel(&self, guild_id: u64, what: &str) -> Option<u64> {
        let raw = match self.db.get_setting("staff_channel") {
            Ok(raw) => raw.filter(|value| !value.is_empty()),
            Err(e) => {
                error!("Impossibile leggere il canale staff per {what}: {e}");
                return None;
            }
        };
        let Some(raw) = raw else {
            error!("Canale staff non configurato per {what}.");
            return None;
        };
        // `live_event::python_int` parse da stringa Python (es. "123456789")
        // `u64::try_from` converte da i64 (Python int) a u64 (Rust), fallisce se negativo
        let Some(channel_id) = live_event::python_int(&raw).and_then(|id| u64::try_from(id).ok()) else {
            error!("ID del canale staff non valido: {raw}.");
            return None;
        };
        match self.api().channel_kind(guild_id, channel_id).await {
            Ok(ChannelKind::Text) => Some(channel_id),
            Ok(_) => {
                error!("Il canale configurato per lo staff {channel_id} non è testuale.");
                None
            }
            Err(e) => {
                error!("Impossibile recuperare il canale staff per {what}: {e}");
                None
            }
        }
    }

    /// Invia notifica allo staff quando un evento programmato si avvia.
    ///
    /// Usa il canale staff configurato per inviare un messaggio con il titolo
    /// e il link per configurare l'OBS Browser Source.
    async fn notify_staff_event_started(&self, title: &str, channel_id: u64, guild_id: u64, link: &str) {
        let Some(staff) = self.staff_text_channel(guild_id, &format!("l'evento {title}")).await else {
            return;
        };
        let message = format!("🎮 Evento programmato **{title}** avviato in <#{channel_id}>.\nLink OBS: {link}");
        if let Err(e) = self.api().send_message(staff, MessagePayload::default().with_content(message)).await {
            error!("Impossibile inviare il link OBS nel canale staff {staff}: {e}");
        }
    }

    /// Pubblica il report finale dell'evento nel canale staff.
    ///
    /// Il report mostra statistiche di parlato, presenze e grafici.
    /// È costruito da `live_event::build_event_report` e inviato come embed grafico.
    async fn publish_event_report(&self, guild_id: u64, event_id: i64) {
        let Some(staff) = self.staff_text_channel(guild_id, &format!("il report evento {event_id}")).await else {
            return;
        };
        let report = match live_event::build_event_report(&self.db, event_id) {
            Ok(Some(report)) => report,
            Ok(None) => {
                error!("Report evento {event_id} non trovato dopo la chiusura.");
                return;
            }
            Err(e) => {
                error!("Impossibile preparare il report evento {event_id}: {e}");
                return;
            }
        };
        if let Err(e) = self.api().send_message(staff, MessagePayload::embed(report.build())).await {
            error!("Impossibile pubblicare il report evento {event_id} nel canale staff: {e}");
        }
    }

    // --- fine ----------------------------------------------------------------------------------

    /// Chiude l'evento in corso con il motivo indicato. Restituisce l'ID del report, o `None` se non
    /// c'era un evento.
    ///
    /// Flow di chiusura:
    /// 1. Prende il lock per chiudere (serializza con eventuali nuovi avvii)
    /// 2. Estrae la sessione e la chiude (`take()` rimpiazza con `None`)
    /// 3. Ferma il listener audio e disconnette dal canale vocale
    /// 4. Salva la chiusura nel database (parlato, presenze, picchi)
    /// 5. Pubblica il report finale
    /// 6. Registra l'audit log
    ///
    /// Nota: `drop(guard)` rilascia il lock PRIMA di pubblicare il report,
    /// altrimenti il report potrebbe chiamare `staff_text_channel` che ha bisogno di lock.
    pub async fn finish_event(&self, reason: &str) -> Option<i64> {
        let guard = self.session_lock.lock().await;
        let session = lock(&self.session).take()?;
        self.live.set_session(None);
        let ended_at = (self.clock)();
        let present_now = session.connection.members().len() as i64;
        if let Err(e) = live_event::finish_event_rows(
            &self.db,
            session.event_id,
            &session.speaking,
            &session.persisted,
            session.peak,
            present_now,
            ended_at,
            reason,
        ) {
            // Un errore del database non deve lasciare l'evento a metà: la pulizia (voce, overlay) e
            // il report proseguono comunque. L'evento non è bloccato, ma i dati parziali verranno
            // recuperati al prossimo avvio.
            error!("Impossibile salvare la chiusura dell'evento {}: {e}", session.event_id);
        }
        // Ferma il listener audio e disconnette dal canale vocale (pulizia risorse)
        session.connection.stop_listening();
        if let Err(e) = session.connection.disconnect().await {
            error!("Impossibile disconnettere il bot dall'evento {}: {e}", session.event_id);
        }
        self.live.hub.event_ended();
        drop(guard);  // Rilascia il lock prima di operazioni async
        self.publish_event_report(session.guild_id, session.event_id).await;
        self.notifier.record_staff_audit(
            AuditEntry::new(None, "live_event_finished")
                .entity("live_event", session.event_id)
                .details(json!({"reason": reason, "title": session.title})),
        );
        Some(session.event_id)
    }

    /// `/evento termina`: chiude l'evento a mano e lo scrive nel registro audit.
    ///
    /// Usato quando uno staff chiude manualmente un evento in corso.
    /// Il motivo è "manuale" e l'azione è registrata con l'ID dell'attore (staff).
    pub async fn stop_event(&self, actor: u64) -> Option<i64> {
        let event_id = self.finish_event("manuale").await?;
        self.notifier.record_staff_audit(AuditEntry::new(Some(actor), "live_event_stopped").entity("live_event", event_id));
        Some(event_id)
    }

    // --- presenze ------------------------------------------------------------------------------

    /// Una persona entra o esce dal canale dell'evento (`on_voice_state_update`).
    ///
    /// Questo è un listener per l'evento Discord `VOICE_STATE_UPDATE`.
    /// Viene chiamato quando qualcuno:
    /// - Si unisce al canale vocale dell'evento
    /// - Esce dal canale vocale
    /// - Muove il bot tra canali (nessun cambiamento evento)
    ///
    /// Aggiorna la statistica di "presenze" nel database.
    ///
    /// Parametri:
    /// - `guild_id`: server Discord
    /// - `user_id`: utente che ha cambiato stato
    /// - `is_bot`: ignora gli eventi dei bot (inclusi i bot Discord)
    /// - `before`: canale vocale prima (`None` = non in voice)
    /// - `after`: canale vocale dopo
    ///
    /// CONCETTO: `Option<u64>` per canale: `Some(id)` se in voice, `None` se non lo è.
    /// Il confronto `before == Some(session.channel_id)` dice se l'utente era nell'evento.
    pub fn on_voice_state_update(&self, guild_id: u64, user_id: u64, is_bot: bool, before: Option<u64>, after: Option<u64>) {
        let mut guard = lock(&self.session);
        let Some(session) = guard.as_mut() else { return };
        // Ignora bot e utenti di altri server
        if is_bot || guild_id != session.guild_id {
            return;
        }
        let was_present = before == Some(session.channel_id);
        let is_present = after == Some(session.channel_id);
        // Se lo stato non è cambiato, non serve fare nulla
        if was_present == is_present {
            return;
        }
        let present_now = session.connection.members().len() as i64;
        match live_event::record_presence_change(&self.db, session.event_id, user_id, was_present, is_present, (self.clock)(), present_now, session.peak) {
            Ok(peak) => session.peak = peak,
            Err(e) => error!("Impossibile aggiornare la presenza dell'utente {user_id} nell'evento {}: {e}", session.event_id),
        }
    }

    // --- ciclo ---------------------------------------------------------------------------------

    /// Aggiorna i contatori della sessione e dice cosa fare dopo.
    ///
    /// Questo è il cuore del monitoraggio in tempo reale dell'evento.
    /// Viene chiamato regolarmente (ogni 250ms) dal ciclo principale.
    ///
    /// Flow:
    /// 1. Legge i livelli di volume audio dalla connessione voce
    /// 2. Aggiorna il tempo parlato per ogni utente (somma le nuove ore audio)
    /// 3. Salva periodicamente il progresso nel database (ogni 5 secondi)
    /// 4. Controlla se la connessione è ancora attiva
    /// 5. Restituisce l'azione da compiere: Idle, Finish o Publish
    fn advance(&self, now: Instant, wall: DateTime<Tz>) -> Next {
        let mut guard = lock(&self.session);
        let Some(session) = guard.as_mut() else { return Next::Idle };
        // Calcola quanto tempo è passato dall'ultimo campione (max 1 secondo per stabilità)
        let elapsed = now.saturating_duration_since(session.last_sample).as_secs_f64().min(1.0);
        session.last_sample = now;
        // Leggi i livelli audio e l'ultimo pacchetto ricevuto
        let levels = session.connection.levels();
        let last_packet = session.connection.last_packet_at();
        // Se abbiamo ricevuto un nuovo pacchetto, resetta il contatore errori
        if last_packet > session.last_audio_packet_at {
            session.last_audio_packet_at = last_packet;
            session.audio_recovery_attempts = 0;
        }
        // Aggiorna il tempo parlato per ogni utente
        // speaking è una lista di (user_id, secondi_parlati)
        let mut speaking: Vec<u64> = levels.keys().copied().collect();
        speaking.sort_unstable();  // Ordina per user_id per stabililità
        for user_id in speaking {
            match session.speaking.iter_mut().find(|(id, _)| *id == user_id) {
                Some((_, seconds)) => *seconds += elapsed,
                None => session.speaking.push((user_id, elapsed)),
            }
        }
        // Salva nel database ogni 5 secondi (PERSIST_EVERY)
        if now.saturating_duration_since(session.last_persist) >= PERSIST_EVERY {
            match live_event::persist_progress(&self.db, session.event_id, &session.speaking, &session.persisted, wall) {
                Ok(()) => session.persisted = session.speaking.clone(),
                Err(e) => error!("Impossibile salvare il progresso dell'evento {}: {e}", session.event_id),
            }
            session.last_persist = now;
        }
        // Controlla se la connessione è ancora attiva
        if !session.connection.is_connected() {
            let since = *session.disconnected_since.get_or_insert(now);
            // Aspetta un po' prima di chiudere (gli eventi spesso si chiudono e riaprono)
            if now.saturating_duration_since(since) < VOICE_DISCONNECT_GRACE {
                return Next::Idle;
            }
            warn!("Connessione vocale persa durante l'evento {}.", session.event_id);
            return Next::Finish("disconnessione");
        }
        session.disconnected_since = None;
        Next::Publish { connection: session.connection.clone(), levels }
    }

    /// Un passo del ciclo: conteggio del parlato, salvataggio, controllo della connessione e
    /// pubblicazione dello stato alle pagine overlay.
    ///
    /// Richiama `advance()` e gestisce il risultato:
    /// - `Idle`: niente da fare, ma continua il ciclo
    /// - `Finish`: chiude l'evento con la ragione data
    /// - `Publish`: invia lo snapshot audio alle pagine web overlay
    pub async fn tick(&self, now: Instant, wall: DateTime<Tz>) -> Result<(), String> {
        match self.advance(now, wall) {
            Next::Idle => Ok(()),
            Next::Finish(reason) => {
                self.finish_event(reason).await;
                Ok(())
            }
            Next::Publish { connection, levels } => {
                // Se nessun client ha le overlay aperti, non serve calcolare lo snapshot
                if self.live.hub.client_count() == 0 {
                    return Ok(());
                }
                // Ottieni le informazioni dell'evento corrente
                let Some(info) = self.live.session() else { return Ok(()) };
                // Costruisci e pubblica lo snapshot per gli overlay
                let snapshot = overlay_web::event_snapshot(&self.db, &info, &connection.members(), &levels).map_err(|e| e.to_string())?;
                self.live.hub.broadcast(&snapshot.to_string());
                Ok(())
            }
        }
    }

    /// Un passo con il conteggio degli errori di fila: dopo 40 l'evento viene chiuso.
    ///
    /// Il ciclo principale chiama questa funzione ogni tick.
    /// Gli errori vengono contati; dopo 40 errori consecutivi l'evento si chiude automaticamente
    /// per prevenire stati inconsistenti.
    /// - Errori consecutivi 1, 10, 40: log a livello error
    /// - Ogni 100 errori: log aggiuntivo (tracciabilità lunghi periodi di instabilità)
    /// - Massimo 500 errori prima che l'evento venga chiuso
    pub async fn loop_step(&self, failures: &mut u32, now: Instant, wall: DateTime<Tz>) {
        match self.tick(now, wall).await {
            Ok(()) => *failures = 0,
            Err(e) => {
                *failures += 1;
                if matches!(*failures, 1 | 10 | 40) || *failures % 100 == 0 {
                    error!("Errore nel ciclo di pubblicazione dell'evento ({} consecutivi): {e}", *failures);
                }
                if *failures >= MAX_FAILURES {
                    self.finish_event("errore").await;
                    *failures = 0;
                }
            }
        }
    }

    /// Il ciclo di pubblicazione (`publish_loop`): quattro passi al secondo.
    ///
    /// Questo è un task asincRONO che gira in background per tutta la durata dell'evento.
    /// Usa il timer tokio `interval` per chiamare `loop_step` regolarmente.
    ///
    /// CONCETTO: `self: Arc<Self>` significa che la funzione prende possesso di una reference
    /// counting smart pointer condivisa. Questo è necessario perché il task gira in un
    /// tokio::spawn (thread separato) e deve poter accedere a `self` dopo che la chiamata
    /// a `run_loop` termina.
    ///
    /// `MissedTickBehavior::Delay`: se il sistema è occupato e mancano i tick, i tick
    /// vengono accumulati e eseguiti in sequenza (non saltati), per evitare di perdere aggiornamenti.
    pub async fn run_loop(self: Arc<Self>) {
        let mut ticker = interval(TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut failures = 0u32;
        loop {
            ticker.tick().await;
            self.loop_step(&mut failures, (self.monotonic)(), (self.clock)()).await;
        }
    }

    // --- ascolto audio -------------------------------------------------------------------------

    /// L'ascolto audio si è interrotto con un errore: tra poco si riprova.
    ///
    /// Questo metodo viene chiamato quando il songbird (libreria audio) segnala un errore
    /// nella ricezione dell'audio. Invece di chiudere l'evento immediatamente,
    /// pianifica un tentativo di ripristino con backoff esponenziale.
    ///
    /// Flow:
    /// 1. Logga l'errore con il motivo
    /// 2. Clona `self` per passarlo al task separato
    /// 3. Avvia un tokio::spawn per il ripristino (non blocca il chiamante)
    ///
    /// CONCETTO: `Arc<Self>` condiviso + `clone()` crea una nuova reference counting,
    /// permettendo al task asincrono di usare `self` anche dopo che il chiamante finisce.
    /// `Arc::ptr_eq` in altre parti verifica se due `Arc` puntono allo stesso oggetto.
    pub fn listener_failed(self: &Arc<Self>, connection: Arc<dyn VoiceConnection>, reason: &str) {
        // Mostra se l'evento è già avviato ("in avvio" se no)
        let event = lock(&self.session).as_ref().map_or_else(|| "in avvio".to_string(), |session| session.event_id.to_string());
        error!("Listener audio dell'evento {event} interrotto ({reason}); tenterò il riavvio.");
        // Clona l'Arc e avvia il recovery in un task separato (non blocca)
        let service = self.clone();
        tokio::spawn(async move { service.recover_audio_listener(connection).await });
    }

    /// Riavvia l'ascolto dopo 1, 2, 4… fino a 30 secondi, a seconda dei tentativi già fatti.
    ///
    /// Usa backoff esponenziale: se l'audio non arriva, aspetta più a lungo prima del prossimo retry.
    /// Formula: delay = min(30, 2^tentativi) secondi.
    ///
    /// Flow:
    /// 1. Calcola il delay basato sui tentativi precedenti
    /// 2. Aspetta il delay
    /// 3. Verifica che la connessione sia ancora quella giusta (può essere cambiata)
    /// 4. Chiama `restart_listening()` sulla connessione
    ///
    /// CONCETTO: `Arc::ptr_eq(&a, &b)` verifica se due `Arc<T>` puntano allo stesso oggetto
    /// sottostante, utile per confrontare connection prima e dopo l'attesa.
    pub async fn recover_audio_listener(&self, connection: Arc<dyn VoiceConnection>) {
        // Calcola il delay: 2^tentativi secondi, max 30
        let delay = {
            let mut guard = lock(&self.session);
            let Some(session) = guard.as_mut().filter(|session| Arc::ptr_eq(&session.connection, &connection)) else {
                return;  // Sessione cambiata o nessuna sessione
            };
            let attempt = session.audio_recovery_attempts;
            session.audio_recovery_attempts += 1;
            Duration::from_secs(30.min(1u64 << attempt.min(5)))  // 2^attempt, max 30
        };
        tokio::time::sleep(delay).await;
        // Rilegge lo stato dopo l'attesa
        let _guard = self.session_lock.lock().await;
        let event_id = {
            let guard = lock(&self.session);
            let Some(session) = guard.as_ref().filter(|session| Arc::ptr_eq(&session.connection, &connection)) else {
                return;  // Sessione cambiata o nessuna sessione
            };
            // Non riavviare se già disconnesso o già in ascolto
            if !connection.is_connected() || connection.is_listening() {
                return;
            }
            session.event_id
        };
        if let Err(e) = connection.restart_listening() {
            error!("Riavvio del listener audio per l'evento {event_id} non riuscito: {e}");
            return;
        }
        info!("Listener audio dell'evento {event_id} riavviato.");
    }

    // --- eventi programmati di Discord ---------------------------------------------------------

    /// Avvia l'evento per un evento programmato abilitato che è diventato attivo
    /// (`_start_scheduled_event`).
    pub async fn start_scheduled_event(&self, guild_id: u64, event: &ScheduledEventInfo) {
        if !live_event::scheduled_event_optins(&self.db).contains(&(event.id as i64))
            || event.status != EventStatus::Active
            || lock(&self.auto_starting).contains(&event.id)
        {
            return;
        }
        let is_voice = match event.channel_id {
            Some(channel) => self.api().channel_kind(guild_id, channel).await.is_ok_and(|kind| kind == ChannelKind::Voice),
            None => false,
        };
        let Some(channel_id) = event.channel_id.filter(|_| is_voice) else {
            warn!("Evento programmato {} abilitato ma non collegato a un canale vocale standard.", event.id);
            return;
        };
        lock(&self.auto_starting).insert(event.id);
        let outcome = self
            .start_event(StartRequest {
                guild_id,
                channel_id,
                title: event.name.chars().take(100).collect(),
                scheduled_event_id: Some(event.id),
                actor: None,
            })
            .await;
        lock(&self.auto_starting).remove(&event.id);
        if !matches!(outcome, StartOutcome::Started { .. } | StartOutcome::AlreadyRunning) {
            error!("Avvio automatico dell'evento programmato {} non riuscito: {outcome:?}.", event.id);
        }
    }

    /// Ricorda lo stato di un evento programmato; restituisce quello precedente.
    pub fn note_event_status(&self, event_id: u64, status: EventStatus) -> Option<EventStatus> {
        lock(&self.event_status).insert(event_id, status)
    }

    /// Un evento programmato è cambiato (`on_scheduled_event_update`). Parte quando diventa attivo,
    /// e finisce quando un evento attivo si conclude o viene annullato.
    pub async fn on_scheduled_event_update(&self, guild_id: u64, event: &ScheduledEventInfo) {
        let before = self.note_event_status(event.id, event.status);
        if before != Some(EventStatus::Active) && event.status == EventStatus::Active {
            self.start_scheduled_event(guild_id, event).await;
        } else if before.is_none_or(|status| status == EventStatus::Active)
            && matches!(event.status, EventStatus::Completed | EventStatus::Canceled)
            && lock(&self.session).as_ref().is_some_and(|session| session.scheduled_event_id == Some(event.id))
        {
            self.finish_event(if event.status == EventStatus::Completed { "completato" } else { "annullato" }).await;
        }
    }

    /// Un evento programmato è stato eliminato (`on_scheduled_event_delete`).
    pub async fn on_scheduled_event_delete(&self, event_id: u64) {
        lock(&self.event_status).remove(&event_id);
        if lock(&self.session).as_ref().is_some_and(|session| session.scheduled_event_id == Some(event_id)) {
            self.finish_event("eliminato").await;
        }
    }

    /// Il bot è pronto: ricorda gli eventi programmati e avvia quelli abilitati già attivi
    /// (`on_ready`).
    pub async fn on_ready(&self, guilds: &[u64]) {
        for guild_id in guilds {
            let events = match self.api().scheduled_events(*guild_id).await {
                Ok(events) => events,
                Err(e) => {
                    error!("Impossibile leggere gli eventi programmati del server {guild_id}: {e}");
                    continue;
                }
            };
            for event in &events {
                self.note_event_status(event.id, event.status);
            }
            // Si leggono qui, come in Python, una volta per ogni server.
            let opted_in = live_event::scheduled_event_optins(&self.db);
            for event in events.iter().filter(|event| opted_in.contains(&(event.id as i64)) && event.status == EventStatus::Active) {
                self.start_scheduled_event(*guild_id, event).await;
            }
        }
    }
}

impl<S: DiscordSender, A: DiscordApi, G: VoiceGateway> LiveController for LiveService<S, A, G> {
    /// Implementazione di `LiveController::start`: avvia un evento manuale (comando `/evento avvia`).
    ///
    /// Crea un `StartRequest` con:
    /// - `guild_id`, `channel_id`: dal comando
    /// - `title`: `DEFAULT_TITLE` ("🎮 Game Night") titolo fisso per avvii manuali
    /// - `scheduled_event_id: None`: non è un evento Discord programmato
    /// - `actor: Some(actor)`: l'ID dello staff che ha lanciato il comando
    ///
    /// `Box::pin(async move { ... })` incapsula il future asincrono in un `BoxFuture`
    /// (smart pointer sul heap + `Pin` per future auto-referenziali), richiesto dalla firma del trait.
    fn start(&self, guild_id: u64, channel_id: u64, actor: u64) -> BoxFuture<'_, StartOutcome> {
        Box::pin(async move {
            self.start_event(StartRequest { guild_id, channel_id, title: DEFAULT_TITLE.to_string(), scheduled_event_id: None, actor: Some(actor) }).await
        })
    }

    /// Implementazione di `LiveController::stop`: chiude l'evento manualmente (comando `/evento termina`).
    ///
    /// Chiama `stop_event` che a sua volta chiama `finish_event("manuale")` e registra
    /// l'audit log con l'ID dell'attore (staff che ha terminato).
    /// Restituisce `Option<i64>` = ID del report chiuso, o `None` se non c'era evento.
    fn stop(&self, actor: u64) -> BoxFuture<'_, Option<i64>> {
        Box::pin(self.stop_event(actor))
    }

    /// Implementazione di `LiveController::standard_voice_channel_of`: trova il canale vocale
    /// standard (non Stage) in cui si trova un utente.
    ///
    /// Flow:
    /// 1. `gateway.user_voice_channel` = in quale canale voice è l'utente (qualunque tipo)
    /// 2. `api().channel_kind` = verifica che sia `ChannelKind::Voice` (esclude Stage)
    /// 3. `.then_some(channel)` = restituisce `Some(channel_id)` se voice, `None` altrimenti
    ///
    /// `?` propaga l'errore (`None`) se l'utente non è in voice o l'API fallisce.
    fn standard_voice_channel_of(&self, guild_id: u64, user_id: u64) -> BoxFuture<'_, Option<u64>> {
        Box::pin(async move {
            let channel = self.gateway.user_voice_channel(guild_id, user_id)?;
            (self.api().channel_kind(guild_id, channel).await.ok()? == ChannelKind::Voice).then_some(channel)
        })
    }

    /// Implementazione di `LiveController::finish`: chiude l'evento con un motivo dato.
    ///
    /// Usato per chiusure automatiche (es. evento programmato completato/annullato/eliminato,
    /// disconnessione prolungata, troppi errori). Non scrive nell'audit log (chi chiama
    /// `finish_event` lo fa a parte se serve).
    /// `reason` è `&'static str` = string literal codificata nel binario (es. "completato", "annullato").
    fn finish(&self, reason: &'static str) -> BoxFuture<'_, Option<i64>> {
        Box::pin(self.finish_event(reason))
    }

    /// Implementazione di `LiveController::start_titled`: avvia un evento con titolo personalizzato.
    ///
    /// Usato dal sito web (staff tools) per avviare un evento come l'avvio automatico:
    /// - `scheduled_event_id: None` = non collegato a evento Discord
    /// - `actor: None` = avviato dal sistema/sito, non da uno staff via comando
    /// - `title` fornito dal chiamante (scelto dall'utente nel pannello)
    fn start_titled(&self, guild_id: u64, channel_id: u64, title: String) -> BoxFuture<'_, StartOutcome> {
        Box::pin(async move { self.start_event(StartRequest { guild_id, channel_id, title, scheduled_event_id: None, actor: None }).await })
    }
}

#[cfg(test)]
mod tests;