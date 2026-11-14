//! The service: answers snapshot and search requests on the pipe, one at a
//! time.
//!
//! It runs as SYSTEM because opening a volume to read its MFT needs that,
//! and it is careful accordingly: it reads, never writes, answers only the
//! user it was installed for (and administrators), only on this machine, and
//! only the requests of [`crate::protocol`]. Between requests it keeps the
//! names it last read, so a search is a scan of memory rather than a minute
//! of reading the MFT — that is the whole point of the search request.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, BufWriter, Read as _, Write as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, HLOCAL, LocalFree, NO_ERROR,
};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FlushFileBuffers, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
    SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::core::{HSTRING, PWSTR};

use crate::{PIPE_NAME, SERVICE_NAME, mft, protocol};

mod journal;
mod index;

#[cfg(test)]
#[path = "service/request_tests.rs"]
mod request_tests;

static ALLOWED: OnceLock<String> = OnceLock::new();
static STATUS: AtomicIsize = AtomicIsize::new(0);
static STOPPING: AtomicBool = AtomicBool::new(false);
static INDEX: OnceLock<Arc<index::Index>> = OnceLock::new();

/// Runs as the service the Service Control Manager started, answering the
/// user `allowed_sid` (validated by the caller).
pub fn run(allowed_sid: String) -> io::Result<()> {
    if ALLOWED.set(allowed_sid).is_err() {
        return Err(io::Error::other("the service is already running"));
    }
    let name: Vec<u16> = SERVICE_NAME.encode_utf16().chain([0]).collect();
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(name.as_ptr().cast_mut()), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.map_err(io::Error::from)
}

fn set_state(state: SERVICE_STATUS_CURRENT_STATE) {
    let handle = SERVICE_STATUS_HANDLE(STATUS.load(Ordering::SeqCst) as *mut c_void);
    if handle.0.is_null() {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_STOP_PENDING { 5_000 } else { 0 },
    };
    if let Err(error) = unsafe { SetServiceStatus(handle, &status) } {
        log(&format!("SetServiceStatus failed: {error}"));
    }
}

unsafe extern "system" fn service_main(_count: u32, _arguments: *mut PWSTR) {
    let name = HSTRING::from(SERVICE_NAME);
    match unsafe { RegisterServiceCtrlHandlerExW(&name, Some(control), None) } {
        Ok(handle) => STATUS.store(handle.0 as isize, Ordering::SeqCst),
        Err(error) => {
            log(&format!("RegisterServiceCtrlHandlerExW failed: {error}"));
            return;
        }
    }
    set_state(SERVICE_RUNNING);
    log("started");
    let index = INDEX.get_or_init(|| Arc::new(index::Index::default())).clone();
    let worker = match std::thread::Builder::new().name("prime-index-updates".into()).spawn({
        let index = index.clone();
        move || {
            if std::panic::catch_unwind(|| index::run(&index)).is_err() {
                log("index worker panicked");
                index.stop();
            }
        }
    }) {
        Ok(worker) => worker,
        Err(error) => {
            log(&format!("could not start index worker: {error}"));
            set_state(SERVICE_STOPPED);
            return;
        }
    };
    serve();
    index.stop();
    if worker.join().is_err() { log("index worker join failed"); }
    log("stopped");
    set_state(SERVICE_STOPPED);
}

