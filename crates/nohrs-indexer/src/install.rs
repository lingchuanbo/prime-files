//! Installing and removing the service, run elevated (the app asks Windows
//! for that with "runas"; the person sees the UAC prompt).
#![allow(unsafe_code)]

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, WIN32_ERROR,
};
use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_HANDLE,
    SC_MANAGER_ALL_ACCESS, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_STATUS,
    SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};
use windows::core::{HSTRING, PWSTR};

use crate::{EXE_NAME, SERVICE_NAME, is_sid};

const DISPLAY_NAME: &str = "Prime Files Indexer";
const DESCRIPTION: &str = "Reads the NTFS file tables so Prime Files can search the whole computer instantly. \
Read-only; answers only the user who installed it.";

/// Where the service's program lives: under Program Files, which only an
/// administrator can write, so no one else can swap what runs as SYSTEM.
pub fn install_dir() -> PathBuf {
    let base = std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from);
    base.join("Prime Files Indexer")
}

struct Handle(SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if let Err(error) = unsafe { CloseServiceHandle(self.0) } {
            crate::service::log(&format!("CloseServiceHandle failed: {error}"));
        }
    }
}

fn manager() -> io::Result<Handle> {
    unsafe { OpenSCManagerW(None, None, SC_MANAGER_ALL_ACCESS) }
        .map(Handle)
        .map_err(io::Error::from)
}

fn open(manager: &Handle) -> io::Result<Option<Handle>> {
    let name = HSTRING::from(SERVICE_NAME);
    match unsafe { OpenServiceW(manager.0, &name, SERVICE_ALL_ACCESS) } {
        Ok(handle) => Ok(Some(Handle(handle))),
        Err(error) if WIN32_ERROR::from_error(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            Ok(None)
        }
        Err(error) => Err(io::Error::from(error)),
    }
}

fn stop(service: &Handle) {
    let mut status = SERVICE_STATUS::default();
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) }.is_err() {
        // Not running.
        return;
    }
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15) {
        if unsafe { QueryServiceStatus(service.0, &mut status) }.is_err()
            || status.dwCurrentState == SERVICE_STOPPED
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Installs (or updates) the service for `allowed_sid` and starts it.
pub fn install(allowed_sid: &str) -> io::Result<()> {
    if !is_sid(allowed_sid) {
        return Err(io::Error::other("not a user SID"));
    }
    let manager = manager()?;
    let existing = open(&manager)?;
    // Its program is in use while it runs.
    if let Some(service) = &existing {
        stop(service);
    }
    let dir = install_dir();
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(EXE_NAME);
    let current = std::env::current_exe()?;
    if !same_file(&current, &target) {
        std::fs::copy(&current, &target)?;
    }
    let command = HSTRING::from(format!(
        "\"{}\" --service --allow {allowed_sid}",
        target.display()
    ));
    let display = HSTRING::from(DISPLAY_NAME);
    let service = match existing {
        Some(service) => {
            unsafe {
                ChangeServiceConfigW(
                    service.0,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    &command,
                    None,
                    None,
                    None,
                    None,
                    None,
                    &display,
                )
            }
            .map_err(io::Error::from)?;
            service
        }
        None => {
            let name = HSTRING::from(SERVICE_NAME);
            // No account given: LocalSystem, the one that may open a volume.
            unsafe {
                CreateServiceW(
                    manager.0,
                    &name,
                    &display,
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    &command,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .map(Handle)
            .map_err(io::Error::from)?
        }
    };
    let mut text: Vec<u16> = DESCRIPTION.encode_utf16().chain([0]).collect();
    let description = SERVICE_DESCRIPTIONW {
        lpDescription: PWSTR(text.as_mut_ptr()),
    };
    if let Err(error) = unsafe {
        ChangeServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            Some(std::ptr::from_ref(&description).cast()),
        )
    } {
        crate::service::log(&format!("could not set the description: {error}"));
    }
    match unsafe { StartServiceW(service.0, None) } {
        Ok(()) => Ok(()),
        Err(error) if WIN32_ERROR::from_error(&error) == Some(ERROR_SERVICE_ALREADY_RUNNING) => {
            Ok(())
        }
        Err(error) => Err(io::Error::from(error)),
    }
}

/// Stops and removes the service and its program.
pub fn uninstall() -> io::Result<()> {
    let manager = manager()?;
    if let Some(service) = open(&manager)? {
        stop(&service);
        unsafe { DeleteService(service.0) }.map_err(io::Error::from)?;
    }
    let dir = install_dir();
    if dir.exists() && std::fs::remove_dir_all(&dir).is_err() {
        // Still in use (this very program, when run from there): gone at the
        // next restart.
        for path in [dir.join(EXE_NAME), dir.clone()] {
            let wide = HSTRING::from(path.as_os_str());
            if let Err(error) = unsafe { MoveFileExW(&wide, None, MOVEFILE_DELAY_UNTIL_REBOOT) } {
                crate::service::log(&format!(
                    "could not schedule {} for removal: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
