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
    DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ, SelectObject,
};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
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
    AO_NONE, BHID_EnumItems, FOLDERID_AppsFolder, IApplicationActivationManager, IEnumShellItems,
    IShellItem, IShellItem2, KF_FLAG_DEFAULT, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOCLOSEPROCESS,
    SHELLEXECUTEINFOW, SHGetKnownFolderItem, SIGDN_NORMALDISPLAY, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DI_NORMAL, DestroyIcon, DrawIconEx, EnumWindows, GW_OWNER, GetIconInfo,
    GetWindow, GetWindowThreadProcessId, HICON, ICONINFO, IsIconic, IsWindowVisible,
    LR_DEFAULTCOLOR, PrivateExtractIconsW, SW_RESTORE, SW_SHOWNORMAL, SetForegroundWindow,
    ShowWindow, SwitchToThisWindow,
};
use windows::core::{BOOL, Error as WindowsError, Interface, PCWSTR, PWSTR};

use crate::launch::{
    ExpandContext, Health, InstalledApp, KIND_APP, KIND_FILE, KIND_URL, KIND_UWP, LaunchAction,
    LaunchError, LaunchMode, LaunchOutcome, LaunchRequest, expand, item_path, validate_url,
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

/// The length of the fixed file-name buffer the Shell's icon call takes, NUL
/// included (§9, B4).
const MAX_PATH_LIMIT: usize = 260;

/// The icon at `index` inside `path` (`"exe,索引"`, §9).
///
/// The Shell extracts it at the size the window wants; the pixels come out of a
/// top-down 32-bit DIB as the premultiplied BGRA the icon path expects.
#[allow(unsafe_code)]
pub fn indexed_icon(path: &str, index: u32, edge: u32) -> Option<(Vec<u8>, u32, u32)> {
    let edge_px = i32::try_from(edge).ok()?;
    let index = i32::try_from(index).ok()?;
    // The binding takes the file name as the classic fixed 260-wide buffer, and
    // it reads it as a NUL-terminated string: a longer path would leave the
    // buffer unterminated and lose its tail, so it is refused here (B4).
    let wide_path: Vec<u16> = path.encode_utf16().collect();
    if wide_path.len() >= MAX_PATH_LIMIT {
        tracing::debug!(
            target: "launch",
            length = wide_path.len(),
            "the icon path does not fit the Shell's 260-unit buffer"
        );
        return None;
    }
    let mut name = [0u16; MAX_PATH_LIMIT];
    for (target, source) in name.iter_mut().zip(wide_path) {
        *target = source;
    }
    let mut icon = HICON::default();
    let mut identifier = 0u32;
    // SAFETY: a buffer this call owns and the icon slot it fills. The icon it
    // returns is owned by the guard below.
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
    // SAFETY: the icon came from the extraction above; the guard destroys it.
    let icon = unsafe { IconGuard::new(icon) };
    unsafe { pixels_of_icon(&icon, edge_px) }
}

/// A device context, released when it goes out of scope.
#[allow(unsafe_code)]
struct Dc(HDC);

#[allow(unsafe_code)]
impl Drop for Dc {
    fn drop(&mut self) {
        // SAFETY: the handle was created by `CreateCompatibleDC` and is released
        // exactly once, here.
        unsafe { DeleteDC(self.0) }.ok().log_err();
    }
}

/// A bitmap or icon handle, released when it goes out of scope.
#[allow(unsafe_code)]
struct Bitmap(HBITMAP);

#[allow(unsafe_code)]
impl Bitmap {
    /// Takes ownership of a bitmap another call made, skipping the null handle
    /// `GetIconInfo` reports for an icon without one.
    unsafe fn from_icon_bitmap(bitmap: HBITMAP) -> Option<Self> {
        (!bitmap.is_invalid()).then_some(Self(bitmap))
    }
}

#[allow(unsafe_code)]
impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: the handle is one this process owns and is released once.
        unsafe { DeleteObject(HGDIOBJ(self.0.0)) }.ok().log_err();
    }
}

/// An icon, released when it goes out of scope.
#[allow(unsafe_code)]
struct IconGuard(HICON);

#[allow(unsafe_code)]
impl IconGuard {
    /// Takes ownership of `icon`.
    unsafe fn new(icon: HICON) -> Self {
        Self(icon)
    }
}

#[allow(unsafe_code)]
impl Drop for IconGuard {
    fn drop(&mut self) {
        // SAFETY: the icon is one this process owns and is released once.
        unsafe { DestroyIcon(self.0) }.log_err();
    }
}

