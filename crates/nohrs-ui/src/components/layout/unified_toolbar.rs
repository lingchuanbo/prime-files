use nohrs_core::t;
use crate::theme::theme;
use gpui::{
    Action, Context, IntoElement, Pixels, Render, Window, WindowControlArea, div, prelude::*, px,
    rgb,
};
use gpui_component::{
    Icon, IconName, Sizable, Size,
    button::{Button, ButtonRounded, ButtonVariant, ButtonVariants},
    menu::DropdownMenu,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// Fixed height of the unified toolbar.
pub const UNIFIED_TOOLBAR_HEIGHT: Pixels = px(36.0);
const ACCOUNT_BUTTON_ID: &str = "unified-toolbar-account-button";

/// Build the unified toolbar element: a draggable window region plus the
/// settings button with its menu.
pub fn unified_toolbar<V: Render>(
    window: &Window,
    app_name: &str,
    _cx: &mut Context<V>,
) -> impl IntoElement + use<V> {
    let drag_region = div()
        .id("unified-toolbar-drag-region")
        .flex_1()
        .h_full()
        .flex()
        .items_center()
        .window_control_area(WindowControlArea::Drag)
        .child(
            div()
                .px(px(8.0))
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(theme::fg()))
                .child(app_name.to_string()),
        );

    div()
        .id("unified-toolbar")
        .h(UNIFIED_TOOLBAR_HEIGHT)
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .pl(px(16.0))
        .bg(rgb(theme::bg()))
        .border_b_1()
        .border_color(rgb(theme::border()))
        .child(drag_region)
        .child(title_bar_controls(window))
}

/// The right end of the title bar: the settings button with its menu and,
/// on Windows, minimize, maximize and close. Drawn by the toolbar above and,
/// when the explorer has one pane, at the end of its tab row, which then is
/// the title bar, as in the Explorer of Windows 11.
pub fn title_bar_controls(window: &Window) -> impl IntoElement + use<> {
    let account_button = Button::new(ACCOUNT_BUTTON_ID)
        .icon(
            Icon::new(IconName::Settings)
                .size_4()
                .text_color(rgb(theme::fg_secondary())),
        )
        .rounded(ButtonRounded::Large)
        .compact()
        .with_variant(ButtonVariant::Ghost)
        .with_size(Size::Small)
        .dropdown_menu(move |menu, _window, _cx| {
            menu.min_w(px(200.0))
                .menu_with_icon(
                    t!("Configurações"),
                    Icon::new(IconName::Settings),
                    AccountMenuAction::boxed(AccountMenuCommand::Settings),
                )
                .menu_with_icon(
                    t!("Atalhos"),
                    Icon::new(IconName::CaseSensitive),
                    AccountMenuAction::boxed(AccountMenuCommand::Keymap),
                )
                .menu_with_icon(
                    t!("Temas"),
                    Icon::new(IconName::Palette),
                    AccountMenuAction::boxed(AccountMenuCommand::Themes),
                )
        });

    div()
        .flex_shrink_0()
        .h_full()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .when(!cfg!(target_os = "windows"), |controls| controls.pr(px(16.0)))
        .child(account_button)
        .when(cfg!(target_os = "windows"), |controls| {
            controls.child(windows_caption_buttons(window.is_maximized()))
        })
}

const CAPTION_BUTTON_WIDTH: Pixels = px(46.0);
const CAPTION_CLOSE_HOVER: u32 = 0xC42B1C;

// `appears_transparent` hides the native caption on Windows as well, but there
// is no counterpart to the macOS traffic lights drawn over it: without these the
// window has no way to be minimized, maximized or closed with the mouse. Marking
// each one as a `WindowControlArea` makes gpui answer `WM_NCHITTEST` with
// `HTMINBUTTON`/`HTMAXBUTTON`/`HTCLOSE`, so the clicks, snap layouts and
// tooltips are the system's own rather than reimplemented here.
fn windows_caption_buttons(maximized: bool) -> impl IntoElement {
    // Glyphs from Segoe Fluent Icons, which ships with Windows 11. Windows 10
    // has the same code points in Segoe MDL2 Assets instead.
    let caption_button = |id: &'static str, glyph: &'static str, area: WindowControlArea| {
        div()
            .id(id)
            .w(CAPTION_BUTTON_WIDTH)
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .font_family("Segoe Fluent Icons")
            .text_size(px(10.0))
            .text_color(rgb(theme::fg()))
            .window_control_area(area)
            .child(glyph)
    };

    div()
        .flex()
        .flex_row()
        .h_full()
        .child(
            caption_button("caption-minimize", "\u{e921}", WindowControlArea::Min)
                .hover(|style| style.bg(rgb(theme::bg_hover()))),
        )
        .child(
            caption_button(
                "caption-maximize",
                if maximized { "\u{e923}" } else { "\u{e922}" },
                WindowControlArea::Max,
            )
            .hover(|style| style.bg(rgb(theme::bg_hover()))),
        )
        .child(
            caption_button("caption-close", "\u{e8bb}", WindowControlArea::Close)
                .hover(|style| style.bg(rgb(CAPTION_CLOSE_HOVER)).text_color(rgb(0xFFFFFF))),
        )
}

