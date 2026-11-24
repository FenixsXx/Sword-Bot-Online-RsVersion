//! Modulo di formattazione e utilità testuali per il bot Discord.
//!
//! Questo modulo contiene funzioni di supporto per la manipolazione delle stringhe,
//! la gestione degli URL, la creazione di barre di avanzamento visive e la formattazione
//! dei dati tipo Python. Tutte le funzioni sono state scritte per comportarsi esattamente
//! come gli equivalenti in Python, garantendo coerenza tra le due implementazioni.
//! Queste funzioni sono utilizzate in tutta l'applicazione per presentare le informazioni
//! agli utenti in modo chiaro e uniformemente formattato.
//!
//! Tutto `sfo/formatting.py` (`progress_bar`, `proposal_type_emoji`, `normalize`,
//! `proposal_image_url`), più `casefold` e `python_json` che servono per essere identici a Python.
// Verranno usate dalle schede e dai comandi.
#![allow(dead_code)]

use std::io;

use serde::Serialize;
use serde_json::Value;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};
use url::Url;

use crate::config::{BAR_LEN, PROPOSAL_TYPE_EMOJIS};

/// Emoji usata quando il tipo di proposta non è tra quelli noti.
pub const DEFAULT_TYPE_EMOJI: &str = "🎯";

/// Barra di avanzamento, es. `` `▰▰▰▱▱▱▱▱▱▱` ``. Il riempimento è clampato a `0..=length`.
///
/// L'arrotondamento è "al pari più vicino" (come `round()` di Python): 0,5 → 0 e 1,5 → 2.
pub fn progress_bar(value: i64, maximum: i64, length: usize) -> String {
    let filled = if maximum == 0 {
        0
    } else {
        let ratio = (value as f64 / maximum as f64 * length as f64).round_ties_even();
        ratio.clamp(0.0, length as f64) as usize
    };
    format!("`{}{}`", "▰".repeat(filled), "▱".repeat(length - filled))
}

/// [`progress_bar`] con la lunghezza predefinita (`BAR_LEN`).
pub fn default_progress_bar(value: i64, maximum: i64) -> String {
    progress_bar(value, maximum, BAR_LEN)
}

/// Equivalente di `str.casefold()` per i testi che arrivano dagli utenti.
///
/// `to_lowercase()` copre le lettere accentate italiane; in più ripiega le uniche forme che
/// Python tratta diversamente dal semplice minuscolo e che si incontrano davvero
/// (ß → ss, ς → σ, ſ → s). Non è il casefolding completo di Unicode.
pub fn casefold(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            'ß' | 'ẞ' => folded.push_str("ss"),
            'ς' => folded.push('σ'),
            'ſ' => folded.push('s'),
            other => folded.extend(other.to_lowercase()),
        }
    }
    folded
}

/// Toglie maiuscole, accenti, spazi e punteggiatura per riconoscere i duplicati.
///
/// `"Jackbox Party!"` e `"jackbox  party"` danno lo stesso risultato (`"jackboxparty"`). Come in
/// Python: scomposizione NFKD, via i segni combinanti (gli accenti), minuscolo e solo `a-z0-9`:
/// le lettere non latine (greco, cirillico, CJK) e le emoji spariscono del tutto.
pub fn normalize(text: &str) -> String {
    text.nfkd()
        .filter(|ch| !is_combining_mark(*ch))
        .flat_map(char::to_lowercase)
        .filter(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        .collect()
}

/// ID di un video YouTube: esattamente 11 caratteri ASCII tra lettere, cifre, `_` e `-`.
fn is_video_id(text: &str) -> bool {
    text.len() == 11 && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `/<prefisso>/<ID>` con uno `/` finale facoltativo (es. `/shorts/dQw4w9WgXcQ/`).
fn video_id_from_path<'a>(path: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|prefix| {
        let rest = path.strip_prefix(prefix)?;
        Some(rest.strip_suffix('/').unwrap_or(rest))
    })
}

