//! The Windows half of the launch service (§8.2, §8.3).
//!
//! Everything here is a system call, so `unsafe` lives in this file and every
//! block says what it is that makes the call sound. The functions are called
//! from the pages layer's dedicated worker thread (§8.4), never from the
//! foreground.
//!
//! `ShellExecuteExW` is used for every document-like start — `.exe`, `.lnk`,
//! URLs — rather than `CreateProcessW` plus a shortcut parser plus a UAC path:
//! one call already covers all three, and it is the call Explorer itself makes.

use std::path::{Path, PathBuf};

use nohrs_core::config::quick_launch::Item;
use nohrs_core::telemetry::LogErr;
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HWND, LPARAM, MAX_PATH};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, HGDIOBJ, SelectObject,
};
use windows::Win32::System::Diagnostics::Debug::{
    FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU, keybd_event,
};
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DI_NORMAL, DestroyIcon, DrawIconEx, EnumWindows, GW_OWNER, GetIconInfo,
    GetWindow, GetWindowThreadProcessId, HICON, ICONINFO, IsIconic, IsWindowVisible,
    LR_DEFAULTCOLOR, PrivateExtractIconsW, SW_RESTORE, SW_SHOWNORMAL, SetForegroundWindow,
    ShowWindow, SwitchToThisWindow,
};
use windows::core::{BOOL, Error as WindowsError, PCWSTR, PWSTR};

use crate::launch::{
    ExpandContext, Health, KIND_APP, KIND_FILE, KIND_URL, LaunchAction, LaunchError, LaunchMode,
    LaunchOutcome, LaunchRequest, expand, item_path, validate_url,
};

/// A document (`file`): the Shell picks the program and the verb is left to it
/// (§8.2). No activation: a document is never "already running".
pub(crate) struct FileAction;

impl LaunchAction for FileAction {
    fn kind(&self) -> &str {
        KIND_FILE
    }

    fn health(&self, item: &Item, context: &ExpandContext) -> Health {
        match item_path(item, context) {
            Ok(path) if path.is_file() => Health::Ok,
            Ok(_) => Health::Missing,
            Err(LaunchError::Expand(error)) => Health::Invalid(error.to_string()),
            Err(error) => Health::Invalid(error.to_string()),
        }
    }

    fn launch(&self, item: &Item, request: &LaunchRequest) -> Result<LaunchOutcome, LaunchError> {
        let path = item_path(item, &request.expand)?;
        if !path.is_file() {
            return Err(LaunchError::NotFound(path));
        }
        let directory = match item.cwd.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => Some(expand(raw, &request.expand)?),
            _ => path.parent().map(Path::to_path_buf),
        };
        // `administrative` false leaves `lpVerb` null, which is what §8.2 asks
        // for: the Shell uses the type's default verb.
        start(
            &path.to_string_lossy(),
            item.args.as_deref(),
            directory.as_deref(),
            false,
        )
    }
}

/// The icon at `index` inside `path` (`"exe,索引"`, §9).
///
/// The Shell extracts it at the size the window wants; the pixels come out of a
/// top-down 32-bit DIB as the premultiplied BGRA the icon path expects.
#[allow(unsafe_code)] // the Shell's icon extraction and GDI have no safe bindings
pub fn indexed_icon(path: &str, index: u32, edge: u32) -> Option<(Vec<u8>, u32, u32)> {
    let edge_px = i32::try_from(edge).ok()?;
    let index = i32::try_from(index).ok()?;
    // The binding takes the file name as the classic fixed 260-wide buffer.
    let mut name = [0u16; 260];
    for (target, source) in name.iter_mut().zip(path.encode_utf16()) {
        *target = source;
    }
    let mut icon = HICON::default();
    let mut identifier = 0u32;
    // SAFETY: a buffer this call owns and the icon slot it fills; the icon it
    // returns is destroyed below.
    let extracted = unsafe {
        PrivateExtractIconsW(
            &name,
            index,
            edge_px,
            edge_px,
            Some(core::slice::from_mut(&mut icon)),
            Some(&raw mut identifier),
            LR_DEFAULTCOLOR.0,
        )
    };
    if extracted == 0 || icon.is_invalid() {
        return None;
    }
    // SAFETY: the icon came from the extraction above and is ours to destroy.
    let pixels = unsafe { pixels_of_icon(icon, edge_px) };
    let _ = unsafe { DestroyIcon(icon) };
    pixels
}

