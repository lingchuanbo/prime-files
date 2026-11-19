use crate::explorer::ExplorerPane;
use gpui::prelude::*;
use gpui::*;
use gpui_component::text::TextView;
use gpui_component::{Icon, IconName};
use nohrs_core::i18n;
use nohrs_core::{k, t, tf};
use nohrs_ui::theme::theme;

/// Edge of the system thumbnail asked for when the pane has no richer preview.
const PREVIEW_THUMBNAIL_SIZE: u32 = 512;

/// The edges a fitted image is asked of the Shell in. Steps rather than the
/// pane's exact size, so dragging the divider does not ask again at every
/// pixel; the top one covers a full-height pane on a 4K screen at 100%.
const FITTED_EDGES: [u32; 3] = [512, 1024, 2048];

/// The edge to ask for so an image fitted in the preview is drawn from at
/// least as many pixels as it covers.
pub(crate) fn fitted_edge(preview_width: f32, window_height: f32, scale: f32) -> u32 {
    let needed = (preview_width.max(window_height) * scale).ceil();
    FITTED_EDGES
        .into_iter()
        .find(|&edge| edge as f32 >= needed)
        .unwrap_or(FITTED_EDGES[FITTED_EDGES.len() - 1])
}

/// An image fitted in the preview, drawn from the Shell's picture at the
/// size the pane shows, or `None` to decode the file itself.
///
/// Decoding the file is what `img(path)` does, at full size: a 48 MP photo
/// took the process from ~122 to ~677 MB, held for as long as it stayed in
/// the preview (`docs/PERFORMANCE.md`, passada 7). Fitted, the pane shows at
/// most a couple of megapixels of it. The Shell's picture comes from the same
/// cache and workers as the grid's tiles, with Windows' own codecs (and the
/// photo's orientation), and the grid tile of the same file is shown while it
/// is made. The file is still decoded whole when that is what is needed: a
/// zoom past 100%, an animated GIF (the Shell's picture is one frame), or a
/// format the Shell has no picture of.
pub(crate) fn fitted_picture(
    page: &mut ExplorerPane,
    image_path: &str,
    window: &Window,
    cx: &mut Context<ExplorerPane>,
) -> Option<AnyElement> {
    let animated = image_path.to_lowercase().ends_with(".gif");
    if page.preview_zoom > 1.0 || animated {
        return None;
    }
    let edge = fitted_edge(
        super::panels::preview_width(),
        f32::from(window.viewport_size().height),
        window.scale_factor(),
    );
    // The file decoded at this size, sharp; the Shell's picture (or the grid
    // tile) only while that is being made, or if it cannot be.
    if let Some(sharp) = sharp_picture(page, image_path, edge, cx) {
        page.gif_cache = None;
        return Some(
            img(ImageSource::Render(sharp))
                .size_full()
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element(),
        );
    }
    let shown = match page.thumbnails.picture(image_path, edge, cx) {
        crate::explorer::thumbnails::Picture::Shown(image) => Some(image),
        crate::explorer::thumbnails::Picture::Waiting(meanwhile) => meanwhile,
        crate::explorer::thumbnails::Picture::Unavailable => return None,
    };
    // Back to fitted after a zoom: the whole decode that the zoom needed is
    // not drawn any more, and holding it would keep the memory this saves.
    page.gif_cache = None;
    Some(match shown {
        Some(image) => img(ImageSource::Render(image))
            .size_full()
            .object_fit(gpui::ObjectFit::Contain)
            .into_any_element(),
        // Nothing yet: an empty frame, not the full decode, which would cost
        // the very memory this avoids just to be replaced a moment later.
        None => div().size_full().into_any_element(),
    })
}

/// The preview's image decoded from its file (`explorer/picture.rs`): for
/// which file and size, and the picture once it is made (`None` while it is,
/// or when the file could not be decoded).
pub(crate) struct SharpPicture {
    path: String,
    edge: u32,
    image: Option<std::sync::Arc<gpui::RenderImage>>,
    // The same file's picture at the size asked before, shown until this one
    // is made, so resizing the panel never falls back to the Shell's.
    previous: Option<std::sync::Arc<gpui::RenderImage>>,
    // Dropped with the request: a newer file or size cancels the wait.
    _decode: Option<gpui::Task<()>>,
}

// The sharp picture of `path` at `edge`, asking for it the first time.
fn sharp_picture(
    page: &mut ExplorerPane,
    path: &str,
    edge: u32,
    cx: &mut Context<ExplorerPane>,
) -> Option<std::sync::Arc<gpui::RenderImage>> {
    if let Some(sharp) = &page.preview_sharp
        && sharp.path == path
        && sharp.edge == edge
    {
        return sharp.image.clone().or_else(|| sharp.previous.clone());
    }
    let decode = cx.background_spawn({
        let path = path.to_string();
        async move { crate::explorer::picture::decode_fitted(&path, edge) }
    });
    let wanted = path.to_string();
    let task = cx.spawn(async move |this: gpui::WeakEntity<ExplorerPane>, cx| {
        let decoded = decode.await;
        this.update(cx, |pane, cx| {
            let Some(sharp) = pane
                .preview_sharp
                .as_mut()
                .filter(|sharp| sharp.path == wanted && sharp.edge == edge)
            else {
                return;
            };
            match decoded {
                Ok((width, height, pixels)) => {
                    sharp.image = crate::explorer::thumbnails::render_image(width, height, pixels)
                        .map(std::sync::Arc::new);
                }
                // The Shell's picture stays, as before.
                Err(error) => tracing::debug!("could not decode {wanted} for the preview: {error}"),
            }
            sharp._decode = None;
            cx.notify();
        })
        .ok();
    });
    // The previous size stays on screen until the new one is ready.
    let previous = page
        .preview_sharp
        .take()
        .filter(|sharp| sharp.path == path)
        .and_then(|sharp| sharp.image.or(sharp.previous));
    page.preview_sharp = Some(SharpPicture {
        path: path.to_string(),
        edge,
        image: None,
        previous: previous.clone(),
        _decode: Some(task),
    });
    previous
}