/// Copertina da mostrare per un link di YouTube o Steam; `None` per qualsiasi altro link.
///
/// Solo `https` e senza credenziali nell'URL: un link `http://` o `https://utente:pw@…` non
/// viene mai trasformato in un'immagine caricata da Discord.
pub fn proposal_image_url(link: Option<&str>) -> Option<String> {
    let parsed = Url::parse(link.filter(|link| !link.is_empty())?).ok()?;
    if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let hostname = parsed.host_str()?.to_lowercase();
    let hostname = hostname.trim_end_matches('.');

    let video_id: Option<String> = match hostname {
        "youtube.com" | "www.youtube.com" | "m.youtube.com" => parsed
            // `parse_qs` di Python ignora i valori vuoti e prende il primo rimasto.
            .query_pairs()
            .find(|(key, value)| key == "v" && !value.is_empty())
            .map(|(_, value)| value.into_owned())
            .or_else(|| video_id_from_path(parsed.path(), &["/shorts/", "/embed/", "/live/"]).map(str::to_string)),
        "youtu.be" | "www.youtu.be" => video_id_from_path(parsed.path(), &["/"]).map(str::to_string),
        _ => None,
    };
    if let Some(video_id) = video_id.filter(|id| is_video_id(id)) {
        return Some(format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"));
    }

    if hostname == "store.steampowered.com" || hostname == "steamcommunity.com" {
        // `/app/<cifre>` seguito da `/` o dalla fine del percorso.
        if let Some(rest) = parsed.path().strip_prefix("/app/") {
            let digits_end = rest.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(rest.len());
            let (app_id, after) = rest.split_at(digits_end);
            if !app_id.is_empty() && (after.is_empty() || after.starts_with('/')) {
                return Some(format!("https://cdn.akamai.steamstatic.com/steam/apps/{app_id}/header.jpg"));
            }
        }
    }

    // Supporto a immagini dirette HTTPS (PNG, JPG, JPEG, WebP, GIF)
    let path_lower = parsed.path().to_lowercase();
    if [".png", ".jpg", ".jpeg", ".webp", ".gif"].iter().any(|ext| path_lower.ends_with(ext)) {
        return Some(parsed.to_string());
    }

    None
}

/// Formattatore JSON con i separatori di `json.dumps` (`", "` e `": "`), così le righe scritte da
/// Rust nella tabella `staff_audit` sono identiche a quelle scritte da Python.
struct PythonJsonFormatter;

impl serde_json::ser::Formatter for PythonJsonFormatter {
    fn begin_array_value<W: ?Sized + io::Write>(&mut self, writer: &mut W, first: bool) -> io::Result<()> {
        if first { Ok(()) } else { writer.write_all(b", ") }
    }

    fn begin_object_key<W: ?Sized + io::Write>(&mut self, writer: &mut W, first: bool) -> io::Result<()> {
        if first { Ok(()) } else { writer.write_all(b", ") }
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        writer.write_all(b": ")
    }
}

/// `json.dumps(value, ensure_ascii=False)`.
pub fn python_json(value: &Value) -> String {
    let mut buffer = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, PythonJsonFormatter);
    value.serialize(&mut serializer).expect("un Value è sempre serializzabile");
    String::from_utf8(buffer).expect("serde_json produce UTF-8")
}

/// `str.capitalize()` di Python: prima lettera maiuscola e tutto il resto minuscolo.
pub fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect(),
        None => String::new(),
    }
}

/// Primi `limit` caratteri (non byte), come lo slicing `text[:limit]` di Python.
pub fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Emoji del tipo di proposta (`Gioco`, `Film`…), insensibile a maiuscole e spazi ai bordi.
pub fn proposal_type_emoji(tipo: &str) -> &'static str {
    let key = casefold(tipo.trim());
    PROPOSAL_TYPE_EMOJIS
        .iter()
        .find(|(name, _)| *name == key)
        .map_or(DEFAULT_TYPE_EMOJI, |(_, emoji)| emoji)
}

#[cfg(test)]
mod tests {
    use super::*;

    const YT: &str = "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg";

