use super::*;
use gpui::{AppContext, Modifiers, TestAppContext, VisualTestContext, point};

#[gpui::test]
fn archive_tool_starts_on_hatchzip_and_resets_to_it(cx: &mut TestAppContext) {
    let page = cx.new(SettingsPage::new);
    page.update(cx, |page, cx| {
        assert_eq!(page.config.archive.tool, config::ArchiveTool::HatchZip);
        let groups = page.files(cx);
        let row = groups
            .iter()
            .flat_map(|group| &group.rows)
            .find(|row| row.label == "解压工具")
            .expect("archive tool setting");
        assert!(row.hint.is_some());
        // At its default there is nothing to reset.
        assert!(row.reset.is_none());

        page.config.archive.tool = config::ArchiveTool::SevenZip;
        let groups = page.files(cx);
        let row = groups
            .iter()
            .flat_map(|group| &group.rows)
            .find(|row| row.label == "解压工具")
            .expect("archive tool setting");
        assert!(matches!(&row.reset,
            Some(Pick::Config("archive", "tool", value)) if value.as_ref() == "\"hatchzip\""
        ));
    });
}

#[gpui::test]
fn runtime_log_setting_is_off_by_default_and_resets_to_off(cx: &mut TestAppContext) {
    let page = cx.new(SettingsPage::new);
    page.update(cx, |page, cx| {
        assert!(!page.config.diagnostics.runtime_log);
        let groups = page.about(cx);
        let row = groups
            .iter()
            .flat_map(|group| &group.rows)
            .find(|row| row.label == t!("Logs de execução"))
            .expect("runtime log setting");
        assert!(row.hint.is_some());
        assert!(row.reset.is_none());
        page.config.diagnostics.runtime_log = true;
        let groups = page.about(cx);
        let row = groups
            .iter()
            .flat_map(|group| &group.rows)
            .find(|row| row.label == t!("Logs de execução"))
            .expect("runtime log setting");
        assert!(matches!(&row.reset,
            Some(Pick::Config("diagnostics", "runtime_log", value)) if value.as_ref() == "false"
        ));
    });
}

struct WheelPreview(Entity<SettingsPage>);

impl Render for WheelPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let control = self.0.update(cx, |page, cx| {
            page.wheel_row(WheelTarget::Background, cx)
                .expect("open wheel")
                .control
        });
        div()
            .id("wheel-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .child(control)
    }
}

fn wheel_preview(cx: &mut TestAppContext) -> (Entity<SettingsPage>, &mut VisualTestContext) {
    let (view, cx) = cx.add_window_view(|_window, cx| {
        let page = cx.new(|cx| {
            let mut page = SettingsPage::new(cx);
            page.wheel = Some(Wheel {
                hsv: Hsv::from_rgb(0xFF0000),
                dragging: None,
                target: WheelTarget::Background,
            });
            page
        });
        WheelPreview(page)
    });
    (view.read_with(cx, |view, _| view.0.clone()), cx)
}

#[gpui::test]
fn wheel_ring_responds_at_its_visible_position(cx: &mut TestAppContext) {
    let (page, cx) = wheel_preview(cx);
    let bounds = cx
        .debug_bounds("background-wheel-ring")
        .expect("visible ring");
    let centre = bounds.center();
    cx.simulate_mouse_down(centre, MouseButton::Left, Modifiers::none());
    page.read_with(cx, |page, _| {
        let wheel = page.wheel.as_ref().expect("open wheel");
        assert_eq!(Some(WheelPart::Ring), wheel.dragging);
        assert!(
            wheel.hsv.s < 0.01,
            "the centre must select white: {:?}",
            wheel.hsv
        );
    });

    cx.simulate_mouse_move(
        point(centre.x, bounds.origin.y - px(20.0)),
        MouseButton::Left,
        Modifiers::none(),
    );
    page.read_with(cx, |page, _| {
        let hsv = page.wheel.as_ref().expect("open wheel").hsv;
        assert!(
            (hsv.h - 270.0).abs() < 0.01,
            "drag must follow the pointer: {hsv:?}"
        );
        assert_eq!(1.0, hsv.s);
    });
}

#[gpui::test]
fn wheel_brightness_responds_at_its_visible_position(cx: &mut TestAppContext) {
    let (page, cx) = wheel_preview(cx);
    let bounds = cx
        .debug_bounds("background-wheel-brightness")
        .expect("visible brightness bar");
    cx.simulate_mouse_down(bounds.center(), MouseButton::Left, Modifiers::none());
    page.read_with(cx, |page, _| {
        let wheel = page.wheel.as_ref().expect("open wheel");
        assert_eq!(Some(WheelPart::Brightness), wheel.dragging);
        assert!((wheel.hsv.v - 0.5).abs() < 0.01);
    });

    cx.simulate_mouse_move(
        point(bounds.right() + px(20.0), bounds.center().y),
        MouseButton::Left,
        Modifiers::none(),
    );
    page.read_with(cx, |page, _| {
        assert_eq!(0.0, page.wheel.as_ref().expect("open wheel").hsv.v)
    });
}