unsafe extern "system" fn control(code: u32, _kind: u32, _data: *mut c_void, _context: *mut c_void) -> u32 {
    match code {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOPPING.store(true, Ordering::SeqCst);
            if let Some(index) = INDEX.get() { index.stop(); }
            set_state(SERVICE_STOP_PENDING);
            // The loop waits in ConnectNamedPipe: a connection wakes it.
            std::thread::spawn(|| {
                if let Err(error) = std::fs::OpenOptions::new().read(true).write(true).open(PIPE_NAME) {
                    log(&format!("could not wake the pipe to stop: {error}"));
                }
            });
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

fn serve() {
    let Some(sid) = ALLOWED.get() else {
        return;
    };
    // System and administrators in full; the allowed user may read and
    // write (its request) and nothing else. "P": no inherited entries.
    let sddl = HSTRING::from(format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{sid})"));
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    if let Err(error) =
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut descriptor, None) }
    {
        log(&format!("bad security descriptor: {error}"));
        return;
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let name = HSTRING::from(PIPE_NAME);
    while !STOPPING.load(Ordering::SeqCst) {
        // The first instance only: if another program already took the name,
        // creating fails instead of joining it.
        let pipe = unsafe {
            CreateNamedPipeW(
                &name,
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                1 << 20,
                4096,
                0,
                Some(&attributes),
            )
        };
        if pipe.is_invalid() {
            log(&format!("CreateNamedPipeW failed: {:?}", unsafe { GetLastError() }));
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        let connected = unsafe { ConnectNamedPipe(pipe, None) }.is_ok() || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        let file = unsafe { File::from_raw_handle(pipe.0) };
        if connected && !STOPPING.load(Ordering::SeqCst)
            && let Err(error) = answer(&file)
        {
            log(&format!("request failed: {error}"));
        }
        if let Err(error) = unsafe { DisconnectNamedPipe(HANDLE(file.as_raw_handle())) } {
            log(&format!("DisconnectNamedPipe failed: {error}"));
        }
    }
    unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
}

fn answer(pipe: &File) -> io::Result<()> {
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    let mut reader = pipe;
    while request.len() < 64 {
        if reader.read(&mut byte)? == 0 {
            break;
        }
        request.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    let mut out = BufWriter::with_capacity(1 << 20, pipe);
    if request == protocol::HELLO {
        out.write_all(format!("PFIX1 OK {}\n", protocol::VERSION).as_bytes())?;
        out.flush()?;
    } else if request == protocol::SNAPSHOT {
        match cached() {
            Ok(volumes) => protocol::write_shared(&mut out, &volumes)?,
            Err(error) => protocol::write_error(&mut out, &error.to_string())?,
        }
    } else if request == protocol::REFRESH {
        if let Some(index) = INDEX.get() { index.refresh(); }
        out.write_all(format!("PFIX1 OK {}\n", protocol::VERSION).as_bytes())?;
        out.flush()?;
    } else if request == protocol::SEARCH {
        // The terms and the count were written after the request line.
        let body = read_request_body(reader)?;
        let (terms, limit) = protocol::read_search_request(&mut &body[..])?;
        match search(&terms, limit) {
            Ok(hits) => protocol::write_hits(&mut out, &hits)?,
            Err(error) => protocol::write_error(&mut out, &error.to_string())?,
        }
    } else {
        protocol::write_error(&mut out, "unknown request")?;
    }
    drop(out);
    // The answer reaches the reader before the pipe is cut.
    unsafe { FlushFileBuffers(HANDLE(pipe.as_raw_handle())) }.map_err(io::Error::from)
}

// The rest of a request after its line: as long as the client says it is.
fn read_request_body(mut pipe: &File) -> io::Result<Vec<u8>> {
    // The body starts with the term count; what follows is sized from it.
    let mut head = [0u8; 2];
    pipe.read_exact(&mut head)?;
    let count = u16::from_le_bytes(head) as usize;
    let mut body = Vec::with_capacity(2 + count * 32 + 4);
    body.extend_from_slice(&head);
    for _ in 0..count {
        let mut length = [0u8; 2];
        pipe.read_exact(&mut length)?;
        body.extend_from_slice(&length);
        let length = u16::from_le_bytes(length) as usize;
        if length > 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "search term too long"));
        }
        let mut term = vec![0u8; length];
        pipe.read_exact(&mut term)?;
        body.extend_from_slice(&term);
    }
    let mut limit = [0u8; 4];
    pipe.read_exact(&mut limit)?;
    body.extend_from_slice(&limit);
    Ok(body)
}

fn cached() -> io::Result<Arc<Vec<Arc<protocol::Volume>>>> {
    INDEX.get().ok_or_else(|| io::Error::other("index worker not running"))?.snapshot()
}

