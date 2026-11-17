//! Letture riutilizzabili su sondaggi e proposte, senza scritture (equivalente di `sfo/queries.py`).
//!
//! Python restituisce righe `sqlite3.Row`; qui ogni query ha la sua struttura tipizzata.
//! Le query SQL sono le stesse, parola per parola.
// Verranno usate da schede, dashboard, statistiche e sito.
#![allow(dead_code)]

use std::collections::HashMap;

use rusqlite::types::Value;
use rusqlite::{Row, params_from_iter};

use crate::database::{self, Connected, DbResult};

/// Opzione di un sondaggio con i suoi voti (`poll_options`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollOption {
    pub id: i64,
    pub label: String,
    pub suggestion_id: Option<i64>,
    pub tipo: Option<String>,
    pub link: Option<String>,
    /// Numero di voti.
    pub n: i64,
}

impl PollOption {
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

/// Opzioni di un sondaggio, con voti e dati della proposta collegata, in ordine di ID.
pub fn poll_options(db: &impl Connected, poll_id: i64) -> DbResult<Vec<PollOption>> {
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
            .collect()
    })
}

/// Proposta approvata della settimana corrente (`current_suggestions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentSuggestion {
    pub id: i64,
    pub week: i64,
    pub norm_name: String,
    pub name: String,
    pub tipo: String,
    pub link: Option<String>,
    /// `pending` o `picked`.
    pub status: Option<String>,
    /// Primo e ultimo momento in cui qualcuno l'ha proposta (timestamp Unix).
    pub created_ts: Option<i64>,
    pub latest_ts: Option<i64>,
}

/// Proposte approvate della settimana, le più recenti per prime.
///
/// Con `pending_only` esclude quelle già nel ballottaggio (`status='picked'`).
pub fn current_suggestions(db: &impl Connected, limit: i64, pending_only: bool) -> DbResult<Vec<CurrentSuggestion>> {
    let status_filter = if pending_only { "AND s.status='pending'" } else { "" };
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

/// Per ogni proposta, quante persone la sostengono: chi l'ha proposta più chi ha aggiunto il suo
/// sostegno, contando una sola volta chi ha fatto entrambe le cose. Le proposte senza
/// sostenitori non compaiono nella mappa.
pub fn suggestion_supporter_counts(
    db: &impl Connected,
    week: i64,
    suggestion_ids: &[i64],
) -> DbResult<HashMap<i64, i64>> {
    if suggestion_ids.is_empty() {
        return Ok(HashMap::new());
    }
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
    // Parametri nell'ordine dei segnaposto: settimana, ID, settimana, ID.
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
            .collect()
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

/// Proposte approvate della settimana ordinate per numero di sostenitori (poi per ID, decrescente).
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

/// Riga dell'elenco proposte (`suggestions_list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSuggestion {
    pub id: i64,
    pub name: String,
    pub tipo: String,
    pub link: Option<String>,
    pub status: Option<String>,
    pub review_status: String,
    /// Persone distinte che l'hanno proposta.
    pub n: i64,
}

/// Proposte della settimana (di qualsiasi stato di revisione) con almeno una proposta registrata,
/// dalla più richiesta. Con `tipo` (non vuoto) filtra per tipo, senza distinguere le maiuscole.
pub fn suggestions_list(db: &impl Connected, tipo: Option<&str>) -> DbResult<Vec<ListedSuggestion>> {
    let mut sql = String::from(
        "SELECT s.id, s.name, s.tipo, s.link, s.status, s.review_status,
                COUNT(DISTINCT p.user_id) AS n
         FROM suggestions s JOIN proposals p ON p.suggestion_id = s.id
         WHERE s.week = ?",
    );
    let mut params = vec![Value::Integer(database::current_week(db)?)];
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

    /// Settimana 1: 4 proposte (3 approvate), una nel ballottaggio; qualche sostegno.
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
                    (1,11,1,310), -- già autore: non conta due volte
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
        assert_eq!(options[1].tipo, None, "opzione libera: niente dati di proposta");
        assert_eq!(options[1].n, 0);
        assert!(poll_options(&db, 99).unwrap().is_empty());
    }

    #[test]
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
        assert_eq!(pending.iter().map(|s| s.id).collect::<Vec<_>>(), [3, 1], "senza quelle già scelte");
        assert_eq!(current_suggestions(&db, 1, false).unwrap().len(), 1, "limite");
        db.set_setting("week", 2).unwrap();
        assert_eq!(current_suggestions(&db, 25, false).unwrap()[0].id, 5);
    }

    #[test]
    fn supporter_counts_merge_authors_and_supporters_without_double_counting() {
        let db = fixture();
        let counts = suggestion_supporter_counts(&db, 1, &[1, 2, 3, 4]).unwrap();
        // Among Us: autori 10 e 11 + sostenitori 11 (già contato) e 14 -> 3.
        assert_eq!(counts, HashMap::from([(1, 3), (2, 1), (3, 2), (4, 1)]));
        assert_eq!(suggestion_supporter_counts(&db, 1, &[1]).unwrap(), HashMap::from([(1, 3)]), "solo gli ID richiesti");
        assert!(suggestion_supporter_counts(&db, 1, &[]).unwrap().is_empty());
        assert!(suggestion_supporter_counts(&db, 1, &[999]).unwrap().is_empty(), "senza sostenitori: assente");
        assert_eq!(suggestion_supporter_counts(&db, 2, &[1, 5]).unwrap(), HashMap::from([(5, 2)]), "per settimana");
    }

    #[test]
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
    fn supporter_ranking_includes_approved_suggestions_without_supporters() {
        let db = fixture();
        db.execute(
            "INSERT INTO suggestions(id,week,norm_name,name,tipo,review_status) VALUES(9,1,'solo','Solitaria','Gioco','approved')",
            [],
        )
        .unwrap();
        let ranking = suggestion_supporter_ranking(&db, 1, 10, false).unwrap();
        let last = ranking.last().unwrap();
        assert_eq!((last.id, last.supporters), (9, 0), "LEFT JOIN: compare con zero sostenitori");
    }

    #[test]
    fn suggestions_list_counts_distinct_proposers_and_filters_by_type() {
        let db = fixture();
        let all = suggestions_list(&db, None).unwrap();
        // Tutte le proposte della settimana con almeno una proposta, anche non approvate.
        assert_eq!(all.iter().map(|s| (s.id, s.n)).collect::<Vec<_>>(), [(1, 2), (2, 1), (3, 1), (4, 1)]);
        assert_eq!(all[3].review_status, "pending");

        let films = suggestions_list(&db, Some("FILM")).unwrap();
        assert_eq!(films.iter().map(|s| s.id).collect::<Vec<_>>(), [3], "tipo senza distinzione di maiuscole");
        assert_eq!(suggestions_list(&db, Some("")).unwrap().len(), 4, "tipo vuoto: nessun filtro");
        assert!(suggestions_list(&db, Some("karaoke")).unwrap().is_empty());
    }

    #[test]
    fn proposals_of_the_same_user_are_counted_once_in_the_list() {
        let db = fixture();
        db.execute("INSERT INTO proposals(suggestion_id,user_id,week,ts) VALUES(?,?,?,?)", params![3, 12, 1, 450]).unwrap();
        let listed = suggestions_list(&db, Some("film")).unwrap();
        assert_eq!(listed[0].n, 1, "COUNT(DISTINCT user_id)");
    }
}