    /// Casi con l'esito calcolato da `sfo.formatting.proposal_image_url` in Python.
    #[test]
    fn proposal_image_url_matches_the_python_implementation() {
        let steam = |id: &str| Some(format!("https://cdn.akamai.steamstatic.com/steam/apps/{id}/header.jpg"));
        let youtube = || Some(YT.to_string());
        let cases: Vec<(Option<&str>, Option<String>)> = vec![
            (None, None),
            (Some(""), None),
            (Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), youtube()),
            (Some("https://youtube.com/watch?v=dQw4w9WgXcQ&t=10"), youtube()),
            (Some("https://m.youtube.com/watch?feature=share&v=dQw4w9WgXcQ"), youtube()),
            (Some("https://www.youtube.com/watch?v=corto"), None),
            (Some("https://www.youtube.com/watch?v="), None),
            (Some("https://www.youtube.com/watch?v=&v=dQw4w9WgXcQ"), youtube()),
            (Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ&v=AAAAAAAAAAA"), youtube()),
            (Some("https://www.youtube.com/shorts/dQw4w9WgXcQ"), youtube()),
            (Some("https://www.youtube.com/shorts/dQw4w9WgXcQ/"), youtube()),
            (Some("https://www.youtube.com/embed/dQw4w9WgXcQ"), youtube()),
            (Some("https://www.youtube.com/live/dQw4w9WgXcQ/"), youtube()),
            (Some("https://www.youtube.com/shorts/dQw4w9WgXcQ/extra"), None),
            (Some("https://www.youtube.com/channel/UC1234567890"), None),
            (Some("https://youtu.be/dQw4w9WgXcQ"), youtube()),
            (Some("https://www.youtu.be/dQw4w9WgXcQ/"), youtube()),
            (Some("https://youtu.be/dQw4w9WgXcQ?t=5"), youtube()),
            (Some("https://youtu.be/troppolungoooo"), None),
            (Some("http://www.youtube.com/watch?v=dQw4w9WgXcQ"), None),
            (Some("https://user:pw@www.youtube.com/watch?v=dQw4w9WgXcQ"), None),
            (Some("https://user@youtu.be/dQw4w9WgXcQ"), None),
            (Some("https://YOUTUBE.COM./watch?v=dQw4w9WgXcQ"), youtube()),
            (Some("https://www.youtube.com:443/watch?v=dQw4w9WgXcQ"), youtube()),
            (Some("https://evil.example/watch?v=dQw4w9WgXcQ"), None),
            (Some("https://notyoutube.com/watch?v=dQw4w9WgXcQ"), None),
            (Some("https://store.steampowered.com/app/570/Dota_2/"), steam("570")),
            (Some("https://store.steampowered.com/app/570"), steam("570")),
            (Some("https://store.steampowered.com/app/570?l=italian"), steam("570")),
            (Some("https://store.steampowered.com/app/570x/"), None),
            (Some("https://store.steampowered.com/app/"), None),
            (Some("https://store.steampowered.com/sub/570/"), None),
            (Some("https://STORE.steampowered.com/app/1234/"), steam("1234")),
            (Some("http://store.steampowered.com/app/570/"), None),
            (Some("https://example.com/"), None),
            (Some("https://steamcommunity.com/app/730/"), steam("730")),
            (Some("https://cdn.example.com/art/cover.webp"), Some("https://cdn.example.com/art/cover.webp".into())),
            (Some("https://i.imgur.com/screenshot.png"), Some("https://i.imgur.com/screenshot.png".into())),
            (Some("ftp://youtube.com/watch?v=dQw4w9WgXcQ"), None),
            (Some("non un url"), None),
            (Some("https://"), None),
            (Some("https://[::1/watch?v=dQw4w9WgXcQ"), None),
        ];
        for (link, expected) in cases {
            assert_eq!(proposal_image_url(link), expected, "link: {link:?}");
        }
    }

