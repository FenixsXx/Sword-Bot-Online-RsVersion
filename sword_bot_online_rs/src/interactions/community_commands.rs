//! I comandi pubblici di `cogs/community.py` e `cogs/seasons.py`: `/guida`, `/profilo`,
//! `/classifica-utenti`, `/record-community`, `/notifiche` e `/classifica-stagione`.

use rusqlite::OptionalExtension;

use super::{InteractionIo, Interactions, Io, IoError, Reply};
use crate::cards::{EmbedSpec, build_community_records_embed, build_contributor_leaderboard_embed, build_public_profile_embed};
use crate::community::{guide_embed, season_embed};
use crate::control_center::MemberInfo;
use crate::discord_api::DiscordApi;
use crate::notify::DiscordSender;
use crate::suggestions::set_dm_notifications;

fn db_error(error: rusqlite::Error) -> IoError {
    IoError(error.to_string())
}

impl<S: DiscordSender, A: DiscordApi> Interactions<S, A> {
    /// `/guida`: visibile a tutti. `public_url` è `EVENT_OVERLAY_PUBLIC_URL`.
    pub async fn command_guide<I: InteractionIo>(&self, io: &I, public_url: &str) -> Io<()> {
        let embed = guide_embed(self.db(), public_url).map_err(db_error)?;
        io.respond_public(Reply::embed(embed.build())).await
    }

    /// `/profilo`: il profilo di un membro (o di chi scrive). `None` se il membro non si recupera.
    pub async fn command_profile<I: InteractionIo>(&self, io: &I, member: Option<MemberInfo>) -> Io<()> {
        let Some(member) = member else {
            return io.respond(Reply::text("⚠️ Non riesco a recuperare il profilo del membro.")).await;
        };
        let embed = build_public_profile_embed(self.db(), member.id, &member.display_name, &member.avatar_url, self.now()).map_err(db_error)?;
        io.respond_public(Reply::embed(embed.build())).await
    }

    /// `/classifica-utenti`.
    pub async fn command_contributor_leaderboard<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let embed = build_contributor_leaderboard_embed(self.db(), 10).map_err(db_error)?;
        io.respond_public(Reply::embed(embed.build())).await
    }

    /// `/record-community`.
    pub async fn command_community_records<I: InteractionIo>(&self, io: &I) -> Io<()> {
        let embed = build_community_records_embed(self.db()).map_err(db_error)?;
        io.respond_public(Reply::embed(embed.build())).await
    }

    /// `/notifiche`: mostra o cambia la preferenza sui messaggi privati.
    pub async fn command_notifications<I: InteractionIo>(&self, io: &I, enabled: Option<bool>) -> Io<()> {
        let message = match enabled {
            None => {
                let current: Option<i64> = self
                    .db()
                    .with_conn(|c| c.query_row("SELECT dm_enabled FROM user_notification_preferences WHERE user_id=?", [io.user_id()], |r| r.get(0)).optional())
                    .map_err(db_error)?;
                if current.is_some_and(|value| value != 0) {
                    "Le notifiche private sono attive.".to_string()
                } else {
                    "Le notifiche private sono disattivate. Usa `/notifiche attive:true` per abilitarle.".to_string()
                }
            }
            Some(attive) => {
                set_dm_notifications(self.db(), io.user_id(), attive).map_err(db_error)?;
                if attive {
                    "✅ Notifiche private attivate: riceverai DM quando una tua proposta entra nel ballottaggio o vince, quando raggiunge 25 sostenitori e se diventi contributor della settimana.".to_string()
                } else {
                    "✅ Notifiche private disattivate.".to_string()
                }
            }
        };
        io.respond(Reply::text(message)).await
    }

    /// L'embed di `/classifica-stagione` (sincrono: `label` può guardare la cache dei membri).
    pub fn build_season(&self, label: &dyn Fn(i64) -> String) -> Result<EmbedSpec, IoError> {
        season_embed(self.db(), self.now(), label).map_err(db_error)
    }

    /// `/classifica-stagione`: visibile a tutti.
    pub async fn command_season<I: InteractionIo>(&self, io: &I, embed: EmbedSpec) -> Io<()> {
        io.respond_public(Reply::embed(embed.build())).await
    }
}
