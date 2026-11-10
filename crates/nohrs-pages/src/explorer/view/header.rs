use nohrs_core::{k, t};
use nohrs_core::i18n;
use super::super::types::ViewMode;
use crate::explorer::ExplorerPane;
use gpui::prelude::*;
use gpui::*;
use gpui_component::breadcrumb::{Breadcrumb, BreadcrumbItem};
use gpui_component::Sizable;
use gpui_component::input::Input;
use gpui_component::list::ListItem;
use gpui_component::{Icon, IconName};
use nohrs_ui::theme::theme;

/// Renders the explorer header with navigation buttons, the breadcrumb path
/// bar, and view-mode controls.
pub fn render(
    page: &mut ExplorerPane,
    _window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    // This PC is an address, not a path: one crumb with its name. Under a
    // drive, a crumb for it comes first, as in Explorer, leading back to it.
    let location = page.location_name(cx);
    let parts = if location.is_some() { Vec::new() } else { path_parts(&page.cwd) };
    let under_this_pc = location.is_none() && starts_with_drive(&page.cwd);
    let this_pc_name = page
        .sidebar
        .places
        .as_ref()
        .and_then(|places| places.this_pc.as_ref())
        .map(|place| place.name.clone())
        .unwrap_or_else(|| i18n::tr(crate::explorer::places::THIS_PC_NAME).to_string());

    let (display_parts, is_truncated) = if parts.len() > 5 {
        (parts[(parts.len() - 5)..].to_vec(), true)
    } else {
        (parts.clone(), false)
    };

    let mut bc = Breadcrumb::new();
    // On a share, Network and the server come first, as in Explorer, each
    // leading back to its list.
    let unc_server = crate::explorer::network::server_of(&page.cwd).map(str::to_string);
    if unc_server.is_some() {
        bc = bc.child(
            BreadcrumbItem::new(i18n::tr(crate::explorer::network::NETWORK_NAME).to_string()).on_click(cx.listener(
                |this, _, window, cx| this.change_dir(crate::explorer::network::NETWORK.to_string(), window, cx),
            )),
        );
    }
    if let Some(name) = location {
        bc = bc.child(BreadcrumbItem::new(name));
    } else if under_this_pc {
        bc = bc.child(BreadcrumbItem::new(this_pc_name).on_click(cx.listener(|this, _, window, cx| {
            this.change_dir(crate::explorer::places::THIS_PC.to_string(), window, cx)
        })));
    } else if let Some(server) = unc_server.clone() {
        let address = format!("\\\\{server}");
        bc = bc.child(
            BreadcrumbItem::new(server).on_click(cx.listener(move |this, _, window, cx| this.change_dir(address.clone(), window, cx))),
        );
    }

    if is_truncated {
        bc = bc.child(BreadcrumbItem::new("…").on_click(cx.listener(move |_this, _, _, _| {})));
    }

    let start_idx = if is_truncated { parts.len() - 5 } else { 0 };

    for (display_i, p) in display_parts.iter().enumerate() {
        let actual_i = start_idx + display_i;
        let text = if p.is_empty() {
            String::from("/")
        } else if actual_i == 0 && unc_server.is_some() {
            // `\\nas\edicao\`: the server has its crumb already.
            p.trim_end_matches('\\').rsplit('\\').next().unwrap_or(p).to_string()
        } else {
            p.clone()
        };

        // `PathBuf::push` knows the root component already ends in a separator;
        // joining the strings by hand produced "//tmp" for every crumb under "/".
        let mut prefix = std::path::PathBuf::new();
        for (j, part) in parts.iter().enumerate() {
            prefix.push(part);
            if j >= actual_i {
                break;
            }
        }
        let mut path_here = prefix.to_string_lossy().to_string();
        if path_here.is_empty() {
            path_here = page.cwd.clone();
        }

        bc = bc.child(BreadcrumbItem::new(text).on_click(
            cx.listener(move |this, _, window, cx| this.change_dir(path_here.clone(), window, cx)),
        ));
    }

    let can_go_back = page.history_index > 0;
    let can_go_forward = page.history_index + 1 < page.history.len();
    let can_go_up = crate::explorer::navigation::parent_location(&page.cwd).is_some();

    // Store search_visible for use in search toggle style
    let preview_visible = page.preview_visible;
    let entry_count = page.filtered_entries.len();

    let framed = page.framed.header;
    div()
        .when(!framed, |this| this.bg(rgb(theme::bg())).border_b_1())
        .border_color(rgb(theme::border()))
        .flex()
        .items_center()
        .text_color(rgb(theme::fg()))
        .px(px(16.0))
        .py(px(10.0))
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .child(
                    ListItem::new("nav-back")
                        .px(px(8.0))
                        .py(px(6.0))
                        .rounded(px(6.0))
                        .when(!can_go_back, |this| this.opacity(0.35))
                        .when(can_go_back, |this| {
                            this.on_click(
                                cx.listener(|view, _, window, cx| view.go_back(window, cx)),
                            )
                        })
                        .child(
                            Icon::new(IconName::ArrowLeft)
                                .size_4()
                                .text_color(rgb(theme::gray_600())),
                        ),
                )
                .child(
                    ListItem::new("nav-forward")
                        .px(px(8.0))
                        .py(px(6.0))
                        .rounded(px(6.0))
                        .when(!can_go_forward, |this| this.opacity(0.35))
                        .when(can_go_forward, |this| {
                            this.on_click(
                                cx.listener(|view, _, window, cx| view.go_forward(window, cx)),
                            )
                        })
                        .child(
                            Icon::new(IconName::ArrowRight)
                                .size_4()
                                .text_color(rgb(theme::gray_600())),
                        ),
                )
                // Up and refresh beside back and forward, in Explorer's order.
                .child(
                    ListItem::new("nav-up")
                        .px(px(8.0))
                        .py(px(6.0))
                        .rounded(px(6.0))
                        .when(!can_go_up, |this| this.opacity(0.35))
                        .when(can_go_up, |this| {
                            this.on_click(cx.listener(|view, _, window, cx| view.go_up(window, cx)))
                        })
                        .child(
                            Icon::new(Icon::empty())
                                .path("icons/arrow-up.svg")
                                .size_4()
                                .text_color(rgb(theme::gray_600())),
                        ),
                )
                .child(
                    ListItem::new("nav-refresh")
                        .px(px(8.0))
                        .py(px(6.0))
                        .rounded(px(6.0))
                        .on_click(cx.listener(|view, _, _, cx| view.reload(cx)))
                        .child(
                            Icon::new(Icon::empty())
                                .path("icons/refresh-cw.svg")
                                .size_4()
                                .text_color(rgb(theme::gray_600())),
                        ),
                )
                .child(
                    div()
                        .w(px(1.0))
                        .h(px(20.0))
                        .bg(rgb(theme::border()))
                        .mx(px(6.0)),
                ),
        )
        .child(match &page.path_edit {
            Some(state) => div()
                .flex_1()
                .min_w(px(0.0))
                .child(Input::new(&state.input).small().text_sm())
                .into_any_element(),
            // A double click anywhere on the bar, crumbs included, turns it
            // into the full path; a single click on a crumb still navigates.
            None => div()
                .id("path-bar")
                .flex_1()
                .h(px(28.0))
                .flex()
                .items_center()
                .overflow_hidden()
                .min_w(px(0.0))
                .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                    if event.click_count() >= 2 {
                        this.begin_path_edit(window, cx);
                    }
                }))
                .child(bc)
                .into_any_element(),
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::fg_secondary()))
                        .whitespace_nowrap()
                        .mr(px(4.0))
                        .child(i18n::plural(entry_count, k!("{n} item"), k!("{n} itens"))),
                )
                .child(render_recent(page, cx))
                .child(render_open_in(page, cx))
                .child(render_view_mode_toggle(page, cx))
                .child(
                    // The preview switch sits up here, beside the view modes,
                    // where it is seen: the preview is one of the explorer's
                    // most used parts. The filter menu went to the bottom bar.
                    ListItem::new("preview-toggle")
                        .px(px(8.0))
                        .py(px(6.0))
                        .rounded(px(6.0))
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.preview_visible = !view.preview_visible;
                            cx.notify();
                        }))
                        .child(
                            Icon::new(Icon::empty())
                                .path("icons/panel-right.svg")
                                .size_4()
                                .text_color(if preview_visible {
                                    rgb(theme::accent())
                                } else {
                                    rgb(theme::gray_600())
                                }),
                        ),
                ),
        )
}

