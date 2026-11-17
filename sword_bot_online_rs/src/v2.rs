//! Components V2 di Discord (`LayoutView`, `Container`, `Section`, `TextDisplay`…), in JSON grezzo.
//!
//! serenity 0.12 non ha i tipi V2, ma si può usare lo stesso: `Http::send_message`,
//! `Http::edit_message` e `Http::create_interaction_response` accettano qualunque corpo
//! serializzabile, e un clic su un messaggio V2 viene letto correttamente (il `Container` di primo
//! livello è saltato dal deserializzatore tollerante di serenity e le righe di pulsanti e menu, che
//! come in Python stanno fuori dal contenitore, restano). I test qui sotto lo dimostrano con un
//! payload di interazione realistico e confrontano il JSON con quello che serializza `discord.py`.
//!
//! Regole di Discord per i messaggi V2: flag `IS_COMPONENTS_V2` (1 << 15), niente `content` né
//! `embeds`, al massimo 4000 caratteri di testo in tutto.
// Usato da `discord_api` e `publisher`.
#![allow(dead_code)]

use poise::serenity_prelude::{CreateActionRow, Http, InteractionId};
use serde_json::{Value, json};

use crate::config::COLOR_PRIMARY;
use crate::database::{Connected, DbResult};
use crate::hub::{HubCard, SUPPORT_INTRO, SupportCard, hub_components, support_card, support_confirm_custom_id, support_select};
use crate::queries::CurrentSuggestion;
use crate::ui::{ButtonKind, ViewButton, button_rows};

/// `MessageFlags.IS_COMPONENTS_V2`.
pub const IS_COMPONENTS_V2: u64 = 1 << 15;
/// `MessageFlags.EPHEMERAL`: visibile solo a chi ha premuto il pulsante.
pub const EPHEMERAL: u64 = 1 << 6;

// --- tipi di componente ----------------------------------------------------------------------

const TYPE_SECTION: u8 = 9;
const TYPE_TEXT_DISPLAY: u8 = 10;
const TYPE_THUMBNAIL: u8 = 11;
const TYPE_SEPARATOR: u8 = 14;
const TYPE_CONTAINER: u8 = 17;

/// Blocco di testo (Markdown).
pub fn text_display(content: &str) -> Value {
    json!({"type": TYPE_TEXT_DISPLAY, "content": content})
}

/// Linea di separazione con un po' di spazio.
pub fn separator() -> Value {
    json!({"type": 14, "divider": true, "spacing": 1})
}

/// Miniatura da mettere accanto a un testo.
pub fn thumbnail(url: &str) -> Value {
    json!({"type": TYPE_THUMBNAIL, "spoiler": false, "media": {"url": url}, "description": null})
}

/// Testo con una miniatura a lato.
pub fn section(text: &str, accessory: Value) -> Value {
    json!({"type": TYPE_SECTION, "components": [text_display(text)], "accessory": accessory})
}

/// Riquadro con il colore di accento sul bordo.
pub fn container(children: Vec<Value>, accent_colour: u32) -> Value {
    json!({"type": TYPE_CONTAINER, "accent_color": accent_colour, "spoiler": false, "components": children})
}

/// Le righe di pulsanti/menu di serenity come JSON.
fn rows_json(rows: Vec<CreateActionRow>) -> Vec<Value> {
    rows.into_iter().map(|row| serde_json::to_value(row).expect("una riga di azione è serializzabile")).collect()
}

// --- corpo dei messaggi ----------------------------------------------------------------------

/// Corpo di un messaggio V2 da inviare. In modifica si azzerano `content` ed `embeds` (come
/// `edit(content=None, embed=None, view=layout)` di Python), così un messaggio classico diventa V2.
pub fn message_body(components: Vec<Value>, editing: bool) -> Value {
    let mut body = json!({
        "flags": IS_COMPONENTS_V2,
        "components": components,
        "allowed_mentions": {"parse": []},
    });
    if editing {
        body["content"] = Value::Null;
        body["embeds"] = json!([]);
    }
    body
}

/// Come rispondere a un'interazione.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    /// Nuovo messaggio (tipo 4).
    NewMessage,
    /// Sostituisce il messaggio del componente premuto (tipo 7).
    UpdateMessage,
}

/// Corpo della risposta a un'interazione con componenti V2.
pub fn interaction_response_body(components: Vec<Value>, kind: ResponseKind, ephemeral: bool) -> Value {
    let flags = IS_COMPONENTS_V2 | if ephemeral && kind == ResponseKind::NewMessage { EPHEMERAL } else { 0 };
    json!({
        "type": match kind { ResponseKind::NewMessage => 4, ResponseKind::UpdateMessage => 7 },
        "data": {"flags": flags, "components": components, "allowed_mentions": {"parse": []}},
    })
}

