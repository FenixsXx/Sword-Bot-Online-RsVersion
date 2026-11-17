//! Gli embed pubblici della community e il benvenuto automatico (`cogs/community.py` e
//! `cogs/seasons.py`): guida, classifica stagionale e messaggio ai nuovi membri.
//!
//! Gli embed sono funzioni pure su database, orologio ed etichette, quindi si provano con gli
//! stessi risultati del codice Python.
// Collegato a `interactions` (comandi) e al gestore degli eventi in `main` (benvenuto).
#![allow(dead_code)]

use chrono::{DateTime, NaiveDate, NaiveTime};
use chrono_tz::Tz;
use tracing::{error, warn};

use crate::cards::EmbedSpec;
use crate::config::{COLOR_PRESTIGE, COLOR_PRIMARY};
use crate::database::{Connected, Database, DbResult};
use crate::discord_api::{ChannelKind, DiscordApi, MessagePayload};
use crate::embeds::discord_timestamp_for;
use crate::formatting::{capitalize, truncate_chars};
use crate::schedule::from_wall_clock;
use crate::seasonal::{SeasonChampion, completed_season_champions, season_for_date, season_leaderboard_rows};
use crate::settings;
use crate::welcome::{NewMember, build_welcome_embed, welcome_settings};

// --- /guida ----------------------------------------------------------------------------------

/// «Come partecipare alla Game Night». `public_url` è `EVENT_OVERLAY_PUBLIC_URL` (può essere vuoto).
pub fn guide_embed(db: &impl Connected, public_url: &str) -> DbResult<EmbedSpec> {
    let mut embed = EmbedSpec::new(
        "⚔️ Benvenuto nella Game Night!",
        "La community decide insieme cosa fare. Bastano tre semplici regole per partecipare.",
        COLOR_PRIMARY,
    )
    .field(
        "1 · Proponi e vota",
        format!(
            "Puoi inviare fino a **{} proposte ogni settimana** e hai **{} voti per sondaggio**. Dal pannello puoi anche sostenere le proposte della community.",
            settings::max_proposals(db)?,
            settings::max_votes(db)?
        ),
        false,
    )
    .field(
        "2 · Tieni d'occhio il reset",
        format!(
            "{} si chiude la settimana: i risultati vengono salvati e si riparte. Il conto alla rovescia è visibile nella dashboard e nei sondaggi.",
            capitalize(&settings::reset_label(db)?)
        ),
        false,
    )
    .field(
        "3 · Costruisci il tuo profilo",
        "Partecipare fa guadagnare punti, livelli e badge. Consulta la classifica o usa `/profilo` per vedere il tuo percorso. La classifica stagionale riparte a ogni stagione; i punti cumulativi restano intatti.",
        false,
    )
    .field(
        "🎤 Eventi live e privacy",
        "Negli eventi live abilitati dallo staff vengono conservati partecipanti e durate stimate di presenza/parlato. Il bot non salva né trasmette audio; i report dettagliati restano visibili allo staff fino alla loro eliminazione.",
        false,
    );
    let public_url = public_url.trim_end_matches('/');
    if !public_url.is_empty() {
        embed = embed.field("🌐 Community", format!("[Apri il sito della community]({public_url}/community)"), false);
    }
    Ok(embed.footer("Apri la dashboard nel canale sondaggi per iniziare."))
}

// --- /classifica-stagione --------------------------------------------------------------------