/// Space's quick look: the preview panel, large, over the whole pane. A
/// click on the dimmed edge closes it; Space and Esc close it from the
/// keyboard (`view.rs`). It is the same panel the side shows, so every kind
/// of file the side previews, it previews, with the same zoom.
pub fn render_quick_look(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> Option<AnyElement> {
    if !page.quick_look {
        return None;
    }
    let content = render(page, window, cx);
    // Inset rather than a percentage: a percentage size under a centring
    // parent comes out 0 in this layout engine.
    let bounds = window.viewport_size();
    let inset_x = (f32::from(bounds.width) * 0.07).clamp(16.0, 120.0);
    let inset_y = (f32::from(bounds.height) * 0.06).clamp(16.0, 80.0);
    Some(
        div()
            .id("quick-look")
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.5))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| this.close_quick_look(cx)),
            )
            .child(
                div()
                    .id("quick-look-card")
                    .absolute()
                    .top(px(inset_y))
                    .bottom(px(inset_y))
                    .left(px(inset_x))
                    .right(px(inset_x))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(rgb(theme::border()))
                    .bg(rgb(theme::bg()))
                    .shadow_lg()
                    // Clicks inside are the preview's, not the backdrop's.
                    .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                        cx.stop_propagation()
                    })
                    .child(content),
            )
            .into_any_element(),
    )
}

// Calculate the maximum line width in characters for horizontal scroll sizing
/// Renders the preview pane for the selected file, showing a text editor,
/// image, status message, or an empty placeholder.
pub fn render(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> impl IntoElement + use<> {
    let title = page
        .preview_path
        .as_ref()
        .map(|p| path_name(p))
        .unwrap_or_else(|| t!("Visualizador").to_string());

    let content = if let Some(editor) = &page.preview_editor {
        // Held the same way as the Markdown below: under the wheel's flex
        // column the editor's full height came out as none, and text files
        // previewed blank in the side panel (the quick look's card, sized
        // by its insets, showed them).
        div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(editor.clone()),
            )
            .into_any_element()
    } else if let Some(markdown) = page.preview_markdown.clone() {
        // Keyed by path so the view's scroll and parse state belong to one
        // file and do not carry over to the next one opened.
        let id = SharedString::from(format!(
            "preview-markdown:{}",
            page.preview_path.as_deref().unwrap_or_default()
        ));
        // The view scrolls with a gpui `list`, which draws nothing without a
        // definite height, and its `size_full` under this flex column came
        // out as no height at all: the preview of every .md was blank. Laid
        // absolutely over a holder, its 100% is the holder's size.
        div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .overflow_hidden()
            .child(
                TextView::markdown(id, markdown, window, cx)
                    .selectable(true)
                    .scrollable(true)
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .px(px(16.0))
                    .py(px(12.0))
                    .text_sm()
                    .text_color(rgb(theme::fg())),
            )
            .into_any_element()
    } else if page.preview_model.is_some() {
        render_model(page, window, cx)
    } else if page.preview_media.is_some() {
        render_media(page, cx)
    } else if page.preview_pdf.is_some() {
        render_pdf(page)
    } else if let Some(listing) = page.preview_listing.clone() {
        render_listing(page, listing, window, cx)
    } else if let Some(picture) = page.preview_picture.clone() {
        let picture = img(ImageSource::Render(picture))
            .size_full()
            .object_fit(gpui::ObjectFit::Contain);
        zoom_frame(
            page,
            "preview-picture-frame",
            picture.into_any_element(),
            true,
            cx,
        )
    } else if let Some(image_path) = page.preview_image_path.clone()
        && let Some(fitted) = fitted_picture(page, &image_path, window, cx)
    {
        zoom_frame(page, "preview-image-frame", fitted, true, cx)
    } else if let Some(image_path) = page.preview_image_path.clone() {
        let cache = page.preview_image_cache(&image_path, cx);
        let image = img(std::path::PathBuf::from(&image_path))
            .image_cache(&cache)
            // GPUI only advances an animated image's frames for an element
            // with an id (the frame counter is element state); without one a
            // GIF showed its first frame only. A `PathBuf`, not the string:
            // GPUI reads a string source as a URL or an embedded asset.
            .id(SharedString::from(format!("preview-image-{image_path}")))
            .size_full()
            .object_fit(gpui::ObjectFit::Contain);
        zoom_frame(
            page,
            "preview-image-frame",
            image.into_any_element(),
            true,
            cx,
        )
    } else if let Some(msg) = &page.preview_message {
        // No text and no image the pane can draw itself (a video, a program, an
        // archive): the system thumbnail still says what the file is, often a
        // frame of the video or the program's own icon.
        let thumbnail = page
            .preview_path
            .clone()
            .and_then(|path| page.thumbnails.get(&path, PREVIEW_THUMBNAIL_SIZE, cx));
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .p(px(16.0))
            // Without this the image's own 512px became the pane's minimum
            // width and pushed the divider over. Not `min_w(0)`: on a
            // stretched item of a column taffy then measured the text at its
            // narrowest, one letter a line, and kept that height.
            .overflow_hidden()
            .when_some(thumbnail, |this, image| {
                this.child(
                    img(ImageSource::Render(image))
                        .min_w(px(0.0))
                        .w_full()
                        .max_h(px(PREVIEW_THUMBNAIL_SIZE as f32))
                        .flex_shrink()
                        .object_fit(gpui::ObjectFit::Contain),
                )
            })
            .child(
                div()
                    .whitespace_nowrap()
                    .text_xs()
                    .text_color(rgb(theme::muted()))
                    .child(msg.clone()),
            )
            .into_any_element()
    } else {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                Icon::new(IconName::File)
                    .size_8()
                    .text_color(rgb(theme::gray_300())),
            )
            .child(
                // On one line: under `items_center` a text is laid out at its
                // narrowest, one letter a line, and `w_full` there came out 0.
                div()
                    .whitespace_nowrap()
                    .text_sm()
                    .text_color(rgb(theme::muted()))
                    .child(t!("Nenhum arquivo selecionado")),
            )
            .into_any_element()
    };

    div()
        .size_full()
        .flex()
        .flex_col()
        .when(!page.framed.preview, |this| this.bg(rgb(theme::bg())))
        .child(
            // Header. Matches the listing's column-header height so the two
            // line up across the divider between the panes.
            div()
                .h(px(super::listing::list::HEADER_HEIGHT))
                .flex()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .pl(px(if page.preview_back.is_empty() {
                    16.0
                } else {
                    6.0
                }))
                .pr(px(6.0))
                .border_b_1()
                .border_color(rgb(theme::border()))
                .when(!page.preview_back.is_empty(), |header| {
                    header.child(header_button("preview-back", "\u{e72b}").on_click(
                        cx.listener(|this, _, window, cx| this.preview_go_back(window, cx)),
                    ))
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::fg()))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(title),
                )
                .when_some(page.preview_caption.clone(), |header, caption| {
                    header.child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(rgb(theme::muted()))
                            .whitespace_nowrap()
                            .child(caption),
                    )
                })
                // A Markdown file: its source to edit, and back to the view.
                .when(
                    page.preview_saveable
                        && (page.preview_markdown.is_some() || page.preview_editing),
                    |header| {
                        let (glyph, tip) = if page.preview_editing {
                            ("\u{e890}", t!("Ver formatado"))
                        } else {
                            ("\u{e70f}", t!("Editar"))
                        };
                        header.child(
                            header_button("preview-edit", glyph)
                                .tooltip(move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(tip).build(window, cx)
                                })
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.toggle_preview_editing(window, cx)
                                })),
                        )
                    },
                )
                .when(page.preview_dirty, |header| {
                    header.child(
                        header_button("preview-save", "\u{e74e}")
                            .text_color(rgb(theme::accent()))
                            .tooltip(|window, cx| {
                                gpui_component::tooltip::Tooltip::new(t!("Salvar (Ctrl+S)"))
                                    .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, _window, cx| this.save_preview(cx))),
                    )
                })
                .when(zoomable(page), |header| {
                    header.child(render_zoom_controls(page, cx))
                }),
        )
        .child(with_preview_wheel(content, cx))
}

