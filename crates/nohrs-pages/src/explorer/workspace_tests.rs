use super::super::types::ViewMode;
use super::{file, new_explorer};
use crate::explorer::ExplorerPane;
use crate::explorer::view::{self, workspace};
use gpui::{
    AppContext, Context, Entity, IntoElement, Modifiers, Render, ScrollDelta, ScrollWheelEvent,
    TestAppContext, VisualTestContext, Window, point, px, size,
};
use gpui_component::{Root, input::InputState, resizable::ResizableState};
use nohrs_services::fs::trash::Ledger as TrashLedger;

#[gpui::test]
fn workspace_starts_with_cards_and_restores_its_chosen_list(cx: &mut TestAppContext) {
    cx.update(|cx| super::super::folder_views::FolderViews::install(None, cx));
    let window = new_explorer(cx);
    window
        .update(cx, |pane, _, cx| {
            pane.cwd = "tag:1".into();
            pane.enter_folder_view(cx);
            assert!(
                pane.view_mode == ViewMode::Grid,
                "new workspaces start with cards"
            );
            pane.set_zoom(0.0, cx);
            pane.cwd = "tag:2".into();
            pane.enter_folder_view(cx);
            assert!(pane.view_mode == ViewMode::Grid);
            pane.cwd = "tag:1".into();
            pane.enter_folder_view(cx);
            assert!(
                pane.view_mode == ViewMode::List,
                "the selected view survives navigation"
            );
        })
        .unwrap();
}

fn workspace_window(cx: &mut TestAppContext) -> (Entity<ExplorerPane>, &mut VisualTestContext) {
    struct WorkspacePage {
        pane: Entity<ExplorerPane>,
        cards: Vec<workspace::Card>,
    }
    impl Render for WorkspacePage {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let cards = self.cards.clone();
            self.pane.update(cx, |pane, cx| {
                if pane.view_mode == ViewMode::Grid {
                    workspace::page(pane, 1, &cards, cx)
                } else {
                    view::listing::render(pane, _window, cx)
                }
            })
        }
    }
    cx.update(gpui_component::init);
    cx.update(|cx| super::super::tags::Tags::install(None, cx));
    let mut pane = None;
    let cards = vec![workspace::Card {
        path: "C:/docs".into(),
        name: "docs".into(),
        parent: "C:/".into(),
        items: 2,
        size: 20,
        changed: None,
    }];
    let (_, cx) = cx.add_window_view(|window, cx| {
        let resizable = cx.new(|_| ResizableState::default());
        let search = cx.new(|cx| InputState::new(window, cx));
        let view = cx.new(|cx| {
            let mut pane = ExplorerPane::new(
                resizable,
                search,
                None,
                TrashLedger::KeptByOs,
                cx.focus_handle(),
            );
            pane.cwd = "tag:1".into();
            pane.loaded = true;
            pane.sidebar_visible = false;
            pane.preview_visible = false;
            pane.entries = vec![file("docs", "dir", 0), file("readme.txt", "file", 20)];
            pane.apply_filter();
            pane.set_zoom(super::super::zoom::DEFAULT_GRID, cx);
            pane
        });
        pane = Some(view.clone());
        Root::new(cx.new(|_| WorkspacePage { pane: view, cards }), window, cx)
    });
    cx.simulate_resize(size(px(1400.0), px(900.0)));
    cx.run_until_parked();
    (pane.unwrap(), cx)
}

#[gpui::test]
fn workspace_ctrl_wheel_switches_between_cards_and_list(cx: &mut TestAppContext) {
    let (pane, cx) = workspace_window(cx);
    let position = point(px(800.0), px(420.0));
    let modifiers = Modifiers {
        control: true,
        ..Modifiers::none()
    };
    cx.simulate_mouse_move(position, None, modifiers);
    cx.simulate_event(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Lines(point(0.0, -3.0)),
        modifiers,
        ..Default::default()
    });
    pane.read_with(cx, |pane, _| {
        assert!(
            pane.view_mode == ViewMode::List,
            "wheel down shows the list"
        );
        assert_eq!(pane.zoom_target, 0.0);
        assert!(pane.zoom_tick.is_none(), "the switch is immediate");
    });
}
