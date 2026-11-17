-- Aggiunte allo stato di `cards_fixture.sql` per i test di parità del reset settimanale.
-- Ultimo reset: lunedì precedente; il reset da eseguire è quello di lunedì 2027-03-01 alle 12:00 (Roma).
UPDATE settings SET value='2027-02-22T12:00:00+01:00' WHERE key='last_reset';
INSERT OR IGNORE INTO settings(key,value) VALUES('last_reset','2027-02-22T12:00:00+01:00');
-- Storico già pieno (200 settimane): il più vecchio deve uscire.
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<200)
INSERT INTO history(week,closed_at,data) SELECT i-300, '2020-01-01T00:00:00+01:00', '{"old": ' || i || '}' FROM n;
-- Attività della community: una oltre i 400 giorni (da eliminare), una recente.
INSERT INTO community_activity(action,occurred_at) VALUES('old', 1000000000),('recent', 1772362800);
-- Sessioni del sito: una scaduta, una valida.
INSERT INTO web_sessions(session_id,user_id,display_name,created_at,expires_at) VALUES
    ('scaduta',1,'Ada','2027-01-01T10:00:00+01:00','2027-02-01T10:00:00+01:00'),
    ('valida',2,'Bruno','2027-02-28T10:00:00+01:00','2027-03-30T10:00:00+02:00');
