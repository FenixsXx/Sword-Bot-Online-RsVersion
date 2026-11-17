//! Dal vincitore del sondaggio all'evento Discord programmato (equivalente di `sfo/gamenight.py`).
//!
//! Dopo il reset lo staff riceve nel canale staff il vincitore con il pulsante "📅 Crea evento": una
//! maschera chiede data e ora, il bot crea l'evento programmato nel canale vocale della Game Night e
//! lo abilita al tracciamento live automatico (stesso elenco usato da /evento programma).
//!
//! Differenza da Python: la maschera non ricorda il vincitore in memoria, che in Python vive nella
//! vista; qui il pulsante e la maschera portano la settimana nel `custom_id` e il vincitore si rilegge
//! da `week_winner:<settimana>`, così il pulsante funziona anche dopo un riavvio del bot.
// Collegato a `reset` (offerta) e a `interactions` (pulsante e maschera).
#![allow(dead_code)]

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use chrono_tz::Tz;
use poise::serenity_prelude::{ButtonStyle, CreateActionRow, CreateButton, CreateInputText, CreateModal, InputTextStyle, ReactionType};
use serde_json::{Value, json};
use tracing::{error, warn};

use crate::cards::EmbedSpec;
use crate::config::COLOR_PRESTIGE;
use crate::database::{Database, DbResult};
use crate::discord_api::{ChannelKind, DiscordApi, MessagePayload, NewScheduledEvent};
use crate::flows::Services;
use crate::formatting::{python_json, truncate_chars};
use crate::hooks::publish_site_change;
use crate::interactions::{InteractionIo, IoError, Reply};
use crate::notify::{AuditEntry, DiscordSender};
use crate::schedule::from_wall_clock;
use crate::stats::Winner;

/// Formato di data e ora accettato dalla maschera (`DATE_FORMAT` di Python).
pub const DATE_FORMAT: &str = "%d/%m/%Y %H:%M";
/// Durata prevista di una Game Night.
const EVENT_DURATION_HOURS: i64 = 2;

pub const CREATE_BUTTON_PREFIX: &str = "gamenight:create:";
pub const EVENT_MODAL_PREFIX: &str = "gamenight:modal:";
pub const MODAL_START_FIELD: &str = "start";
pub const MODAL_DESCRIPTION_FIELD: &str = "description";

type Io<T> = Result<T, IoError>;

// --- date ------------------------------------------------------------------------------------

/// Proposta predefinita: il prossimo venerdì alle 21:00.
///
/// Come in Python, i confronti e le somme di giorni si fanno sull'orologio da muro del fuso.
pub fn default_event_start(now: DateTime<Tz>) -> DateTime<Tz> {
    let local = now.naive_local();
    let days_ahead = (4 - i64::from(local.weekday().num_days_from_monday())).rem_euclid(7);
    let at_nine = NaiveTime::from_hms_opt(21, 0, 0).expect("ora valida");
    let mut candidate = (local.date() + Duration::days(days_ahead)).and_time(at_nine);
    if candidate <= local {
        candidate += Duration::days(7);
    }
    from_wall_clock(candidate)
}

/// Legge fino a due cifre ASCII all'inizio del testo: il valore e il resto.
fn take_number(text: &str, max_digits: usize) -> Option<(u32, &str)> {
    let digits = text.bytes().take(max_digits).take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    Some((text[..digits].parse().ok()?, &text[digits..]))
}

/// `GG/MM/AAAA HH:MM` come lo accetta `datetime.strptime` di Python: giorno, mese, ora e minuti
/// anche con una sola cifra, anno di quattro cifre, uno o più spazi tra data e ora.
fn parse_naive(text: &str) -> Option<NaiveDateTime> {
    let (day, rest) = take_number(text, 2)?;
    let rest = rest.strip_prefix('/')?;
    let (month, rest) = take_number(rest, 2)?;
    let rest = rest.strip_prefix('/')?;
    let year_digits = rest.bytes().take(4).take_while(u8::is_ascii_digit).count();
    if year_digits != 4 {
        return None;
    }
    let year: i32 = rest[..4].parse().ok()?;
    let rest = &rest[4..];
    let after_spaces = rest.trim_start_matches(char::is_whitespace);
    if after_spaces.len() == rest.len() {
        return None; // serve almeno uno spazio
    }
    let (hour, rest) = take_number(after_spaces, 2)?;
    let rest = rest.strip_prefix(':')?;
    let (minute, rest) = take_number(rest, 2)?;
    if !rest.is_empty() || !(1..=31).contains(&day) || !(1..=12).contains(&month) || hour > 23 || minute > 59 || year < 1 {
        return None;
    }
    NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)
}

