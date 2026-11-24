//! `date.fromisoformat` e `time.fromisoformat` di Python 3.12.
//!
//! Python accetta più forme di quelle che ci si aspetta (`20270305`, `2027-W09-5`, `21`, `2100`,
//! `T21:00`, `21.5`, un fuso dopo uno spazio…): le regole sono le sue, ricalcate dall'implementazione
//! in C, perché i report del sito salvano la data così com'è stata scritta e i dati devono essere
//! quelli che Python accetterebbe.
//!
//! Per chi non è programmatore: questo file serve a leggere date e orari scritti nel formato
//! usato da Python (e da strumenti basati su Python). Il bot deve capire le date che i
//! report del sito salvano, anche se sono in formati diversi da "standard".

use chrono::{Datelike, NaiveDate, Weekday};

/// Struttura che rappresenta un orario letto da `time.fromisoformat`
///
/// Per chi non è programmatore: contiene tutti gli elementi di un orario:
/// - ore (0-23)
/// - minuti (0-59)
/// - secondi (0-59)
/// - microsecondi (0-999999)
/// - offset fuso (se l'orario ha un fuso, es. +02:00)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoTime {
    /// Ore del giorno (0-23)
    pub hour: u32,
    /// Minuti dell'ora (0-59)
    pub minute: u32,
    /// Secondi del minuto (0-59)
    pub second: u32,
    /// Microsecondi (0-999999) - le parti decimali dei secondi
    pub microsecond: u32,
    /// Differenza dal fuso orario UTC, in microsecondi
    /// - `None` = nessun fuso indicato (ora "locale")
    /// - `Some(0)` = fuso UTC (es. "Z")
    /// - `Some(+/- valore)` = offset rispetto a UTC
    pub offset_micros: Option<i64>,
}

/// I byte del testo, con un NUL «virtuale» dopo la fine (come la stringa C).
///
/// Per chi non è programmatore: questo è un "wrapper" (un'incapsulazione) per leggere
/// i caratteri di una stringa uno alla volta, anche se si tenta di leggere oltre la fine.
/// È come se la stringa avesse un carattere nastro (NUL, valore 0) alla fine e all'infinito.
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    /// Legge il byte all'indice dato, ritornando 0 se l'indice è fuori dai limiti.
    /// Questo è un semplificatore per non dover controllare ogni volta se l'indice è valido.
    fn at(&self, index: usize) -> u8 {
        // .get(index) = "prendi l'elemento all'indice, se c'è"
        // .copied() = "trasforma l'opzionale in un valore"
        // .unwrap_or(0) = "se c'è l'errore (indice fuori), ritorna 0"
        // Ritorna il byte letto o 0 se oltre la fine della stringa
        self.0.get(index).copied().unwrap_or(0)
    }
}

/// `parse_digits`: esattamente `count` cifre ASCII da `start`.
///
/// Per chi non è programmatore: legge esattamente N cifre numeriche da una posizione della stringa
/// e le converte in un numero. Se trova un carattere non numerico o finisce la stringa,
/// ritorna `None` (fallimento).
///
/// Esempio: parse_digits("12:30", 0, 2) ritorna Some((12, 2) - il numero 12 e la posizione dopo)
fn parse_digits(text: &Bytes, start: usize, count: usize) -> Option<(u32, usize)> {
    let mut value = 0u32;  // Il numero che stai costruendo, inizialmente 0

    // Ciclo per leggere ogni cifra
    for offset in 0..count {
        // Leggi il byte (carattere) alla posizione corrente
        let byte = text.at(start + offset);

        // Controlla se è una cifra (0-9) e se non siamo oltre la fine
        if !byte.is_ascii_digit() || start + offset >= text.0.len() {
            return None;  // Errore: non c'è la cifra attesa
        }

        // Converte il carattere '0'..'9' in un numero 0..9
        // '0' ha valore numerico 48 in ASCII, quindi byte - b'0' = 0
        // Poi aggiungi la nuova cifra al numero esistente: valore * 10 + nuova_cifra
        value = value * 10 + u32::from(byte - b'0');
    }

    // Ritorna il numero letto e la posizione successiva a quelle lette
    Some((value, start + count))
}

