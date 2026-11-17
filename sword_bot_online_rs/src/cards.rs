//! Embed mostrati dal bot: sondaggi, dashboard, classifiche, profili e schede
//! (equivalente di `sfo/cards.py`).
//!
//! Ogni funzione restituisce un [`EmbedSpec`]: una descrizione semplice dell'embed (titolo, campi,
//! colore…) che si può ispezionare e confrontare, e che [`EmbedSpec::build`] trasforma in un
//! `CreateEmbed` di serenity. L'orologio (`datetime.now(TZ)`) è il parametro `now`.
// Verrà usato da dashboard, sondaggi, comandi e sito.
#![allow(dead_code)]

use chrono::DateTime;
use chrono_tz::Tz;
use poise::serenity_prelude::{CreateEmbed, CreateEmbedFooter, Timestamp};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::Value;

use crate::config::{
    BAR_LEN, COLOR_PRESTIGE, COLOR_PRIMARY, HIDDEN_BADGE_HINTS, HIDDEN_BADGES,
};
use crate::database::{self, Connected, DbResult};
use crate::embeds::discord_timestamp_for;
use crate::formatting::{
    capitalize, casefold, default_progress_bar, proposal_image_url, proposal_type_emoji, truncate_chars,
};
use crate::polls::{PollRow, list_active_polls, poll_voter_count};
use crate::queries::{PollOption, current_suggestions, poll_options, suggestion_supporter_counts, suggestion_supporter_ranking};
use crate::schedule::next_scheduled_reset;
use crate::seasonal::{season_champion_titles, season_for_date, season_points_for_user};
use crate::settings;
use crate::stats::{
    UserStats, Winner, contributor_leaderboard_rows, contributor_level_progress, contributor_points,
    user_hidden_badge_names,
};

// --- descrizione di un embed -----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

/// Un embed descritto con tipi semplici.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedSpec {
    pub title: Option<String>,
    pub description: Option<String>,
    pub color: u32,
    pub fields: Vec<EmbedField>,
    pub footer: Option<String>,
    pub thumbnail: Option<String>,
    pub image: Option<String>,
    pub timestamp: Option<DateTime<Tz>>,
}

impl EmbedSpec {
    pub fn new(title: impl Into<String>, description: impl Into<String>, color: u32) -> Self {
        Self {
            title: Some(title.into()),
            description: Some(description.into()),
            color,
            fields: Vec::new(),
            footer: None,
            thumbnail: None,
            image: None,
            timestamp: None,
        }
    }

    pub fn field(mut self, name: impl Into<String>, value: impl Into<String>, inline: bool) -> Self {
        self.fields.push(EmbedField { name: name.into(), value: value.into(), inline });
        self
    }

    pub fn footer(mut self, text: impl Into<String>) -> Self {
        self.footer = Some(text.into());
        self
    }

    pub fn timestamp(mut self, at: DateTime<Tz>) -> Self {
        self.timestamp = Some(at);
        self
    }

    pub fn description_text(&self) -> &str {
        self.description.as_deref().unwrap_or_default()
    }

    /// L'embed di serenity pronto da inviare.
    pub fn build(&self) -> CreateEmbed {
        let mut embed = CreateEmbed::new().colour(self.color);
        if let Some(title) = &self.title {
            embed = embed.title(title);
        }
        if let Some(description) = &self.description {
            embed = embed.description(description);
        }
        for field in &self.fields {
            embed = embed.field(&field.name, &field.value, field.inline);
        }
        if let Some(url) = &self.thumbnail {
            embed = embed.thumbnail(url);
        }
        if let Some(url) = &self.image {
            embed = embed.image(url);
        }
        if let Some(text) = &self.footer {
            embed = embed.footer(CreateEmbedFooter::new(text));
        }
        if let Some(at) = self.timestamp {
            if let Ok(timestamp) = Timestamp::from_unix_timestamp(at.timestamp()) {
                embed = embed.timestamp(timestamp);
            }
        }
        embed
    }
}

const MEDALS: [&str; 3] = ["🥇", "🥈", "🥉"];

/// Campi `bool(x)` di Python per interi nullable.
fn truthy(value: Option<i64>) -> bool {
    value.is_some_and(|flag| flag != 0)
}

// --- record della community ------------------------------------------------------------------

