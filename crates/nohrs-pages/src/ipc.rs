//! The local channel other programs reach a running Nohrs through: a second
//! `LumaFiles.exe <path>` handing its path over instead of opening another window,
//! and the Listary plugin (`integrations/listary`) asking which folder is open
//! and telling Nohrs where to go.
//!
//! A TCP socket on 127.0.0.1 with a random port, described in
//! `userdata/runtime/ipc.json` in portable mode, otherwise `%LOCALAPPDATA%\nohrs\ipc.json`, together with a token generated per run.
//! The token is what keeps another local user, or a web page poking at
//! localhost, from driving the explorer: only someone who can read the file in
//! this user's profile has it. Each connection carries one JSON request line
//! and gets one JSON reply line.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{AsyncWindowContext, Context, WeakEntity, Window};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::root::RootView;

/// What `ipc.json` records about the running instance.
#[derive(Serialize, Deserialize)]
pub struct Endpoint {
    /// Port the instance listens on, on 127.0.0.1.
    pub port: u16,
    /// Secret every request must carry.
    pub token: String,
    /// Process id, so a client can hand it the right to come to the front.
    pub pid: u32,
    /// The Explorer window, so the Listary plugin binds to it and not to the
    /// launcher, which belongs to the same process.
    pub hwnd: i64,
    /// The executable, so a client can start Nohrs when it is not running.
    pub exe: String,
}

/// A request, as one JSON line.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum Request {
    /// Is anyone there.
    Ping,
    /// Bring the window to the front, showing nothing new.
    Show,
    /// Show a folder, or a file selected inside its folder, in a new tab.
    Open { path: String },
    /// Point an existing tab at a folder, or at a file inside one.
    Navigate { tab: u64, path: String },
    /// The active tab and its folder.
    Current,
    /// Every tab and its folder.
    Tabs,
}

#[derive(Deserialize)]
struct Envelope {
    token: String,
    #[serde(flatten)]
    request: Request,
}

fn endpoint_file() -> Option<PathBuf> {
    Some(nohrs_core::config::paths::runtime_dir().join("ipc.json"))
}

fn legacy_endpoint_file() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("nohrs").join("ipc.json"))
}
/// The native handle of `window`, for Win32 calls that need an owner.
pub(crate) fn window_hwnd(window: &Window) -> Option<i64> {
    platform::window_handle(window)
}

/// Reads the endpoint the running instance (if any) left behind.
// Only `main` calls this, deciding whether to hand off to a running instance
// before any UI exists, so there is no frame to stall.
#[allow(clippy::disallowed_methods)]
pub fn read_endpoint() -> Option<Endpoint> {
    let active_file = endpoint_file()?;
    let text = std::fs::read_to_string(&active_file).ok().or_else(|| {
        legacy_endpoint_file()
            .filter(|legacy| legacy != &active_file)
            .and_then(|legacy| std::fs::read_to_string(legacy).ok())
    })?;
    serde_json::from_str(&text).ok()
}

/// Sends one request to the running instance and returns its reply, or `None`
/// when no instance answers (none is running, or the file is stale).
pub fn send(request: Value) -> Option<Value> {
    let endpoint = read_endpoint()?;
    let mut stream = TcpStream::connect_timeout(
        &([127, 0, 0, 1], endpoint.port).into(),
        Duration::from_millis(500),
    )
    .ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut request = request;
    request["token"] = Value::String(endpoint.token);
    // The instance brings itself forward to show what was asked for; Windows
    // only lets it if the process the user is interacting with allows it.
    platform::allow_foreground(endpoint.pid);
    writeln!(stream, "{request}").ok()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).ok()?;
    serde_json::from_str(&reply).ok()
}

/// Hands `path` to the running instance to show in a new tab. Returns `false`
/// when no instance took it, so the caller opens a window of its own.
pub fn request_open(path: &Path) -> bool {
    // Made absolute here: the running instance resolves it against its own
    // working directory, not the caller's.
    let Ok(path) = std::path::absolute(path) else {
        return false;
    };
    let reply = send(json!({ "cmd": "open", "path": path.to_string_lossy() }));
    reply.is_some_and(|reply| reply["ok"] == true)
}

/// Brings the running instance to the front. Returns `false` when there is
/// none.
pub fn request_show() -> bool {
    send(json!({ "cmd": "show" })).is_some_and(|reply| reply["ok"] == true)
}

/// Returns the active explorer tab's folder from the running instance.
pub fn current_path() -> Result<String, String> {
    let reply = send(json!({ "cmd": "current" }))
        .ok_or_else(|| "no running LumaFiles window responded".to_string())?;
    if reply["ok"] != true {
        return Err(reply["error"].as_str().unwrap_or("no active explorer tab").to_string());
    }
    reply["folder"].as_str().map(str::to_owned)
        .ok_or_else(|| "invalid response from LumaFiles".to_string())
}