#[gpui::test]
fn list_appearance_rows_sit_in_appearance_and_reset_to_their_defaults(cx: &mut TestAppContext) {
    fn reset_of(groups: &[Group], label: &'static str) -> Option<Pick> {
        groups
            .iter()
            .flat_map(|group| &group.rows)
            .find(|row| row.label == label)
            .unwrap_or_else(|| panic!("{label} setting"))
            .reset
            .clone()
    }

    let page = cx.new(SettingsPage::new);
    page.update(cx, |page, cx| {
        // At their defaults there is nothing to reset.
        let groups = page.appearance(cx);
        assert!(reset_of(&groups, t!("Altura das linhas")).is_none());
        assert!(reset_of(&groups, t!("Linhas zebradas")).is_none());
        assert!(reset_of(&groups, t!("Contorno da seleção")).is_none());
        assert!(reset_of(&groups, t!("Duplo clique no espaço vazio")).is_none());

        page.config.ui.list_row_height = ListRowHeight::Relaxed;
        page.config.ui.zebra_rows = true;
        page.config.ui.selection_outline = true;
        page.config.ui.double_click_empty = DoubleClickEmpty::Up;
        let groups = page.appearance(cx);
        assert!(matches!(
            reset_of(&groups, t!("Altura das linhas")),
            Some(Pick::Config("ui", "list_row_height", value)) if value.as_ref() == "\"standard\""
        ));
        assert!(matches!(
            reset_of(&groups, t!("Linhas zebradas")),
            Some(Pick::Config("ui", "zebra_rows", value)) if value.as_ref() == "false"
        ));
        assert!(matches!(
            reset_of(&groups, t!("Contorno da seleção")),
            Some(Pick::Config("ui", "selection_outline", value)) if value.as_ref() == "false"
        ));
        assert!(matches!(
            reset_of(&groups, t!("Duplo clique no espaço vazio")),
            Some(Pick::Config("ui", "double_click_empty", value)) if value.as_ref() == "\"none\""
        ));
    });
}

#[gpui::test]
fn file_status_rows_sit_in_files_and_reset_to_off(cx: &mut TestAppContext) {
    let page = cx.new(SettingsPage::new);
    page.update(cx, |page, cx| {
        let groups = page.files(cx);
        let group = groups
            .iter()
            .find(|group| group.title == Some(t!("Estado dos arquivos")))
            .expect("file status group");
        assert_eq!(group.rows.len(), 2);
        assert!(group.rows.iter().all(|row| row.reset.is_none()));

        page.config.ui.show_sync_status = true;
        page.config.ui.show_vcs_status = true;
        let groups = page.files(cx);
        let group = groups
            .iter()
            .find(|group| group.title == Some(t!("Estado dos arquivos")))
            .expect("file status group");
        let reset_of = |label: &'static str| {
            group
                .rows
                .iter()
                .find(|row| row.label == label)
                .and_then(|row| row.reset.clone())
        };
        assert!(matches!(
            reset_of(t!("Mostrar estado de sincronização")),
            Some(Pick::Config("ui", "show_sync_status", value)) if value.as_ref() == "false"
        ));
        assert!(matches!(
            reset_of(t!("Mostrar estado do Git/SVN")),
            Some(Pick::Config("ui", "show_vcs_status", value)) if value.as_ref() == "false"
        ));
    });
}

#[test]
fn search_ignores_case_and_accents() {
    assert_eq!(super::fold("Áudio nos ÍCONES"), "audio nos icones");
    assert!(super::fold("Configuração").contains(&super::fold("configuracao")));
}

#[gpui::test]
fn the_index_section_shows_its_three_tabs(cx: &mut TestAppContext) {
    use super::index::Tab;
    let page = cx.new(SettingsPage::new);
    page.update(cx, |page, cx| {
        assert!(SettingsSection::ALL.contains(&SettingsSection::Index));
        let labels = |page: &SettingsPage, cx: &mut Context<SettingsPage>| {
            let groups = page.groups(SettingsSection::Index, cx);
            groups
                .iter()
                .flat_map(|group| &group.rows)
                .map(|row| row.label.clone())
                .collect::<Vec<_>>()
        };

        page.index.tab = Tab::Volumes;
        assert!(
            labels(page, cx)
                .iter()
                .any(|label| label == t!("Unidades fixas")),
            "the volumes tab lists the drives"
        );

        page.index.tab = Tab::Exclude;
        assert!(
            labels(page, cx)
                .iter()
                .any(|label| label == t!("Ativar lista de exclusão")),
            "the exclude tab has the list switch"
        );

        page.index.tab = Tab::Advanced;
        assert!(
            labels(page, cx)
                .iter()
                .any(|label| label == t!("Indexação instantânea")),
            "the advanced tab has the instant indexing"
        );
    });
}
