//! What the space map draws: the bar above the mosaic, the tiles, and their
//! labels.
//!
//! The tiles are painted rather than built out of elements: a mosaic is
//! thousands of rectangles, and one element each would spend the frame in
//! layout. Their names are shaped straight into the canvas too, clipped to the
//! tile they belong to, so a long name cannot bleed into its neighbour.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::Icon;
use gpui_component::tooltip::Tooltip;
use nohrs_core::config::{SpaceMap, SpaceMapColor};
use nohrs_core::i18n;
use nohrs_core::{k, t};
use nohrs_services::fs::space::{SpaceNode, Tile};
use nohrs_ui::theme::theme;

use crate::explorer::ExplorerPane;
use crate::explorer::filter::KindFilter;
use crate::explorer::places::THIS_PC;
use crate::explorer::preview_listing::human_size;
use crate::explorer::space_map::SpaceSource;

/// Height of the row above the mosaic.
const BAR_HEIGHT: f32 = 36.0;

/// Depth steps a fill distinguishes; deeper clamps.
const DEPTHS: usize = 5;

/// The map fills the listing's place: the bar that says what is measured and
/// where, then the mosaic.
pub fn render(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    if page.space.is_none() {
        return div().into_any_element();
    }
    let error = page.space.as_ref().and_then(|map| map.error.clone());
    let still_scanning = page.space.as_ref().is_some_and(|map| map.is_scanning());
    // A folder with nothing in it has no mosaic to draw.
    let empty = page
        .space
        .as_ref()
        .is_some_and(|map| map.focus_node().is_some_and(|node| node.bytes == 0));
    let bar = bar(page, cx);
    let body: AnyElement = match (error, still_scanning) {
        (Some(reason), _) => notice(
            t!("Não foi possível analisar a pasta"),
            Some(reason),
            theme::danger(),
        ),
        (None, true) => scanning_body(page),
        (None, false) if empty => notice(t!("Pasta vazia"), None, theme::muted()),
        (None, false) => mosaic(page, window, cx),
    };
    div()
        .id("space-map")
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::bg()))
        .child(bar)
        .child(body)
        .into_any_element()
}