/// L'inizio dell'evento: l'istante e l'orario scritto dallo staff.
///
/// Per un'ora che non esiste (le 02:30 della notte del cambio all'ora legale) Python tiene l'orario
/// scritto con l'offset di prima del salto, mentre l'istante, riletto nel fuso, direbbe le 03:30.
/// I testi (registro audit, log staff) usano l'orario scritto, come in Python.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventStart {
    wall: NaiveDateTime,
    instant: DateTime<Tz>,
}

impl EventStart {
    fn new(wall: NaiveDateTime) -> Self {
        Self { wall, instant: from_wall_clock(wall) }
    }

    pub fn instant(&self) -> DateTime<Tz> {
        self.instant
    }

    /// `GG/MM/AAAA HH:MM`.
    pub fn display(&self) -> String {
        self.wall.format(DATE_FORMAT).to_string()
    }

    /// Come `datetime.isoformat()` di Python: `2027-03-05T21:00:00+01:00`.
    pub fn isoformat(&self) -> String {
        let offset = (self.wall - self.instant.naive_utc()).num_minutes();
        let sign = if offset < 0 { '-' } else { '+' };
        format!("{}{sign}{:02}:{:02}", self.wall.format("%Y-%m-%dT%H:%M:%S"), offset.abs() / 60, offset.abs() % 60)
    }

    /// Fine dell'evento: `ore` ore dopo sull'orologio da muro (`start + timedelta(hours=...)`).
    pub fn end_instant(&self, hours: i64) -> DateTime<Tz> {
        from_wall_clock(self.wall + Duration::hours(hours))
    }
}

/// Data e ora scritte dallo staff: nel futuro (almeno tra 5 minuti) e non oltre 60 giorni.
///
/// Gli errori sono i messaggi mostrati all'utente.
pub fn parse_event_start(text: &str, now: DateTime<Tz>) -> Result<EventStart, String> {
    let Some(start) = parse_naive(text.trim()) else {
        return Err("Formato non valido: usa GG/MM/AAAA HH:MM, es. 21/11/2026 21:00.".to_string());
    };
    // Python confronta due orari con lo stesso fuso: vale l'orologio da muro.
    let local_now = now.naive_local();
    if start <= local_now + Duration::minutes(5) {
        return Err("La data deve essere nel futuro (almeno tra 5 minuti).".to_string());
    }
    if start > local_now + Duration::days(60) {
        return Err("La data è troppo lontana: massimo 60 giorni.".to_string());
    }
    Ok(EventStart::new(start))
}

// --- vincitore -------------------------------------------------------------------------------

/// Il vincitore ricordato per una settimana (`week_winner:<n>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWinner {
    pub suggestion_id: Option<i64>,
    pub name: String,
    pub votes: i64,
    pub tie_break: Option<String>,
}

/// Il vincitore di una settimana, se ricordato e leggibile.
pub fn winner_for_week(db: &Database, week: i64) -> DbResult<Option<StoredWinner>> {
    let Some(raw) = db.get_setting(&format!("week_winner:{week}"))?.filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let Ok(Value::Object(data)) = serde_json::from_str::<Value>(&raw) else {
        return Ok(None);
    };
    let (Some(name), Some(votes)) = (data.get("name").and_then(Value::as_str), data.get("votes").and_then(Value::as_i64)) else {
        return Ok(None);
    };
    Ok(Some(StoredWinner {
        suggestion_id: data.get("suggestion_id").and_then(Value::as_i64),
        name: name.to_string(),
        votes,
        tie_break: data.get("tie_break").and_then(Value::as_str).map(str::to_string),
    }))
}

/// Ricorda il vincitore della settimana (`week_winner:<n>`), usato per creare l'evento.
pub fn remember_winner(db: &Database, week: i64, winner: &Winner) -> DbResult<()> {
    let data = json!({
        "suggestion_id": winner.result.suggestion_id,
        "name": winner.result.name,
        "votes": winner.result.votes,
        "tie_break": winner.tie_break,
    });
    db.set_setting(&format!("week_winner:{week}"), python_json(&data))
}

/// Nome della proposta vincitrice collegata a un evento programmato (per i report).
pub fn event_proposal_name(db: &Database, scheduled_event_id: u64) -> DbResult<Option<String>> {
    db.get_setting(&format!("scheduled_event_proposal:{scheduled_event_id}"))
}