    /// Ingressi e uscite calcolati da `sfo.formatting.normalize` in Python.
    #[test]
    fn normalize_matches_the_python_implementation() {
        let cases = [
            ("Jackbox Party!", "jackboxparty"),
            ("jackbox  party", "jackboxparty"),
            ("Università", "universita"),
            ("ÀÈÌÒÙ àèìòù", "aeiouaeiou"),
            ("Caffè ☕", "caffe"),
            ("Café Society 2", "cafesociety2"),
            ("Among Us", "amongus"),
            ("Ｆｕｌｌｗｉｄｔｈ １２３", "fullwidth123"),
            ("\u{FB01}nal \u{FB02}ight", "finalflight"),
            ("\u{130}stanbul", "istanbul"),
            ("ß-Straße", "strae"),
            ("Σίσυφος", ""),
            ("Привет мир", ""),
            ("日本語のタイトル", ""),
            ("🎮🎮", ""),
            ("  ", ""),
            ("", ""),
            ("A-B_C.D", "abcd"),
            ("naïve résumé", "naiveresume"),
            ("\u{1C4} d\u{17E}", "dzdz"),
            ("x\u{301}y\u{323}z", "xyz"),
            ("№ 5", "no5"),
            ("½ + ¼", "1214"),
            ("\u{216B}", "xii"),
            ("\u{1F0}", "j"),
            ("Œuvre", "uvre"),
            ("æ", ""),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize(input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn capitalize_and_truncate_follow_python() {
        assert_eq!(capitalize("ogni lunedì alle 12:00"), "Ogni lunedì alle 12:00");
        assert_eq!(capitalize("ÈCCO TUTTO"), "Ècco tutto", "il resto va in minuscolo");
        assert_eq!(capitalize(""), "");
        assert_eq!(truncate_chars("àèìòù", 3), "àèì");
        assert_eq!(truncate_chars("ab", 5), "ab");
    }

    #[test]
    fn progress_bar_fills_proportionally() {
        assert_eq!(default_progress_bar(5, 10), "`▰▰▰▰▰▱▱▱▱▱`");
        assert_eq!(default_progress_bar(10, 10), "`▰▰▰▰▰▰▰▰▰▰`");
        assert_eq!(default_progress_bar(0, 10), "`▱▱▱▱▱▱▱▱▱▱`");
        assert_eq!(progress_bar(1, 4, 8), "`▰▰▱▱▱▱▱▱`");
    }

    #[test]
    fn progress_bar_handles_zero_maximum_and_out_of_range_values() {
        assert_eq!(default_progress_bar(3, 0), "`▱▱▱▱▱▱▱▱▱▱`");
        assert_eq!(default_progress_bar(15, 10), "`▰▰▰▰▰▰▰▰▰▰`", "oltre il massimo: barra piena");
        assert_eq!(default_progress_bar(-3, 10), "`▱▱▱▱▱▱▱▱▱▱`", "negativo: barra vuota");
    }

    #[test]
    fn progress_bar_rounds_half_to_even_like_python() {
        // round(0.5)=0, round(1.5)=2, round(2.5)=2 in Python.
        assert_eq!(default_progress_bar(1, 20), "`▱▱▱▱▱▱▱▱▱▱`");
        assert_eq!(default_progress_bar(3, 20), "`▰▰▱▱▱▱▱▱▱▱`");
        assert_eq!(default_progress_bar(5, 20), "`▰▰▱▱▱▱▱▱▱▱`");
    }

    #[test]
    fn type_emoji_ignores_case_and_surrounding_spaces() {
        assert_eq!(proposal_type_emoji("Gioco"), "🎮");
        assert_eq!(proposal_type_emoji("  FILM "), "🎬");
        assert_eq!(proposal_type_emoji("karaoke"), "🎤");
        assert_eq!(proposal_type_emoji("Altro"), "✨");
    }

    #[test]
    fn unknown_type_gets_the_default_emoji() {
        assert_eq!(proposal_type_emoji("Serie TV"), "🎯");
        assert_eq!(proposal_type_emoji(""), "🎯");
    }

    #[test]
    fn casefold_matches_python_for_common_text() {
        assert_eq!(casefold("Università"), "università");
        assert_eq!(casefold("STRASSE"), "strasse");
        assert_eq!(casefold("Straße"), "strasse");
        assert_eq!(casefold("ΟΔΟΣ"), "οδοσ", "il sigma finale si riduce a σ, come in Python");
        assert_eq!(casefold("Among Us"), "among us");
    }
}