/// Risponde a un'interazione con componenti V2 (es. la scheda privata di "Supporta").
pub async fn respond_with_components(
    http: &Http,
    interaction_id: u64,
    token: &str,
    components: Vec<Value>,
    kind: ResponseKind,
    ephemeral: bool,
) -> Result<(), poise::serenity_prelude::Error> {
    let body = interaction_response_body(components, kind, ephemeral);
    http.create_interaction_response(InteractionId::new(interaction_id), token, &body, Vec::new()).await
}

// --- dashboard -------------------------------------------------------------------------------

/// Componenti V2 della dashboard: il riquadro con la scheda, poi i pulsanti e il menu "Altro…"
/// (fuori dal riquadro, come nell'`HubLayout` di Python).
pub fn hub_v2_components(card: &HubCard) -> Vec<Value> {
    let mut children = vec![text_display(&card.header), separator(), text_display(&card.stats)];
    match &card.spotlight {
        Some((text, Some(image))) => children.push(section(text, thumbnail(image))),
        Some((text, None)) => children.push(text_display(text)),
        None => {}
    }
    children.push(text_display(&card.ballot));
    children.push(text_display(&card.recent));
    let mut components = vec![container(children, COLOR_PRIMARY)];
    components.extend(rows_json(hub_components()));
    components
}

// --- scheda di supporto ----------------------------------------------------------------------

/// Il messaggio privato del pulsante "Supporta" in V2.
#[derive(Debug, Clone)]
pub struct SupportV2 {
    pub components: Vec<Value>,
    /// La proposta scelta non è più disponibile.
    pub missing: bool,
}

pub fn support_v2(db: &impl Connected, suggestions: &[CurrentSuggestion], selected_id: i64) -> DbResult<SupportV2> {
    let card: Option<SupportCard> = support_card(db, selected_id)?;
    let mut components = Vec::new();
    if let Some(card) = &card {
        let body = match &card.image {
            Some(image) => section(&card.text, thumbnail(image)),
            None => text_display(&card.text),
        };
        components.push(container(vec![text_display(SUPPORT_INTRO), separator(), body], COLOR_PRIMARY));
    }
    components.extend(rows_json(vec![CreateActionRow::SelectMenu(support_select(db, suggestions, selected_id)?)]));
    if card.is_some() {
        let button = ViewButton {
            custom_id: support_confirm_custom_id(selected_id),
            label: "Supporta questa proposta".into(),
            emoji: Some("👍"),
            kind: ButtonKind::Success,
            disabled: false,
        };
        components.extend(rows_json(button_rows(std::slice::from_ref(&button))));
    }
    Ok(SupportV2 { components, missing: card.is_none() })
}

