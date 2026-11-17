//! Autocompletamento delle proposte per nome, al posto degli ID numerici
//! (equivalente di `sfo/autocomplete.py`).
//!
//! Si usano con `#[autocomplete = "any_suggestion"]` su un parametro `i64`
//! (poise: `#[poise::command(slash_command)] async fn cmd(ctx, #[autocomplete = "..."] id: i64)`).
// Verranno usati dai comandi `/scegli`, `/rimuovi-scelta`, `/unisci-proposte` e di revisione.
#![allow(dead_code)]

use poise::serenity_prelude::{AutocompleteChoice, CreateAutocompleteResponse};
use rusqlite::params_from_iter;
use rusqlite::types::Value;
use tracing::error;

use crate::database::{Database, DbResult};
use crate::formatting::{casefold, proposal_type_emoji};
use crate::Context;

/// Discord accetta al massimo 25 scelte, ciascuna con nome fino a 100 caratteri.
const MAX_CHOICES: usize = 25;
const MAX_NAME_CHARS: usize = 100;

/// Stato di revisione in forma breve per l'etichetta; uno stato sconosciuto resta com'è.
pub fn status_short(review_status: &str) -> &str {
    match review_status {
        "pending" => "in attesa",
        "reviewing" => "in revisione",
        "approved" => "approvata",
        "rejected" => "rifiutata",
        other => other,
    }
}

/// Una voce dell'autocompletamento: etichetta mostrata e ID della proposta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestionChoice {
    pub name: String,
    pub value: i64,
}

/// Calcola la distanza di Levenshtein per tolleranza di refusi.
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let mut dp = vec![vec![0; b_chars.len() + 1]; a_chars.len() + 1];
    for i in 0..=a_chars.len() {
        dp[i][0] = i;
    }
    for j in 0..=b_chars.len() {
        dp[0][j] = j;
    }
    for i in 1..=a_chars.len() {
        for j in 1..=b_chars.len() {
            let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
        }
    }
    dp[a_chars.len()][b_chars.len()]
}

/// Calcola un punteggio di rilevanza fuzzy tra la query digitata e il nome della proposta.
/// Restituisce `Some(score)` se c'è corrispondenza o `None` se non corrisponde.
fn fuzzy_score(pattern: &str, target: &str) -> Option<u32> {
    if pattern.is_empty() {
        return Some(100);
    }
    // Corrispondenza esatta
    if target == pattern {
        return Some(1000);
    }
    // Prefisso
    if target.starts_with(pattern) {
        return Some(800 + (50 - target.chars().count().min(50)) as u32);
    }
    // Sottostringa
    if let Some(pos) = target.find(pattern) {
        let is_word_boundary = pos == 0
            || target.as_bytes().get(pos.saturating_sub(1)).map_or(false, |b| b.is_ascii_whitespace() || b.is_ascii_punctuation());
        let bonus = if is_word_boundary { 120 } else { 0 };
        return Some(600 + bonus + (50 - target.chars().count().min(50)) as u32);
    }

    // Sottosequenza di caratteri ordinati (es. "amgus" -> "among us")
    let mut pattern_chars = pattern.chars().peekable();
    let mut score = 0u32;
    let mut consecutive = 0u32;
    let mut prev_idx = None;

    for (idx, ch) in target.chars().enumerate() {
        if let Some(&p_ch) = pattern_chars.peek() {
            if ch == p_ch {
                pattern_chars.next();
                consecutive += 1;
                score += 20 + (consecutive * 10);
                if prev_idx == Some(idx.saturating_sub(1)) {
                    score += 15;
                }
                prev_idx = Some(idx);
            } else {
                consecutive = 0;
            }
        }
    }

    if pattern_chars.peek().is_none() {
        return Some(score.min(500));
    }

    // Tolleranza refuso per parole di almeno 4 lettere (distanza <= 1)
    if pattern.chars().count() >= 4 {
        for word in target.split_whitespace() {
            if levenshtein_distance(pattern, word) <= 1 {
                return Some(350);
            }
        }
    }

    None
}

