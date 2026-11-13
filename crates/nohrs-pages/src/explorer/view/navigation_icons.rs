use gpui::prelude::*;
use gpui::*;

use crate::explorer::places::{Place, THIS_PC};
use crate::explorer::tags::Tag;

const ICON_SIZE: f32 = 16.0;
const FOLDER_COLOR: u32 = 0xFBBF24;
const SYSTEM_COLOR: u32 = 0x38BDF8;

pub(super) fn workspace(tag: &Tag) -> Div {
    div()
        .size(px(20.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .bg(rgba((tag.color << 8) | 0x1A))
        .child(svg().path(tag.icon().path()).size(px(14.0)).text_color(rgb(tag.color)))
}

pub(super) fn place(place: &Place) -> AnyElement {
    if place.parsing_name.eq_ignore_ascii_case(THIS_PC) {
        return glyph("icons/aura-monitor.svg", SYSTEM_COLOR);
    }
    if place.parsing_name.eq_ignore_ascii_case(crate::explorer::network::NETWORK) {
        return glyph("icons/aura-network.svg", SYSTEM_COLOR);
    }
    path(place.path.as_deref().unwrap_or(&place.parsing_name))
}

pub(super) fn path(path: &str) -> AnyElement {
    let (asset, color) = path_style(path);
    glyph(asset, color)
}

pub(super) fn glyph(asset: &'static str, color: u32) -> AnyElement {
    svg().path(asset).size(px(ICON_SIZE)).flex_shrink_0().text_color(rgb(color)).into_any_element()
}

fn path_style(path: &str) -> (&'static str, u32) {
    let path = path.trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase();
    if path.len() == 2 && path.as_bytes().get(1) == Some(&b':') {
        let color = if path == "c:" { SYSTEM_COLOR } else { FOLDER_COLOR };
        return ("icons/aura-hard-drive.svg", color);
    }
    if path.starts_with("\\\\") {
        return ("icons/aura-server.svg", SYSTEM_COLOR);
    }
    let name = path.rsplit('\\').next().unwrap_or_default();
    match name {
        "desktop" => ("icons/aura-monitor.svg", SYSTEM_COLOR),
        "downloads" => ("icons/aura-download.svg", 0x34D399),
        "documents" => ("icons/aura-file-text.svg", SYSTEM_COLOR),
        "pictures" => ("icons/aura-image.svg", 0x34D399),
        "music" => ("icons/aura-music.svg", 0xA78BFA),
        "videos" => ("icons/aura-video.svg", 0xFB7185),
        _ if path.contains("645ff040-5081-101b-9f08-00aa002f954e") =>
            ("icons/aura-trash-2.svg", 0x94A3B8),
        _ => ("icons/aura-folder.svg", FOLDER_COLOR),
    }
}

#[cfg(test)]
mod tests {
    use super::{FOLDER_COLOR, SYSTEM_COLOR, path_style};

    #[test]
    fn navigation_paths_handle_roots_shares_and_folder_names() {
        assert_eq!(("icons/aura-hard-drive.svg", SYSTEM_COLOR), path_style("C:\\"));
        assert_eq!(("icons/aura-hard-drive.svg", FOLDER_COLOR), path_style("D:/"));
        assert_eq!(("icons/aura-server.svg", SYSTEM_COLOR), path_style("\\\\server\\share"));
        assert_eq!(("icons/aura-download.svg", 0x34D399), path_style("C:/Users/BoBO/Downloads/"));
        assert_eq!(("icons/aura-folder.svg", FOLDER_COLOR), path_style("D:/Projects"));
    }
}