/// The row above the mosaic: going back out of a folder, the path inside the
/// tree, and what the tiles weigh — or what the tile under the pointer is.
fn bar(page: &mut ExplorerPane, cx: &mut Context<ExplorerPane>) -> AnyElement {
    let Some(map) = page.space.as_ref() else {
        return div().into_any_element();
    };
    let focus = map.focus.clone();
    let tree = map.tree.clone();
    // The names down the zoom path, root first: the breadcrumb.
    let mut names: Vec<String> = vec![display_name(&map.root)];
    if let Some(tree) = tree.as_deref() {
        names[0] = if tree.name.is_empty() {
            display_name(&map.root)
        } else {
            tree.name.clone()
        };
        for length in 1..=focus.len() {
            if let Some(node) = tree.node_at(&focus[..length]) {
                names.push(if node.name.is_empty() {
                    i18n::plural(node.count as usize, k!("{n} item"), k!("{n} itens"))
                } else {
                    node.name.clone()
                });
            }
        }
    }
    let status = status_text(page);
    let status_tooltip = status.clone();
    let scanning = page.space.as_ref().is_some_and(|map| map.is_scanning());
    // The figures came from the indexer's memory, not from a walk of the
    // folder: worth saying, since its sizes are what the volume allocates.
    let indexed = page.space.as_ref().and_then(|map| map.source) == Some(SpaceSource::Index);
    let style = page.space_map_style.clone();
    let can_go_up = !focus.is_empty();
    let drives = page
        .space
        .as_ref()
        .is_some_and(|map| map.root.eq_ignore_ascii_case(THIS_PC));

    let mut crumbs = div()
        .flex()
        .items_center()
        .gap(px(2.0))
        .min_w(px(0.0))
        .overflow_hidden();
    for (index, name) in names.iter().enumerate() {
        let last = index + 1 == names.len();
        if index > 0 {
            crumbs = crumbs.child(
                Icon::new(Icon::empty())
                    .path(SharedString::from("icons/chevron-right.svg"))
                    .size_3()
                    .text_color(rgb(theme::muted())),
            );
        }
        let crumbs_here: Vec<usize> = focus[..index.min(focus.len())].to_vec();
        let clickable = !last && tree.is_some();
        let row = div()
            .id(SharedString::from(format!("space-crumb-{index}")))
            .h(px(22.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .rounded(px(4.0))
            .max_w(px(220.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_sm()
            .when(last, |this| {
                this.font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::fg()))
            })
            .when(!last, |this| {
                this.text_color(rgb(theme::fg_secondary()))
                    .when(clickable, |this| {
                        this.cursor_pointer()
                            .hover(|style| style.bg(rgb(theme::bg_hover())))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.space_zoom_to(crumbs_here.clone(), cx);
                            }))
                    })
            })
            .child(name.clone());
        crumbs = crumbs.child(row);
    }

    let mut right = div().flex().items_center().gap(px(6.0)).flex_shrink_0();
    if indexed {
        right = right.child(
            div()
                .flex_shrink_0()
                .h(px(18.0))
                .px(px(6.0))
                .flex()
                .items_center()
                .rounded(px(4.0))
                .bg(rgb(theme::accent_subtle()))
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .child(t!("Índice")),
        );
    }
    right = right.child(
        div()
            .id("space-status")
            .max_w(px(420.0))
            .min_w(px(0.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_xs()
            .text_color(rgb(if scanning {
                theme::warning()
            } else {
                theme::fg_secondary()
            }))
            .child(status)
            .when(!status_tooltip.is_empty(), |this| {
                this.tooltip(move |window, cx| {
                    Tooltip::new(status_tooltip.clone()).build(window, cx)
                })
            }),
    );
    // A drive's mosaic is made of the shell's own figures, not of a walk: it
    // has nothing to measure again.
    let refreshable = !drives && tree.is_some();
    right = right
        .child(
            bar_icon(
                "space-refresh",
                Icon::new(Icon::empty()).path(SharedString::from("icons/refresh-cw.svg")),
                refreshable,
            )
            .when(refreshable, |this| {
                this.tooltip(|window, cx| Tooltip::new(t!("Atualizar")).build(window, cx))
                    .on_click(cx.listener(|this, _, _, cx| this.space_refresh(cx)))
            }),
        )
        .child(
            bar_icon(
                "space-close",
                Icon::new(Icon::empty()).path(SharedString::from("icons/close.svg")),
                true,
            )
            .tooltip(|window, cx| Tooltip::new(t!("Fechar")).build(window, cx))
            .on_click(cx.listener(|this, _, _, cx| this.close_space_map(cx))),
        );

    let up = bar_icon(
        "space-up",
        Icon::new(Icon::empty()).path(SharedString::from("icons/chevron-left.svg")),
        can_go_up,
    );
    let up = if can_go_up {
        up.tooltip(|window, cx| Tooltip::new(t!("Subir para a pasta de cima")).build(window, cx))
            .on_click(cx.listener(|this, _, _, cx| this.space_zoom_out(cx)))
    } else {
        up
    };

    div()
        .id("space-bar")
        .flex_shrink_0()
        .w_full()
        .h(px(BAR_HEIGHT))
        .px(px(8.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .bg(rgb(theme::footer_bg()))
        .border_b_1()
        .border_color(rgb(theme::border()))
        .child(up)
        .child(crumbs)
        .child(div().flex_1().min_w(px(0.0)))
        .when(style.color == SpaceMapColor::Size, |this| {
            this.child(size_legend(&style))
        })
        .child(right)
        .into_any_element()
}

/// What the colours mean: one swatch per size band, as the configured palette
/// draws them.
fn size_legend(style: &SpaceMap) -> impl IntoElement {
    let bands = style.bands_gb;
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .flex_shrink_0()
        .children((0..4).map(move |band| {
            let color: Hsla = style
                .band_color(band)
                .map_or_else(|| rgb(theme::muted()), rgb)
                .into();
            let label = if band < 3 {
                format!("≥{} GB", bands[band])
            } else {
                format!("<{} GB", bands[2])
            };
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .text_xs()
                .text_color(rgb(theme::fg_secondary()))
                .child(div().size(px(10.0)).rounded(px(2.0)).bg(color))
                .child(label)
        }))
}

/// A square icon button of the bar.
fn bar_icon(id: &'static str, icon: Icon, enabled: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.0))
        .text_color(rgb(if enabled {
            theme::fg_secondary()
        } else {
            theme::muted()
        }))
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|style| style.bg(rgb(theme::bg_hover())))
        })
        .child(icon.size_4())
}