/// Whether what the preview shows can be magnified.
fn zoomable(page: &ExplorerPane) -> bool {
    page.preview_image_path.is_some()
        || page.preview_picture.is_some()
        || page.preview_pdf.is_some()
        || page.preview_model.is_some()
        || page.preview_listing.is_some()
        || page
            .preview_media
            .as_ref()
            .is_some_and(|media| media.has_video || !media.peaks.is_empty())
}

// A small square button of the header, a Segoe Fluent glyph in it.
fn header_button(id: &'static str, glyph: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::bg_hover())))
        .font_family("Segoe Fluent Icons")
        .text_size(px(12.0))
        .text_color(rgb(theme::fg_secondary()))
        .child(glyph)
}

// Out, the zoom as a percentage (a click goes back to fit), and in.
fn render_zoom_controls(page: &ExplorerPane, cx: &mut Context<ExplorerPane>) -> impl IntoElement {
    let zoom = page.current_preview_zoom();
    div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .child(
            header_button("preview-zoom-out", "\u{e71f}")
                .on_click(cx.listener(|this, _, _, cx| this.step_preview_zoom(-1.0, None, cx))),
        )
        .child(
            div()
                .id("preview-zoom-reset")
                .min_w(px(40.0))
                .h(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .on_click(cx.listener(|this, _, _, cx| this.set_preview_zoom(1.0, cx)))
                .child(format!("{:.0}%", zoom * 100.0)),
        )
        .child(
            header_button("preview-zoom-in", "\u{e710}")
                .on_click(cx.listener(|this, _, _, cx| this.step_preview_zoom(1.0, None, cx))),
        )
}

// The wheel over the preview's content zooms it (Ctrl+wheel for what also
// scrolls), ahead of the scrolling the content would otherwise do (the
// capture phase, as the listing's own zoom).
fn with_preview_wheel(content: AnyElement, cx: &mut Context<ExplorerPane>) -> AnyElement {
    let entity = cx.entity();
    div()
        .relative()
        .flex_1()
        .flex()
        .flex_col()
        .min_h(px(0.0))
        .overflow_hidden()
        .child(content)
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _cx| {
                    let entity = entity.clone();
                    window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _window, cx| {
                        if phase != DispatchPhase::Capture || !bounds.contains(&event.position) {
                            return;
                        }
                        // A picture, a video, a waveform or a model zooms
                        // with the plain wheel too: it fits the view, so the
                        // wheel has nothing to scroll. A PDF, a folder or a
                        // text scrolls with it and zooms with Ctrl.
                        let plain = {
                            let page = entity.read(cx);
                            page.preview_model.is_some()
                                || page.preview_image_path.is_some()
                                || page.preview_media.is_some()
                        };
                        if !event.modifiers.secondary() && !plain {
                            return;
                        }
                        // Shift, or a wheel that tilts sideways, moves along
                        // what is zoomed instead: the frame under it scrolls.
                        // A zoomed waveform had no other way to be scrolled.
                        let sideways = match event.delta {
                            ScrollDelta::Lines(lines) => lines.x != 0.0,
                            ScrollDelta::Pixels(pixels) => pixels.x != px(0.0),
                        };
                        if plain && (event.modifiers.shift || sideways) {
                            return;
                        }
                        let notches = match event.delta {
                            ScrollDelta::Lines(lines) => lines.y.signum(),
                            ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / 60.0,
                        };
                        if notches != 0.0 {
                            let anchor = event.position;
                            entity.update(cx, |page, cx| {
                                page.step_preview_zoom(notches, Some(anchor), cx)
                            });
                        }
                        cx.stop_propagation();
                    });
                },
            )
            .absolute()
            .size_full(),
        )
        .into_any_element()
}

/// Factor of one zoom step: a wheel notch or a click on + or -.
const ZOOM_STEP: f32 = 1.2;

impl ExplorerPane {
    /// The zoom of what the preview shows: the listing keeps its own.
    pub(crate) fn current_preview_zoom(&self) -> f32 {
        if self.preview_listing.is_some() {
            self.preview_listing_zoom
        } else {
            self.preview_zoom
        }
    }

    /// Zooms by `steps` steps, in (positive) or out, keeping still the point
    /// under `anchor` (the mouse, in window coordinates) — or the middle of
    /// the view, for the + and - buttons.
    pub(crate) fn step_preview_zoom(
        &mut self,
        steps: f32,
        anchor: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let zoom = self.current_preview_zoom() * ZOOM_STEP.powf(steps);
        self.set_preview_zoom_at(zoom, anchor, cx);
    }

