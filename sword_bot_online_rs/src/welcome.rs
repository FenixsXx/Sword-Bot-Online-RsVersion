//! Modulo per la composizione e formattazione dei messaggi di benvenuto automatici.
//!
//! Quando un utente si unisce al server Discord, il bot invia un messaggio "Embed" (ovvero una scheda
//! grafica formattata) per accoglierlo. Questo modulo si occupa di applicare dei modelli di testo (template)
//! predefiniti o personalizzati dallo staff, sostituendo dei segnaposto (es. `{user}`, `{server}`)
//! con i dati reali del nuovo membro.

#![allow(dead_code)]

use poise::serenity_prelude::{CreateEmbed, CreateEmbedFooter};

/// Titolo predefinito se lo staff non ne specifica uno personalizzato nel database.
pub const DEFAULT_WELCOME_TITLE: &str = "Benvenuto/a {user}, su {server}.";

/// Testo del corpo del messaggio predefinito. Contiene segnaposto come `{ordinal}` che verranno rimpiazzati.
pub const DEFAULT_WELCOME_BODY: &str = "Tu sei il **{ordinal}** ad entrare nel nostro caloroso server! 🌸\n\n\
• Passa su canali e ruoli per personalizzare il tuo profilo e renderlo più tuo!\n\
• Consulta la sezione info per avere le idee chiare su chi siamo.\n\
• Dai un'occhiata al calendario e ai canali vocali per i prossimi eventi.\n\
• Presentati e apri un ticket se ti serve aiuto.\n\
• Usa `/guida` per scoprire proposte, voti e attività della community.";

/// URL predefinito dell'immagine/GIF animata (di default vuoto se non impostato).
pub const DEFAULT_WELCOME_GIF: &str = "";

/// Testo mostrato sempre in fondo alla scheda di benvenuto (footer).
pub const WELCOME_FOOTER: &str = "Buona permanenza!";

/// Colore di sfondo della striscia laterale dell'Embed espresso in esadecimale (verde acqua/turchese).
const WELCOME_COLOR: u32 = 0x39C7BB;

/// Limite massimo di caratteri consentito da Discord per il titolo di un Embed.
const EMBED_TITLE_LIMIT: usize = 256;

/// Limite massimo di caratteri consentito da Discord per la descrizione (corpo principale) di un Embed.
const EMBED_DESCRIPTION_LIMIT: usize = 4096;

/// Struttura che contiene i dati delle impostazioni per il messaggio di benvenuto.
/// Viene riempita leggendo la tabella `settings` presente nel database del bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeSettings {
    /// Il modello di testo per il titolo (contiene eventuali segnaposto).
    pub title_template: String,
    /// Il modello di testo per la descrizione principale.
    pub body_template: String,
    /// L'indirizzo web dell'immagine o GIF da mostrare nel messaggio.
    pub gif_url: String,
}

/// Contiene le informazioni dell'utente che è appena entrato nel server Discord.
/// Queste informazioni vengono usate per rimpiazzare i segnaposto nei modelli di testo.
#[derive(Debug, Clone)]
pub struct NewMember<'a> {
    /// Nome utente visualizzato su Discord.
    pub username: &'a str,
    /// Nome del server (gilda) a cui si è unito.
    pub server_name: &'a str,
    /// Numero totale di membri del server (incluso il nuovo arrivato).
    pub member_count: u64,
    /// Link all'immagine dell'avatar dell'utente.
    pub avatar_url: &'a str,
}

/// Recupera le impostazioni di benvenuto (titolo, corpo, GIF) usando una funzione passata in input
/// che ricerca nel database le opzioni personalizzate. Se un valore non viene trovato, si usa quello predefinito.
///
/// `get_setting`: una funzione (o closure) che accetta la chiave dell'impostazione e un valore di default,
/// restituendo la stringa risultante dal DB.
pub fn welcome_settings(get_setting: impl Fn(&str, &str) -> String) -> WelcomeSettings {
    WelcomeSettings {
        title_template: get_setting("welcome_title", DEFAULT_WELCOME_TITLE),
        body_template: get_setting("welcome_body", DEFAULT_WELCOME_BODY),
        gif_url: get_setting("welcome_gif", DEFAULT_WELCOME_GIF),
    }
}

/// Converte un numero di membri in un testo "ordinale" formattato all'italiana con il punto delle migliaia.
/// Esempio: `1456` diventa `"1.456°"`.
///
/// Questa funzione prende la sequenza di cifre, inserisce i punti per separare le migliaia (partendo da destra)
/// e aggiunge il simbolo dell'indicatore ordinale mascherato `°` alla fine.
fn ordinal(member_count: u64) -> String {
    let digits = member_count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3 + 2);
    for (index, ch) in digits.chars().enumerate() {
        // Aggiunge un punto ogni 3 cifre partendo dalla fine del numero, tranne all'inizio
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(ch);
    }
    grouped.push('°');
    grouped
}

/// Taglia una stringa se supera un determinato numero di *caratteri* (non byte).
///
/// In Rust, le stringhe usano la codifica UTF-8, in cui i caratteri speciali o le emoji
/// occupano più di un byte. Questa funzione conta attentamente i caratteri individuali per
/// evitare che la stringa venga tagliata a metà di un carattere (cosa che causerebbe un crash del programma).
fn truncate_chars(text: String, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((byte_index, _)) => text[..byte_index].to_string(),
        None => text,
    }
}

/// Costruisce l'oggetto `CreateEmbed` (la scheda grafica finale) pronto per essere inviato a Discord.
///
/// Prende i dati dell'utente (`member`) e i modelli di testo (`settings`), sostituisce le variabili/segnaposto
/// come `{user}`, `{server}`, `{member_count}` e `{ordinal}`, applica i limiti di lunghezza per Discord
/// ed eventualmente aggancia la foto profilo dell'utente (thumbnail) e la GIF animata.
pub fn build_welcome_embed(settings: &WelcomeSettings, member: &NewMember<'_>) -> CreateEmbed {
    // Definizione delle sostituzioni da effettuare.
    // L'ordine è importante per evitare che eventuali segnaposto nidificati si sovrappongano.
    let replacements = [
        ("{user}", member.username.to_string()),
        ("{server}", member.server_name.to_string()),
        ("{member_count}", member.member_count.to_string()),
        ("{ordinal}", ordinal(member.member_count)),
    ];

    // Closure (funzione anonima) locale per eseguire tutte le sostituzioni di testo in sequenza.
    let render = |template: &str| {
        replacements
            .iter()
            .fold(template.to_string(), |text, (token, value)| text.replace(token, value))
    };

    // Creazione della scheda Embed configurandone i vari elementi grafici e testuali.
    let mut embed = CreateEmbed::new()
        .title(truncate_chars(render(&settings.title_template), EMBED_TITLE_LIMIT))
        .description(truncate_chars(render(&settings.body_template), EMBED_DESCRIPTION_LIMIT))
        .colour(WELCOME_COLOR);

    // Mostra l'avatar dell'utente come miniatura (in alto a destra nel messaggio) se il link è una risorsa HTTPS valida.
    if member.avatar_url.starts_with("https://") {
        embed = embed.thumbnail(member.avatar_url);
    }

    // Mostra un'immagine grande/GIF (al centro del messaggio) se configurata correttamente.
    if settings.gif_url.starts_with("https://") {
        embed = embed.image(settings.gif_url.as_str());
    }

    // Imposta il testo del piè di pagina.
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
