//! Messaggio di benvenuto (equivalente di `cogs/welcome_message.py`).
// Verrà agganciato all'evento di ingresso membro quando porteremo `cogs/community.py`.
#![allow(dead_code)]

use poise::serenity_prelude::{CreateEmbed, CreateEmbedFooter};

pub const DEFAULT_WELCOME_TITLE: &str = "Benvenuto/a {user}, su {server}.";
pub const DEFAULT_WELCOME_BODY: &str = "Tu sei il **{ordinal}** ad entrare nel nostro caloroso server! 🌸\n\n\
• Passa su canali e ruoli per personalizzare il tuo profilo e renderlo più tuo!\n\
• Consulta la sezione info per avere le idee chiare su chi siamo.\n\
• Dai un'occhiata al calendario e ai canali vocali per i prossimi eventi.\n\
• Presentati e apri un ticket se ti serve aiuto.\n\
• Usa `/guida` per scoprire proposte, voti e attività della community.";
pub const DEFAULT_WELCOME_GIF: &str = "";
pub const WELCOME_FOOTER: &str = "Buona permanenza!";

const WELCOME_COLOR: u32 = 0x39C7BB;
const EMBED_TITLE_LIMIT: usize = 256;
const EMBED_DESCRIPTION_LIMIT: usize = 4096;

/// Modelli configurabili dallo staff (tabella `settings`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeSettings {
    pub title_template: String,
    pub body_template: String,
    pub gif_url: String,
}

/// Dati del nuovo membro usati per riempire i segnaposto.
#[derive(Debug, Clone)]
pub struct NewMember<'a> {
    pub username: &'a str,
    pub server_name: &'a str,
    pub member_count: u64,
    pub avatar_url: &'a str,
}

/// Legge i modelli tramite `get_setting(chiave, predefinito)`, come `core.get_setting` in Python.
pub fn welcome_settings(get_setting: impl Fn(&str, &str) -> String) -> WelcomeSettings {
    WelcomeSettings {
        title_template: get_setting("welcome_title", DEFAULT_WELCOME_TITLE),
        body_template: get_setting("welcome_body", DEFAULT_WELCOME_BODY),
        gif_url: get_setting("welcome_gif", DEFAULT_WELCOME_GIF),
    }
}

/// `1456` -> `"1.456°"` (separatore delle migliaia all'italiana).
fn ordinal(member_count: u64) -> String {
    let digits = member_count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3 + 2);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(ch);
    }
    grouped.push('°');
    grouped
}

/// Taglia a `limit` caratteri (non byte), come lo slicing delle stringhe Python.
fn truncate_chars(text: String, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((byte_index, _)) => text[..byte_index].to_string(),
        None => text,
    }
}

pub fn build_welcome_embed(settings: &WelcomeSettings, member: &NewMember<'_>) -> CreateEmbed {
    // L'ordine conta: le sostituzioni sono sequenziali, come nel dizionario Python.
    let replacements = [
        ("{user}", member.username.to_string()),
        ("{server}", member.server_name.to_string()),
        ("{member_count}", member.member_count.to_string()),
        ("{ordinal}", ordinal(member.member_count)),
    ];
    let render = |template: &str| {
        replacements
            .iter()
            .fold(template.to_string(), |text, (token, value)| text.replace(token, value))
    };

    let mut embed = CreateEmbed::new()
        .title(truncate_chars(render(&settings.title_template), EMBED_TITLE_LIMIT))
        .description(truncate_chars(render(&settings.body_template), EMBED_DESCRIPTION_LIMIT))
        .colour(WELCOME_COLOR);
    if member.avatar_url.starts_with("https://") {
        embed = embed.thumbnail(member.avatar_url);
    }
    if settings.gif_url.starts_with("https://") {
        embed = embed.image(settings.gif_url.as_str());
    }
    embed.footer(CreateEmbedFooter::new(WELCOME_FOOTER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn settings_from(map: HashMap<&'static str, &'static str>) -> WelcomeSettings {
        welcome_settings(|key, default| map.get(key).map_or(default, |v| v).to_string())
    }

    #[test]
    fn template_expands_member_and_server_values_and_sets_media() {
        // Stesso caso di tests/test_site_admin.py::WelcomeTemplateTests.
        let settings = settings_from(HashMap::from([
            ("welcome_title", "Benvenuto/a {user}, su {server}."),
            ("welcome_body", "Sei il {ordinal} membro ({member_count})."),
            ("welcome_gif", "https://cdn.example.test/welcome.gif"),
        ]));
        let member = NewMember {
            username: "Rita",
            server_name: "SFO",
            member_count: 1456,
            avatar_url: "https://cdn.example.test/avatar.png",
        };
        let json = serde_json::to_value(build_welcome_embed(&settings, &member)).unwrap();
        assert_eq!(json["title"], "Benvenuto/a Rita, su SFO.");
        assert_eq!(json["description"], "Sei il 1.456° membro (1456).");
        assert_eq!(json["thumbnail"]["url"], "https://cdn.example.test/avatar.png");
        assert_eq!(json["image"]["url"], "https://cdn.example.test/welcome.gif");
        assert_eq!(json["footer"]["text"], WELCOME_FOOTER);
        assert_eq!(json["color"], WELCOME_COLOR);
    }

    #[test]
    fn defaults_are_used_when_settings_are_missing() {
        let settings = settings_from(HashMap::new());
        assert_eq!(settings.title_template, DEFAULT_WELCOME_TITLE);
        assert_eq!(settings.body_template, DEFAULT_WELCOME_BODY);
        assert_eq!(settings.gif_url, "");
    }

    #[test]
    fn non_https_media_is_ignored() {
        let settings = settings_from(HashMap::from([("welcome_gif", "http://insicuro.test/a.gif")]));
        let member = NewMember {
            username: "Rita",
            server_name: "SFO",
            member_count: 7,
            avatar_url: "http://insicuro.test/a.png",
        };
        let json = serde_json::to_value(build_welcome_embed(&settings, &member)).unwrap();
        assert!(json.get("thumbnail").is_none_or(|v| v.is_null()));
        assert!(json.get("image").is_none_or(|v| v.is_null()));
    }

    #[test]
    fn ordinal_groups_thousands_with_dots() {
        assert_eq!(ordinal(0), "0°");
        assert_eq!(ordinal(999), "999°");
        assert_eq!(ordinal(1000), "1.000°");
        assert_eq!(ordinal(1456), "1.456°");
        assert_eq!(ordinal(1_234_567), "1.234.567°");
    }

    #[test]
    fn title_and_body_are_truncated_by_characters() {
        let long = "è".repeat(5000);
        let settings = WelcomeSettings {
            title_template: long.clone(),
            body_template: long,
            gif_url: String::new(),
        };
        let member = NewMember { username: "x", server_name: "y", member_count: 1, avatar_url: "" };
        let json = serde_json::to_value(build_welcome_embed(&settings, &member)).unwrap();
        assert_eq!(json["title"].as_str().unwrap().chars().count(), 256);
        assert_eq!(json["description"].as_str().unwrap().chars().count(), 4096);
    }
}
