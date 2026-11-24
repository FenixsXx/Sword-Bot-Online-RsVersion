//! Controlli dei permessi staff/admin per comandi e componenti (equivalente di `sfo/permissions.py`).
//!
//! Questo modulo gestisce chi può usare quali comandi. In Discord, i permessi
//! sono basati sui ruoli. Questo modulo fornisce due livelli:
//! - **Admin**: Utenti con il permesso "Amministratore" nel server
//! - **Staff**: Utenti con ruoli specifici configurati dal server
//!
//! # Come funziona
//!
//! I comandi in poise possono avere un "check" (controllo) che viene eseguito
//! prima del comando. Se il check fallisce, l'utente vede "Permesso negato".
//!
//! ```text
//! #[poise::command(slash_command, check = "staff_only")]
//! async fn solo_staff(ctx: Context<'_>) -> Result<(), Error> {
//!     // Solo chi passa il check arriva qui
//! }
//! ```
//!
//! # Ruoli staff
//!
//! I ruoli staff sono configurati nel server con il comando `/config`
//! e salvati nel database come stringa separata da virgole:
//! `"123456789,987654321"`
//!
//! # Differenza da Python
//!
//! In Python, i controlli sono più flessibili. In Rust con poise,
//! i check sono funzioni `async` che restituiscono `Result<bool, Error>`.
//!
//! # Verranno usati dai cog di amministrazione e staff.
#![allow(dead_code)]

use std::collections::HashSet;

use tracing::warn;

use crate::{Context, Error};

/// Converte una stringa di ID ruoli in un insieme di ID validi.
///
/// La stringa ha formato: `"123,456,789"` (separata da virgole).
/// Le voci vuote vengono ignorate.
/// Le voci non numeriche vengono scartate con un avviso (non fanno fallire il comando).
///
/// # Robustezza
///
/// In Python, `int("abc")` solleva un errore e il comando fallirebbe completamente.
/// In Rust, usiamo `filter_map` per ignorare silenziosamente gli errori,
/// loggando solo un avviso. Così un valore sporco nel database non blocca
/// tutto il sistema dei permessi.
///
/// # Esempio
///
/// ```text
/// Input: "1,,2, 3 ,abc,4"
/// Output: {1, 2, 3, 4}
/// Warning loggato: "ID ruolo staff non valido nelle impostazioni: \"abc\""
/// ```
pub fn parse_staff_roles(raw: &str) -> HashSet<u64> {
    raw.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .filter_map(|part| match part.parse::<u64>() {
            Ok(id) => Some(id),
            Err(_) => {
                warn!("ID ruolo staff non valido nelle impostazioni: {part:?}");
                None
            }
        })
        .collect()
}

/// Decide se un utente è staff, basandosi sui suoi ruoli e permessi admin.
///
/// Questa è la logica pura (senza I/O) che può essere testata isolatamente.
///
/// # Regole
///
/// 1. **Admin batte tutto**: Se `is_admin` è true, ritorna true immediatamente.
/// 2. **Ruoli staff**: Se l'utente ha almeno uno dei ruoli configurati, è staff.
/// 3. **@everyone non conta**: Il ruolo @everyone ha lo stesso ID del server
///    e viene rimosso automaticamente dalla lista staff.
///
/// # Parametri
///
/// - `is_admin`: Se l'utente ha permesso Amministratore
/// - `staff_roles_setting`: Stringa dal DB (es. `"123,456"`)
/// - `guild_id`: ID del server corrente (per filtrare @everyone)
/// - `member_roles`: Lista degli ID ruoli dell'utente
///
/// # Esempio
///
/// ```text
/// // Utente admin
/// is_staff_member(true, "123", Some(1), []) → true
///
/// // Utente con ruolo staff
/// is_staff_member(false, "123,456", Some(1), [123]) → true
///
/// // Utente senza ruoli staff
/// is_staff_member(false, "123,456", Some(1), [789]) → false
///
/// // @everyone non dà permessi
/// is_staff_member(false, "555", Some(555), [555]) → false
/// ```
pub fn is_staff_member(
    is_admin: bool,
    staff_roles_setting: &str,
    guild_id: Option<u64>,
    member_roles: impl IntoIterator<Item = u64>,
) -> bool {
    if is_admin {
        return true;
    }
    let mut staff_ids = parse_staff_roles(staff_roles_setting);
    if let Some(guild_id) = guild_id {
        staff_ids.remove(&guild_id);
    }
    member_roles.into_iter().any(|role| staff_ids.contains(&role))
}