    /// Sets the preview's zoom, held to what each kind of content can use,
    /// and keeps the point at the middle of the view in the middle.
    pub(crate) fn set_preview_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        self.set_preview_zoom_at(zoom, None, cx);
    }

    fn set_preview_zoom_at(
        &mut self,
        zoom: f32,
        anchor: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if self.preview_listing.is_some() {
            self.preview_listing_zoom = zoom.clamp(LISTING_ZOOM.0, LISTING_ZOOM.1);
        } else {
            let old = self.preview_zoom;
            // A waveform only grows (it already spans the view at 1): below
            // it the number went down to 25% with nothing on screen changing,
            // and the way back took as many notches doing nothing.
            let least = if self.preview_is_waveform() {
                1.0
            } else {
                0.25
            };
            let zoom = zoom.clamp(least, 12.0);
            self.preview_zoom = zoom;
            let bounds = self.preview_scroll.bounds();
            // Where the anchor sits in the view; outside it (the buttons
            // below the frame) the middle stands in.
            let local = anchor.filter(|anchor| bounds.contains(anchor)).map_or(
                point(bounds.size.width / 2.0, bounds.size.height / 2.0),
                |anchor| anchor - bounds.origin,
            );
            self.preview_scroll.set_offset(anchored_offset(
                self.preview_scroll.offset(),
                bounds.size,
                local,
                old,
                zoom,
            ));
            self.draw_model(cx);
        }
        cx.notify();
    }

    /// Whether the preview shows an audio file's waveform.
    fn preview_is_waveform(&self) -> bool {
        self.preview_media
            .as_ref()
            .is_some_and(|media| media.frame.is_none() && !media.peaks.is_empty())
    }

    // Pans the zoomed content by a drag to `position`.
    fn pan_preview(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(last) = self.preview_pan.replace(position) else {
            return;
        };
        let max = self.preview_scroll.max_offset();
        let offset = self.preview_scroll.offset() + (position - last);
        self.preview_scroll.set_offset(point(
            offset.x.clamp(-max.width, px(0.0)),
            offset.y.clamp(-max.height, px(0.0)),
        ));
        cx.notify();
    }
}

/// The scroll offset that keeps the content point under `anchor` (in view
/// coordinates) under it when the zoom goes from `old` to `new`. Content only
/// grows past the view above 1, so the scale between the two is that of the
/// part above 1; the result stays within what the grown content can scroll.
fn anchored_offset(
    offset: Point<Pixels>,
    view: Size<Pixels>,
    anchor: Point<Pixels>,
    old: f32,
    new: f32,
) -> Point<Pixels> {
    let factor = new.max(1.0) / old.max(1.0);
    let axis = |offset: Pixels, view: Pixels, anchor: Pixels| {
        let (offset, view, anchor) = (f32::from(offset), f32::from(view), f32::from(anchor));
        let under = anchor - offset;
        let most = view * (new.max(1.0) - 1.0);
        px((anchor - under * factor).clamp(-most, 0.0))
    };
    point(
        axis(offset.x, view.width, anchor.x),
        axis(offset.y, view.height, anchor.y),
    )
}

// Content shown at the preview's zoom: at 1 it fits the view, smaller below
// it, and above it it grows past the view, which then scrolls to show the
// rest. Dragging with the wheel button pans it in every preview; with `pan`,
// the left button does too (not in the player, where a click plays).
fn zoom_frame(
    page: &ExplorerPane,
    id: &'static str,
    child: AnyElement,
    pan: bool,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let zoom = page.preview_zoom;
    let grown = zoom.max(1.0);
    let frame = div()
        .id(id)
        // Laid over a holder rather than in the flow: the frame must be the
        // size of the room it is given, never of what it shows. In the flow
        // the picture's own size (a 512 px icon, a 1280 px video) came up as
        // the frame's minimum width: at 100% it showed cropped, the zoom kept
        // still a point of that oversized frame instead of the one under the
        // mouse, and with nothing left over to scroll the wheel-button drag
        // did not move it. `min_w(0)` used to hold it, but on the preview's
        // columns it also measured every text at its narrowest, one letter a
        // line; an absolute child adds nothing to its holder's size at all.
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .bg(rgb(theme::preview_backdrop()))
        .overflow_scroll()
        .track_scroll(&page.preview_scroll)
        .when(pan && zoom > 1.0, |frame| {
            frame
                .cursor_grab()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, _window, _cx| {
                        this.preview_pan = Some(event.position);
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                        this.preview_pan = None
                    }),
                )
        })
        .when(zoom > 1.0, |frame| {
            frame
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                        this.preview_pan = Some(event.position);
                        // The player under it must not take the press.
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up(
                    MouseButton::Middle,
                    cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                        this.preview_pan = None
                    }),
                )
                .on_mouse_move(
                    cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                        let dragging = match event.pressed_button {
                            Some(MouseButton::Middle) => true,
                            Some(MouseButton::Left) => pan,
                            _ => false,
                        };
                        if dragging {
                            this.pan_preview(event.position, cx);
                        } else {
                            this.preview_pan = None;
                        }
                    }),
                )
        })
        .child(
            div()
                .w(relative(grown))
                .h(relative(grown))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(relative(zoom.min(1.0)))
                        .h(relative(zoom.min(1.0)))
                        .child(child),
                ),
        );
    div()
        .relative()
        .flex_1()
        .w_full()
        .min_h(px(0.0))
        .child(frame)
        .into_any_element()
}

