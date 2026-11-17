use super::*;
use gpui::{TestAppContext, WindowHandle};
use gpui_component::{input::InputState, resizable::ResizableState};
use nohrs_services::fs::{listing::FileEntryDto, trash::Ledger};

fn explorer(cx: &mut TestAppContext) -> WindowHandle<ExplorerPane> {
    cx.update(gpui_component::init);
    cx.add_window(|window, cx| {
        let resizable = cx.new(|_| ResizableState::default());
        let search = cx.new(|cx| InputState::new(window, cx));
        ExplorerPane::new(resizable, search, None, Ledger::KeptByOs, cx.focus_handle())
    })
}

fn entry(name: &str, kind: &str) -> FileEntryDto {
    FileEntryDto {
        name: name.into(),
        path: format!("C:/menu-test/{name}"),
        kind: kind.into(),
        size: 0,
        modified: 0,
    }
}

#[gpui::test]
fn item_menu_is_ready_synchronously_without_a_shell_session(cx: &mut TestAppContext) {
    let window = explorer(cx);
    window
        .update(cx, |pane, window, cx| {
            pane.filtered_entries = vec![entry("folder", "dir")];
            pane.open_context_menu(Some(0), true, window, cx);
            let menu = pane
                .context_menu
                .as_ref()
                .expect("menu ready without pumping tasks");
            assert!(menu.session.is_none());
            assert_eq!(4, menu.quick.len());
            for command in [
                Command::OpenSelection,
                Command::BatchRename,
                Command::SpaceMapSelection,
                Command::MoreOptions,
            ] {
                assert!(menu.items.iter().any(|item| item.id == command as u32));
            }
            let MenuPurpose::Listing {
                target: Target::Items(paths),
                one_folder,
                extended,
            } = &menu.purpose
            else {
                panic!("original Shell target must be retained")
            };
            assert_eq!(&vec!["C:/menu-test/folder".to_owned()], paths);
            assert!(*one_folder && *extended);
        })
        .unwrap();
}

#[gpui::test]
fn background_menu_is_ready_without_shell_and_keeps_common_commands(cx: &mut TestAppContext) {
    let window = explorer(cx);
    window
        .update(cx, |pane, window, cx| {
            pane.cwd = "C:/menu-test".into();
            pane.open_context_menu(None, false, window, cx);
            let menu = pane.context_menu.as_ref().expect("background menu ready");
            assert!(menu.session.is_none());
            assert!(menu.quick.is_empty());
            for command in [
                Command::Refresh,
                Command::Paste,
                Command::NewFolder,
                Command::MoreOptions,
            ] {
                assert!(menu.items.iter().any(|item| item.id == command as u32));
            }
            let MenuPurpose::Listing {
                target: Target::Background(path),
                ..
            } = &menu.purpose
            else {
                panic!("background target must be retained for more options")
            };
            assert_eq!("C:/menu-test", path);
        })
        .unwrap();
}

#[gpui::test]
fn own_open_command_opens_all_selected_regular_files(cx: &mut TestAppContext) {
    let window = explorer(cx);
    window
        .update(cx, |pane, window, cx| {
            super::super::navigation::OPENED_WITH_DEFAULT_APP
                .with(|paths| paths.borrow_mut().clear());
            pane.filtered_entries = vec![entry("first.txt", "file"), entry("second.txt", "file")];
            pane.select_all();
            pane.open_context_menu(Some(0), false, window, cx);
            pane.choose_menu_item(Command::OpenSelection as u32, window, cx);
            assert!(pane.context_menu.is_none());
            super::super::navigation::OPENED_WITH_DEFAULT_APP.with(|paths| {
                assert_eq!(
                    vec!["C:/menu-test/first.txt", "C:/menu-test/second.txt"],
                    *paths.borrow()
                );
            });
        })
        .unwrap();
}

#[test]
fn quick_actions_preserve_common_file_operations() {
    let commands: Vec<_> = quick_actions()
        .into_iter()
        .map(|action| action.id)
        .collect();
    assert_eq!(
        vec![
            Command::Cut as u32,
            Command::Copy as u32,
            Command::Rename as u32,
            Command::Delete as u32
        ],
        commands
    );
}
