//! Embed con i colori semantici del bot (equivalente di `cogs/embeds.py`).
//!
//! I embed sono le "schede" che appaiono nei messaggi Discord con titolo,
//! descrizione e colore. Questo modulo definisce i colori semantici per
//! dare feedback visivo immediato all'utente.
//!
//! # Panoramica del modulo
//!
//! Questo è un modulo di utility (helper) per creare embed coerenti.
//! Gli stili di colori verranno usati anche da componenti aggiunti più avanti.
//!
//! # Colori semantici
//!
//! - **Success (verde)**: azioni completate con successo
//! - **Error (rosso)**: errori o cancellazioni
//! - **Info (blu)**: informazioni o domande
//!
//! # Formato dei timestamp
//!
//! I bot Discord possono mostrare timestamp che si aggiornano in tempo reale.
//! Questo modulo fornisce helper per creare questi timestamp nel formato
//! corretto di Discord.
//!
//! # Esempio di embed
//!
//! ```text
//! Titolo: "Operazione completata"
//! Descrizione: "I punti sono stati azzerati."
//! Colore: Verde (0x2ECC71)
//! ```
// Libreria di helper: parte delle funzioni verrà usata solo dai cog portati più avanti.
#![allow(dead_code)]

use chrono::{DateTime, TimeZone};
use poise::serenity_prelude::CreateEmbed;

/// Tipo di embed basato sul significato, determina il colore asociato.
///
/// I colori seguono le convenzioni standard di UI/UX:
/// - Verde (SUCCESS) = positivo, completato
/// - Rosso (ERROR) = negativo, errore/cancel
/// - Blu (INFO) = neutro, informazione
///
/// Questi colori sono le stessi usati in Python/Sass (`@tailwindcss/forms` e design system comuni).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedKind {
    /// Azione completata con successo. Colore verde brillante.
    Success,
    /// Errore o operazione annullata. Colore rosso acceso.
    Error,
    /// Informazione o domanda. Colore blu.
    Info,
}

impl EmbedKind {
    /// Restituisce il colore HTTP (RGB) per questo tipo di embed.
    ///
    /// I colori sono in formato esadecimale senza trattino:
    /// - `0x2ECC71` = verde (6509585)
    /// - `0xE74C3C` = rosso (15258140)
    /// - `0x3498DB` = blu (3430523)
    ///
    /// Questi sono i colori ufficiali di Discord per gli stati.
    pub const fn color(self) -> u32 {
        match self {
            EmbedKind::Success => 0x2ECC71,
            EmbedKind::Error => 0xE74C3C,
            EmbedKind::Info => 0x3498DB,
        }
    }
}

/// Stili di timestamp Discord.
///
/// Discord supporta diversi formati per i timestamp, tutti nella forma `<t:timestamp:style>`.
/// Gli stili a breve mostrano ora/giorno, quelli lunghi mostrano giorno/mese/anno.
/// Lo stile "Relative" mostra "2 ore fa" o "tra 3 giorni".
///
/// # Codici Discord
///
/// | Stile | Codice | Esempio |
/// |-------|--------|---------|
/// | ShortTime | 't' | 12:00 PM |
/// | LongTime | 'T' | 12:00:00 PM |
/// | ShortDate | 'd' | Jan 1, 2026 |
/// | LongDate | 'D' | January 1, 2026 |
/// | ShortDateTime | 'f' | January 1, 2026 12:00 PM |
/// | LongDateTime | 'F' | Saturday, January 1, 2026 12:00 PM |
/// | Relative | 'R' | 2 hours ago |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampStyle {
    /// Ora singola: 12:00 PM
    ShortTime,
    /// Ora con secondi: 12:00:00 PM
    LongTime,
    /// Data singola: Jan 1, 2026
    ShortDate,
    /// Data completa: January 1, 2026
    LongDate,
    /// Data e ora breve: January 1, 2026 12:00 PM
    ShortDateTime,
    /// Data e ora lunga: Saturday, January 1, 2026 12:00 PM
    LongDateTime,
    /// Relativo: "2 hours ago", "in 3 days" (aggiornabile in tempo reale)
    Relative,
}

impl TimestampStyle {
    /// Restituisce il codice carattere per questo stile di timestamp.
    ///
    /// Questi codici sono formati letterali che Discord legge per interpretare
    /// il timestamp come `<t:unix_timestamp:codice>`.
    ///
    /// # Esempio
    ///
    /// ```text
    /// timestamp: 1794826800
    /// style: Relative
    /// risultato: "2 hours ago"
    ///
    /// Il testo generato: <t:1794826800:R>
    /// ```
    pub const fn code(self) -> char {
        match self {
            TimestampStyle::ShortTime => 't',
            TimestampStyle::LongTime => 'T',
            TimestampStyle::ShortDate => 'd',
            TimestampStyle::LongDate => 'D',
            TimestampStyle::ShortDateTime => 'f',
            TimestampStyle::LongDateTime => 'F',
            TimestampStyle::Relative => 'R',
        }
    }
}