// The model, turned by dragging, zoomed by the wheel; a double click puts
// it back as it opened.
fn render_model(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let Some(model) = page.preview_model.as_ref() else {
        return div().into_any_element();
    };
    let status = match (&model.error, model.mesh.is_some()) {
        (Some(error), _) => tf!("Não foi possível abrir o modelo: {error}", error = error),
        (None, false) => t!("Abrindo…").to_string(),
        (None, true) => tf!(
            "{n} triângulos · arraste para girar, roda para aproximar, duplo clique volta",
            n = model.triangle_count()
        ),
    };
    let view = model.view.clone();
    let scale = window.scale_factor();
    let image = model.image.clone();
    // The view's size changed since the last drawing (the pane was resized,
    // or this is the first frame): draw again at the new size.
    page.draw_model(cx);
    div()
        .flex_1()
        .min_h(px(0.0))
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(
            div()
                .flex_shrink_0()
                .px(px(16.0))
                .py(px(8.0))
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child(status),
        )
        .child(
            div()
                .id("preview-model")
                .flex_1()
                .min_h(px(0.0))
                .relative()
                .bg(rgb(theme::preview_backdrop()))
                .cursor_grab()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                        if event.click_count >= 2 {
                            this.reset_model_view(cx);
                        }
                        this.preview_pan = Some(event.position);
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                        this.preview_pan = None
                    }),
                )
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                    if event.pressed_button != Some(MouseButton::Left) {
                        this.preview_pan = None;
                        return;
                    }
                    if let Some(last) = this.preview_pan.replace(event.position) {
                        let delta = event.position - last;
                        this.turn_model(f32::from(delta.x), f32::from(delta.y), cx);
                    }
                }))
                .when_some(image, |area, image| {
                    area.child(
                        img(ImageSource::Render(image))
                            .size_full()
                            .object_fit(gpui::ObjectFit::Fill),
                    )
                })
                .child(
                    canvas(
                        move |bounds, window, _| {
                            let size = (
                                (f32::from(bounds.size.width) * scale).round() as u32,
                                (f32::from(bounds.size.height) * scale).round() as u32,
                            );
                            if view.replace(size) != size {
                                window.refresh();
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                ),
        )
        .into_any_element()
}

// The PDF's pages, one under the other, as they finish rendering.
fn render_pdf(page: &ExplorerPane) -> AnyElement {
    let Some(pdf) = page.preview_pdf.as_ref() else {
        return div().into_any_element();
    };
    let status = match (&pdf.error, pdf.total) {
        (Some(error), _) => tf!("Não foi possível abrir o PDF: {error}", error = error),
        (None, None) => t!("Abrindo…").to_string(),
        (None, Some(total)) if (pdf.pages.len() as u32) < total.min(40) => {
            tf!(
                "{loaded} de {total} páginas…",
                loaded = pdf.pages.len(),
                total = total
            )
        }
        (None, Some(total)) if total > 40 => tf!("Primeiras 40 de {total} páginas", total = total),
        (None, Some(total)) => i18n::plural(total as usize, k!("{n} página"), k!("{n} páginas")),
    };
    div()
        .flex_1()
        .min_h(px(0.0))
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(
            div()
                .flex_shrink_0()
                .px(px(16.0))
                .py(px(8.0))
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child(status),
        )
        .child(
            div()
                .id(SharedString::from(format!(
                    "preview-pdf:{}",
                    page.preview_path.as_deref().unwrap_or_default()
                )))
                .flex_1()
                .min_h(px(0.0))
                .overflow_scroll()
                .track_scroll(&page.preview_scroll)
                .bg(rgb(theme::preview_backdrop()))
                .p(px(12.0))
                .child(
                    // At the preview's zoom: wider than the view past 1, which
                    // then scrolls sideways too; centred below it.
                    div()
                        .relative()
                        .w(relative(page.preview_zoom))
                        .mx_auto()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .children(pdf.pages.iter().map(|image| {
                            // Each page as wide as the column and as tall as
                            // its own proportions make it. With only a
                            // relative width, gpui gave the image the height
                            // it was rendered at (1100 px wide), and every
                            // page left that much empty below it in a narrow
                            // panel (seen 05/11/2026).
                            let rendered = image.size(0);
                            let page_width = f32::from(page.preview_pdf_bounds.get().size.width);
                            let picture = img(ImageSource::Render(image.clone())).flex_shrink_0();
                            if page_width > 0.0 && rendered.width.0 > 0 {
                                let ratio = rendered.height.0 as f32 / rendered.width.0 as f32;
                                picture.w(px(page_width)).h(px(page_width * ratio))
                            } else {
                                picture.w_full()
                            }
                        }))
                        .child(crate::dock::measure(page.preview_pdf_bounds.clone())),
                ),
        )
        .into_any_element()
}

// The player: the video (or a note for audio) and a bar of controls under it.
fn render_media(page: &ExplorerPane, cx: &mut Context<ExplorerPane>) -> AnyElement {
    let Some(media) = page.preview_media.as_ref() else {
        return div().into_any_element();
    };
    // Glyphs from Segoe Fluent Icons, as the window's caption buttons use.
    let glyph = |text: &'static str| {
        div()
            .font_family("Segoe Fluent Icons")
            .text_size(px(14.0))
            .text_color(rgb(theme::fg()))
            .child(text)
    };
    let fraction = if media.duration > 0.0 {
        (media.position / media.duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let track = std::rc::Rc::new(std::cell::Cell::new(Bounds::<Pixels>::default()));
    let track_for_click = track.clone();
    let screen = div()
        .id("preview-media-screen")
        .flex_1()
        .min_h(px(0.0))
        .overflow_hidden()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .bg(rgb(theme::preview_backdrop()))
        .cursor_pointer()
        .on_click(cx.listener(|this, _event, _window, cx| this.toggle_media(cx)))
        .map(|screen| match (&media.error, &media.frame) {
            (Some(error), _) => screen.child(
                div()
                    .px(px(16.0))
                    .text_sm()
                    .text_color(rgb(theme::muted()))
                    .child(error.clone()),
            ),
            (None, Some(frame)) => screen.child(zoom_frame(
                page,
                "preview-media-frame",
                img(ImageSource::Render(frame.clone()))
                    .size_full()
                    .object_fit(gpui::ObjectFit::Contain)
                    .into_any_element(),
                false,
                cx,
            )),
            (None, None) if !media.peaks.is_empty() => {
                // Stretched across the screen rather than `w_full`: under the
                // screen's `items_center` a percentage width came out 0 and the
                // wave was not drawn at all.
                let mut screen = screen;
                screen.style().align_items = Some(gpui::AlignItems::Stretch);
                screen.child(
                    // Zoomed, the wave grows sideways and scrolls.
                    div()
                        .id("preview-media-wave-scroll")
                        .h(relative(0.5))
                        // Flex, here and in the zoomed strip: in a plain block
                        // the percentage widths below came out as wide as the
                        // wave's own contents, which is nothing (a canvas).
                        .flex()
                        .overflow_x_scroll()
                        .track_scroll(&page.preview_scroll)
                        // The left button seeks; the wheel button drags along
                        // the zoomed wave, as it pans a zoomed video.
                        .on_mouse_down(
                            MouseButton::Middle,
                            cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                                this.preview_pan = Some(event.position);
                                cx.stop_propagation();
                            }),
                        )
                        .on_mouse_up(
                            MouseButton::Middle,
                            cx.listener(|this, _event: &MouseUpEvent, _window, _cx| {
                                this.preview_pan = None
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                            if event.pressed_button == Some(MouseButton::Middle) {
                                this.pan_preview(event.position, cx);
                            } else if this.preview_pan.is_some() && event.pressed_button.is_none() {
                                this.preview_pan = None;
                            }
                        }))
                        .child(
                            div()
                                .h_full()
                                .w(relative(page.preview_zoom.max(1.0)))
                                .flex_shrink_0()
                                .flex()
                                .child(render_waveform(media, cx)),
                        ),
                )
            }
            (None, None) if !media.has_video && media.duration > 0.0 => screen.child(
                Icon::new(Icon::empty())
                    .path("icons/music.svg")
                    .size_16()
                    .text_color(rgb(theme::gray_400())),
            ),
            (None, None) => screen.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::muted()))
                    .child(t!("Carregando…")),
            ),
        });
    let controls = div()
        .flex_shrink_0()
        .h(px(40.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .gap_3()
        .border_t_1()
        .border_color(rgb(theme::border()))
        .child(
            div()
                .id("preview-media-play")
                .size(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .on_click(cx.listener(|this, _event, _window, cx| this.toggle_media(cx)))
                .child(glyph(if media.playing {
                    "\u{e769}"
                } else {
                    "\u{e768}"
                })),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .child(format!(
                    "{} / {}",
                    crate::explorer::media::clock(media.position),
                    crate::explorer::media::clock(media.duration)
                )),
        )
        .child(
            div()
                .id("preview-media-track")
                .flex_1()
                .h(px(20.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .relative()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        let bounds = track_for_click.get();
                        let width = f32::from(bounds.size.width);
                        if width > 0.0 {
                            let x = f32::from(event.position.x - bounds.origin.x);
                            this.seek_media(f64::from(x / width), cx);
                        }
                    }),
                )
                .child(
                    div()
                        .w_full()
                        .h(px(4.0))
                        .rounded_full()
                        .bg(rgb(theme::border()))
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .bg(rgb(theme::accent()))
                                .w(relative(fraction as f32)),
                        ),
                )
                .child(
                    canvas(move |bounds, _, _| track.set(bounds), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                ),
        )
        .child(render_volume(page, glyph, cx));
    div()
        .flex_1()
        .min_h(px(0.0))
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(screen)
        .child(controls)
        .into_any_element()
}

// Width of the volume slider's track.
const VOLUME_TRACK: f32 = 72.0;

// The mute button and a slider beside it; click or drag along the slider,
// or turn the wheel over either, to set the volume.
fn render_volume(
    page: &ExplorerPane,
    glyph: impl Fn(&'static str) -> Div,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let level = if page.media_muted {
        0.0
    } else {
        page.media_volume as f32
    };
    // Segoe Fluent's speaker with no, one, two or three waves.
    let speaker = match level {
        0.0 => "\u{e74f}",
        level if level < 0.34 => "\u{e993}",
        level if level < 0.67 => "\u{e994}",
        _ => "\u{e995}",
    };
    let track = std::rc::Rc::new(std::cell::Cell::new(Bounds::<Pixels>::default()));
    let (track_down, track_move) = (track.clone(), track.clone());
    let set_from = |bounds: Bounds<Pixels>, x: Pixels| {
        let width = f32::from(bounds.size.width);
        (width > 0.0).then(|| f64::from(f32::from(x - bounds.origin.x) / width))
    };
    let wheel = cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
        let delta = f32::from(event.delta.pixel_delta(window.line_height()).y);
        if delta != 0.0 {
            let current = if this.media_muted {
                0.0
            } else {
                this.media_volume
            };
            this.set_media_volume(current + 0.05 * f64::from(delta.signum()), cx);
            cx.stop_propagation();
        }
    });
    div()
        .id("preview-media-volume")
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .on_scroll_wheel(wheel)
        .child(
            div()
                .id("preview-media-mute")
                .size(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
                .on_click(cx.listener(|this, _event, _window, cx| this.toggle_media_mute(cx)))
                .child(glyph(speaker)),
        )
        .child(
            div()
                .id("preview-media-volume-track")
                .w(px(VOLUME_TRACK))
                .h(px(20.0))
                .flex()
                .items_center()
                .cursor_pointer()
                .relative()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        if let Some(volume) = set_from(track_down.get(), event.position.x) {
                            this.set_media_volume(volume, cx);
                        }
                    }),
                )
                .on_mouse_move(
                    cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                        if event.pressed_button == Some(MouseButton::Left)
                            && let Some(volume) = set_from(track_move.get(), event.position.x)
                        {
                            this.set_media_volume(volume, cx);
                        }
                    }),
                )
                .child(
                    div()
                        .w_full()
                        .h(px(4.0))
                        .rounded_full()
                        .bg(rgb(theme::border()))
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .bg(rgb(theme::accent()))
                                .w(relative(level)),
                        ),
                )
                .child(
                    // The knob, centred on the fill's end.
                    div()
                        .absolute()
                        .left(px(level * VOLUME_TRACK - 5.0))
                        .size(px(10.0))
                        .rounded_full()
                        .bg(rgb(theme::fg())),
                )
                .child(
                    canvas(move |bounds, _, _| track.set(bounds), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                ),
        )
        .into_any_element()
}