/// What the bar says on the right: the tile under the pointer, else the
/// selected tile, else what the mosaic is made of.
fn status_text(page: &ExplorerPane) -> String {
    let Some(map) = page.space.as_ref() else {
        return String::new();
    };
    if let Some(error) = &map.error {
        return error.clone();
    }
    if map.is_scanning() {
        let snapshot = map.progress.snapshot();
        return format!(
            "{} {} · {}",
            t!("Analisando…"),
            i18n::plural(
                snapshot.files as usize,
                k!("{n} arquivo"),
                k!("{n} arquivos")
            ),
            human_size(snapshot.bytes)
        );
    }
    let Some(tree) = map.tree.as_deref() else {
        return String::new();
    };
    let Some(focus_node) = tree.node_at(&map.focus) else {
        return String::new();
    };
    let hovered = map
        .layout
        .as_ref()
        .zip(map.hover)
        .and_then(|(layout, index)| layout.tiles.get(index))
        .and_then(|tile| focus_node.node_at(&tile.crumbs));
    let selected = map
        .selection
        .as_deref()
        .and_then(|crumbs| tree.node_at(crumbs));
    if let Some(node) = hovered.or(selected) {
        let share = if focus_node.bytes == 0 {
            0.0
        } else {
            node.bytes as f64 / focus_node.bytes as f64 * 100.0
        };
        let name = if node.name.is_empty() {
            i18n::plural(node.count as usize, k!("{n} item"), k!("{n} itens"))
        } else {
            node.name.clone()
        };
        let path = if node.path.is_empty() {
            String::new()
        } else {
            format!(" · {}", node.path)
        };
        return format!("{name} · {} · {share:.1}%{path}", human_size(node.bytes));
    }
    if focus_node.bytes == 0 {
        return t!("Pasta vazia").to_string();
    }
    let total = tree.bytes;
    let share = if total == 0 {
        100.0
    } else {
        focus_node.bytes as f64 / total as f64 * 100.0
    };
    if map.focus.is_empty() {
        human_size(focus_node.bytes)
    } else {
        format!("{} · {share:.1}%", human_size(focus_node.bytes))
    }
}

/// The message a map that could not measure its folder shows instead of a
/// mosaic.
fn notice(title: &str, detail: Option<String>, color: u32) -> AnyElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.0))
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(color))
                .child(title.to_string()),
        )
        .children(detail.map(|detail| {
            div()
                .max_w(px(560.0))
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child(detail)
                .into_any_element()
        }))
        .into_any_element()
}

/// The map while the walk is still running: what has been counted so far.
fn scanning_body(page: &ExplorerPane) -> AnyElement {
    let Some(map) = page.space.as_ref() else {
        return div().into_any_element();
    };
    let snapshot = map.progress.snapshot();
    let seconds = map.started.elapsed().as_secs();
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .child(
            div()
                .text_sm()
                .text_color(rgb(theme::fg_secondary()))
                .child(t!("Analisando…").to_string()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::muted()))
                .child(format!(
                    "{} · {} · {seconds}s",
                    i18n::plural(
                        snapshot.files as usize,
                        k!("{n} arquivo"),
                        k!("{n} arquivos")
                    ),
                    human_size(snapshot.bytes)
                )),
        )
        .into_any_element()
}

