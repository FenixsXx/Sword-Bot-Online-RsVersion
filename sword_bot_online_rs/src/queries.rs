//! Letture riutilizzabili su sondaggi e proposte, senza scritture (equivalente di `sfo/queries.py`).
//!
//! In Python le query restituiscono righe generiche `sqlite3.Row` da cui si estraggono i campi
//! a runtime per nome o indice. In Rust, per garantire la massima sicurezza ed efficienza,
//! ogni query importante mappa i risultati direttamente in strutture dati fortemente tipizzate
//! (struct) con campi definiti in fase di compilazione.
//!
//! Le query SQL sottostanti mantengono la stessa identica logica della versione Python,
//! garantendo piena compatibilità con lo schema del database.
//! Questi helper vengono utilizzati da dashboard, schede Discord, statistiche e portale web.

// `dead_code` permette di mantenere funzioni e strutture che saranno consumate
// dai moduli futuri del bot senza generare warning di compilazione.
#![allow(dead_code)]

use std::collections::HashMap;

use rusqlite::types::Value;
use rusqlite::{Row, params_from_iter};

use crate::database::{self, Connected, DbResult};

/// Rappresenta un'opzione di un sondaggio con i relativi dati di voto e proposta collegata (`poll_options`).
///
/// In Rust, una `struct` è un tipo di dato personalizzato che raggruppa campi correlati.
/// `#[derive(Debug, Clone, PartialEq, Eq)]` genera automaticamente il codice per:
/// - Stampare la struttura a scopo di debug (`Debug`).
/// - Duplicarla in memoria (`Clone`).
/// - Confrontarla per uguaglianza (`PartialEq`, `Eq`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollOption {
    /// ID univoco dell'opzione nella tabella `options`.
    pub id: i64,
    /// Testo descrittivo dell'opzione mostrato agli utenti.
    pub label: String,
    /// ID della proposta associata (se l'opzione deriva da una proposta della community).
    /// `Option<i64>` significa che può contenere un numero intero (`Some(id)`) oppure essere vuoto (`None`).
    pub suggestion_id: Option<i64>,
    /// Tipo della proposta (es. "Gioco", "Film"), se presente.
    pub tipo: Option<String>,
    /// Link opzionale associato alla proposta (es. URL di Steam o YouTube).
    pub link: Option<String>,
    /// Numero totale di voti ricevuti da questa opzione.
    pub n: i64,
}

impl PollOption {
    /// Funzione di utilità per convertire una singola riga del database (`Row`) in un'istanza di `PollOption`.
    ///
    /// Sfrutta l'operatore `?` per propagare eventuali errori di estrazione dei campi
    /// (ad esempio se un tipo non corrisponde o una colonna manca).
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            label: row.get("label")?,
            suggestion_id: row.get("suggestion_id")?,
            tipo: row.get("tipo")?,
            link: row.get("link")?,
            n: row.get("n")?,
        })
    }
}

/// Recupera tutte le opzioni di un dato sondaggio, calcolando il numero di voti per ciascuna
/// e unendo i dati della proposta eventualmente collegata. I risultati sono ordinati per ID crescente.
pub fn poll_options(db: &impl Connected, poll_id: i64) -> DbResult<Vec<PollOption>> {
    // `db.with_conn` acquisisce una connessione sicura dal pool o gestore thread-safe.
    db.with_conn(|connection| {
        connection
            .prepare(
                "SELECT o.id, o.label, o.suggestion_id, s.tipo, s.link,
                        COUNT(v.user_id) AS n
                 FROM options o
                 LEFT JOIN votes v ON v.option_id = o.id
                 LEFT JOIN suggestions s ON s.id = o.suggestion_id
                 WHERE o.poll_id = ? GROUP BY o.id ORDER BY o.id",
            )?
            .query_map([poll_id], PollOption::from_row)?
            .collect() // Raccoglie tutte le righe mappate in un `Vec<PollOption>` all'interno di un `Result`.
    })
}

