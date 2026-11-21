//! The folder windows other programs find when they ask the shell to show a
//! file: "Open file location" (QQ, WeChat, WPS, Premiere), a browser's "Show
//! in folder" and `explorer /select,…` all go through the shell, which looks
//! up `IShellWindows` for a window already showing the folder and asks it to
//! select the item. Nothing else is involved: no Explorer window opens, so the
//! caller does not stall behind a window it never wanted.
//!
//! Ported from Pulse (`cankao/pulse-main/src/app/shell_window_registry.cpp`),
//! whose users do not see the flash or the freeze these programs still caused
//! here. Each open tab is registered as an `SWC_BROWSER` shell window whose
//! document serves an `IShellView`; `IShellView::SelectItem` becomes a
//! selection request queued for the UI thread. When no tab shows the folder
//! the shell runs the folder's open verb — LumaFiles opens a tab — and the
//! registration that follows lets it select the item in that tab.
//!
//! Every call into Explorer runs on the registry's own STA thread; the UI
//! thread only publishes the wanted set and never blocks on the shell.

use gpui::{AsyncWindowContext, Context, WeakEntity, Window};

use crate::root::RootView;

/// One open tab as the shell sees it: the tab's entity id and the folder it
/// shows. An empty `path` is This PC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The tab's entity id, echoed back with a selection for that tab.
    pub key: u64,
    /// The folder the tab shows, in the form the shell compares.
    pub path: String,
    /// The window showing the tab; the shell activates it as it selects. Kept
    /// per entry so a tab moved to another window registers under that window,
    /// as in Pulse.
    pub hwnd: isize,
}

/// A selection the shell asked for: the tab it found, and the item's path.
#[derive(Clone, Debug)]
pub struct Selection {
    /// The tab that answered the shell's lookup.
    pub key: u64,
    /// Full path of the item to show selected in that tab.
    pub path: String,
}

/// Replaces the set of tabs the shell can find. Cheap to call on a timer: an
/// equal set changes nothing.
pub fn publish(entries: Vec<Entry>) {
    platform::publish(entries);
}

/// Starts delivering the shell's selection requests to `cx`, revealing the
/// requested item in the tab that answered.
pub fn watch(window: &mut Window, cx: &mut Context<RootView>) {
    let (sender, receiver) = async_channel::unbounded::<Selection>();
    platform::start(sender);
    cx.spawn_in(
        window,
        async move |this: WeakEntity<RootView>, cx: &mut AsyncWindowContext| {
            let mut cx = cx.clone();
            while let Ok(selection) = receiver.recv().await {
                tracing::debug!(
                    "shell selection in tab {}: {}",
                    selection.key,
                    selection.path
                );
                let shown = this.update_in(&mut cx, |root, window, cx| {
                    let shown = root.explorer_entity().update(cx, |page, cx| {
                        page.reveal_shell_selection(selection.key, &selection.path, window, cx)
                    });
                    if shown {
                        root.show_explorer(cx);
                        // The program that asked for the reveal has the
                        // foreground; take it so the selection is seen.
                        crate::foreground::bring_to_front(window);
                    }
                });
                if shown.is_err() {
                    return;
                }
            }
        },
    )
    .detach();
}

/// Gives the shell back its windows: revokes every registration. Called when
/// the app closes, so Explorer is not left holding objects of a dead process.
pub fn shutdown() {
    platform::shutdown();
}

#[cfg(not(windows))]
mod platform {
    use super::{Entry, Selection};

    pub fn start(_sender: async_channel::Sender<Selection>) {}

    pub fn publish(_entries: Vec<Entry>) {}