/// The mosaic itself: the selected tile stays outlined and the pointer's tile
/// gets a temporary outline.
fn mosaic(
    page: &mut ExplorerPane,
    window: &mut Window,
    cx: &mut Context<ExplorerPane>,
) -> AnyElement {
    let (viewport, tree, focus, hovered, selection) = {
        let Some(map) = page.space.as_ref() else {
            return div().into_any_element();
        };
        (
            Rc::clone(&map.viewport),
            map.tree.clone(),
            map.focus.clone(),
            map.hover,
            map.selection.clone(),
        )
    };
    let measured = viewport.get();
    let width = f32::from(measured.size.width).max(0.0);
    let height = f32::from(measured.size.height).max(0.0);
    let tiles = page
        .space_tiles(width, height)
        .unwrap_or_else(|| Rc::new(Vec::new()));
    let selected = selection.as_ref().and_then(|crumbs| {
        tiles
            .iter()
            .position(|tile| tile.crumbs.as_slice() == crumbs.as_slice())
    });
    let dark = theme::is_dark();
    let style = page.space_map_style.clone();
    let rem = window.rem_size();
    // Labels are the map's reading: larger than the surrounding chrome, so a
    // mosaic reads at arm's length.
    let name_size = rem * 0.84;
    let size_size = rem * 0.74;
    let font = window.text_style().font();

    let move_viewport = Rc::clone(&viewport);
    let press_viewport = Rc::clone(&viewport);
    let paint_tiles_rc = Rc::clone(&tiles);
    let paint_selection = selected;
    let paint_tree = tree.clone();
    let paint_focus = focus.clone();
    let paint_style = style.clone();
    div()
        .id("space-mosaic")
        .relative()
        .flex_1()
        .min_h(px(0.0))
        .min_w(px(0.0))
        .overflow_hidden()
        .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
            let Some(map) = this.space.as_mut() else {
                return;
            };
            if !*hovered && map.hover.take().is_some() {
                cx.notify();
            }
        }))
        .on_mouse_move(
            cx.listener(move |this, event: &MouseMoveEvent, _window, cx| {
                let Some(map) = this.space.as_mut() else {
                    return;
                };
                let bounds = move_viewport.get();
                let hit = map.layout.as_ref().and_then(|layout| {
                    let x = f32::from(event.position.x - bounds.origin.x);
                    let y = f32::from(event.position.y - bounds.origin.y);
                    layout
                        .tiles
                        .iter()
                        .rposition(|tile| tile.rect.contains(x, y))
                });
                if map.hover != hit {
                    map.hover = hit;
                    cx.notify();
                }
            }),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                let Some(map) = this.space.as_ref() else {
                    return;
                };
                let bounds = press_viewport.get();
                let crumbs = map.layout.as_ref().and_then(|layout| {
                    let x = f32::from(event.position.x - bounds.origin.x);
                    let y = f32::from(event.position.y - bounds.origin.y);
                    layout
                        .tiles
                        .iter()
                        .rev()
                        .find(|tile| tile.rect.contains(x, y))
                        .map(|tile| tile.crumbs.clone())
                });
                if event.click_count > 1 {
                    if let Some(crumbs) = crumbs {
                        this.space_click(crumbs, window, cx);
                    }
                } else {
                    this.space_select(crumbs, cx);
                }
            }),
        )
        .child(
            canvas(
                move |bounds, window, _cx| {
                    if viewport.get() != bounds {
                        viewport.set(bounds);
                        // The layout was computed for the area of the previous
                        // frame; one more frame draws it for this one.
                        window.request_animation_frame();
                    }
                },
                move |bounds, (), window, cx| {
                    let Some(tree) = paint_tree.as_deref() else {
                        return;
                    };
                    let Some(focus_node) = tree.node_at(&paint_focus) else {
                        return;
                    };
                    paint_mosaic(
                        &paint_tiles_rc,
                        focus_node,
                        &paint_style,
                        bounds,
                        paint_selection,
                        hovered,
                        dark,
                        window,
                    );
                    paint_labels(
                        &paint_tiles_rc,
                        focus_node,
                        &paint_style,
                        bounds,
                        &font,
                        name_size,
                        size_size,
                        window,
                        cx,
                    );
                },
            )
            .absolute()
            .inset_0(),
        )
        .into_any_element()
}