/// Whether a Prime Files window answers on the channel, whatever it says
/// to a command it does not know: it is there, and nothing is asked of it.
pub fn is_running() -> bool {
    send(json!({ "cmd": "ping" })).is_some()
}

/// Splits a path into the folder to show and the entry to select in it.
pub fn folder_and_selection(path: &Path) -> Option<(String, Option<String>)> {
    let path = std::fs::canonicalize(path).ok()?;
    let path = strip_verbatim(&path);
    if path.is_dir() {
        Some((path.to_string_lossy().into_owned(), None))
    } else {
        let folder = path.parent()?.to_string_lossy().into_owned();
        Some((folder, Some(path.to_string_lossy().into_owned())))
    }
}

// `canonicalize` on Windows answers `\\?\C:\...`, which the listing would show
// as a path nobody typed.
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => PathBuf::from(format!(r"\\{rest}")),
        None => PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text)),
    }
}

type Pending = (Request, async_channel::Sender<Value>);

/// Starts listening for requests and records the endpoint for clients. A
/// failure leaves Nohrs working without the channel, and is logged.
pub fn serve(window: &mut Window, cx: &mut Context<RootView>) {
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!("ipc disabled, could not listen: {error}");
            return;
        }
    };
    let Ok(address) = listener.local_addr() else {
        return;
    };
    let token = new_token();
    let endpoint = Endpoint {
        port: address.port(),
        token: token.clone(),
        pid: std::process::id(),
        hwnd: platform::window_handle(window).unwrap_or(0),
        exe: std::env::current_exe()
            .map(|exe| exe.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let (sender, receiver) = async_channel::unbounded::<Pending>();
    let spawned = std::thread::Builder::new()
        .name("nohrs-ipc".into())
        .spawn(move || {
            // Written here rather than before spawning: this runs on the UI
            // thread, and the file is disk I/O. Clients only find the channel
            // once it is written, so nothing can arrive before.
            if let Err(error) = write_endpoint(&endpoint) {
                tracing::warn!("ipc disabled, could not write the endpoint file: {error}");
                return;
            }
            accept(listener, token, sender);
        });
    if let Err(error) = spawned {
        tracing::warn!("ipc disabled, could not start the listener: {error}");
        return;
    }

    cx.spawn_in(
        window,
        async move |this: WeakEntity<RootView>, cx: &mut AsyncWindowContext| {
            while let Ok((request, reply)) = receiver.recv().await {
                let answer = this
                    .update_in(cx, |root, window, cx| root.handle_ipc(request, window, cx))
                    .unwrap_or_else(|_| json!({ "ok": false, "error": "window closed" }));
                reply.try_send(answer).ok();
            }
        },
    )
    .detach();
}

fn accept(listener: TcpListener, token: String, sender: async_channel::Sender<Pending>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let token = token.clone();
        let sender = sender.clone();
        // One short-lived thread per connection: requests are rare and tiny,
        // and a client that connects and says nothing must not block the next.
        std::thread::spawn(move || {
            if let Err(error) = answer(stream, &token, &sender) {
                tracing::debug!("ipc connection dropped: {error}");
            }
        });
    }
}

fn answer(
    stream: TcpStream,
    token: &str,
    sender: &async_channel::Sender<Pending>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let reply = match serde_json::from_str::<Envelope>(&line) {
        Ok(envelope) if envelope.token == token => {
            let (reply_sender, reply) = async_channel::bounded(1);
            if sender
                .send_blocking((envelope.request, reply_sender))
                .is_err()
            {
                json!({ "ok": false, "error": "shutting down" })
            } else {
                reply
                    .recv_blocking()
                    .unwrap_or_else(|_| json!({ "ok": false, "error": "no reply" }))
            }
        }
        Ok(_) => json!({ "ok": false, "error": "bad token" }),
        Err(error) => json!({ "ok": false, "error": format!("bad request: {error}") }),
    };
    let mut stream = stream;
    writeln!(stream, "{reply}")
}

// Runs on the ipc thread (see `serve`), never the UI thread.
#[allow(clippy::disallowed_methods)]
fn write_endpoint(endpoint: &Endpoint) -> std::io::Result<()> {
    let Some(file) = endpoint_file() else {
        return Err(std::io::Error::other("LOCALAPPDATA is not set"));
    };
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let text = serde_json::to_string_pretty(endpoint).map_err(std::io::Error::other)?;
    // Written whole and renamed into place, so a client never reads half a file.
    let partial = file.with_extension("json.tmp");
    std::fs::write(&partial, text)?;
    std::fs::rename(partial, file)
}

