//! Settings → Codex history import.
//!
//! Discovery is bounded to the newest 100 primary rollouts on the selected
//! engine device. Import preserves the source thread id, so the imported chat
//! continues that Codex thread on its next turn.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, Context, Entity, EventEmitter, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};

use comet_proto::{CodexHistoryListing, CodexImportResult, CodexThreadSummary};
use comet_rpc::methods;

use crate::state::AppState;
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub enum CodexImportsEvent {
    OpenChat(String),
}

pub struct CodexImportsPage {
    state: Entity<AppState>,
    listing: Option<CodexHistoryListing>,
    loading: bool,
    busy_source: Option<String>,
    imported: HashMap<String, CodexImportResult>,
    error: Option<SharedString>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    _observe: Subscription,
}

impl EventEmitter<CodexImportsEvent> for CodexImportsPage {}

impl CodexImportsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            state,
            listing: None,
            loading: false,
            busy_source: None,
            imported: HashMap::new(),
            error: None,
            load_task: None,
            action_task: None,
            _observe: observe,
        };
        page.load(cx);
        page
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.loading = true;
        self.error = None;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::LIST_CODEX_THREADS,
                    serde_json::json!({"limit": 100}),
                )
                .await
                .and_then(|value| {
                    serde_json::from_value::<CodexHistoryListing>(value)
                        .map_err(|error| comet_rpc::RpcError::Failed(error.to_string()))
                });
            this.update(cx, |page, cx| {
                page.loading = false;
                match result {
                    Ok(listing) => page.listing = Some(listing),
                    Err(error) => {
                        page.error = Some(format!("Could not read Codex history: {error}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn import(&mut self, source_key: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy_source = Some(source_key.clone());
        self.error = None;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::IMPORT_CODEX_THREAD,
                    serde_json::json!({"sourceKey": source_key}),
                )
                .await
                .and_then(|value| {
                    serde_json::from_value::<CodexImportResult>(value)
                        .map_err(|error| comet_rpc::RpcError::Failed(error.to_string()))
                });
            this.update(cx, |page, cx| {
                page.busy_source = None;
                match result {
                    Ok(imported) => {
                        page.imported.insert(imported.source_key.clone(), imported);
                    }
                    Err(error) => page.error = Some(format!("Codex import failed: {error}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Render for CodexImportsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;

        let theme = Theme::of(cx).clone();
        let rows = self
            .listing
            .as_ref()
            .map(|listing| listing.threads.clone())
            .unwrap_or_default();
        let available = self
            .listing
            .as_ref()
            .is_some_and(|listing| listing.available);
        let root = self
            .listing
            .as_ref()
            .map(|listing| listing.root.clone())
            .unwrap_or_else(|| "$CODEX_HOME".into());
        let truncated = self
            .listing
            .as_ref()
            .is_some_and(|listing| listing.truncated);

        let items: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(index, thread)| {
                let source_key = thread.source_key.clone();
                let busy = self.busy_source.as_deref() == Some(source_key.as_str());
                let imported = self.imported.get(&source_key).cloned();
                let title: SharedString = thread_title(thread).into();
                let mut fragments = vec![
                    div()
                        .child(SharedString::from(thread.project.clone()))
                        .into_any_element(),
                    div()
                        .child(SharedString::from(format_bytes(thread.size_bytes)))
                        .into_any_element(),
                ];
                if thread.archived {
                    fragments.push(
                        div()
                            .child(SharedString::from("Archived"))
                            .into_any_element(),
                    );
                }
                if let Some(cwd) = &thread.cwd {
                    fragments.push(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(cwd.clone()))
                            .into_any_element(),
                    );
                }
                if let Some(modified) = DateTime::<Utc>::from_timestamp_millis(thread.modified_at) {
                    fragments.push(
                        div()
                            .child(SharedString::from(crate::state::format_time_ago(
                                modified,
                                Utc::now(),
                            )))
                            .into_any_element(),
                    );
                }

                let actions = if let Some(imported) = imported {
                    let open_chat = imported.chat_id.clone();
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(widgets::badge_active(&theme, "Imported"))
                        .child(
                            widgets::ghost_action(&theme)
                                .id(("open-codex-import", index))
                                .hover(|style| widgets::ghost_hover(&theme, style))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(CodexImportsEvent::OpenChat(open_chat.clone()));
                                }))
                                .child(SharedString::from("Open")),
                        )
                        .into_any_element()
                } else {
                    let import_key = source_key.clone();
                    widgets::ghost_action(&theme)
                        .id(("import-codex-thread", index))
                        .when(busy, |el| el.opacity(0.4))
                        .hover(|style| widgets::ghost_hover(&theme, style))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !busy {
                                this.import(import_key.clone(), cx);
                            }
                        }))
                        .child(SharedString::from(if busy {
                            "Importing…"
                        } else {
                            "Import"
                        }))
                        .into_any_element()
                };

                widgets::card_row(&theme, index == 0)
                    .child(widgets::row_tile(&theme, crate::icons::OPENAI_MARK))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(widgets::row_title(&theme, title))
                            .child(widgets::meta_line(&theme, fragments)),
                    )
                    .child(actions)
                    .into_any_element()
            })
            .collect();

        let body: AnyElement = if self.loading && self.listing.is_none() {
            div()
                .mt(px(64.0))
                .text_center()
                .text_size(px(13.0))
                .text_color(theme.text_muted)
                .child(SharedString::from("Reading recent Codex threads…"))
                .into_any_element()
        } else if !available {
            div()
                .mt(px(64.0))
                .text_center()
                .text_size(px(13.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(format!(
                    "No Codex sessions directory was found beneath {root}."
                )))
                .into_any_element()
        } else if items.is_empty() {
            div()
                .mt(px(64.0))
                .text_center()
                .text_size(px(13.0))
                .text_color(theme.text_muted)
                .child(SharedString::from("No primary Codex threads found."))
                .into_any_element()
        } else {
            widgets::section_card(&theme)
                .children(items)
                .into_any_element()
        };

        div()
            .id("codex-imports-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(
                        &theme,
                        "Codex history",
                        self.listing.as_ref().map(|listing| listing.threads.len()),
                    ))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Import a Codex thread into Zeron and continue it from where it left off. Rollout JSONLs are read-only during import.",
                    ))
                    .child(
                        div()
                            .mt(px(8.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(11.5))
                            .text_color(theme.text_muted.opacity(0.65))
                            .child(SharedString::from(root))
                            .when(truncated, |el| {
                                el.child(SharedString::from("· newest 100 shown"))
                            })
                            .child(
                                widgets::ghost_action(&theme)
                                    .id("refresh-codex-history")
                                    .when(self.loading, |el| el.opacity(0.4))
                                    .hover(|style| widgets::ghost_hover(&theme, style))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if !this.loading {
                                            this.load(cx);
                                        }
                                    }))
                                    .child(SharedString::from(if self.loading {
                                        "Refreshing…"
                                    } else {
                                        "Refresh"
                                    })),
                            ),
                    )
                    .when_some(self.error.clone(), |el, error| {
                        el.child(
                            widgets::error_strip(&theme, error)
                                .id("codex-import-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(body),
            )
    }
}

fn thread_title(thread: &CodexThreadSummary) -> String {
    thread.title.clone().unwrap_or_else(|| {
        let short: String = thread.session_id.chars().take(8).collect();
        format!("Codex thread {short}")
    })
}

fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else if bytes >= 1024 {
        format!("{:.0} KiB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_labels_are_compact() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(2048), "2 KiB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MiB");
    }
}
