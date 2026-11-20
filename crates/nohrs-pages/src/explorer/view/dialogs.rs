//! Modal dialogs for explorer file operations: the permanent-delete confirmation
//! and the paste name-conflict resolver (`docs/explorer-essentials.md` §1.1,
//! §1.2). These render on `gpui_component`'s `ContextModal` layer (mounted in the
//! window root) and are driven entirely by the operation state held on
//! [`ExplorerPane`]; the filesystem logic lives in `super::super::file_ops`.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::WindowExt as _;
use gpui_component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::dialog::DialogButtonProps;
use gpui_component::progress::Progress;
use gpui_component::Disableable as _;
use nohrs_core::telemetry::LogErr as _;
use nohrs_core::{t, tf};
use nohrs_services::fs::ops::ConflictResolution;
use nohrs_ui::theme::theme;

use crate::explorer::ExplorerPane;
use crate::explorer::file_ops::file_name_of;

impl ExplorerPane {
    /// Opens a confirmation dialog before permanently deleting the selection
    /// (§1.1 — permanent delete is the one operation undo cannot reverse, so it
    /// is gated behind a confirm).
    pub(crate) fn request_permanent_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        let title = if count == 1 {
            let name =
                file_name_of(std::path::Path::new(&paths[0])).unwrap_or_else(|| paths[0].clone());
            tf!("Excluir “{name}” para sempre?", name = name)
        } else {
            tf!("Excluir {count} itens para sempre?", count = count)
        };
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let weak = weak.clone();
            let paths = paths.clone();
            dialog
                .confirm()
                .title(title.clone())
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(t!("Excluir"))
                        .ok_variant(ButtonVariant::Danger),
                )
                .child(t!("Isso não pode ser desfeito."))
                .on_ok(move |_, _window, cx| {
                    let paths = paths.clone();
                    weak.update(cx, |pane, cx| pane.delete_permanent_paths(paths, cx))
                        .log_err();
                    true
                })
        });
    }

    /// Opens the paste name-conflict dialog, resolving one collision at a time
    /// against the pane's [`super::super::file_ops::PastePlan`] (§1.2).
    pub(crate) fn open_conflict_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _window, cx| {
            let weak = weak.clone();
            let Some(pane) = weak.upgrade() else {
                return dialog;
            };
            let Some((name, dest_name, remaining, apply_to_all)) = pane.read_with(cx, |pane, _| {
                let plan = pane.paste_plan.as_ref()?;
                let current = plan.pending.front()?;
                let name = file_name_of(current).unwrap_or_default();
                let dest_name = file_name_of(&plan.dest_dir)
                    .unwrap_or_else(|| plan.dest_dir.display().to_string());
                Some((name, dest_name, plan.pending.len() - 1, plan.apply_to_all))
            }) else {
                return dialog;
            };

            let checkbox_weak = weak.clone();
            let close_weak = weak.clone();
            dialog
                .title(tf!(
                    "“{name}” já existe em “{folder}”",
                    name = name,
                    folder = dest_name
                ))
                // Dismissing without choosing has to abandon the plan exactly as
                // Cancel does, or the pending queue survives and the next paste
                // resumes into this one.
                .on_close(move |_, _window, cx| {
                    close_weak
                        .update(cx, |pane, cx| pane.cancel_paste(cx))
                        .log_err();
                })
                // Enter reaches this dialog as `Confirm`. With a custom footer
                // and no `on_ok`, gpui-component 0.5.1 takes its `else if
                // has_footer` branch and closes the dialog *without* running
                // `on_close` — leaking the plan the same way the close button
                // used to. Claiming `on_ok` puts Enter back on the path that
                // runs `on_close`, so it abandons the paste like Escape does
                // rather than silently picking one of the four outcomes.
                .on_ok(|_, _window, _cx| true)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(t!("Escolha como resolver o conflito de nomes."))
                        .when(remaining > 0, |this| {
                            this.child(
                                Checkbox::new("apply-to-all")
                                    .label(tf!(
                                        "Aplicar a todos os conflitos restantes (mais {remaining})",
                                        remaining = remaining
                                    ))
                                    .checked(apply_to_all)
                                    .on_click(move |checked, _window, cx| {
                                        checkbox_weak
                                            .update(cx, |pane, cx| {
                                                pane.set_apply_to_all(*checked, cx)
                                            })
                                            .log_err();
                                    }),
                            )
                        }),
                )
                // Cancel leads and Rename trails, keeping destructive Overwrite
                // out of the slot the eye reads as the default. None of the four
                // is styled primary: Enter abandons the paste (above), so
                // highlighting one would promise a keyboard default it does not
                // have — the four outcomes differ too much to guess between.
                .footer(move |_ok, _cancel, _window, _cx| {
                    vec![
                        cancel_button(&weak),
                        overwrite_button(&weak),
                        conflict_button(&weak, "skip", t!("Pular"), ConflictResolution::Skip),
                        conflict_button(
                            &weak,
                            "rename",
                            t!("Renomear"),
                            ConflictResolution::Rename,
                        ),
                    ]
                })
        });
    }
}

