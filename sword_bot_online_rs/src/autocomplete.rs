//! Modulo di autocompletamento per le proposte
//!
//! Questo modulo implementa funzioni di autocompletamento intelligente per i comandi slash del bot,
//! consentendo agli utenti di cercare proposte per nome invece di dover ricordare gli ID numerici.
//! È stato creato come equivalente in Rust della libreria Python `sfo/autocomplete.py`.
//!
//! Le funzioni sono destinate all'uso con l'attributo `#[autocomplete = "..."]` di poise,
//! ad esempio: `#[autocomplete = "any_suggestion"]` su un parametro `i64` in un comando slash.
//! Sono usate dai comandi `/scegli`, `/rimuovi-scelta`, `/unisci-proposte` e dai comandi di revisione.
//!
//! Il sistema cerca di corrispondere testi parziali o imperfetti ai nomi delle proposte memorizzate,
//! offrendo suggerimenti anche in presenza di errori di digitazione o refusi.
#![allow(dead_code)]

use poise::serenity_prelude::{AutocompleteChoice, CreateAutocompleteResponse};
use rusqlite::params_from_iter;
use rusqlite::types::Value;
use tracing::error;

use crate::database::{Database, DbResult};
use crate::formatting::{casefold, proposal_type_emoji};
use crate::Context;

// Discord impone dei limiti: al massimo 25 scelte per richiesta, nomi mostrati non oltre 100 caratteri.
const MAX_CHOICES: usize = 25;
const MAX_NAME_CHARS: usize = 100;

/// Stato di revisione in forma abbreviata per l'etichetta visualizzata agli utenti.
/// Se lo stato non è riconosciuto, viene restituito invariato.
pub fn status_short(review_status: &str) -> &str {
    match review_status {
        "pending" => "in attesa",        // Utente vede che la proposta è in attesa di azione
        "reviewing" => "in revisione",  // In fase di valutazione da parte del team
        "approved" => "approvata",      // Proposta accettata
        "rejected" => "rifiutata",      // Proposta scartata
        other => other,                 // Stato sconosciuto: mostralo come è
    }
}

/// Una voce del menu di autocompletamento: contiene l'etichetta da mostrare all'utente
/// e il valore numerico (l'ID univoco della proposta) da usare se l'utente la seleziona.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestionChoice {
    pub name: String,  // Testo che l'utente vede nel menu (es. "#1 · 🎮 Among Us · approvata")
    pub value: i64,    // L'ID univoco della proposta da usare quando l'utente la conferma
}

/// Calcola la distanza di Levenshtein tra due stringhe.
/// **Cosa fa:** Misura quanti caratteri bisogna inserire, cancellare o sostituire
/// per trasformare una stringa nell'altra. È l'algoritmo standard per il "correggio" di testo.
///
/// **Perché esiste:** Per tollerare piccoli errori di digitazione quando l'utente cerca una proposta.
/// Ad esempio, digitando "Haxbox" si troverà comunque "Jackbox Party".
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    // Crea una tabella dinamica per i calcoli: dp[i][j] = distanza tra primi i char di a e j char di b
    let mut dp = vec![vec![0; b_chars.len() + 1]; a_chars.len() + 1];
    // Inizializza la prima colonna: trasformare stringa vuota in a richiede i insert di tutti i char
    for i in 0..=a_chars.len() {
        dp[i][0] = i;
    }
    // Inizializza la prima riga: trasformare a in stringa vuota richiede i delete di tutti i char
    for j in 0..=b_chars.len() {
        dp[0][j] = j;
    }
    // Riempie la tabella: per ogni coppia di caratteri, sceglie l'operazione più economica
    for i in 1..=a_chars.len() {
        for j in 1..=b_chars.len() {
            let cost = if a_chars[i - 1] == b_chars[j - 1] { 0 } else { 1 };
            // La nuova cella = minimo tra:
            // - cancellare un char da a (dp[i-1][j] + 1)
            // - inserire un char in a (dp[i][j-1] + 1)
            // - sostituire se i char differiscono (dp[i-1][j-1] + cost)
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
        }
    }
    // Il risultato finale è nell'angolo in basso a destra della tabella
    dp[a_chars.len()][b_chars.len()]
}

