//! A terminal drawn inside the explorer, one per tab, started in that tab's
//! folder.
//!
//! The shell runs in a Windows pseudo console (ConPTY) and its output is
//! parsed into a grid by `alacritty_terminal`, which owns both: a thread of
//! its own reads the console and updates the grid, and this view only paints
//! the grid and turns keys into the bytes a terminal sends.

use nohrs_core::{t, tf};
use std::borrow::Cow;
use std::cell::Cell as StdCell;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color as TermColor, CursorShape, NamedColor, Rgb};
use gpui::prelude::*;
use gpui::*;
use nohrs_ui::theme::theme;

/// The key context of a focused terminal.
pub const TERMINAL_CONTEXT: &str = "Terminal";

const FONT_SIZE: f32 = 13.0;
const PADDING: f32 = 6.0;

/// Something the page has to act on.
pub enum TerminalEvent {
    /// Enter pressed after the shell ended: the page starts a new one in the
    /// tab's current folder, which this view does not know.
    RestartRequested,
}

#[derive(Clone)]
struct Listener(async_channel::Sender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        // Fails only once the view, and with it the receiver, is gone, when
        // nobody is left to show the event to.
        if self.0.try_send(event).is_err() {
            tracing::trace!("terminal event after the view closed");
        }
    }
}