/// La classifica della stagione corrente e i campioni di quelle concluse.
///
/// `label` dà il nome con cui mostrare un utente (in Python il nome nel server se il membro è in
/// cache, altrimenti la menzione).
pub fn season_embed(db: &impl Connected, now: DateTime<Tz>, label: &dyn Fn(i64) -> String) -> DbResult<EmbedSpec> {
    let season = season_for_date(now.date_naive());
    let today = now.date_naive();
    let (rows, champions) = db.with_conn(|connection| -> DbResult<_> {
        Ok((season_leaderboard_rows(connection, &season.key, 10)?, completed_season_champions(connection, today, 5)?))
    })?;
    // La stagione termina a mezzanotte (ora di Roma) del primo giorno della successiva.
    let end = NaiveDate::parse_from_str(&season.ends_at, "%Y-%m-%d")
        .map(|date| from_wall_clock(date.and_time(NaiveTime::MIN)).timestamp())
        .unwrap_or_default();
    let lines: Vec<String> =
        rows.iter().enumerate().map(|(index, row)| format!("`{}`. **{}** · `{} punti`", index + 1, label(row.user_id), row.points)).collect();

    // Campioni raggruppati per stagione, nell'ordine di arrivo.
    let mut by_season: Vec<Vec<&SeasonChampion>> = Vec::new();
    for champion in &champions {
        match by_season.last_mut() {
            Some(group) if group[0].season_key == champion.season_key => group.push(champion),
            _ => by_season.push(vec![champion]),
        }
    }
    let archive_lines: Vec<String> = by_season
        .iter()
        .map(|winners| {
            // I pareggi non hanno un limite: si mostrano i primi e si riassume il resto, altrimenti
            // il campo può superare i 1024 caratteri di Discord.
            let shown = &winners[..winners.len().min(5)];
            let mut names = shown.iter().map(|winner| label(winner.user_id)).collect::<Vec<_>>().join(", ");
            if winners.len() > shown.len() {
                names.push_str(&format!(" e altri {}", winners.len() - shown.len()));
            }
            format!("🏅 **{}** · {names} · `{} punti`", winners[0].label, winners[0].points)
        })
        .collect();

    let emoji = match season.key.split('-').next().unwrap_or_default() {
        "inverno" => "❄️",
        "primavera" => "🌸",
        "estate" => "☀️",
        "autunno" => "🍂",
        _ => "🏆",
    };
    let board = if lines.is_empty() { "La classifica si popolerà con le prossime attività.".to_string() } else { lines.join("\n") };
    let mut embed = EmbedSpec::new(
        format!("{emoji} Classifica · {}", season.label),
        format!("La stagione termina {}.\n\n{board}", discord_timestamp_for(end)),
        COLOR_PRESTIGE,
    )
    .field(
        "Come si accumulano i punti",
        "Proposta `+1` · supporto ricevuto `+2` · ballottaggio `+5` · vittoria `+10`. I punti della stagione ripartono a ogni cambio stagione; il profilo e la classifica cumulativa non vengono azzerati.",
        false,
    );
    if !archive_lines.is_empty() {
        embed = embed.field("🏆 Campioni stagionali", truncate_chars(&archive_lines.join("\n"), 1024), false);
    }
    Ok(embed.footer("Le stagioni iniziano con l'attivazione della funzione; i punti storici non sono ricalcolati."))
}

// --- benvenuto -------------------------------------------------------------------------------

/// Chi è appena entrato, con i dati del server che servono al messaggio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedMember {
    pub id: u64,
    pub display_name: String,
    pub avatar_url: String,
    pub guild_id: u64,
    pub guild_name: String,
    pub member_count: u64,
}