    pub fn shutdown() {}
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    //! The COM objects and the STA thread that owns them. `unsafe` is confined
    //! here: none of these interfaces has a safe binding.

    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicIsize, AtomicU8, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use nohrs_core::telemetry::LogErr;
    use windows::Win32::Foundation::{
        DISP_E_BADINDEX, DISP_E_MEMBERNOTFOUND, DISP_E_UNKNOWNNAME, E_FAIL, E_NOINTERFACE,
        E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, HWND, S_FALSE, SHANDLE_PTR, VARIANT_BOOL,
        VARIANT_FALSE, VARIANT_TRUE,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
        CoInitializeEx, CoTaskMemFree, CoUninitialize, DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO,
        IDispatch, IDispatch_Impl, IServiceProvider, IServiceProvider_Impl, ITypeInfo,
    };
    use windows::Win32::System::Ole::{
        DispGetIDsOfNames, DispInvoke, IOleWindow_Impl, LoadRegTypeLib,
    };
    use windows::Win32::System::Variant::{InitVariantFromBuffer, VARIANT, VariantClear};
    use windows::Win32::UI::Controls::LPFNSVADDPROPSHEETPAGE;
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        _SVGIO, FOLDERID_ComputerFolder, FOLDERSETTINGS, IFolderView, ILCombine, ILCreateFromPathW,
        ILGetSize, IShellBrowser, IShellView, IShellView_Impl, IShellWindows, IWebBrowser_Impl,
        IWebBrowserApp, IWebBrowserApp_Impl, SHGetKnownFolderIDList, SHGetNameFromIDList,
        SHParseDisplayName, SIGDN, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_NORMALDISPLAY, SVSI_EDIT,
        SVSI_ENSUREVISIBLE, SVSI_FOCUSED, SVSI_SELECT, SWC_BROWSER, ShellWindows,
        UrlCreateFromPathW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetWindowThreadProcessId, IsWindowVisible, MSG, MWMO_INPUTAVAILABLE,
        MsgWaitForMultipleObjectsEx, PM_NOREMOVE, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
        TranslateMessage, WM_QUIT, WM_USER,
    };
    use windows::core::{
        BOOL, BSTR, ComObject, GUID, IUnknownImpl, Interface, PCWSTR, PWSTR, Ref, Result, implement,
    };

    use super::{Entry, Selection};

    /// How often the STA looks for a new set or a retry that is due. The thread
    /// also wakes at once for a message (a COM call from Explorer).
    const IDLE_MS: u32 = 25;
    const RETRY_FIRST_MS: u32 = 250;
    const RETRY_MAX_MS: u32 = 5_000;

    // The folder forms the shell compares byte for byte. File Explorer registers
    // its windows under the path form (`ILCreateFromPathW`); programs that go
    // through the desktop folder (Chromium, Electron) parse the known-folder
    // form (`SHParseDisplayName`). A tab registers under both when they differ.
    const PRIMARY: u8 = 0;
    const ALIAS: u8 = 1;

    struct Shared {
        desired: Mutex<Vec<Entry>>,
        revision: AtomicU64,
        stopping: AtomicU8,
        window: AtomicIsize,
    }

    impl Shared {
        fn new() -> Self {
            Self {
                desired: Mutex::new(Vec::new()),
                revision: AtomicU64::new(0),
                stopping: AtomicU8::new(0),
                window: AtomicIsize::new(0),
            }
        }

        fn stopping(&self) -> bool {
            self.stopping.load(Ordering::Relaxed) != 0
        }

        fn lock_desired(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
            self.desired
                .lock()
                .unwrap_or_else(|error| error.into_inner())
        }
    }

    struct Registry {
        shared: Arc<Shared>,
        thread: Mutex<Option<JoinHandle<()>>>,
    }

    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    static SENDER: OnceLock<async_channel::Sender<Selection>> = OnceLock::new();

    fn registry() -> &'static Registry {
        REGISTRY.get_or_init(|| Registry {
            shared: Arc::new(Shared::new()),
            thread: Mutex::new(None),
        })
    }

    pub fn start(sender: async_channel::Sender<Selection>) {
        let registry = registry();
        if SENDER.set(sender.clone()).is_err() {
            return;
        }
        let mut thread = registry
            .thread
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if thread.is_some() {
            return;
        }
        let shared = registry.shared.clone();
        match std::thread::Builder::new()
            .name("luma-shell-windows".into())
            .spawn(move || run(shared, sender))
        {
            Ok(handle) => *thread = Some(handle),
            Err(error) => tracing::warn!("shell windows disabled: {error}"),
        }
    }

    pub fn publish(entries: Vec<Entry>) {
        // Nothing to talk to before `start`; the first publish after it replaces
        // the empty set.
        if SENDER.get().is_none() {
            return;
        }
        let registry = registry();
        let mut desired = registry.shared.lock_desired();
        if *desired == entries {
            return;
        }
        let hwnd = entries.first().map_or(0, |entry| entry.hwnd);
        *desired = entries;
        registry.shared.window.store(hwnd, Ordering::Relaxed);
        registry.shared.revision.fetch_add(1, Ordering::Release);
    }

    pub fn shutdown() {
        let Some(registry) = REGISTRY.get() else {
            return;
        };
        registry.shared.stopping.store(1, Ordering::Relaxed);
        let mut slot = registry
            .thread
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(thread) = slot.take() else {
            return;
        };
        // A call into a hung Explorer must not hold up closing the window; the
        // thread is left behind and dies with the process.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                tracing::warn!("shell windows did not stop in time; leaving the thread");
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Err(error) = thread.join() {
            tracing::warn!("shell windows thread panicked: {error:?}");
        }
    }

    fn run(shared: Arc<Shared>, sender: async_channel::Sender<Selection>) {
        let init =
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if init.is_err() {
            tracing::warn!("shell windows disabled: CoInitializeEx failed");
            return;
        }
        let started = Instant::now();
        // Forces this thread's message queue into being before registering: COM
        // calls from Explorer arrive as window messages.
        let mut message = MSG::default();
        let _ = unsafe { PeekMessageW(&mut message, None, WM_USER, WM_USER, PM_NOREMOVE) };
        let mut worker = Worker::new(shared.clone(), sender, started);
        while !shared.stopping() {
            worker.apply();
            let wait = worker.wait_ms();
            unsafe { MsgWaitForMultipleObjectsEx(None, wait, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
            loop {
                let mut message = MSG::default();
                let more = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) };
                if !more.as_bool() {
                    break;
                }
                if message.message == WM_QUIT {
                    shared.stopping.store(1, Ordering::Relaxed);
                    break;
                }
                unsafe {
                    let _ = TranslateMessage(&message);
                    let _ = DispatchMessageW(&message);
                }
            }
        }
        worker.stop();
        unsafe { CoUninitialize() };
    }

    /// A folder's location as the shell matches it: byte for byte, without
    /// canonicalizing. Copied out of shell memory so nothing outlives the call
    /// that produced it.
    #[derive(Clone)]
    struct OwnedPidl(Vec<u16>);

    impl OwnedPidl {
        fn from_raw(pidl: *const ITEMIDLIST) -> Option<Self> {
            if pidl.is_null() {
                return None;
            }
            let size = unsafe { ILGetSize(Some(pidl)) } as usize;
            if size < 2 {
                return None;
            }
            let mut data = vec![0u16; size.div_ceil(2)];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    pidl as *const u8,
                    data.as_mut_ptr() as *mut u8,
                    size,
                );
            }
            Some(Self(data))
        }

        fn as_ptr(&self) -> *const ITEMIDLIST {
            self.0.as_ptr() as *const ITEMIDLIST
        }

        fn size(&self) -> u32 {
            (self.0.len() * 2) as u32
        }
    }

    /// What one registered slot shows, read by the COM objects and rewritten
    /// when the tab navigates.
    #[derive(Default)]
    struct SlotState {
        folder: Option<OwnedPidl>,
        display: String,
        url: String,
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn free(pointer: *const c_void) {
        if !pointer.is_null() {
            unsafe { CoTaskMemFree(Some(pointer)) };
        }
    }

    fn pidl_name(pidl: *const ITEMIDLIST, kind: SIGDN) -> Option<String> {
        if pidl.is_null() {
            return None;
        }
        let name = unsafe { SHGetNameFromIDList(pidl, kind) }.ok()?;
        let text = unsafe { name.to_string() }.ok();
        free(name.0 as *const c_void);
        text
    }

    fn url_for_path(path: &str) -> String {
        let wide = wide(path);
        let mut buffer = [0u16; 2084]; // INTERNET_MAX_URL_LENGTH
        let mut length = buffer.len() as u32;
        let result = unsafe {
            UrlCreateFromPathW(
                PCWSTR(wide.as_ptr()),
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
                0,
            )
        };
        if result.is_err() {
            return String::new();
        }
        let length = (length as usize).min(buffer.len()).saturating_sub(1);
        String::from_utf16_lossy(&buffer[..length])
    }

    /// The folder's location in the form the caller is likely to use: the path
    /// form, and the known-folder form when it differs. Empty is This PC.
    fn folder_pidls(path: &str) -> (Option<OwnedPidl>, Option<OwnedPidl>) {
        if path.is_empty() {
            let primary = unsafe { SHGetKnownFolderIDList(&FOLDERID_ComputerFolder, 0, None) }
                .ok()
                .and_then(|pidl| {
                    let owned = OwnedPidl::from_raw(pidl);
                    free(pidl as *const c_void);
                    owned
                });
            return (primary, None);
        }
        let wide = wide(path);
        let pointer = unsafe { ILCreateFromPathW(PCWSTR(wide.as_ptr())) };
        let primary = OwnedPidl::from_raw(pointer);
        free(pointer as *const c_void);

        let mut alias_pointer: *mut ITEMIDLIST = std::ptr::null_mut();
        let parsed =
            unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut alias_pointer, 0, None) };
        let alias = match parsed {
            Ok(()) => {
                let owned = OwnedPidl::from_raw(alias_pointer);
                free(alias_pointer as *const c_void);
                owned
            }
            Err(error) => {
                tracing::debug!("could not parse {path} as a known-folder path: {error}");
                None
            }
        };

        let primary = primary.or_else(|| alias.clone());
        let alias = match (&primary, &alias) {
            (Some(primary), Some(alias)) if primary.0 == alias.0 => None,
            (_, alias) => alias.clone(),
        };
        (primary, alias)
    }

    // IShellView over one tab: only SelectItem does anything. The shell asks for
    // it through the document's IServiceProvider (IID_IFolderView service).
    #[implement(IShellView)]
    struct FolderView {
        key: u64,
        window: isize,
        sender: async_channel::Sender<Selection>,
        state: Arc<Mutex<SlotState>>,
    }

    impl FolderView_Impl {
        fn slot(&self) -> std::sync::MutexGuard<'_, SlotState> {
            self.state.lock().unwrap_or_else(|error| error.into_inner())
        }
    }

    impl IOleWindow_Impl for FolderView_Impl {
        fn GetWindow(&self) -> Result<HWND> {
            Ok(HWND(self.window as *mut c_void))
        }

        fn ContextSensitiveHelp(&self, _fentermode: BOOL) -> Result<()> {
            Err(E_NOTIMPL.into())
        }
    }

    impl IShellView_Impl for FolderView_Impl {
        fn TranslateAccelerator(&self, _pmsg: *const MSG) -> windows::core::HRESULT {
            S_FALSE
        }

        fn EnableModeless(&self, _fenable: BOOL) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn UIActivate(&self, _ustate: u32) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Refresh(&self) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn CreateViewWindow(
            &self,
            _psvprevious: Ref<'_, IShellView>,
            _pfs: *const FOLDERSETTINGS,
            _psb: Ref<'_, IShellBrowser>,
            _prcview: *const windows::Win32::Foundation::RECT,
        ) -> Result<HWND> {
            Err(E_NOTIMPL.into())
        }

        fn DestroyViewWindow(&self) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn GetCurrentInfo(&self) -> Result<FOLDERSETTINGS> {
            Err(E_NOTIMPL.into())
        }

        fn AddPropertySheetPages(
            &self,
            _dwreserved: u32,
            _pfn: LPFNSVADDPROPSHEETPAGE,
            _lparam: windows::Win32::Foundation::LPARAM,
        ) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn SaveViewState(&self) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn SelectItem(&self, pidlitem: *const ITEMIDLIST, uflags: u32) -> Result<()> {
            if pidlitem.is_null() {
                return Err(E_POINTER.into());
            }
            // A deselect alone has nothing to show.
            let wanted =
                (SVSI_SELECT.0 | SVSI_EDIT.0 | SVSI_FOCUSED.0 | SVSI_ENSUREVISIBLE.0) as u32;
            if uflags & wanted == 0 {
                return Ok(());
            }
            let state = self.slot();
            let Some(folder) = &state.folder else {
                return Err(E_FAIL.into());
            };
            let combined = unsafe { ILCombine(Some(folder.as_ptr()), Some(pidlitem)) };
            if combined.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            let path = pidl_name(combined, SIGDN_DESKTOPABSOLUTEPARSING);
            free(combined as *const c_void);
            drop(state);
            let Some(path) = path else {
                return Err(E_FAIL.into());
            };
            // The shell's call must return at once; the UI thread shows the item
            // on its own time.
            if let Err(error) = self.sender.try_send(Selection {
                key: self.key,
                path,
            }) {
                tracing::warn!("a shell selection could not be delivered: {error}");
            }
            Ok(())
        }

        fn GetItemObject(
            &self,
            _uitem: &_SVGIO,
            _riid: *const GUID,
            _ppv: *mut *mut c_void,
        ) -> Result<()> {
            Err(E_NOTIMPL.into())
        }
    }

    // IWebBrowserApp::get_Document's result. The shell only queries it for
    // IServiceProvider; the IDispatch side has no members.
    #[implement(IDispatch, IServiceProvider)]
    struct FolderDocument {
        view: ComObject<FolderView>,
    }

    impl IDispatch_Impl for FolderDocument_Impl {
        fn GetTypeInfoCount(&self) -> Result<u32> {
            Ok(0)
        }

        fn GetTypeInfo(&self, _itinfo: u32, _lcid: u32) -> Result<ITypeInfo> {
            Err(DISP_E_BADINDEX.into())
        }

        fn GetIDsOfNames(
            &self,
            _riid: *const GUID,
            _rgsznames: *const PCWSTR,
            _cnames: u32,
            _lcid: u32,
            _rgdispid: *mut i32,
        ) -> Result<()> {
            Err(DISP_E_UNKNOWNNAME.into())
        }

        fn Invoke(
            &self,
            _dispidmember: i32,
            _riid: *const GUID,
            _lcid: u32,
            _wflags: DISPATCH_FLAGS,
            _pdispparams: *const DISPPARAMS,
            _pvarresult: *mut VARIANT,
            _pexcepinfo: *mut EXCEPINFO,
            _puargerr: *mut u32,
        ) -> Result<()> {
            Err(DISP_E_MEMBERNOTFOUND.into())
        }
    }

    impl IServiceProvider_Impl for FolderDocument_Impl {
        fn QueryService(
            &self,
            guidservice: *const GUID,
            riid: *const GUID,
            ppvobject: *mut *mut c_void,
        ) -> Result<()> {
            unsafe {
                if !ppvobject.is_null() {
                    *ppvobject = std::ptr::null_mut();
                }
                if guidservice.is_null() || riid.is_null() {
                    return Err(E_POINTER.into());
                }
                let service = *guidservice;
                if service == <IFolderView as Interface>::IID
                    || service == <IShellView as Interface>::IID
                {
                    self.view
                        .as_interface::<IShellView>()
                        .query(riid, ppvobject)
                        .ok()
                } else {
                    Err(E_NOINTERFACE.into())
                }
            }
        }
    }

    // SHDocVw's type library, so the shell's identifier marshalling finds names
    // for the properties below. {EAB22AC0-30C1-11CF-A7EB-0000C05BAE0B}
    const LIBID_SHDOCVW: GUID = GUID::from_u128(0xEAB22AC0_30C1_11CF_A7EB_0000C05BAE0B);

    fn browser_type_info() -> Option<ITypeInfo> {
        let library = unsafe { LoadRegTypeLib(&LIBID_SHDOCVW, 1, 1, 0) }
            .inspect_err(|error| tracing::debug!("SHDocVw type library unavailable: {error}"))
            .ok()?;
        unsafe { library.GetTypeInfoOfGuid(&<IWebBrowserApp as Interface>::IID) }
            .inspect_err(|error| tracing::debug!("IWebBrowserApp type info unavailable: {error}"))
            .ok()
    }

    fn no_dispatch() -> Result<IDispatch> {
        Err(E_NOTIMPL.into())
    }

    // What IShellWindows holds for a tab. Location* also lets programs that list
    // shell windows (Shell.Application.Windows()) see which folder a tab shows.
    #[implement(IWebBrowserApp)]
    struct BrowserApp {
        window: isize,
        state: Arc<Mutex<SlotState>>,
        document: ComObject<FolderDocument>,
        type_info: Option<ITypeInfo>,
    }

    impl BrowserApp_Impl {
        fn slot(&self) -> std::sync::MutexGuard<'_, SlotState> {
            self.state.lock().unwrap_or_else(|error| error.into_inner())
        }
    }

    impl IDispatch_Impl for BrowserApp_Impl {
        fn GetTypeInfoCount(&self) -> Result<u32> {
            Ok(u32::from(self.type_info.is_some()))
        }

        fn GetTypeInfo(&self, itinfo: u32, _lcid: u32) -> Result<ITypeInfo> {
            if itinfo != 0 {
                return Err(DISP_E_BADINDEX.into());
            }
            self.type_info.clone().ok_or_else(|| DISP_E_BADINDEX.into())
        }

        fn GetIDsOfNames(
            &self,
            _riid: *const GUID,
            rgsznames: *const PCWSTR,
            cnames: u32,
            _lcid: u32,
            rgdispid: *mut i32,
        ) -> Result<()> {
            let Some(info) = &self.type_info else {
                return Err(DISP_E_UNKNOWNNAME.into());
            };
            unsafe { DispGetIDsOfNames(info, rgsznames, cnames, rgdispid) }
        }

        fn Invoke(
            &self,
            dispidmember: i32,
            _riid: *const GUID,
            _lcid: u32,
            wflags: DISPATCH_FLAGS,
            pdispparams: *const DISPPARAMS,
            pvarresult: *mut VARIANT,
            pexcepinfo: *mut EXCEPINFO,
            puargerr: *mut u32,
        ) -> Result<()> {
            let Some(info) = &self.type_info else {
                return Err(DISP_E_MEMBERNOTFOUND.into());
            };
            // The type library dispatches on the interface pointer, not the
            // outer object: fetch ours.
            let interface = IUnknownImpl::to_interface::<IWebBrowserApp>(self);
            let raw = Interface::as_raw(&interface);
            unsafe {
                DispInvoke(
                    raw,
                    info,
                    dispidmember,
                    wflags.0,
                    pdispparams as *mut DISPPARAMS,
                    pvarresult,
                    pexcepinfo,
                    puargerr,
                )
            }
        }
    }

    impl IWebBrowser_Impl for BrowserApp_Impl {
        fn GoBack(&self) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn GoForward(&self) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn GoHome(&self) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn GoSearch(&self) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Navigate(
            &self,
            _url: &BSTR,
            _flags: *const VARIANT,
            _targetframename: *const VARIANT,
            _postdata: *const VARIANT,
            _headers: *const VARIANT,
        ) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Refresh(&self) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Refresh2(&self, _level: *const VARIANT) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Stop(&self) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Application(&self) -> Result<IDispatch> {
            no_dispatch()
        }

        fn Parent(&self) -> Result<IDispatch> {
            no_dispatch()
        }

        fn Container(&self) -> Result<IDispatch> {
            no_dispatch()
        }

        fn Document(&self) -> Result<IDispatch> {
            Ok(self.document.to_interface::<IDispatch>())
        }

        fn TopLevelContainer(&self) -> Result<VARIANT_BOOL> {
            Ok(VARIANT_TRUE)
        }

        fn Type(&self) -> Result<BSTR> {
            Err(E_NOTIMPL.into())
        }

        fn Left(&self) -> Result<i32> {
            Err(E_NOTIMPL.into())
        }

        fn SetLeft(&self, _left: i32) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Top(&self) -> Result<i32> {
            Err(E_NOTIMPL.into())
        }

        fn SetTop(&self, _top: i32) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Width(&self) -> Result<i32> {
            Err(E_NOTIMPL.into())
        }

        fn SetWidth(&self, _width: i32) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn Height(&self) -> Result<i32> {
            Err(E_NOTIMPL.into())
        }

        fn SetHeight(&self, _height: i32) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn LocationName(&self) -> Result<BSTR> {
            let state = self.slot();
            Ok(BSTR::from(state.display.as_str()))
        }

        fn LocationURL(&self) -> Result<BSTR> {
            let state = self.slot();
            Ok(BSTR::from(state.url.as_str()))
        }

        fn Busy(&self) -> Result<VARIANT_BOOL> {
            Ok(VARIANT_FALSE)
        }
    }

    impl IWebBrowserApp_Impl for BrowserApp_Impl {
        fn Quit(&self) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn ClientToWindow(&self, _pcx: *mut i32, _pcy: *mut i32) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn PutProperty(&self, _property: &BSTR, _vtvalue: &VARIANT) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn GetProperty(&self, _property: &BSTR) -> Result<VARIANT> {
            Err(E_NOTIMPL.into())
        }

        fn Name(&self) -> Result<BSTR> {
            Ok(BSTR::from("LumaFiles"))
        }

        fn HWND(&self) -> Result<SHANDLE_PTR> {
            Ok(SHANDLE_PTR(self.window))
        }

        fn FullName(&self) -> Result<BSTR> {
            Err(E_NOTIMPL.into())
        }

        fn Path(&self) -> Result<BSTR> {
            Err(E_NOTIMPL.into())
        }

        fn Visible(&self) -> Result<VARIANT_BOOL> {
            let visible = unsafe { IsWindowVisible(HWND(self.window as *mut c_void)) };
            Ok(if visible.as_bool() {
                VARIANT_TRUE
            } else {
                VARIANT_FALSE
            })
        }

        fn SetVisible(&self, _value: VARIANT_BOOL) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn StatusBar(&self) -> Result<VARIANT_BOOL> {
            Err(E_NOTIMPL.into())
        }

        fn SetStatusBar(&self, _value: VARIANT_BOOL) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn StatusText(&self) -> Result<BSTR> {
            Err(E_NOTIMPL.into())
        }

        fn SetStatusText(&self, _statustext: &BSTR) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn ToolBar(&self) -> Result<i32> {
            Err(E_NOTIMPL.into())
        }

        fn SetToolBar(&self, _value: i32) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn MenuBar(&self) -> Result<VARIANT_BOOL> {
            Err(E_NOTIMPL.into())
        }

        fn SetMenuBar(&self, _value: VARIANT_BOOL) -> Result<()> {
            Err(E_NOTIMPL.into())
        }

        fn FullScreen(&self) -> Result<VARIANT_BOOL> {
            Err(E_NOTIMPL.into())
        }

        fn SetFullScreen(&self, _bfullscreen: VARIANT_BOOL) -> Result<()> {
            Err(E_NOTIMPL.into())
        }
    }

    /// One tab's registration under one location form.
    struct Registered {
        cookie: i32,
        window_cookie: i32,
        hwnd: isize,
        pending: bool,
        bound: bool,
        ready: bool,
        revoking: bool,
        path: String,
        state: Arc<Mutex<SlotState>>,
        app: ComObject<BrowserApp>,
    }

    struct Worker {
        shared: Arc<Shared>,
        sender: async_channel::Sender<Selection>,
        started: Instant,
        windows: Option<IShellWindows>,
        registered: HashMap<(u64, u8), Registered>,
        wanted: Vec<Entry>,
        revision: u64,
        window: isize,
        window_thread: u32,
        dirty: bool,
        due: u64,
        retry_ms: u32,
    }

    fn tick(started: Instant) -> u64 {
        started.elapsed().as_millis() as u64
    }

    fn disconnected(error: windows::core::HRESULT) -> bool {
        matches!(
            error.0 as u32,
            0x8001_0108 // RPC_E_DISCONNECTED
                | 0x8004_01FD // CO_E_OBJNOTCONNECTED
                | 0x8007_06BA // RPC_S_SERVER_UNAVAILABLE
                | 0x8001_0114 // RPC_E_SERVER_DIED
                | 0x8001_0115 // RPC_E_SERVER_DIED_DNE
        )
    }

    impl Worker {
        fn new(
            shared: Arc<Shared>,
            sender: async_channel::Sender<Selection>,
            started: Instant,
        ) -> Self {
            Self {
                shared,
                sender,
                started,
                windows: None,
                registered: HashMap::new(),
                wanted: Vec::new(),
                revision: 0,
                window: 0,
                window_thread: 0,
                dirty: false,
                due: 0,
                retry_ms: RETRY_FIRST_MS,
            }
        }

        fn now(&self) -> u64 {
            tick(self.started)
        }

        fn wait_ms(&self) -> u32 {
            if !self.dirty {
                return IDLE_MS;
            }
            let now = self.now();
            if now >= self.due {
                0
            } else {
                (self.due - now).min(IDLE_MS as u64) as u32
            }
        }

        fn apply(&mut self) {
            let revision = self.shared.revision.load(Ordering::Acquire);
            if revision != self.revision {
                self.revision = revision;
                self.wanted = self.shared.lock_desired().clone();
                self.window = self.shared.window.load(Ordering::Relaxed);
                self.window_thread =
                    unsafe { GetWindowThreadProcessId(HWND(self.window as *mut c_void), None) };
                self.dirty = true;
                self.due = 0;
                self.retry_ms = RETRY_FIRST_MS;
            }
            if !self.dirty || self.shared.stopping() {
                return;
            }
            if self.now() < self.due {
                return;
            }

            let mut failure = windows::core::HRESULT(0);
            if self.windows.is_none() && (!self.wanted.is_empty() || !self.registered.is_empty()) {
                match unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL) } {
                    Ok(windows) => self.windows = Some(windows),
                    Err(error) => failure = error.code(),
                }
            }
            if failure.is_ok() {
                let wanted = self.wanted.clone();
                let removed: Vec<(u64, u8)> = self
                    .registered
                    .iter()
                    .filter(|(slot, entry)| {
                        entry.revoking
                            || !wanted
                                .iter()
                                .any(|want| want.key == slot.0 && want.hwnd == entry.hwnd)
                    })
                    .map(|(slot, _)| *slot)
                    .collect();
                for slot in removed {
                    if self.shared.stopping() {
                        return;
                    }
                    if let Err(error) = self.revoke(slot) {
                        failure = error.code();
                    }
                    if disconnected(failure) {
                        break;
                    }
                }
                if !disconnected(failure) {
                    for entry in wanted {
                        if self.shared.stopping() {
                            return;
                        }
                        if let Err(error) = self.advance(entry.key, &entry.path, entry.hwnd) {
                            failure = error.code();
                        }
                        if disconnected(failure) {
                            break;
                        }
                    }
                }
            }

            if disconnected(failure) {
                // Explorer restarted underneath us: forget every cookie and
                // start over with a fresh object.
                self.registered.clear();
                self.windows = None;
            }

            self.dirty = failure.is_err();
            if self.dirty {
                let delay = self.retry_ms;
                self.due = self.now() + delay as u64;
                self.retry_ms = (self.retry_ms * 2).min(RETRY_MAX_MS);
                tracing::debug!(
                    "shell window registration retry in {delay} ms: 0x{:08x}",
                    failure.0 as u32
                );
            } else {
                self.due = 0;
                self.retry_ms = RETRY_FIRST_MS;
            }
        }

        fn settled(&self, slot: &(u64, u8), path: &str) -> bool {
            self.registered
                .get(slot)
                .is_some_and(|entry| entry.ready && entry.path.eq_ignore_ascii_case(path))
        }

        fn advance(&mut self, key: u64, path: &str, hwnd: isize) -> Result<()> {
            if self
                .registered
                .get(&(key, PRIMARY))
                .is_some_and(|entry| entry.revoking)
            {
                return Ok(());
            }
            if self.settled(&(key, PRIMARY), path)
                && (!self.registered.contains_key(&(key, ALIAS))
                    || self.settled(&(key, ALIAS), path))
            {
                return Ok(());
            }
            let (primary, alias) = folder_pidls(path);
            let Some(primary) = primary else {
                return Err(windows::core::Error::from(E_FAIL));
            };
            self.advance_slot((key, PRIMARY), path, hwnd, primary)?;
            match alias {
                Some(alias) => self.advance_slot((key, ALIAS), path, hwnd, alias),
                None => self.revoke((key, ALIAS)),
            }
        }

        fn advance_slot(
            &mut self,
            slot: (u64, u8),
            path: &str,
            hwnd: isize,
            folder: OwnedPidl,
        ) -> Result<()> {
            let window = if hwnd != 0 { hwnd } else { self.window };
            if !self.registered.contains_key(&slot) {
                let state = Arc::new(Mutex::new(SlotState::default()));
                let view = ComObject::new(FolderView {
                    key: slot.0,
                    window,
                    sender: self.sender.clone(),
                    state: state.clone(),
                });
                let document = ComObject::new(FolderDocument { view });
                let app = ComObject::new(BrowserApp {
                    window,
                    state: state.clone(),
                    document,
                    type_info: browser_type_info(),
                });
                self.registered.insert(
                    slot,
                    Registered {
                        cookie: 0,
                        window_cookie: 0,
                        hwnd,
                        pending: false,
                        bound: false,
                        ready: false,
                        revoking: false,
                        path: String::new(),
                        state,
                        app,
                    },
                );
            }
            if self
                .registered
                .get(&slot)
                .is_some_and(|entry| entry.revoking)
            {
                return Ok(());
            }

            let mut location =
                unsafe { InitVariantFromBuffer(folder.as_ptr() as *const c_void, folder.size()) }?;
            let registered = self.register_slot(slot, &location, window);
            unsafe { VariantClear(&mut location) }.log_err();
            registered?;

            let display =
                pidl_name(folder.as_ptr(), SIGDN_NORMALDISPLAY).unwrap_or_else(|| path.to_owned());
            tracing::debug!("shell window registered for {path}");
            if let Some(entry) = self.registered.get_mut(&slot) {
                entry.path = path.to_owned();
                entry.ready = true;
                let mut state = entry
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                state.display = display;
                state.url = url_for_path(path);
                state.folder = Some(folder);
            }
            Ok(())
        }

        fn register_slot(
            &mut self,
            slot: (u64, u8),
            location: &VARIANT,
            window: isize,
        ) -> Result<()> {
            let Some(windows) = self.windows.clone() else {
                return Err(windows::core::Error::from(E_FAIL));
            };
            let stopping = self.shared.stopping();
            let Some(entry) = self.registered.get_mut(&slot) else {
                return Err(windows::core::Error::from(E_FAIL));
            };
            if !entry.pending && !stopping {
                let root = VARIANT::default();
                let cookie = unsafe {
                    windows.RegisterPending(self.window_thread as i32, location, &root, SWC_BROWSER)
                }?;
                entry.cookie = cookie;
                entry.pending = true;
            }
            if !entry.bound && !stopping {
                let app = entry.app.to_interface::<IWebBrowserApp>();
                let cookie = unsafe { windows.Register(&app, window as i32, SWC_BROWSER) }?;
                entry.window_cookie = cookie;
                entry.bound = true;
            }
            if !stopping {
                unsafe { windows.OnNavigate(entry.cookie, location) }?;
            }
            Ok(())
        }

        fn revoke(&mut self, slot: (u64, u8)) -> Result<()> {
            let Some(entry) = self.registered.get_mut(&slot) else {
                return Ok(());
            };
            entry.revoking = true;
            entry.ready = false;
            entry
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .folder = None;
            let Some(windows) = self.windows.clone() else {
                self.registered.remove(&slot);
                return Ok(());
            };
            if entry.pending {
                unsafe { windows.Revoke(entry.cookie) }?;
                entry.pending = false;
                if entry.bound && entry.window_cookie == entry.cookie {
                    entry.bound = false;
                }
            }
            if entry.bound {
                unsafe { windows.Revoke(entry.window_cookie) }?;
                entry.bound = false;
            }
            self.registered.remove(&slot);
            Ok(())
        }

        fn stop(&mut self) {
            let slots: Vec<(u64, u8)> = self.registered.keys().copied().collect();
            let total = slots.len();
            for slot in slots {
                // Best effort: the process is going away, and a hung Explorer
                // must not keep it here.
                if let Err(error) = self.revoke(slot) {
                    tracing::debug!("shell window could not be revoked: {error}");
                    self.registered.remove(&slot);
                }
            }
            tracing::debug!("shell windows revoked: {total}");
            self.windows = None;
        }
    }
}