/// Draws one icon into a top-down 32-bit DIB and copies the pixels out.
///
/// Unsafe because GDI has no safe bindings; every handle created here is
/// released before the function returns.
#[allow(unsafe_code)]
unsafe fn pixels_of_icon(icon: HICON, edge: i32) -> Option<(Vec<u8>, u32, u32)> {
    let mut information = ICONINFO::default();
    // SAFETY: the icon is valid and the structure is this call's output.
    unsafe { GetIconInfo(icon, &mut information) }.ok()?;
    let coloured = information.hbmColor;
    let mask = information.hbmMask;

    // SAFETY: a screen-compatible DC with no bitmap of its own, deleted below.
    let device = unsafe { CreateCompatibleDC(None) };
    if device.is_invalid() {
        // SAFETY: both bitmaps were made by `GetIconInfo`.
        unsafe {
            let _ = DeleteObject(HGDIOBJ(coloured.0));
            let _ = DeleteObject(HGDIOBJ(mask.0));
        }
        return None;
    }
    let header = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(core::mem::size_of::<BITMAPINFOHEADER>()).unwrap_or(0),
            biWidth: edge,
            // Top-down, so the bytes come out in reading order.
            biHeight: -edge,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
    // SAFETY: `bits` is the out parameter of this call and is valid while the
    // bitmap lives.
    let bitmap = unsafe {
        CreateDIBSection(
            Some(device),
            &header,
            DIB_RGB_COLORS,
            &raw mut bits,
            None,
            0,
        )
    };
    let Some(bitmap) = bitmap.ok() else {
        // SAFETY: the handles were made by this function or by `GetIconInfo`.
        unsafe {
            let _ = DeleteDC(device);
            let _ = DeleteObject(HGDIOBJ(coloured.0));
            let _ = DeleteObject(HGDIOBJ(mask.0));
        }
        return None;
    };

    // SAFETY: every handle belongs to this function; the old object is put back
    // before the DC goes away, and the pixels are copied before the bitmap is
    // deleted.
    let pixels = unsafe {
        let previous = SelectObject(device, HGDIOBJ(bitmap.0));
        let _ = DrawIconEx(device, 0, 0, icon, edge, edge, 0, None, DI_NORMAL);
        SelectObject(device, previous);
        let pixels = match (usize::try_from(edge), bits.is_null()) {
            (Ok(edge), false) => {
                let length = edge.saturating_mul(edge).saturating_mul(4);
                Some(core::slice::from_raw_parts(bits as *const u8, length).to_vec())
            }
            _ => None,
        };
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(device);
        let _ = DeleteObject(HGDIOBJ(coloured.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        pixels
    };

    Some((
        premultiply_bgra(pixels?),
        u32::try_from(edge).ok()?,
        u32::try_from(edge).ok()?,
    ))
}

/// The DIB gives straight BGRA; the icon path works in premultiplied values.
fn premultiply_bgra(mut pixels: Vec<u8>) -> Vec<u8> {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let alpha = u16::from(pixel[3]);
        let scale = |channel: u8| ((u16::from(channel) * alpha + 127) / 255) as u8;
        pixel[0] = scale(pixel[0]);
        pixel[1] = scale(pixel[1]);
        pixel[2] = scale(pixel[2]);
    }
    pixels
}

/// An application or shortcut, started through the Shell.
pub(crate) struct AppAction;

impl LaunchAction for AppAction {
    fn kind(&self) -> &str {
        KIND_APP
    }

    fn health(&self, item: &Item, context: &crate::launch::ExpandContext) -> Health {
        match item_path(item, context) {
            Ok(path) if path.is_file() => Health::Ok,
            Ok(_) => Health::Missing,
            Err(error) => Health::Invalid(error.to_string()),
        }
    }

    fn launch(&self, item: &Item, request: &LaunchRequest) -> Result<LaunchOutcome, LaunchError> {
        let path = item_path(item, &request.expand)?;
        if !path.is_file() {
            return Err(LaunchError::NotFound(path));
        }
        let administrative = item.admin || request.mode == LaunchMode::Admin;
        // A shortcut is handed to the Shell whole: its own target, arguments
        // and "run as administrator" flag are the Shell's to read (§8.2).
        let is_shortcut = path
            .extension()
            .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("lnk"));
        let wants_new_instance = request.mode == LaunchMode::ForceNew
            || matches!(
                request.on_running,
                nohrs_core::config::quick_launch::OnRunning::New
            );
        // Activation is for the ordinary case only: an elevated start has to go
        // through UAC, and a shortcut's target is not known without parsing it,
        // which this milestone does not do (§8.3).
        if !administrative
            && !wants_new_instance
            && !is_shortcut
            && let Some(process) = process_id_for(&path)
            && activate_main_window(process)
        {
            return Ok(LaunchOutcome::Activated);
        }

        let directory = match item.cwd.as_deref().map(str::trim) {
            Some(raw) if !raw.is_empty() => Some(expand(raw, &request.expand)?),
            _ => path.parent().map(Path::to_path_buf),
        };
        start(
            &path.to_string_lossy(),
            item.args.as_deref(),
            directory.as_deref(),
            administrative,
        )
    }
}