/// Rappresenta una proposta approvata della settimana corrente (`current_suggestions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentSuggestion {
    pub id: i64,
    pub week: i64,
    pub norm_name: String,
    pub name: String,
    pub tipo: String,
    pub link: Option<String>,
    /// Stato di avanzamento della proposta (es. `pending` se in attesa, `picked` se selezionata per il ballottaggio).
    pub status: Option<String>,
    /// Timestamp Unix del primo momento in cui qualcuno ha proposto quest'opera.
    pub created_ts: Option<i64>,
    /// Timestamp Unix dell'ultimo inserimento o aggiornamento della proposta.
    pub latest_ts: Option<i64>,
}

/// Restituisce l'elenco delle proposte approvate per la settimana corrente, ordinate dalla più recente.
///
/// Se `pending_only` è true, esclude le proposte che sono già state inserite nel ballottaggio (`status='picked'`).
pub fn current_suggestions(db: &impl Connected, limit: i64, pending_only: bool) -> DbResult<Vec<CurrentSuggestion>> {
    // Costruisce dinamicamente il filtro SQL in base al parametro booleano.
    let status_filter = if pending_only { "AND s.status='pending'" } else { "" };
    // Recupera il numero della settimana corrente dal database.
    let week = database::current_week(db)?;
    db.with_conn(|connection| {
        connection
            .prepare(&format!(
                "SELECT s.id, s.week, s.norm_name, s.name, s.tipo, s.link, s.status,
                        MIN(p.ts) AS created_ts, MAX(p.ts) AS latest_ts
                 FROM suggestions s
                 LEFT JOIN proposals p ON p.suggestion_id=s.id AND p.week=s.week
                 WHERE s.week=? AND s.review_status='approved' {status_filter}
                 GROUP BY s.id
                 ORDER BY latest_ts DESC, s.id DESC
                 LIMIT ?"
            ))?
            .query_map([week, limit], |row| {
                Ok(CurrentSuggestion {
                    id: row.get("id")?,
                    week: row.get("week")?,
                    norm_name: row.get("norm_name")?,
                    name: row.get("name")?,
                    tipo: row.get("tipo")?,
                    link: row.get("link")?,
                    status: row.get("status")?,
                    created_ts: row.get("created_ts")?,
                    latest_ts: row.get("latest_ts")?,
                })
            })?
            .collect()
    })
}

/// Calcola il numero totale di sostenitori per ciascuna proposta specificata in una data settimana.
///
/// Il conteggio unisce gli autori originali della proposta (`proposals`) e chi ha aggiunto
/// il proprio supporto in un secondo momento (`suggestion_supports`), evitando di contare
/// due volte lo stesso utente (grazie all'operatore SQL `UNION`).
/// Restituisce una mappa (`HashMap`) che associa l'ID della proposta al numero di sostenitori.
pub fn suggestion_supporter_counts(
    db: &impl Connected,
    week: i64,
    suggestion_ids: &[i64],
) -> DbResult<HashMap<i64, i64>> {
    // Se non ci sono ID richiesti, restituisce subito una mappa vuota senza eseguire query.
    if suggestion_ids.is_empty() {
        return Ok(HashMap::new());
    }
    // Genera una stringa di punti interrogativi per la clausola `IN (?, ?, ...)` in modo dinamico e sicuro.
    let placeholders = vec!["?"; suggestion_ids.len()].join(",");
    let sql = format!(
        "SELECT suggestion_id, COUNT(*) AS supporters
         FROM (
             SELECT suggestion_id, user_id
             FROM proposals
             WHERE week=? AND suggestion_id IN ({placeholders})
             UNION
             SELECT suggestion_id, user_id
             FROM suggestion_supports
             WHERE week=? AND suggestion_id IN ({placeholders})
         )
         GROUP BY suggestion_id"
    );
    // Costruisce la lista dei parametri nell'ordine esatto dei segnaposto:
    // prima query (settimana + ID) e seconda query (settimana + ID).
    let params: Vec<Value> = std::iter::once(week)
        .chain(suggestion_ids.iter().copied())
        .chain(std::iter::once(week))
        .chain(suggestion_ids.iter().copied())
        .map(Value::Integer)
        .collect();
    db.with_conn(|connection| {
        connection
            .prepare(&sql)?
            .query_map(params_from_iter(params.iter()), |row| {
                Ok((row.get::<_, i64>("suggestion_id")?, row.get::<_, i64>("supporters")?))
            })?
            .collect() // Raccoglie i risultati in un iteratore e li converte automaticamente in HashMap.
    })
}