pub fn build_community_records_embed(db: &impl Connected) -> DbResult<EmbedSpec> {
    let labels = [
        ("most_supported", "🏅 Proposta più supportata"),
        ("most_voted", "🗳️ Opzione più votata"),
        ("longest_streak", "🔥 Streak più lunga"),
        ("most_wins", "🏆 Più proposte vincitrici"),
    ];
    let mut embed = EmbedSpec::new(
        "🏅 Record della community",
        "Record registrati dai reset settimanali in poi. A parità resta il primo record raggiunto.",
        COLOR_PRESTIGE,
    );
    let rows: Vec<(String, i64, String, i64)> = db.with_conn(|connection| -> DbResult<_> {
        connection
            .prepare(
                "SELECT record_key, metric, label, week
                 FROM community_records
                 ORDER BY CASE record_key
                     WHEN 'most_supported' THEN 1
                     WHEN 'most_voted' THEN 2
                     WHEN 'longest_streak' THEN 3
                     WHEN 'most_wins' THEN 4
                     ELSE 5 END",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect()
    })?;
    for (key, metric, label, week) in rows {
        let unit = match key.as_str() {
            "most_supported" => "sostenitori",
            "most_voted" => "voti",
            "longest_streak" => "settimane",
            _ => "vittorie",
        };
        let name = labels.iter().find(|(known, _)| *known == key).map_or(key.as_str(), |(_, text)| *text);
        embed = embed.field(name, format!("**{label}** · `{metric} {unit}`\nSettimana `#{week}`"), true);
    }
    if embed.fields.is_empty() {
        let description = format!("{}\n\nI record appariranno dopo il primo reset con attività.", embed.description_text());
        embed.description = Some(description);
    }
    Ok(embed)
}

// --- riepilogo settimanale -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct HistoryOption {
    label: String,
    votes: i64,
    #[serde(default)]
    suggestion_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct HistoryPoll {
    is_community: bool,
    #[serde(default)]
    options: Vec<HistoryOption>,
}

/// Lo snapshot di una settimana archiviata (`history.data`, scritto dal reset).
#[derive(Debug, Deserialize)]
struct HistoryData {
    #[serde(default)]
    polls: Vec<HistoryPoll>,
    #[serde(default)]
    suggestions: Vec<serde::de::IgnoredAny>,
}

fn history_data(raw: &str) -> DbResult<HistoryData> {
    serde_json::from_str(raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

/// Un identificativo come lo stampa Python da JSON: `11` o `"11"` diventano `11`.
fn json_display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub fn build_weekly_recap_embed(
    db: &impl Connected,
    week: i64,
    winner: Option<&Winner>,
    now: DateTime<Tz>,
) -> DbResult<EmbedSpec> {
    let snapshot: Option<String> = db.with_conn(|connection| {
        connection
            .query_row("SELECT data FROM history WHERE week=? ORDER BY id DESC LIMIT 1", [week], |row| row.get(0))
            .optional()
    })?;
    let mut community_options: Vec<HistoryOption> = Vec::new();
    if let Some(raw) = snapshot {
        let data = history_data(&raw)?;
        if let Some(community) = data.polls.into_iter().find(|poll| poll.is_community) {
            community_options = community.options;
            // Il vincitore (dopo lo spareggio) è sempre il primo.
            community_options.sort_by_key(|option| {
                let is_winner = winner.is_some_and(|w| option.suggestion_id == Some(w.result.suggestion_id));
                (!is_winner, -option.votes, casefold(&option.label))
            });
        }
    }
    let top_lines: Vec<String> = community_options
        .iter()
        .take(3)
        .enumerate()
        .map(|(index, option)| format!("{} **{}** · `{} voti`", MEDALS[index], option.label, option.votes))
        .collect();

    let award: Option<(String, i64)> = db.with_conn(|connection| {
        connection
            .query_row("SELECT user_ids, points FROM weekly_awards WHERE week=?", [week], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()
    })?;
    let contributor_text = match &award {
        Some((user_ids, points)) => {
            let contributors: Vec<Value> = serde_json::from_str(user_ids).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
            })?;
            if contributors.is_empty() {
                "Nessun contributor questa settimana.".to_string()
            } else {
                let mentions = contributors.iter().map(|id| format!("<@{}>", json_display(id))).collect::<Vec<_>>().join(", ");
                format!("{mentions} · `{points} punti`")
            }
        }
        None => "Nessun contributor questa settimana.".to_string(),
    };
    let streak_records: Vec<String> = db.with_conn(|connection| -> DbResult<Vec<String>> {
        connection
            .prepare(
                "SELECT user_id, streak FROM weekly_streak_records
                 WHERE week=? ORDER BY streak DESC, user_id LIMIT 5",
            )?
            .query_map([week], |row| {
                Ok(format!("🔥 <@{}> · `{} settimane`", row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect()
    })?;

    let mut embed = EmbedSpec::new(
        format!("🏆 RISULTATI SETTIMANA `#{week}`"),
        "Ecco i risultati della settimana appena conclusa.",
        COLOR_PRESTIGE,
    )
    .timestamp(now)
    .field(
        "🥇 Classifica community",
        if top_lines.is_empty() { "Nessun voto nel sondaggio community.".to_string() } else { top_lines.join("\n") },
        false,
    );
    if let Some(winner) = winner {
        if let Some(tie_break) = winner.tie_break {
            embed = embed.field("⚖️ Spareggio", format!("**{}** vince: {tie_break}.", winner.result.name), false);
        }
    }
    embed = embed.field("🌟 Contributor della settimana", truncate_chars(&contributor_text, 1024), false).field(
        "🔥 Nuovi record streak",
        if streak_records.is_empty() { "Nessun nuovo record questa settimana.".to_string() } else { streak_records.join("\n") },
        false,
    );
    Ok(embed)
}

// --- sondaggio -------------------------------------------------------------------------------

/// Colore di un sondaggio (`#RRGGBB`); se non è valido, quello di default.
fn poll_color(color: Option<&str>) -> u32 {
    color
        .and_then(|text| u32::from_str_radix(text.trim_start_matches('#'), 16).ok())
        .unwrap_or(COLOR_PRIMARY)
}

/// Embed del messaggio di un sondaggio nel canale.
pub fn build_embed(db: &impl Connected, poll: &PollRow, now: DateTime<Tz>) -> DbResult<EmbedSpec> {
    let opts = poll_options(db, poll.id)?;
    let voters = poll_voter_count(db, poll.id)?;
    let top = opts.iter().map(|option| option.n).max().unwrap_or(0);
    let max_votes = settings::max_votes(db)?;

    let lines: Vec<String> = opts
        .iter()
        .map(|option| {
            let filled = if top > 0 { (option.n as f64 / top as f64 * BAR_LEN as f64).round_ties_even() as usize } else { 0 };
            let bar = format!("{}{}", "▰".repeat(filled), "▱".repeat(BAR_LEN - filled));
            let star = if top > 0 && option.n == top { " ★" } else { "" };
            let mut title = format!("**{}**{star}", option.label);
            if let Some(tipo) = option.tipo.as_deref().filter(|tipo| !tipo.is_empty()) {
                title.push_str(&format!(" · *{tipo}*"));
            }
            if let Some(link) = option.link.as_deref().filter(|link| !link.is_empty()) {
                title.push_str(&format!(" · [link]({link})"));
            }
            format!("{title}\n└ `{bar}` · `{} voti`", option.n)
        })
        .collect();

    let community = truthy(poll.is_community);
    let (intro, empty) = if community {
        (
            format!("Lo staff ha selezionato le proposte migliori. Hai `{max_votes} voti`: scegli le tue preferite."),
            "Nessuna proposta in votazione questa settimana.",
        )
    } else if opts.len() <= 10 {
        (
            format!(
                "Hai `{max_votes} voti` a disposizione: premi un'opzione per votare, ripremila per togliere il voto, oppure usa **📋 I miei voti**."
            ),
            "Nessuna opzione disponibile al momento.",
        )
    } else {
        (format!("Hai `{max_votes} voti` a disposizione: premi **🗳️ Vota** per scegliere."), "Nessuna opzione disponibile al momento.")
    };
    let body = if lines.is_empty() { format!("*{empty}*") } else { lines.join("\n\n") };
    let description = truncate_chars(&format!("{intro}\n\n──────────\n\n{body}"), 4000);

    // I timestamp <t:...> non vengono resi nel footer: la scadenza va nel testo.
    let poll_status = if !poll.is_active() {
        "🔒 Sondaggio chiuso".to_string()
    } else if let Some(closes_at) = poll.closes_at.filter(|closes_at| *closes_at != 0.0) {
        format!("⏳ Chiude <t:{}:R>", closes_at.floor() as i64)
    } else {
        format!("⏳ Reset {}", discord_timestamp_for(next_scheduled_reset(db, now)?.timestamp()))
    };
    let mut embed = EmbedSpec::new(
        format!("{}  {}", poll.emoji.as_deref().unwrap_or_default(), poll.name),
        truncate_chars(&format!("{poll_status}\n{description}"), 4096),
        poll_color(poll.color.as_deref()),
    );
    if community {
        let mut ranked: Vec<&PollOption> = opts.iter().collect();
        ranked.sort_by_key(|option| -option.n); // stabile: a parità resta l'ordine delle opzioni
        let leader = ranked.into_iter().find(|option| proposal_image_url(option.link.as_deref()).is_some());
        if let Some(leader) = leader.filter(|leader| leader.n > 0) {
            embed.thumbnail = proposal_image_url(leader.link.as_deref());
        }
    }
    Ok(embed.footer(format!("{voters} votanti · POLL_ID={}", poll.id)))
}

// --- storico ---------------------------------------------------------------------------------

/// Righe del riepilogo storico (le ultime `limit` settimane archiviate).
pub fn history_summary_lines(db: &impl Connected, limit: i64) -> DbResult<Vec<String>> {
    let rows: Vec<(i64, String, String)> = db.with_conn(|connection| -> DbResult<_> {
        connection
            .prepare("SELECT week, closed_at, data FROM history ORDER BY id DESC LIMIT ?")?
            .query_map([limit], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect()
    })?;
    let mut lines = Vec::new();
    for (index, (week, closed_at, raw)) in rows.iter().enumerate() {
        let data = history_data(raw)?;
        let mut winner = "nessun vincitore".to_string();
        let mut winner_votes = 0;
        if let Some(poll) = data.polls.iter().find(|poll| poll.is_community) {
            let top = poll.options.iter().map(|option| option.votes).max().unwrap_or(0);
            if top > 0 {
                let names: Vec<&str> = poll.options.iter().filter(|option| option.votes == top).map(|option| option.label.as_str()).collect();
                winner_votes = top;
                winner = if names.len() > 1 { format!("pareggio: {}", names.join(" / ")) } else { names[0].to_string() };
            }
        }
        let prefix = MEDALS.get(index).map_or_else(|| format!("#{week}"), |medal| medal.to_string());
        let winner = if winner_votes > 0 { format!("**{winner}** · {winner_votes} voti") } else { format!("*{winner}*") };
        lines.push(format!(
            "{prefix} {winner}\nSettimana `#{week}` · `{}` · `{} proposte`",
            truncate_chars(closed_at, 10),
            data.suggestions.len()
        ));
    }
    Ok(lines)
}

pub fn build_history_embed(db: &impl Connected) -> DbResult<EmbedSpec> {
    let lines = history_summary_lines(db, 10)?;
    Ok(EmbedSpec::new(
        "📚 Hall of Fame",
        if lines.is_empty() { "Nessuna settimana in archivio.".to_string() } else { lines.join("\n") },
        COLOR_PRESTIGE,
    ))
}

pub fn build_combined_hall_of_fame_embed(db: &impl Connected) -> DbResult<EmbedSpec> {
    let mut history = build_history_embed(db)?;
    let records = build_community_records_embed(db)?;
    history.description = Some(truncate_chars(
        &format!(
            "ℹ️ {} vengono archiviati i risultati della settimana, le proposte e il contributor della settimana. I record si aggiornano automaticamente e non sono modificabili manualmente.\n\n{}\n\n━━━━━━━━━━━━━━━━\n\n{}",
            capitalize(&settings::reset_label(db)?),
            history.description_text(),
            records.description_text()
        ),
        4000,
    ));
    history.fields.extend(records.fields);
    Ok(history)
}

// --- dashboard -------------------------------------------------------------------------------

/// Embed della dashboard "GAME NIGHT HUB".
pub fn build_dashboard_embed(db: &impl Connected, now: DateTime<Tz>) -> DbResult<EmbedSpec> {
    let polls = list_active_polls(db, None)?;
    let week = database::current_week(db)?;
    let closes_at = next_scheduled_reset(db, now)?;
    let (participants, proposal_count): (i64, i64) = db.with_conn(|connection| -> DbResult<_> {
        let participants = connection.query_row(
            "SELECT COUNT(DISTINCT v.user_id)
             FROM votes v
             JOIN options o ON o.id=v.option_id
             JOIN polls p ON p.id=o.poll_id
             WHERE p.active=1",
            [],
            |row| row.get(0),
        )?;
        let proposals = connection.query_row(
            "SELECT COUNT(DISTINCT suggestion_id) FROM proposals WHERE week=?",
            [week],
            |row| row.get(0),
        )?;
        Ok((participants, proposals))
    })?;

    // (voti, etichetta, nome del sondaggio): il primo con più voti.
    let mut most_voted: Option<(i64, String, String)> = None;
    for poll in &polls {
        for option in poll_options(db, poll.id)? {
            if most_voted.as_ref().is_none_or(|best| option.n > best.0) {
                most_voted = Some((option.n, option.label, poll.name.clone()));
            }
        }
    }
    let recent = current_suggestions(db, 3, false)?;
    let supporters = suggestion_supporter_counts(db, week, &recent.iter().map(|s| s.id).collect::<Vec<_>>())?;
    let spotlight = suggestion_supporter_ranking(db, week, 1, true)?;

    let community_summary = match polls.iter().find(|poll| truthy(poll.is_community)) {
        None => "Sondaggio community non configurato.".to_string(),
        Some(community) => {
            let mut options = poll_options(db, community.id)?;
            options.sort_by_key(|option| (-option.n, casefold(&option.label)));
            match options.first() {
                None => "In attesa delle scelte dello staff.".to_string(),
                Some(first) if first.n == 0 => format!("{} proposte pronte · nessun voto ancora.", options.len()),
                Some(_) => options
                    .iter()
                    .take(3)
                    .enumerate()
                    .map(|(index, option)| format!("{}. **{}** · `{} voti`", index + 1, option.label, option.n))
                    .collect::<Vec<_>>()
                    .join("\n"),
            }
        }
    };

    let most_voted_text = match &most_voted {
        Some((votes, label, poll_name)) if *votes > 0 => format!("**{label}** · `{votes} voti`\n{poll_name}"),
        _ => "In attesa dei primi voti.".to_string(),
    };
    let spotlight_text = match spotlight.first() {
        Some(proposal) => {
            let proposer: Option<i64> = db.with_conn(|connection| {
                connection
                    .query_row(
                        "SELECT user_id FROM proposals WHERE suggestion_id=? AND week=? ORDER BY id LIMIT 1",
                        [proposal.id, week],
                        |row| row.get(0),
                    )
                    .optional()
            })?;
            let mut text = format!(
                "{} **{}**\n└ 👥 `{} sostenitori`",
                proposal_type_emoji(&proposal.tipo),
                proposal.name,
                proposal.supporters
            );
            if let Some(user_id) = proposer {
                text.push_str(&format!("\nProposta da <@{user_id}>"));
            }
            text
        }
        None => "Nessuna proposta in attesa.".to_string(),
    };
    let mut poll_names: Vec<String> =
        polls.iter().take(10).map(|poll| format!("└ {} {}", poll.emoji.as_deref().unwrap_or_default(), poll.name)).collect();
    if polls.len() > 10 {
        poll_names.push(format!("… e altri {}", polls.len() - 10));
    }
    let recent_lines: Vec<String> = recent
        .iter()
        .map(|item| {
            format!(
                "{} **{}**\n└ {} · `{} sostenitori`",
                proposal_type_emoji(&item.tipo),
                item.name,
                item.tipo,
                supporters.get(&item.id).copied().unwrap_or(0)
            )
        })
        .collect();

    Ok(EmbedSpec::new(
        "🎮 GAME NIGHT HUB",
        "Proponi un'attività, apri un sondaggio per votare oppure consulta classifica e storico.",
        COLOR_PRIMARY,
    )
    .timestamp(now)
    .field(format!("Settimana `#{week}`"), format!("Chiusura <t:{}:R>", closes_at.timestamp()), true)
    .field("👥 Partecipanti", format!("`{participants}`"), true)
    .field("✨ Proposte", format!("`{proposal_count}`"), true)
    .field("🔥 Più votato ora", most_voted_text, true)
    .field("⭐ Proposta del momento", truncate_chars(&spotlight_text, 1024), true)
    .field("🏆 Ballottaggio community", truncate_chars(&community_summary, 1024), false)
    .field(
        "Sondaggi attivi",
        if poll_names.is_empty() { "Nessun sondaggio attivo.".to_string() } else { truncate_chars(&poll_names.join("\n"), 1024) },
        false,
    )
    .field(
        "✨ Proposte recenti",
        if recent_lines.is_empty() { "Nessuna proposta recente.".to_string() } else { truncate_chars(&recent_lines.join("\n"), 1024) },
        false,
    )
    .footer("Usa i pulsanti qui sotto: non serve conoscere i comandi."))
}

// --- classifiche -----------------------------------------------------------------------------

pub fn build_leaderboard_embed(db: &impl Connected) -> DbResult<EmbedSpec> {
    let week = database::current_week(db)?;
    let mut embed = EmbedSpec::new("📊 Classifica della settimana", format!("Settimana #{week} · voti anonimi"), COLOR_PRIMARY);
    let polls = list_active_polls(db, Some(25))?;
    for poll in &polls {
        let mut options = poll_options(db, poll.id)?;
        options.sort_by_key(|option| (-option.n, casefold(&option.label)));
        let voters = poll_voter_count(db, poll.id)?;
        let top: Vec<String> = options
            .iter()
            .take(3)
            .enumerate()
            .map(|(index, option)| format!("{}. {} — `{} voti`", index + 1, option.label, option.n))
            .collect();
        embed = embed.field(
            truncate_chars(&format!("{} {} · `{voters} votanti`", poll.emoji.as_deref().unwrap_or_default(), poll.name), 256),
            if top.is_empty() { "Nessuna opzione.".to_string() } else { top.join("\n") },
            true,
        );
    }
    let ranking = suggestion_supporter_ranking(db, week, 5, false)?;
    let ranking_block = (!ranking.is_empty()).then(|| {
        let lines: Vec<String> = ranking
            .iter()
            .enumerate()
            .map(|(index, row)| format!("{}. **{}** · `{} sostenitori`", index + 1, row.name, row.supporters))
            .collect();
        format!("\n\n**👍 Proposte più sostenute**\n{}", lines.join("\n"))
    });
    if let Some(block) = &ranking_block {
        embed.description = Some(format!("{}{block}", embed.description_text()));
    }
    if polls.is_empty() {
        embed.description = Some(format!("Non ci sono sondaggi attivi al momento.{}", ranking_block.unwrap_or_default()));
    }
    Ok(embed)
}

pub fn build_contributor_leaderboard_embed(db: &impl Connected, limit: usize) -> DbResult<EmbedSpec> {
    let rows = db.with_conn(|connection| contributor_leaderboard_rows(connection, limit))?;
    let description = if rows.is_empty() {
        "Nessun contributo registrato: la classifica si aggiornerà con la partecipazione.".to_string()
    } else {
        rows.iter()
            .enumerate()
            .map(|(index, row)| {
                let place = MEDALS.get(index).map_or_else(|| format!("**{}.**", index + 1), |medal| medal.to_string());
                format!("{place} <@{}> — `{} punti` · {}", row.user_id, row.points, row.level)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(EmbedSpec::new("🏆 Classifica utenti", truncate_chars(&description, 4000), COLOR_PRESTIGE).footer(format!(
        "Punti: proposta +1 · supporto ricevuto +2 · ballottaggio +5 · vittoria +10. Classifica dal {}.",
        database::setting_or(db, "stats_since", "oggi")?
    )))
}

// --- profili ---------------------------------------------------------------------------------

fn user_totals(connection: &Connection, user_id: i64) -> DbResult<UserStats> {
    Ok(connection
        .query_row(
            "SELECT proposals_submitted, supports_given, supports_received,
                    ballot_entries, winning_proposals
             FROM user_stats WHERE user_id=?",
            [user_id],
            |row| {
                Ok(UserStats {
                    proposals_submitted: row.get(0)?,
                    supports_given: row.get(1)?,
                    supports_received: row.get(2)?,
                    ballot_entries: row.get(3)?,
                    winning_proposals: row.get(4)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

/// `(livello, barra e testo di avanzamento)` per un totale di punti.
fn level_progress(points: i64) -> (&'static str, String) {
    let (level, next_level, _, next_threshold) = contributor_level_progress(points);
    let progress = if next_threshold > 0 {
        format!("{} `{points}/{next_threshold} punti` · prossimo: {next_level}", default_progress_bar(points, next_threshold))
    } else {
        format!("{} livello massimo raggiunto", default_progress_bar(1, 1))
    };
    (level, progress)
}

/// Le proposte vincitrici di un utente, le più recenti per prime (al massimo 5).
fn winning_proposals(connection: &Connection, user_id: i64) -> DbResult<Vec<(i64, String, i64)>> {
    connection
        .prepare(
            "SELECT week, name, votes FROM user_winning_proposals
             WHERE user_id=? ORDER BY week DESC, votes DESC, name LIMIT 5",
        )?
        .query_map([user_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect()
}

/// Righe "🔒 N badge segreti da scoprire" con un indizio per quello successivo.
pub fn hidden_badge_teaser(db: &impl Connected, user_id: i64) -> DbResult<Vec<String>> {
    let unlocked: Vec<String> = db.with_conn(|connection| -> DbResult<_> {
        connection
            .prepare("SELECT badge_key FROM user_hidden_badges WHERE user_id=?")?
            .query_map([user_id], |row| row.get(0))?
            .collect()
    })?;
    let locked: Vec<&str> = HIDDEN_BADGES.iter().map(|(key, _)| *key).filter(|key| !unlocked.iter().any(|u| u == key)).collect();
    let Some(first) = locked.first() else {
        return Ok(Vec::new());
    };
    let hint = HIDDEN_BADGE_HINTS.iter().find(|(key, _)| key == first).map_or("", |(_, hint)| *hint);
    let noun = if locked.len() == 1 { "badge segreto" } else { "badge segreti" };
    Ok(vec![String::new(), format!("🔒 **{} {noun} da scoprire**", locked.len()), format!("*Indizio: {hint}*")])
}

/// Embed "Il mio stato": il profilo privato di un utente.
pub fn build_profile_embed(db: &impl Connected, user_id: i64, now: DateTime<Tz>) -> DbResult<EmbedSpec> {
    let week = database::current_week(db)?;
    let max_votes = settings::max_votes(db)?;
    let max_proposals = settings::max_proposals(db)?;
    let stats_since = database::setting_or(db, "stats_since", "oggi")?;
    let awards_since = database::setting_or(db, "awards_since", "oggi")?;
    let today = now.date_naive();
    let season = season_for_date(today);

    let (vote_lines, proposals_used, latest, supports_used, totals, season_points, seasonal_titles) =
        db.with_conn(|connection| -> DbResult<_> {
            let polls: Vec<(i64, String, Option<String>)> = connection
                .prepare("SELECT id, name, emoji FROM polls WHERE active=1 ORDER BY position, id LIMIT 15")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<DbResult<_>>()?;
            let mut vote_lines = Vec::new();
            for (poll_id, name, emoji) in polls {
                let used: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM votes v JOIN options o ON o.id=v.option_id WHERE o.poll_id=? AND v.user_id=?",
                    [poll_id, user_id],
                    |row| row.get(0),
                )?;
                vote_lines.push(format!("{} {name}: `{used}/{max_votes} voti`", emoji.unwrap_or_default()));
            }
            let proposals_used: i64 =
                connection.query_row("SELECT COUNT(*) FROM proposals WHERE user_id=? AND week=?", [user_id, week], |row| row.get(0))?;
            let latest: Option<(String, String)> = connection
                .query_row(
                    "SELECT s.name, s.tipo FROM proposals p
                     JOIN suggestions s ON s.id=p.suggestion_id
                     WHERE p.user_id=? AND p.week=?
                     ORDER BY p.ts DESC, p.id DESC LIMIT 1",
                    [user_id, week],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let supports_used: i64 = connection.query_row(
                "SELECT COUNT(*) FROM suggestion_supports WHERE user_id=? AND week=?",
                [user_id, week],
                |row| row.get(0),
            )?;
            let totals = user_totals(connection, user_id)?;
            let season_points = season_points_for_user(connection, &season.key, user_id)?;
            let titles = season_champion_titles(connection, user_id, today, 3)?;
            Ok((vote_lines, proposals_used, latest, supports_used, totals, season_points, titles))
        })?;
    let points = contributor_points(&totals);
    let (level, progress) = level_progress(points);

    let (streak, active_this_week, winning, hidden_badges) = db.with_conn(|connection| -> DbResult<_> {
        let streak: Option<(i64, i64)> = connection
            .query_row("SELECT current_streak, best_streak FROM user_streaks WHERE user_id=?", [user_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        let active: bool = connection
            .query_row("SELECT 1 FROM weekly_activity WHERE user_id=? AND week=?", [user_id, week], |_| Ok(()))
            .optional()?
            .is_some();
        Ok((streak, active, winning_proposals(connection, user_id)?, user_hidden_badge_names(connection, user_id)?))
    })?;
    let current_streak = streak.map_or(0, |(current, _)| current);
    let mut streak_line = if current_streak > 0 {
        format!(
            "🔥 **Streak:** `{current_streak} settimane`{}",
            if active_this_week { " · mantenuta questa settimana" } else { " · partecipa per mantenerla" }
        )
    } else {
        "🔥 **Streak:** da iniziare questa settimana".to_string()
    };
    if let Some((_, best)) = streak {
        streak_line.push_str(&format!(" · record: `{best}`"));
    }

    let mut badges = Vec::new();
    if totals.proposals_submitted >= 1 {
        badges.push("🌱 Prima proposta");
    }
    if totals.supports_given >= 10 {
        badges.push("🎯 10 supporti dati");
    }
    if totals.supports_received >= 10 {
        badges.push("⭐ Community Hero");
    }
    if totals.ballot_entries >= 3 {
        badges.push("🗳️ Presenza in 3 ballottaggi");
    }
    if totals.winning_proposals >= 1 {
        badges.push("🏆 Proposta vincitrice");
    }

    let mut lines: Vec<String> = vec![
        format!("Settimana `#{week}`"),
        String::new(),
        format!(
            "**Proposte inviate:** `{proposals_used}/{max_proposals}` ({} disponibili)",
            (max_proposals - proposals_used).max(0)
        ),
        format!("**Proposte supportate:** `{supports_used}`"),
        String::new(),
        "**Voti usati per sondaggio**".to_string(),
    ];
    if vote_lines.is_empty() {
        lines.push("Nessun sondaggio attivo.".to_string());
    } else {
        lines.extend(vote_lines);
    }
    lines.push(String::new());
    lines.push(match &latest {
        Some((name, tipo)) => format!("**Ultima proposta:** {name} · {tipo}"),
        None => "**Ultima proposta:** nessuna questa settimana".to_string(),
    });
    lines.push(String::new());
    lines.push(format!("**Statistiche dal {stats_since}**"));
    lines.push(format!("**Livello:** {level} · **Punti:** `{points}`"));
    lines.push(format!("**Stagione {}:** `{season_points} punti`", season.label));
    lines.push(progress);
    lines.push(streak_line);
    lines.push(format!("Proposte inviate: `{}` · supporti dati: `{}`", totals.proposals_submitted, totals.supports_given));
    lines.push(format!(
        "Supporti ricevuti: `{}` · ballottaggi: `{}` · proposte vincitrici: `{}`",
        totals.supports_received, totals.ballot_entries, totals.winning_proposals
    ));
    lines.push(String::new());
    lines.push("**Titoli stagionali**".to_string());
    if seasonal_titles.is_empty() {
        lines.push("Nessun titolo stagionale ancora.".to_string());
    } else {
        lines.extend(seasonal_titles.iter().map(|label| format!("🏅 Campione {label}")));
    }
    lines.push(String::new());
    lines.push("**Badge**".to_string());
    if badges.is_empty() {
        lines.push("Ancora nessun badge: partecipa per sbloccarne uno!".to_string());
    } else {
        lines.extend(badges.iter().map(|badge| badge.to_string()));
    }
    if !hidden_badges.is_empty() {
        lines.push(String::new());
        lines.push("**Easter egg sbloccati**".to_string());
        lines.extend(hidden_badges.iter().map(|badge| badge.to_string()));
    }
    lines.extend(hidden_badge_teaser(db, user_id)?);
    lines.push(String::new());
    lines.push(format!("**Hall of Fame personale** (dal {awards_since})"));
    if winning.is_empty() {
        lines.push("Nessuna proposta vincitrice registrata per ora.".to_string());
    } else {
        lines.extend(winning.iter().map(|(week, name, votes)| format!("🏆 **{name}** · settimana `#{week}` · `{votes} voti`")));
    }
    Ok(EmbedSpec::new("👤 Il mio stato", truncate_chars(&lines.join("\n"), 4000), COLOR_PRIMARY))
}

/// Embed del profilo pubblico di un membro (nessun voto individuale).
pub fn build_public_profile_embed(
    db: &impl Connected,
    user_id: i64,
    display_name: &str,
    avatar_url: &str,
    now: DateTime<Tz>,
) -> DbResult<EmbedSpec> {
    let today = now.date_naive();
    let season = season_for_date(today);
    let (totals, season_points, seasonal_titles, streak, winning, hidden_badges) = db.with_conn(|connection| -> DbResult<_> {
        let streak: Option<(i64, i64)> = connection
            .query_row("SELECT current_streak, best_streak FROM user_streaks WHERE user_id=?", [user_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        Ok((
            user_totals(connection, user_id)?,
            season_points_for_user(connection, &season.key, user_id)?,
            season_champion_titles(connection, user_id, today, 3)?,
            streak,
            winning_proposals(connection, user_id)?,
            user_hidden_badge_names(connection, user_id)?,
        ))
    })?;
    let points = contributor_points(&totals);
    let (level, progress) = level_progress(points);
    let (current_streak, best_streak) = streak.unwrap_or((0, 0));
    let hall_of_fame: Vec<String> =
        winning.iter().map(|(week, name, votes)| format!("🏆 **{name}** · settimana `#{week}` · `{votes} voti`")).collect();

    let mut embed = EmbedSpec::new(
        format!("👤 Profilo di {display_name}"),
        format!(
            "**Livello:** {level} · **Punti:** `{points}`\n{progress}\n🍂 **{}:** `{season_points} punti` · 🔥 **Streak:** `{current_streak} settimane` · **Record:** `{best_streak} settimane`",
            season.label
        ),
        COLOR_PRIMARY,
    );
    embed.thumbnail = Some(avatar_url.to_string());
    embed = embed
        .field(
            "Statistiche",
            format!(
                "Proposte: `{}` · Supporti dati: `{}`\nSupporti ricevuti: `{}` · Ballottaggi: `{}` · Vittorie: `{}`",
                totals.proposals_submitted, totals.supports_given, totals.supports_received, totals.ballot_entries, totals.winning_proposals
            ),
            false,
        )
        .field(
            "🏆 Hall of Fame personale",
            if hall_of_fame.is_empty() { "Nessuna proposta vincitrice registrata per ora.".to_string() } else { hall_of_fame.join("\n") },
            false,
        );
    if !seasonal_titles.is_empty() {
        embed = embed.field(
            "🎖️ Titoli stagionali",
            seasonal_titles.iter().map(|label| format!("🏅 Campione {label}")).collect::<Vec<_>>().join("\n"),
            false,
        );
    }
    if !hidden_badges.is_empty() {
        embed = embed.field("🎭 Easter egg sbloccati", hidden_badges.join("\n"), false);
    }
    Ok(embed.footer("I voti individuali restano anonimi e non sono mostrati."))
}

// --- scheda di una proposta ------------------------------------------------------------------

/// Scheda pubblica di una proposta approvata della settimana; `None` se non esiste o non è approvata.
pub fn build_proposal_card(db: &impl Connected, suggestion_id: i64) -> DbResult<Option<EmbedSpec>> {
    let current_week = database::current_week(db)?;
    let suggestion: Option<(i64, String, String, Option<String>, Option<String>, String)> = db.with_conn(|connection| {
        connection
            .query_row(
                "SELECT week, name, tipo, link, status, review_status
                 FROM suggestions WHERE id=? AND week=?",
                [suggestion_id, current_week],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .optional()
    })?;
    let Some((week, name, tipo, link, status, review_status)) = suggestion else {
        return Ok(None);
    };
    if review_status != "approved" {
        return Ok(None);
    }
    let supporters = suggestion_supporter_counts(db, week, &[suggestion_id])?.get(&suggestion_id).copied().unwrap_or(0);
    let proposer: Option<i64> = db.with_conn(|connection| {
        connection
            .query_row(
                "SELECT user_id FROM proposals WHERE suggestion_id=? AND week=? ORDER BY id LIMIT 1",
                [suggestion_id, week],
                |row| row.get(0),
            )
            .optional()
    })?;
    let state = if status.as_deref() == Some("picked") { "🏆 Nel ballottaggio" } else { "In valutazione" };
    let mut embed = EmbedSpec::new(
        format!("{} {name}", proposal_type_emoji(&tipo)),
        format!("**{tipo}** · {state}\n──────────\n👥 Sostenitori: `{supporters}`"),
        COLOR_PRIMARY,
    );
    if let Some(user_id) = proposer {
        embed = embed.field("Proposta da", format!("└ <@{user_id}>"), true);
    }
    if let Some(link) = link.as_deref().filter(|link| !link.is_empty()) {
        embed = embed.field("Link", format!("└ [Apri link]({link})"), true);
    }
    embed.image = proposal_image_url(link.as_deref());
    Ok(Some(embed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::database::Database;
    use chrono::TimeZone;

    fn now() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()
    }

    fn spec_json(spec: &EmbedSpec) -> Value {
        serde_json::json!({
            "title": spec.title,
            "description": spec.description,
            "color": spec.color,
            "fields": spec.fields.iter().map(|f| serde_json::json!([f.name, f.value, f.inline])).collect::<Vec<_>>(),
            "thumbnail": spec.thumbnail,
            "image": spec.image,
            "footer": spec.footer,
            "has_timestamp": spec.timestamp.is_some(),
        })
    }

    /// Stato condiviso con `golden_cards.py`: `initialize` + `golden/cards_fixture.sql`.
    fn fixture_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, now()).unwrap();
        db.execute_batch(include_str!("golden/cards_fixture.sql")).unwrap();
        db
    }

    /// Ogni embed di `cards.py` costruito da Python sullo stesso stato deve coincidere, campo per
    /// campo: titolo, descrizione, colore, campi, miniatura, immagine, piè di pagina, timestamp.
    #[test]
    fn every_card_matches_the_python_implementation() {
        use crate::polls::find_poll;
        use crate::stats::weekly_winner;

        let golden: Value = serde_json::from_str(include_str!("golden/cards_expected.json")).unwrap();
        let db = fixture_db();
        let mut seen = 0;
        let mut check = |label: &str, actual: Value, expected: &Value| {
            assert_eq!(&actual, expected, "embed «{label}»");
            seen += 1;
        };

        let winner = db.with_conn(|c| weekly_winner(c, 7).unwrap()).expect("c'è un vincitore");
        check(
            "winner",
            serde_json::json!({
                "suggestion_id": winner.result.suggestion_id,
                "name": winner.result.name,
                "votes": winner.result.votes,
                "supporters": winner.result.supporters,
                "tie_break": winner.tie_break,
            }),
            &golden["winner"],
        );
        check("records", spec_json(&build_community_records_embed(&db).unwrap()), &golden["records"]);
        check("recap_6_winner", spec_json(&build_weekly_recap_embed(&db, 6, Some(&winner), now()).unwrap()), &golden["recap_6_winner"]);
        check("recap_4_none", spec_json(&build_weekly_recap_embed(&db, 4, None, now()).unwrap()), &golden["recap_4_none"]);
        check("recap_5", spec_json(&build_weekly_recap_embed(&db, 5, None, now()).unwrap()), &golden["recap_5"]);
        check("recap_99", spec_json(&build_weekly_recap_embed(&db, 99, None, now()).unwrap()), &golden["recap_99"]);
        let tie_winner = Winner {
            result: crate::stats::BallotResult { suggestion_id: 2, name: "Jackbox".into(), votes: 0, supporters: 0, first_ts: 0 },
            tie_break: Some("a parità di voti e sostenitori vince la proposta inviata per prima"),
        };
        check("recap_4_tie", spec_json(&build_weekly_recap_embed(&db, 4, Some(&tie_winner), now()).unwrap()), &golden["recap_4_tie"]);

        for poll_id in 1..=8 {
            let poll = find_poll(&db, poll_id).unwrap().unwrap();
            check(&format!("sondaggio {poll_id}"), spec_json(&build_embed(&db, &poll, now()).unwrap()), &golden["polls"][poll_id.to_string()]);
        }
        check("history_lines", serde_json::json!(history_summary_lines(&db, 10).unwrap()), &golden["history_lines"]);
        check("history_lines_2", serde_json::json!(history_summary_lines(&db, 2).unwrap()), &golden["history_lines_2"]);
        check("history", spec_json(&build_history_embed(&db).unwrap()), &golden["history"]);
        check("hall_of_fame", spec_json(&build_combined_hall_of_fame_embed(&db).unwrap()), &golden["hall_of_fame"]);
        check("dashboard", spec_json(&build_dashboard_embed(&db, now()).unwrap()), &golden["dashboard"]);
        check("leaderboard", spec_json(&build_leaderboard_embed(&db).unwrap()), &golden["leaderboard"]);
        check("contributors_10", spec_json(&build_contributor_leaderboard_embed(&db, 10).unwrap()), &golden["contributors_10"]);
        check("contributors_2", spec_json(&build_contributor_leaderboard_embed(&db, 2).unwrap()), &golden["contributors_2"]);
        for user in [1, 2, 3, 99] {
            check(&format!("profilo {user}"), spec_json(&build_profile_embed(&db, user, now()).unwrap()), &golden["profiles"][user.to_string()]);
            let public = build_public_profile_embed(&db, user, &format!("Utente {user}"), "https://cdn.example.test/rita.png", now()).unwrap();
            check(&format!("profilo pubblico {user}"), spec_json(&public), &golden["public_profiles"][user.to_string()]);
            check(&format!("indizi {user}"), serde_json::json!(hidden_badge_teaser(&db, user).unwrap()), &golden["teasers"][user.to_string()]);
        }
        for suggestion_id in [1, 2, 3, 4, 5, 6, 7, 999] {
            let card = build_proposal_card(&db, suggestion_id).unwrap();
            check(
                &format!("scheda proposta {suggestion_id}"),
                card.as_ref().map_or(Value::Null, spec_json),
                &golden["proposal_cards"][suggestion_id.to_string()],
            );
        }

        // Stati limite, nello stesso ordine di Python.
        db.execute("UPDATE polls SET is_community=0 WHERE id=5", []).unwrap();
        check("dashboard_no_community", spec_json(&build_dashboard_embed(&db, now()).unwrap()), &golden["dashboard_no_community"]);
        db.execute("UPDATE polls SET is_community=1 WHERE id=5", []).unwrap();
        db.execute("DELETE FROM options WHERE poll_id=5", []).unwrap();
        check("dashboard_empty_community", spec_json(&build_dashboard_embed(&db, now()).unwrap()), &golden["dashboard_empty_community"]);
        let poll_5 = find_poll(&db, 5).unwrap().unwrap();
        check("poll_5_empty", spec_json(&build_embed(&db, &poll_5, now()).unwrap()), &golden["poll_5_empty"]);
        db.execute("UPDATE polls SET active=0", []).unwrap();
        check("leaderboard_no_polls", spec_json(&build_leaderboard_embed(&db).unwrap()), &golden["leaderboard_no_polls"]);
        check("dashboard_no_polls", spec_json(&build_dashboard_embed(&db, now()).unwrap()), &golden["dashboard_no_polls"]);
        check("profile_no_polls", spec_json(&build_profile_embed(&db, 1, now()).unwrap()), &golden["profile_no_polls"]);

        assert_eq!(seen, 49, "numero di embed confrontati");
    }

    #[test]
    fn spec_converts_to_a_serenity_embed() {
        let spec = EmbedSpec::new("Titolo", "Testo", 0x123456)
            .field("Campo", "Valore", true)
            .footer("Piè di pagina")
            .timestamp(now());
        let mut spec = spec;
        spec.thumbnail = Some("https://example.com/t.png".into());
        spec.image = Some("https://example.com/i.png".into());
        let json = serde_json::to_value(spec.build()).unwrap();
        assert_eq!(json["title"], "Titolo");
        assert_eq!(json["description"], "Testo");
        assert_eq!(json["color"], 0x123456);
        assert_eq!(json["fields"][0]["name"], "Campo");
        assert_eq!(json["fields"][0]["inline"], true);
        assert_eq!(json["footer"]["text"], "Piè di pagina");
        assert_eq!(json["thumbnail"]["url"], "https://example.com/t.png");
        assert_eq!(json["image"]["url"], "https://example.com/i.png");
        assert!(json["timestamp"].as_str().unwrap().starts_with("2026-11-11T17:00:00"), "UTC: {}", json["timestamp"]);
    }

    #[test]
    fn poll_color_parses_hex_and_falls_back() {
        assert_eq!(poll_color(Some("#5865F2")), 0x5865F2);
        assert_eq!(poll_color(Some("E0A030")), 0xE0A030);
        assert_eq!(poll_color(Some("##D95F6A")), 0xD95F6A, "più '#' iniziali, come lstrip");
        assert_eq!(poll_color(Some("non-un-colore")), COLOR_PRIMARY);
        assert_eq!(poll_color(Some("")), COLOR_PRIMARY);
        assert_eq!(poll_color(None), COLOR_PRIMARY);
    }

    #[test]
    fn empty_dashboard_and_leaderboards_have_friendly_texts() {
        let db = Database::open_in_memory().unwrap();
        let dashboard = build_dashboard_embed(&db, now()).unwrap();
        let value = |name: &str| dashboard.fields.iter().find(|f| f.name == name).map(|f| f.value.clone()).unwrap();
        assert_eq!(value("Sondaggi attivi"), "Nessun sondaggio attivo.");
        assert_eq!(value("🏆 Ballottaggio community"), "Sondaggio community non configurato.");
        assert_eq!(value("🔥 Più votato ora"), "In attesa dei primi voti.");
        assert_eq!(value("⭐ Proposta del momento"), "Nessuna proposta in attesa.");
        assert_eq!(value("✨ Proposte recenti"), "Nessuna proposta recente.");
        let leaderboard = build_leaderboard_embed(&db).unwrap();
        assert_eq!(leaderboard.description_text(), "Non ci sono sondaggi attivi al momento.");
        assert!(build_contributor_leaderboard_embed(&db, 10).unwrap().description_text().starts_with("Nessun contributo registrato"));
        assert_eq!(build_history_embed(&db).unwrap().description_text(), "Nessuna settimana in archivio.");
        assert!(build_community_records_embed(&db).unwrap().description_text().ends_with("I record appariranno dopo il primo reset con attività."));
        assert!(build_proposal_card(&db, 1).unwrap().is_none());
    }
}