/// Proposte della settimana che contengono il testo digitato (o il loro `#ID`).
///
/// * `review_statuses`: se non vuoto, solo le proposte in uno di questi stati di revisione.
/// * `picked`: `Some(true)` solo quelle nel ballottaggio, `Some(false)` solo le altre.
pub fn suggestion_choices(
    db: &Database,
    current: &str,
    review_statuses: &[&str],
    picked: Option<bool>,
) -> DbResult<Vec<SuggestionChoice>> {
    let mut sql = String::from(
        "SELECT id, name, tipo, review_status, status FROM suggestions WHERE week=?",
    );
    let mut params: Vec<Value> = vec![Value::Integer(db.current_week()?)];
    if !review_statuses.is_empty() {
        let mut statuses = review_statuses.to_vec();
        statuses.sort_unstable();
        sql.push_str(&format!(" AND review_status IN ({})", vec!["?"; statuses.len()].join(",")));
        params.extend(statuses.into_iter().map(|status| Value::Text(status.to_string())));
    }
    if let Some(picked) = picked {
        sql.push_str(" AND status=?");
        params.push(Value::Text(if picked { "picked" } else { "pending" }.to_string()));
    }
    sql.push_str(" ORDER BY id DESC");

    let text = casefold(current.trim().trim_start_matches('#'));
    db.with_conn(|connection| {
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(params.iter()))?;
        let mut scored_choices: Vec<(u32, i64, SuggestionChoice)> = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get("id")?;
            let name: String = row.get("name")?;
            let tipo: String = row.get("tipo")?;
            let review_status: String = row.get("review_status")?;
            let status: Option<String> = row.get("status")?;

            let score = if text.is_empty() {
                Some(0)
            } else if text == id.to_string() {
                Some(1000)
            } else {
                fuzzy_score(&text, &casefold(&name))
            };

            let Some(score) = score else {
                continue;
            };

            let mut label = format!(
                "#{id} · {} {name} · {}",
                proposal_type_emoji(&tipo),
                status_short(&review_status)
            );
            if status.as_deref() == Some("picked") {
                label.push_str(" · nel ballottaggio");
            }
            scored_choices.push((
                score,
                id,
                SuggestionChoice { name: label.chars().take(MAX_NAME_CHARS).collect(), value: id },
            ));
        }

        // Ordina per rilevanza decrescente; in caso di parità, la proposta più recente prima
        scored_choices.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

        Ok(scored_choices.into_iter().take(MAX_CHOICES).map(|(_, _, choice)| choice).collect())
    })
}

/// Lo scambio con Discord: un errore del database non deve rompere l'autocompletamento,
/// al peggio l'elenco è vuoto e l'utente può scrivere l'ID a mano.
fn discord_choices(
    ctx: Context<'_>,
    current: &str,
    review_statuses: &[&str],
    picked: Option<bool>,
) -> CreateAutocompleteResponse {
    let choices = match suggestion_choices(&ctx.data().db, current, review_statuses, picked) {
        Ok(choices) => choices
            .into_iter()
            .map(|choice| AutocompleteChoice::new(choice.name, choice.value))
            .collect(),
        Err(e) => {
            error!("Autocompletamento delle proposte non riuscito: {e}");
            Vec::new()
        }
    };
    CreateAutocompleteResponse::new().set_choices(choices)
}

pub async fn any_suggestion(ctx: Context<'_>, current: &str) -> CreateAutocompleteResponse {
    discord_choices(ctx, current, &[], None)
}

pub async fn approved_unpicked_suggestion(ctx: Context<'_>, current: &str) -> CreateAutocompleteResponse {
    discord_choices(ctx, current, &["approved"], Some(false))
}

pub async fn picked_suggestion(ctx: Context<'_>, current: &str) -> CreateAutocompleteResponse {
    discord_choices(ctx, current, &[], Some(true))
}

pub async fn pending_review_suggestion(ctx: Context<'_>, current: &str) -> CreateAutocompleteResponse {
    discord_choices(ctx, current, &["pending"], None)
}

