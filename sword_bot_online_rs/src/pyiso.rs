//! `date.fromisoformat` e `time.fromisoformat` di Python 3.12.
//!
//! Python accetta più forme di quelle che ci si aspetta (`20270305`, `2027-W09-5`, `21`, `2100`,
//! `T21:00`, `21.5`, un fuso dopo uno spazio…): le regole sono le sue, ricalcate dall'implementazione
//! in C, perché i report del sito salvano la data così com'è stata scritta e i dati devono essere
//! quelli che Python accetterebbe.

use chrono::{Datelike, NaiveDate, Weekday};

/// Un orario letto da `time.fromisoformat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoTime {
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub microsecond: u32,
    /// Lo scarto dal fuso in microsecondi, se l'orario ne indica uno.
    pub offset_micros: Option<i64>,
}

/// I byte del testo, con un NUL «virtuale» dopo la fine (come la stringa C).
struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn at(&self, index: usize) -> u8 {
        self.0.get(index).copied().unwrap_or(0)
    }
}

/// `parse_digits`: esattamente `count` cifre ASCII da `start`.
fn parse_digits(text: &Bytes, start: usize, count: usize) -> Option<(u32, usize)> {
    let mut value = 0u32;
    for offset in 0..count {
        let byte = text.at(start + offset);
        if !byte.is_ascii_digit() || start + offset >= text.0.len() {
            return None;
        }
        value = value * 10 + u32::from(byte - b'0');
    }
    Some((value, start + count))
}

/// `parse_hh_mm_ss_ff`: restituisce `Ok(extra)` (se c'è altro oltre l'orario, 1) o `Err(())`.
/// `end` è dove finisce la parte oraria (l'inizio del fuso, o la fine del testo).
fn parse_hh_mm_ss_ff(text: &Bytes, start: usize, end: usize) -> Result<([u32; 4], bool), ()> {
    let mut values = [0u32; 4];
    let mut p = start;
    let mut has_separator = true;
    let mut broke_for_fraction = false;
    for index in 0..3 {
        let (value, next) = parse_digits(text, p, 2).ok_or(())?;
        values[index] = value;
        p = next;
        let c = text.at(p);
        p += 1;
        if index == 0 {
            has_separator = c == b':';
        }
        if p >= end {
            return Ok((values, c != 0));
        } else if has_separator && c == b':' {
            continue;
        } else if c == b'.' || c == b',' {
            broke_for_fraction = true;
            break;
        } else if !has_separator {
            p -= 1;
        } else {
            return Err(());
        }
    }
    let _ = broke_for_fraction;
    // Parte frazionaria: fino a 6 cifre, le altre si saltano.
    let remaining = end.saturating_sub(p);
    let to_parse = remaining.min(6);
    let (mut micros, mut next) = parse_digits(text, p, to_parse).ok_or(())?;
    const CORRECTION: [u32; 5] = [100_000, 10_000, 1_000, 100, 10];
    if to_parse < 6 {
        if to_parse == 0 {
            return Err(());
        }
        micros *= CORRECTION[to_parse - 1];
    }
    while text.at(next).is_ascii_digit() {
        next += 1;
    }
    values[3] = micros;
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