/// `parse_hh_mm_ss_ff`: restituisce `Ok(extra)` (se c'è altro oltre l'orario, 1) o `Err(())`.
/// `end` è dove finisce la parte oraria (l'inizio del fuso, o la fine del testo).
///
/// Per chi non è programmatore: legge ore, minuti, secondi e microsecondi da una stringa.
/// Gestisce anche il separatore ":", ".", "," e la mancanza di separatore.
/// Ritorna Ok((ore, minuti, secondi, microsecondi), c'è_altro) oppure Err(())
fn parse_hh_mm_ss_ff(text: &Bytes, start: usize, end: usize) -> Result<([u32; 4], bool), ()> {
    // Array con 4 elementi: [ore, minuti, secondi, microsecondi]
    let mut values = [0u32; 4];
    // p = posizione corrente nella lettura
    let mut p = start;
    // has_separator = true significa che ci aspettiamo un separatore ":" prima delle cifre
    let mut has_separator = true;
    // broke_for_fraction = true se abbiamo trovato un separatore frazionario "." o ","
    let mut broke_for_fraction = false;

    // Leggi ore, minuti, secondi (3 cicli, una volta per ogni unità temporale)
    for index in 0..3 {
        // Leggi 2 cifre alla volta (es. "14" per le ore)
        let (value, next) = parse_digits(text, p, 2).ok_or(())?;
        values[index] = value;  // Salva il valore letto
        p = next;  // Avanza la posizione

        // Leggi il carattere che segue le due cifre
        let c = text.at(p);
        p += 1;

        // Prima ciclo (ore): controlla se c'è un separator ":"
        if index == 0 {
            has_separator = c == b':';  // true se c'è ":", false se c'è "." o altro
        }

        // Se siamo arrivati alla fine della parte oraria...
        if p >= end {
            // Ritorna il risultato e dice se c'è altro dopo (c != 0 significa c'è un carattere)
            return Ok((values, c != 0));
        }
        // Se c'è un separatore ":" e ci aspettiamo separatori...
        else if has_separator && c == b':' {
            continue;  // Salta al prossimo ciclo, leggendo i minuti/secondi
        }
        // Se troviamo un separatore frazionario "." o ","
        else if c == b'.' || c == b',' {
            broke_for_fraction = true;  // Segna che siamo passati alle microsecondi
            break;  // Esce dal ciclo, salta alla parte delle microsecondi
        }
        // Se non c'era separatore e non troviamo nulla...
        else if !has_separator {
            p -= 1;  // Torna indietro di una posizione (abbiamo letto un carattere inutile)
        }
        // Qualcosa di imprevisto
        else {
            return Err(());  // Fallimento: formato non riconosciuto
        }
    }

    // Ignora la variabile broke_for_fraction (non serve, ma serve per la logica)
    let _ = broke_for_fraction;

    // --- Parte frazionaria: microsecondi ---
    // Calcola quante cifre ci sono per le microsecondi (massimo 6)
    let remaining = end.saturating_sub(p);  // saturend_sub evita numeri negativi
    let to_parse = remaining.min(6);  // Leggi al massimo 6 cifre

    // Leggi le cifre delle microsecondi
    let (mut micros, mut next) = parse_digits(text, p, to_parse).ok_or(())?;

    // Tabella di moltiplicazione per estendere le cifre mancanti
    // Se leggi "21" invece di "210000", moltiplica per 100_000
    const CORRECTION: [u32; 5] = [100_000, 10_000, 1_000, 100, 10];
    if to_parse < 6 {
        if to_parse == 0 {
            return Err(());  // Nessuna cifra: errore
        }
        // Moltiplica per estendere le cifre letti
        micros *= CORRECTION[to_parse - 1];
    }

    // Salta eventuali cifre in eccesso (es. "21.123456789")
    while text.at(next).is_ascii_digit() {
        next += 1;
    }

    // Salva i microsecondi
    values[3] = micros;

    // Ritorna i valori e dice se c'è qualcosa dopo
    Ok((values, text.at(next) != 0))
}

