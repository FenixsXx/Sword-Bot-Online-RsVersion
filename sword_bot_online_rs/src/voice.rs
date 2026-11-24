//! Il collegamento ai canali vocali, dietro dei trait (in Python `discord.VoiceChannel`,
//! `VoiceRecvClient` e `SpeakerTracker`).
//!
//! L'evento live (`live_service`) non sa come ci si collega a un canale e come si ascoltano i
//! pacchetti audio: chiede a un [`VoiceGateway`] di collegarsi e riceve una [`VoiceConnection`]. In
//! produzione c'è [`SongbirdGateway`]: legge dalla cache canali, permessi e presenze e si collega
//! con songbird, che riceve l'audio (anche con la cifratura end-to-end DAVE) e lo decodifica in PCM.
//! [`SpeakerTracker`] ne ricava chi sta parlando senza conservare audio.
//!
//! --- SPIEGAZIONE PER NON PROGRAMMATORI ---
//!
//! Questo modulo gestisce tutto ciò che riguarda l'audio vocale su Discord.
//!
//! **Cosa sono i "canali vocali" su Discord:**
//! Su Discord, oltre ai canali di testo (dove si scrive), ci sono i "canali vocali" dove le persone
//! possono parlare tra loro in tempo reale, come in una chiamata telefonica o una videoconferenza.
//! Il bot può "entrare" in questi canali per ascoltare chi sta parlando.
//!
//! **Cosa sono i "trait" (tratti):**
//! In Rust, un "trait" è come un "contratto" o un'interfaccia: definisce QUALI operazioni deve
//! supportare un oggetto, senza dire COME le implementa. È simile alle "classi astratte" o
//! "interfacce" in altri linguaggi. Qui usiamo i trait per separare:
//! - COSA fa il bot con la voce (collegarsi, ascoltare, scollegarsi) → `VoiceConnection`
//! - COME si collega effettivamente (usando songbird, o un finto per i test) → `VoiceGateway`
//!
//! Questo permette di cambiare la libreria audio sottostante senza riscrivere la logica dell'evento live.
//!
//! **Cos'è songbird:**
//! È la libreria Rust che usa il bot per collegarsi ai canali vocali Discord. Gestisce la connessione
//! WebRTC, la cifratura DAVE (Discord Audio Voice Encryption - crittografia end-to-end dell'audio),
//! la decodifica dei pacchetti audio Opus in PCM (dati audio grezzi non compressi).
//!
//! **Cos'è il PCM (Pulse Code Modulation):**
//! È il formato "grezzo" dell'audio digitale: una sequenza di numeri (campioni) che rappresentano
//! l'ampiezza dell'onda sonora nel tempo. Qui usiamo campioni a 16 bit (valori da -32768 a +32767).
//! Songbird decodifica l'audio Opus (compresso) in PCM per poterlo analizzare.
//!
//! **Cos'è l'RMS (Root Mean Square - Radice Quadratica Media):**
//! È una misura matematica del "volume" effettivo di un segnale audio. Non è il picco massimo,
//! ma la media quadratica dei campioni: dà un valore che corrisponde meglio a come l'orecchio umano
//! percepisce il volume. La formula è: radice quadrata della media dei quadrati dei campioni.
//!
//! **Cos'è l'SSRC (Synchronization Source):**
//! In WebRTC/RTP, ogni flusso audio ha un identificatore univoco a 32 bit chiamato SSRC.
//! Quando Discord invia pacchetti audio, li etichetta con l'SSRC di chi li ha mandati.
//! Il bot usa una tabella SSRC → User ID per sapere chi sta parlando.
//!
//! **Cosa sono AtomicBool e Mutex:**
//! Sono strumenti per la "concorrenza" (più cose che succedono insieme in modo sicuro).
//! - `AtomicBool`: un valore vero/falso che può essere letto e scritto da più thread contemporaneamente
//!   senza conflitti (operazioni "atomiche", indivisibili).
//! - `Mutex` (Mutual Exclusion): un "lucchetto" che permette a un solo thread alla volta di accedere
//!   a un dato. Gli altri thread aspettano. Usato qui per proteggere le mappe (dizionari) condivise.
//!
//! **Cos'è `BoxFuture` (type erasure per async):**
//! In Rust, ogni funzione `async` ha un tipo diverso e anonimo. Per usare metodi `async` nei trait
//! (che richiedono un tipo concreto), "cancelliamo" il tipo specifico mettendo il future in un
//! `Box` (allocazione sull'heap) e `Pin` (garantisce che non si sposti in memoria). Questo si chiama
//! "type erasure" (cancellazione del tipo) e permette il polimorfismo con funzioni asincrone.
//!
//! **Cos'è DAVE (Discord Audio Voice Encryption):**
//! È il sistema di cifratura end-to-end che Discord usa per l'audio vocale. Anche se i pacchetti
//! passano per i server Discord, solo i partecipanti alla chiamata hanno le chiavi per decifrarli.
//! Songbird gestisce automaticamente lo scambio chiavi e la decifratura.
//!
//! **Cosa fa SpeakerTracker:**
//! Riceve pacchetti PCM decodificati, calcola il volume (RMS) di ciascuno, e tiene traccia di chi
//! sta parlando "ora". Non registra/salva l'audio: mantiene solo il volume recente per ogni utente.
//! Dopo 800ms senza pacchetti sopra soglia, l'utente viene considerato "non in parlato".

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::serenity_prelude::{self as serenity, ChannelId, GuildId, UserId};
use songbird::Songbird;
use songbird::driver::{DecodeConfig, DecodeMode};
use songbird::events::context_data::VoiceTick;
use songbird::events::{CoreEvent, EventContext};
use tokio::time::Instant;
use tracing::warn;

use crate::overlay::{VoiceMember, VoiceSource};

/// Un futuro con tipo cancellato, per i metodi `async` dei trait usati come `dyn`.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa è un "futuro" (Future):**
/// Un futuro è un valore che arriverà in futuro, come promettere di "ritirarsi dallo shopping" e poi
/// ritornare con l'acquisto. In Rust, le funzioni `async` ritornano immediatamente un "futuro",
/// che rappresenta l'operazione in corso. Il futuro può essere "attenduto" con `.await` per ottenere
/// il risultato quando disponibile.
///
/// **Cosa significa "con tipo cancellato":**
/// Ogni funzione `async` in Rust ha un tipo anonimo (es. `impl Future<Output = Result<...>>` o
/// `Pin<Box<...>>`). Quando si vuole passare un futuro come parametro o in una struttura, bisogna
/// "cancelare" (cancellare) il tipo specifico, lasciandolo come `dyn Future`. Questo si chiama
/// "type erasure" e permette di trattare futuri diversi come se fossero uguali.
///
/// **Perché serve questo alias:**
/// Gli interceptor di trait (come `VoiceConnection`) hanno metodi `async`, ma Rust non permette
/// direttamente di usare `.await` nei metodi dei trait se il tipo non è noto. Questo alias crea
/// un tipo compatto che "nasconde" il dettaglio del futuro specifico, permettendo il polimorfismo.
///
/// **Spiegazione dei componenti:**
/// - `T`: il tipo del risultato (es. `Result<(), VoiceError>` o `bool`)
/// - `'a`: il "lifetime" - indica per quanto tempo il futuro può esistere
/// - `Pin`: garantisce che il futuro non si sposti in memoria dopo essere stato creato
/// - `Box`: allocazione dinamica (sull'heap) invece che sullo stack
/// - `dyn Future`: type erasure: qualunque tipo che implementa Future
/// - `Send`: il futuro può essere spasticato tra thread (necessario per l'asincrono multi-thread)
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Rappresenta tutti gli errori che possono verificarsi durante le operazioni vocali.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa è un'enum in Rust:**
/// Un'enum è una "scelta" tra diverse opzioni. Immagina una scatola con etichette diversate:
/// - "Disponibile" (se il bot non supporta la voce)
/// - "Fallito" (se c'è stato un errore con un messaggio descrittivo)
/// Ogni variante può avere dati associati (come `Failed(String)` ha una stringa con il messaggio).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceError {
    /// Questa versione del bot non sa collegarsi ai canali vocali.
    /// Si verifica quando si tenta di usarlo in un ambiente dove l'audio non è supportato.
    Unavailable,
    /// Il collegamento o l'avvio dell'ascolto sono falliti (permessi, libreria, rete).
    /// Contiene un messaggio dettagliato sull'errore specifico.
    Failed(String),
}

