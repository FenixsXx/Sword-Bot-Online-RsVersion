//! `cogs/proposals.py`: `/proposte` (lista paginata) e `/proposta revisiona|approva|rifiuta`.
//!
//! `ProposalPaginator` di Python è una vista che ricorda le pagine e l'indice per 5 minuti. Qui
//! lo stato sta in una mappa temporanea (le pagine sono quelle calcolate all'apertura, come in
//! Python) e i pulsanti portano nel `custom_id` solo il numero della lista.

use std::time::{Duration, Instant};

use poise::serenity_prelude::{ButtonStyle, CreateActionRow, CreateButton, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption};

use super::{InteractionIo, Interactions, Io, IoError, Reply};
use crate::cards::EmbedSpec;
use crate::config::{COLOR_STAFF, PAGE_SIZE};
use crate::discord_api::DiscordApi;
use crate::formatting::proposal_type_emoji;
use crate::notify::DiscordSender;
use crate::queries::{ListedSuggestion, suggestions_list};
use crate::review::{ReviewRequest, ReviewSource, apply_review};

/// Quanto vive una lista aperta (`timeout=300` della vista Python).
pub const PAGINATOR_TTL: Duration = Duration::from_secs(300);

const OWNER_ONLY: &str = "⚠️ Questa lista è riservata a chi l'ha aperta.";
const EXPIRED: &str = "⌛ Questa lista è scaduta: riapri /proposte.";
const SELECT_WINDOW: i64 = 25;

/// Una lista aperta: le pagine e quella mostrata.
#[derive(Debug, Clone)]
pub(super) struct Paginator {
    owner: i64,
    pages: Vec<EmbedSpec>,
    index: usize,
    created: Instant,
}

/// `plist:<lista>:prev|next|jump`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageComponent {
    Previous { nonce: u64 },
    Next { nonce: u64 },
    Jump { nonce: u64 },
}

impl PageComponent {
    pub fn parse(custom_id: &str) -> Option<Self> {
        let (nonce, action) = custom_id.strip_prefix("plist:")?.split_once(':')?;
        if nonce.is_empty() || !nonce.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let nonce = nonce.parse().ok()?;
        match action {
            "prev" => Some(Self::Previous { nonce }),
            "next" => Some(Self::Next { nonce }),
            "jump" => Some(Self::Jump { nonce }),
            _ => None,
        }
    }

    pub fn id(nonce: u64, action: &str) -> String {
        format!("plist:{nonce}:{action}")
    }
}

/// Le righe della lista in pagine da al massimo [`PAGE_SIZE`] voci e 4000 caratteri.
///
/// I link possono arrivare a 300 caratteri, e Discord ammette 4096 caratteri per descrizione.
pub fn proposal_pages(rows: &[ListedSuggestion]) -> Vec<EmbedSpec> {
    let lines: Vec<String> = rows
        .iter()
        .map(|row| {
            let mut line = format!(
                "`#{}` {} **{}**\n└ {} · `{} sostenitori` · `{}`",
                row.id,
                proposal_type_emoji(&row.tipo),
                row.name,
                row.tipo,
                row.n,
                row.review_status
            );
            if row.status.as_deref() == Some("picked") {
                line.push_str(" ✅");
            }
            if let Some(link) = row.link.as_deref().filter(|link| !link.is_empty()) {
                line.push_str(&format!(" · [link]({link})"));
            }
            line
        })
        .collect();
    let header = "──────────\n";
    let budget = 4000 - header.chars().count();
    let mut chunks: Vec<Vec<&String>> = vec![Vec::new()];
    for line in &lines {
        let current = chunks.last_mut().expect("c'è sempre almeno un gruppo");
        let size: usize = current.iter().map(|item| item.chars().count()).sum::<usize>() + 2 * current.len();
        if !current.is_empty() && (current.len() >= PAGE_SIZE || size + 2 + line.chars().count() > budget) {
            chunks.push(vec![line]);
        } else {
            current.push(line);
        }
    }
    let total = chunks.len();
    chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            let body = chunk.iter().map(|line| line.as_str()).collect::<Vec<_>>().join("\n\n");
            let description: String = format!("{header}{body}").chars().take(4096).collect();
            EmbedSpec::new("📋 Proposte della settimana", description, COLOR_STAFF)
                .footer(format!("Pagina {}/{total} · usa /scegli con l'ID", index + 1))
        })
        .collect()
}

