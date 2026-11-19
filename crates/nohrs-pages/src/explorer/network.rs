//! Shared folders: a homelab server, a NAS or another PC sharing over SMB,
//! a mapped drive, a WebDAV site.
//!
//! A share's own folders (`\\server\share\…`) are read like any folder. What
//! Windows does not give as a folder is filled in here, as Explorer does it:
//! the Network place lists the computers found, the drives mapped to shares
//! and the places added by hand; `\\server` lists that server's shares; and a
//! share that asks for a login gets the Windows credential window before it
//! is read. The places added by hand matter most at home: a Samba or TrueNAS
//! box rarely shows up in network discovery, so it is reached by its address.
//!
//! Addresses are also accepted the way other programs write them
//! (`smb://nas/edicao`, `//nas/edicao`, `https://cloud/dav`) and turned into
//! the UNC form Windows reads (`normalize_address`).

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};

use gpui::{App, AppContext as _, BorrowAppContext, Global, Task};
use nohrs_core::telemetry::LogErr;
use nohrs_services::fs::listing::FileEntryDto;
use nohrs_store::{KvKey, KvStore, kv_key};

/// The Network place, and the address a pane shows it at.
pub(crate) const NETWORK: &str = "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}";

/// What the Network place is called.
pub(crate) const NETWORK_NAME: &str = nohrs_core::k!("Rede");

const PLACES_KEY: KvKey = kv_key!("explorer.network_places");

/// The window the credential prompt belongs to, so it opens over it.
static OWNER: AtomicIsize = AtomicIsize::new(0);

/// The places added by hand, for the listing thread.
static ADDED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Notes the main window, which owns the credential prompt. Not called in
/// tests, whose windows have no platform handle.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn set_owner(hwnd: i64) {
    OWNER.store(hwnd as isize, Ordering::Relaxed);
}

/// An address as other programs write it, in the form Windows reads:
/// `smb://host/share/x` and `//host/share/x` become `\\host\share\x`;
/// `https://host/x` and `http://host:8080/x` become the WebDAV redirector's
/// `\\host@SSL\DavWWWRoot\x` and `\\host@8080\DavWWWRoot\x`. Anything else
/// is returned as it was typed, without surrounding quotes or blanks.
pub(crate) fn normalize_address(text: &str) -> String {
    let text = text.trim().trim_matches('"').trim();
    let lower = text.to_lowercase();
    let unc = |rest: &str| -> String {
        let rest = rest.trim_matches('/').replace('/', "\\");
        format!("\\\\{rest}")
    };
    if lower.starts_with("smb://") {
        return unc(&text[6..]);
    }
    for (scheme, ssl) in [("https://", true), ("http://", false)] {
        if lower.starts_with(scheme) {
            let rest = text[scheme.len()..].trim_matches('/');
            let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
            let (host, port) = match authority.rsplit_once(':') {
                Some((host, port)) if port.chars().all(|digit| digit.is_ascii_digit()) => {
                    (host, Some(port))
                }
                _ => (authority, None),
            };
            let mut server = host.to_string();
            if ssl {
                server.push_str("@SSL");
            }
            if let Some(port) =
                port.filter(|port| !(ssl && *port == "443") && !(!ssl && *port == "80"))
            {
                server.push('@');
                server.push_str(port);
            }
            let path = path.trim_matches('/').replace('/', "\\");
            return if path.is_empty() {
                format!("\\\\{server}\\DavWWWRoot")
            } else {
                format!("\\\\{server}\\DavWWWRoot\\{path}")
            };
        }
    }
    if text.starts_with("//") && !text.starts_with("///") {
        return unc(&text[2..]);
    }
    text.to_string()
}

/// Whether `path` is a network address (`\\server…`), not a device or
/// long-path prefix (`\\?\`, `\\.\`).
pub(crate) fn is_unc(path: &str) -> bool {
    path.starts_with("\\\\") && !path.starts_with("\\\\?\\") && !path.starts_with("\\\\.\\")
}