/// Aggiunge l'evento all'elenco di quelli con tracciamento live automatico.
fn opt_in_live_tracking(db: &Database, scheduled_event_id: u64) -> DbResult<()> {
    let mut values: Vec<u64> = db
        .get_setting_or("scheduled_event_optins", "[]")
        .ok()
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| serde_json::from_str::<Vec<u64>>(&raw).ok())
        .unwrap_or_default();
    values.push(scheduled_event_id);
    values.sort_unstable();
    values.dedup();
    db.set_setting("scheduled_event_optins", python_json(&json!(values)))
}

// --- messaggio per lo staff ------------------------------------------------------------------

pub fn create_button_id(week: i64) -> String {
    format!("{CREATE_BUTTON_PREFIX}{week}")
}

pub fn event_modal_id(week: i64) -> String {
    format!("{EVENT_MODAL_PREFIX}{week}")
}

fn parse_week(rest: &str) -> Option<i64> {
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())).then(|| rest.parse().ok()).flatten()
}

/// Settimana dal `custom_id` del pulsante "Crea evento".
pub fn parse_create_button(custom_id: &str) -> Option<i64> {
    parse_week(custom_id.strip_prefix(CREATE_BUTTON_PREFIX)?)
}

/// Settimana dal `custom_id` della maschera inviata.
pub fn parse_event_modal(custom_id: &str) -> Option<i64> {
    parse_week(custom_id.strip_prefix(EVENT_MODAL_PREFIX)?)
}

pub fn create_button(week: i64) -> CreateButton {
    CreateButton::new(create_button_id(week)).label("Crea evento").emoji(ReactionType::Unicode("📅".into())).style(ButtonStyle::Primary)
}

/// Il messaggio del vincitore per lo staff.
pub fn offer_embed(week: i64, name: &str, votes: i64, tie_break: Option<&str>) -> EmbedSpec {
    let title = truncate_chars(&format!("🏆 Vincitore della settimana #{week}: {name}"), 256);
    let mut description = format!("`{votes} voti`");
    if let Some(reason) = tie_break.filter(|reason| !reason.is_empty()) {
        description.push_str(&format!("\nSpareggio: {reason}."));
    }
    description.push_str("\n\nVuoi programmare la Game Night? Il bot creerà l'evento Discord e avvierà in automatico il tracciamento live.");
    EmbedSpec::new(title, description, COLOR_PRESTIGE)
}

/// Propone allo staff di trasformare il vincitore in un evento programmato.
///
/// Ricorda sempre il vincitore; il messaggio parte solo se il canale staff è configurato.
pub async fn offer_event_creation<A: DiscordApi>(db: &Database, api: &A, week: i64, winner: Option<&Winner>) {
    let Some(winner) = winner else {
        return;
    };
    if let Err(e) = remember_winner(db, week, winner) {
        error!("Impossibile ricordare il vincitore della settimana {week}: {e}");
    }
    let channel = db.get_setting("staff_channel").ok().flatten().and_then(|raw| raw.trim().parse::<u64>().ok()).filter(|id| *id != 0);
    let Some(channel) = channel else {
        return;
    };
    let embed = offer_embed(week, &winner.result.name, winner.result.votes, winner.tie_break).build();
    let payload = MessagePayload::embed(embed).with_components(vec![CreateActionRow::Buttons(vec![create_button(week)])]);
    if let Err(e) = api.send_message(channel, payload).await {
        error!("Impossibile proporre la creazione dell'evento allo staff nel canale {channel}: {e}");
    }
}

// --- maschera --------------------------------------------------------------------------------

/// La maschera con data e ora (proposta: il prossimo venerdì alle 21:00) e una descrizione facoltativa.
pub fn event_modal(week: i64, now: DateTime<Tz>) -> CreateModal {
    CreateModal::new(event_modal_id(week), "Crea l'evento della Game Night").components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Data e ora (GG/MM/AAAA HH:MM)", MODAL_START_FIELD)
                .min_length(16)
                .max_length(16)
                .value(default_event_start(now).format(DATE_FORMAT).to_string()),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Paragraph, "Descrizione (facoltativa)", MODAL_DESCRIPTION_FIELD)
                .required(false)
                .max_length(500),
        ),
    ])
}