/// The kinds a tile's colour stands for: what took the space.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Folder,
    Images,
    Videos,
    Audio,
    Documents,
    Archives,
    Programs,
    Other,
}

impl Kind {
    fn of(node: &SpaceNode) -> Self {
        if node.is_dir {
            return Self::Folder;
        }
        // A tile that stands for several entries has no name to classify.
        if node.name.is_empty() {
            return Self::Other;
        }
        match KindFilter::of_name(&node.name) {
            KindFilter::Images => Self::Images,
            KindFilter::Videos => Self::Videos,
            KindFilter::Audio => Self::Audio,
            KindFilter::Documents => Self::Documents,
            KindFilter::Archives => Self::Archives,
            KindFilter::Programs => Self::Programs,
            _ => Self::Other,
        }
    }

    /// Hue in turns, 0..=1: warm for pictures and archives, cool for what is
    /// read, green for what runs.
    fn hue(self) -> f32 {
        match self {
            Self::Folder => 0.53,
            Self::Images => 0.09,
            Self::Videos => 0.92,
            Self::Audio => 0.75,
            Self::Documents => 0.59,
            Self::Archives => 0.04,
            Self::Programs => 0.35,
            Self::Other => 0.0,
        }
    }

    fn saturation(self, dark: bool) -> f32 {
        let base = match self {
            Self::Folder => 0.34,
            Self::Other => 0.04,
            _ => 0.52,
        };
        if dark { base } else { base * 0.9 }
    }
}

/// The fill of a tile: by how much it weighs (the configured bands), or by
/// what it holds. Each level of nesting steps a little away from the surface
/// so the mosaic reads as layers rather than as a flat stain.
fn tile_fill(style: &SpaceMap, node: &SpaceNode, depth: u32, dark: bool, header: bool) -> Hsla {
    match style.color {
        SpaceMapColor::Kind => kind_fill(Kind::of(node), depth, dark, header),
        SpaceMapColor::Size => {
            let base: Hsla = style
                .color_for_bytes(node.bytes)
                .map_or_else(|| rgb(theme::muted()), rgb)
                .into();
            hsla(base.h, base.s, shade(base.l, depth, dark, header), 1.0)
        }
    }
}

// The kind's hue, lighter or darker for each level of nesting.
fn kind_fill(kind: Kind, depth: u32, dark: bool, header: bool) -> Hsla {
    let step = depth.min(DEPTHS as u32 - 1) as f32;
    let shift = if header { 0.05 } else { 0.0 };
    if dark {
        hsla(
            kind.hue(),
            kind.saturation(true),
            (0.32 + step * 0.035 + shift).min(0.62),
            1.0,
        )
    } else {
        hsla(
            kind.hue(),
            kind.saturation(false),
            (0.80 - step * 0.030 - shift).max(0.42),
            1.0,
        )
    }
}

// A step toward the surface: lighter under a dark palette, darker under a
// light one.
fn shade(lightness: f32, depth: u32, dark: bool, header: bool) -> f32 {
    let step = depth.min(DEPTHS as u32 - 1) as f32;
    let shift = if header { 0.05 } else { 0.0 };
    if dark {
        (lightness + step * 0.04 + shift).min(0.86)
    } else {
        (lightness - step * 0.035 - shift).max(0.14)
    }
}

/// The name and the weight colours that read on `fill`: near-white on a dark
/// tile, near-black on a light one. A tile can be either, since the palette is
/// the user's.
fn text_on(fill: Hsla) -> (Hsla, Hsla) {
    let color = Rgba::from(fill);
    let luminance = 0.2126 * color.r + 0.7152 * color.g + 0.0722 * color.b;
    if luminance < 0.55 {
        (rgb(0xf8fafc).into(), rgb(0xd7dee8).into())
    } else {
        (rgb(0x0f172a).into(), rgb(0x3b4658).into())
    }
}