/// The server part of a network address, without the backslashes.
pub(crate) fn server_of(path: &str) -> Option<&str> {
    if !is_unc(path) {
        return None;
    }
    let rest = &path[2..];
    let server = rest.split('\\').next().unwrap_or(rest);
    (!server.is_empty()).then_some(server)
}

/// Whether `path` is a server alone (`\\nas`), which lists its shares.
pub(crate) fn is_server(path: &str) -> bool {
    is_unc(path) && !path[2..].trim_end_matches('\\').contains('\\')
}

/// `\\server\share` of a path inside a share.
pub(crate) fn share_root(path: &str) -> Option<String> {
    if !is_unc(path) {
        return None;
    }
    let mut parts = path[2..].split('\\').filter(|part| !part.is_empty());
    let server = parts.next()?;
    let share = parts.next()?;
    Some(format!("\\\\{server}\\{share}"))
}

/// Where "up" leads from a network address: from a share's root to its
/// server, from a server to the Network place. `None` for any other path.
pub(crate) fn parent_location(path: &str) -> Option<String> {
    if path.eq_ignore_ascii_case(NETWORK) {
        return None;
    }
    if is_server(path) {
        return Some(NETWORK.to_string());
    }
    let root = share_root(path)?;
    if path.trim_end_matches('\\').eq_ignore_ascii_case(&root) {
        return server_of(path).map(|server| format!("\\\\{server}"));
    }
    None
}

/// The rows of a network place that is no folder on disk: the Network place
/// itself, or a server's shares. `None` for any other address. Called from
/// the listing thread: finding computers and asking a server can take seconds.
pub(crate) fn read_location(path: &str) -> Option<Result<Vec<FileEntryDto>, String>> {
    if path.eq_ignore_ascii_case(NETWORK) {
        return Some(Ok(network_entries()));
    }
    if is_server(path) {
        let server = server_of(path)?;
        return Some(platform::shares(server).map(|shares| {
            shares
                .into_iter()
                .map(|share| {
                    folder_row(
                        share.rsplit('\\').next().unwrap_or(&share).to_string(),
                        share.clone(),
                    )
                })
                .collect()
        }));
    }
    None
}

/// Before a share's folder is read: when Windows refuses it for want of a
/// login, the credential window is shown (as Explorer shows it) and the
/// connection made, so the read that follows succeeds. Called from the
/// listing thread; returns at once for anything not on a share.
#[allow(clippy::disallowed_methods)]
pub(crate) fn log_in_if_asked(path: &str) {
    let Some(root) = share_root(path) else {
        return;
    };
    match std::fs::read_dir(path) {
        Err(error) if error.raw_os_error().is_some_and(platform::wants_login) => {
            if !platform::connect_interactive(&root) {
                tracing::info!("no login given for {root}");
            }
        }
        _ => {}
    }
}

fn folder_row(name: String, path: String) -> FileEntryDto {
    FileEntryDto {
        name,
        path,
        kind: "dir".to_string(),
        size: 0,
        modified: 0,
    }
}

// The Network place: the places added by hand first (the ones a homelab is
// reached by), then the mapped drives, then the computers network discovery
// found. Each once, however it was found.
fn network_entries() -> Vec<FileEntryDto> {
    let mut rows: Vec<FileEntryDto> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let added = ADDED.lock().map(|added| added.clone()).unwrap_or_default();
    for path in added {
        if seen.insert(path.to_lowercase()) {
            rows.push(folder_row(display_name(&path), path));
        }
    }
    for (drive, remote) in platform::mapped_drives() {
        if seen.insert(drive.to_lowercase()) {
            rows.push(folder_row(
                format!(
                    "{} ({})",
                    display_name(&remote),
                    drive.trim_end_matches('\\')
                ),
                drive,
            ));
        }
    }
    for computer in platform::computers() {
        if seen.insert(computer.to_lowercase()) {
            rows.push(folder_row(
                computer.trim_start_matches('\\').to_string(),
                computer,
            ));
        }
    }
    rows
}

