//! Le schermate del centro controllo dello staff (la parte `build_*_embed` di `cogs/admin.py`):
//! home, handbook, analytics, analisi di una proposta, analisi di un utente, elenco utenti e stato
//! del sistema.
//!
//! Ogni funzione restituisce un [`EmbedSpec`]. L'orologio e i dati dell'ambiente (variabili
//! d'ambiente, dimensione del file del database) sono parametri, così le schermate si provano con
//! gli stessi risultati del codice Python.
// Collegato ai comandi e ai pulsanti del centro controllo.
#![allow(dead_code)]

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::DateTime;
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use crate::cards::EmbedSpec;
use crate::config::{COLOR_STAFF, CONTRIBUTOR_LEVELS};
use crate::database::{Connected, DbResult, current_week};
use crate::formatting::{normalize, truncate_chars};
use crate::queries::suggestion_supporter_counts;
use crate::seasonal::{season_for_date, season_leaderboard_rows};
use crate::stats::{UserStats, contributor_level, contributor_points};

/// Etichetta, valore e icona delle sezioni del menu del pannello.
pub const SECTIONS: [(&str, &str, &str); 5] = [
    ("Handbook", "handbook", "📖"),
    ("Analytics", "analytics", "📈"),
    ("Proposte", "proposals", "✨"),
    ("Utenti", "users", "👥"),
    ("Sistema", "system", "⚙️"),
];

// --- statistiche dello storico ---------------------------------------------------------------

/// I numeri di una settimana archiviata (`_week_snapshot_metrics`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeekMetrics {
    pub week: i64,
    /// `None` per gli snapshot scritti prima che si contassero i votanti.
    pub participants: Option<i64>,
    pub votes: i64,
    pub supports: i64,
}

