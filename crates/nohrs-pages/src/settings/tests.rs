use super::*;
use gpui::{AppContext, Modifiers, TestAppContext, VisualTestContext, point};

struct WheelPreview(Entity<SettingsPage>);

impl Render for WheelPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let control = self.0.update(cx, |page, cx| {
            page.wheel_row(cx).expect("open wheel").control
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