/// A network address's own name: the share or folder at its end, with the
/// server beside it (`edicao em nas`).
pub(crate) fn display_name(path: &str) -> String {
    let Some(server) = server_of(path) else {
        return path.to_string();
    };
    let last = path
        .trim_end_matches('\\')
        .rsplit('\\')
        .next()
        .unwrap_or(server);
    if last.eq_ignore_ascii_case(server) {
        server.to_string()
    } else {
        nohrs_core::tf!("{share} em {server}", share = last, server = server)
    }
}

/// The places added by hand, kept for the whole app and saved with the tabs.
#[derive(Default)]
pub(crate) struct NetworkPlaces {
    places: Vec<String>,
    store: Option<Arc<dyn KvStore>>,
    save: Option<Task<()>>,
}

impl Global for NetworkPlaces {}

impl NetworkPlaces {
    pub(crate) fn install(store: Option<Arc<dyn KvStore>>, cx: &mut App) {
        if cx.has_global::<Self>() {
            return;
        }
        let places: Vec<String> = store
            .as_ref()
            .and_then(|store| store.get(&PLACES_KEY).log_err().flatten())
            .and_then(|bytes| serde_json::from_slice(&bytes).log_err())
            .unwrap_or_default();
        let places = Self {
            places,
            store,
            save: None,
        };
        places.publish();
        cx.set_global(places);
    }

    fn publish(&self) {
        match ADDED.lock() {
            Ok(mut added) => *added = self.places.clone(),
            Err(error) => tracing::error!("network places poisoned: {error}"),
        }
    }

    /// The places added by hand, in the order they were added.
    pub(crate) fn all(cx: &App) -> Vec<String> {
        cx.try_global::<Self>()
            .map(|places| places.places.clone())
            .unwrap_or_default()
    }

    /// Adds `address` (any spelling `normalize_address` reads). Returns the
    /// address as kept, or `None` when it is not a network address.
    pub(crate) fn add(address: &str, cx: &mut App) -> Option<String> {
        let address = normalize_address(address);
        let address = address.trim_end_matches('\\').to_string();
        if !is_unc(&address) || server_of(&address).is_none() {
            return None;
        }
        let kept = address.clone();
        Self::change(cx, move |places| {
            if !places
                .places
                .iter()
                .any(|place| place.eq_ignore_ascii_case(&address))
            {
                places.places.push(address);
            }
        });
        Some(kept)
    }

    pub(crate) fn remove(address: &str, cx: &mut App) {
        let address = address.to_string();
        Self::change(cx, move |places| {
            places
                .places
                .retain(|place| !place.eq_ignore_ascii_case(&address))
        });
    }

    fn change(cx: &mut App, edit: impl FnOnce(&mut Self)) {
        if !cx.has_global::<Self>() {
            return;
        }
        cx.update_global(|places: &mut Self, cx| {
            edit(places);
            places.publish();
            let (Some(store), Ok(bytes)) =
                (places.store.clone(), serde_json::to_vec(&places.places))
            else {
                return;
            };
            places.save = Some(cx.background_spawn(async move {
                store.put(&PLACES_KEY, &bytes).log_err();
            }));
        });
        cx.refresh_windows();
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! The network provider (WNet) and the Shell's Network folder have no
    //! safe binding.

    use std::sync::atomic::Ordering;

    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, HANDLE, HWND, NO_ERROR,
        WIN32_ERROR,
    };
    use windows::Win32::NetworkManagement::WNet::{
        CONNECT_INTERACTIVE, CONNECT_PROMPT, CONNECT_TEMPORARY, NETRESOURCEW, RESOURCE_GLOBALNET,
        RESOURCETYPE_DISK, WNET_OPEN_ENUM_USAGE, WNetAddConnection3W, WNetCloseEnum,
        WNetEnumResourceW, WNetGetConnectionW, WNetOpenEnumW,
    };
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    use windows::core::{HSTRING, PCWSTR, PWSTR};