// The grid size handed to `alacritty_terminal`; the scrollback is its own.
struct GridSize {
    columns: usize,
    lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

struct Session {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
}

impl Session {
    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        if let Err(error) = self.sender.send(Msg::Input(bytes.into())) {
            tracing::warn!("terminal input lost: {error:?}");
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Ends the reader thread, which closes the console and with it the
        // shell, so closing a tab leaves no shell running.
        if let Err(error) = self.sender.send(Msg::Shutdown) {
            tracing::debug!("terminal already shut down: {error:?}");
        }
    }
}

// Cell size and where the grid was last painted, read back by the mouse.
#[derive(Clone, Copy, Default)]
struct Layout {
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    columns: usize,
    lines: usize,
}

/// One shell and the grid it draws in.
pub struct TerminalView {
    session: Result<Session, String>,
    directory: PathBuf,
    focus_handle: FocusHandle,
    title: Option<String>,
    exited: bool,
    // Whether anything was typed or pasted into it. Until then the panel may
    // replace it with one in the folder the tab moved to; after, it is the
    // user's shell and stays where they left it.
    used: bool,
    layout: Rc<StdCell<Layout>>,
    // Wheel movement smaller than a line, kept for the next event.
    scroll_remainder: Pixels,
    _events: Option<Task<()>>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    /// Starts a shell in `directory`.
    pub fn new(directory: PathBuf, cx: &mut Context<Self>) -> Self {
        let (sender, receiver) = async_channel::unbounded();
        let session = start(&directory, Listener(sender));
        if let Err(error) = &session {
            tracing::error!(
                "could not start a terminal in {}: {error}",
                directory.display()
            );
        }
        let events = cx.spawn(async move |this: WeakEntity<Self>, cx| {
            while let Ok(event) = receiver.recv().await {
                if this
                    .update(cx, |view, cx| view.on_event(event, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            session,
            directory,
            focus_handle: cx.focus_handle(),
            title: None,
            exited: false,
            used: false,
            layout: Rc::new(StdCell::new(Layout::default())),
            scroll_remainder: px(0.0),
            _events: Some(events),
        }
    }

    /// The folder the shell started in.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The title the shell set, if any.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Whether anything was typed or pasted into it.
    pub fn is_used(&self) -> bool {
        self.used
    }

    /// Whether the shell ended, or never started.
    pub fn has_exited(&self) -> bool {
        self.exited || self.session.is_err()
    }

    fn on_event(&mut self, event: TermEvent, cx: &mut Context<Self>) {
        let Ok(session) = &self.session else {
            return;
        };
        match event {
            TermEvent::Wakeup | TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => {}
            TermEvent::Title(title) => self.title = Some(title),
            TermEvent::ResetTitle => self.title = None,
            TermEvent::PtyWrite(text) => session.write(text.into_bytes()),
            TermEvent::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text))
            }
            TermEvent::ClipboardLoad(_, format) => {
                let text = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .unwrap_or_default();
                session.write(format(&text).into_bytes());
            }
            TermEvent::ColorRequest(index, format) => {
                let configured = session.term.lock().colors()[index];
                let rgb = configured.unwrap_or_else(|| default_rgb(index));
                session.write(format(rgb).into_bytes());
            }
            TermEvent::TextAreaSizeRequest(format) => {
                let layout = self.layout.get();
                session.write(format(window_size(&layout)).into_bytes());
            }
            TermEvent::Bell => {}
            TermEvent::Exit | TermEvent::ChildExit(_) => self.exited = true,
        }
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Ok(session) = &self.session else {
            return;
        };
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bracketed = session
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        let bytes = if bracketed {
            // The end marker inside the text would end the paste early and run
            // the rest as typed commands.
            format!("\x1b[200~{}\x1b[201~", text.replace("\x1b[201~", "")).into_bytes()
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
        };
        session.write(bytes);
        self.used = true;
    }

    // Copies the selection, if any; whether there was one.
    fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let Ok(session) = &self.session else {
            return false;
        };
        let mut term = session.term.lock();
        let Some(text) = term.selection_to_string().filter(|text| !text.is_empty()) else {
            return false;
        };
        term.selection = None;
        drop(term);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        cx.notify();
        true
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let key = keystroke.key.as_str();

        if self.has_exited() {
            if key == "enter" {
                cx.emit(TerminalEvent::RestartRequested);
                cx.stop_propagation();
            }
            return;
        }

        // Windows Terminal's choices: Ctrl+C copies when something is
        // selected and interrupts otherwise; Ctrl+V pastes.
        let handled = match (modifiers.control, modifiers.shift, modifiers.alt, key) {
            (true, true, false, "c") => {
                self.copy(cx);
                true
            }
            (true, false, false, "c") => self.copy(cx),
            (true, _, false, "v") | (false, true, false, "insert") => {
                self.paste(cx);
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
            return;
        }

        let Ok(session) = &self.session else {
            return;
        };
        let mode = *session.term.lock().mode();
        let Some(bytes) = key_bytes(keystroke, mode) else {
            // Plain text: left for the input handler, which also receives the
            // accented letters dead keys compose (´ + e = é on ABNT2).
            return;
        };
        let mut term = session.term.lock();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        drop(term);
        session.write(bytes);
        self.used = true;
        cx.stop_propagation();
        cx.notify();
    }

    fn grid_point(&self, position: Point<Pixels>) -> Option<(GridPoint, Side)> {
        let layout = self.layout.get();
        if layout.columns == 0 || layout.lines == 0 || layout.cell_width <= px(0.0) {
            return None;
        }
        let x = ((position.x - layout.origin.x) / layout.cell_width).max(0.0);
        let y = ((position.y - layout.origin.y) / layout.line_height).max(0.0);
        let column = (x.floor() as usize).min(layout.columns - 1);
        let row = (y.floor() as usize).min(layout.lines - 1);
        let side = if x.fract() > 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        let offset = match &self.session {
            Ok(session) => session.term.lock().grid().display_offset(),
            Err(_) => 0,
        };
        Some((
            GridPoint::new(Line(row as i32 - offset as i32), Column(column)),
            side,
        ))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        let Some((point, side)) = self.grid_point(event.position) else {
            return;
        };
        let Ok(session) = &self.session else {
            return;
        };
        let kind = match event.click_count {
            2 => SelectionType::Semantic,
            3.. => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        session.term.lock().selection = Some(Selection::new(kind, point, side));
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some((point, side)) = self.grid_point(event.position) else {
            return;
        };
        let Ok(session) = &self.session else {
            return;
        };
        if let Some(selection) = session.term.lock().selection.as_mut() {
            selection.update(point, side);
        }
        cx.notify();
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let Ok(session) = &self.session else {
            return;
        };
        let line_height = self.layout.get().line_height;
        if line_height <= px(0.0) {
            return;
        }
        self.scroll_remainder += event.delta.pixel_delta(line_height).y;
        let lines = (self.scroll_remainder / line_height).trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_remainder -= line_height * lines as f32;
        let mut term = session.term.lock();
        let mode = *term.mode();
        if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            // A full-screen program (less, vim) has no scrollback: the wheel
            // becomes arrow keys, as in other terminals.
            drop(term);
            let arrow: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            session.write(arrow.repeat(lines.unsigned_abs() as usize));
        } else {
            term.scroll_display(Scroll::Delta(lines));
        }
        cx.notify();
    }
}

fn start(directory: &Path, listener: Listener) -> Result<Session, String> {
    let options = tty::Options {
        shell: Some(tty::Shell::new(
            shell_program(),
            vec!["-NoLogo".to_string()],
        )),
        working_directory: Some(directory.to_path_buf()),
        drain_on_exit: true,
        ..Default::default()
    };
    let size = WindowSize {
        num_lines: 24,
        num_cols: 80,
        cell_width: 8,
        cell_height: 16,
    };
    let pty = tty::new(&options, size, 0).map_err(|error| error.to_string())?;
    let term = Term::new(
        Config::default(),
        &GridSize {
            columns: 80,
            lines: 24,
        },
        listener.clone(),
    );
    let term = Arc::new(FairMutex::new(term));
    let event_loop = EventLoop::new(term.clone(), listener, pty, true, false)
        .map_err(|error| error.to_string())?;
    let sender = event_loop.channel();
    // The reader thread ends on `Msg::Shutdown` or when the shell exits; its
    // handle is not needed to stop it.
    drop(event_loop.spawn());
    Ok(Session { term, sender })
}

// PowerShell 7 when installed, the PowerShell every Windows has otherwise.
fn shell_program() -> String {
    let found = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join("pwsh.exe").is_file())
    });
    if found { "pwsh.exe" } else { "powershell.exe" }.to_string()
}