/// Paints the rectangles: every fill first in the order the layout gave them
/// (parents before their children), then the hovered tile's ring on top.
fn paint_mosaic(
    tiles: &[Tile],
    focus_node: &SpaceNode,
    style: &SpaceMap,
    bounds: Bounds<Pixels>,
    selected: Option<usize>,
    hovered: Option<usize>,
    dark: bool,
    window: &mut Window,
) {
    let origin_x = f32::from(bounds.origin.x);
    let origin_y = f32::from(bounds.origin.y);
    let scale = window.scale_factor();
    let mut selected_ring: Option<Bounds<Pixels>> = None;
    let mut hover_ring: Option<Bounds<Pixels>> = None;
    for (index, tile) in tiles.iter().enumerate() {
        if tile.rect.w < 0.5 || tile.rect.h < 0.5 {
            continue;
        }
        let Some(node) = focus_node.node_at(&tile.crumbs) else {
            continue;
        };
        let quad_bounds = snap(
            Bounds::new(
                point(px(origin_x + tile.rect.x), px(origin_y + tile.rect.y)),
                size(px(tile.rect.w), px(tile.rect.h)),
            ),
            scale,
        );
        window.paint_quad(fill(quad_bounds, tile_fill(style, node, tile.depth, dark, false)));
        if let Some(header) = tile.header {
            let band = snap(
                Bounds::new(
                    point(px(origin_x + header.x), px(origin_y + header.y)),
                    size(px(header.w), px(header.h)),
                ),
                scale,
            );
            window.paint_quad(fill(band, tile_fill(style, node, tile.depth, dark, true)));
        }
        if selected == Some(index) {
            selected_ring = Some(quad_bounds);
        }
        if hovered == Some(index) && hovered != selected {
            hover_ring = Some(quad_bounds);
        }
    }
    if let Some(ring) = selected_ring {
        window.paint_quad(quad(
            ring,
            Corners::<Pixels>::default(),
            transparent_black(),
            Edges::all(px(2.0)),
            rgb(theme::accent()),
            BorderStyle::Solid,
        ));
    }
    if let Some(ring) = hover_ring {
        window.paint_quad(quad(
            ring,
            Corners::<Pixels>::default(),
            transparent_black(),
            Edges::all(px(1.0)),
            rgb(theme::fg_secondary()),
            BorderStyle::Solid,
        ));
    }
}

/// Rounds a rectangle's edges to whole device pixels. Tiles land on fractional
/// positions, and a ring drawn there smears across a pixel row; rounding each
/// edge keeps neighbours flush.
fn snap(bounds: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
    let round = |value: Pixels| px((f32::from(value) * scale).round() / scale);
    let left = round(bounds.origin.x);
    let top = round(bounds.origin.y);
    let right = round(bounds.origin.x + bounds.size.width);
    let bottom = round(bounds.origin.y + bounds.size.height);
    Bounds::new(
        point(left, top),
        size((right - left).max(px(0.0)), (bottom - top).max(px(0.0))),
    )
}