// "Recent folders", beside the button that opens the folder elsewhere: the
// places visited last, shown on hover and listed by a click (or Alt+Down). The
// arrow says the list is there; the menu says where each one goes.
fn render_recent(page: &mut ExplorerPane, cx: &mut Context<ExplorerPane>) -> impl IntoElement + use<> {
    use nohrs_core::shortcuts::{Action, display};
    let recent = super::super::recent::RecentFolders::all(cx);
    let open = page.recent_menu;
    // What the tooltip shows: the few newest, which is what one reaches for.
    let preview: String = recent
        .iter()
        .take(6)
        .map(|folder| folder.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let tooltip = if preview.is_empty() {
        t!("Nenhuma pasta visitada ainda").to_string()
    } else {
        format!("{}\n\n{preview}", t!("Pastas recentes"))
    };
    let shortcut = display(Action::RecentFolders);

    div()
        .relative()
        .flex_shrink_0()
        .child(
            div()
                .id("recent-folders")
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .when(open, |style| style.bg(rgb(theme::bg_hover())))
                .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx))
                .on_click(cx.listener(|page, _, _, cx| {
                    page.recent_menu = !page.recent_menu;
                    page.open_in_menu = false;
                    cx.notify();
                }))
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size_4()
                        .text_color(rgb(if open { theme::accent() } else { theme::gray_600() })),
                ),
        )
        .when(open, |this| {
            this.child(deferred(
                div()
                    .id("recent-folders-menu")
                    .absolute()
                    .top(px(34.0))
                    .right_0()
                    .min_w(px(300.0))
                    .max_w(px(520.0))
                    .p(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(rgb(theme::border()))
                    .bg(rgb(theme::bg_secondary()))
                    .shadow_xl()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|page, _, _, cx| {
                        page.recent_menu = false;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px(px(8.0))
                            .pt(px(4.0))
                            .pb(px(2.0))
                            .when_some(shortcut.clone(), |this, keys| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::muted()))
                                        .child(t!("Pastas recentes")),
                                )
                                .child(div().text_xs().text_color(rgb(theme::muted())).child(keys))
                            })
                            .when(shortcut.is_none(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::muted()))
                                        .child(t!("Pastas recentes")),
                                )
                            }),
                    )
                    .when(recent.is_empty(), |this| {
                        this.child(
                            div()
                                .px(px(8.0))
                                .py(px(6.0))
                                .text_sm()
                                .text_color(rgb(theme::muted()))
                                .child(t!("Nenhuma pasta visitada ainda")),
                        )
                    })
                    .children(recent.into_iter().enumerate().map(|(index, folder)| {
                        let name = folder
                            .trim_end_matches(['\\', '/'])
                            .rsplit(['\\', '/'])
                            .next()
                            .filter(|name| !name.is_empty())
                            .unwrap_or(folder.as_str())
                            .to_string();
                        let target = folder.clone();
                        div()
                            .id(SharedString::from(format!("recent-{index}")))
                            .h(px(30.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .text_sm()
                            .hover(|style| style.bg(rgb(theme::bg_hover())))
                            .on_click(cx.listener(move |page, _, window, cx| {
                                page.recent_menu = false;
                                page.change_dir(target.clone(), window, cx);
                            }))
                            .child(
                                Icon::new(IconName::Folder)
                                    .size_4()
                                    .flex_shrink_0()
                                    .text_color(rgb(theme::muted())),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .max_w(px(160.0))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_color(rgb(theme::fg()))
                                    .child(name),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_xs()
                                    .text_color(rgb(theme::muted()))
                                    .child(folder),
                            )
                    })),
            ))
        })
}