/// A `http` / `https` / `mailto` address.
pub(crate) struct UrlAction;

impl LaunchAction for UrlAction {
    fn kind(&self) -> &str {
        KIND_URL
    }

    fn health(&self, item: &Item, _context: &crate::launch::ExpandContext) -> Health {
        match item.path.as_deref() {
            Some(raw) => match validate_url(raw) {
                Ok(()) => Health::Ok,
                Err(error) => Health::Invalid(error.to_string()),
            },
            None => Health::Invalid("the item has no address".to_string()),
        }
    }

    fn launch(&self, item: &Item, _request: &LaunchRequest) -> Result<LaunchOutcome, LaunchError> {
        let address = item.path.as_deref().unwrap_or_default().trim();
        validate_url(address)?;
        start(address, None, None, false)
    }
}

/// Hands one item to `ShellExecuteExW` and maps the result.
#[allow(unsafe_code)]
fn start(
    target: &str,
    arguments: Option<&str>,
    directory: Option<&Path>,
    administrative: bool,
) -> Result<LaunchOutcome, LaunchError> {
    let verb = administrative.then(|| wide("runas"));
    let file = wide(target);
    let parameters = arguments
        .map(str::trim)
        .filter(|arguments| !arguments.is_empty())
        .map(wide);
    let working_directory = directory.map(|path| wide(&path.to_string_lossy()));

    let mut information = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(std::mem::size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb
            .as_ref()
            .map_or(PCWSTR::null(), |verb| PCWSTR(verb.as_ptr())),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: parameters
            .as_ref()
            .map_or(PCWSTR::null(), |parameters| PCWSTR(parameters.as_ptr())),
        lpDirectory: working_directory
            .as_ref()
            .map_or(PCWSTR::null(), |directory| PCWSTR(directory.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // SAFETY: `information` is a fully initialised `SHELLEXECUTEINFOW` whose
    // `cbSize` matches it, and every string it points at outlives the call.
    let started = unsafe { ShellExecuteExW(&raw mut information) };
    if let Err(error) = started {
        let code = win32_code(&error);
        // The user answering "no" to the elevation prompt is a decision, not a
        // failure (§8.2): nothing is shown for it.
        if code == ERROR_CANCELLED.0 {
            return Ok(LaunchOutcome::Cancelled);
        }
        return Err(LaunchError::Os {
            code,
            message: system_message(code),
        });
    }
    // The process handle is only handed back because the structure has room
    // for it; nothing here waits on the process, so it is closed at once.
    if !information.hProcess.is_invalid() {
        // SAFETY: a live handle this call owns, closed exactly once.
        unsafe { CloseHandle(information.hProcess) }.ok();
    }
    Ok(LaunchOutcome::Started)
}

/// A NUL-terminated UTF-16 copy of `text`.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The Win32 code inside a `windows` error.
///
/// The crate wraps Win32 failures in an `HRESULT` (`0x8007xxxx`, negative as
/// `i32`), the trap PERFORMANCE.md §2 records: read as a plain integer it is
/// never 1223, and "the user said no" becomes a spurious error dialog.
fn win32_code(error: &WindowsError) -> u32 {
    let hresult = error.code().0 as u32;
    if hresult & 0xFFFF_0000 == 0x8007_0000 {
        hresult & 0xFFFF
    } else {
        hresult
    }
}

/// The system's own description of `code`, without its trailing newline.
#[allow(unsafe_code)]
fn system_message(code: u32) -> String {
    let mut buffer = [0u16; 512];
    // SAFETY: the buffer is writable for its whole length, which is what `nsize`
    // says, and the call writes at most that many units.
    let length = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            None,
            code,
            0,
            PWSTR(buffer.as_mut_ptr()),
            u32::try_from(buffer.len()).unwrap_or(0),
            None,
        )
    };
    if length == 0 {
        return format!("error {code}");
    }
    let end = usize::try_from(length).unwrap_or(0).min(buffer.len());
    String::from_utf16_lossy(&buffer[..end]).trim().to_string()
}

/// The process running `executable`, compared by full path without regard to
/// case (§8.3).
#[allow(unsafe_code)]
fn process_id_for(executable: &Path) -> Option<u32> {
    let wanted = comparable(executable);
    // §8.3: the scan may take 100 ms at most, after which the caller starts a
    // new instance rather than keeping the user waiting.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
    // SAFETY: a snapshot handle, closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    let mut entry = PROCESSENTRY32W {
        dwSize: u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: `entry.dwSize` describes the structure the call writes into.
    let mut found = unsafe { Process32FirstW(snapshot, &raw mut entry) }.is_ok();
    let mut process = None;
    while found {
        if std::time::Instant::now() >= deadline {
            // §8.3: past the budget the caller starts a new instance instead.
            tracing::debug!(target: "launch", "the already-running scan hit the 100 ms budget");
            break;
        }
        if let Some(path) = process_image(entry.th32ProcessID)
            && comparable(&path).eq_ignore_ascii_case(&wanted)
        {
            process = Some(entry.th32ProcessID);
            break;
        }
        // SAFETY: same structure, same size, advanced by the call itself.
        found = unsafe { Process32NextW(snapshot, &raw mut entry) }.is_ok();
    }
    // SAFETY: the snapshot is this call's to close, once.
    unsafe { CloseHandle(snapshot) }.ok();
    process
}

/// `path` in the form both sides of the comparison can be written in.
///
/// `canonicalize` answers `\\?\C:\.` and the process image is `C:\.`; without
/// this the two would never match and an application the user already has open
/// would be started a second time.
fn comparable(path: &Path) -> String {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
    }
}