/// Riga della classifica per sostenitori (`suggestion_supporter_ranking`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupporterRank {
    pub id: i64,
    pub name: String,
    pub tipo: String,
    pub status: Option<String>,
    pub supporters: i64,
}

/// Restituisce le proposte approvate della settimana ordinate per numero di sostenitori decrescente
/// (e a parità di sostenitori, per ID decrescente).
pub fn suggestion_supporter_ranking(
    db: &impl Connected,
    week: i64,
    limit: i64,
    pending_only: bool,
) -> DbResult<Vec<SupporterRank>> {
    let status_filter = if pending_only { "AND s.status='pending'" } else { "" };
    db.with_conn(|connection| {
        connection
            .prepare(&format!(
                "WITH supporters AS (
                     SELECT suggestion_id, user_id FROM proposals WHERE week=?
                     UNION
                     SELECT suggestion_id, user_id FROM suggestion_supports WHERE week=?
                 )
                 SELECT s.id, s.name, s.tipo, s.status,
                        COUNT(sup.user_id) AS supporters
                 FROM suggestions s
                 LEFT JOIN supporters sup ON sup.suggestion_id=s.id
                 WHERE s.week=? AND s.review_status='approved' {status_filter}
                 GROUP BY s.id
                 ORDER BY supporters DESC, s.id DESC
                 LIMIT ?"
            ))?
            .query_map([week, week, week, limit], |row| {
                Ok(SupporterRank {
                    id: row.get("id")?,
                    name: row.get("name")?,
                    tipo: row.get("tipo")?,
                    status: row.get("status")?,
                    supporters: row.get("supporters")?,
                })
            })?
            .collect()
    })
}

/// Rappresenta una voce nell'elenco generale delle proposte (`suggestions_list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSuggestion {
    pub id: i64,
    pub name: String,
    pub tipo: String,
    pub link: Option<String>,
    pub status: Option<String>,
    pub review_status: String,
    /// Numero di persone distinte che hanno avanzato questa proposta.
    pub n: i64,
}

