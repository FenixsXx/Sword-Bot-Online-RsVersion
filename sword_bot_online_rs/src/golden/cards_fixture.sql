-- Stato di partenza condiviso dai test di parità di `cards` (eseguito dopo `initialize`, che crea
-- i 5 sondaggi di partenza con le loro 16 opzioni e la stagione corrente).
INSERT OR REPLACE INTO settings(key,value) VALUES('week','7'),('stats_since','2026-09-01'),('awards_since','2026-09-15');

-- Sondaggi: scadenza con decimali, nessuna scadenza, chiuso, colore non valido, tanti e nessuna opzione.
UPDATE polls SET closes_at=1794826800.5 WHERE id=1;
UPDATE polls SET active=0, closes_at=1794222000 WHERE id=3;
UPDATE polls SET color='xyz' WHERE id=4;
INSERT INTO polls(id,name,emoji,color,position,active,is_community) VALUES
    (6,'Tanti','🎲','#123456',1,1,0),
    (7,'Vuoto','🕳️','#654321',2,1,0),
    (8,'Extra 8','8️⃣','#111111',3,1,0),(9,'Extra 9','9️⃣','#222222',3,1,0),(10,'Extra 10','🔟','#333333',3,1,0),
    (11,'Extra 11','1️⃣','#444444',3,1,0),(12,'Extra 12','2️⃣','#555555',3,1,0),(13,'Extra 13','3️⃣','#666666',3,1,0),
    (14,'Extra 14','4️⃣','#777777',3,1,0);
INSERT INTO options(id,poll_id,label) VALUES
    (200,6,'Opzione 1'),(201,6,'Opzione 2'),(202,6,'Opzione 3'),(203,6,'Opzione 4'),(204,6,'Opzione 5'),(205,6,'Opzione 6'),
    (206,6,'Opzione 7'),(207,6,'Opzione 8'),(208,6,'Opzione 9'),(209,6,'Opzione 10'),(210,6,'Opzione 11'),(211,6,'Opzione 12');

-- Proposte della settimana 7 (e una della 6).
INSERT INTO suggestions(id,week,norm_name,name,tipo,link,status,review_status) VALUES
    (1,7,'amongus','Among Us','Gioco','https://www.youtube.com/watch?v=dQw4w9WgXcQ','picked','approved'),
    (2,7,'jackboxparty','Jackbox Party','Gioco','https://store.steampowered.com/app/945360/','picked','approved'),
    (3,7,'ilgladiatore','Il Gladiatore','Film','https://example.com/film','pending','approved'),
    (4,7,'karaokenight','Karaoke Night','Karaoke',NULL,'pending','approved'),
    (5,7,'bozza','Bozza','Altro',NULL,'pending','pending'),
    (6,7,'garticphone','Gartic Phone','Gioco','https://youtu.be/AAAAAAAAAAA','picked','approved'),
    (7,6,'vecchia','Vecchia','Gioco',NULL,'pending','approved');
INSERT INTO proposals(suggestion_id,user_id,week,ts) VALUES
    (1,1,7,1000),(1,2,7,1005),(2,3,7,1010),(3,4,7,1020),(4,5,7,1030),(5,6,7,1040),(6,7,7,1050),(7,1,6,500);
INSERT INTO suggestion_supports(suggestion_id,user_id,week,created_at) VALUES
    (1,10,7,1100),(1,11,7,1101),(2,12,7,1102),(6,13,7,1103),(3,10,7,1104),(4,11,7,1105),(4,12,7,1106);

-- Ballottaggio community (sondaggio 5) con i voti, più qualche voto negli altri sondaggi.
INSERT INTO options(id,poll_id,label,suggestion_id) VALUES
    (100,5,'Among Us',1),(101,5,'Jackbox Party',2),(102,5,'Gartic Phone',6),(103,5,'Libera',NULL);
INSERT INTO votes(option_id,user_id) VALUES
    (100,21),(100,22),(100,23),(101,21),(101,24),(102,22),
    (1,21),(1,22),(4,21),(200,30),(7,40);