/// Un messaggio con una sola riga di testo (`_notice_layout` di Python).
pub fn notice_components(text: &str) -> Vec<Value> {
    vec![text_display(text)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::database::Database;
    use crate::hub::{hub_card, hub_data};
    use crate::queries::current_suggestions;
    use chrono::TimeZone;
    use poise::serenity_prelude::Interaction;
    use serde_json::{Value, json};

    fn user(id: &str, name: &str, bot: bool) -> Value {
        json!({"id": id, "username": name, "discriminator": "0", "avatar": null, "global_name": name, "bot": bot})
    }

    /// I componenti di un messaggio V2: `Container` di primo livello e righe di azione sotto.
    fn v2_message_components() -> Value {
        let container = json!({"type": 17, "id": 1, "accent_color": 5793266, "spoiler": false, "components": [
            {"type": 10, "id": 2, "content": "## 🎮 Game Night Hub"},
            {"type": 14, "id": 3, "divider": true, "spacing": 1},
            {"type": 9, "id": 4, "components": [{"type": 10, "id": 5, "content": "### ⭐ Proposta"}],
             "accessory": {"type": 11, "id": 6, "media": {"url": "https://i.ytimg.com/vi/x/hqdefault.jpg"}}}
        ]});
        let buttons = json!({"type": 1, "id": 7, "components": [
            {"type": 2, "id": 8, "style": 1, "label": "Vota", "custom_id": "hub:vote", "emoji": {"name": "🗳️"}},
            {"type": 2, "id": 9, "style": 3, "label": "Proponi", "custom_id": "hub:suggest"}
        ]});
        let menu = json!({"type": 1, "id": 10, "components": [
            {"type": 3, "id": 11, "custom_id": "hub:more", "placeholder": "Altro…", "min_values": 1, "max_values": 1,
             "options": [{"label": "Classifica", "value": "leaderboard", "default": false}]}
        ]});
        Value::Array(vec![container, buttons, menu])
    }

    fn v2_message() -> Value {
        let mut message = json!({
            "id": "900", "channel_id": "500", "content": "",
            "timestamp": "2026-11-11T17:00:00.000000+00:00", "edited_timestamp": null,
            "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false, "type": 0, "flags": 32768
        });
        message["author"] = user("7", "bot", true);
        message["components"] = v2_message_components();
        message
    }

    /// Un clic reale su un messaggio V2.
    fn v2_click_payload(custom_id: &str, component_type: u8, extra_data: Value) -> Value {
        let mut data = json!({"custom_id": custom_id, "component_type": component_type});
        data.as_object_mut().unwrap().extend(extra_data.as_object().cloned().unwrap_or_default());
        let mut member = json!({
            "nick": null, "avatar": null, "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00",
            "premium_since": null, "deaf": false, "mute": false, "flags": 0, "permissions": "8",
            "pending": false, "communication_disabled_until": null
        });
        member["user"] = user("10", "rita", false);
        let channel = json!({"id": "500", "type": 0, "guild_id": "1", "name": "sondaggi", "permissions": "8"});
        let mut payload = json!({
            "id": "1100", "application_id": "7", "type": 3, "guild_id": "1", "channel_id": "500",
            "token": "tok", "version": 1, "app_permissions": "8", "locale": "it", "guild_locale": "it",
            "entitlements": [], "authorizing_integration_owners": {}, "context": 0, "attachment_size_limit": 10485760
        });
        payload["data"] = data;
        payload["channel"] = channel;
        payload["member"] = member;
        payload["message"] = v2_message();
        payload
    }

    #[test]
    fn a_click_on_a_v2_message_is_understood_by_serenity() {
        let button = v2_click_payload("hub:vote", 2, json!({}));
        let interaction: Interaction = serde_json::from_value(button).expect("clic su pulsante V2");
        let component = interaction.message_component().expect("interazione di componente");
        assert_eq!(component.data.custom_id, "hub:vote");
        // serenity non conosce il bit IS_COMPONENTS_V2 (1 << 15) e lo scarta: non ci serve leggerlo.
        assert_eq!(component.message.flags.map(|f| f.bits()), Some(0));
        assert_eq!(component.message.components.len(), 2, "il Container è saltato, le due righe di azione restano");

        let menu = v2_click_payload("hub:more", 3, json!({"values": ["leaderboard"]}));
        let interaction: Interaction = serde_json::from_value(menu).expect("scelta nel menu V2");
        assert_eq!(interaction.message_component().unwrap().data.custom_id, "hub:more");
    }

    // --- parità con discord.py -------------------------------------------------------------

    /// Forma confrontabile di una riga di azione: serenity e discord.py omettono campi opzionali
    /// diversi (ma equivalenti per Discord), quindi si confrontano i valori che contano.
    fn normalize_rows(rows: &[Value], ignore_custom_ids: bool) -> Value {
        let id = |component: &Value| if ignore_custom_ids { Value::Null } else { component["custom_id"].clone() };
        Value::Array(
            rows.iter()
                .map(|row| {
                    assert_eq!(row["type"], 1, "righe di azione di primo livello");
                    Value::Array(
                        row["components"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|component| match component["type"].as_u64().unwrap() {
                                2 => json!([
                                    "button",
                                    id(component),
                                    component["label"],
                                    component["style"],
                                    component["emoji"]["name"],
                                    component["disabled"].as_bool().unwrap_or(false)
                                ]),
                                3 => json!({
                                    "select": id(component),
                                    "placeholder": component["placeholder"],
                                    "min": component["min_values"].as_u64().unwrap_or(1),
                                    "max": component["max_values"].as_u64().unwrap_or(1),
                                    "options": component["options"].as_array().unwrap().iter().map(|o| json!([
                                        o["label"], o["value"], o["default"].as_bool().unwrap_or(false), o["description"], o["emoji"]["name"]
                                    ])).collect::<Vec<_>>(),
                                }),
                                other => panic!("componente inatteso {other}"),
                            })
                            .collect(),
                    )
                })
                .collect(),
        )
    }

    fn clock() -> chrono::DateTime<chrono_tz::Tz> {
        TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()
    }

    fn fixture_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, clock()).unwrap();
        db.execute_batch(include_str!("golden/cards_fixture.sql")).unwrap();
        db
    }

    fn hub_json(db: &Database) -> Vec<Value> {
        hub_v2_components(&hub_card(&hub_data(db, clock()).unwrap()))
    }

    #[test]
    fn hub_layout_matches_the_json_discord_py_sends() {
        let golden: Value = serde_json::from_str(include_str!("golden/v2_expected.json")).unwrap();
        let db = fixture_db();
        for (variant, expected) in [("hub", &golden["hub"]), ("hub_image", &golden["hub_image"])] {
            if variant == "hub_image" {
                db.execute("UPDATE suggestions SET link='https://store.steampowered.com/app/570/' WHERE id=4", []).unwrap();
            }
            let actual = hub_json(&db);
            let expected = expected.as_array().unwrap();
            assert_eq!(actual[0], expected[0], "riquadro della dashboard ({variant}): identico, campo per campo");
            assert_eq!(normalize_rows(&actual[1..], false), normalize_rows(&expected[1..], false), "pulsanti e menu ({variant})");
            assert_eq!(actual.len(), 3);
        }
        // Con la copertina il blocco della proposta del momento è una sezione con miniatura.
        let with_image = hub_json(&db);
        assert_eq!(with_image[0]["components"][3]["type"], 9);
        assert_eq!(
            with_image[0]["components"][3]["accessory"]["media"]["url"],
            "https://cdn.akamai.steamstatic.com/steam/apps/570/header.jpg"
        );
    }

    #[test]
    fn support_layout_matches_the_json_discord_py_sends() {
        let golden: Value = serde_json::from_str(include_str!("golden/v2_expected.json")).unwrap();
        let db = fixture_db();
        let suggestions = current_suggestions(&db, 25, false).unwrap();
        for selected in [1, 3, 999] {
            let actual = support_v2(&db, &suggestions, selected).unwrap();
            let expected = golden["support"][selected.to_string()].as_array().unwrap();
            assert_eq!(actual.missing, selected == 999);
            if selected == 999 {
                // Proposta sparita: solo il menu, senza riquadro né pulsante.
                assert_eq!(actual.components.len(), 1);
                assert_eq!(normalize_rows(&actual.components, true), normalize_rows(expected, true));
            } else {
                assert_eq!(actual.components[0], expected[0], "riquadro della scheda {selected}");
                // Gli ID di menu e pulsante sono casuali in Python: qui sono fissi e riconoscibili.
                assert_eq!(normalize_rows(&actual.components[1..], true), normalize_rows(&expected[1..], true));
                assert_eq!(actual.components[2]["components"][0]["custom_id"], format!("hub:support-confirm:{selected}"));
            }
            assert_eq!(actual.components.last().unwrap()["type"], 1);
        }
        assert_eq!(notice_components("⚠️ Questa proposta non è più disponibile."), golden["notice"].as_array().unwrap().clone());
    }

    #[test]
    fn bodies_carry_the_v2_flag_and_no_legacy_fields() {
        let create = message_body(vec![text_display("ciao")], false);
        assert_eq!(create["flags"], 32768);
        assert!(create.get("content").is_none() && create.get("embeds").is_none(), "V2 vieta contenuto ed embed");
        assert_eq!(create["allowed_mentions"], json!({"parse": []}));
        let edit = message_body(vec![text_display("ciao")], true);
        assert!(edit["content"].is_null() && edit["embeds"] == json!([]), "in modifica si azzera il vecchio contenuto");
        assert_eq!(edit["flags"], 32768);

        let reply = interaction_response_body(vec![text_display("x")], ResponseKind::NewMessage, true);
        assert_eq!((reply["type"].as_u64(), reply["data"]["flags"].as_u64()), (Some(4), Some(32768 | 64)));
        let public = interaction_response_body(vec![], ResponseKind::NewMessage, false);
        assert_eq!(public["data"]["flags"], 32768);
        let update = interaction_response_body(vec![], ResponseKind::UpdateMessage, true);
        assert_eq!((update["type"].as_u64(), update["data"]["flags"].as_u64()), (Some(7), Some(32768)));
    }

    #[test]
    fn rust_built_messages_round_trip_through_serenitys_message_parser() {
        // Il messaggio che Discord restituisce dopo l'invio (e quello degli eventi) si legge sempre.
        let db = fixture_db();
        let mut message = v2_message();
        message["components"] = Value::Array(hub_json(&db));
        let parsed: poise::serenity_prelude::Message = serde_json::from_value(message).expect("messaggio V2 costruito da noi");
        assert_eq!(parsed.components.len(), 2, "il riquadro è saltato, pulsanti e menu restano");
    }
}