    // ERROR_LOGON_FAILURE, ERROR_ACCOUNT_RESTRICTION, ERROR_INVALID_PASSWORD,
    // ERROR_NOT_AUTHENTICATED, ERROR_SESSION_CREDENTIAL_CONFLICT,
    // ERROR_BAD_USERNAME and access denied: what a share answers when it
    // wants someone else, or anyone, to log in.
    const LOGIN_ERRORS: [i32; 7] = [1326, 1327, 86, 1244, 1219, 2202, 5];
    const DRIVE_REMOTE: u32 = 4;

    pub fn wants_login(code: i32) -> bool {
        LOGIN_ERRORS.contains(&code)
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Shows the Windows credential window for `remote` and connects with
    /// what is typed there (remembered for the session, as Explorer does
    /// unless "Remember" is ticked in the window). Whether it connected.
    pub fn connect_interactive(remote: &str) -> bool {
        let mut name = wide(remote);
        let resource = NETRESOURCEW {
            dwType: RESOURCETYPE_DISK,
            lpRemoteName: PWSTR(name.as_mut_ptr()),
            ..Default::default()
        };
        let owner = super::OWNER.load(Ordering::Relaxed);
        let owner = (owner != 0).then(|| HWND(owner as *mut core::ffi::c_void));
        let result = unsafe {
            WNetAddConnection3W(
                owner,
                &resource,
                PCWSTR::null(),
                PCWSTR::null(),
                CONNECT_INTERACTIVE | CONNECT_PROMPT | CONNECT_TEMPORARY,
            )
        };
        result == NO_ERROR
    }

    /// The disk shares of `server` (`nas`), as `\\nas\share`. Hidden shares
    /// (`C$`, `IPC$`) are left out, as Explorer leaves them. A server that
    /// wants a login gets the credential window first.
    pub fn shares(server: &str) -> Result<Vec<String>, String> {
        let address = format!("\\\\{server}");
        let listed = match enumerate(&address) {
            Err(code) if wants_login(code.0 as i32) || code == ERROR_ACCESS_DENIED => {
                if connect_interactive(&format!("{address}\\IPC$")) {
                    enumerate(&address)
                } else {
                    Err(code)
                }
            }
            listed => listed,
        };
        match listed {
            Ok(shares) => Ok(shares
                .into_iter()
                .filter(|share| !share.ends_with('$'))
                .collect()),
            Err(code) => Err(std::io::Error::from_raw_os_error(code.0 as i32).to_string()),
        }
    }

    fn enumerate(address: &str) -> Result<Vec<String>, WIN32_ERROR> {
        let mut name = wide(address);
        let container = NETRESOURCEW {
            dwScope: RESOURCE_GLOBALNET,
            dwType: RESOURCETYPE_DISK,
            lpRemoteName: PWSTR(name.as_mut_ptr()),
            ..Default::default()
        };
        let mut handle = HANDLE::default();
        let opened = unsafe {
            WNetOpenEnumW(
                RESOURCE_GLOBALNET,
                RESOURCETYPE_DISK,
                WNET_OPEN_ENUM_USAGE(0),
                Some(&container),
                &mut handle,
            )
        };
        if opened != NO_ERROR {
            return Err(opened);
        }
        let mut names = Vec::new();
        // u64 for the alignment of the NETRESOURCEW records written into it.
        let mut buffer: Vec<u64> = vec![0; 2048];
        let result = loop {
            let mut count = u32::MAX;
            let mut size = (buffer.len() * 8) as u32;
            let read = unsafe {
                WNetEnumResourceW(handle, &mut count, buffer.as_mut_ptr().cast(), &mut size)
            };
            if read == ERROR_NO_MORE_ITEMS {
                break Ok(names);
            }
            if read == ERROR_MORE_DATA {
                buffer = vec![0; (size as usize).div_ceil(8) + 1];
                continue;
            }
            if read != NO_ERROR {
                break Err(read);
            }
            let records = unsafe {
                std::slice::from_raw_parts(buffer.as_ptr().cast::<NETRESOURCEW>(), count as usize)
            };
            for record in records {
                if !record.lpRemoteName.is_null()
                    && let Ok(remote) = unsafe { record.lpRemoteName.to_string() }
                {
                    names.push(remote);
                }
            }
        };
        let closed = unsafe { WNetCloseEnum(handle) };
        if closed != NO_ERROR {
            tracing::debug!("WNetCloseEnum: {closed:?}");
        }
        result
    }

    /// The drive letters mapped to a share, with the share each points at.
    pub fn mapped_drives() -> Vec<(String, String)> {
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|bit| mask & (1 << bit) != 0)
            .filter_map(|bit| {
                let letter = (b'A' + bit) as char;
                let root = format!("{letter}:\\");
                if unsafe { GetDriveTypeW(&HSTRING::from(root.as_str())) } != DRIVE_REMOTE {
                    return None;
                }
                let mut buffer = vec![0u16; 1024];
                let mut length = buffer.len() as u32;
                let local = HSTRING::from(format!("{letter}:"));
                let found = unsafe {
                    WNetGetConnectionW(&local, Some(PWSTR(buffer.as_mut_ptr())), &mut length)
                };
                let remote = if found == NO_ERROR {
                    let end = buffer
                        .iter()
                        .position(|&unit| unit == 0)
                        .unwrap_or(buffer.len());
                    String::from_utf16_lossy(&buffer[..end])
                } else {
                    root.clone()
                };
                Some((root, remote))
            })
            .collect()
    }