/// Draws one icon into a top-down 32-bit DIB and copies the pixels out.
///
/// Unsafe because GDI has no safe bindings; every handle is owned by a guard
/// and released when this returns, whatever the outcome (B5).
#[allow(unsafe_code)]
unsafe fn pixels_of_icon(icon: &IconGuard, edge: i32) -> Option<(Vec<u8>, u32, u32)> {
    let mut information = ICONINFO::default();
    // SAFETY: the icon is valid and the structure is this call's output.
    unsafe { GetIconInfo(icon.0, &mut information) }.ok()?;
    // SAFETY: both bitmaps were made by `GetIconInfo`; the guards release them.
    let _coloured = unsafe { Bitmap::from_icon_bitmap(information.hbmColor) };
    let _mask = unsafe { Bitmap::from_icon_bitmap(information.hbmMask) };

    // SAFETY: a screen-compatible DC with no bitmap of its own; the guard
    // releases it.
    let device = unsafe { Dc(CreateCompatibleDC(None)) };
    if device.0.is_invalid() {
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
    // bitmap lives; the guard releases the bitmap.
    let bitmap = unsafe {
        CreateDIBSection(
            Some(device.0),
            &header,
            DIB_RGB_COLORS,
            &raw mut bits,
            None,
            0,
        )
    };
    let Ok(bitmap) = bitmap else {
        return None;
    };
    let bitmap = Bitmap(bitmap);

    // SAFETY: every handle is owned here; the old object is put back before the
    // DC goes away, and the pixels are copied before the bitmap is released.
    unsafe {
        let previous = SelectObject(device.0, HGDIOBJ(bitmap.0.0));
        let drawn = DrawIconEx(device.0, 0, 0, icon.0, edge, edge, 0, None, DI_NORMAL);
        SelectObject(device.0, previous);
        // A failed draw leaves the bitmap transparent; caching that would show
        // an empty tile as a real icon (B5).
        drawn.ok()?;
    }

    let edge = usize::try_from(edge).ok()?;
    if bits.is_null() {
        return None;
    }
    // SAFETY: the DIB is 32 bits per pixel and `edge * edge` pixels were
    // allocated for it; it is copied before the guards release it.
    let pixels =
        unsafe { core::slice::from_raw_parts(bits as *const u8, edge * edge * 4) }.to_vec();
    Some((
        premultiply_bgra(pixels),
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

/// A COM apartment entered on this thread, left when it is dropped (§8.4).
///
/// The Shell's calls need one, and the CLI is a console program that starts with
/// none; this is the one place that knows how to enter and leave it.
pub struct ComApartment {
    initialized: bool,
}

impl ComApartment {
    /// Enters a single-threaded apartment, or reports why not.
    #[allow(unsafe_code)]
    pub fn enter() -> Result<Self, LaunchError> {
        // SAFETY: no arguments outlive the call, and a successful call is balanced
        // by `CoUninitialize` in `Drop`.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map(|()| Self { initialized: true })
            .map_err(|error| LaunchError::Internal(error.to_string()))
    }
}

#[allow(unsafe_code)]
impl Drop for ComApartment {
    fn drop(&mut self) {
        if !self.initialized {
            return;
        }
        // SAFETY: balances the successful `CoInitializeEx` in `enter`, and every
        // object made through it has been released by now.
        unsafe { CoUninitialize() };
    }
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

/// An application started by its AppUserModelID, through the activation
/// manager (§17.2 B).
///
/// The item's path is an AppUserModelID, not a file; `cwd` and `admin` do not
/// apply to it and are ignored whatever the file says.
pub(crate) struct UwpAction;

impl LaunchAction for UwpAction {
    fn kind(&self) -> &str {
        KIND_UWP
    }

    fn health(&self, item: &Item, _context: &ExpandContext) -> Health {
        if aumid_of(item).is_some() {
            Health::Ok
        } else {
            Health::Invalid("the app ID must name both sides of a ! and hold no spaces".to_string())
        }
    }

    fn launch(&self, item: &Item, _request: &LaunchRequest) -> Result<LaunchOutcome, LaunchError> {
        // The health check cannot query the system, so the format is checked
        // here too: a malformed ID is refused before COM is asked (§17.2 B).
        let aumid = aumid_of(item).ok_or_else(|| {
            LaunchError::Blocked(format!(
                "the uwp item {:?} has no well-formed app ID",
                item.name
            ))
        })?;
        activate_application(aumid, item.args.as_deref())
    }
}

/// The item's path, when it is a well-formed AppUserModelID (§17.2 B).
///
/// The rules are the ones §17.2 B asks for: a `!` with something on both sides,
/// no whitespace anywhere and at most 256 characters. Nothing here asks the
/// system, so a health pass stays inside its 5 ms budget.
fn aumid_of(item: &Item) -> Option<&str> {
    let aumid = item.path.as_deref()?;
    let (package, application) = aumid.split_once('!')?;
    let well_formed = !package.is_empty()
        && !application.is_empty()
        && aumid.chars().count() <= 256
        && !aumid.chars().any(char::is_whitespace);
    well_formed.then_some(aumid)
}

/// Hands `aumid` to the activation manager with `arguments` (§17.2 B).
///
/// The manager is created on this thread, which already owns the `quick-launch`
/// apartment (§8.4); both the manager and the strings it reads are released
/// before this returns.
#[allow(unsafe_code)]
fn activate_application(
    aumid: &str,
    arguments: Option<&str>,
) -> Result<LaunchOutcome, LaunchError> {
    // SAFETY: `ApplicationActivationManager` is a constant class ID, the call
    // returns an owned interface, and that interface is released by its own
    // `Drop` when it goes out of scope.
    let manager: IApplicationActivationManager = unsafe {
        CoCreateInstance(
            &windows::Win32::UI::Shell::ApplicationActivationManager,
            None,
            CLSCTX_ALL,
        )
    }
    .map_err(|error| os_error(&error))?;
    let aumid = wide(aumid);
    let arguments = wide(
        arguments
            .map(str::trim)
            .filter(|arguments| !arguments.is_empty())
            .unwrap_or_default(),
    );
    // SAFETY: both buffers are NUL-terminated and outlive the call; the manager
    // is live for the whole call. `AO_NONE` is the plain activation §17.2 B
    // asks for, and there is no process handle to release.
    unsafe {
        manager.ActivateApplication(PCWSTR(aumid.as_ptr()), PCWSTR(arguments.as_ptr()), AO_NONE)
    }
    .map_err(|error| os_error(&error))?;
    Ok(LaunchOutcome::Started)
}

/// The applications the Apps folder lists, with their display names (§17.2 B).
///
/// This is the slow call the settings page runs on a background task: it walks
/// the shell's own enumeration and copies two properties out of every item. The
/// caller must have entered a COM apartment on the calling thread.
#[allow(unsafe_code)]
pub fn installed_apps() -> Result<Vec<InstalledApp>, LaunchError> {
    // SAFETY: `FOLDERID_AppsFolder` is a constant GUID and the call returns an
    // owned shell item.
    let folder: IShellItem =
        unsafe { SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None) }
            .map_err(|error| os_error(&error))?;
    // SAFETY: the enumeration handler is requested with no bind context, and
    // the enumerator is owned by this call.
    let enumerator: IEnumShellItems =
        unsafe { folder.BindToHandler(None, &BHID_EnumItems) }.map_err(|error| os_error(&error))?;
    let mut apps = Vec::new();
    loop {
        let mut batch = [None];
        let mut fetched = 0u32;
        // SAFETY: the batch is writable for its whole length, which is the
        // count the call is told, and the fetched count is its own out
        // parameter.
        unsafe { enumerator.Next(&mut batch, Some(&mut fetched)) }
            .map_err(|error| os_error(&error))?;
        if fetched == 0 {
            break;
        }
        let Some(item) = batch[0].take() else {
            break;
        };
        let Ok(item) = item.cast::<IShellItem2>() else {
            tracing::debug!(target: "launch", "an app item is not an IShellItem2; skipped");
            continue;
        };
        let Some(aumid) = shell_string(&item, &PKEY_AppUserModel_ID) else {
            // A classic program without an AppUserModelID cannot be activated
            // by ID, so it has nothing to offer the picker.
            continue;
        };
        if aumid.is_empty() {
            continue;
        }
        let name = display_name(&item).unwrap_or_else(|| aumid.clone());
        apps.push(InstalledApp { name, aumid });
    }
    Ok(apps)
}

/// One property of a shell item, copied out of the shell's allocator.
#[allow(unsafe_code)]
fn shell_string(
    item: &IShellItem2,
    key: &windows::Win32::Foundation::PROPERTYKEY,
) -> Option<String> {
    // SAFETY: `key` is a valid property key and the call returns a string the
    // shell allocated, which `taken_string` copies and releases exactly once.
    let pointer = unsafe { item.GetString(key) }.log_err()?;
    // SAFETY: the pointer came from the call above, so it is NUL-terminated and
    // owned by this process.
    unsafe { taken_string(pointer) }
}

/// The display name of a shell item, copied out of the shell's allocator.
#[allow(unsafe_code)]
fn display_name(item: &IShellItem2) -> Option<String> {
    // SAFETY: the call returns a string the shell allocated; `taken_string`
    // copies it and releases it exactly once.
    let pointer = unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) }.log_err()?;
    // SAFETY: the pointer came from the call above, so it is NUL-terminated and
    // owned by this process.
    unsafe { taken_string(pointer) }
}

/// Copies a shell string and frees the shell's copy.
#[allow(unsafe_code)]
unsafe fn taken_string(pointer: PWSTR) -> Option<String> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the caller passes a pointer the shell returned, which is
    // NUL-terminated and may be read.
    let text = unsafe { pointer.to_string() }.log_err();
    // SAFETY: the same pointer was allocated by the shell for this process and
    // is released exactly once, here.
    unsafe { CoTaskMemFree(Some(pointer.0.cast())) };
    text
}

/// Maps a Shell `HRESULT` failure to the error the window reports (§8.2,
/// §17.2 B), reusing the conversion `start` uses.
fn os_error(error: &WindowsError) -> LaunchError {
    let code = win32_code(error);
    LaunchError::Os {
        code,
        message: system_message(code),
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

    fn uwp_item(aumid: &str) -> Item {
        let mut item = Item::new(0, KIND_UWP, "Calculadora");
        item.path = Some(aumid.to_string());
        item
    }

    /// §17.2 B.3: the format alone decides, and a real AppUserModelID passes.
    #[test]
    fn a_well_formed_app_id_is_healthy_without_asking_the_system() {
        let context = crate::launch::ExpandContext::current();
        for aumid in [
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App",
            "Microsoft.Windows.Photos_8wekyb3d8bbwe!App",
            "a!b",
        ] {
            assert_eq!(
                UwpAction.health(&uwp_item(aumid), &context),
                Health::Ok,
                "{aumid} should be accepted"
            );
        }
    }

    /// §17.2 B.3 and B.7: each way an AppUserModelID can be malformed.
    #[test]
    fn a_malformed_app_id_is_invalid() {
        let context = crate::launch::ExpandContext::current();
        let longest_ok = format!("{}!{}", "a".repeat(128), "b".repeat(127));
        assert_eq!(longest_ok.chars().count(), 256);
        assert_eq!(
            UwpAction.health(&uwp_item(&longest_ok), &context),
            Health::Ok
        );
        let too_long = format!("{}!{}", "a".repeat(128), "b".repeat(128));
        let rejected = [
            "",
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe",
            "!App",
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe!",
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe!A pp",
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe!\tApp",
            too_long.as_str(),
        ];
        for aumid in rejected {
            assert!(
                matches!(
                    UwpAction.health(&uwp_item(aumid), &context),
                    Health::Invalid(_)
                ),
                "{aumid:?} should be refused"
            );
        }
        let mut without_path = Item::new(1, KIND_UWP, "Sem ID");
        without_path.path = None;
        assert!(matches!(
            UwpAction.health(&without_path, &context),
            Health::Invalid(_)
        ));
    }

    /// §17.2 B.7: a malformed ID is refused before COM is asked to activate it.
    #[test]
    fn a_malformed_app_id_is_blocked_before_activation() {
        let request = LaunchRequest {
            mode: LaunchMode::Normal,
            on_running: nohrs_core::config::quick_launch::OnRunning::Activate,
            expand: crate::launch::ExpandContext::current(),
        };
        let error = UwpAction
            .launch(&uwp_item("nao-tem-exclamacao"), &request)
            .expect_err("nothing to activate");
        assert!(matches!(error, LaunchError::Blocked(_)), "{error:?}");
    }

    /// §17.2 B.4 and B.7: the real Apps folder, on a real machine. Run by hand:
    /// `cargo test --release -p nohrs-services -- --ignored --nocapture`.
    #[test]
    #[ignore = "enumerates the real Apps folder; run by hand for the PR"]
    fn the_apps_folder_has_at_least_one_application() {
        let _apartment = ComApartment::enter().expect("the thread enters a COM apartment");
        let apps = installed_apps().expect("the Apps folder is enumerated");
        assert!(!apps.is_empty(), "the Apps folder should hold an app");
        assert!(
            apps.iter()
                .any(|app| !app.name.is_empty() && !app.aumid.is_empty()),
            "at least one app should have a display name and an AppUserModelID"
        );
    }
}