fn new_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    // `RandomState` is seeded from the OS's random source per instance; two
    // draws give 128 bits without another dependency.
    let draw = || RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}", draw(), draw())
}

impl RootView {
    /// Shows `path` as a just-started Nohrs was asked to on its command line.
    pub fn open_initial_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        match folder_and_selection(path) {
            Some((folder, select)) => {
                let explorer = self.explorer_entity();
                explorer.update(cx, |page, cx| page.open_path(folder, select, window, cx));
            }
            None => tracing::warn!("cannot open {}: no such path", path.display()),
        }
    }

    fn handle_ipc(
        &mut self,
        request: Request,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Value {
        let explorer = self.explorer_entity();
        match request {
            Request::Ping => json!({ "ok": true }),
            Request::Show => {
                window.activate_window();
                json!({ "ok": true })
            }
            Request::Open { path } => match folder_and_selection(Path::new(&path)) {
                Some((folder, select)) => {
                    self.show_explorer(cx);
                    explorer.update(cx, |page, cx| page.open_path(folder, select, window, cx));
                    window.activate_window();
                    json!({ "ok": true })
                }
                None => json!({ "ok": false, "error": "no such path" }),
            },
            Request::Navigate { tab, path } => match folder_and_selection(Path::new(&path)) {
                Some((folder, select)) => {
                    let found = explorer.update(cx, |page, cx| {
                        page.navigate_tab(tab, folder, select, window, cx)
                    });
                    json!({ "ok": found })
                }
                None => json!({ "ok": false, "error": "no such path" }),
            },
            Request::Current => {
                let tabs = explorer.read(cx).tabs(cx);
                match tabs.into_iter().find(|(_, _, active)| *active) {
                    Some((id, folder, _)) => json!({ "ok": true, "tab": id, "folder": folder }),
                    None => json!({ "ok": false }),
                }
            }
            Request::Tabs => {
                let tabs: Vec<Value> = explorer
                    .read(cx)
                    .tabs(cx)
                    .into_iter()
                    .map(|(id, folder, active)| json!({ "tab": id, "folder": folder, "active": active }))
                    .collect();
                json!({ "ok": true, "tabs": tabs })
            }
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! The two Win32 calls the channel needs. `unsafe` stays here.

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    pub fn window_handle(window: &gpui::Window) -> Option<i64> {
        // Through the trait: `Window` has an inherent `window_handle` of its own
        // that answers gpui's handle, not the platform one.
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as i64),
            _ => None,
        }
    }

    pub fn allow_foreground(pid: u32) {
        // A refusal only means the instance comes up behind the current
        // window; nothing to recover.
        unsafe { windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid) }.ok();
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn window_handle(_window: &gpui::Window) -> Option<i64> {
        None
    }

    pub fn allow_foreground(_pid: u32) {}
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_from_their_wire_form() {
        let envelope: Envelope =
            serde_json::from_str(r#"{"token":"t","cmd":"open","path":"C:\\x"}"#).unwrap();
        assert_eq!(envelope.token, "t");
        assert!(matches!(envelope.request, Request::Open { path } if path == r"C:\x"));
        let envelope: Envelope =
            serde_json::from_str(r#"{"token":"t","cmd":"navigate","tab":7,"path":"D:\\"}"#)
                .unwrap();
        assert!(matches!(envelope.request, Request::Navigate { tab: 7, .. }));
        let envelope: Envelope = serde_json::from_str(r#"{"token":"t","cmd":"current"}"#).unwrap();
        assert!(matches!(envelope.request, Request::Current));
        assert!(serde_json::from_str::<Envelope>(r#"{"token":"t","cmd":"format_c"}"#).is_err());
    }

    #[test]
    fn a_file_opens_its_folder_with_it_selected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let (folder, select) = folder_and_selection(&file).unwrap();
        assert!(!folder.starts_with(r"\\?\"));
        assert!(crate::explorer::same_path(
            &folder,
            &strip_verbatim(&std::fs::canonicalize(dir.path()).unwrap()).to_string_lossy()
        ));
        assert!(select.unwrap().to_lowercase().ends_with("a.txt"));
        let (folder, select) = folder_and_selection(dir.path()).unwrap();
        assert!(select.is_none());
        assert!(!folder.is_empty());
        assert!(folder_and_selection(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn tokens_differ_between_draws() {
        assert_ne!(new_token(), new_token());
        assert_eq!(new_token().len(), 32);
    }
}