/// Crea un timestamp Discord formattato come markdown.
///
/// Genera la stringa `<t:timestamp:style>` che Discord renderà come timestamp
/// interattivo. L'utente vedrà il timestamp nel proprio fuso orario.
///
/// # Formato
///
/// ```text
/// Input: value=2026-11-16 12:00:00 UTC, style=Relative
/// Output: "<t:1794826800:R>"
/// Visualizzato da Discord: "in 2 ore" (aggiornabile)
/// ```
///
/// # Perché usare i macro di Discord?
///
/// - I timestamp si aggiornano automaticamente nel tempo
/// - Vengono mostrati nel fuso orario dell'utente che li vede
/// - Supportano vari formati (breve/lungo/relativo)
///
/// # Esempio d'uso
///
/// ```text
/// let quando = chrono::Utc::now();
/// let testo = format!("Il reset è {}", discord_timestamp(&quando, TimestampStyle::LongDateTime));
/// // Mostrerebbe: "Il reset è giovedì 16 novembre 2026, ore 12:00:00"
/// ```
pub fn discord_timestamp<Tz: TimeZone>(value: &DateTime<Tz>, style: TimestampStyle) -> String {
    format!("<t:{}:{}>", value.timestamp(), style.code())
}

/// Crea un timestamp relativo Unix.
///
/// Questa è la forma più comune: `<t:unix:R>` per "relative time".
/// Molto usata nel bot per mostrare scadenze e orari.
///
/// # Esempio
///
/// ```text
/// let unix = 1794826800;
/// let testo = discord_timestamp_for(unix);
/// // Output: "<t:1794826800:R>"
/// // Discord mostrerà: "in 3 giorni" o "2 ore fa"
/// ```
pub fn discord_timestamp_for(unix_seconds: i64) -> String {
    format!("<t:{unix_seconds}:{}>", TimestampStyle::Relative.code())
}

/// Crea un embed vuoto con il colore semantico appropriato.
///
/// Questa è la funzione base, gli altri helper (`success`, `error`, `info`)
/// costruiscono su questa.
///
/// # Esempio
///
/// ```text
/// let mut e = embed(EmbedKind::Success);
/// e.title("Fatto!").description("Operazione completata.");
/// ```
pub fn embed(kind: EmbedKind) -> CreateEmbed {
    CreateEmbed::new().colour(kind.color())
}

// Crea un embed con titolo e descrizione.
//
// Funzione interna usata dagli helper `success`, `error`, `info`.
// Se la descrizione è vuota, non viene aggiunta al messaggio.
//
// # Ottimizzazione Discord
//
// Discord non mostra bene gli embed vuoti (senza descrizione).
// Questa funzione evita di aggiungere `description: ""` che
// potrebbe causare problemi di rendering.
fn titled(kind: EmbedKind, title: &str, description: &str) -> CreateEmbed {
    let embed = embed(kind).title(title);
    if description.is_empty() {
        embed
    } else {
        embed.description(description)
    }
}

/// Crea un embed per un'azione completata con successo.
///
/// Colore verde brillante (0x2ECC71). Usato per:
/// - Conferme di operazioni
/// - Aggiornamenti riusciti
/// - Messaggi positivi
///
/// # Esempio
///
/// ```text
/// success("Reset completato", "I punti sono stati azzerati.")
/// ```
pub fn success(title: &str, description: &str) -> CreateEmbed {
    titled(EmbedKind::Success, title, description)
}

/// Crea un embed per un errore o cancellazione.
///
/// Colore rosso acceso (0xE74C3C). Usato per:
/// - Errori di sistema
/// - Operazioni annullate
/// - Messaggi di errore
///
/// # Esempio
///
/// ```text
/// error("Errore", "Non hai i permessi per questa azione.")
/// ```
pub fn error(title: &str, description: &str) -> CreateEmbed {
    titled(EmbedKind::Error, title, description)
}

/// Crea un embed per informazioni.
///
/// Colore blu (0x3498DB). Usato per:
/// - Domande
/// - Informazioni
/// - Promemori
///
/// # Esempio
///
/// ```text
/// info("Prossimo reset", "Il prossimo reset è lunedì alle 12:00.")
/// ```
pub fn info(title: &str, description: &str) -> CreateEmbed {
    titled(EmbedKind::Info, title, description)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn timestamp_formatter_uses_discord_relative_markdown() {
        // Stesso caso di tests/test_platform_data_cache.py.
        let value = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(
            discord_timestamp(&value, TimestampStyle::Relative),
            "<t:1767225600:R>"
        );
    }

    #[test]
    fn timestamp_style_codes_match_discord() {
        let value = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(
            discord_timestamp(&value, TimestampStyle::LongDateTime),
            "<t:1767225600:F>"
        );
    }

    #[test]
    fn semantic_embeds_carry_their_colour_and_text() {
        let json = serde_json::to_value(error("Titolo", "Testo")).unwrap();
        assert_eq!(json["color"], 0xE74C3C);
        assert_eq!(json["title"], "Titolo");
        assert_eq!(json["description"], "Testo");
        assert_eq!(serde_json::to_value(success("a", "")).unwrap()["color"], 0x2ECC71);
        assert_eq!(serde_json::to_value(info("a", "")).unwrap()["color"], 0x3498DB);
    }

    #[test]
    fn empty_description_is_omitted() {
        let json = serde_json::to_value(info("Solo titolo", "")).unwrap();
        assert!(json.get("description").is_none_or(|v| v.is_null()));
    }
}
