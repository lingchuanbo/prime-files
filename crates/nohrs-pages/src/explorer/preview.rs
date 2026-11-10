use nohrs_core::t;
use nohrs_core::config;
use nohrs_core::telemetry::LogErr as _;

use gpui::{AppContext, AsyncWindowContext, Context, Window};

use super::ExplorerPane;
use super::view::preview::editor::PreviewEditor;

/// How long the row the arrow keys left active has to stay put before the
/// preview follows it. Holding Down through a folder passes a row every ~30 ms
/// (the keyboard's repeat), and each preview reads up to 2 MB, lists a folder
/// or starts 7-Zip for an archive — none of which can be stopped once
/// running. Waiting this long reads only the row the keys stop on; it is about
/// one frame more than a repeat, and well under the 100 ms past which the
/// preview would feel behind (OTIMIZACAO.md §3.10, §5.4, §7.7).
pub(crate) const PREVIEW_FOLLOW_DELAY: std::time::Duration = std::time::Duration::from_millis(80);

/// Result of reading a file for preview off the UI thread.
enum PreviewOutcome {
    TooLarge,
    Text(String),
    Image,
    /// A folder's contents or a zip's entries.
    Listing(super::preview_listing::PreviewListing),
    /// A PDF, rendered page by page once chosen.
    Pdf,
    /// Video or audio, played in the pane.
    Media,
    /// A 3D model, drawn by the pane and turned by dragging.
    Model,
    /// A document read as formatted text (`documents.rs`), in Markdown.
    Document(String),
    /// A creative app's file (`creative.rs`).
    Creative(super::creative::Creative),
    Unsupported,
    /// Nothing could be read — an archive entry that would not unpack, say.
    Failed(String),
}

fn detect_language(path: &str) -> String {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    match extension.as_str() {
        "rs" => "rust",
        "md" | "markdown" | "mdx" => "markdown",
        "json" | "jsonc" | "json5" | "geojson" => "json",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "html" | "htm" | "xml" | "svg" | "xaml" | "csproj" | "plist" => "html",
        "go" => "go",
        "zig" => "zig",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "css" | "scss" => "css",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "py" | "pyw" => "python",
        "sh" | "bash" | "zsh" => "bash",
        "java" => "java",
        "rb" => "ruby",
        "sql" => "sql",
        "swift" => "swift",
        "diff" | "patch" => "diff",
        _ => "plain",
    }
    .to_string()
}

/// Re-indents text that a machine wrote on one line so a person can read it.
///
/// Only JSON for now, and only when it parses: anything else, including JSON
/// with comments, is shown as written rather than risk showing something that
/// is not what the file says.
fn prettify(language: &str, text: &str) -> Option<String> {
    if language != "json" {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let pretty = serde_json::to_string_pretty(&value).ok()?;
    // Already laid out by hand: keep the author's formatting (and key order
    // `Value` would otherwise sort).
    (pretty.lines().count() > text.lines().count()).then_some(pretty)
}

fn is_image_path(path: &str) -> bool {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "svg" | "webp"
    )
}

/// Image formats read by path and drawn, never decoded as text: the raster
/// ones, and SVG, which is text but is looked at as the picture it draws.
fn is_binary_image_path(path: &str) -> bool {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    // SVG is drawn as the picture it is, as the other images are; until
    // 05/11/2026 it showed as its XML.
    matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg"
    )
}