/// Controlla se l'autore del comando è un amministratore del server.
///
/// In Discord, il permesso "Amministratore" è un permesso speciale che
/// bypassa tutti gli altri controlli. È l'unico modo per essere admin
/// senza dipendere da ruoli configurati.
///
/// # Comportamento nei DM
///
/// Nei messaggi diretti (DM), non c'è un "server", quindi non ci sono
/// permessi di server. La funzione restituisce `false`.
///
/// # Implementazione
///
/// Usa `ctx.author_member()` per ottenere il membro (se in un server),
/// poi controlla `member.permissions.administrator()`.
pub async fn is_admin(ctx: Context<'_>) -> bool {
    ctx.author_member()
        .await
        .and_then(|member| member.permissions)
        .is_some_and(|permissions| permissions.administrator())
}

/// Controlla se l'autore del comando è staff (admin o ruoli configurati).
///
/// Combina il controllo admin con il controllo ruoli staff.
/// Se è admin, ritorna true subito. Altrimenti controlla i ruoli.
///
/// # Parametri dal contesto
///
/// - `staff_roles`: Letto dal database (`settings::staff_roles`)
/// - `guild_id`: ID del server corrente
/// - `member.roles`: Ruoli dell'utente che ha eseguito il comando
pub async fn is_staff(ctx: Context<'_>) -> Result<bool, Error> {
    if is_admin(ctx).await {
        return Ok(true);
    }
    let setting = ctx.data().db.get_setting_or("staff_roles", "")?;
    let guild_id = ctx.guild_id().map(|id| id.get());
    let roles: Vec<u64> = match ctx.author_member().await {
        Some(member) => member.roles.iter().map(|role| role.get()).collect(),
        None => Vec::new(),
    };
    Ok(is_staff_member(false, &setting, guild_id, roles))
}

/// Check di poise per comandi solo-admin.
///
/// Si usa con `#[poise::command(slash_command, check = "admin_only")]`.
/// Se restituisce `false`, poise mostra automaticamente "Permesso negato".
///
/// # Esempio
///
/// ```text
/// #[poise::command(slash_command, check = "admin_only")]
/// async fn solo_admin(ctx: Context<'_>) -> Result<(), Error> {
///     // Solo gli admin arrivano qui
/// }
/// ```
pub async fn admin_only(ctx: Context<'_>) -> Result<bool, Error> {
    Ok(is_admin(ctx).await)
}

/// Check di poise per comandi solo-staff.
///
/// Si usa con `#[poise::command(slash_command, check = "staff_only")]`.
/// Accetta sia amministratori che utenti con ruoli staff configurati.
///
/// # Esempio
///
/// ```text
/// #[poise::command(slash_command, check = "staff_only")]
/// async fn solo_staff(ctx: Context<'_>) -> Result<(), Error> {
///     // Solo admin e staff arrivano qui
/// }
/// ```
pub async fn staff_only(ctx: Context<'_>) -> Result<bool, Error> {
    is_staff(ctx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everyone_role_never_grants_staff() {
        // Stesso caso di tests/test_weekly_cycle.py: il ruolo @everyone ha l'ID del server.
        let guild_id = 555;
        assert!(!is_staff_member(false, "555", Some(guild_id), [555]));
    }

    #[test]
    fn staff_role_grants_staff_and_others_do_not() {
        assert!(is_staff_member(false, "10,20", Some(1), [5, 20]));
        assert!(!is_staff_member(false, "10,20", Some(1), [5, 6]));
        assert!(!is_staff_member(false, "", Some(1), [5, 6]), "nessun ruolo staff configurato");
        assert!(!is_staff_member(false, "10", Some(1), []), "membro senza ruoli");
    }

    #[test]
    fn admins_are_always_staff() {
        assert!(is_staff_member(true, "", Some(1), []));
        assert!(is_staff_member(true, "garbage", None, []));
    }

    #[test]
    fn everyone_is_only_discarded_for_the_current_guild() {
        // In un altro server (o in DM) l'ID 555 è un ruolo qualunque.
        assert!(is_staff_member(false, "555", Some(1), [555]));
        assert!(is_staff_member(false, "555", None, [555]));
    }

    #[test]
    fn staff_roles_parsing_tolerates_blanks_and_garbage() {
        assert_eq!(parse_staff_roles(""), HashSet::new());
        assert_eq!(parse_staff_roles("1,,2, 3 ,"), HashSet::from([1, 2, 3]));
        assert_eq!(parse_staff_roles("1,abc,-4,2"), HashSet::from([1, 2]));
    }
}