/// Commands dispatched from the settings menu in the unified toolbar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccountMenuCommand {
    /// Open the settings page.
    Settings,
    /// Open the settings page on the shortcut list.
    Keymap,
    /// Open the settings page on the appearance section.
    Themes,
}

impl AccountMenuCommand {
    fn from_value(value: &Value) -> Option<Self> {
        if let Some(s) = value.as_str() {
            return Self::from_str(s);
        }
        value
            .get("command")
            .and_then(Value::as_str)
            .and_then(Self::from_str)
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "settings" => Some(Self::Settings),
            "keymap" => Some(Self::Keymap),
            "themes" => Some(Self::Themes),
            _ => None,
        }
    }
}

/// GPUI action wrapping an [`AccountMenuCommand`] dispatched from the account menu.
#[derive(Clone)]
pub struct AccountMenuAction {
    /// The command this action carries.
    pub command: AccountMenuCommand,
}

impl AccountMenuAction {
    /// Create an action for the given command.
    pub fn new(command: AccountMenuCommand) -> Self {
        Self { command }
    }

    /// Create a boxed action for the given command, ready to dispatch.
    pub fn boxed(command: AccountMenuCommand) -> Box<dyn Action> {
        Box::new(Self::new(command))
    }
}

impl fmt::Debug for AccountMenuAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountMenuAction")
            .field("command", &self.command)
            .finish()
    }
}

impl Action for AccountMenuAction {
    fn boxed_clone(&self) -> Box<dyn Action> {
        Box::new(self.clone())
    }

    fn partial_eq(&self, other: &dyn Action) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|action| action.command == self.command)
    }

    fn name(&self) -> &'static str {
        match self.command {
            AccountMenuCommand::Settings => "account-menu.settings",
            AccountMenuCommand::Keymap => "account-menu.keymap",
            AccountMenuCommand::Themes => "account-menu.themes",
        }
    }

    fn name_for_type() -> &'static str
    where
        Self: Sized,
    {
        "AccountMenuAction"
    }

    fn build(value: serde_json::Value) -> gpui::Result<Box<dyn Action>>
    where
        Self: Sized,
    {
        let command =
            AccountMenuCommand::from_value(&value).unwrap_or(AccountMenuCommand::Settings);
        Ok(Box::new(Self::new(command)))
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountMenuCommand, unified_toolbar};
    use gpui::{IntoElement, Render, TestAppContext, Window};
    use serde_json::json;

    #[test]
    fn from_str_maps_known_commands_and_rejects_unknown() {
        assert_eq!(
            AccountMenuCommand::from_str("settings"),
            Some(AccountMenuCommand::Settings)
        );
        assert_eq!(AccountMenuCommand::from_str("sign-out"), None);
        assert_eq!(AccountMenuCommand::from_str("nope"), None);
    }

    #[test]
    fn from_value_accepts_string_and_command_object() {
        assert_eq!(
            AccountMenuCommand::from_value(&json!("themes")),
            Some(AccountMenuCommand::Themes)
        );
        assert_eq!(
            AccountMenuCommand::from_value(&json!({ "command": "keymap" })),
            Some(AccountMenuCommand::Keymap)
        );
        assert_eq!(AccountMenuCommand::from_value(&json!(42)), None);
    }

    struct ToolbarHost {
        renders: usize,
    }

    impl Render for ToolbarHost {
        fn render(
            &mut self,
            window: &mut Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            self.renders += 1;
            unified_toolbar(window, "Prime Files", cx)
        }
    }

    #[gpui::test]
    async fn toolbar_builds_drag_region_and_account_button(cx: &mut TestAppContext) {
        // The account `Button` widget reads the gpui-component `Theme` global.
        cx.update(gpui_component::init);
        let (host, cx) = cx.add_window_view(|_window, _cx| ToolbarHost { renders: 0 });
        cx.run_until_parked();
        host.read_with(cx, |host, _cx| assert!(host.renders > 0));
    }
}
