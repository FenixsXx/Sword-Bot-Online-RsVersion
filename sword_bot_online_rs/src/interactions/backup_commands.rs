//! `/backup stato` e `/backup crea` (la parte dei comandi di `cogs/backups.py`).

use serde_json::json;
use tracing::error;

use super::{InteractionIo, Interactions, Io, Reply};
use crate::backups::{DAILY_RETENTION, WEEKLY_RETENTION};
use crate::discord_api::DiscordApi;
use crate::notify::{AuditEntry, DiscordSender};

impl<S: DiscordSender, A: DiscordApi> Interactions<S, A> {
    /// Collega il servizio dei backup (serve a `/backup`).
    pub fn set_backup_service(&self, service: std::sync::Arc<crate::backups::BackupService>) {
        let _ = self.backup.set(service);
    }

    /// `/backup stato`: quanti backup ci sono, l'ultimo di ciascun tipo e l'esito del cloud.
    pub async fn command_backup_status<I: InteractionIo>(&self, io: &I) -> Io<()> {
        if !io.is_admin() {
            return io.respond(Reply::text("⚠️ Solo un amministratore può consultare lo stato dei backup.")).await;
        }
        let Some(service) = self.backup.get() else {
            return io.respond(Reply::text("⚠️ Il servizio di backup non è attivo.")).await;
        };
        let status = service.status();
        let mut text = format!(
            "Backup giornalieri: `{}/{DAILY_RETENTION}` · ultimo `{}`\nBackup settimanali: `{}/{WEEKLY_RETENTION}` · ultimo `{}`\nDestinazione: `{}`",
            status.daily_count,
            status.latest_daily.as_deref().unwrap_or("nessuno"),
            status.weekly_count,
            status.latest_weekly.as_deref().unwrap_or("nessuno"),
            status.root.display(),
        );
        if let Some(message) = &status.cloud_upload_error {
            text.push_str(&format!("\n⚠️ Ultimo caricamento R2 non riuscito: {message}"));
        }
        io.respond(Reply::text(text)).await
    }

    /// `/backup crea`: crea subito i backup e li registra nell'audit.
    pub async fn command_backup_create<I: InteractionIo>(&self, io: &I) -> Io<()> {
        if !io.is_admin() {
            return io.respond(Reply::text("⚠️ Solo un amministratore può creare un backup.")).await;
        }
        let Some(service) = self.backup.get() else {
            return io.respond(Reply::text("⚠️ Il servizio di backup non è attivo.")).await;
        };
        io.defer().await?;
        let created = match service.create_backups(None).await {
            Ok(created) => created,
            Err(e) => {
                error!("Backup manuale locale/R2 richiesto da {} non riuscito: {e}", io.user_id());
                return io.followup(Reply::text("❌ Backup non riuscito; controlla i log del bot.")).await;
            }
        };
        let names: Vec<String> = created.iter().filter_map(|path| path.file_name()).map(|name| name.to_string_lossy().into_owned()).collect();
        let mut entry = AuditEntry::new(u64::try_from(io.user_id()).ok(), "database_backup_created").details(json!({"files": names}));
        entry.entity_type = Some("database".to_string());
        self.services.notifier.record_staff_audit(entry);
        let cloud_note = service
            .last_cloud_error()
            .map(|message| format!("\n⚠️ Copia cloud R2 non riuscita: {message}. I backup locali sono comunque validi."))
            .unwrap_or_default();
        let files = created.iter().map(|path| format!("`{}`", path.display())).collect::<Vec<_>>().join("\n");
        io.followup(Reply::text(format!("✅ Backup creato e verificato:\n{files}{cloud_note}"))).await
    }
}