impl std::fmt::Display for VoiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VoiceError::Unavailable => f.write_str("collegamento vocale non disponibile"),
            VoiceError::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for VoiceError {}

/// Un collegamento attivo a un canale vocale, con l'ascolto già avviato.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa è un trait:**
/// Un trait è un "contratto" che definisce cosa deve fare un oggetto.
/// Immagina che ogni oggetto che implementa questo trait debba avere tutti questi metodi.
/// Serve come interfaccia comune per diversi tipi di connessioni (reale, finta per i test).
///
/// **Cosa eredita da VoiceSource:**
/// Questo trait eredita da `VoiceSource`, il che significa che una `VoiceConnection`
/// ha anche le capacità di `VoiceSource` (mostrare i membri nel canale e i livelli di voce).
///
/// **Metodi:**
/// - `is_connected()`: verifica se il bot è ancora nel canale vocale
/// - `is_listening()`: verifica se sta ricevendo e processando l'audio
/// - `last_packet_at()`: quando è arrivato l'ultimo pacchetto audio (per debug/monitoring)
/// - `restart_listening()`: riavvia l'ascolto dopo un pause o errore temporaneo
/// - `stop_listening()`: ferma la ricezione dell'audio (ma non lascia il canale)
/// - `disconnect()`: lascia completamente il canale e chiude la connessione
pub trait VoiceConnection: VoiceSource {
    /// Il bot è ancora nel canale (`voice_client.is_connected()`).
    /// Ritorna `true` finché il bot è connesso al canale, `false` se è stato scollegato.
    fn is_connected(&self) -> bool;

    /// L'ascolto dei pacchetti è attivo (`voice_client.is_listening()`).
    /// Ritorna `true` se il bot sta ricevendo audio e processandolo,
    /// `false` se è in pausa (es. quando l'audio è troppo basso).
    fn is_listening(&self) -> bool;

    /// Quando è arrivato l'ultimo pacchetto audio (`sink.last_packet_at`), se è mai arrivato.
    /// Usato per monitorare se l'audio sta arrivando o se la connessione è inattiva.
    fn last_packet_at(&self) -> Option<Instant>;

    /// Riavvia l'ascolto dopo un errore.
    /// Normalmente l'ascolto si ferma quando il volume è basso per risparmiare risorse.
    /// Questo metodo lo riattiva.
    fn restart_listening(&self) -> Result<(), VoiceError>;

    /// Ferma l'ascolto senza lasciare il canale.
    /// Usato quando il bot deve smettere di processare l'audio ma rimanere connesso.
    fn stop_listening(&self);

    /// Lascia il canale (`disconnect(force=True)`).
    /// Chiude completamente la connessione vocale e libera le risorse.
    /// Ritorna un errore se il collegamento non riesce a chiudersi.
    fn disconnect(&self) -> BoxFuture<'_, Result<(), VoiceError>>;
}

/// Quanto serve sapere di un canale vocale per decidere se collegarsi.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa è una struct:**
/// Una struct è come un "record" o una "scheda" che raggruppa dati correlati.
/// Qui rappresenta le informazioni minime per decidere se entrare in un canale vocale.
///
/// **Perché servono questi campi:**
/// - `name`: il nome visibile del canale (es. "Generale", "Musica")
/// - `can_connect`: se il bot ha i permessi per entrare. Discord controlla i permessi
///   per ogni canale: il bot può vedere il canale ma non entrare se non ha il permesso "Connetti".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceChannelInfo {
    /// Il nome visibile del canale vocale (es. "Generale", "Riunioni", "Musica").
    pub name: String,
    /// Il bot può connettersi (`channel.permissions_for(guild.me).connect`).
    /// `true` se il bot ha il permesso "Connetti" in quel canale, `false` altrimenti.
    pub can_connect: bool,
}

/// Interfaccia per gestire la connessione ai canali vocali (gateway = "porta d'ingresso").
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cos'è un "gateway" in questo contesto:**
/// È l'interfaccia che sa COME connettersi ai canali vocali Discord. L'evento live non sa
/// come farlo direttamente: chiede al gateway "connettimi a questo canale" e riceve una
/// `VoiceConnection` già pronta per ascoltare.
///
/// **Perché separare gateway e connection:**
/// - Il gateway sa COME connettersi (usa songbird, cache Discord, permessi)
/// - La connection è il risultato: una connessione ATTIVA che sta ascoltando
/// - Questo permette di avere implementazioni diverse: produzione (songbird) e test (fake)
///
/// **I bound (limiti) del trait: `Send + Sync + 'static`:**
/// - `Send`: può essere spostato tra thread in modo sicuro
/// - `Sync`: può essere condiviso tra thread (riferimenti &self thread-safe)
/// - `'static`: vive per tutta la durata del programma (nessun riferimento temporaneo)
/// Questo è necessario perché il gateway viene usato in contesti asincroni multi-thread.
pub trait VoiceGateway: Send + Sync + 'static {
    /// Il bot è tra i membri del server (`guild.me`).
    /// Verifica se il bot è presente nel server specificato (non è stato cacciato/bannato).
    fn bot_in_guild(&self, guild_id: u64) -> bool;

    /// Recupera le informazioni di un canale vocale per decidere se collegarsi.
    /// Ritorna `None` se il canale non esiste o non è accessibile.
    fn channel_info(&self, guild_id: u64, channel_id: u64) -> Option<VoiceChannelInfo>;

    /// Il canale vocale in cui si trova una persona (`member.voice.channel`).
    /// Ritorna l'ID del canale vocale dove l'utente è connesso ora, o `None` se non è in voce.
    fn user_voice_channel(&self, guild_id: u64, user_id: u64) -> Option<u64>;

    /// Si collega al canale e avvia l'ascolto.
    /// Se il bot è già in un altro canale dello stesso server, lo lascia prima.
    /// Se in caso di errore è rimasto collegato, si scollega prima di restituire l'errore.
    /// Ritorna una `VoiceConnection` avvolta in `Arc` (condivisione thread-safe).
    fn connect(&self, guild_id: u64, channel_id: u64) -> impl Future<Output = Result<Arc<dyn VoiceConnection>, VoiceError>> + Send;
}