/// The full path of the executable of `process_id`, when it can be read.
#[allow(unsafe_code)]
fn process_image(process_id: u32) -> Option<PathBuf> {
    // SAFETY: the handle is opened for the one query below and closed here.
    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
    let mut buffer = [0u16; MAX_PATH as usize * 4];
    let mut length = u32::try_from(buffer.len()).unwrap_or(0);
    // SAFETY: `length` says how many units the buffer holds; the call writes at
    // most that many and updates the count.
    let read = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    // SAFETY: the process handle is this call's to close, once.
    unsafe { CloseHandle(handle) }.ok();
    read.ok()?;
    let end = usize::try_from(length).unwrap_or(0).min(buffer.len());
    Some(PathBuf::from(String::from_utf16_lossy(&buffer[..end])))
}

/// Brings the main window of `process_id` to the front. `false` when it has no
/// window to bring.
///
/// The Alt trick is the one `nohrs_pages::foreground` uses (Windows only lets
/// the foreground process hand the foreground over, and a synthetic Alt press
/// counts as user input). It is repeated here rather than shared because
/// `nohrs-services` cannot depend on `nohrs-pages` (§3.4).
#[allow(unsafe_code)]
fn activate_main_window(process_id: u32) -> bool {
    let mut search = WindowSearch {
        process: process_id,
        window: None,
    };
    // SAFETY: the callback only reads the two values it is given and stores one
    // handle in `search`, which outlives the call.
    // `EnumWindows` answers FALSE when the callback stopped it, which is how a
    // successful search ends (it returns `BOOL(0)` once the window is stored);
    // windows-rs reports that FALSE as `Err`. Only `search.window` says whether
    // a window was found, so the result is logged and otherwise ignored.
    if let Err(error) = unsafe {
        EnumWindows(
            Some(enum_window),
            LPARAM((&mut search as *mut WindowSearch).cast::<core::ffi::c_void>() as isize),
        )
    } {
        tracing::debug!(target: "launch", process = process_id, "the window enumeration ended: {error}");
    }
    let Some(window) = search.window else {
        tracing::debug!(target: "launch", process = process_id, "no visible, ownerless window for the process");
        return false;
    };
    tracing::debug!(target: "launch", process = process_id, hwnd = window.0 as isize, "activating the window");
    // SAFETY: `window` came from `EnumWindows` and is still a handle Windows
    // knows; the calls take no buffers.
    unsafe {
        if IsIconic(window).as_bool() {
            // S10: restoring a minimised window has no failure to report beyond
            // the log; dropping the result silently would hide it.
            ShowWindow(window, SW_RESTORE).ok().log_err();
        }
        keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
        keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        if !SetForegroundWindow(window).as_bool() {
            // The older request still works where the rules refuse the plain
            // call.
            SwitchToThisWindow(window, true);
        }
        BringWindowToTop(window).ok();
    }
    true
}

