//! Questo modulo gestisce le comunicazioni pubbliche della community,
//! come la visualizzazione della guida e del benvenuto automatico per i nuovi membri.
//! Il sistema è progettato per essere interfacciabile con script esterni (come quelli in Python,
//! qui richiamati concettualmente dai nomi dei file `.py` menzionati).
//!
//! L'architettura utilizza i "Database Embed" (messaggi grafici formattati per Discord):
//! sono funzioni pure che, partendo dai dati nel Database, dal tempo corrente e dalle impostazioni,
//! producono un oggetto `EmbedSpec` che il bot invia poi sulla piattaforma.
//! Questo garantisce coerenza tra quanto visto sul sito web e quanto inviato su Discord.

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

/// Genera il messaggio "guida" che spiega come partecipare alla Game Night della community.
///
/// Gli Embed su Discord sono dei box colorati che contengono testo formattato, link e immagini.
/// Questa funzione costruisce l'Embed richiamando impostazioni salvate nel database (come
/// il numero massimo di proposte e voti consentiti) e lo formatta in modo leggibile.
///
/// `db`: la connessione al database per leggere le regole attuali.
/// `public_url`: l'indirizzo web pubblico per accedere alla dashboard (se configurato).
pub fn guide_embed(db: &impl Connected, public_url: &str) -> DbResult<EmbedSpec> {
    // Inizializziamo l'Embed con un titolo, una descrizione generale e il colore primario definito
    // nelle configurazioni del progetto.
    let mut embed = EmbedSpec::new(
        "⚔️ Benvenuto nella Game Night!",
        "La community decide insieme cosa fare. Bastano tre semplici regole per partecipare.",
        COLOR_PRIMARY,
    )
    .field(
        "1 · Proponi e vota",
        // Utilizziamo le impostazioni dinamiche del database per informare l'utente
        // su quante proposte e voti ha a disposizione.
        format!(
            "Puoi inviare fino a **{} proposte ogni settimana** e hai **{} voti per sondaggio**. Dal pannello puoi anche sostenere le proposte della community.",
            settings::max_proposals(db)?,
            settings::max_votes(db)?
        ),
        false,
    )
    .field(
        "2 · Tieni d'occhio il reset",
        // Il "reset" è il momento in cui la settimana di attività termina e le proposte
        // vengono archiviate. Informiamo l'utente che può controllare il tempo rimanente.
        format!(
            "{} si chiude la settimana: i risultati vengono salvati e si riparte. Il conto alla rovescia è visibile nella dashboard e nei sondaggi.",
            capitalize(&settings::reset_label(db)?)
        ),
        false,
    )
    .field(
        "3 · Costruisci il tuo profilo",
        // Spiegazione dei vantaggi per l'utente, come punti ed esperienza.
        "Partecipare fa guadagnare punti, livelli e badge. Consulta la classifica o usa `/profilo` per vedere il tuo percorso. La classifica stagionale riparte a ogni stagione; i punti cumulativi restano intatti.",
        false,
    )
    .field(
        "🎤 Eventi live e privacy",
        // Informativa sulla privacy: spieghiamo che il bot non registra l'audio,
        // ma tiene solo traccia statistica degli eventi live per i report.
        "Negli eventi live abilitati dallo staff vengono conservati partecipanti e durate stimate di presenza/parlato. Il bot non salva né trasmette audio; i report dettagliati restano visibili allo staff fino alla loro eliminazione.",
        false,
    );
    // Se c'è un sito web, aggiungiamo il link nell'embed.
    let public_url = public_url.trim_end_matches('/');
    if !public_url.is_empty() {
        embed = embed.field("🌐 Community", format!("[Apri il sito della community]({public_url}/community)"), false);
    }
    // Aggiungiamo un piè di pagina (footer) come invito all'azione.
    Ok(embed.footer("Apri la dashboard nel canale sondaggi per iniziare."))
}

// --- /classifica-stagione --------------------------------------------------------------------