fn window_size(layout: &Layout) -> WindowSize {
    WindowSize {
        num_lines: layout.lines.max(1) as u16,
        num_cols: layout.columns.max(1) as u16,
        cell_width: f32::from(layout.cell_width).round() as u16,
        cell_height: f32::from(layout.line_height).round() as u16,
    }
}

/// The bytes a terminal sends for a key, or `None` for plain text, which
/// arrives through the input handler instead.
fn key_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let modifiers = keystroke.modifiers;
    let key = keystroke.key.as_str();
    // The xterm modifier parameter: 1 + Shift + 2·Alt + 4·Ctrl.
    let parameter = 1
        + u8::from(modifiers.shift)
        + 2 * u8::from(modifiers.alt)
        + 4 * u8::from(modifiers.control);
    let alt_prefix = |bytes: &[u8]| -> Vec<u8> {
        if modifiers.alt {
            [b"\x1b".as_slice(), bytes].concat()
        } else {
            bytes.to_vec()
        }
    };
    let cursor = |letter: char| -> Vec<u8> {
        if parameter > 1 {
            format!("\x1b[1;{parameter}{letter}").into_bytes()
        } else if mode.contains(TermMode::APP_CURSOR) {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let tilde = |number: u8| -> Vec<u8> {
        if parameter > 1 {
            format!("\x1b[{number};{parameter}~").into_bytes()
        } else {
            format!("\x1b[{number}~").into_bytes()
        }
    };
    let bytes = match key {
        "enter" => alt_prefix(b"\r"),
        "backspace" if modifiers.control => alt_prefix(b"\x08"),
        "backspace" => alt_prefix(b"\x7f"),
        "tab" if modifiers.shift => b"\x1b[Z".to_vec(),
        "tab" => alt_prefix(b"\t"),
        "escape" => b"\x1b".to_vec(),
        "up" => cursor('A'),
        "down" => cursor('B'),
        "right" => cursor('C'),
        "left" => cursor('D'),
        "home" => cursor('H'),
        "end" => cursor('F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" => cursor_function('P', parameter),
        "f2" => cursor_function('Q', parameter),
        "f3" => cursor_function('R', parameter),
        "f4" => cursor_function('S', parameter),
        "f5" => tilde(15),
        "f6" => tilde(17),
        "f7" => tilde(18),
        "f8" => tilde(19),
        "f9" => tilde(20),
        "f10" => tilde(21),
        "f11" => tilde(23),
        "f12" => tilde(24),
        "space" if modifiers.control => vec![0],
        _ => {
            // AltGr arrives as Ctrl+Alt with the character it types (AltGr+Q
            // is "/" on ABNT2): that is text, not a control key.
            if modifiers.control && modifiers.alt && keystroke.key_char.is_some() {
                return None;
            }
            if modifiers.control {
                let code = control_code(key)?;
                return Some(alt_prefix(&[code]));
            }
            if modifiers.alt
                && let Some(text) = keystroke.key_char.as_deref()
            {
                return Some(alt_prefix(text.as_bytes()));
            }
            return None;
        }
    };
    Some(bytes)
}

fn cursor_function(letter: char, parameter: u8) -> Vec<u8> {
    if parameter > 1 {
        format!("\x1b[1;{parameter}{letter}").into_bytes()
    } else {
        format!("\x1bO{letter}").into_bytes()
    }
}

fn control_code(key: &str) -> Option<u8> {
    let mut characters = key.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        return None;
    }
    match character {
        'a'..='z' => Some(character as u8 - b'a' + 1),
        '@' | '2' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' => Some(0x1f),
        _ => None,
    }
}

// The 16 ANSI colors in Windows Terminal's "Campbell" scheme, what a Windows
// user's programs are written to look right in.
const ANSI: [u32; 16] = [
    0x0C0C0C, 0xC50F1F, 0x13A10E, 0xC19C00, 0x0037DA, 0x881798, 0x3A96DD, 0xCCCCCC, 0x767676,
    0xE74856, 0x16C60C, 0xF9F1A5, 0x3B78FF, 0xB4009E, 0x61D6D6, 0xF2F2F2,
];

fn default_rgb(index: usize) -> Rgb {
    let color = match index {
        0..=15 => ANSI.get(index).copied().unwrap_or(0),
        16..=231 => {
            let cube = index - 16;
            let level = |value: usize| {
                if value == 0 {
                    0
                } else {
                    55 + value as u32 * 40
                }
            };
            (level(cube / 36) << 16) | (level(cube / 6 % 6) << 8) | level(cube % 6)
        }
        232..=255 => {
            let gray = 8 + (index as u32 - 232) * 10;
            (gray << 16) | (gray << 8) | gray
        }
        _ if index == NamedColor::Background as usize => theme::bg(),
        _ if index == NamedColor::Cursor as usize => theme::fg(),
        _ if index == NamedColor::DimForeground as usize => theme::fg_secondary(),
        _ => theme::fg(),
    };
    Rgb {
        r: (color >> 16) as u8,
        g: (color >> 8) as u8,
        b: color as u8,
    }
}

fn resolve(color: TermColor, colors: &Colors) -> Hsla {
    let rgb = match color {
        TermColor::Spec(rgb) => rgb,
        TermColor::Named(named) => colors[named].unwrap_or_else(|| default_rgb(named as usize)),
        TermColor::Indexed(index) => {
            colors[index as usize].unwrap_or_else(|| default_rgb(index as usize))
        }
    };
    gpui::rgb((u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)).into()
}

fn terminal_font(bold: bool, italic: bool) -> Font {
    Font {
        family: "Cascadia Mono".into(),
        features: FontFeatures::default(),
        fallbacks: Some(FontFallbacks::from_fonts(vec!["Consolas".to_string()])),
        weight: if bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        },
        style: if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
    }
}