// "Open in": the folder on screen in the Command Prompt, PowerShell,
// Windows Terminal (when installed) or Windows' own Explorer, each with the
// shortcut it was given in Settings › Shortcuts.
fn render_open_in(page: &mut ExplorerPane, cx: &mut Context<ExplorerPane>) -> impl IntoElement + use<> {
    use crate::explorer::commands::{OpenIn, has_windows_terminal};
    use nohrs_core::shortcuts::{Action, display};
    let open = page.open_in_menu;
    let mut items: Vec<(OpenIn, &'static str, &'static str, Option<String>)> = vec![
        (OpenIn::CommandPrompt, "icons/square-terminal.svg", t!("Prompt de Comando"), display(Action::OpenCommandPrompt)),
        (OpenIn::PowerShell, "icons/square-terminal.svg", "PowerShell", display(Action::OpenPowerShell)),
    ];
    if has_windows_terminal() {
        items.push((OpenIn::WindowsTerminal, "icons/app-terminal.svg", "Windows Terminal", None));
    }
    items.push((OpenIn::FileExplorer, "icons/folder-open.svg", t!("Explorador do Windows"), display(Action::OpenFileExplorer)));
    div()
        .relative()
        .flex_shrink_0()
        .child(
            div()
                .id("open-in")
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .when(open, |style| style.bg(rgb(theme::bg_hover())))
                .tooltip(|window, cx| {
                    gpui_component::tooltip::Tooltip::new(t!("Abrir a pasta em outro programa")).build(window, cx)
                })
                .on_click(cx.listener(|page, _, _, cx| {
                    page.open_in_menu = !page.open_in_menu;
                    cx.notify();
                }))
                .child(
                    Icon::new(Icon::empty())
                        .path("icons/external-link.svg")
                        .size_4()
                        .text_color(rgb(if open { theme::accent() } else { theme::gray_600() })),
                ),
        )
        .when(open, |this| {
            this.child(deferred(
                div()
                    .id("open-in-menu")
                    .absolute()
                    .top(px(34.0))
                    .right_0()
                    .min_w(px(240.0))
                    .p(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(rgb(theme::border()))
                    .bg(rgb(theme::bg_secondary()))
                    .shadow_xl()
                    .occlude()
                    .on_mouse_down_out(cx.listener(|page, _, _, cx| {
                        page.open_in_menu = false;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .px(px(8.0))
                            .pt(px(4.0))
                            .pb(px(2.0))
                            .text_xs()
                            .text_color(rgb(theme::muted()))
                            .child(t!("Abrir esta pasta no")),
                    )
                    .children(items.into_iter().map(|(program, icon, label, keys)| {
                        div()
                            .id(SharedString::from(format!("open-in-{program:?}")))
                            .h(px(30.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .text_sm()
                            .text_color(rgb(theme::fg()))
                            .hover(|style| style.bg(rgb(theme::bg_hover())))
                            .on_click(cx.listener(move |page, _, _, cx| page.open_in(program, cx)))
                            .child(Icon::new(Icon::empty()).path(icon).size_4().text_color(rgb(theme::fg_secondary())))
                            .child(div().flex_1().child(label))
                            .children(keys.map(|keys| div().text_xs().text_color(rgb(theme::muted())).child(keys)))
                    })),
            ))
        })
}

fn render_view_mode_toggle(
    page: &mut ExplorerPane,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .items_center()
        .gap_1()
        .p(px(2.0))
        .rounded(px(8.0))
        .bg(rgb(theme::bg_secondary()))
        .border_1()
        .border_color(rgb(theme::border()))
        // `list.svg` ships with this crate but has no `IconName` variant, so it
        // is loaded by asset path.
        .child(view_mode_button(
            page,
            ViewMode::List,
            "view-mode-list",
            Icon::new(Icon::empty()).path(SharedString::from("icons/list.svg")),
            t!("Lista"),
            cx,
        ))
        .child(view_mode_button(
            page,
            ViewMode::Grid,
            "view-mode-grid",
            Icon::new(IconName::LayoutDashboard),
            t!("Grade"),
            cx,
        ))
}

fn view_mode_button(
    page: &mut ExplorerPane,
    mode: ViewMode,
    id: &'static str,
    icon: Icon,
    label: &'static str,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    // Tree and List are views of the list side, so its button stays lit.
    let is_active = (page.view_mode == ViewMode::Grid) == (mode == ViewMode::Grid);
    // The active segment lifts onto white so its accent icon reads against the
    // group's tinted track, rather than accent-on-gray.
    ListItem::new(id)
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .when(is_active, |this| this.bg(rgb(theme::bg())).shadow_sm())
        .on_click(cx.listener(move |this, _, _, cx| this.set_view_mode(mode, cx)))
        .child(
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .child(icon.size_4().text_color(if is_active {
                    rgb(theme::accent())
                } else {
                    rgb(theme::gray_500())
                }))
                .child(
                    div()
                        .text_xs()
                        .when(is_active, |this| this.font_weight(gpui::FontWeight::MEDIUM))
                        .text_color(if is_active {
                            rgb(theme::fg())
                        } else {
                            rgb(theme::fg_secondary())
                        })
                        .child(label),
                ),
        )
}

// `C:\...`: a path on a lettered drive, which Explorer shows under This PC.
fn starts_with_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn path_parts(path: &str) -> Vec<String> {
    use std::path::Component;
    let mut parts: Vec<String> = Vec::new();
    let mut after_prefix = false;
    for c in std::path::Path::new(path).components() {
        match (c, parts.last_mut()) {
            // Windows splits `C:\` (and `\\server\share\`) into a prefix and a
            // `\` root. Shown apart they are two crumbs where the user sees one
            // drive, and the first navigates to `C:`, which Windows reads as the
            // current directory on that drive rather than its root.
            (Component::RootDir, Some(prefix)) if after_prefix => {
                prefix.push(std::path::MAIN_SEPARATOR);
            }
            _ => parts.push(c.as_os_str().to_string_lossy().to_string()),
        }
        after_prefix = matches!(c, Component::Prefix(_));
    }
    if parts.is_empty() {
        parts.push(path.to_string());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::path_parts;

    #[cfg(windows)]
    #[test]
    fn drive_and_root_are_one_crumb() {
        assert_eq!(path_parts(r"C:\Users\ada"), ["C:\\", "Users", "ada"]);
        assert_eq!(path_parts(r"C:\"), ["C:\\"]);
        assert_eq!(
            path_parts(r"\\server\share\docs"),
            [r"\\server\share\", "docs"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn up_goes_to_the_parent_and_from_a_drive_to_this_pc() {
        use crate::explorer::navigation::parent_location;
        use crate::explorer::places::THIS_PC;
        assert_eq!(parent_location(r"C:\Users\ada").as_deref(), Some(r"C:\Users"));
        assert_eq!(parent_location(r"C:\Users").as_deref(), Some(r"C:\"));
        assert_eq!(parent_location(r"C:\").as_deref(), Some(THIS_PC));
        assert_eq!(parent_location(r"d:").as_deref(), Some(THIS_PC));
        assert_eq!(parent_location(THIS_PC), None);
    }

    #[cfg(unix)]
    #[test]
    fn root_stays_its_own_crumb() {
        assert_eq!(path_parts("/tmp/ada"), ["/", "tmp", "ada"]);
    }
}