/// What [`enum_window`] carries between the callback and the caller.
struct WindowSearch {
    process: u32,
    window: Option<HWND>,
}

/// Takes the first visible, ownerless top-level window of the process.
///
/// Ownerless and visible is the combination that means "the window the user
/// thinks of as the program": splash screens and tool windows have an owner.
#[allow(unsafe_code)]
extern "system" fn enum_window(window: HWND, parameter: LPARAM) -> BOOL {
    // SAFETY: `parameter` is the `&mut WindowSearch` that `activate_main_window`
    // handed to `EnumWindows`, alive for the whole enumeration.
    let search = unsafe { &mut *(parameter.0 as *mut WindowSearch) };
    if !unsafe { IsWindowVisible(window) }.as_bool() {
        return BOOL(1);
    }
    if unsafe { GetWindow(window, GW_OWNER) }.is_ok_and(|owner| !owner.is_invalid()) {
        return BOOL(1);
    }
    let mut owner = 0u32;
    // SAFETY: no buffers; the process id is written into `owner`.
    unsafe { GetWindowThreadProcessId(window, Some(&mut owner)) };
    if owner != search.process {
        return BOOL(1);
    }
    search.window = Some(window);
    // Stop the enumeration: the first match is the one to activate.
    BOOL(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_code_inside_a_win32_hresult_is_the_plain_error() {
        let error = WindowsError::from_hresult(windows::core::HRESULT(0x8007_04C7u32 as i32));
        assert_eq!(win32_code(&error), ERROR_CANCELLED.0);
        let error = WindowsError::from_hresult(windows::core::HRESULT(0x8000_4005u32 as i32));
        assert_eq!(win32_code(&error), 0x8000_4005);
    }

    #[test]
    fn a_system_message_describes_a_known_code() {
        let message = system_message(ERROR_CANCELLED.0);
        assert!(!message.is_empty());
        assert!(!message.ends_with('\n'), "{message:?}");
    }

    #[test]
    fn wide_text_is_nul_terminated() {
        let encoded = wide("açúcar");
        assert_eq!(encoded.last(), Some(&0));
        assert_eq!(encoded.len(), "açúcar".encode_utf16().count() + 1);
    }

    #[test]
    fn a_running_process_is_found_by_its_own_path() {
        // The test binary is running this very test.
        let executable = std::env::current_exe().expect("the test has a path");
        let process = process_id_for(&executable).expect("the test process should be found");
        assert_eq!(process, std::process::id());
    }

    #[test]
    fn a_path_nothing_runs_is_not_found() {
        assert!(process_id_for(Path::new("C:\\nohrs\\nada\\aqui.exe")).is_none());
    }

    /// A real `ShellExecuteExW` with arguments (§16.1). Run by hand:
    /// `cargo test --release -p nohrs-services -- --ignored --nocapture`.
    #[test]
    #[ignore = "starts a real process; run by hand for the PR"]
    fn launching_a_program_with_arguments_starts_it() {
        use nohrs_core::config::quick_launch::OnRunning;

        let executable = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("C:\\Windows"))
            .join("System32\\cmd.exe");
        assert!(executable.is_file(), "{} is missing", executable.display());
        let mut item = Item::new(0, KIND_APP, "Espera");
        item.path = Some(executable.to_string_lossy().into_owned());
        item.args = Some("/c timeout /t 3 /nobreak".to_string());
        let request = LaunchRequest {
            mode: LaunchMode::Normal,
            on_running: OnRunning::New,
            expand: crate::launch::ExpandContext::current(),
        };
        let outcome = AppAction
            .launch(&item, &request)
            .expect("the program starts");
        assert_eq!(outcome, LaunchOutcome::Started);
        // The process is there right after the call; `ShellExecuteExW` does not
        // wait for it, and the wait below is the only synchronisation.
        let found = (0..20).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            process_id_for(&executable).is_some()
        });
        assert!(found, "cmd.exe should be running");
    }

    /// §9, B4: a path the Shell's buffer cannot hold is refused instead of
    /// being truncated into an unterminated buffer.
    #[test]
    fn an_icon_path_of_300_characters_is_refused() {
        let long = format!("C:\\{}.exe", "a".repeat(297));
        assert!(long.chars().count() >= 300);
        assert_eq!(super::indexed_icon(&long, 0, 32), None);
    }
    #[test]
    #[ignore = "starts a real process; run by hand for the PR"]
    fn a_program_that_is_not_there_is_reported_as_not_found() {
        use nohrs_core::config::quick_launch::OnRunning;

        let mut item = Item::new(0, KIND_APP, "Inexistente");
        item.path = Some("C:\\nao\\existe\\nada.exe".to_string());
        let request = LaunchRequest {
            mode: LaunchMode::Normal,
            on_running: OnRunning::Activate,
            expand: crate::launch::ExpandContext::current(),
        };
        let error = AppAction
            .launch(&item, &request)
            .expect_err("nothing to start");
        assert!(
            matches!(error, LaunchError::NotFound(_)),
            "expected NotFound, got {error:?}"
        );
    }

    #[test]
    fn a_url_item_is_checked_before_the_shell_sees_it() {
        let mut item = Item::new(0, KIND_URL, "Link");
        item.path = Some("file:///C:/Windows/System32/cmd.exe".to_string());
        let request = LaunchRequest {
            mode: LaunchMode::Normal,
            on_running: nohrs_core::config::quick_launch::OnRunning::Activate,
            expand: crate::launch::ExpandContext::current(),
        };
        let error = UrlAction.launch(&item, &request).expect_err("blocked");
        assert!(matches!(error, LaunchError::Blocked(_)), "{error:?}");
        assert_eq!(
            UrlAction.health(&item, &request.expand),
            Health::Invalid("blocked: the file: scheme is not allowed".to_string())
        );
        item.path = Some("https://example.com".to_string());
        assert_eq!(UrlAction.health(&item, &request.expand), Health::Ok);
    }
}