// A stretch of one row painted with one style, placed by its first column so
// the grid stays aligned whatever the font's real advance.
struct Run {
    column: usize,
    text: String,
    color: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strikeout: bool,
    // A wide character's run: its glyph is not exactly two cells wide, so
    // nothing is appended and what follows starts on the grid again.
    closed: bool,
}

struct Background {
    row: usize,
    column: usize,
    width: usize,
    color: Hsla,
}

struct Frame {
    rows: Vec<Vec<Run>>,
    backgrounds: Vec<Background>,
    cursor: Option<(usize, usize, CursorShape, Hsla, Option<Run>)>,
}

fn build_frame(term: &Term<Listener>, columns: usize, lines: usize, focused: bool) -> Frame {
    let content = term.renderable_content();
    let colors = content.colors;
    let offset = content.display_offset as i32;
    let default_background = resolve(TermColor::Named(NamedColor::Background), colors);
    let selection_background: Hsla = gpui::rgb(theme::accent_subtle()).into();
    let mut rows: Vec<Vec<Run>> = (0..lines).map(|_| Vec::new()).collect();
    let mut backgrounds = Vec::new();
    let mut cursor_cell = None;
    let cursor_point = content.cursor.point;

    for indexed in content.display_iter {
        let Ok(row) = usize::try_from(indexed.point.line.0 + offset) else {
            continue;
        };
        let column = indexed.point.column.0;
        if row >= lines || column >= columns {
            continue;
        }
        let cell = &indexed.cell;
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        let mut foreground = resolve(cell.fg, colors);
        let mut background = resolve(cell.bg, colors);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut foreground, &mut background);
        }
        if cell.flags.contains(Flags::DIM) {
            foreground = foreground.opacity(0.7);
        }
        if cell.flags.contains(Flags::HIDDEN) {
            foreground = background;
        }
        let selected = content
            .selection
            .is_some_and(|range| range.contains(indexed.point));
        let width = if cell.flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        if selected {
            background = selection_background;
        }
        if background != default_background {
            backgrounds.push(Background {
                row,
                column,
                width,
                color: background,
            });
        }
        let character = if cell.c == '\0' { ' ' } else { cell.c };
        let mut text = String::from(character);
        if let Some(zerowidth) = cell.zerowidth() {
            text.extend(zerowidth);
        }
        let bold = cell.flags.contains(Flags::BOLD);
        let italic = cell.flags.contains(Flags::ITALIC);
        let underline = cell.flags.intersects(Flags::ALL_UNDERLINES);
        let strikeout = cell.flags.contains(Flags::STRIKEOUT);

        if indexed.point == cursor_point {
            cursor_cell = Some(Run {
                column,
                text: text.clone(),
                color: background,
                bold,
                italic,
                underline,
                strikeout,
                closed: true,
            });
        }

        let Some(runs) = rows.get_mut(row) else {
            continue;
        };
        let continues = runs.last().is_some_and(|last: &Run| {
            !last.closed
                && last.color == foreground
                && last.bold == bold
                && last.italic == italic
                && last.underline == underline
                && last.strikeout == strikeout
                && last.column + last.text.chars().count() == column
        });
        if continues && width == 1 {
            if let Some(last) = runs.last_mut() {
                last.text.push_str(&text);
            }
        } else if character != ' ' || underline || strikeout || continues {
            let closed = width == 2;
            runs.push(Run {
                column,
                text,
                color: foreground,
                bold,
                italic,
                underline,
                strikeout,
                closed,
            });
        }
    }

    let cursor = if content.mode.contains(TermMode::SHOW_CURSOR)
        && content.cursor.shape != CursorShape::Hidden
    {
        usize::try_from(cursor_point.line.0 + offset)
            .ok()
            .filter(|row| *row < lines)
            .map(|row| {
                let color = resolve(TermColor::Named(NamedColor::Cursor), colors);
                let shape = if focused {
                    content.cursor.shape
                } else {
                    CursorShape::HollowBlock
                };
                (
                    row,
                    cursor_point.column.0.min(columns.saturating_sub(1)),
                    shape,
                    color,
                    cursor_cell,
                )
            })
    } else {
        None
    };

    Frame {
        rows,
        backgrounds,
        cursor,
    }
}