/// Calcola un punteggio di "rilevanza fuzzy" (approssimato) tra la query dell'utente e il nome della proposta.
/// **Cosa fa:** Assegna un punteggio numerico più alto a corrispondenze migliori.
/// Restituisce `Some(score)` se c'è una qualche corrispondenza, `None` se non c'è match.
///
/// **Perché esiste:** Per ordinare i risultati mostrando prima i suggerimenti più pertinenti.
/// Il punteggio tiene conto di corrispondenze esatte, prefissi, sottostringhe e simili parziali.
fn fuzzy_score(pattern: &str, target: &str) -> Option<u32> {
    // Se l'utente non ha digitato nulla, dà il punteggio massimo di base (100)
    if pattern.is_empty() {
        return Some(100);
    }
    // Corrispondenza esatta: il punteggio più alto possibile
    if target == pattern {
        return Some(1000);
    }
    // L'input dell'utente è all'inizio del nome della proposta (es. "gla" trova "Gladiator")
    if target.starts_with(pattern) {
        // Bonus aggiuntivo per nomi corti (meno di 50 caratteri)
        return Some(800 + (50 - target.chars().count().min(50)) as u32);
    }
    // La query compare da qualche parte dentro il nome (es. "among" in "Among Us")
    if let Some(pos) = target.find(pattern) {
        // Controlla se la corrispondenza inizia dopo uno spazio o un carattere di punteggiatura
        // (indica una corrispondenza "a parola", non parte di una parola lunga)
        let is_word_boundary = pos == 0
            || target.as_bytes().get(pos.saturating_sub(1)).map_or(false, |b| b.is_ascii_whitespace() || b.is_ascii_punctuation());
        let bonus = if is_word_boundary { 120 } else { 0 };
        // Punteggio base + bonus per boundary + penalità per nomi lunghi
        return Some(600 + bonus + (50 - target.chars().count().min(50)) as u32);
    }

    // Cerca una "sottosequenza ordinata" di caratteri (es. digitando "amgus" si trova "Among Us")
    // L'utente ha digitato caratteri che appaiono nell'ordine nel target, ma forse con lettere extra
    let mut pattern_chars = pattern.chars().peekable();
    let mut score = 0u32;
    let mut consecutive = 0u32;  // Quanti caratteri sono consecutivi nell'ordinamento
    let mut prev_idx = None;     // Posizione dell'ultimo carattere matched

    for (idx, ch) in target.chars().enumerate() {
        if let Some(&p_ch) = pattern_chars.peek() {
            if ch == p_ch {
                // Trovato il carattere cercato: avanzare nel pattern
                pattern_chars.next();
                consecutive += 1;
                score += 20 + (consecutive * 10);  // Più caratteri consecutivi = punteggio più alto
                if prev_idx == Some(idx.saturating_sub(1)) {
                    score += 15;  // Bonus se i caratteri consecutivi sono adiacenti nel target
                }
                prev_idx = Some(idx);
            } else {
                // Carattere non corrisponde, reset della sequenza consecutiva
                consecutive = 0;
            }
        }
    }

    // Se abbiamo percorso tutto il pattern, significa che i caratteri ordinati sono presenti
    if pattern_chars.peek().is_none() {
        return Some(score.min(500));  // Massimo 500 per questo tipo di match
    }

    // Tolleranza refuso: se l'utente ha scritto una parola di almeno 4 caratteri,
    // controlla se la distanza di Levenshtein con una parola singola è al massimo 1
    if pattern.chars().count() >= 4 {
        for word in target.split_whitespace() {
            if levenshtein_distance(pattern, word) <= 1 {
                return Some(350);  // Buon match nonostante l'errore di battitura
            }
        }
    }

    // Nessun tipo di match trovato
    None
}