/// Reads `path` and classifies it for preview. Runs on a background thread, so
/// it must not touch any GPUI state.
// Always invoked from `cx.background_spawn` (see `open_preview`), so its
// blocking reads run off the GPUI foreground thread.
fn extension_of(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

#[allow(clippy::disallowed_methods)]
fn read_preview(path: &str, show_hidden: bool, edge: u32) -> PreviewOutcome {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return PreviewOutcome::Unsupported,
    };
    if metadata.is_dir() {
        return match super::preview_listing::read_folder(std::path::Path::new(path), show_hidden) {
            Ok(listing) => PreviewOutcome::Listing(listing),
            Err(_) => PreviewOutcome::Unsupported,
        };
    }
    if !metadata.is_file() {
        return PreviewOutcome::Unsupported;
    }
    if extension_of(path) == "pdf" {
        return PreviewOutcome::Pdf;
    }
    if super::creative::is_creative(path) {
        return match super::creative::read(path, edge) {
            Ok(creative) => PreviewOutcome::Creative(creative),
            Err(error) => {
                tracing::debug!("no preview of {path}: {error}");
                PreviewOutcome::Unsupported
            }
        };
    }
    if super::documents::is_document(path) {
        return match super::documents::read(path) {
            Ok(markdown) => PreviewOutcome::Document(markdown),
            Err(error) => {
                tracing::debug!("no preview of {path}: {error}");
                PreviewOutcome::Unsupported
            }
        };
    }
    if super::media::is_media(path) {
        return PreviewOutcome::Media;
    }
    if super::model3d::is_model(path) {
        return PreviewOutcome::Model;
    }
    if extension_of(path) == "zip" {
        return match super::preview_listing::read_zip(std::path::Path::new(path)) {
            Ok(listing) => PreviewOutcome::Listing(listing),
            Err(_) => PreviewOutcome::Unsupported,
        };
    }
    if super::preview_listing::is_other_archive(path) {
        return match super::preview_listing::read_archive(std::path::Path::new(path)) {
            Ok(listing) => PreviewOutcome::Listing(listing),
            Err(_) => PreviewOutcome::Unsupported,
        };
    }
    // Binary images are rendered from disk by path, so avoid reading the whole
    // file and attempting a UTF-8 decode just to discover it is binary. Ahead of
    // the size cap, which is about how much text the editor can take: a 20 MB
    // photo is an ordinary image, not too large to look at.
    if is_binary_image_path(path) {
        return PreviewOutcome::Image;
    }
    if metadata.len() > config::PREVIEW_MAX_FILE_SIZE {
        return PreviewOutcome::TooLarge;
    }
    match read_unless_binary(path, is_image_path(path)) {
        Ok(None) => PreviewOutcome::Unsupported,
        Ok(Some(bytes)) => match String::from_utf8(bytes) {
            Ok(text) => PreviewOutcome::Text(text),
            Err(_) if is_image_path(path) => PreviewOutcome::Image,
            Err(_) => PreviewOutcome::Unsupported,
        },
        Err(_) => PreviewOutcome::Unsupported,
    }
}

/// How much of a file is read to tell text from binary.
const SNIFF_BYTES: usize = 8 * 1024;

/// The whole file, or `None` when its first [`SNIFF_BYTES`] hold a zero byte,
/// which no UTF-8 text the preview can show has. Until 04/11/2026 an unknown
/// file was read whole (up to 2 MB) just for `from_utf8` to fail on it —
/// from a cloud drive, a download of the whole thing. Text in UTF-16 has
/// zeros too, and came out as "not available" before as well. `image` keeps
/// what may be an image whole, for the caller's own test.
#[allow(clippy::disallowed_methods)]
fn read_unless_binary(path: &str, image: bool) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    (&mut file).take(SNIFF_BYTES as u64).read_to_end(&mut bytes)?;
    if !image && bytes.contains(&0) {
        return Ok(None);
    }
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// Computes the byte offset of the start of the given 0-based line index,
/// accounting for `\n` and `\r\n` line endings.
fn line_start_offset(text: &str, target_line: usize) -> Option<usize> {
    let mut current_off = 0;
    for (i, line) in text.lines().enumerate() {
        if i == target_line {
            return Some(current_off);
        }
        let consumed = line.len();
        let remainder = &text[current_off + consumed..];
        let newline_len = if remainder.starts_with("\r\n") {
            2
        } else if remainder.starts_with('\n') {
            1
        } else {
            0
        };
        current_off += consumed + newline_len;
    }
    None
}

impl ExplorerPane {
    /// Moves the active row like [`Self::move_active`] and has the preview
    /// follow it, as Explorer's preview pane follows the arrow keys. Until
    /// 04/11/2026 the preview stayed on the last row clicked.
    pub(crate) fn move_active_following(
        &mut self,
        delta: isize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active(delta, extend);
        self.follow_active_in_preview(window, cx);
    }