-- Storico: tre settimane archiviate (formato scritto dal reset).
INSERT INTO history(week,closed_at,data) VALUES
    (4,'2026-10-19T12:00:00+02:00','{"polls": [{"id": 5, "name": "Proposte della community", "is_community": true, "options": [{"label": "Among Us", "suggestion_id": 1, "votes": 5}, {"label": "Jackbox", "suggestion_id": 2, "votes": 5}]}], "suggestions": [{}, {}, {}], "participants": 9}'),
    (5,'2026-10-26T12:00:00+01:00','{"polls": [{"id": 5, "name": "Proposte della community", "is_community": true, "options": [{"label": "Nessuno", "suggestion_id": 3, "votes": 0}]}], "suggestions": [], "participants": 0}'),
    (6,'2026-11-02T12:00:00+01:00','{"polls": [{"id": 1, "name": "Giochi", "is_community": false, "options": [{"label": "X", "suggestion_id": null, "votes": 99}]}, {"id": 5, "name": "Proposte della community", "is_community": true, "options": [{"label": "Zeta", "suggestion_id": 9, "votes": 4}, {"label": "Among Us", "suggestion_id": 1, "votes": 4}, {"label": "Alfa", "suggestion_id": 8, "votes": 9}, {"label": "beta", "suggestion_id": 7, "votes": 4}]}], "suggestions": [{}, {}], "participants": 20}');
INSERT INTO weekly_awards(week,user_ids,points,recorded_at) VALUES (6,'[11, 12]',9,'x'),(5,'[]',0,'x');
INSERT INTO weekly_streak_records(user_id,week,streak) VALUES (11,6,6),(12,6,4),(13,6,3),(14,6,3),(15,6,2),(16,6,2),(11,5,5);
INSERT INTO community_records(record_key,metric,label,week,user_id) VALUES
    ('most_wins',3,'<@12>',5,12),('alieno',1,'Boh',2,NULL),('most_supported',4,'Among Us',6,NULL),
    ('longest_streak',6,'<@11>',6,11),('most_voted',9,'Jackbox',5,NULL);

-- Statistiche utenti: ricco (1), quasi nuovo (2), al massimo livello (3) e altri per la classifica.
INSERT INTO user_stats(user_id,proposals_submitted,supports_given,supports_received,ballot_entries,winning_proposals) VALUES
    (1,12,11,10,3,1),(2,1,0,0,0,0),(3,30,0,50,10,10),
    (4,5,0,5,1,0),(5,5,0,5,1,0),(6,20,0,10,0,0),(7,9,0,0,0,1),(8,1,3,0,0,0),(9,15,0,0,0,0),(10,8,0,2,1,0),
    (11,3,0,1,1,0),(12,2,0,0,0,0),(13,40,0,0,0,0);
INSERT INTO user_streaks(user_id,current_streak,best_streak,last_active_week) VALUES (1,3,5,7),(2,0,2,5),(3,8,8,7);
INSERT INTO weekly_activity(user_id,week,activity_count,points) VALUES (1,7,2,3),(3,6,1,1);
INSERT INTO user_hidden_badges(user_id,badge_key,unlocked_at) VALUES
    (1,'night_owl','2026-11-02T03:00:00+01:00'),(1,'indecisive','2026-11-03T10:00:00+01:00'),
    (3,'night_owl','a'),(3,'indecisive','b'),(3,'influencer','c');
INSERT INTO user_winning_proposals(user_id,week,suggestion_id,name,votes,recorded_at) VALUES
    (1,6,1,'Among Us',9,'x'),(1,5,2,'Jackbox',5,'x'),(1,4,3,'Karaoke',5,'x'),
    (1,3,4,'Aaa',2,'x'),(1,2,5,'Bbb',2,'x'),(1,1,6,'Ccc',1,'x'),(3,6,1,'Among Us',9,'x');

-- Stagioni concluse con i loro campioni (la stagione corrente è già creata da `initialize`).
INSERT INTO seasons(season_key,label,starts_at,ends_at) VALUES
    ('inverno-2025','Inverno 2025/26','2025-12-01','2026-03-01'),
    ('primavera-2026','Primavera 2026','2026-03-01','2026-06-01'),
    ('estate-2026','Estate 2026','2026-06-01','2026-09-01');
INSERT INTO season_activity(season_key,user_id,week,activity_count,points) VALUES
    ('inverno-2025',1,1,2,20),('inverno-2025',2,1,1,5),
    ('primavera-2026',2,5,3,30),('primavera-2026',1,5,3,30),
    ('estate-2026',3,9,1,7),
    ('autunno-2026',1,7,2,12),('autunno-2026',3,7,1,4);