/// Genera un Embed che mostra la classifica dei membri per la stagione in corso.
///
/// La funzione:
/// 1. Identifica la stagione attuale in base alla data.
/// 2. Recupera dal database i top 10 membri attuali.
/// 3. Recupera i vincitori delle stagioni passate.
/// 4. Formatta tutto in un messaggio graficamente accattivante per Discord.
///
/// `now`: il tempo corrente, fondamentale per capire in quale stagione ci troviamo.
/// `label`: una funzione che traduce un ID utente in un nome leggibile (menzione o nome server).
pub fn season_embed(db: &impl Connected, now: DateTime<Tz>, label: &dyn Fn(i64) -> String) -> DbResult<EmbedSpec> {
    let season = season_for_date(now.date_naive());
    let today = now.date_naive();

    // Eseguiamo le interrogazioni al database per ottenere dati sui partecipanti.
    let (rows, champions) = db.with_conn(|connection| -> DbResult<_> {
        Ok((season_leaderboard_rows(connection, &season.key, 10)?, completed_season_champions(connection, today, 5)?))
    })?;

    // Calcoliamo la data di fine stagione per mostrarla correttamente come timestamp Discord.
    let end = NaiveDate::parse_from_str(&season.ends_at, "%Y-%m-%d")
        .map(|date| from_wall_clock(date.and_time(NaiveTime::MIN)).timestamp())
        .unwrap_or_default();

    // Prepariamo la lista dei nomi della classifica per inserirla nell'Embed.
    let lines: Vec<String> =
        rows.iter().enumerate().map(|(index, row)| format!("`{}`. **{}** · `{} punti`", index + 1, label(row.user_id), row.points)).collect();

    // Raggruppiamo i campioni passati per stagione.
    let mut by_season: Vec<Vec<&SeasonChampion>> = Vec::new();
    for champion in &champions {
        match by_season.last_mut() {
            Some(group) if group[0].season_key == champion.season_key => group.push(champion),
            _ => by_season.push(vec![champion]),
        }
    }

    // Creiamo le righe per l'archivio dei vincitori passati.
    let archive_lines: Vec<String> = by_season
        .iter()
        .map(|winners| {
            // Se ci sono molti vincitori, ne mostriamo alcuni e raggruppiamo gli altri.
            let shown = &winners[..winners.len().min(5)];
            let mut names = shown.iter().map(|winner| label(winner.user_id)).collect::<Vec<_>>().join(", ");
            if winners.len() > shown.len() {
                names.push_str(&format!(" e altri {}", winners.len() - shown.len()));
            }
            format!("🏅 **{}** · {names} · `{} punti`", winners[0].label, winners[0].points)
        })
        .collect();

    // Scegliamo un'emoji basata sulla stagione per un tocco estetico.
    let emoji = match season.key.split('-').next().unwrap_or_default() {
        "inverno" => "❄️",
        "primavera" => "🌸",
        "estate" => "☀️",
        "autunno" => "🍂",
        _ => "🏆",
    };

    let board = if lines.is_empty() { "La classifica si popolerà con le prossime attività.".to_string() } else { lines.join("\n") };

    // Costruiamo l'Embed finale.
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

    // Se ci sono campioni passati, aggiungiamo la sezione dedicata.
    if !archive_lines.is_empty() {
        embed = embed.field("🏆 Campioni stagionali", truncate_chars(&archive_lines.join("\n"), 1024), false);
    }
    Ok(embed.footer("Le stagioni iniziano con l'attivazione della funzione; i punti storici non sono ricalcolati."))
}

// --- benvenuto -------------------------------------------------------------------------------

/// Rappresenta i dati di un nuovo membro appena entrato nel server.
/// Questi dati vengono usati per popolare il messaggio di benvenuto dinamico.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedMember {
    pub id: u64,
    pub display_name: String,
    pub avatar_url: String,
    pub guild_id: u64,
    pub guild_name: String,
    pub member_count: u64,
}

/// Gestisce l'invio del messaggio di benvenuto.
///
/// Questa funzione viene chiamata automaticamente dal sistema quando un nuovo utente entra.
/// Il processo è:
/// 1. Controlla se la funzione benvenuto è attiva nelle impostazioni.
/// 2. Recupera il canale dedicato al benvenuto.
/// 3. Verifica che il canale sia valido e accessibile.
/// 4. Carica le impostazioni di personalizzazione.
/// 5. Costruisce l'Embed (il box colorato) e lo invia menzionando l'utente.
pub async fn welcome_member<A: DiscordApi>(db: &Database, api: &A, member: &JoinedMember) {
    // Prima verifica: la funzione benvenuto è abilitata?
    if db.get_setting_or("welcome_enabled", "1").ok().as_deref() != Some("1") {
        return;
    }

    // Seconda verifica: quale canale è configurato per il benvenuto?
    let Some(raw) = db.get_setting("welcome_channel").ok().flatten().filter(|value| !value.is_empty()) else {
        return;
    };
    let Ok(channel) = raw.trim().parse::<u64>() else {
        error!("Impossibile recuperare il canale di benvenuto {raw}.");
        return;
    };

    // Terza verifica: il canale esiste ed è testuale?
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

    // Costruiamo il messaggio usando i modelli configurati nello staff panel.
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

    // Inviamo il messaggio. Utilizziamo MessagePayload per menzionare l'utente specificamente.
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