    /// The computers network discovery found, as `\\NAME`. Media servers
    /// and other devices that are no file server are left out.
    pub fn computers() -> Vec<String> {
        super::super::places::network_children()
            .into_iter()
            .map(|place| place.parsing_name)
            .filter(|name| super::is_unc(name) && super::is_server(name))
            .collect()
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn wants_login(_code: i32) -> bool {
        false
    }
    pub fn connect_interactive(_remote: &str) -> bool {
        false
    }
    pub fn shares(_server: &str) -> Result<Vec<String>, String> {
        Err("shares are listed on Windows only".into())
    }
    pub fn mapped_drives() -> Vec<(String, String)> {
        Vec::new()
    }
    pub fn computers() -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_from_other_programs_become_unc() {
        assert_eq!(
            normalize_address("smb://nas/edicao/2026"),
            r"\\nas\edicao\2026"
        );
        assert_eq!(
            normalize_address("//192.168.1.10/media/"),
            r"\\192.168.1.10\media"
        );
        assert_eq!(normalize_address(" \"\\\\nas\\edicao\" "), r"\\nas\edicao");
        assert_eq!(
            normalize_address("https://cloud.example/remote.php/dav"),
            r"\\cloud.example@SSL\DavWWWRoot\remote.php\dav"
        );
        assert_eq!(
            normalize_address("http://nas:5005/"),
            r"\\nas@5005\DavWWWRoot"
        );
        assert_eq!(normalize_address(r"C:\Users"), r"C:\Users");
    }

    #[test]
    fn up_from_a_share_goes_to_its_server_then_to_the_network() {
        assert_eq!(parent_location(r"\\nas\edicao").as_deref(), Some(r"\\nas"));
        assert_eq!(parent_location(r"\\nas\edicao\").as_deref(), Some(r"\\nas"));
        assert_eq!(parent_location(r"\\nas").as_deref(), Some(NETWORK));
        assert_eq!(parent_location(r"\\nas\edicao\2026"), None);
        assert_eq!(parent_location(r"C:\Users"), None);
        assert!(!is_unc(r"\\?\C:\long"));
        assert!(is_server(r"\\nas\"));
        assert_eq!(
            share_root(r"\\nas\edicao\2026\a.mp4").as_deref(),
            Some(r"\\nas\edicao")
        );
    }

    #[test]
    fn a_share_is_named_with_its_server() {
        assert_eq!(display_name(r"\\nas"), "nas");
    }
}
