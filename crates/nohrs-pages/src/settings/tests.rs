use super::*;
use gpui::{AppContext, Modifiers, TestAppContext, VisualTestContext, point};

#[gpui::test]
fn archive_tool_starts_on_hatchzip_and_resets_to_it(cx: &mut TestAppContext) {
    let page = cx.new(SettingsPage::new);
    page.update(cx, |page, cx| {
        assert_eq!(page.config.archive.tool, config::ArchiveTool::HatchZip);
        let groups = page.files(cx);
        let row = groups.iter().flat_map(|group| &group.rows)
            .find(|row| row.label == "解压工具").expect("archive tool setting");
        assert!(row.hint.is_some());
        // At its default there is nothing to reset.
        assert!(row.reset.is_none());

        page.config.archive.tool = config::ArchiveTool::SevenZip;
        let groups = page.files(cx);
        let row = groups.iter().flat_map(|group| &group.rows)
            .find(|row| row.label == "解压工具").expect("archive tool setting");
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
        let row = groups.iter().flat_map(|group| &group.rows)
            .find(|row| row.label == t!("Logs de execução")).expect("runtime log setting");
        assert!(row.hint.is_some());
        assert!(row.reset.is_none());
        page.config.diagnostics.runtime_log = true;
        let groups = page.about(cx);
        let row = groups.iter().flat_map(|group| &group.rows)
            .find(|row| row.label == t!("Logs de execução")).expect("runtime log setting");
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

#[test]
fn search_ignores_case_and_accents() {
    assert_eq!(super::fold("Áudio nos ÍCONES"), "audio nos icones");
    assert!(super::fold("Configuração").contains(&super::fold("configuracao")));
}