/// A tile's name and weight, painted inside the tile that owns them.
#[expect(
    clippy::too_many_arguments,
    reason = "the paint pass threads the window and the fonts; a struct would
              only move these fields somewhere else"
)]
fn paint_labels(
    tiles: &[Tile],
    focus_node: &SpaceNode,
    style: &SpaceMap,
    bounds: Bounds<Pixels>,
    font: &Font,
    name_size: Pixels,
    size_size: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let origin_x = f32::from(bounds.origin.x);
    let origin_y = f32::from(bounds.origin.y);
    let name_px = f32::from(name_size);
    let size_px = f32::from(size_size);
    let line_height = name_px * 1.32;
    let padding = name_px * 0.45;
    let dark = theme::is_dark();
    for tile in tiles {
        let Some(node) = focus_node.node_at(&tile.crumbs) else {
            continue;
        };
        // A subdivided directory keeps its name in the band it reserved; a
        // leaf's sits at the top of its own tile.
        let owned = tile.header.unwrap_or(tile.rect);
        if owned.w < name_px * 3.0 || owned.h < line_height {
            continue;
        }
        // The text has to read on the tile's own colour, whatever the palette.
        let (name_color, size_color) =
            text_on(tile_fill(style, node, tile.depth, dark, tile.header.is_some()));
        let label_x = origin_x + owned.x + padding;
        let label_y = origin_y + owned.y + (owned.h.min(line_height * 1.6) - line_height) * 0.5;
        let max_name_width = (owned.w - padding * 2.0 - size_px).max(name_px * 2.0);
        let name = if node.name.is_empty() {
            i18n::plural(node.count as usize, k!("{n} item"), k!("{n} itens"))
        } else {
            node.name.clone()
        };
        // The first level is set in semibold: it names a region, not an entry.
        let weight = if tile.depth == 0 && tile.header.is_some() {
            FontWeight::SEMIBOLD
        } else if tile.depth == 0 {
            FontWeight::MEDIUM
        } else {
            FontWeight::NORMAL
        };
        let name_line = fit_line(
            window.text_system(),
            &name,
            font.clone(),
            name_size,
            name_color,
            max_name_width,
            weight,
        );
        if let Err(error) =
            name_line.paint(point(px(label_x), px(label_y)), px(line_height), window, cx)
        {
            tracing::debug!("painting a space-map label: {error}");
        }

        // The weight: at the far end of a band, under the name on a tall tile,
        // or after the name when there is room on one line.
        let size_text = human_size(node.bytes);
        let size_run = TextRun {
            len: size_text.len(),
            font: font.clone(),
            color: size_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let size_line = window.text_system().shape_line(
            SharedString::from(size_text),
            size_size,
            &[size_run],
            None,
        );
        let name_width = f32::from(name_line.width);
        let size_width = f32::from(size_line.width);
        let size_origin = if tile.header.is_some() {
            let right = origin_x + owned.x + owned.w - padding - size_width;
            if right <= label_x + name_width + padding * 0.5 {
                continue;
            }
            point(px(right), px(label_y + (name_px - size_px) * 0.35))
        } else if owned.h >= line_height * 2.0 + name_px * 0.4 {
            point(px(label_x), px(label_y + line_height * 0.95))
        } else {
            let left = label_x + name_width + padding * 0.6;
            if origin_x + owned.x + owned.w - padding - left < size_width {
                continue;
            }
            point(px(left), px(label_y + (name_px - size_px) * 0.35))
        };
        if let Err(error) = size_line.paint(size_origin, px(line_height), window, cx) {
            tracing::debug!("painting a space-map size: {error}");
        }
    }
}

/// `text` shaped for one line, shortened with an ellipsis until it fits
/// `max_width`. A label that ran past its tile would be clipped mid-glyph.
fn fit_line(
    text_system: &WindowTextSystem,
    text: &str,
    font: Font,
    font_size: Pixels,
    color: Hsla,
    max_width: f32,
    weight: FontWeight,
) -> ShapedLine {
    let run = |len: usize| TextRun {
        len,
        font: Font {
            weight,
            ..font.clone()
        },
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let whole = text_system.shape_line(
        SharedString::from(text.to_string()),
        font_size,
        &[run(text.len())],
        None,
    );
    if f32::from(whole.width) <= max_width || text.chars().count() < 2 {
        return whole;
    }
    let chars: Vec<char> = text.chars().collect();
    let average = f32::from(whole.width) / chars.len().max(1) as f32;
    let mut keep = ((max_width / average).floor() as usize)
        .saturating_sub(1)
        .min(chars.len().saturating_sub(1));
    while keep > 1 {
        let mut shortened: String = chars[..keep].iter().collect();
        shortened.push('…');
        let line = text_system.shape_line(
            SharedString::from(shortened.clone()),
            font_size,
            &[run(shortened.len())],
            None,
        );
        if f32::from(line.width) <= max_width {
            return line;
        }
        keep = (keep * 8 / 10).max(1);
    }
    text_system.shape_line(
        SharedString::from("…".to_string()),
        font_size,
        &[run("…".len())],
        None,
    )
}

/// The name of a location for the bar, when there is no scanned tree yet.
fn display_name(path: &str) -> String {
    if path.eq_ignore_ascii_case(THIS_PC) {
        return t!("Este Computador").to_string();
    }
    let trimmed = path.trim_end_matches(['\\', '/']);
    trimmed
        .rsplit(['\\', '/'])
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| path.to_string())
}