// --- chi sta parlando ---------------------------------------------------------------------------

/// Il volume (RMS relativo al massimo) da cui si considera che una persona stia parlando.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa è la soglia (threshold):**
/// È il livello minimo di volume per considerare qualcuno "in parlato".
/// 0.012 = 1.2% del volume massimo. È una soglia bassa per captare anche voci basse,
/// ma abbastanza alta da ignorare il rumore di fondo (ventole, tastiere, ecc.).
///
/// **Come si calcola:**
/// L'RMS (Root Mean Square) dei campioni PCM a 16 bit, diviso 32768 (il valore massimo).
/// Restituisce un valore tra 0.0 (silenzio) e 1.0 (volume massimo).
pub const SPEAKER_THRESHOLD: f64 = 0.012;

/// Dopo quanto senza pacchetti sopra soglia una persona non risulta più in parlato.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Perché un timeout:**
/// L'audio arriva a pacchetti (ogni 20ms circa). Quando qualcuno smette di parlare,
/// l'ultimo pacchetto "forte" arriva e poi non ne arrivano più sopra soglia.
/// Dopo 800ms (quasi un secondo) senza pacchetti forti, consideriamo la persona zitta.
///
/// **Valore scelto:**
/// 800ms è un compromesso:
/// - Troppo breve: taglia le pause naturali nel parlato (respiro, punteggiatura)
/// - Troppo lungo: mostra persone "in parlato" che hanno smesso da tempo
pub const SPEAKER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(800);

/// `audioop.rms(pcm, 2) / 32768`: il valore efficace dei campioni a 16 bit, troncato all'intero
/// come fa `audioop`, rispetto al fondo scala.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa fa questa funzione:**
/// Calcola il livello di volume (RMS) di un pacchetto audio PCM.
///
/// **Passaggi:**
/// 1. Eleva al quadrato ogni campione (perché il volume non ha segno: + e - sono uguali)
/// 2. Fa la media dei quadrati
/// 3. Radice quadrata della media = RMS (Root Mean Square)
/// 4. Tronca all'intero (come fa `audioop` di Python per compatibilità)
/// 5. Divide per 32768 (valore massimo di un campione a 16 bit) per normalizzare 0.0-1.0
///
/// **Perché i16 (16 bit):**
/// L'audio CD usa 16 bit per campione. I valori vanno da -32768 a +32767.
/// 32768 è 2^15 (il valore assoluto massimo per un intero con segno a 16 bit).
pub fn pcm_level(pcm: &[i16]) -> f64 {
    let sum_of_squares: f64 = pcm.iter().map(|sample| f64::from(*sample) * f64::from(*sample)).sum();
    let rms = (sum_of_squares / pcm.len() as f64).sqrt() as u32;
    f64::from(rms) / 32768.0
}

/// Stato interno del tracker: non pubblico, solo per SpeakerTracker.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa è una struct privata:**
/// È una struttura dati che solo il codice in questo file può usare. Non fa parte
/// dell'interfaccia pubblica. Qui contiene i dati grezzi che il tracker mantiene.
///
/// **Campi:**
/// - `levels`: una HashMap (dizionario) che mappa User ID → (volume, timestamp).
///   Solo gli utenti che hanno parlato RECENTEMENTE (sopra soglia) sono qui.
/// - `last_packet_at`: quando è arrivato l'ultimo pacchetto audio di QUALSIASI tipo,
///   usato per monitoraggio/debug.
#[derive(Debug, Default)]
struct TrackerState {
    /// Per ogni utente il volume dell'ultimo pacchetto sopra soglia e quando è arrivato.
    /// Chiave: User ID (u64), Valore: (livello volume 0.0-1.0, istante in cui è arrivato).
    levels: std::collections::HashMap<u64, (f64, Instant)>,
    /// Quando è arrivato l'ultimo pacchetto audio (di chiunque, anche sotto soglia).
    last_packet_at: Option<Instant>,
}

/// Misura chi sta parlando dai pacchetti audio decodificati (`SpeakerTracker` di Python). Non
/// conserva audio: tiene solo, per ogni persona, l'ultimo volume sopra soglia.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa fa SpeakerTracker:**
/// È il "cervello" che decide chi sta parlando. Riceve pacchetti audio PCM decodificati
/// e mantiene una lista aggiornata di chi sta parlando ORA.
///
/// **NON conserva l'audio:**
/// Non registra, non salva file audio, non bufferizza. Mantiene solo metadati:
/// "Utente 123 ha parlato a volume 0.15 alle 10:30:45.123". Questo è per privacy e prestazioni.
///
/// **Thread-safety con Mutex:**
/// L'audio arriva da thread diversi (eventi Discord). `Mutex` (Mutual Exclusion = esclusione mutua)
/// è un lucchetto: solo un thread alla volta può leggere/scrivere `state`. Gli altri aspettano.
/// `MutexGuard` è il "token" che dice "hai il lucchetto", si libera automaticamente a fine scope.
///
/// **Perché Default:**
/// `#[derive(Default)]` crea un `SpeakerTracker::default()` vuoto, pronto all'uso.
#[derive(Debug, Default)]
pub struct SpeakerTracker {
    /// Stato protetto da Mutex per accesso thread-safe.
    state: std::sync::Mutex<TrackerState>,
}

impl SpeakerTracker {
    /// Ottiene l'accesso esclusivo allo stato (blocca il Mutex).
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Cosa fa `.lock()`:**
    /// Prende il "lucchetto" del Mutex. Se un altro thread ce l'ha, aspetta qui finché non lo libera.
    /// Ritorna un `MutexGuard`: finché questa variabile esiste, il lucchetto è tenuto.
    /// Quando la variabile esce dallo scope (fine della funzione), il lucchetto si libera automaticamente.
    ///
    /// **`unwrap_or_else(|poisoned| poisoned.into_inner())` - "recupero da avvelenamento":**
    /// Se un thread panica mentre tiene il lucchetto, il Mutex diventa "avvelenato" (poisoned).
    /// Normalmente `.lock()` restituirebbe `Err` in quel caso. Qui recuperiamo lo stato interno
    /// anche se avvelenato: i dati sono probabilmente ancora validi, il panico era altrove.
    fn state(&self) -> std::sync::MutexGuard<'_, TrackerState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Un pacchetto di PCM di `user`. I pacchetti di sconosciuti o vuoti non contano; quelli dei bot
    /// contano come audio in arrivo ma non come parlato.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Quando viene chiamata:**
    /// Ad ogni "tick" audio (ogni ~20ms) per ogni persona che sta mandando audio.
    /// Il parametro `user` è `None` se l'SSRC non è ancora mappato a un utente noto.
    ///
    /// **Logica di filtraggio:**
    /// 1. `user` è `None` → ignora (non sappiamo chi è)
    /// 2. `pcm` è vuoto → ignora (pacchetto vuoto/keepalive)
    /// 3. Calcola `level` (volume RMS normalizzato 0.0-1.0)
    /// 4. Aggiorna `last_packet_at` (per monitoraggio: "l'audio sta arrivando?")
    /// 5. Se `level >= SPEAKER_THRESHOLD` E non è un bot → registra come "in parlato"
    ///
    /// **Perché ignorare i bot:**
    /// I bot (musica, altri bot) mandano audio costante. Non sono "persone che parlano".
    /// Ma il loro audio conta per `last_packet_at` (sapere che l'audio fluisce).
    pub fn write(&self, user: Option<u64>, is_bot: bool, pcm: &[i16], now: Instant) {
        let Some(user) = user else { return };
        if pcm.is_empty() {
            return;
        }
        let level = pcm_level(pcm);
        let mut state = self.state();
        state.last_packet_at = Some(now);
        if level >= SPEAKER_THRESHOLD && !is_bot {
            state.levels.insert(user, (level, now));
        }
    }

