//! Controlli dei permessi staff/admin per comandi e componenti (equivalente di `sfo/permissions.py`).
//!
//! `admin_only` e `staff_only` sono i check di poise: si usano con
//! `#[poise::command(slash_command, check = "admin_only")]`. Un check che restituisce `false`
//! arriva all'error handler globale come `CommandCheckFailed` ("Permesso negato").
// Verranno usati dai cog di amministrazione e staff.
#![allow(dead_code)]

use std::collections::HashSet;

use tracing::warn;

use crate::{Context, Error};

/// ID dei ruoli staff salvati in `staff_roles` (elenco separato da virgole).
///
/// Le voci vuote sono ignorate. Una voce non numerica non può dare permessi a nessuno: viene
/// scartata con un avviso (in Python `int()` solleverebbe un errore e il comando fallirebbe).
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

/// Decisione pura: admin, oppure almeno uno dei ruoli del membro è un ruolo staff.
///
/// Il ruolo @everyone ha lo stesso ID del server: non deve mai dare permessi staff.
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

/// L'autore ha il permesso Amministratore nel server. In DM non ci sono permessi di server.
pub async fn is_admin(ctx: Context<'_>) -> bool {
    ctx.author_member()
        .await
        .and_then(|member| member.permissions)
        .is_some_and(|permissions| permissions.administrator())
}

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

/// Check di poise: solo amministratori.
pub async fn admin_only(ctx: Context<'_>) -> Result<bool, Error> {
    Ok(is_admin(ctx).await)
}

/// Check di poise: staff (ruoli configurati) o amministratori.
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