/// Una riga per settimana (l'ultimo snapshot di ciascuna), dalla più recente, al massimo 200.
///
/// Uno snapshot con campi mancanti conta zero: Python in quel caso fallirebbe con un errore.
pub fn week_snapshot_metrics(connection: &Connection) -> DbResult<Vec<WeekMetrics>> {
    let rows: Vec<(i64, String)> = connection
        .prepare(
            "SELECT week, data FROM history
             WHERE id IN (SELECT MAX(id) FROM history GROUP BY week)
             ORDER BY week DESC LIMIT 200",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<DbResult<_>>()?;
    Ok(rows
        .into_iter()
        .map(|(week, raw)| {
            let data: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            let sum_over = |items: Option<&Vec<Value>>, field: &dyn Fn(&Value) -> i64| items.map_or(0, |items| items.iter().map(field).sum::<i64>());
            let votes = data["polls"].as_array().map_or(0, |polls| {
                polls.iter().map(|poll| sum_over(poll["options"].as_array(), &|option| option["votes"].as_i64().unwrap_or(0))).sum()
            });
            WeekMetrics {
                week,
                participants: data["participants"].as_i64(),
                votes,
                supports: sum_over(data["suggestions"].as_array(), &|suggestion| suggestion["supporters"].as_i64().unwrap_or(0)),
            }
        })
        .collect())
}

/// Media come la scrive Python: `{:.1f}` del valore esatto.
fn mean(values: &[i64]) -> f64 {
    values.iter().sum::<i64>() as f64 / values.len() as f64
}

// --- schermate statiche ----------------------------------------------------------------------

pub fn staff_home_embed() -> EmbedSpec {
    EmbedSpec::new(
        "🛠️ Centro controllo staff",
        "Seleziona una sezione dal menu. Le schermate sono visibili solo a te.\n\n\
         📖 **Handbook** · punti, livelli e criteri\n\
         📈 **Analytics** · andamento delle ultime settimane\n\
         ✨ **Proposte** · storico e risultati per nome\n\
         👥 **Utenti** · profilo staff, contributor emergenti e inattivi\n\
         ⚙️ **Sistema** · configurazione, conteggi, verifiche ed export",
        COLOR_STAFF,
    )
}

pub fn handbook_embed() -> EmbedSpec {
    let points = [("➕ Proposta inviata", 1), ("👍 Sostenitore ricevuto", 2), ("✅ Ingresso nel ballottaggio", 5), ("🥇 Vittoria finale", 10)];
    let point_lines: Vec<String> = points.iter().map(|(label, value)| format!("{label} · `+{value} punti`")).collect();
    let level_lines: Vec<String> = CONTRIBUTOR_LEVELS.iter().map(|(threshold, name)| format!("{name} · `{threshold} punti`")).collect();
    EmbedSpec::new(
        "📖 Handbook contributor",
        "I punti si accumulano nel profilo e determinano livello e classifica. I valori sono cumulativi: un'azione può assegnare punti a più autori.",
        COLOR_STAFF,
    )
    .field("🏆 Punti", point_lines.join("\n"), false)
    .field("🎖️ Livelli", level_lines.join("\n"), false)
    .field(
        "🌟 Contributor della settimana",
        "Al reset viene premiato chi ha ottenuto più punti nella settimana; in caso di parità il riconoscimento è condiviso.",
        false,
    )
    .field(
        "⭐ Raccomandazione staff",
        "Ordina le proposte usando sostenitori e tempo di attesa, con una penalità per le vittorie recenti. È solo un suggerimento: la selezione resta una decisione dello staff.",
        false,
    )
    .footer("Reset e archiviazione ogni lunedì alle 12:00 · Europe/Rome")
}

/// La schermata della sezione «Proposte»: invita a usare la ricerca.
pub fn proposals_section_embed() -> EmbedSpec {
    EmbedSpec::new(
        "✨ Analisi proposte",
        "Cerca una proposta per nome. I risultati aggregano lo storico archiviato e le proposte della settimana corrente.",
        COLOR_STAFF,
    )
}

// --- analytics -------------------------------------------------------------------------------

pub fn analytics_embed(db: &impl Connected, now: DateTime<Tz>) -> DbResult<EmbedSpec> {
    let current = current_week(db)?;
    let (first_week, last_week) = (current - 8, current - 1);
    db.with_conn(|connection| -> DbResult<EmbedSpec> {
        let history = week_snapshot_metrics(connection)?;
        let contributors: HashMap<i64, i64> = connection
            .prepare(
                "SELECT week, COUNT(*) AS contributors
                 FROM weekly_activity WHERE week BETWEEN ? AND ?
                 GROUP BY week",
            )?
            .query_map([first_week, last_week], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<DbResult<_>>()?;
        let by_week: HashMap<i64, &WeekMetrics> = history.iter().map(|metrics| (metrics.week, metrics)).collect();
        let lines: Vec<String> = (first_week..current)
            .map(|week| {
                let snapshot = by_week.get(&week);
                let voters = snapshot.and_then(|s| s.participants).map_or("`n/d`".to_string(), |n| format!("`{n}`"));
                let votes = snapshot.map_or("`n/d`".to_string(), |s| format!("`{}`", s.votes));
                let supports = snapshot.map_or("`n/d`".to_string(), |s| format!("`{}`", s.supports));
                format!(
                    "`#{week}` · `{}` contributor · {voters} votanti · {votes} voti · {supports} sostenitori",
                    contributors.get(&week).copied().unwrap_or(0)
                )
            })
            .collect();

        let in_range: Vec<&WeekMetrics> = history.iter().filter(|m| (first_week..=last_week).contains(&m.week)).collect();
        let voter_counts: Vec<i64> = in_range.iter().filter_map(|m| m.participants).collect();
        let average_voters = if voter_counts.is_empty() {
            "Dato non disponibile per le settimane archiviate.".to_string()
        } else {
            format!("`{:.1}` su {} settimane archiviate", mean(&voter_counts), voter_counts.len())
        };
        let support_counts: Vec<i64> = in_range.iter().map(|m| m.supports).collect();
        let average_supports = if support_counts.is_empty() {
            "Dato non disponibile.".to_string()
        } else {
            format!("`{:.1}` su {} settimane archiviate", mean(&support_counts), support_counts.len())
        };

        let growth = if in_range.len() < 8 {
            "Dati insufficienti per confrontare due periodi completi.".to_string()
        } else {
            let distinct_users = |from: i64, to: i64| -> DbResult<i64> {
                connection.query_row("SELECT COUNT(DISTINCT user_id) FROM weekly_activity WHERE week BETWEEN ? AND ?", [from, to], |row| row.get(0))
            };
            let recent = distinct_users(current - 4, last_week)?;
            let previous = distinct_users(current - 8, current - 5)?;
            if previous != 0 {
                let percentage = (recent - previous) as f64 / previous as f64 * 100.0;
                format!("`{:+}` contributor unici ({percentage:+.1}%) rispetto alle 4 settimane precedenti.", recent - previous)
            } else {
                "Nessun contributor nel periodo di confronto precedente.".to_string()
            }
        };

        let season = season_for_date(now.date_naive());
        let season_lines: Vec<String> = season_leaderboard_rows(connection, &season.key, 3)?
            .iter()
            .enumerate()
            .map(|(index, row)| format!("`{}`. <@{}> · `{} punti`", index + 1, row.user_id, row.points))
            .collect();
        let description = truncate_chars(&format!("Settimane archiviate `#{first_week}`–`#{last_week}`.\n{}", lines.join("\n")), 4000);
        Ok(EmbedSpec::new("📈 Community analytics", description, COLOR_STAFF)
            .field("👥 Votanti medi", average_voters, true)
            .field("👍 Sostenitori medi", average_supports, true)
            .field("📈 Crescita contributor", growth, false)
            .field(
                format!("🍂 Classifica · {}", season.label),
                if season_lines.is_empty() { "Nessun punto stagionale registrato.".to_string() } else { season_lines.join("\n") },
                false,
            )
            .footer("I votanti storici sono disponibili solo dagli snapshot che includono il conteggio anonimo; n/d indica un dato non raccolto."))
    })
}

// --- analisi di una proposta -----------------------------------------------------------------

pub fn proposal_analytics_embed(db: &impl Connected, name: &str, tipo: Option<&str>) -> DbResult<EmbedSpec> {
    let target_name = normalize(name);
    let target_type = tipo.filter(|t| !t.is_empty()).map(normalize);
    let type_matches = |candidate: &str| target_type.as_deref().is_none_or(|target| normalize(candidate) == target);
    let current = current_week(db)?;

    let mut supports_by_week: HashMap<i64, i64> = HashMap::new();
    let mut ballot_weeks: BTreeSet<i64> = BTreeSet::new();
    let mut matching_types: BTreeSet<String> = BTreeSet::new();
    let mut total_supports = 0;

    let wins = db.with_conn(|connection| -> DbResult<usize> {
        let history: Vec<(i64, String)> = connection
            .prepare("SELECT week, data FROM history WHERE id IN (SELECT MAX(id) FROM history GROUP BY week) ORDER BY week")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<DbResult<_>>()?;
        for (week, raw) in &history {
            let data: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
            for proposal in data["suggestions"].as_array().into_iter().flatten() {
                let (Some(proposal_name), Some(proposal_type)) = (proposal["name"].as_str(), proposal["tipo"].as_str()) else {
                    continue;
                };
                if normalize(proposal_name) != target_name || !type_matches(proposal_type) {
                    continue;
                }
                matching_types.insert(proposal_type.to_string());
                let supporters = proposal["supporters"].as_i64().unwrap_or(0);
                total_supports += supporters;
                *supports_by_week.entry(*week).or_insert(0) += supporters;
                if proposal["status"].as_str() == Some("picked") {
                    ballot_weeks.insert(*week);
                }
            }
        }
        let picked: Vec<(i64, String, String)> = connection
            .prepare("SELECT week, name, tipo FROM picked_suggestions")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<DbResult<_>>()?;
        for (week, picked_name, picked_type) in picked {
            if normalize(&picked_name) == target_name && type_matches(&picked_type) {
                matching_types.insert(picked_type);
                ballot_weeks.insert(week);
            }
        }
        let wins: Vec<(i64, String)> = connection
            .prepare("SELECT week, name FROM user_winning_proposals")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<DbResult<_>>()?;
        Ok(wins.into_iter().filter(|(_, won)| normalize(won) == target_name).map(|(week, _)| week).collect::<HashSet<_>>().len())
    })?;

    let current_rows: Vec<(i64, String, String, String)> = db.with_conn(|connection| -> DbResult<_> {
        connection
            .prepare("SELECT id, name, tipo, status FROM suggestions WHERE week=?")?
            .query_map([current], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect()
    })?;
    let current_ids: Vec<i64> = current_rows.iter().filter(|(_, n, t, _)| normalize(n) == target_name && type_matches(t)).map(|row| row.0).collect();
    let current_supporters = suggestion_supporter_counts(db, current, &current_ids)?;
    for (id, _, row_type, status) in &current_rows {
        if !current_ids.contains(id) {
            continue;
        }
        matching_types.insert(row_type.clone());
        let supporters = current_supporters.get(id).copied().unwrap_or(0);
        total_supports += supporters;
        *supports_by_week.entry(current).or_insert(0) += supporters;
        if status == "picked" {
            ballot_weeks.insert(current);
        }
    }

    if matching_types.is_empty() {
        return Ok(EmbedSpec::new("✨ Analisi proposta", format!("Non ho trovato **{name}** nello storico o tra le proposte attuali."), COLOR_STAFF));
    }
    let appearance = match ballot_weeks.last() {
        None => "Mai entrata nel ballottaggio".to_string(),
        Some(last) => {
            let weeks_ago = (current - last).max(0);
            let when = if weeks_ago == 0 { "questa settimana".to_string() } else { format!("{weeks_ago} settimane fa") };
            format!("Settimana `#{last}` · {when}")
        }
    };
    let trend = supports_by_week.get(&current).copied().unwrap_or(0) - supports_by_week.get(&(current - 1)).copied().unwrap_or(0);
    let type_text = matching_types.into_iter().collect::<Vec<_>>().join(", ");
    Ok(EmbedSpec::new(format!("✨ Analisi · {}", truncate_chars(name, 200)), format!("Tipo: **{type_text}**"), COLOR_STAFF)
        .field("👍 Sostenitori storici", format!("`{total_supports}`"), true)
        .field("🏆 Settimane nel ballottaggio", format!("`{}`", ballot_weeks.len()), true)
        .field("🥇 Vittorie", format!("`{wins}`"), true)
        .field("📅 Ultima apparizione", appearance, false)
        .field("📈 Trend sostenitori", format!("`{trend:+}` questa settimana rispetto alla precedente"), false)
        .footer("Conteggi aggregati; i voti individuali non sono consultabili."))
}

// --- utenti ----------------------------------------------------------------------------------

/// Il membro di cui mostrare il riepilogo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    pub id: i64,
    /// Nome mostrato nel server (o il nome utente, se non è un membro).
    pub display_name: String,
    pub avatar_url: String,
}

pub fn user_embed(db: &impl Connected, member: &MemberInfo) -> DbResult<EmbedSpec> {
    let current = current_week(db)?;
    db.with_conn(|connection| -> DbResult<EmbedSpec> {
        let stats = connection
            .query_row(
                "SELECT proposals_submitted, supports_given, supports_received, ballot_entries, winning_proposals
                 FROM user_stats WHERE user_id=?",
                [member.id],
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
            .unwrap_or_default();
        let points = contributor_points(&stats);
        let streak: Option<(i64, i64)> = connection
            .query_row("SELECT current_streak, best_streak FROM user_streaks WHERE user_id=?", [member.id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let last_active: Option<i64> = connection.query_row("SELECT MAX(week) FROM weekly_activity WHERE user_id=?", [member.id], |row| row.get(0))?;
        let last_active_text = match last_active {
            Some(week) => format!("Settimana `#{week}` · {} settimane fa", (current - week).max(0)),
            None => "Nessuna attività registrata".to_string(),
        };
        let mut embed = EmbedSpec::new(format!("👤 Analisi utente · {}", truncate_chars(&member.display_name, 200)), "", COLOR_STAFF);
        embed.description = None;
        embed.thumbnail = Some(member.avatar_url.clone());
        Ok(embed
            .field("🏆 Progressione", format!("**{}** · `{points} punti`", contributor_level(points)), true)
            .field(
                "🔥 Streak",
                match streak {
                    Some((now, best)) => format!("`{now}` attuale · `{best}` record"),
                    None => "`0` settimane".to_string(),
                },
                true,
            )
            .field(
                "✨ Contributi",
                format!(
                    "Proposte `{}` · Sostenitori ricevuti `{}` · Ballottaggi `{}` · Vittorie `{}`",
                    stats.proposals_submitted, stats.supports_received, stats.ballot_entries, stats.winning_proposals
                ),
                false,
            )
            .field("📅 Ultima attività", last_active_text, false)
            .footer("Nessun voto individuale viene mostrato o associato a questo profilo."))
    })
}

pub fn users_embed(db: &impl Connected) -> DbResult<EmbedSpec> {
    let current = current_week(db)?;
    db.with_conn(|connection| -> DbResult<EmbedSpec> {
        let emerging: Vec<(i64, i64)> = connection
            .prepare(
                "SELECT user_id, SUM(points) AS points
                 FROM weekly_activity
                 WHERE week BETWEEN ? AND ?
                 GROUP BY user_id HAVING SUM(points)>0
                 ORDER BY points DESC, user_id LIMIT 5",
            )?
            .query_map([current - 1, current], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<DbResult<_>>()?;
        let inactive: Vec<(i64, i64)> = connection
            .prepare(
                "SELECT user_id, MAX(week) AS last_week
                 FROM weekly_activity GROUP BY user_id
                 HAVING MAX(week)<=?
                 ORDER BY last_week, user_id LIMIT 5",
            )?
            .query_map([current - 8], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<DbResult<_>>()?;
        let emerging_lines: Vec<String> = emerging.iter().map(|(user, points)| format!("<@{user}> · `+{points} punti`")).collect();
        let inactive_lines: Vec<String> =
            inactive.iter().map(|(user, week)| format!("<@{user}> · ultima attività `#{week}` ({} settimane fa)", current - week)).collect();
        Ok(EmbedSpec::new(
            "👥 Analisi utenti",
            "Seleziona un membro dal menu per aprire il suo riepilogo staff. I segnali automatici sono indicatori, non valutazioni.",
            COLOR_STAFF,
        )
        .field(
            "🔥 Contributor emergenti · ultime 2 settimane",
            if emerging_lines.is_empty() { "Nessun dato recente.".to_string() } else { emerging_lines.join("\n") },
            false,
        )
        .field(
            "😴 Contributor inattivi · 8+ settimane",
            if inactive_lines.is_empty() { "Nessuno rilevato.".to_string() } else { inactive_lines.join("\n") },
            false,
        )
        .footer("La ricerca non mostra voti individuali."))
    })
}

// --- sistema ---------------------------------------------------------------------------------

/// Ciò che lo stato del sistema legge fuori dal database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemInfo {
    /// `EVENT_OVERLAY_PUBLIC_URL`.
    pub public_url: String,
    /// `PUBLIC_API_TOKEN`.
    pub api_token: String,
    /// Dimensione del file del database; `None` se non si riesce a leggerla.
    pub database_bytes: Option<u64>,
}

impl SystemInfo {
    /// Legge variabili d'ambiente e dimensione del file dal sistema.
    pub fn from_environment() -> Self {
        let database_bytes = match std::fs::metadata(crate::config::database_path()) {
            Ok(metadata) => Some(metadata.len()),
            Err(e) => {
                tracing::error!("Impossibile leggere la dimensione del database: {e}");
                None
            }
        };
        Self {
            public_url: std::env::var("EVENT_OVERLAY_PUBLIC_URL").unwrap_or_default(),
            api_token: std::env::var("PUBLIC_API_TOKEN").unwrap_or_default(),
            database_bytes,
        }
    }
}

pub fn system_embed(db: &impl Connected, info: &SystemInfo) -> DbResult<EmbedSpec> {
    let setting = |key: &str| -> DbResult<Option<String>> { Ok(crate::database::setting(db, key)?.filter(|value| !value.is_empty())) };
    let channel_id = setting("poll_channel")?;
    let dashboard_message = setting("dashboard_message")?;
    let activity_channel = setting("activity_channel")?;
    let feed_active = crate::database::setting_or(db, "activity_feed_enabled", "0")? == "1";
    let public_url = info.public_url.trim_end_matches('/');
    let api_active = info.api_token.chars().count() >= 32 && info.api_token.trim() == info.api_token;
    let (proposals, supports, votes, weeks, reports, historical_votes) = db.with_conn(|connection| -> DbResult<_> {
        let count = |sql: &str| connection.query_row(sql, [], |row| row.get::<_, i64>(0));
        Ok((
            count("SELECT COALESCE(SUM(proposals_submitted), 0) FROM user_stats")?,
            count("SELECT COALESCE(SUM(supports_given), 0) FROM user_stats")?,
            count("SELECT COUNT(*) FROM votes")?,
            count("SELECT COUNT(*) FROM history")?,
            count("SELECT COUNT(*) FROM live_events WHERE ended_at IS NOT NULL")?,
            week_snapshot_metrics(connection)?.iter().map(|metrics| metrics.votes).sum::<i64>(),
        ))
    })?;
    let database_size = match info.database_bytes {
        Some(bytes) => format!("`{:.2} MiB`", bytes as f64 / (1024.0 * 1024.0)),
        None => "non disponibile".to_string(),
    };
    let dashboard_status = match (&channel_id, &dashboard_message) {
        (Some(channel), Some(message)) => format!("Configurata · <#{channel}> · messaggio `{message}`"),
        _ => "Non configurata o non ancora pubblicata".to_string(),
    };
    let feed_status = match (&activity_channel, feed_active) {
        (Some(channel), true) => format!("ATTIVO · <#{channel}>"),
        (Some(channel), false) => format!("DISATTIVO · canale <#{channel}>"),
        (None, _) => "DISATTIVO · canale non configurato".to_string(),
    };
    let description = format!(
        "Database: **OK** · connessione SQLite verificata\nDashboard: **{dashboard_status}**\nFeed attività: **{feed_status}**\nSito e overlay: **{}**\nAPI protetta: **{}**\nDimensione database: {database_size}",
        if public_url.is_empty() { "non configurati" } else { "configurati" },
        if api_active { "configurata" } else { "non configurata" },
    );
    Ok(EmbedSpec::new("⚙️ Stato del sistema", description, COLOR_STAFF)
        .field(
            "📊 Conteggi",
            format!(
                "Proposte inviate `{proposals}` · supporti dati `{supports}`\nVoti correnti `{votes}` · voti archiviati `{historical_votes}`\nSettimane archiviate `{weeks}` · report eventi `{reports}`"
            ),
            false,
        )
        .field(
            "♻️ Ricalcoli",
            "Classifica e Hall of Fame sono lette direttamente dai dati, senza cache da ricostruire. L'aggiornamento dei record conserva i massimi storici e non riscrive le settimane archiviate.",
            false,
        )
        .footer("L'export è disponibile solo agli amministratori ed è redatto."))
}

// --- export redatto --------------------------------------------------------------------------

/// Le tabelle di riepilogo aggregato e la rimozione dei dati individuali (`_create_redacted_export`).
const REDACTION_SCRIPT: &str = "
CREATE TABLE exported_vote_counts AS
SELECT p.name AS poll_name, o.label AS option_label,
       COUNT(v.user_id) AS vote_count
FROM polls p
JOIN options o ON o.poll_id=p.id
LEFT JOIN votes v ON v.option_id=o.id
GROUP BY p.id, o.id;
CREATE TABLE exported_support_counts AS
SELECT suggestion_id, COUNT(DISTINCT user_id) AS support_count
FROM suggestion_supports
GROUP BY suggestion_id;
CREATE TABLE exported_live_event_summaries AS
SELECT title, started_at, ended_at, peak_participants,
       participant_count, end_reason
FROM live_events
WHERE ended_at IS NOT NULL;
CREATE TABLE exported_season_summaries AS
SELECT s.label, COUNT(DISTINCT a.user_id) AS active_users,
       SUM(a.points) AS total_points, MAX(a.points) AS top_points
FROM seasons s
LEFT JOIN season_activity a ON a.season_key=s.season_key
GROUP BY s.season_key;
DROP TABLE IF EXISTS votes;
DROP TABLE IF EXISTS vote_change_counts;
DROP TABLE IF EXISTS suggestion_supports;
DROP TABLE IF EXISTS user_notification_preferences;
DROP TABLE IF EXISTS user_hidden_badges;
DROP TABLE IF EXISTS user_ballot_weeks;
DROP TABLE IF EXISTS season_activity;
DROP TABLE IF EXISTS live_event_participants;
DROP TABLE IF EXISTS live_events;
DROP TABLE IF EXISTS web_sessions;
DROP TABLE IF EXISTS staff_audit;
DELETE FROM settings WHERE key='live_event_token';
";

/// Copia del database senza i dati individuali, come contenuto di un file SQLite.
///
/// Copia coerente di tutto il database (come `Connection.backup` di Python), poi tiene solo i
/// riepiloghi aggregati. Il file temporaneo sta in una cartella che sparisce alla fine, qualunque
/// sia l'esito.
pub fn create_redacted_export(db: &crate::database::Database) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    use std::time::Duration;

    let directory = tempfile::Builder::new().prefix("sword-bot-").tempdir()?;
    let path = directory.path().join("export.db");
    {
        let mut output = Connection::open(&path)?;
        db.with_conn(|source| -> DbResult<()> {
            let backup = rusqlite::backup::Backup::new(source, &mut output)?;
            backup.run_to_completion(100, Duration::ZERO, None)
        })?;
        output.execute_batch(REDACTION_SCRIPT)?;
        // Un solo file autosufficiente, senza file `-wal` da portarsi dietro.
        output.pragma_update(None, "journal_mode", "DELETE")?;
        output.close().map_err(|(_, e)| e)?;
    }
    Ok(std::fs::read(&path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::database::Database;
    use chrono::TimeZone;
    use serde_json::json;

    /// Mercoledì 2027-03-03, 10:00 (Roma): lo stesso istante di `golden_control_center.py`.
    fn now() -> DateTime<Tz> {
        TZ.with_ymd_and_hms(2027, 3, 3, 10, 0, 0).unwrap()
    }

    fn golden() -> Value {
        serde_json::from_str(include_str!("golden/control_center_expected.json")).unwrap()
    }

    /// Stato A: `initialize` + `cards_fixture.sql` + `admin_fixture.sql` (settimana corrente 7).
    fn db() -> Database {
        let db = Database::open_in_memory().unwrap();
        // Il golden è stato generato il 2026-11-11: la stagione di quel giorno esiste già nel fixture.
        crate::core::initialize(&db, TZ.with_ymd_and_hms(2026, 11, 11, 18, 0, 0).unwrap()).unwrap();
        db.execute_batch(include_str!("golden/cards_fixture.sql")).unwrap();
        db.execute_batch(include_str!("golden/admin_fixture.sql")).unwrap();
        db
    }

    fn spec_json(spec: &EmbedSpec) -> Value {
        json!({
            "title": spec.title,
            "description": spec.description,
            "color": spec.color,
            "fields": spec.fields.iter().map(|f| json!([f.name, f.value, f.inline])).collect::<Vec<_>>(),
            "thumbnail": spec.thumbnail,
            "footer": spec.footer,
        })
    }

    #[test]
    fn static_screens_match_the_python_implementation() {
        let golden = golden();
        assert_eq!(spec_json(&staff_home_embed()), golden["home"]);
        assert_eq!(spec_json(&handbook_embed()), golden["handbook"]);
    }

    #[test]
    fn proposals_section_text_is_the_one_in_the_python_menu() {
        let spec = proposals_section_embed();
        assert_eq!(spec.title.as_deref(), Some("✨ Analisi proposte"));
        assert_eq!(
            spec.description.as_deref(),
            Some("Cerca una proposta per nome. I risultati aggregano lo storico archiviato e le proposte della settimana corrente.")
        );
        assert_eq!(spec.color, COLOR_STAFF);
    }

    #[test]
    fn analytics_and_users_match_python_before_and_after_two_more_weeks_of_history() {
        let golden = golden();
        let db = db();
        assert_eq!(spec_json(&analytics_embed(&db, now()).unwrap()), golden["analytics_a"], "analytics con 6 settimane");
        assert_eq!(spec_json(&users_embed(&db).unwrap()), golden["users"]);
        // Due settimane in più: ora il confronto tra periodi si calcola.
        db.execute_batch(include_str!("golden/admin_fixture_b.sql")).unwrap();
        assert_eq!(spec_json(&analytics_embed(&db, now()).unwrap()), golden["analytics_b"], "analytics con 8 settimane");
        assert_eq!(spec_json(&users_embed(&db).unwrap()), golden["users_b"]);
    }

    #[test]
    fn proposal_analysis_matches_python() {
        let golden = golden();
        let db = db();
        let long = "N".repeat(300);
        let cases: [(&str, &str, Option<&str>); 10] = [
            ("among_us", "Among Us", None),
            ("among_us_lower", "among us!", None),
            ("among_us_gioco", "Among Us", Some("Gioco")),
            ("among_us_film", "Among Us", Some("film")),
            ("jackbox", "Jackbox Party", None),
            ("karaoke", "Karaoke Night", None),
            ("gartic", "Gartic Phone", Some("Gioco")),
            ("missing", "Minecraft", None),
            ("missing_type", "Among Us", Some("Serie")),
            ("long_name", &long, None),
        ];
        for (label, name, tipo) in cases {
            assert_eq!(spec_json(&proposal_analytics_embed(&db, name, tipo).unwrap()), golden["proposals"][label], "{label}");
        }
        // Un tipo vuoto vale «nessun filtro», come `None`.
        assert_eq!(
            spec_json(&proposal_analytics_embed(&db, "Among Us", Some("")).unwrap()),
            spec_json(&proposal_analytics_embed(&db, "Among Us", None).unwrap())
        );
    }

    #[test]
    fn user_analysis_matches_python() {
        let golden = golden();
        let db = db();
        let member = |id: i64, name: &str, avatar: &str| MemberInfo { id, display_name: name.to_string(), avatar_url: avatar.to_string() };
        let cases = [
            ("rich", member(1, "Ada", "https://cdn.example/1.png")),
            ("poor", member(2, "Bruno", "https://cdn.example/2.png")),
            ("none", member(999, "Nuovo", "https://cdn.example/999.png")),
            ("not_member", member(3, "solo_nome", "https://cdn.example/3.png")),
            ("long_name", member(1, &"D".repeat(300), "https://cdn.example/1.png")),
        ];
        for (label, info) in cases {
            assert_eq!(spec_json(&user_embed(&db, &info).unwrap()), golden["user"][label], "{label}");
        }
    }

    #[test]
    fn system_status_matches_python() {
        let golden = golden();
        let db = db();
        db.set_setting("poll_channel", 500).unwrap();
        db.set_setting("dashboard_message", 900).unwrap();
        db.set_setting("activity_channel", 700).unwrap();
        db.set_setting("activity_feed_enabled", "1").unwrap();
        let info = SystemInfo { public_url: "https://overlay.example.com/".into(), api_token: "t".repeat(40), database_bytes: Some(1_572_864 + 4096) };
        assert_eq!(spec_json(&system_embed(&db, &info).unwrap()), golden["system"]["configured"]);
        db.set_setting("activity_feed_enabled", "0").unwrap();
        let short_token = SystemInfo { api_token: "corto".into(), ..info.clone() };
        assert_eq!(spec_json(&system_embed(&db, &short_token).unwrap()), golden["system"]["feed_off"]);
        db.execute("DELETE FROM settings WHERE key IN ('poll_channel','dashboard_message','activity_channel')", []).unwrap();
        let bare = SystemInfo { public_url: String::new(), api_token: format!(" {}", "t".repeat(40)), database_bytes: None };
        assert_eq!(spec_json(&system_embed(&db, &bare).unwrap()), golden["system"]["bare"]);
    }

    /// L'export redatto ha le stesse tabelle, gli stessi conteggi e gli stessi riepiloghi di quello
    /// prodotto da `_create_redacted_export` di Python sullo stesso stato.
    #[test]
    fn redacted_export_matches_python() {
        let golden: Value = serde_json::from_str(include_str!("golden/export_panel_expected.json")).unwrap();
        let db = db();
        db.set_setting("live_event_token", "segreto").unwrap();
        let bytes = create_redacted_export(&db).unwrap();
        assert!(bytes.starts_with(b"SQLite format 3\0"));

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("export.db");
        std::fs::write(&path, &bytes).unwrap();
        let export = Connection::open(&path).unwrap();
        let tables: Vec<String> = export
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(json!(tables), golden["tables"], "elenco delle tabelle");
        for table in &tables {
            let count: i64 = export.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap();
            assert_eq!(json!(count), golden["counts"][table], "righe di {table}");
        }
        for (table, expected) in golden["exported"].as_object().unwrap() {
            let mut statement = export.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let columns = statement.column_count();
            let mut rows: Vec<Value> = statement
                .query_map([], |row| {
                    Ok(Value::Array(
                        (0..columns)
                            .map(|index| match row.get_ref(index).unwrap() {
                                rusqlite::types::ValueRef::Null => Value::Null,
                                rusqlite::types::ValueRef::Integer(n) => json!(n),
                                rusqlite::types::ValueRef::Real(f) => json!(f),
                                rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                                rusqlite::types::ValueRef::Blob(_) => Value::Null,
                            })
                            .collect(),
                    ))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            // Stesso ordine del golden (testo JSON di ogni riga).
            rows.sort_by_key(|row| serde_json::to_string(row).unwrap());
            let mut wanted: Vec<Value> = expected.as_array().unwrap().clone();
            wanted.sort_by_key(|row| serde_json::to_string(row).unwrap());
            assert_eq!(rows, wanted, "contenuto di {table}");
        }
        let settings: Vec<String> = export.prepare("SELECT key FROM settings ORDER BY key").unwrap().query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(json!(settings), golden["settings"], "il token live è rimosso");
        // La copia non è un file con giornale da portarsi dietro.
        assert!(!directory.path().join("export.db-wal").exists());
    }

    #[test]
    fn export_of_a_broken_database_fails_cleanly() {
        let db = db();
        db.execute("DROP TABLE live_events", []).unwrap();
        assert!(create_redacted_export(&db).is_err());
    }
}