// An audio file's waveform across the player, the part already played in the
// accent colour. Click or drag on it to seek; it fills in as the file is read.
fn render_waveform(
    media: &crate::explorer::media::MediaPreview,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    // Bar and gap, in logical pixels.
    const BAR: f32 = 2.0;
    const GAP: f32 = 1.0;
    let peaks = media.peaks.clone();
    let (peak_max, duration) = (media.peak_max, media.duration);
    let played = if duration > 0.0 {
        (media.position / duration).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let (played_color, rest_color) = (rgb(theme::accent()), rgb(theme::gray_400()));
    let area = std::rc::Rc::new(std::cell::Cell::new(Bounds::<Pixels>::default()));
    let (area_down, area_move) = (area.clone(), area.clone());
    let fraction_at = |bounds: Bounds<Pixels>, x: Pixels| {
        let width = f32::from(bounds.size.width);
        (width > 0.0).then(|| f64::from(f32::from(x - bounds.origin.x) / width))
    };
    div()
        .id("preview-media-waveform")
        .size_full()
        .px(px(16.0))
        // A press here seeks; it does not reach the screen, whose click plays.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                cx.stop_propagation();
                if let Some(fraction) = fraction_at(area_down.get(), event.position.x) {
                    this.seek_media(fraction, cx);
                }
            }),
        )
        .on_mouse_move(
            cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                if event.pressed_button == Some(MouseButton::Left)
                    && let Some(fraction) = fraction_at(area_move.get(), event.position.x)
                {
                    this.seek_media(fraction, cx);
                }
            }),
        )
        .child(
            canvas(
                move |bounds, _, _| area.set(bounds),
                move |bounds, _, window, _| {
                    let width = f32::from(bounds.size.width);
                    let height = f32::from(bounds.size.height);
                    let count = (width / (BAR + GAP)).floor() as usize;
                    let bars =
                        crate::explorer::media::waveform_bars(&peaks, peak_max, duration, count);
                    for (index, bar) in bars.into_iter().enumerate() {
                        let Some(level) = bar else { continue };
                        let x = index as f32 * (BAR + GAP);
                        // At least a hairline, so silence still shows as a line.
                        let bar_height = (level * height).max(1.0);
                        let color = if x / width < played {
                            played_color
                        } else {
                            rest_color
                        };
                        window.paint_quad(fill(
                            Bounds::new(
                                point(
                                    bounds.origin.x + px(x),
                                    bounds.origin.y + px((height - bar_height) / 2.0),
                                ),
                                size(px(BAR), px(bar_height)),
                            ),
                            color,
                        ));
                    }
                },
            )
            .size_full(),
        )
        .into_any_element()
}