    /// Chi sta parlando ora e con che volume; chi tace da più di 0,8 secondi esce.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Cosa restituisce:**
    /// Un HashMap (dizionario) User ID → volume (0.0-1.0) di chi sta parlando ADESSO.
    ///
    /// **Pulizia automatica (retain):**
    /// Prima di restituire, rimuove chi non ha parlato da più di `SPEAKER_TIMEOUT` (800ms).
    /// `retain` mantiene solo gli elementi dove la closure ritorna `true`.
    /// `now.saturating_duration_since(*last_active)` calcola il tempo trascorso senza underflow.
    ///
    /// **Perché passare `now` come parametro:**
    /// Per testabilità: nei test possiamo simulare il tempo che passa senza aspettare davvero.
    /// In produzione si passa `Instant::now()` (l'orario reale).
    pub fn active_levels(&self, now: Instant) -> std::collections::HashMap<u64, f64> {
        let mut state = self.state();
        state.levels.retain(|_, (_, last_active)| now.saturating_duration_since(*last_active) <= SPEAKER_TIMEOUT);
        state.levels.iter().map(|(user, (level, _))| (*user, *level)).collect()
    }

    /// Quando è arrivato l'ultimo pacchetto audio di qualsiasi tipo.
    /// Usato per monitoraggio: "la connessione audio è viva?".
    pub fn last_packet_at(&self) -> Option<Instant> {
        self.state().last_packet_at
    }

    /// Resetta completamente il tracker (svuota tutto).
    /// Chiamato quando si ferma l'ascolto o cambia canale.
    pub fn cleanup(&self) {
        let mut state = self.state();
        state.levels.clear();
        state.last_packet_at = None;
    }
}

// --- songbird ----------------------------------------------------------------------------------

/// Le persone (bot esclusi) collegate a un canale vocale, dalla cache del gateway.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa fa questa funzione:**
/// Legge dalla cache di Discord (dati che il bot ha già ricevuto via gateway) chi è nel canale vocale.
/// Non fa richieste di rete: usa solo dati già in memoria.
///
/// **Passaggi:**
/// 1. Prende il server (guild) dalla cache per `guild_id`
/// 2. Itera su tutti i `voice_states` (stato vocale di ogni utente nel server)
/// 3. Filtra: solo chi ha `channel_id` uguale al nostro canale
/// 4. Per ogni utente filtrato, cerca i dettagli completi in `guild.members`
/// 5. Filtra: esclude i bot (`member.user.bot == true`)
/// 6. Converte in `VoiceMember` (id, nome visualizzato, URL avatar)
/// 7. Ordina per ID per output deterministico
///
/// **Perché usare la cache:**
/// Discord invia aggiornamenti di presenza (chi entra/esce dai canali vocali) via WebSocket.
/// Il bot li riceve e li tiene in cache. Leggere la cache è istantaneo, non richiede API calls.
fn humans_in_channel(cache: &serenity::Cache, guild_id: GuildId, channel_id: ChannelId) -> Vec<VoiceMember> {
    let Some(guild) = cache.guild(guild_id) else { return Vec::new() };
    let mut members: Vec<VoiceMember> = guild
        .voice_states
        .iter()
        .filter(|(_, state)| state.channel_id == Some(channel_id))
        .filter_map(|(user, _)| guild.members.get(user))
        .filter(|member| !member.user.bot)
        .map(|member| VoiceMember { id: member.user.id.get(), name: member.display_name().to_string(), avatar_url: member.face() })
        .collect();
    members.sort_by_key(|member| member.id);
    members
}

/// Un collegamento vocale con ricezione audio (implementazione di produzione con songbird).
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cos'è SongbirdConnection:**
/// È la connessione REALE a un canale vocale Discord, usando la libreria songbird.
/// Gestisce: connessione WebRTC, decodifica audio Opus→PCM, tracciamento chi parla.
///
/// **Campi:**
/// - `cache`: accesso alla cache Discord (per vedere chi è nel canale)
/// - `manager`: il gestore songbird globale (per creare/chiudere chiamate)
/// - `guild_id` / `channel_id`: dove siamo connessi
/// - `tracker`: il SpeakerTracker che calcola chi sta parlando
/// - `ssrc_users`: mappa SSRC → User ID (chi manda quale flusso audio)
/// - `connected`: flag atomico "siamo nel canale?"
/// - `listening`: flag atomico "stiamo processando l'audio?"
///
/// **Perché Arc e AtomicBool:**
/// - `Arc` (Atomic Reference Counting): condivisione thread-safe. Più parti del codice
///   possono avere riferimenti alla stessa connessione. Quando l'ultimo Arc viene droppato,
///   la connessione viene liberata.
/// - `AtomicBool`: flag booleani leggibili/scrivibili da più thread senza Mutex (più veloce).
pub struct SongbirdConnection {
    /// Cache Discord per leggere membri, permessi, stati vocali.
    cache: Arc<serenity::Cache>,
    /// Gestore songbird globale (singleton per bot) per gestire le chiamate vocali.
    manager: Arc<Songbird>,
    /// ID del server (guild) dove siamo connessi.
    guild_id: GuildId,
    /// ID del canale vocale dove siamo connessi.
    channel_id: ChannelId,
    /// Tracker per misurare chi sta parlando dai pacchetti PCM.
    tracker: SpeakerTracker,
    /// SSRC (identificatore RTP a 32 bit) -> User ID.
    /// Popolato dagli eventi "SpeakingStateUpdate" di Discord.
    /// Serve a sapere CHI sta mandando i pacchetti audio che riceviamo.
    ssrc_users: std::sync::Mutex<std::collections::HashMap<u32, u64>>,
    /// Il bot è nel canale vocale (flag atomico, thread-safe).
    connected: AtomicBool,
    /// L'ascolto/decodifica audio è attivo (flag atomico, thread-safe).
    listening: AtomicBool,
}

impl std::fmt::Debug for SongbirdConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SongbirdConnection").field("guild", &self.guild_id).field("channel", &self.channel_id).finish_non_exhaustive()
    }
}

impl SongbirdConnection {
    /// Verifica se un utente è un bot (usando la cache).
    fn is_bot(&self, user: u64) -> bool {
        self.cache.guild(self.guild_id).and_then(|guild| guild.members.get(&UserId::new(user)).map(|member| member.user.bot)).unwrap_or(false)
    }