/// Clic su "Crea evento": solo staff, una volta sola per vincitore, poi si apre la maschera.
pub async fn create_button_clicked<I: InteractionIo>(db: &Database, io: &I, week: i64, is_staff: bool, now: DateTime<Tz>) -> Io<()> {
    if !is_staff {
        return io.respond(Reply::text("⚠️ Solo lo staff può creare l'evento.")).await;
    }
    let already = db.get_setting(&format!("week_event_created:{week}")).ok().flatten().is_some_and(|value| !value.is_empty());
    if already {
        return io.respond(Reply::text("ℹ️ L'evento per questo vincitore è già stato creato.")).await;
    }
    match winner_for_week(db, week) {
        Ok(Some(_)) => io.show_modal(event_modal(week, now)).await,
        Ok(None) => io.respond(Reply::text("❌ Vincitore non trovato per questa settimana.")).await,
        Err(e) => {
            error!("Impossibile leggere il vincitore della settimana {week}: {e}");
            io.respond(Reply::text("❌ Vincitore non trovato per questa settimana.")).await
        }
    }
}

/// Maschera inviata: crea l'evento programmato nel canale vocale della Game Night.
pub async fn event_modal_submitted<S: DiscordSender, A: DiscordApi, I: InteractionIo>(
    services: &Services<S, A>,
    io: &I,
    week: i64,
    now: DateTime<Tz>,
) -> Io<()> {
    let db = &services.db;
    let start = match parse_event_start(&io.modal_field(MODAL_START_FIELD).unwrap_or_default(), now) {
        Ok(start) => start,
        Err(message) => return io.respond(Reply::text(format!("❌ {message}"))).await,
    };
    let Some(guild) = io.guild_id() else {
        return io.respond(Reply::text("❌ Usa questo bottone nel server.")).await;
    };
    let winner = match winner_for_week(db, week) {
        Ok(Some(winner)) => winner,
        _ => return io.respond(Reply::text("❌ Vincitore non trovato per questa settimana.")).await,
    };
    let channel = db.get_setting("game_night_channel").ok().flatten().and_then(|raw| raw.trim().parse::<u64>().ok());
    let kind = match channel {
        Some(id) => services.publisher.api().channel_kind(guild, id).await.unwrap_or_else(|e| {
            warn!("Canale della Game Night {id} non leggibile: {e}");
            ChannelKind::Other
        }),
        None => ChannelKind::Other,
    };
    let (Some(channel), ChannelKind::Voice | ChannelKind::Stage) = (channel, kind) else {
        return io
            .respond(Reply::text("⚠️ Configura prima il canale vocale della Game Night con `/config canale_game_night`."))
            .await;
    };
    io.defer().await?;

    let custom = io.modal_field(MODAL_DESCRIPTION_FIELD).unwrap_or_default();
    let description = match custom.trim() {
        "" => format!("La community ha scelto **{}** con {} voti nella settimana #{week}.", winner.name, winner.votes),
        text => text.to_string(),
    };
    let end = start.end_instant(EVENT_DURATION_HOURS);
    let event = NewScheduledEvent {
        name: truncate_chars(&format!("🎮 {}", winner.name), 100),
        start_unix: start.instant().timestamp(),
        end_unix: end.timestamp(),
        channel_id: channel,
        stage: kind == ChannelKind::Stage,
        description: truncate_chars(&description, 1000),
        reason: format!("Game Night creata da {}", io.user_name()),
    };
    let event_id = match services.publisher.api().create_scheduled_event(guild, event).await {
        Ok(id) => id,
        Err(e) => {
            error!("Creazione dell'evento Game Night non riuscita: {e}");
            return io
                .followup(Reply::text("❌ Discord non ha creato l'evento: controlla che il bot abbia il permesso «Gestisci eventi»."))
                .await;
        }
    };
    let url = format!("https://discord.com/events/{guild}/{event_id}");

    if let Err(e) = opt_in_live_tracking(db, event_id)
        .and_then(|()| db.set_setting(&format!("scheduled_event_proposal:{event_id}"), &winner.name))
        .and_then(|()| db.set_setting(&format!("week_event_created:{week}"), event_id))
    {
        error!("Evento {event_id} creato, ma salvarne i dati non è riuscito: {e}");
    }
    services.notifier.record_staff_audit(
        AuditEntry::new(u64::try_from(io.user_id()).ok(), "game_night_event_created")
            .entity("scheduled_event", event_id)
            .details(json!({"proposal": winner.name, "week": week, "start": start.isoformat()})),
    );
    publish_site_change("events");
    io.followup(Reply::text(format!("✅ Evento creato: {url}\nIl tracciamento live partirà in automatico."))).await?;

    let mention = format!("<@{}>", io.user_id());
    if let Err(e) = io.edit_source_message(format!("📅 Evento creato da {mention}: {url}")).await {
        error!("Impossibile aggiornare il messaggio del vincitore: {e}");
    }
    services.notifier.staff_log(format!("📅 {mention} ha creato l'evento «{}» per il {}.", winner.name, start.display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TZ;
    use crate::discord_api::testing::{Call, FakeApi};
    use crate::schedule::isoformat;
    use crate::stats::BallotResult;
    use chrono::TimeZone;

    fn rome(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Tz> {
        TZ.with_ymd_and_hms(year, month, day, hour, minute, 0).unwrap()
    }

    fn golden() -> Value {
        serde_json::from_str(include_str!("golden/gamenight_expected.json")).unwrap()
    }

    /// Esito come coppia (riuscito, testo), il formato dei golden.
    fn outcome(result: &Result<String, String>) -> (bool, &str) {
        match result {
            Ok(text) => (true, text),
            Err(text) => (false, text),
        }
    }

    #[test]
    fn default_start_matches_python() {
        for case in golden()["defaults"].as_array().unwrap() {
            let now = DateTime::parse_from_rfc3339(case[0].as_str().unwrap()).unwrap().with_timezone(&TZ);
            let start = default_event_start(now);
            assert_eq!(isoformat(start), case[1].as_str().unwrap(), "da {now}");
            assert_eq!(start.format(DATE_FORMAT).to_string(), case[2].as_str().unwrap());
        }
    }

    /// Stessi ingressi e stessi messaggi di errore di `parse_event_start` di Python, comprese le
    /// forme che `strptime` accetta (cifre singole, più spazi) e quelle che rifiuta.
    #[test]
    fn parsing_matches_python() {
        let golden = golden();
        let now = rome(2027, 2, 28, 18, 0);
        for case in golden["parsed"].as_array().unwrap() {
            let text = case[0].as_str().unwrap();
            let actual = parse_event_start(text, now).map(|start| start.isoformat());
            assert_eq!(outcome(&actual), (case[1].as_str().unwrap() == "ok", case[2].as_str().unwrap()), "«{text}»");
        }
        for (label, current, text) in [
            ("dst_sunday_morning", rome(2027, 3, 27, 23, 0), "28/03/2027 03:00"),
            ("dst_sunday_gap", rome(2027, 3, 27, 23, 0), "28/03/2027 02:30"),
            ("dst_5_minutes", rome(2027, 3, 28, 1, 56), "28/03/2027 03:00"),
            ("far_across_dst", rome(2027, 1, 28, 12, 0), "29/03/2027 12:00"),
            ("far_across_dst_edge", rome(2027, 1, 28, 12, 0), "29/03/2027 12:01"),
        ] {
            let expected = &golden[label];
            let actual = parse_event_start(text, current).map(|start| start.isoformat());
            assert_eq!(outcome(&actual), (expected[0].as_str().unwrap() == "ok", expected[1].as_str().unwrap()), "{label}");
        }
    }

    #[test]
    fn nonexistent_time_keeps_the_written_wall_clock() {
        let start = parse_event_start("28/03/2027 02:30", rome(2027, 3, 27, 23, 0)).unwrap();
        assert_eq!(start.isoformat(), "2027-03-28T02:30:00+01:00");
        assert_eq!(start.display(), "28/03/2027 02:30");
        // Stesso istante di Python (01:30 UTC); la fine è due ore dopo sull'orologio da muro (04:30 estive).
        assert_eq!(start.instant().naive_utc().to_string(), "2027-03-28 01:30:00");
        assert_eq!(start.end_instant(2).naive_utc().to_string(), "2027-03-28 02:30:00");
    }

    fn db_with_staff_channel() -> Database {
        let db = Database::open_in_memory().unwrap();
        crate::core::initialize(&db, rome(2027, 2, 28, 18, 0)).unwrap();
        db.set_setting("staff_channel", 600).unwrap();
        db
    }

    fn winner(id: i64, name: &str, votes: i64, tie_break: Option<&'static str>) -> Winner {
        Winner { result: BallotResult { suggestion_id: id, name: name.to_string(), votes, supporters: 0, first_ts: 0 }, tie_break }
    }

    /// Il messaggio per lo staff (titolo, testo, colore, pulsante) e il vincitore ricordato.
    #[tokio::test]
    async fn offer_matches_the_python_implementation() {
        let golden = golden();
        let db = db_with_staff_channel();
        let api = FakeApi::default();
        let cases: [(i64, Option<Winner>); 4] = [
            (7, Some(winner(1, "Among Us", 3, None))),
            (8, Some(winner(2, "Jackbox Party", 4, Some("più sostenitori")))),
            (9, Some(winner(3, &"N".repeat(300), 12, None))),
            (10, None),
        ];
        let mut expected = golden["offers"].as_array().unwrap().iter();
        for (week, winner) in &cases {
            let before = api.calls().len();
            offer_event_creation(&db, &api, *week, winner.as_ref()).await;
            let case = expected.next().unwrap();
            let sent: Vec<Call> = api.calls()[before..].to_vec();
            let wanted = case[1].as_array().unwrap();
            assert_eq!(sent.len(), wanted.len(), "settimana {week}");
            for (call, wanted) in sent.iter().zip(wanted) {
                let Call::Send { channel, message } = call else { panic!("atteso un invio") };
                assert_eq!(json!(channel), wanted["channel"]);
                let message = api.message(*channel, *message).unwrap();
                let embed = &message.embeds[0];
                assert_eq!(embed["title"], wanted["title"]);
                assert_eq!(embed["description"], wanted["description"]);
                assert_eq!(embed["color"], wanted["color"]);
                assert!(embed.get("footer").is_none() && embed.get("timestamp").is_none());
                let buttons: Vec<Value> = message
                    .components
                    .iter()
                    .flat_map(|row| row["components"].as_array().unwrap().clone())
                    .map(|b| json!([b["custom_id"], b["label"], b["emoji"]["name"], b["style"]]))
                    .collect();
                assert_eq!(json!(buttons), wanted["buttons"]);
            }
            assert_eq!(db.get_setting(&format!("week_winner:{week}")).unwrap().map_or(Value::Null, Value::from), case[2], "vincitore ricordato");
        }
        // Senza canale staff il vincitore si ricorda lo stesso, ma non parte nessun messaggio.
        db.execute("DELETE FROM settings WHERE key='staff_channel'", []).unwrap();
        let before = api.calls().len();
        offer_event_creation(&db, &api, 11, Some(&winner(4, "Karaoke", 2, None))).await;
        let case = expected.next().unwrap();
        assert_eq!(api.calls().len(), before);
        assert_eq!(db.get_setting("week_winner:11").unwrap().map(Value::from).unwrap(), case[2]);
        assert!(expected.next().is_none());
    }

    #[tokio::test]
    async fn offer_survives_a_failing_channel() {
        let db = db_with_staff_channel();
        let api = FakeApi::default();
        api.fail_send(Some(crate::discord_api::ApiError::Forbidden));
        offer_event_creation(&db, &api, 7, Some(&winner(1, "Among Us", 3, None))).await;
        assert!(db.get_setting("week_winner:7").unwrap().is_some(), "il vincitore si ricorda comunque");
    }

    #[test]
    fn stored_winner_is_read_defensively() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(winner_for_week(&db, 7).unwrap(), None);
        for garbage in ["", "non json", "[]", "null", "{}", r#"{"name": "X"}"#, r#"{"votes": 1}"#] {
            db.set_setting("week_winner:7", garbage).unwrap();
            assert_eq!(winner_for_week(&db, 7).unwrap(), None, "{garbage}");
        }
        db.set_setting("week_winner:7", r#"{"suggestion_id": 1, "name": "Among Us", "votes": 3, "tie_break": "x"}"#).unwrap();
        let stored = winner_for_week(&db, 7).unwrap().unwrap();
        assert_eq!((stored.name.as_str(), stored.votes, stored.tie_break.as_deref(), stored.suggestion_id), ("Among Us", 3, Some("x"), Some(1)));
    }

    #[test]
    fn live_tracking_list_stays_sorted_and_unique() {
        let db = Database::open_in_memory().unwrap();
        opt_in_live_tracking(&db, 30).unwrap();
        opt_in_live_tracking(&db, 10).unwrap();
        opt_in_live_tracking(&db, 30).unwrap();
        assert_eq!(db.get_setting("scheduled_event_optins").unwrap().as_deref(), Some("[10, 30]"));
        db.set_setting("scheduled_event_optins", "rotto").unwrap();
        opt_in_live_tracking(&db, 5).unwrap();
        assert_eq!(db.get_setting("scheduled_event_optins").unwrap().as_deref(), Some("[5]"));
    }
}