/// Height of one line of a folder or archive listing, at zoom 1.
const LISTING_ROW: f32 = 26.0;

/// Range of the listing's zoom.
pub(crate) const LISTING_ZOOM: (f32, f32) = (0.6, 5.0);

/// From this zoom up the listing is a grid of thumbnails, as the
/// explorer's own icons view.
const LISTING_GRID_AT: f32 = 1.6;

/// Lines of a name under a thumbnail in the listing's grid.
const LISTING_NAME_LINES: f32 = 2.0;

// A folder's contents or an archive's entries: a summary line, then the
// items, virtualized so a folder of thousands lays out only what is on
// screen. A click on an item of a folder opens it in the preview, a folder
// as its own listing; the header's back button returns.
fn render_listing(
    page: &mut ExplorerPane,
    listing: std::rc::Rc<crate::explorer::preview_listing::PreviewListing>,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let zoom = page.preview_listing_zoom;
    let scale = window.scale_factor();
    let width = page.preview_width.clone();
    let measured = width.clone();
    let scroll = page.preview_listing_scroll.clone();
    let list = if zoom >= LISTING_GRID_AT {
        render_listing_grid(listing.clone(), zoom, scale, width.get(), &scroll, cx)
    } else {
        render_listing_rows(listing.clone(), zoom, scale, &scroll, cx)
    };
    div()
        .id("preview-listing-area")
        .flex_1()
        .min_h(px(0.0))
        .flex()
        .flex_col()
        // Here, around the items rather than on each: a press that moves fast
        // leaves its row before it has moved far enough to be a drag.
        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
            this.preview_drag_moved(event, window, cx);
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _event: &MouseUpEvent, _window, _cx| this.release_preview_item()),
        )
        .child(
            div()
                .flex_shrink_0()
                .px(px(16.0))
                .py(px(8.0))
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child(match listing.hidden {
                    0 => listing.summary.clone(),
                    hidden => tf!(
                        "{summary} · mais {hidden} não listados",
                        summary = listing.summary,
                        hidden = hidden
                    ),
                }),
        )
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .px(px(4.0))
                .relative()
                .child(list)
                .child(
                    // The grid's columns follow the pane's width.
                    canvas(
                        move |bounds, window, _| {
                            let now = f32::from(bounds.size.width);
                            if (measured.replace(now) - now).abs() > 0.5 {
                                window.refresh();
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                ),
        )
        .into_any_element()
}

// A press on an item of the listing, which becomes a drag out if the pointer
// moves (`drag::preview_drag_moved`, wired on the listing around the items).
// An archive's entries are not on disk, so there is nothing to hand over.
fn listing_press(
    item: &crate::explorer::preview_listing::ListingItem,
    cx: &mut Context<ExplorerPane>,
) -> Option<impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static> {
    let path = item.path.clone()?;
    Some(
        cx.listener(move |this, event: &MouseDownEvent, _window, _cx| {
            this.press_preview_item(&path, event.position);
        }),
    )
}

// What a click on an item of the listing does: open it in the preview. An
// archive's entries are not on disk, so they have nothing to open.
fn listing_click(
    item: &crate::explorer::preview_listing::ListingItem,
    cx: &mut Context<ExplorerPane>,
) -> Option<impl Fn(&ClickEvent, &mut Window, &mut App) + 'static> {
    let path = item.path.clone()?;
    Some(cx.listener(move |this, _event: &ClickEvent, window, cx| {
        this.preview_into(path.clone(), window, cx);
    }))
}

// The listing as lines: icon, name and size, larger with the zoom.
fn render_listing_rows(
    listing: std::rc::Rc<crate::explorer::preview_listing::PreviewListing>,
    zoom: f32,
    scale: f32,
    scroll: &gpui_component::VirtualListScrollHandle,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let row = (LISTING_ROW * zoom).round();
    let icon = (16.0 * zoom).round();
    let icon_px = (icon * scale).round() as u32;
    let sizes = std::rc::Rc::new(vec![size(px(1.0), px(row)); listing.items.len()]);
    let rows = listing.clone();
    gpui_component::v_virtual_list(
        cx.entity(),
        "preview-listing",
        sizes,
        move |page, range, _window, cx| {
            range
                .filter_map(|ix| {
                    let item = rows.items.get(ix)?.clone();
                    let shell_icon = item
                        .path
                        .as_deref()
                        .and_then(|path| page.thumbnails.icon(path, item.is_dir, icon_px, cx));
                    let click = listing_click(&item, cx);
                    let press = listing_press(&item, cx);
                    Some(
                        div()
                            .id(("preview-listing-row", ix))
                            .when_some(press, |row, press| {
                                row.on_mouse_down(MouseButton::Left, press)
                            })
                            .h(px(row))
                            .w_full()
                            .flex()
                            .items_center()
                            .gap_2()
                            .pl(px(12.0 + 14.0 * zoom * item.depth as f32))
                            .pr(px(12.0))
                            .rounded(px(4.0))
                            .text_size(px(13.0 * zoom.min(2.0)))
                            .hover(|style| style.bg(rgb(theme::bg_hover())))
                            .when_some(click, |row, click| row.cursor_pointer().on_click(click))
                            .child(listing_icon(&item, shell_icon, icon))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .text_color(rgb(theme::fg()))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(item.name.clone()),
                            )
                            .when_some(item.size, |row, bytes| {
                                row.child(
                                    div()
                                        .flex_shrink_0()
                                        .text_xs()
                                        .text_color(rgb(theme::muted()))
                                        .child(crate::explorer::preview_listing::human_size(bytes)),
                                )
                            })
                            .into_any_element(),
                    )
                })
                .collect()
        },
    )
    .track_scroll(scroll)
    .into_any_element()
}