fn paint_run(
    run: &Run,
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    if run.text.is_empty() {
        return;
    }
    let font_size = px(FONT_SIZE);
    let text_run = TextRun {
        len: run.text.len(),
        font: terminal_font(run.bold, run.italic),
        color: run.color,
        background_color: None,
        underline: run.underline.then(|| UnderlineStyle {
            thickness: px(1.0),
            color: Some(run.color),
            wavy: false,
        }),
        strikethrough: run.strikeout.then(|| StrikethroughStyle {
            thickness: px(1.0),
            color: Some(run.color),
        }),
    };
    let shaped =
        window
            .text_system()
            .shape_line(run.text.clone().into(), font_size, &[text_run], None);
    let position = point(origin.x + cell_width * run.column as f32, origin.y);
    if let Err(error) = shaped.paint(position, line_height, window, cx) {
        tracing::debug!("painting terminal text: {error}");
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background: Hsla = gpui::rgb(theme::bg()).into();
        let root = div()
            .id("terminal")
            .key_context(TERMINAL_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(background)
            .cursor(CursorStyle::IBeam)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_scroll_wheel(cx.listener(Self::scroll));

        let term = match &self.session {
            Ok(session) => session.term.clone(),
            Err(error) => {
                return root
                    .p(px(PADDING))
                    .text_sm()
                    .text_color(rgb(theme::danger()))
                    .child(tf!(
                        "Não foi possível abrir o terminal: {error}",
                        error = error
                    ))
                    .into_any_element();
            }
        };
        let sender = match &self.session {
            Ok(session) => session.sender.clone(),
            Err(_) => return root.into_any_element(),
        };
        let focused = self.focus_handle.is_focused(window);
        let layout_cell = self.layout.clone();
        let focus_handle = self.focus_handle.clone();
        let entity = cx.entity();
        let exited = self.exited;

        root.child(
            canvas(
                |_bounds, _window, _cx| {},
                move |bounds, (), window, cx| {
                    let font_size = px(FONT_SIZE);
                    let font_id = window
                        .text_system()
                        .resolve_font(&terminal_font(false, false));
                    let cell_width = window
                        .text_system()
                        .advance(font_id, font_size, 'm')
                        .map(|size| size.width)
                        .unwrap_or(px(FONT_SIZE * 0.6));
                    let line_height = (font_size * 1.3).round();
                    let origin =
                        point(bounds.origin.x + px(PADDING), bounds.origin.y + px(PADDING));
                    let usable = bounds.size - size(px(PADDING * 2.0), px(PADDING * 2.0));
                    let columns = ((usable.width / cell_width).floor() as usize).max(2);
                    let lines = ((usable.height / line_height).floor() as usize).max(1);
                    let layout = Layout {
                        origin,
                        cell_width,
                        line_height,
                        columns,
                        lines,
                    };
                    let previous = layout_cell.get();
                    layout_cell.set(layout);

                    let mut term = term.lock();
                    if previous.columns != columns || previous.lines != lines {
                        term.resize(GridSize { columns, lines });
                        if let Err(error) = sender.send(Msg::Resize(window_size(&layout))) {
                            tracing::debug!("terminal resize after shutdown: {error:?}");
                        }
                    }
                    let frame = build_frame(&term, columns, lines, focused && !exited);
                    drop(term);

                    window.handle_input(
                        &focus_handle,
                        ElementInputHandler::new(bounds, entity.clone()),
                        cx,
                    );

                    for background in &frame.backgrounds {
                        let position = point(
                            origin.x + cell_width * background.column as f32,
                            origin.y + line_height * background.row as f32,
                        );
                        window.paint_quad(fill(
                            Bounds::new(
                                position,
                                size(cell_width * background.width as f32, line_height),
                            ),
                            background.color,
                        ));
                    }
                    for (row, runs) in frame.rows.iter().enumerate() {
                        let row_origin = point(origin.x, origin.y + line_height * row as f32);
                        for run in runs {
                            paint_run(run, row_origin, cell_width, line_height, window, cx);
                        }
                    }
                    if let Some((row, column, shape, color, under)) = frame.cursor {
                        let position = point(
                            origin.x + cell_width * column as f32,
                            origin.y + line_height * row as f32,
                        );
                        let cell = Bounds::new(position, size(cell_width, line_height));
                        match shape {
                            CursorShape::Block => {
                                window.paint_quad(fill(cell, color));
                                if let Some(mut under) = under {
                                    under.column = column;
                                    paint_run(
                                        &under,
                                        point(origin.x, position.y),
                                        cell_width,
                                        line_height,
                                        window,
                                        cx,
                                    );
                                }
                            }
                            CursorShape::Beam => {
                                window.paint_quad(fill(
                                    Bounds::new(position, size(px(2.0), line_height)),
                                    color,
                                ));
                            }
                            CursorShape::Underline => {
                                let y = position.y + line_height - px(2.0);
                                window.paint_quad(fill(
                                    Bounds::new(point(position.x, y), size(cell_width, px(2.0))),
                                    color,
                                ));
                            }
                            CursorShape::HollowBlock => {
                                window.paint_quad(outline(cell, color, BorderStyle::Solid));
                            }
                            CursorShape::Hidden => {}
                        }
                    }
                },
            )
            .size_full(),
        )
        .when(exited, |this| {
            this.child(
                div()
                    .absolute()
                    .bottom(px(PADDING))
                    .right(px(PADDING * 2.0))
                    .text_xs()
                    .text_color(rgb(theme::fg_secondary()))
                    .child(t!("Terminal encerrado — Enter abre outro")),
            )
        })
        .into_any_element()
    }
}

// Text typed on the keyboard, including what dead keys and the IME compose.
// The terminal has no editable text of its own, so the ranges are empty.
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.exited {
            return;
        }
        let Ok(session) = &self.session else {
            return;
        };
        // Enter and Tab come as keys; the input handler would repeat them.
        let text: String = text
            .chars()
            .filter(|character| !matches!(character, '\r' | '\n' | '\t' | '\x08'))
            .collect();
        if text.is_empty() {
            return;
        }
        let mut term = session.term.lock();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        drop(term);
        session.write(text.into_bytes());
        self.used = true;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        _new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // A dead key waiting for its letter: nothing reaches the shell until
        // the composed character arrives through `replace_text_in_range`.
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Where the IME draws its candidates: at the cursor.
        let layout = self.layout.get();
        let session = self.session.as_ref().ok()?;
        let term = session.term.lock();
        let cursor = term.grid().cursor.point;
        let offset = term.grid().display_offset() as i32;
        drop(term);
        let row = (cursor.line.0 + offset).max(0) as f32;
        let position = point(
            layout.origin.x + layout.cell_width * cursor.column.0 as f32,
            layout.origin.y + layout.line_height * row,
        );
        Some(Bounds::new(
            position,
            size(layout.cell_width, layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: it brings `gpui::test`, which shadows `#[test]`.
    use super::{TermMode, default_rgb, key_bytes};
    use alacritty_terminal::vte::ansi::Rgb;
    use gpui::Keystroke;

    fn keystroke(text: &str) -> Keystroke {
        Keystroke::parse(text).expect("valid keystroke")
    }

    #[test]
    fn special_keys_send_what_a_terminal_sends() {
        let normal = TermMode::empty();
        assert_eq!(key_bytes(&keystroke("enter"), normal), Some(b"\r".to_vec()));
        assert_eq!(
            key_bytes(&keystroke("backspace"), normal),
            Some(b"\x7f".to_vec())
        );
        assert_eq!(
            key_bytes(&keystroke("up"), normal),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_bytes(&keystroke("up"), TermMode::APP_CURSOR),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            key_bytes(&keystroke("ctrl-left"), normal),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            key_bytes(&keystroke("delete"), normal),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            key_bytes(&keystroke("shift-tab"), normal),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            key_bytes(&keystroke("f5"), normal),
            Some(b"\x1b[15~".to_vec())
        );
    }

    #[test]
    fn control_letters_become_control_codes() {
        let normal = TermMode::empty();
        assert_eq!(key_bytes(&keystroke("ctrl-d"), normal), Some(vec![4]));
        assert_eq!(key_bytes(&keystroke("ctrl-l"), normal), Some(vec![12]));
        assert_eq!(key_bytes(&keystroke("ctrl-space"), normal), Some(vec![0]));
    }

    #[test]
    fn plain_text_and_altgr_are_left_to_the_input_handler() {
        let normal = TermMode::empty();
        assert_eq!(key_bytes(&keystroke("a"), normal), None);
        let mut altgr = keystroke("ctrl-alt-q");
        altgr.key_char = Some("/".to_string());
        assert_eq!(key_bytes(&altgr, normal), None);
    }

    #[test]
    fn the_256_color_cube_matches_xterm() {
        assert_eq!(default_rgb(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(default_rgb(196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(
            default_rgb(244),
            Rgb {
                r: 128,
                g: 128,
                b: 128
            }
        );
    }
}