    /// Space: opens the quick look on the active entry, or closes it.
    pub(crate) fn toggle_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quick_look {
            self.close_quick_look(cx);
            return;
        }
        let Some(path) = self
            .active_row()
            .and_then(|row| self.filtered_entries.get(row))
            .filter(|entry| entry.kind == "file" || entry.kind == "dir")
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        self.quick_look = true;
        self.preview_follow = None;
        // A hidden side panel has not been reading the rows it passed.
        if self.preview_path.as_deref() != Some(path.as_str()) {
            self.open_preview(path, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn close_quick_look(&mut self, cx: &mut Context<Self>) {
        if self.quick_look {
            self.quick_look = false;
            cx.notify();
        }
    }

    /// Previews the active row once it has stayed put for
    /// [`PREVIEW_FOLLOW_DELAY`]. Each call replaces the wait before it, so a
    /// run of key presses reads one file, the last.
    pub(crate) fn follow_active_in_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A hidden preview reads nothing; the next click shows the row then.
        if !self.preview_visible && !self.quick_look {
            self.preview_follow = None;
            return;
        }
        let Some(path) = self
            .active_row()
            .and_then(|row| self.filtered_entries.get(row))
            // What a click previews (`row.rs`): not a link or a device.
            .filter(|entry| entry.kind == "file" || entry.kind == "dir")
            .map(|entry| entry.path.clone())
        else {
            self.preview_follow = None;
            return;
        };
        if self.preview_path.as_deref() == Some(path.as_str()) {
            self.preview_follow = None;
            return;
        }
        let delay = self.preview_follow_delay;
        self.preview_follow = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update_in(cx, |pane, window, cx| {
                // A click in the meantime moved on to another row, and
                // previewed it itself.
                if pane.active_path.as_deref() == Some(path.as_str())
                    && pane.preview_path.as_deref() != Some(path.as_str())
                {
                    pane.open_preview(path, window, cx);
                }
            })
            .log_err();
        }));
    }

    /// Reads the folder the preview lists again, keeping where it was scrolled
    /// and zoomed: an item dragged out of it to Explorer on the same drive was
    /// moved, and must not stay listed. An archive's entries do not change by
    /// dragging, so only a folder is read.
    pub(crate) fn refresh_preview_listing(&mut self, cx: &mut Context<Self>) {
        use nohrs_core::telemetry::LogErr as _;
        let (Some(path), Some(_)) = (self.preview_path.clone(), self.preview_listing.as_ref()) else {
            return;
        };
        let show_hidden = self.show_hidden;
        let read = cx.background_spawn({
            let path = path.clone();
            async move {
                let folder = std::path::Path::new(&path);
                #[allow(clippy::disallowed_methods)]
                let is_folder = std::fs::metadata(folder).is_ok_and(|metadata| metadata.is_dir());
                is_folder
                    .then(|| super::preview_listing::read_folder(folder, show_hidden).ok())
                    .flatten()
            }
        });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let Some(listing) = read.await else {
                return;
            };
            this.update(cx, |pane, cx| {
                // Another file was previewed in the meantime.
                if pane.preview_path.as_deref() == Some(path.as_str()) {
                    pane.preview_listing = Some(std::rc::Rc::new(listing));
                    cx.notify();
                }
            })
            .log_err();
        })
        .detach();
    }

    /// Previews `path`, picked in the listing: a new look, so the preview's
    /// own back history starts over.
    pub(crate) fn open_preview(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preview_back.clear();
        self.load_preview(path, window, cx);
    }

    /// Previews `path`, clicked inside the preview's folder listing; the
    /// back button returns to what was shown before.
    pub(crate) fn preview_into(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(current) = self.preview_path.take() {
            self.preview_back.push(current);
        }
        self.load_preview(path, window, cx);
    }

    /// Back to what the preview showed before the last [`Self::preview_into`].
    pub(crate) fn preview_go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.preview_back.pop() {
            self.load_preview(previous, window, cx);
        }
    }

    /// Closes whatever the preview shows, releasing the player, the PDF pages
    /// and the model's image. The one list of what makes up the preview, for
    /// opening another file and for a refresh that found the file gone.
    pub(crate) fn clear_preview(&mut self, cx: &mut Context<Self>) {
        self.close_model_preview(cx);
        self.preview_editor = None;
        self.preview_image_path = None;
        self.preview_sharp = None;
        self.preview_markdown = None;
        self.preview_message = None;
        self.preview_text = None;
        self.preview_listing = None;
        self.close_pdf_preview(cx);
        self.close_media_preview(cx);
        self.preview_picture = None;
        self.preview_caption = None;
        self.preview_editing = false;
        self.preview_dirty = false;
        self.preview_saveable = false;
        self.preview_editor_changes = None;
        self.preview_path = None;
    }

    fn load_preview(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.preview_zoom = 1.0;
        self.preview_pan = None;
        self.preview_scroll.set_offset(gpui::Point::default());
        self.clear_preview(cx);
        // Record the path being loaded so that out-of-order async completions
        // (the user clicking another file before this read finishes) can be
        // detected and discarded below.
        self.preview_path = Some(path.clone());
        cx.notify();

        let show_hidden = self.show_hidden;
        // A picture decoded here is decoded at the size the panel shows.
        let edge = super::view::preview::fitted_edge(
            super::view::panels::preview_width(),
            f32::from(window.viewport_size().height),
            window.scale_factor(),
        );
        // An entry inside an archive is unpacked first, into the temporary
        // folder, and the preview reads that: no reader here can open a path
        // inside an archive. The outcome carries the file that was read, so
        // the PDF, media and model readers get the unpacked one too.
        let from_archive = super::archive::split(&path).is_some();
        let read_task = cx.background_spawn({
            let path = path.clone();
            async move {
                let source = if from_archive {
                    match super::archive::extract(&path) {
                        Ok(unpacked) => unpacked.to_string_lossy().into_owned(),
                        Err(error) => {
                            return (path.clone(), PreviewOutcome::Failed(error));
                        }
                    }
                } else {
                    path.clone()
                };
                let outcome = read_preview(&source, show_hidden, edge);
                (source, outcome)
            }
        });

        // Held, not detached: the next preview replaces it, which drops the
        // read of this file if a worker has not started it yet. One already
        // running finishes and is discarded by the path check below.
        self.preview_load = Some(cx.spawn_in(
            window,
            move |this: gpui::WeakEntity<ExplorerPane>, cx: &mut AsyncWindowContext| {
                let mut cx = cx.clone();
                async move {
                    let (source, outcome) = read_task.await;
                    if let Err(error) = this.update_in(&mut cx, |this, window, cx| {
                        // Skip if a newer preview was requested while we were reading.
                        if this.preview_path.as_deref() != Some(path.as_str()) {
                            return;
                        }
                        this.apply_preview_outcome(path, source, outcome, window, cx);
                    }) {
                        tracing::debug!("Preview update skipped: {error}");
                    }
                }
            },
        ));
    }

    fn apply_preview_outcome(
        &mut self,
        path: String,
        source: String,
        outcome: PreviewOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An entry inside an archive is read from the unpacked copy: its own
        // path names nothing on disk, so nothing may be saved back over it.
        if super::archive::split(&path).is_some() {
            self.preview_saveable = false;
        }
        match outcome {
            PreviewOutcome::Failed(error) => {
                self.preview_path = Some(path);
                self.preview_message = Some(error);
            }
            PreviewOutcome::TooLarge => {
                self.preview_path = Some(path);
                self.preview_message = Some(t!("(Arquivo grande demais para visualizar)").to_string());
            }
            PreviewOutcome::Image => {
                self.preview_path = Some(path.clone());
                self.preview_image_path = Some(source.clone());
            }
            PreviewOutcome::Pdf => {
                self.preview_path = Some(path.clone());
                self.open_pdf_preview(source, cx);
            }
            PreviewOutcome::Media => {
                self.preview_path = Some(path.clone());
                self.open_media_preview(source, cx);
            }
            PreviewOutcome::Model => {
                self.preview_path = Some(path.clone());
                self.open_model_preview(source, cx);
            }
            PreviewOutcome::Listing(listing) => {
                self.preview_path = Some(path);
                self.preview_listing = Some(std::rc::Rc::new(listing));
                // Another folder starts at its top.
                self.preview_listing_scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
            }
            PreviewOutcome::Unsupported => {
                self.preview_path = Some(path);
                self.preview_message = Some(t!("(Sem visualização para este arquivo)").to_string());
            }
            PreviewOutcome::Document(markdown) => {
                self.preview_path = Some(path);
                self.preview_markdown = Some(markdown.into());
            }
            PreviewOutcome::Creative(super::creative::Creative::Pdf) => {
                self.preview_path = Some(path.clone());
                self.open_pdf_preview(path, cx);
            }
            PreviewOutcome::Creative(super::creative::Creative::Summary(markdown)) => {
                self.preview_path = Some(path);
                self.preview_markdown = Some(markdown.into());
            }
            PreviewOutcome::Creative(super::creative::Creative::Picture { width, height, bgra, caption }) => {
                self.preview_path = Some(path);
                self.preview_picture = super::thumbnails::render_image(width, height, bgra).map(std::sync::Arc::new);
                self.preview_caption = (!caption.is_empty()).then_some(caption);
                if self.preview_picture.is_none() {
                    self.preview_message = Some(t!("(Sem visualização para este arquivo)").to_string());
                }
            }
            PreviewOutcome::Text(text) => {
                self.preview_path = Some(path.clone());
                let language = detect_language(&path);

                // A file opened from a content-search hit is shown exactly as
                // written: the hit is a line number in the file on disk, and
                // re-indenting or rendering it would move that line.
                // A hit of a name search (Ctrl+F, the whole computer) has no
                // line to keep: it is rendered like any file. Until 05/11/2026
                // every hit counted, and a .md found that way showed raw.
                let from_search = self.search_results.as_ref().is_some_and(|results| {
                    results.iter().any(|result| result.path == path && !result.matches.is_empty())
                });
                self.preview_crlf = text.contains("\r\n");
                if !from_search && language == "markdown" {
                    self.preview_text = Some(text.clone());
                    self.preview_saveable = true;
                    self.preview_markdown = Some(text.into());
                    cx.notify();
                    return;
                }
                let (text, reformatted) = match prettify(&language, &text) {
                    Some(pretty) if !from_search => (pretty, true),
                    _ => (text, false),
                };
                self.preview_saveable = !reformatted;
                self.preview_text = Some(text.clone());

                let editor_view = cx.new(|cx| PreviewEditor::new(window, cx));
                editor_view.update(cx, |editor, cx| {
                    editor.set_text(text.clone(), window, cx);
                    if language != "plain" {
                        editor.set_language(language, window, cx);
                    }
                });
                if self.preview_saveable {
                    self.watch_preview_editor(&editor_view, window, cx);
                }
                self.preview_editor = Some(editor_view);

                // Highlights (search only; syntax handled by editor)
                self.update_editor_search(window, cx);

                // Scroll to the first match for the active query, if any.
                if let Some(results) = &self.search_results
                    && let Some(file_result) = results.iter().find(|r| r.path == path)
                    && let Some(first_match) = file_result.matches.first()
                    // `line_number` is 1-based; `line_start_offset` takes a 0-based index.
                    && let Some(offset) =
                        line_start_offset(&text, first_match.line_number.saturating_sub(1))
                    && let Some(editor) = self.preview_editor.clone()
                {
                    editor.update(cx, |editor, cx| {
                        editor.scroll_to(offset, window, cx);
                    });
                }
            }
        }
        cx.notify();
    }

    /// A Markdown file between its rendered view and its source, which can
    /// be edited and saved. Back to the view, it shows what was typed.
    pub(crate) fn toggle_preview_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preview_editing {
            if let Some(editor) = self.preview_editor.take() {
                self.preview_markdown = Some(editor.read(cx).text(cx).into());
            }
            self.preview_editor_changes = None;
            self.preview_editing = false;
            cx.focus_self(window);
            cx.notify();
            return;
        }
        let Some(text) = self.preview_markdown.as_ref().map(|text| text.to_string()).or_else(|| self.preview_text.clone()) else {
            return;
        };
        let editor = cx.new(|cx| PreviewEditor::new(window, cx));
        editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
            editor.set_language("markdown".to_string(), window, cx);
            editor.focus(window, cx);
        });
        self.watch_preview_editor(&editor, window, cx);
        self.preview_editor = Some(editor);
        self.preview_editing = true;
        cx.notify();
    }

    // Each change in the editor says whether it now differs from the file.
    fn watch_preview_editor(&mut self, editor: &gpui::Entity<PreviewEditor>, window: &mut Window, cx: &mut Context<Self>) {
        let state = editor.read(cx).state().clone();
        let editor = editor.downgrade();
        self.preview_editor_changes = Some(cx.subscribe_in(
            &state,
            window,
            move |this, _state, event: &gpui_component::input::InputEvent, _window, cx| {
                if !matches!(event, gpui_component::input::InputEvent::Change) {
                    return;
                }
                let Some(editor) = editor.upgrade() else {
                    return;
                };
                let text = editor.read(cx).text(cx);
                let dirty = this.preview_text.as_deref().map(|saved| saved.replace("\r\n", "\n")) != Some(text);
                if dirty != this.preview_dirty {
                    this.preview_dirty = dirty;
                    cx.notify();
                }
            },
        ));
    }

    /// Writes the editor's text to the file it came from (Ctrl+S, or the
    /// preview's save button), keeping its line endings.
    pub(crate) fn save_preview(&mut self, cx: &mut Context<Self>) {
        let (Some(path), Some(editor)) = (self.preview_path.clone(), self.preview_editor.clone()) else {
            return;
        };
        if !self.preview_saveable {
            return;
        }
        let text = editor.read(cx).text(cx);
        let written = if self.preview_crlf { text.replace("\r\n", "\n").replace('\n', "\r\n") } else { text.clone() };
        let write = cx.background_spawn({
            let path = path.clone();
            #[allow(clippy::disallowed_methods)]
            async move { std::fs::write(&path, written) }
        });
        cx.spawn(async move |this: gpui::WeakEntity<Self>, cx| {
            let result = write.await;
            this.update(cx, |pane, cx| {
                match result {
                    Ok(()) => {
                        if pane.preview_path.as_deref() == Some(path.as_str()) {
                            pane.preview_text = Some(text);
                            pane.preview_dirty = false;
                        }
                        pane.set_status(
                            super::types::StatusLevel::Info,
                            nohrs_core::tf!("Salvo: {name}", name = std::path::Path::new(&path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()),
                        );
                    }
                    Err(error) => pane.set_status(
                        super::types::StatusLevel::Error,
                        nohrs_core::tf!("Não foi possível salvar: {error}", error = error),
                    ),
                }
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }

    pub(crate) fn update_editor_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor_entity) = self.preview_editor.clone() {
            let query = self.search_query.clone();
            editor_entity.update(cx, |editor, cx| {
                editor.set_search_query(query, window, cx);
            });
        }
    }

    pub(crate) fn scroll_to_line(
        &mut self,
        line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = self.preview_text.clone() else {
            return;
        };
        // 1-based line number to 0-based index
        let target_idx = line.saturating_sub(1);
        if let Some(offset) = line_start_offset(&text, target_idx)
            && let Some(editor) = self.preview_editor.clone()
        {
            editor.update(cx, |editor, cx| {
                editor.scroll_to(offset, window, cx);
            });
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn prettify_reindents_minified_json_only() {
        let pretty =
            prettify("json", r#"{"a":{"b":[1,2]}}"#).expect("minified JSON is re-indented");
        assert!(pretty.lines().count() > 1);
        assert!(pretty.contains("\n  \"a\": {"));
        // Already laid out: left as the author wrote it.
        assert_eq!(prettify("json", "{\n  \"a\": 1\n}"), None);
        // Not JSON, or JSON that does not parse: shown as written.
        assert_eq!(prettify("json", "{\"a\": 1, // note\n}"), None);
        assert_eq!(prettify("rust", "fn main(){}"), None);
    }

    #[test]
    fn detect_language_maps_known_extensions() {
        for (path, expected) in [
            ("a.rs", "rust"),
            ("a.md", "markdown"),
            ("a.json", "json"),
            ("a.js", "javascript"),
            ("a.ts", "typescript"),
            ("a.html", "html"),
            ("a.go", "go"),
            ("a.zig", "zig"),
            ("a.toml", "toml"),
            ("a.yaml", "yaml"),
            ("a.yml", "yaml"),
            ("a.css", "css"),
            ("a.c", "c"),
            ("a.cpp", "cpp"),
        ] {
            assert_eq!(detect_language(path), expected, "for {path}");
        }
    }

    #[test]
    fn detect_language_is_case_insensitive_and_defaults_to_plain() {
        assert_eq!(detect_language("MAIN.RS"), "rust");
        assert_eq!(detect_language("notes.unknown"), "plain");
        assert_eq!(detect_language("noext"), "plain");
    }

    #[test]
    fn image_path_classification() {
        for ext in ["png", "jpg", "jpeg", "gif", "bmp", "svg", "webp"] {
            assert!(is_image_path(&format!("a.{ext}")), "{ext} is an image");
        }
        assert!(is_image_path("PHOTO.PNG"));
        assert!(!is_image_path("a.txt"));
    }

    #[test]
    fn svg_previews_as_a_picture() {
        assert!(is_binary_image_path("a.png"));
        assert!(is_binary_image_path("a.gif"));
        // Drawn, not shown as its XML (decided 05/11/2026).
        assert!(is_binary_image_path("a.svg"));
        assert!(!is_binary_image_path("a.txt"));
    }

    #[test]
    fn a_binary_file_is_not_read_past_its_first_bytes() {
        let dir = tempfile::tempdir().unwrap();
        // A program: zeros early, and far bigger than what is looked at.
        let program = dir.path().join("app.dll");
        let mut bytes = b"MZ\x90\x00".to_vec();
        bytes.resize(1024 * 1024, 7);
        std::fs::write(&program, &bytes).unwrap();
        assert!(read_unless_binary(&program.to_string_lossy(), false).unwrap().is_none());

        // Text longer than the sniffed part arrives whole.
        let notes = dir.path().join("notas.txt");
        let text = "linha de texto\n".repeat(2000);
        std::fs::write(&notes, &text).unwrap();
        assert_eq!(read_unless_binary(&notes.to_string_lossy(), false).unwrap(), Some(text.into_bytes()));

        // What may be an image is read whole for the caller to try.
        assert!(read_unless_binary(&program.to_string_lossy(), true).unwrap().is_some());
    }

    #[test]
    fn read_preview_classifies_files() {
        let dir = tempfile::tempdir().unwrap();

        let text_path = dir.path().join("note.txt");
        std::fs::write(&text_path, "hello\nworld").unwrap();
        match read_preview(&text_path.to_string_lossy(), false, 512) {
            PreviewOutcome::Text(body) => assert_eq!(body, "hello\nworld"),
            _ => panic!("expected Text"),
        }

        // Binary-image extension short-circuits to Image without decoding.
        let png_path = dir.path().join("pic.png");
        std::fs::write(&png_path, [0u8, 1, 2, 3]).unwrap();
        assert!(matches!(
            read_preview(&png_path.to_string_lossy(), false, 512),
            PreviewOutcome::Image
        ));

        // Non-UTF-8, non-image → Unsupported.
        let bin_path = dir.path().join("blob.bin");
        std::fs::write(&bin_path, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(matches!(
            read_preview(&bin_path.to_string_lossy(), false, 512),
            PreviewOutcome::Unsupported
        ));

        // A directory lists its contents; a missing path is Unsupported.
        assert!(matches!(
            read_preview(&dir.path().to_string_lossy(), false, 512),
            PreviewOutcome::Listing(_)
        ));
        assert!(matches!(
            read_preview("/nonexistent/path/here", false, 512),
            PreviewOutcome::Unsupported
        ));
    }

    #[test]
    fn line_start_offset_handles_lf_crlf_and_bounds() {
        let lf = "a\nbb\nccc";
        assert_eq!(line_start_offset(lf, 0), Some(0));
        assert_eq!(line_start_offset(lf, 1), Some(2));
        assert_eq!(line_start_offset(lf, 2), Some(5));
        assert_eq!(line_start_offset(lf, 3), None);

        let crlf = "a\r\nbb";
        assert_eq!(line_start_offset(crlf, 1), Some(3));
    }
}