/// The paths holding every one of `terms`, whole, ignoring case, limited to
/// `limit` hits. The name of every entry is checked before the path is built:
/// a path is walked up its parents, which the millions of names that do not
/// match must not pay for.
fn search(terms: &[String], limit: u32) -> io::Result<Vec<protocol::Hit>> {
    if terms.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let volumes = cached()?;
    let mut hits = Vec::new();
    for volume in volumes.iter() {
        volume.search(terms, limit as usize, &mut hits);
        if hits.len() >= limit as usize {
            break;
        }
    }
    Ok(hits)
}

/// Reads a standalone snapshot; pipe requests use the background index instead.
pub fn snapshot() -> Vec<protocol::Volume> {
    let profile = ALLOWED.get().and_then(|sid| profile_path(sid));
    ntfs_volumes().into_iter().filter_map(|root| match mft::read_volume(&root) {
        Ok(table) => Some(mft::tree(&table, &root, &policy_for(&root, profile.as_deref()))),
        Err(error) => { log(&format!("{root}: {error}")); None }
    }).collect()
}

// Which profile folders of `root` are listed: the user's own and Public.
// With the profile unknown, none but Public, on the system drive.
fn policy_for(root: &str, profile: Option<&str>) -> mft::Policy {
    let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let (holder, own) = match profile {
        Some(profile) => {
            let lowered = profile.to_lowercase();
            let mut parts: Vec<&str> = lowered.split('\\').filter(|part| !part.is_empty()).collect();
            let drive = parts.first().map(|drive| drive.to_string()).unwrap_or_default();
            let own = parts.pop().map(str::to_string);
            let holder: Vec<String> = parts.iter().skip(1).map(|part| part.to_string()).collect();
            (drive.eq_ignore_ascii_case(root).then_some(holder), own)
        }
        None => (system_drive.eq_ignore_ascii_case(root).then(|| vec!["users".to_string()]), None),
    };
    match holder {
        Some(profiles) => mft::Policy { profiles, allowed: own.into_iter().chain(["public".to_string()]).collect() },
        None => mft::Policy { profiles: Vec::new(), allowed: Vec::new() },
    }
}

fn profile_path(sid: &str) -> Option<String> {
    let key = HSTRING::from(format!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\{sid}"));
    let value = HSTRING::from("ProfileImagePath");
    let mut buffer = vec![0u16; 1024];
    let mut size = (buffer.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &key,
            &value,
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    }
    .ok()
    .ok()?;
    let length = buffer.iter().position(|&unit| unit == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..length]))
}

/// "C:", "D:"… for every fixed drive formatted NTFS.
pub fn ntfs_volumes() -> Vec<String> {
    const DRIVE_FIXED: u32 = 3;
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| format!("{}:", char::from(b'A' + bit)))
        .filter(|root| {
            let wide = HSTRING::from(format!("{root}\\"));
            if unsafe { GetDriveTypeW(&wide) } != DRIVE_FIXED {
                return false;
            }
            let mut file_system = [0u16; 32];
            unsafe { GetVolumeInformationW(&wide, None, None, None, None, Some(&mut file_system)) }.is_ok()
                && String::from_utf16_lossy(&file_system).trim_end_matches('\0') == "NTFS"
        })
        .collect()
}

fn log_file() -> PathBuf {
    let data = std::env::var_os("ProgramData").map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
    data.join("Prime Files Indexer").join("indexer.log")
}

/// A line in the service's own log, in ProgramData (it has no console and
/// no user to show anything to). Kept under a megabyte.
pub fn log(line: &str) {
    let path = log_file();
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let append = std::fs::metadata(&path).map_or(true, |metadata| metadata.len() < 1 << 20);
    let file = std::fs::OpenOptions::new().create(true).append(append).write(true).truncate(!append).open(&path);
    if let Ok(mut file) = file {
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        if writeln!(file, "{seconds} {line}").is_err() {
            // Nowhere else to say it.
        }
    }
}