// Builds one of the conflict dialog's resolution buttons.
fn conflict_button(
    weak: &WeakEntity<ExplorerPane>,
    id: &'static str,
    label: &'static str,
    choice: ConflictResolution,
) -> Button {
    let weak = weak.clone();
    Button::new(id).label(label).on_click(move |_, window, cx| {
        let Some(pane) = weak.upgrade() else {
            return;
        };
        let done = pane.update(cx, |pane, cx| pane.resolve_current_conflict(choice, cx));
        if done {
            window.close_dialog(cx);
            pane.update(cx, |pane, cx| pane.execute_paste_plan(cx));
        }
    })
}

fn overwrite_button(weak: &WeakEntity<ExplorerPane>) -> Button {
    // Danger-coloured, and placed away from the trailing default slot (§1.2).
    conflict_button(
        weak,
        "overwrite",
        t!("Substituir"),
        ConflictResolution::Overwrite,
    )
    .danger()
}

fn cancel_button(weak: &WeakEntity<ExplorerPane>) -> Button {
    let weak = weak.clone();
    Button::new("cancel-paste")
        .label(t!("Cancelar"))
        .on_click(move |_, window, cx| {
            window.close_dialog(cx);
            weak.update(cx, |pane, cx| pane.cancel_paste(cx)).log_err();
        })
}

/// The progress dialog over the pane while a copy, move, delete, extract or
/// compress runs: what it is doing, how far, and a Cancel that the worker
/// answers at its next check.
pub(crate) fn transfer_dialog(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> Option<AnyElement> {
    let active = page.transfer.as_ref()?;
    let title = active.title;
    let progress = active.progress.clone();
    let snapshot = progress.snapshot();
    // The bar and the counters move without any input, so keep the window
    // drawing until the worker says it is done.
    window.request_animation_frame();

    let value = bar_value(&snapshot);
    let counts = counts_line(&snapshot);
    let detail = if snapshot.detail.is_empty() {
        "正在准备…".to_string()
    } else {
        snapshot.detail.clone()
    };
    let weak = cx.entity().downgrade();

    Some(
        div()
            .id("transfer-progress-dialog")
            .debug_selector(|| "transfer-progress-dialog".into())
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .bottom_0()
            // The pane under it must not take the clicks aimed at the dialog.
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba((theme::bg() << 8) | 0xB4))
            .child(
                div()
                    .w(px(440.0))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(rgb(theme::bg_secondary()))
                    .border_1()
                    .border_color(rgb(theme::border()))
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .when(snapshot.cancelled, |row| {
                                row.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::muted()))
                                        .child("正在取消…"),
                                )
                            }),
                    )
                    .child(
                        Progress::new()
                            .value(value)
                            .bg(Hsla::from(rgb(theme::accent()))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::fg_secondary()))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(detail),
                    )
                    .when(!counts.is_empty(), |card| {
                        card.child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::muted()))
                                .child(counts),
                        )
                    })
                    .child(
                        div()
                            .debug_selector(|| "transfer-cancel".into())
                            .flex()
                            .justify_end()
                            .child(
                                Button::new("transfer-cancel")
                                    .label(if snapshot.cancelled {
                                        "正在取消…"
                                    } else {
                                        "取消"
                                    })
                                    .disabled(snapshot.cancelled)
                                    .on_click(move |_, _, cx| {
                                        weak.update(cx, |pane, cx| pane.cancel_transfer(cx))
                                            .log_err();
                                    }),
                            ),
                    ),
            )
            .into_any_element(),
    )
}

// The bar's percentage: bytes when their total is known (a copy), the item
// count for a batch counted as it goes (a delete), and a moving bar while
// neither is known yet (measuring, or an archive tool that reports no
// numbers).
fn bar_value(snapshot: &nohrs_services::fs::transfer::TransferSnapshot) -> f32 {
    if snapshot.total > 0 {
        return (snapshot.done as f64 / snapshot.total as f64 * 100.0) as f32;
    }
    // A batch counted as it goes (a delete) moves by item; one item alone has
    // no total to show, so the bar runs until it is done.
    if snapshot.items_total > 1 {
        return (snapshot.items_done as f64 / snapshot.items_total as f64 * 100.0) as f32;
    }
    // Indeterminate: a band that crosses the bar and repeats.
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    let phase = (millis % 1400) as f32 / 1400.0;
    (phase * 100.0).clamp(0.0, 100.0)
}

// What the dialog says it has done: bytes for a copy, items and files for a
// delete, one or both while it is still measuring.
fn counts_line(snapshot: &nohrs_services::fs::transfer::TransferSnapshot) -> String {
    let mut parts = Vec::new();
    if snapshot.items_total > 1 {
        parts.push(format!(
            "{}/{} 项",
            snapshot.items_done, snapshot.items_total
        ));
    }
    if snapshot.files_done > 0 {
        parts.push(format!("已处理 {} 个文件", snapshot.files_done));
    }
    if snapshot.done > 0 {
        if snapshot.total > 0 {
            parts.push(format!(
                "{} / {}",
                nohrs_ui::components::file_list::human_bytes(snapshot.done),
                nohrs_ui::components::file_list::human_bytes(snapshot.total)
            ));
        } else {
            parts.push(nohrs_ui::components::file_list::human_bytes(snapshot.done));
        }
    }
    parts.join(" · ")
}