    /// Ottiene l'accesso esclusivo alla mappa SSRC → User ID.
    fn ssrc_users(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<u32, u64>> {
        self.ssrc_users.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Un passo di 20 ms: i pacchetti decodificati di chi sta mandando audio.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Cos'è un "VoiceTick":**
    /// Songbird invia questo evento ogni ~20ms (50 volte al secondo).
    /// Contiene i pacchetti audio decodificati (PCM) di chi sta parlando in quel momento.
    ///
    /// **Cosa fa questo metodo:**
    /// 1. Se non stiamo ascoltando (`listening == false`), esce subito (risparmio CPU)
    /// 2. Per ogni SSRC che sta mandando audio in questo tick:
    ///    - Prende i dati PCM decodificati (audio grezzo)
    ///    - Cerca l'utente corrispondente nella mappa `ssrc_users`
    ///    - Manda il pacchetto al `tracker` per calcolare il volume
    /// 3. Il tracker aggiorna chi sta parlando e con che volume
    ///
    /// **Perché `data.decoded_voice.as_deref()`:**
    /// `decoded_voice` è un `Option<Box<[i16]>>` (audio decodificato o None se non decodificabile).
    /// `.as_deref()` lo converte in `Option<&[i16]>` per passarlo a `tracker.write()`.
    fn on_tick(&self, tick: &VoiceTick, now: Instant) {
        if !self.listening.load(Ordering::SeqCst) {
            return;
        }
        for (ssrc, data) in &tick.speaking {
            let Some(pcm) = data.decoded_voice.as_deref() else { continue };
            let user = self.ssrc_users().get(ssrc).copied();
            self.tracker.write(user, user.is_some_and(|user| self.is_bot(user)), pcm, now);
        }
    }
}

impl VoiceSource for SongbirdConnection {
    /// Le persone (bot esclusi) nel canale vocale, lette dalla cache Discord.
    fn members(&self) -> Vec<VoiceMember> {
        humans_in_channel(&self.cache, self.guild_id, self.channel_id)
    }

    /// Il volume di chi sta parlando ora, per utente.
    /// Chi non parla (sotto soglia o timeout) non compare nel risultato.
    fn levels(&self) -> std::collections::HashMap<u64, f64> {
        self.tracker.active_levels(Instant::now())
    }
}

impl VoiceConnection for SongbirdConnection {
    /// Il bot è ancora nel canale (flag atomico).
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// L'ascolto dei pacchetti è attivo (flag atomico).
    fn is_listening(&self) -> bool {
        self.listening.load(Ordering::SeqCst)
    }

    /// Quando è arrivato l'ultimo pacchetto audio (delegato al tracker).
    fn last_packet_at(&self) -> Option<Instant> {
        self.tracker.last_packet_at()
    }

    /// Riavvia l'ascolto dopo un errore o pausa.
    fn restart_listening(&self) -> Result<(), VoiceError> {
        self.listening.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Ferma l'ascolto e pulisce il tracker.
    fn stop_listening(&self) {
        self.listening.store(false, Ordering::SeqCst);
        self.tracker.cleanup();
    }

    /// Lascia il canale vocale completamente.
    /// Imposta `connected = false` e chiede a songbird di chiudere la chiamata.
    fn disconnect(&self) -> BoxFuture<'_, Result<(), VoiceError>> {
        Box::pin(async move {
            self.connected.store(false, Ordering::SeqCst);
            self.manager.remove(self.guild_id).await.map_err(|e| VoiceError::Failed(e.to_string()))
        })
    }
}

/// Gli eventi del collegamento vocale che interessano al tracker.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cos'è un EventHandler in songbird:**
/// Songbird usa un sistema a eventi per notificare cosa succede nella connessione vocale.
/// Questa struct `Receiver` "avvolge" una `SongbirdConnection` e implementa `EventHandler`
/// per reagire agli eventi che ci interessano.
///
/// **Perché una struct separata (newtype pattern):**
/// `Receiver(Arc<SongbirdConnection>)` è una "newtype" - una tupla con un solo campo.
/// Permette di implementare `EventHandler` PER questo tipo specifico, senza modificare
/// `SongbirdConnection` direttamente. È un pattern Rust comune per l'estensione.

struct Receiver(Arc<SongbirdConnection>);

#[serenity::async_trait]
impl songbird::EventHandler for Receiver {
    /// Gestisce gli eventi della connessione vocale.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Eventi gestiti:**
    /// - `SpeakingStateUpdate`: Discord dice "l'utente X ha iniziato/finito di parlare" (cambio SSRC)
    /// - `VoiceTick`: Ogni 20ms, pacchetti audio decodificati pronti per il tracker
    /// - `ClientDisconnect`: Un utente ha lasciato il canale vocale (pulizia mappa SSRC)
    /// - `DriverConnect` / `DriverReconnect`: Connessione WebRTC stabilita/ristabilita
    /// - `DriverDisconnect`: Connessione WebRTC persa
    ///
    /// **Cosa NON gestiamo ( `_ => {}` ):**
    /// Altri eventi come cambi volume, mute/deafen, ecc. non servono al tracker.
    async fn act(&self, context: &EventContext<'_>) -> Option<songbird::Event> {
        match context {
            // Qualcuno ha iniziato o smesso di parlare: aggiorna mappa SSRC → User ID
            EventContext::SpeakingStateUpdate(speaking) => {
                if let Some(user) = speaking.user_id {
                    self.0.ssrc_users().insert(speaking.ssrc, user.0);
                }
            }
            // Tick audio: processa i pacchetti PCM per il tracker
            EventContext::VoiceTick(tick) => self.0.on_tick(tick, Instant::now()),
            // Utente uscito dal canale: rimuovi il suo SSRC dalla mappa
            EventContext::ClientDisconnect(left) => self.0.ssrc_users().retain(|_, user| *user != left.user_id.0),
            // Connessione WebRTC stabilita/ristabilita
            EventContext::DriverConnect(_) | EventContext::DriverReconnect(_) => self.0.connected.store(true, Ordering::SeqCst),
            // Connessione WebRTC persa
            EventContext::DriverDisconnect(_) => self.0.connected.store(false, Ordering::SeqCst),
            // Altri eventi: ignorati
            _ => {}
        }
        // Ritorna None: non vogliamo rimuovere l'handler dopo questo evento
        None
    }
}

/// Il gateway di produzione: legge canali, permessi e presenze dalla cache e si collega con songbird.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Cosa è un "Gateway" in questo contesto:**
/// Il gateway è il "ponte" tra il nostro bot e Discord. È responsabile di:
/// - Sapere se il bot è in un server (guild)
/// - Sapere quali canali vocali esistono e se il bot può entrarci
/// - Sapere in quale canale vocale si trova un utente
/// - Creare la connessione audio vera e propria (WebRTC) quando chiesto
///
/// **Perché una struct separata `SongbirdGateway`:**
/// Implementa il trait `VoiceGateway` (definito sopra). Questo pattern (trait + implementazione concreta)
/// permette di scambiare l'implementazione reale con una finta (`FakeGateway`) nei test, senza cambiare
/// il codice che usa il gateway. È il pattern "Strategy" o "Dependency Injection".
///
/// **Campi:**
/// - `cache`: La cache di Serenity (la libreria Discord per Rust). Contiene info su server, canali, utenti, permessi.
/// - `manager`: Il gestore di songbird (la libreria audio per Discord). Gestisce le connessioni vocali WebRTC.
/// - `bot_id`: L'ID numerico del bot su Discord (serve per cercare il bot nella cache).
#[derive(Clone)]
pub struct SongbirdGateway {
    cache: Arc<serenity::Cache>,
    manager: Arc<Songbird>,
    bot_id: u64,
}

impl SongbirdGateway {
    /// Crea un nuovo gateway di produzione.
    ///
    /// `Arc` (Atomic Reference Counting) permette di condividere la cache e il manager tra più parti
    /// del programma in modo sicuro (thread-safe). È come un "puntatore intelligente" che conta
    /// quanti riferimenti ci sono e libera la memoria solo quando l'ultimo riferimento sparisce.
    pub fn new(cache: Arc<serenity::Cache>, manager: Arc<Songbird>, bot_id: u64) -> Self {
        Self { cache, manager, bot_id }
    }

    /// La configurazione di songbird per questo bot: i pacchetti audio si decodificano in PCM, che
    /// serve a misurare chi parla (e non si conserva).
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Cosa è `DecodeMode::Decode`:**
    /// Quando Discord invia audio, lo manda criptato e compresso (Opus). Songbird può:
    /// 1. Passarlo "così com'è" (utile se lo devi ritrasmettere)
    /// 2. Decodificarlo in PCM (dati audio grezzi, non compressi)
    ///
    /// Noi scegliamo la decodifica (opzione 2) perché il nostro `SpeakerTracker` ha bisogno di
    /// campioni PCM (valori `i16`) per calcolare l'RMS (volume). Il PCM è l'audio "srotolato":
    /// ogni campione è un numero che rappresenta l'ampiezza dell'onda sonora in quel momento.
    ///
    /// **Nota sulla privacy:** Decodifichiamo solo per misurare il volume (RMS). Non salviamo,
    /// non registriamo, non trasmettiamo l'audio. Appena calcolato il livello, i dati PCM
    /// vengono scartati.
    pub fn config() -> songbird::Config {
        songbird::Config::default().decode_mode(DecodeMode::Decode(DecodeConfig::default()))
    }
}

impl VoiceGateway for SongbirdGateway {
    /// Controlla se il bot è presente nel server (guild) specificato.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Come funziona:**
    /// 1. Prende il server dalla cache usando `guild_id`
    /// 2. Controlla se nella lista dei membri del server c'è il bot (cercato per `bot_id`)
    /// 3. Ritorna `true` se c'è, `false` altrimenti
    ///
    /// **`is_some_and`:** Metodo comodo di `Option`. Se `guild` è `Some(guild)`, esegue la
    /// closure e ritorna il suo risultato booleano. Se è `None`, ritorna `false` direttamente.
    /// Equivale a: `guild.map(|g| g.members.contains_key(...)).unwrap_or(false)`.
    fn bot_in_guild(&self, guild_id: u64) -> bool {
        self.cache.guild(GuildId::new(guild_id)).is_some_and(|guild| guild.members.contains_key(&UserId::new(self.bot_id)))
    }

    /// Ottiene informazioni su un canale vocale: nome e se il bot può entrarci.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Cosa fa passo per passo:**
    /// 1. Cerca il server nella cache
    /// 2. Cerca il canale dentro il server
    /// 3. Cerca il bot nella lista membri del server (per calcolare i permessi)
    /// 4. Calcola i permessi del bot in quel canale specifico
    /// 5. Controlla se ha il permesso `connect` (entrare in vocale)
    ///
    /// **Il pattern `?` (operatore try):**
    /// Ogni `?` significa "se questo è `None`, ritorna subito `None` dalla funzione".
    /// È un modo pulito per gestire catene di operazioni che possono fallire senza
    /// annidare tanti `if let Some(...) = ...`.
    ///
    /// **`user_permissions_in`:** Calcola i permessi effettivi considerando ruoli, overwrites
    /// del canale, permessi del server, ecc. È la fonte di verità per "può fare X?".
    fn channel_info(&self, guild_id: u64, channel_id: u64) -> Option<VoiceChannelInfo> {
        let guild = self.cache.guild(GuildId::new(guild_id))?;
        let channel = guild.channels.get(&ChannelId::new(channel_id))?;
        let me = guild.members.get(&UserId::new(self.bot_id))?;
        Some(VoiceChannelInfo { name: channel.name.clone(), can_connect: guild.user_permissions_in(channel, me).connect() })
    }

    /// Trova in quale canale vocale si trova un utente in un dato server.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Voice States (stati vocali):**
    /// Discord traccia per ogni utente in un server: è in un canale vocale? Quale? È mutato?
    /// Sordo? In streaming? Questi sono i "voice states".
    ///
    /// **Come funziona:**
    /// 1. Prende il server dalla cache
    /// 2. Cerca lo stato vocale dell'utente (`voice_states` è una HashMap: UserId -> VoiceState)
    /// 3. Se l'utente ha un `channel_id` (cioè è in un canale), lo ritorna
    ///
    /// **`map(ChannelId::get)`:** Se c'è un `channel_id` (Some), estrae il numero `u64` grezzo.
    /// Se non c'è (None), propaga il None.
    fn user_voice_channel(&self, guild_id: u64, user_id: u64) -> Option<u64> {
        let guild = self.cache.guild(GuildId::new(guild_id))?;
        guild.voice_states.get(&UserId::new(user_id))?.channel_id.map(ChannelId::get)
    }

    /// Si connette a un canale vocale e ritorna una connessione gestita dal bot.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Il flusso completo:**
    /// 1. Se c'era già una connessione in questo server, la chiude (pulizia)
    /// 2. Chiede a songbird di unirsi al canale vocale (WebRTC handshake, DAVE encryption)
    /// 3. Crea una `SongbirdConnection` nuova con tracker, mappe SSRC, flag atomici
    /// 4. Registra gli event handler per: chi parla, tick audio, utenti che escono, stato connessione
    /// 5. Ritorna la connessione incapsulata in `Arc<dyn VoiceConnection>` (type erasure)
    ///
    /// **Perché chiudere la connessione precedente (righe 717-721):**
    /// Se il bot era già in vocale (anche nello stesso canale), la vecchia connessione
    /// ha il suo `SpeakerTracker` e la sua mappa SSRC. Ne creiamo una nuova pulita per
    /// evitare dati vecchi. `manager.remove(guild)` disconnette pulitamente.
    ///
    /// **`call.lock().await`:** La connessione di songbird (`Call`) è protetta da un Mutex
    /// perché può essere usata da più task asincroni contemporaneamente. Il `.await`
    /// significa che questa funzione è `async` e può "pausare" mentre aspetta il lock.
    ///
    /// **Eventi registrati (righe 735-742):**
    /// - `SpeakingStateUpdate`: Discord avvisa "utente X ha iniziato/finito di parlare" (cambio SSRC)
    /// - `VoiceTick`: Ogni ~20ms, pacchetti audio decodificati pronti per il tracker
    /// - `ClientDisconnect`: Un utente ha lasciato il canale vocale
    /// - `DriverConnect`/`DriverReconnect`: Connessione WebRTC stabilita/ristabilita
    /// - `DriverDisconnect`: Connessione WebRTC persa
    ///
    /// **`Receiver(connection.clone())`:** Crea un nuovo handler per ogni evento, tutti
    /// puntano alla stessa `Arc<SongbirdConnection>` (condivisione thread-safe).
    async fn connect(&self, guild_id: u64, channel_id: u64) -> Result<Arc<dyn VoiceConnection>, VoiceError> {
        let guild = GuildId::new(guild_id);
        let channel = ChannelId::new(channel_id);
        // Un collegamento che c'era già (anche nello stesso canale) si lascia: il nuovo ha il suo tracker.
        if self.manager.get(guild).is_some() {
            if let Err(e) = self.manager.remove(guild).await {
                warn!("Impossibile lasciare il collegamento vocale precedente del server {guild_id}: {e}");
            }
        }
        let call = self.manager.join(guild, channel).await.map_err(|e| VoiceError::Failed(e.to_string()))?;
        let connection = Arc::new(SongbirdConnection {
            cache: self.cache.clone(),
            manager: self.manager.clone(),
            guild_id: guild,
            channel_id: channel,
            tracker: SpeakerTracker::default(),
            ssrc_users: std::sync::Mutex::default(),
            connected: AtomicBool::new(true),
            listening: AtomicBool::new(true),
        });
        {
            let mut handler = call.lock().await;
            for event in [
                CoreEvent::SpeakingStateUpdate,
                CoreEvent::VoiceTick,
                CoreEvent::ClientDisconnect,
                CoreEvent::DriverConnect,
                CoreEvent::DriverReconnect,
                CoreEvent::DriverDisconnect,
            ] {
                handler.add_global_event(event.into(), Receiver(connection.clone()));
            }
        }
        Ok(connection)
    }
}

/// I finti per i test: registrano le chiamate come fa il finto `VoiceRecvClient` dei golden Python.
///
/// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
///
/// **Perché moduli "fake" (mock/stub) nei test:**
/// Quando testiamo il codice che usa la voce, non vogliamo:
/// - Connetterci davvero a Discord (lento, richiede rete, token, account)
/// - Dipendere da songbird/WebRTC (complesso, non deterministico)
///
/// Invece creiamo "doppi" (fake) che:
/// - Simulano il comportamento atteso
/// - Registrano quali metodi sono stati chiamati (`CallLog`)
/// - Possono essere configurati per fallire o riuscire a comando
/// - Sono veloci, deterministici, isolati
///
/// **`#[cfg(test)]`:** Questo modulo viene compilato SOLO durante `cargo test`.
/// In produzione (`cargo build --release`) non esiste, non occupa spazio, non ha costo.
///
/// **`pub(crate)`:** Visibile dentro tutto il crate (il pacchetto Rust), ma non all'esterno.
/// I test in altri file dello stesso crate possono usare `FakeGateway`, `FakeConnection`, ecc.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::overlay::VoiceMember;

    /// Helper per lockare un Mutex gestendo il "poisoning".
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Mutex Poisoning:**
    /// Se un thread fa panic mentre tiene un Mutex lockato, il Mutex diventa "avvelenato".
    /// Qualsiasi altro thread che prova a lockarlo riceve un `Err` invece di bloccarsi.
    /// Questo previene la corruzione silenziosa dei dati.
    ///
    /// **`unwrap_or_else(|poisoned| poisoned.into_inner())`:**
    /// Se il lock riesce → `Ok(guard)`, ritorna il guard.
    /// Se è avvelenato → `Err(poisoned)`, chiama `into_inner()` che recupera i dati
    /// ignorando il veleno (i dati sono ancora lì, solo il flag "poisoned" è attivo).
    /// Nei test è accettabile: se un test panica, non ci importa proseguire pulitamente.
    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Il registro delle chiamate: `connect`, `listen`, `stop_listening`, `disconnect`.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **`CallLog = Arc<Mutex<Vec<String>>>`:**
    /// - `Vec<String>`: Lista di nomi di metodi chiamati (es. `["connect", "listen", "disconnect"]`)
    /// - `Mutex<...>`: Protegge l'accesso concorrente (più thread potrebbero chiamare metodi insieme)
    /// - `Arc<...>`: Condivisione thread-safe tra `FakeGateway` e `FakeConnection` clonati
    ///
    /// Serve ai test per verificare: "è stato chiamato `connect`? poi `listen`? poi `disconnect`?"
    pub(crate) type CallLog = Arc<Mutex<Vec<String>>>;

    /// Connessione vocale finta per i test.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Campi pubblici (pub):**
    /// Nei test è comodo accedere direttamente ai campi per configurarli (`conn.members.lock()...`).
    /// In codice di produzione si userebbero metodi setter privati, ma qui la semplicità vince.
    ///
    /// **Campi:**
    /// - `members`: Lista di `VoiceMember` (chi è nel canale)
    /// - `levels`: Mappa UserId -> volume corrente (chi sta parlando e quanto)
    /// - `connected`: Flag atomico, true = connesso
    /// - `listening`: Flag atomico, true = sta ricevendo audio
    /// - `last_packet`: Quando è arrivato l'ultimo pacchetto audio (per timeout)
    /// - `disconnect_error`: Se impostato, `disconnect()` fallisce con questo errore
    /// - `restart_error`: Se impostato, `restart_listening()` fallisce con questo errore
    /// - `log`: Registro condiviso delle chiamate fatte
    #[derive(Debug)]
    pub(crate) struct FakeConnection {
        pub members: Mutex<Vec<VoiceMember>>,
        pub levels: Mutex<HashMap<u64, f64>>,
        pub connected: AtomicBool,
        pub listening: AtomicBool,
        pub last_packet: Mutex<Option<Instant>>,
        pub disconnect_error: Mutex<Option<VoiceError>>,
        pub restart_error: Mutex<Option<VoiceError>>,
        log: CallLog,
    }

    impl FakeConnection {
        /// Imposta la lista di membri nel canale (per ID numerico).
        pub(crate) fn set_members(&self, ids: &[u64]) {
            *lock(&self.members) = ids.iter().map(|id| member(*id)).collect();
        }
        /// Imposta i livelli volume per utente (per simulare chi parla).
        pub(crate) fn set_levels(&self, levels: &[(u64, f64)]) {
            *lock(&self.levels) = levels.iter().copied().collect();
        }
    }

    /// Crea un `VoiceMember` finto per test (nome "utente{id}", avatar finto).
    pub(crate) fn member(id: u64) -> VoiceMember {
        VoiceMember { id, name: format!("utente{id}"), avatar_url: format!("https://cdn/{id}.png") }
    }

    /// Implementazione di `VoiceSource` per `FakeConnection`: fornisce membri e livelli.
    impl VoiceSource for FakeConnection {
        fn members(&self) -> Vec<VoiceMember> {
            lock(&self.members).clone()
        }
        fn levels(&self) -> HashMap<u64, f64> {
            lock(&self.levels).clone()
        }
    }

    /// Implementazione di `VoiceConnection` per `FakeConnection`: simula il ciclo di vita.
    impl VoiceConnection for FakeConnection {
        /// True se la connessione è attiva.
        fn is_connected(&self) -> bool {
            self.connected.load(Ordering::SeqCst)
        }
        /// True se sta ricevendo audio.
        fn is_listening(&self) -> bool {
            self.listening.load(Ordering::SeqCst)
        }
        /// Quando è arrivato l'ultimo pacchetto (per logica di timeout).
        fn last_packet_at(&self) -> Option<Instant> {
            *lock(&self.last_packet)
        }
        /// Riprende l'ascolto: registra la chiamata, fallisce se `restart_error` è impostato.
        fn restart_listening(&self) -> Result<(), VoiceError> {
            lock(&self.log).push("listen".into());
            match lock(&self.restart_error).clone() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
        /// Ferma l'ascolto: registra solo la chiamata.
        fn stop_listening(&self) {
            lock(&self.log).push("stop_listening".into());
        }
        /// Si disconnette: asincrono, registra, imposta connected=false, fallisce se `disconnect_error` impostato.
        fn disconnect(&self) -> BoxFuture<'_, Result<(), VoiceError>> {
            Box::pin(async move {
                if let Some(error) = lock(&self.disconnect_error).clone() {
                    return Err(error);
                }
                lock(&self.log).push("disconnect".into());
                self.connected.store(false, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    /// Stato interno del gateway finto: cosa sa del "mondo" simulato.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Campi:**
    /// - `bot_in_guild`: Il bot è nel server? (controllato da `bot_in_guild`)
    /// - `channels`: Mappa (guild_id, channel_id) -> info canale (nome, può connettersi?)
    /// - `user_channels`: Mappa (guild_id, user_id) -> channel_id (dove sta l'utente)
    /// - `connect_error`: Se impostato, `connect()` fallisce subito con questo errore
    /// - `listen_error`: Se impostato, `connect()` riesce ma poi fallisce ad avviare l'ascolto
    ///   (simula: WebRTC si connette ma audio non parte → il gateway si disconnette pulitamente)
    /// - `members`: Lista di user_id che saranno nel canale quando il bot si collega
    /// - `connections`: Lista delle `FakeConnection` create (per ispezionarle nei test)
    #[derive(Default)]
    struct GatewayState {
        bot_in_guild: bool,
        channels: HashMap<(u64, u64), VoiceChannelInfo>,
        user_channels: HashMap<(u64, u64), u64>,
        connect_error: Option<VoiceError>,
        /// Il collegamento riesce ma l'avvio dell'ascolto no: il gateway si scollega e fallisce.
        listen_error: Option<VoiceError>,
        members: Vec<u64>,
        connections: Vec<Arc<FakeConnection>>,
    }

    /// Gateway finto per i test: simula `VoiceGateway` senza rete né Discord.
    ///
    /// --- SPIEGAZIONE PER NON PROGRAMMATORI ---
    ///
    /// **Perché `Clone` per `FakeGateway`:**
    /// `Arc<Mutex<GatewayState>>` e `CallLog` (che è `Arc<Mutex<Vec<String>>>`) sono entrambi
    /// `Clone` economici (clonano solo il puntatore Arc, non i dati). Questo permette di
    /// passare il gateway per valore, condividendo lo stesso stato interno.
    ///
    /// **`pub log`:** Il registro delle chiamate è pubblico così i test possono leggerlo
    /// direttamente: `gateway.calls()` o `gateway.log.lock()...`.
    #[derive(Clone)]
    pub(crate) struct FakeGateway {
        state: Arc<Mutex<GatewayState>>,
        pub log: CallLog,
    }

    impl Default for FakeGateway {
        fn default() -> Self {
            // Di default il bot è nel server (bot_in_guild: true)
            Self { state: Arc::new(Mutex::new(GatewayState { bot_in_guild: true, ..GatewayState::default() })), log: CallLog::default() }
        }
    }

    impl FakeGateway {
        /// Imposta se il bot è presente nel server.
        pub(crate) fn set_bot_in_guild(&self, present: bool) {
            lock(&self.state).bot_in_guild = present;
        }
        /// Aggiunge/aggiorna un canale vocale simulato.
        pub(crate) fn add_channel(&self, guild_id: u64, channel_id: u64, name: &str, can_connect: bool) {
            lock(&self.state).channels.insert((guild_id, channel_id), VoiceChannelInfo { name: name.into(), can_connect });
        }
        /// Imposta in quale canale si trova un utente.
        pub(crate) fn set_user_channel(&self, guild_id: u64, user_id: u64, channel_id: u64) {
            lock(&self.state).user_channels.insert((guild_id, user_id), channel_id);
        }
        /// Fa fallire `connect()` con l'errore dato.
        pub(crate) fn fail_connect(&self, error: VoiceError) {
            lock(&self.state).connect_error = Some(error);
        }
        /// Fa fallire l'avvio dell'ascolto dopo `connect()` riuscito.
        pub(crate) fn fail_listen(&self, error: VoiceError) {
            lock(&self.state).listen_error = Some(error);
        }
        /// Chi è nel canale quando il bot si collega (solo persone).
        pub(crate) fn set_members(&self, ids: &[u64]) {
            lock(&self.state).members = ids.to_vec();
        }
        /// Ritorna la lista delle chiamate registrate.
        pub(crate) fn calls(&self) -> Vec<String> {
            lock(&self.log).clone()
        }
        /// Svuota il registro chiamate.
        pub(crate) fn clear_calls(&self) {
            lock(&self.log).clear();
        }
        /// Un collegamento che non appartiene a nessuna sessione (per test di edge case).
        pub(crate) fn detached_connection(&self) -> Arc<dyn VoiceConnection> {
            Arc::new(FakeConnection {
                members: Mutex::default(),
                levels: Mutex::default(),
                connected: AtomicBool::new(true),
                listening: AtomicBool::new(false),
                last_packet: Mutex::new(None),
                disconnect_error: Mutex::new(None),
                restart_error: Mutex::new(None),
                log: self.log.clone(),
            })
        }
        /// L'ultima connessione creata da `connect()`.
        pub(crate) fn connection(&self) -> Arc<FakeConnection> {
            lock(&self.state).connections.last().cloned().expect("nessun collegamento")
        }
    }

    /// Implementazione di `VoiceGateway` per `FakeGateway`: delega allo stato interno.
    impl VoiceGateway for FakeGateway {
        fn bot_in_guild(&self, _guild_id: u64) -> bool {
            lock(&self.state).bot_in_guild
        }
        fn channel_info(&self, guild_id: u64, channel_id: u64) -> Option<VoiceChannelInfo> {
            lock(&self.state).channels.get(&(guild_id, channel_id)).cloned()
        }
        fn user_voice_channel(&self, guild_id: u64, user_id: u64) -> Option<u64> {
            lock(&self.state).user_channels.get(&(guild_id, user_id)).copied()
        }
        async fn connect(&self, _guild_id: u64, _channel_id: u64) -> Result<Arc<dyn VoiceConnection>, VoiceError> {
            lock(&self.log).push("connect".into());
            // Prende errori e membri dallo stato (sotto lock breve), poi lavora fuori dal lock.
            let (connect_error, listen_error, members) = {
                let state = lock(&self.state);
                (state.connect_error.clone(), state.listen_error.clone(), state.members.clone())
            };
            // Errore di connessione immediato
            if let Some(error) = connect_error {
                return Err(error);
            }
            // Simula avvio ascolto
            lock(&self.log).push("listen".into());
            if let Some(error) = listen_error {
                // Se l'ascolto fallisce, il gateway si disconnette pulitamente
                lock(&self.log).push("disconnect".into());
                return Err(error);
            }
            // Crea la connessione finta con i membri configurati
            let connection = Arc::new(FakeConnection {
                members: Mutex::new(members.iter().map(|id| member(*id)).collect()),
                levels: Mutex::default(),
                connected: AtomicBool::new(true),
                listening: AtomicBool::new(true),
                last_packet: Mutex::new(None),
                disconnect_error: Mutex::new(None),
                restart_error: Mutex::new(None),
                log: self.log.clone(),
            });
            // Memorizza per ispezione nei test
            lock(&self.state).connections.push(connection.clone());
            Ok(connection)
        }
    }
}

#[cfg(test)]
mod tests;