/// `time.fromisoformat(testo)`; `None` dove Python solleva `ValueError`.
pub fn parse_time(raw: &str) -> Option<IsoTime> {
    let mut bytes = raw.as_bytes();
    if bytes.first() == Some(&b'T') {
        bytes = &bytes[1..];
    }
    let text = Bytes(bytes);
    let length = bytes.len();
    let tz_start = bytes.iter().position(|byte| matches!(byte, b'Z' | b'+' | b'-')).unwrap_or(length);
    let (values, extra) = parse_hh_mm_ss_ff(&text, 0, tz_start).ok()?;
    let offset_micros = if tz_start == length {
        if extra {
            return None;
        }
        None
    } else if bytes[tz_start] == b'Z' {
        if tz_start + 1 != length {
            return None;
        }
        Some(0)
    } else {
        let sign: i64 = if bytes[tz_start] == b'-' { -1 } else { 1 };
        let (tz, tz_extra) = parse_hh_mm_ss_ff(&text, tz_start + 1, length).ok()?;
        if tz_extra {
            return None;
        }
        let micros = (i64::from(tz[0]) * 3600 + i64::from(tz[1]) * 60 + i64::from(tz[2])) * 1_000_000 + i64::from(tz[3]);
        Some(sign * micros)
    };
    let [hour, minute, second, microsecond] = values;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    // Il fuso deve restare entro le 24 ore.
    if offset_micros.is_some_and(|micros| micros.abs() >= 24 * 3600 * 1_000_000) {
        return None;
    }
    Some(IsoTime { hour, minute, second, microsecond, offset_micros })
}

/// `date.fromisoformat(testo)`: la data, se è valida.
pub fn parse_date(raw: &str) -> Option<NaiveDate> {
    let bytes = raw.as_bytes();
    let length = bytes.len();
    if !matches!(length, 7 | 8 | 10) {
        return None;
    }
    let text = Bytes(bytes);
    let (year, mut p) = parse_digits(&text, 0, 4)?;
    let uses_separator = text.at(p) == b'-';
    if uses_separator {
        p += 1;
    }
    let (year, month, day) = if text.at(p) == b'W' {
        p += 1;
        let (week, next) = parse_digits(&text, p, 2)?;
        p = next;
        let day = if p < length {
            if uses_separator {
                let separator = text.at(p);
                p += 1;
                if separator != b'-' {
                    return None;
                }
            }
            parse_digits(&text, p, 1)?.0
        } else {
            1
        };
        let weekday = match day {
            1 => Weekday::Mon,
            2 => Weekday::Tue,
            3 => Weekday::Wed,
            4 => Weekday::Thu,
            5 => Weekday::Fri,
            6 => Weekday::Sat,
            7 => Weekday::Sun,
            _ => return None,
        };
        let date = NaiveDate::from_isoywd_opt(i32::try_from(year).ok()?, week, weekday)?;
        (date.year() as u32, date.month(), date.day())
    } else {
        let (month, next) = parse_digits(&text, p, 2)?;
        p = next;
        if uses_separator {
            let separator = text.at(p);
            p += 1;
            if separator != b'-' {
                return None;
            }
        }
        let (day, _) = parse_digits(&text, p, 2)?;
        (year, month, day)
    };
    if !(1..=9999).contains(&year) {
        return None;
    }
    NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn golden() -> Value {
        serde_json::from_str(include_str!("golden/isoformat_expected.json")).unwrap()
    }

    #[test]
    fn times_match_python() {
        let golden = golden();
        let cases = golden["time"].as_array().unwrap();
        assert!(cases.len() > 10_000);
        let mut valid = 0;
        for case in cases {
            let text = case[0].as_str().unwrap();
            let actual = parse_time(text).map(|time| serde_json::json!([time.hour, time.minute, time.second, time.microsecond, time.offset_micros]));
            let expected = (!case[1].is_null()).then(|| case[1].clone());
            assert_eq!(actual, expected, "{text:?}");
            valid += usize::from(expected.is_some());
        }
        assert!(valid > 900);
    }

    #[test]
    fn dates_match_python() {
        let golden = golden();
        let cases = golden["date"].as_array().unwrap();
        assert!(cases.len() > 6000);
        let mut valid = 0;
        for case in cases {
            let text = case[0].as_str().unwrap();
            let actual = parse_date(text).map(|date| date.format("%Y-%m-%d").to_string());
            assert_eq!(actual.as_deref(), case[1].as_str(), "{text:?}");
            valid += usize::from(actual.is_some());
        }
        assert!(valid > 50);
    }
}
