//! The app's side: is the service there, give me the names, install it.
#![allow(unsafe_code)]

use std::fs::File;
use std::io::{self, BufReader, Read as _, Write as _};
use std::path::PathBuf;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, ERROR_SERVICE_DOES_NOT_EXIST, HANDLE, HLOCAL, LocalFree, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
    SERVICE_RUNNING, SERVICE_STATUS,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcessToken, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{HSTRING, PWSTR};

use crate::{EXE_NAME, PIPE_NAME, SERVICE_NAME, protocol};

/// Where the service stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Never installed, or removed.
    NotInstalled,
    /// Installed and not running (stopped by hand, or failed).
    Stopped,
    /// Answering.
    Running,
}

/// Asks the Service Control Manager, which any user may.
pub fn status() -> Status {
    let Ok(manager) = (unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }) else {
        return Status::NotInstalled;
    };
    let name = HSTRING::from(SERVICE_NAME);
    let found = match unsafe { OpenServiceW(manager, &name, SERVICE_QUERY_STATUS) } {
        Ok(service) => {
            let mut state = SERVICE_STATUS::default();
            let running =
                unsafe { QueryServiceStatus(service, &mut state) }.is_ok() && state.dwCurrentState == SERVICE_RUNNING;
            if let Err(error) = unsafe { CloseServiceHandle(service) } {
                eprintln!("CloseServiceHandle failed: {error}");
            }
            if running { Status::Running } else { Status::Stopped }
        }
        Err(error) if WIN32_ERROR::from_error(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => Status::NotInstalled,
        Err(_) => Status::Stopped,
    };
    if let Err(error) = unsafe { CloseServiceHandle(manager) } {
        eprintln!("CloseServiceHandle failed: {error}");
    }
    found
}

fn connect() -> io::Result<File> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let name = HSTRING::from(PIPE_NAME);
    for _ in 0..3 {
        match std::fs::OpenOptions::new().read(true).write(true).open(PIPE_NAME) {
            // Another request is being answered: wait for the pipe, then try again.
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if !unsafe { WaitNamedPipeW(&name, 30_000) }.as_bool() {
                    return Err(error);
                }
            }
            other => return other,
        }
    }
    Err(io::Error::other("the indexer stayed busy"))
}

/// The version the running service speaks.
pub fn hello() -> io::Result<u32> {
    let mut pipe = connect()?;
    pipe.write_all(protocol::HELLO)?;
    let mut answer = String::new();
    pipe.read_to_string(&mut answer)?;
    answer
        .trim()
        .strip_prefix("PFIX1 OK ")
        .and_then(|version| version.parse().ok())
        .ok_or_else(|| io::Error::other(format!("unexpected answer: {answer:?}")))
}

/// Reads every NTFS volume's names into `sink`.
pub fn snapshot(sink: &mut impl protocol::Sink) -> io::Result<()> {
    let mut pipe = connect()?;
    pipe.write_all(protocol::SNAPSHOT)?;
    protocol::read(&mut BufReader::with_capacity(1 << 20, pipe), sink)
}

/// Asks the service for the names holding every one of `terms`, ignoring
/// case: the few thousand paths that matched, rather than the whole table.
/// The service keeps the names itself, so nothing of it is held here.
pub fn search(terms: &[String], limit: u32) -> io::Result<Vec<protocol::Hit>> {
    let mut pipe = connect()?;
    protocol::write_search_request(&mut pipe, terms, limit)?;
    protocol::read_hits(&mut BufReader::with_capacity(1 << 20, pipe))
}

/// Schedules a background rebuild, keeping the published search snapshot
/// available until the replacement is ready.
pub fn refresh() -> io::Result<()> {
    let mut pipe = connect()?;
    pipe.write_all(protocol::REFRESH)?;
    let mut answer = String::new();
    pipe.read_to_string(&mut answer)?;
    answer.trim().strip_prefix("PFIX1 OK ").map(drop).ok_or_else(|| io::Error::other(format!("unexpected answer: {answer:?}")))
}

/// The string SID of the user running this program.
pub fn current_user_sid() -> io::Result<String> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.map_err(io::Error::from)?;
    let mut size = 0u32;
    // The first call says how much room the answer needs.
    let _sizing = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut size) };
    let mut buffer = vec![0u8; size as usize];
    let read = unsafe { GetTokenInformation(token, TokenUser, Some(buffer.as_mut_ptr().cast()), size, &mut size) };
    if let Err(error) = unsafe { CloseHandle(token) } {
        eprintln!("CloseHandle failed: {error}");
    }
    read.map_err(io::Error::from)?;
    if buffer.len() < std::mem::size_of::<TOKEN_USER>() {
        return Err(io::Error::other("short token information"));
    }
    let user = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast::<TOKEN_USER>()) };
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }.map_err(io::Error::from)?;
    let sid = unsafe { text.to_string() }.map_err(|error| io::Error::other(error.to_string()));
    unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
    sid
}

/// The indexer program shipped beside the running app. A development build
/// can run from `target\release\deps`, where cargo keeps a copy of the app
/// but not of the indexer, which is one folder up.
pub fn shipped_indexer() -> io::Result<PathBuf> {
    let app = std::env::current_exe()?;
    let beside = app.with_file_name(EXE_NAME);
    let candidates = [
        Some(beside.clone()),
        app.parent()
            .filter(|folder| folder.file_name().is_some_and(|name| name.eq_ignore_ascii_case("deps")))
            .and_then(|deps| deps.parent())
            .map(|up| up.join(EXE_NAME)),
    ];
    candidates.into_iter().flatten().find(|path| path.exists()).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("{} is missing", beside.display()))
    })
}

/// Runs the shipped indexer elevated with `arguments`, waiting for it.
/// `None` when the person said no to the UAC prompt; otherwise its exit
/// code, and what it reported in `report` on failure.
pub fn run_elevated(arguments: &str, report: &std::path::Path) -> io::Result<Option<(u32, String)>> {
    let program = HSTRING::from(shipped_indexer()?.as_os_str());
    let parameters = HSTRING::from(format!("{arguments} --report \"{}\"", report.display()));
    let verb = HSTRING::from("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: windows::core::PCWSTR(verb.as_ptr()),
        lpFile: windows::core::PCWSTR(program.as_ptr()),
        lpParameters: windows::core::PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
        if WIN32_ERROR::from_error(&error) == Some(ERROR_CANCELLED) {
            return Ok(None);
        }
        return Err(io::Error::from(error));
    }
    let mut code = 0u32;
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code).map_err(io::Error::from)?;
        if let Err(error) = CloseHandle(info.hProcess) {
            eprintln!("CloseHandle failed: {error}");
        }
    }
    let message = std::fs::read_to_string(report).unwrap_or_default();
    Ok(Some((code, message.trim().to_string())))
}