/// Restituisce tutte le proposte della settimana corrente che hanno almeno una proposta registrata,
/// ordinate per numero di proponenti (dalla più richiesta).
///
/// Se viene fornito un `tipo` (non vuoto), filtra i risultati per quel tipo ignorando le maiuscole/minuscole.
pub fn suggestions_list(db: &impl Connected, tipo: Option<&str>) -> DbResult<Vec<ListedSuggestion>> {
    let mut sql = String::from(
        "SELECT s.id, s.name, s.tipo, s.link, s.status, s.review_status,
                COUNT(DISTINCT p.user_id) AS n
         FROM suggestions s JOIN proposals p ON p.suggestion_id = s.id
         WHERE s.week = ?",
    );
    let mut params = vec![Value::Integer(database::current_week(db)?)];
    // Se è stato specificato un filtro per tipo, aggiunge la clausola SQL e converte il testo in minuscolo.
    if let Some(tipo) = tipo.filter(|tipo| !tipo.is_empty()) {
        sql.push_str(" AND LOWER(s.tipo) = ?");
        params.push(Value::Text(tipo.to_lowercase()));
    }
    sql.push_str(" GROUP BY s.id ORDER BY n DESC, s.id");
    db.with_conn(|connection| {
        connection
            .prepare(&sql)?
            .query_map(params_from_iter(params.iter()), |row| {
                Ok(ListedSuggestion {
                    id: row.get("id")?,
                    name: row.get("name")?,
                    tipo: row.get("tipo")?,
                    link: row.get("link")?,
                    status: row.get("status")?,
                    review_status: row.get("review_status")?,
                    n: row.get("n")?,
                })
            })?
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use rusqlite::params;

    /// Crea un database di test in memoria popolato con una fixture standard:
    /// Settimana 1: 4 proposte (3 approvate), una inserita nel ballottaggio, con vari sostegni e voti.
    fn fixture() -> Database {
        let db = Database::open_in_memory().unwrap();
        db.with_conn(|c| {
            c.execute_batch(
                "INSERT INTO polls(id,name) VALUES(1,'Sondaggio');
                 INSERT INTO suggestions(id,week,norm_name,name,tipo,link,status,review_status) VALUES
                    (1,1,'amongus','Among Us','Gioco',NULL,'pending','approved'),
                    (2,1,'jackbox','Jackbox','Gioco','https://x.test','picked','approved'),
                    (3,1,'gladiatore','Il Gladiatore','Film',NULL,'pending','approved'),
                    (4,1,'bozza','Bozza','Gioco',NULL,'pending','pending'),
                    (5,2,'altra','Altra settimana','Gioco',NULL,'pending','approved');
                 INSERT INTO proposals(suggestion_id,user_id,week,ts) VALUES
                    (1,10,1,100),(1,11,1,300),(2,10,1,200),(3,12,1,400),(4,13,1,500),(5,10,2,600);
                 INSERT INTO suggestion_supports(suggestion_id,user_id,week,created_at) VALUES
                    (1,11,1,310), -- già autore: non deve contare due volte
                    (1,14,1,320),
                    (3,10,1,410),
                    (5,11,2,610);
                 INSERT INTO options(id,poll_id,label,suggestion_id) VALUES (1,1,'Among Us',1),(2,1,'Libera',NULL);
                 INSERT INTO votes(option_id,user_id) VALUES (1,10),(1,11),(1,12);",
            )
            .unwrap();
        });
        db
    }

    #[test]
    /// Verifica che `poll_options` conti correttamente i voti e unisca i dati delle proposte.
    fn poll_options_count_votes_and_join_suggestion_data() {
        let db = fixture();
        let options = poll_options(&db, 1).unwrap();
        assert_eq!(options.len(), 2);
        assert_eq!(
            options[0],
            PollOption {
                id: 1,
                label: "Among Us".into(),
                suggestion_id: Some(1),
                tipo: Some("Gioco".into()),
                link: None,
                n: 3,
            }
        );
        assert_eq!(options[1].suggestion_id, None);
        assert_eq!(options[1].tipo, None, "opzione libera: nessun dato di proposta collegato");
        assert_eq!(options[1].n, 0);
        assert!(poll_options(&db, 99).unwrap().is_empty(), "ID sondaggio inesistente restituisce vettore vuoto");
    }

    #[test]
    /// Verifica che `current_suggestions` restituisca le proposte approvate della settimana in ordine di data decrescente.
    fn current_suggestions_are_approved_this_week_newest_first() {
        let db = fixture();
        let all = current_suggestions(&db, 25, false).unwrap();
        // Ordinate per ultimo momento di proposta: Gladiatore (400), Among Us (300), Jackbox (200).
        assert_eq!(all.iter().map(|s| s.id).collect::<Vec<_>>(), [3, 1, 2]);
        let among_us = &all[1];
        assert_eq!((among_us.created_ts, among_us.latest_ts), (Some(100), Some(300)));
        assert_eq!(among_us.norm_name, "amongus");
        assert_eq!(all[2].link.as_deref(), Some("https://x.test"));

        let pending = current_suggestions(&db, 25, true).unwrap();
        assert_eq!(pending.iter().map(|s| s.id).collect::<Vec<_>>(), [3, 1], "senza quelle già scelte per il ballottaggio");
        assert_eq!(current_suggestions(&db, 1, false).unwrap().len(), 1, "rispetta il limite massimo");
        db.set_setting("week", 2).unwrap();
        assert_eq!(current_suggestions(&db, 25, false).unwrap()[0].id, 5);
    }

    #[test]
    /// Verifica che il conteggio dei sostenitori unisca autori e sostenitori senza duplicati.
    fn supporter_counts_merge_authors_and_supporters_without_double_counting() {
        let db = fixture();
        let counts = suggestion_supporter_counts(&db, 1, &[1, 2, 3, 4]).unwrap();
        // Among Us: autori 10 e 11 + sostenitori 11 (già autore, non ricalcolato) e 14 -> totale 3.
        assert_eq!(counts, HashMap::from([(1, 3), (2, 1), (3, 2), (4, 1)]));
        assert_eq!(suggestion_supporter_counts(&db, 1, &[1]).unwrap(), HashMap::from([(1, 3)]), "solo gli ID richiesti");
        assert!(suggestion_supporter_counts(&db, 1, &[]).unwrap().is_empty());
        assert!(suggestion_supporter_counts(&db, 1, &[999]).unwrap().is_empty(), "ID inesistente non produce conteggi");
        assert_eq!(suggestion_supporter_counts(&db, 2, &[1, 5]).unwrap(), HashMap::from([(5, 2)]), "filtro per settimana corretto");
    }

    #[test]
    /// Verifica che la classifica dei sostenitori ordini per numero di sostenitori e poi per ID.
    fn supporter_ranking_orders_by_supporters_then_newest_id() {
        let db = fixture();
        let ranking = suggestion_supporter_ranking(&db, 1, 5, false).unwrap();
        let summary: Vec<(i64, i64)> = ranking.iter().map(|r| (r.id, r.supporters)).collect();
        assert_eq!(summary, [(1, 3), (3, 2), (2, 1)], "la proposta non approvata (4) non compare");
        assert_eq!(ranking[0].name, "Among Us");
        assert_eq!(ranking[2].status.as_deref(), Some("picked"));

        let pending = suggestion_supporter_ranking(&db, 1, 5, true).unwrap();
        assert_eq!(pending.iter().map(|r| r.id).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(suggestion_supporter_ranking(&db, 1, 1, false).unwrap().len(), 1);
    }

    #[test]
    /// Verifica che la classifica includa anche le proposte approvate che non hanno ricevuto sostenitori extra.
    fn supporter_ranking_includes_approved_suggestions_without_supporters() {
        let db = fixture();
        db.execute(
            "INSERT INTO suggestions(id,week,norm_name,name,tipo,review_status) VALUES(9,1,'solo','Solitaria','Gioco','approved')",
            [],
        )
        .unwrap();
        let ranking = suggestion_supporter_ranking(&db, 1, 10, false).unwrap();
        let last = ranking.last().unwrap();
        assert_eq!((last.id, last.supporters), (9, 0), "LEFT JOIN garantisce la presenza con zero sostenitori");
    }

    #[test]
    /// Verifica l'elenco generale delle proposte con conteggio proponenti distinti e filtro per tipo.
    fn suggestions_list_counts_distinct_proposers_and_filters_by_type() {
        let db = fixture();
        let all = suggestions_list(&db, None).unwrap();
        assert_eq!(all.iter().map(|s| (s.id, s.n)).collect::<Vec<_>>(), [(1, 2), (2, 1), (3, 1), (4, 1)]);
        assert_eq!(all[3].review_status, "pending");

        let films = suggestions_list(&db, Some("FILM")).unwrap();
        assert_eq!(films.iter().map(|s| s.id).collect::<Vec<_>>(), [3], "filtro tipo case-insensitive");
        assert_eq!(suggestions_list(&db, Some("")).unwrap().len(), 4, "tipo vuoto non applica filtri");
        assert!(suggestions_list(&db, Some("karaoke")).unwrap().is_empty());
    }

    #[test]
    /// Verifica che proposte multiple dello stesso utente vengano contate una sola volta (`COUNT(DISTINCT user_id)`).
    fn proposals_of_the_same_user_are_counted_once_in_the_list() {
        let db = fixture();
        db.execute("INSERT INTO proposals(suggestion_id,user_id,week,ts) VALUES(?,?,?,?)", params![3, 12, 1, 450]).unwrap();
        let listed = suggestions_list(&db, Some("film")).unwrap();
        assert_eq!(listed[0].n, 1, "lo stesso utente non deve gonfiare il conteggio dei proponenti");
    }
}