/// Le opzioni del menu «Vai a una pagina»: una finestra di 25 attorno alla pagina mostrata.
fn jump_options(total: usize, index: usize) -> Vec<CreateSelectMenuOption> {
    let (total_i, index_i) = (total as i64, index as i64);
    let first = (index_i - 12).min(total_i - SELECT_WINDOW).max(0);
    let last = total_i.min(first + SELECT_WINDOW);
    (first..last)
        .map(|page| {
            CreateSelectMenuOption::new(format!("Pagina {}", page + 1), page.to_string())
                .description(format!("Vai alla pagina {} di {total}", page + 1))
                .default_selection(page == index_i)
        })
        .collect()
}

/// I componenti sotto una pagina: il menu (solo con più pagine) e le frecce.
pub fn paginator_rows(nonce: u64, total: usize, index: usize) -> Vec<CreateActionRow> {
    let mut rows = Vec::new();
    if total > 1 {
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(PageComponent::id(nonce, "jump"), CreateSelectMenuKind::String { options: jump_options(total, index) })
                .placeholder("Vai direttamente a una pagina")
                .min_values(1)
                .max_values(1),
        ));
    }
    rows.push(CreateActionRow::Buttons(vec![
        CreateButton::new(PageComponent::id(nonce, "prev")).label("◀").style(ButtonStyle::Secondary),
        CreateButton::new(PageComponent::id(nonce, "next")).label("▶").style(ButtonStyle::Secondary),
    ]));
    rows
}

impl<S: DiscordSender, A: DiscordApi> Interactions<S, A> {
    /// `/proposte`: le proposte della settimana, a pagine (solo staff).
    pub async fn command_proposals<I: InteractionIo>(&self, io: &I, tipo: Option<&str>) -> Io<()> {
        if !self.is_staff(io) {
            return io.respond(Reply::text("⚠️ Non hai i permessi per usare questo comando.")).await;
        }
        let rows = suggestions_list(self.db(), tipo.filter(|t| !t.is_empty())).map_err(|e| IoError(e.to_string()))?;
        if rows.is_empty() {
            return io.respond(Reply::text("⚠️ Nessuna proposta al momento.")).await;
        }
        let pages = proposal_pages(&rows);
        let first = pages[0].build();
        if pages.len() == 1 {
            return io.respond(Reply::embed(first)).await;
        }
        let total = pages.len();
        let nonce = self.next_nonce.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut map = self.paginators.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let ttl = self.paginator_ttl;
            map.retain(|_, entry| entry.created.elapsed() < ttl);
            map.insert(nonce, Paginator { owner: io.user_id(), pages, index: 0, created: Instant::now() });
        }
        io.respond(Reply::embed(first).with_components(paginator_rows(nonce, total, 0))).await
    }

    pub(super) async fn page_component<I: InteractionIo>(&self, io: &I, component: PageComponent) -> Io<()> {
        let (PageComponent::Previous { nonce } | PageComponent::Next { nonce } | PageComponent::Jump { nonce }) = component;
        let shown = {
            let mut map = self.paginators.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match map.get_mut(&nonce) {
                Some(entry) if entry.created.elapsed() >= self.paginator_ttl => {
                    map.remove(&nonce);
                    None
                }
                None => None,
                Some(entry) if entry.owner != io.user_id() => Some(Err(())),
                Some(entry) => {
                    let total = entry.pages.len();
                    match component {
                        PageComponent::Previous { .. } => entry.index = (entry.index + total - 1) % total,
                        PageComponent::Next { .. } => entry.index = (entry.index + 1) % total,
                        PageComponent::Jump { .. } => {
                            if let Some(page) = io.values().first().and_then(|value| value.parse::<usize>().ok()).filter(|page| *page < total) {
                                entry.index = page;
                            }
                        }
                    }
                    Some(Ok((entry.pages[entry.index].build(), total, entry.index)))
                }
            }
        };
        match shown {
            None => io.update(Reply { content: Some(EXPIRED.into()), embeds: Some(Vec::new()), components: Some(Vec::new()), v2: None }).await,
            Some(Err(())) => io.respond(Reply::text(OWNER_ONLY)).await,
            Some(Ok((embed, total, index))) => io.update(Reply::embed(embed).with_components(paginator_rows(nonce, total, index))).await,
        }
    }

    /// `/proposta revisiona|approva|rifiuta`.
    pub async fn command_review<I: InteractionIo>(&self, io: &I, suggestion_id: i64, action: &str, reason: Option<&str>) -> Io<()> {
        if !self.is_staff(io) {
            return io.respond(Reply::text("⚠️ Non hai i permessi per revisionare le proposte.")).await;
        }
        io.defer().await?;
        let outcome = apply_review(
            self.db(),
            &self.services.notifier,
            &self.effects(),
            ReviewRequest { actor_id: io.user_id(), suggestion_id, action, reason, source: ReviewSource::Discord },
        )
        .await;
        io.followup(Reply::text(outcome.message)).await
    }
}