/// Recupera dalla database le proposte della settimana corrente, applicando filtri opzionali
/// su stato di revisione e selezione, e restituisce un elenco "scores" ordinato per rilevanza.
///
/// **Cosa fa:** Interroga il database SQLite delle proposte settimanali, calcola per ogni proposta
/// un punteggio di corrispondenza basato sul testo digitato dall'utente (usando `fuzzy_score`),
/// e restituisce fino a 25 migliori corrispondenze con le loro etichette formattate.
///
/// **Perché esiste:** È il cuore del sistema di autocompletamento: senza questa funzione, gli utenti
/// dovrebbero ricordarsi gli ID numerici esatti invece di poter cercare per nome.
///
/// **Come funziona:**
/// 1. Costruisce una query SQL per selezionare le proposte della settimana corrente
/// 2. Aggiunge filtri opzionali per stato di revisione (`review_statuses`) e stato di ballottaggio (`picked`)
/// 3. Per ogni riga recuperata, calcola un punteggio di rilevanza usando `fuzzy_score`
/// 4. Costruisce un'etichetta formattata per ogni scelta (ID, emoji del tipo, nome, stato)
/// 5. Ordina i risultati per punteggio decrescente (miglior punteggio prima)
/// 6. Restituisce solo i primi 25 (limite imposto da Discord)
pub fn suggestion_choices(
    db: &Database,
    current: &str,
    review_statuses: &[&str],
    picked: Option<bool>,
) -> DbResult<Vec<SuggestionChoice>> {
    // Parte base della query SQL: seleziona le colonne necessarie dalla tabella settimanale
    let mut sql = String::from(
        "SELECT id, name, tipo, review_status, status FROM suggestions WHERE week=?",
    );
    let mut params: Vec<Value> = vec![Value::Integer(db.current_week()?)];

    // Aggiunge filtro per stati di recensione specificati (es. solo "approved" o "pending")
    if !review_statuses.is_empty() {
        let mut statuses = review_statuses.to_vec();
        statuses.sort_unstable();  // Ordina per garantire ordine costante nella query
        sql.push_str(&format!(
            " AND review_status IN ({})",
            vec!["?"; statuses.len()].join(",")
        ));
        params.extend(statuses.into_iter().map(|status| Value::Text(status.to_string())));
    }

    // Aggiunge filtro per proposte nel ballottaggio o meno
    if let Some(picked) = picked {
        sql.push_str(" AND status=?");
        params.push(Value::Text(if picked { "picked" } else { "pending" }.to_string()));
    }

    // Ordina i risultati per ID decrescente (nuove proposte prima)
    sql.push_str(" ORDER BY id DESC");

    // Normalizza il testo digitato dall'utente (minuscolo, senza accenti, spazi extra, ecc.)
    let text = casefold(current.trim().trim_start_matches('#'));
    db.with_conn(|connection| {
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(params.iter()))?;
        let mut scored_choices: Vec<(u32, i64, SuggestionChoice)> = Vec::new();

        // Processa ogni riga ritornata dal database
        while let Some(row) = rows.next()? {
            let id: i64 = row.get("id")?;
            let name: String = row.get("name")?;      // Nome originale della proposta
            let tipo: String = row.get("tipo")?;      // Categoria (Gioco, Film, ecc.)
            let review_status: String = row.get("review_status")?;  // Stato di revisione
            let status: Option<String> = row.get("status")?;  // Stato ballottaggio (picked/pending)

            // Calcola il punteggio di rilevanza:
            // - Se l'utente ha digitato nulla, punteggio 0 (mostra tutto ordinato per recenza)
            // - Se l'input corrisponde esattamente all'ID, punteggio altissimo (1000)
            // - Altrimenti, usa il fuzzy matching sul nome della proposta
            let score = if text.is_empty() {
                Some(0)
            } else if text == id.to_string() {
                Some(1000)  // Match esatto per ID: prioritário assoluto
            } else {
                fuzzy_score(&text, &casefold(&name))  // Match fuzzy sul nome
            };

            // Se per qualche motivo il punteggio non è calcolabile, scarta questa scelta
            let Some(score) = score else {
                continue;
            };

            // Costruisce l'etichetta visibile all'utente:
            // Formato: "#ID · [emoji] Nome · [stato_revisione] · [nel_ballottaggio opzionale]"
            let mut label = format!(
                "#{id} · {} {name} · {}",
                proposal_type_emoji(&tipo),
                status_short(&review_status)
            );
            if status.as_deref() == Some("picked") {
                label.push_str(" · nel ballottaggio");
            }

            // Aggiunge il risultato al vettore con: (punteggio, id, oggetto_choice)
            scored_choices.push((
                score,
                id,
                SuggestionChoice { name: label.chars().take(MAX_NAME_CHARS).collect(), value: id },
            ));
        }

        // Ordina i risultati:
        // 1. Per punteggio decrescente (migliori match prima)
        // 2. In caso di parità di punteggio, per ID decrescente (le proposte più recenti prima)
        scored_choices.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

        // Restituisce solo i primi 25 (limite Discord) e mappa al tipo restituito
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