// The listing as a grid: each item's thumbnail (a frame of a video, the
// picture itself, the folder's icon) with its name under it.
fn render_listing_grid(
    listing: std::rc::Rc<crate::explorer::preview_listing::PreviewListing>,
    zoom: f32,
    scale: f32,
    width: f32,
    scroll: &gpui_component::VirtualListScrollHandle,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    const GAP: f32 = 6.0;
    let tile = (48.0 * zoom).round();
    let line = 16.0;
    let cell_w = tile + 2.0 * GAP;
    let cell_h = tile + LISTING_NAME_LINES * line + 3.0 * GAP;
    let columns = (((width - 8.0) / cell_w).floor() as usize).max(1);
    let row_count = listing.items.len().div_ceil(columns);
    let thumb_px = (tile * scale).round() as u32;
    let sizes = std::rc::Rc::new(vec![size(px(1.0), px(cell_h)); row_count]);
    let rows = listing.clone();
    gpui_component::v_virtual_list(
        cx.entity(),
        "preview-listing-grid",
        sizes,
        move |page, range, _window, cx| {
            range
                .map(|row_ix| {
                    let start = row_ix * columns;
                    let end = (start + columns).min(rows.items.len());
                    let cells = (start..end).filter_map(|ix| {
                        let item = rows.items.get(ix)?.clone();
                        // Sound files with waveforms on, as in the icons view:
                        // the type's icon until the waveform arrives.
                        let waveform_wanted = page.audio_waveforms
                            && !item.is_dir
                            && item
                                .path
                                .as_deref()
                                .is_some_and(crate::explorer::waveforms::is_audio);
                        let waveform = match (&item.path, waveform_wanted) {
                            (Some(path), true) => {
                                page.waveforms
                                    .get(path, item.size.unwrap_or(0), item.modified, cx)
                            }
                            _ => None,
                        };
                        let image = item
                            .path
                            .as_deref()
                            .filter(|_| waveform.is_none())
                            .and_then(|path| {
                                if item.is_dir {
                                    page.thumbnails.icon(path, true, thumb_px, cx)
                                } else if waveform_wanted {
                                    page.thumbnails.icon(path, false, thumb_px, cx)
                                } else {
                                    page.thumbnails.get(path, thumb_px, cx)
                                }
                            });
                        let click = listing_click(&item, cx);
                        let press = listing_press(&item, cx);
                        Some(
                            div()
                                .id(("preview-listing-cell", ix))
                                .when_some(press, |cell, press| {
                                    cell.on_mouse_down(MouseButton::Left, press)
                                })
                                .w(px(cell_w))
                                .h(px(cell_h))
                                .flex_shrink_0()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap(px(GAP))
                                .p(px(GAP))
                                .rounded(px(6.0))
                                .hover(|style| style.bg(rgb(theme::bg_hover())))
                                .when_some(click, |cell, click| {
                                    cell.cursor_pointer().on_click(click)
                                })
                                .child(
                                    div()
                                        .size(px(tile))
                                        .flex_shrink_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(match (waveform, image) {
                                            (Some(bars), _) => crate::explorer::waveforms::canvas(
                                                bars, tile, false, None,
                                            ),
                                            (None, Some(image)) => img(ImageSource::Render(image))
                                                .size_full()
                                                .object_fit(gpui::ObjectFit::Contain)
                                                .into_any_element(),
                                            (None, None) => listing_icon(&item, None, tile * 0.6),
                                        }),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .h(px(LISTING_NAME_LINES * line))
                                        .overflow_hidden()
                                        .text_xs()
                                        .line_height(px(line))
                                        .text_center()
                                        .text_color(rgb(theme::fg()))
                                        .child(item.name.clone()),
                                ),
                        )
                    });
                    div()
                        .id(("preview-listing-grid-row", row_ix))
                        .h(px(cell_h))
                        .w_full()
                        .flex()
                        .children(cells)
                        .into_any_element()
                })
                .collect()
        },
    )
    .track_scroll(scroll)
    .into_any_element()
}

// An item's icon: the Shell's when it has arrived, a drawn one before.
fn listing_icon(
    item: &crate::explorer::preview_listing::ListingItem,
    shell_icon: Option<std::sync::Arc<RenderImage>>,
    edge: f32,
) -> AnyElement {
    match shell_icon {
        Some(image) => img(ImageSource::Render(image))
            .size(px(edge))
            .flex_shrink_0()
            .into_any_element(),
        None => Icon::new(if item.is_dir {
            IconName::Folder
        } else {
            IconName::File
        })
        .size(px(edge))
        .flex_shrink_0()
        .text_color(rgb(if item.is_dir {
            0xE8B339
        } else {
            theme::gray_500()
        }))
        .into_any_element(),
    }
}

fn path_name(p: &str) -> String {
    std::path::Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string())
}

/// The text editor used to render file previews.
pub mod editor;

#[cfg(test)]
mod zoom_anchor_tests {
    use super::anchored_offset;
    use gpui::{point, px, size};

    // The content point under the anchor before the zoom is under it after.
    #[test]
    fn the_point_under_the_mouse_stays_under_it() {
        let view = size(px(400.0), px(300.0));
        for (offset, anchor, old, new) in [
            (
                point(px(0.0), px(0.0)),
                point(px(100.0), px(50.0)),
                1.0,
                2.0,
            ),
            (
                point(px(-120.0), px(-40.0)),
                point(px(300.0), px(200.0)),
                2.0,
                3.0,
            ),
            (
                point(px(-500.0), px(-300.0)),
                point(px(10.0), px(290.0)),
                3.0,
                2.4,
            ),
        ] {
            let after = anchored_offset(offset, view, anchor, old, new);
            for (offset, after, anchor) in
                [(offset.x, after.x, anchor.x), (offset.y, after.y, anchor.y)]
            {
                let before = (f32::from(anchor) - f32::from(offset)) / old;
                let now = (f32::from(anchor) - f32::from(after)) / new;
                assert!((before - now).abs() < 0.01, "{before} against {now}");
            }
        }
    }

    // Zooming in at a corner cannot scroll past the content, and back at 1
    // nothing is scrolled.
    #[test]
    fn the_offset_stays_inside_the_content() {
        let view = size(px(400.0), px(300.0));
        let corner = anchored_offset(
            point(px(0.0), px(0.0)),
            view,
            point(px(400.0), px(300.0)),
            1.0,
            2.0,
        );
        assert_eq!(corner, point(px(-400.0), px(-300.0)));
        let back = anchored_offset(
            point(px(-400.0), px(-300.0)),
            view,
            point(px(0.0), px(0.0)),
            2.0,
            1.0,
        );
        assert_eq!(back, point(px(0.0), px(0.0)));
    }
}
