//! Embed con i colori semantici del bot (equivalente di `cogs/embeds.py`).
// Libreria di helper: parte delle funzioni verrà usata solo dai cog portati più avanti.
#![allow(dead_code)]

use chrono::{DateTime, TimeZone};
use poise::serenity_prelude::CreateEmbed;

/// Significato dell'embed: ne determina il colore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedKind {
    Success,
    Error,
    Info,
}

impl EmbedKind {
    pub const fn color(self) -> u32 {
        match self {
            EmbedKind::Success => 0x2ECC71,
            EmbedKind::Error => 0xE74C3C,
            EmbedKind::Info => 0x3498DB,
        }
    }
}

/// Stili di `<t:...>` di Discord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampStyle {
    ShortTime,
    LongTime,
    ShortDate,
    LongDate,
    ShortDateTime,
    LongDateTime,
    Relative,
}

impl TimestampStyle {
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

/// Markdown di timestamp Discord, es. `<t:1767225600:R>`.
pub fn discord_timestamp<Tz: TimeZone>(value: &DateTime<Tz>, style: TimestampStyle) -> String {
    format!("<t:{}:{}>", value.timestamp(), style.code())
}

/// `<t:ts:R>` per un timestamp Unix (stile relativo, quello usato dal bot quasi ovunque).
pub fn discord_timestamp_for(unix_seconds: i64) -> String {
    format!("<t:{unix_seconds}:{}>", TimestampStyle::Relative.code())
}

/// Embed vuoto con il colore del tipo indicato.
pub fn embed(kind: EmbedKind) -> CreateEmbed {
    CreateEmbed::new().colour(kind.color())
}

fn titled(kind: EmbedKind, title: &str, description: &str) -> CreateEmbed {
    let embed = embed(kind).title(title);
    if description.is_empty() {
        embed
    } else {
        embed.description(description)
    }
}

pub fn success(title: &str, description: &str) -> CreateEmbed {
    titled(EmbedKind::Success, title, description)
}

pub fn error(title: &str, description: &str) -> CreateEmbed {
    titled(EmbedKind::Error, title, description)
}

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