pub async fn open_review_suggestion(ctx: Context<'_>, current: &str) -> CreateAutocompleteResponse {
    discord_choices(ctx, current, &["pending", "reviewing"], None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (id, settimana, nome, tipo, stato revisione, stato)
    const ROWS: [(i64, i64, &str, &str, &str, &str); 6] = [
        (1, 1, "Among Us", "Gioco", "approved", "pending"),
        (2, 1, "Jackbox Party", "Gioco", "approved", "picked"),
        (3, 1, "Il Gladiatore", "Film", "pending", "pending"),
        (4, 1, "Karaoke Night", "Karaoke", "reviewing", "pending"),
        (5, 1, "Scartata", "Altro", "rejected", "pending"),
        (6, 2, "Altra settimana", "Gioco", "approved", "pending"),
    ];

    fn db_with_suggestions() -> Database {
        let db = Database::open_in_memory().unwrap();
        for (id, week, name, tipo, review, status) in ROWS {
            db.execute(
                "INSERT INTO suggestions(id,week,norm_name,name,tipo,review_status,status) VALUES(?,?,?,?,?,?,?)",
                rusqlite::params![id, week, name.to_lowercase(), name, tipo, review, status],
            )
            .unwrap();
        }
        db
    }

    fn ids(choices: &[SuggestionChoice]) -> Vec<i64> {
        choices.iter().map(|choice| choice.value).collect()
    }

    #[test]
    fn lists_only_current_week_newest_first() {
        let db = db_with_suggestions();
        let all = suggestion_choices(&db, "", &[], None).unwrap();
        assert_eq!(ids(&all), [5, 4, 3, 2, 1], "la proposta della settimana 2 non compare");
        db.set_setting("week", 2).unwrap();
        assert_eq!(ids(&suggestion_choices(&db, "", &[], None).unwrap()), [6]);
    }

    #[test]
    fn labels_show_id_emoji_name_and_status() {
        let db = db_with_suggestions();
        let all = suggestion_choices(&db, "", &[], None).unwrap();
        let by_id = |id: i64| all.iter().find(|c| c.value == id).unwrap().name.as_str();
        assert_eq!(by_id(1), "#1 · 🎮 Among Us · approvata");
        assert_eq!(by_id(2), "#2 · 🎮 Jackbox Party · approvata · nel ballottaggio");
        assert_eq!(by_id(3), "#3 · 🎬 Il Gladiatore · in attesa");
        assert_eq!(by_id(4), "#4 · 🎤 Karaoke Night · in revisione");
        assert_eq!(by_id(5), "#5 · ✨ Scartata · rifiutata");
    }

    #[test]
    fn filters_by_name_case_insensitively_or_by_id() {
        let db = db_with_suggestions();
        assert_eq!(ids(&suggestion_choices(&db, "  JACK ", &[], None).unwrap()), [2]);
        assert_eq!(ids(&suggestion_choices(&db, "gla", &[], None).unwrap()), [3]);
        assert_eq!(ids(&suggestion_choices(&db, "#3", &[], None).unwrap()), [3], "#ID");
        assert_eq!(ids(&suggestion_choices(&db, "3", &[], None).unwrap()), [3], "ID senza #");
        assert!(suggestion_choices(&db, "inesistente", &[], None).unwrap().is_empty());
    }

    #[test]
    fn fuzzy_matching_supports_typos_and_subsequences() {
        let db = db_with_suggestions();
        // Sottosequenza: "amgus" trova "Among Us" (ID 1)
        assert_eq!(ids(&suggestion_choices(&db, "amgus", &[], None).unwrap()), [1]);
        // Refuso a 1 lettera: "jakbox" trova "Jackbox Party" (ID 2)
        assert_eq!(ids(&suggestion_choices(&db, "jakbox", &[], None).unwrap()), [2]);
    }

    #[test]
    fn filters_by_review_status_and_picked() {
        let db = db_with_suggestions();
        // /scegli: approvate e non ancora nel ballottaggio.
        assert_eq!(ids(&suggestion_choices(&db, "", &["approved"], Some(false)).unwrap()), [1]);
        // /rimuovi-scelta: solo quelle nel ballottaggio.
        assert_eq!(ids(&suggestion_choices(&db, "", &[], Some(true)).unwrap()), [2]);
        // Revisione: in attesa, oppure in attesa o in revisione.
        assert_eq!(ids(&suggestion_choices(&db, "", &["pending"], None).unwrap()), [3]);
        assert_eq!(ids(&suggestion_choices(&db, "", &["pending", "reviewing"], None).unwrap()), [4, 3]);
    }

    #[test]
    fn results_are_capped_at_25_and_names_at_100_characters() {
        let db = Database::open_in_memory().unwrap();
        for id in 1..=40 {
            db.execute(
                "INSERT INTO suggestions(id,week,norm_name,name,tipo) VALUES(?,1,?,?, 'Gioco')",
                rusqlite::params![id, format!("n{id}"), "x".repeat(150)],
            )
            .unwrap();
        }
        let choices = suggestion_choices(&db, "", &[], None).unwrap();
        assert_eq!(choices.len(), 25);
        assert_eq!(choices[0].value, 40);
        assert!(choices.iter().all(|choice| choice.name.chars().count() == 100));
    }

    // Comando di prova: se le firme degli autocompletamenti non fossero compatibili con
    // `#[autocomplete = "..."]` di poise, questo modulo non compilerebbe.
    #[poise::command(slash_command)]
    async fn probe(
        _ctx: Context<'_>,
        #[autocomplete = "any_suggestion"] _any: i64,
        #[autocomplete = "approved_unpicked_suggestion"] _approved: i64,
        #[autocomplete = "picked_suggestion"] _picked: i64,
        #[autocomplete = "pending_review_suggestion"] _pending: i64,
        #[autocomplete = "open_review_suggestion"] _open: i64,
    ) -> Result<(), crate::Error> {
        Ok(())
    }

    #[test]
    fn autocompletes_plug_into_poise_commands() {
        let command = probe();
        assert_eq!(command.parameters.len(), 5);
        assert!(command.parameters.iter().all(|parameter| parameter.autocomplete_callback.is_some()));
    }

    #[test]
    fn unknown_review_status_is_shown_as_is() {
        assert_eq!(status_short("approved"), "approvata");
        assert_eq!(status_short("boh"), "boh");
    }
}