/// Accoglie un nuovo membro nel canale di benvenuto (`on_member_join`): solo se l'accoglienza è
/// attiva e il canale è un canale testuale di questo server. Ogni problema si registra nei log,
/// senza errori verso chi ha causato l'evento.
pub async fn welcome_member<A: DiscordApi>(db: &Database, api: &A, member: &JoinedMember) {
    if db.get_setting_or("welcome_enabled", "1").ok().as_deref() != Some("1") {
        return;
    }
    let Some(raw) = db.get_setting("welcome_channel").ok().flatten().filter(|value| !value.is_empty()) else {
        return;
    };
    let Ok(channel) = raw.trim().parse::<u64>() else {
        error!("Impossibile recuperare il canale di benvenuto {raw}.");
        return;
    };
    match api.channel_kind(member.guild_id, channel).await {
        Ok(ChannelKind::Text) => {}
        Ok(_) => {
            warn!("Canale di benvenuto {channel} non trovato o non accessibile.");
            return;
        }
        Err(e) => {
            error!("Impossibile recuperare il canale di benvenuto {channel}: {e}");
            return;
        }
    }
    let settings = welcome_settings(|key, default| db.get_setting_or(key, default).unwrap_or_else(|_| default.to_string()));
    let embed = build_welcome_embed(
        &settings,
        &NewMember {
            username: &member.display_name,
            server_name: &member.guild_name,
            member_count: member.member_count,
            avatar_url: &member.avatar_url,
        },
    );
    let payload = MessagePayload::embed(embed).with_content(format!("<@{}>", member.id)).mentioning_users();
    if let Err(e) = api.send_message(channel, payload).await {
        error!("Impossibile inviare il benvenuto per il membro {} nel canale {channel}: {e}", member.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::discord_api::testing::{Call, FakeApi};
    use chrono::TimeZone;
    use serde_json::{Value, json};

    fn db() -> Database {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()).unwrap();
        db
    }

    fn new_member(avatar: &str, member_count: u64) -> JoinedMember {
        JoinedMember { id: 800, display_name: "Nuova Persona".into(), avatar_url: avatar.into(), guild_id: 1, guild_name: "Sword Bot Online".into(), member_count }
    }

    fn summary(embed: &Value) -> Value {
        json!({
            "title": embed["title"], "description": embed["description"], "color": embed["color"],
            "fields": [], "thumbnail": embed["thumbnail"]["url"], "image": embed["image"]["url"], "footer": embed["footer"]["text"],
        })
    }

    /// Stessi casi di `golden_community.py` per `on_member_join`: quando non scrive nulla e che cosa
    /// scrive (contenuto, embed e menzioni consentite).
    #[tokio::test]
    async fn welcome_matches_the_python_implementation() {
        let golden: Value = serde_json::from_str(include_str!("golden/community_expected.json")).unwrap();
        let db = db();
        let api = FakeApi::default();
        let mut cases = golden["benvenuto"].as_array().unwrap().iter();

        let mut join = async |label: &str, member: &JoinedMember| {
            let case = cases.next().unwrap();
            assert_eq!(case["label"], label, "ordine dei casi");
            let before = api.calls().len();
            welcome_member(&db, &api, member).await;
            let sent: Vec<Call> = api.calls()[before..].to_vec();
            let wanted = case["sent"].as_array().unwrap();
            assert_eq!(sent.len(), wanted.len(), "{label}: messaggi inviati");
            for (call, wanted) in sent.iter().zip(wanted) {
                let Call::Send { channel, message } = call else { panic!("atteso un invio") };
                assert_eq!(json!(channel), wanted["channel"], "{label}");
                let stored = api.message(*channel, *message).unwrap();
                assert_eq!(json!(stored.content), wanted["content"], "{label}: contenuto");
                assert_eq!(summary(&stored.embeds[0]), wanted["embed"], "{label}: embed");
                assert_eq!(json!(stored.mentions_users), wanted["users"], "{label}: menzioni");
            }
        };

        let member = new_member("https://cdn.example/800.png", 1234);
        db.set_setting("welcome_enabled", "0").unwrap();
        join("disattivato", &member).await;
        db.set_setting("welcome_enabled", "1").unwrap();
        join("senza_canale", &member).await;
        db.set_setting("welcome_channel", 4000).unwrap();
        join("canale_non_in_cache", &member).await;
        api.set_channel_kind(4000, ChannelKind::Voice);
        join("canale_vocale", &member).await;
        api.set_channel_kind(4000, ChannelKind::Text);
        api.set_channel_guild(4000, 2);
        join("altro_server", &member).await;
        api.set_channel_guild(4000, 1);
        join("ok_predefinito", &member).await;
        db.set_setting("welcome_title", "Ciao {user}! ({member_count})").unwrap();
        db.set_setting("welcome_body", "Sei il {ordinal} su {server}.").unwrap();
        db.set_setting("welcome_gif", "https://gif.example/w.gif").unwrap();
        join("ok_personalizzato", &member).await;
        db.set_setting("welcome_gif", "http://insicuro.example/w.gif").unwrap();
        join("avatar_gif_non_https", &new_member("http://cdn.example/800.png", 0)).await;
        db.set_setting("welcome_channel", "9999").unwrap();
        join("canale_inesistente", &member).await;
        db.set_setting("welcome_channel", "abc").unwrap();
        join("canale_non_numerico", &member).await;
        assert!(cases.next().is_none(), "tutti i casi Python verificati");
    }

    #[tokio::test]
    async fn welcome_survives_a_channel_that_refuses_the_message() {
        let db = db();
        let api = FakeApi::default();
        db.set_setting("welcome_channel", 4000).unwrap();
        api.set_channel_kind(4000, ChannelKind::Text);
        api.fail_send(Some(crate::discord_api::ApiError::Forbidden));
        welcome_member(&db, &api, &new_member("https://cdn.example/800.png", 5)).await;
        assert!(api.calls().is_empty());
    }
}
