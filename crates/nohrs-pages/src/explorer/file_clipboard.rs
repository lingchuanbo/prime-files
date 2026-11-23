use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use nohrs_core::telemetry::LogErr;
use windows::Win32::Foundation::{
    ERROR_SUCCESS, GetLastError, GlobalFree, HANDLE, HGLOBAL, SetLastError,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_MOVE};
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows::Win32::UI::Shell::DROPFILES;
use windows::core::w;

use super::file_ops::ClipMode;

pub(super) fn write(paths: &[String], mode: ClipMode) -> Result<()> {
    let files = Memory::new(&file_drop_bytes(paths)?)?;
    let effect = Memory::new(&drop_effect(mode).to_le_bytes())?;
    let effect_format = unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) };
    ensure!(
        effect_format != 0,
        "Could not register the file clipboard format"
    );
    let _clipboard = Clipboard::open()?;
    unsafe { EmptyClipboard()? };
    files.publish(CF_HDROP.0 as u32)?;
    if let Err(error) = effect.publish(effect_format) {
        unsafe { EmptyClipboard() }.log_err();
        return Err(error);
    }
    Ok(())
}

fn drop_effect(mode: ClipMode) -> u32 {
    match mode {
        ClipMode::Copy => DROPEFFECT_COPY.0,
        ClipMode::Cut => DROPEFFECT_MOVE.0,
    }
}

fn file_drop_bytes(paths: &[String]) -> Result<Vec<u8>> {
    ensure!(!paths.is_empty(), "No files selected");
    let header = DROPFILES {
        pFiles: std::mem::size_of::<DROPFILES>() as u32,
        fWide: true.into(),
        ..Default::default()
    };
    let mut bytes = vec![0; std::mem::size_of::<DROPFILES>()];
    unsafe { std::ptr::write_unaligned(bytes.as_mut_ptr().cast::<DROPFILES>(), header) };
    for path in paths {
        ensure!(
            Path::new(path).is_absolute(),
            "File clipboard requires an absolute path"
        );
        ensure!(!path.contains('\0'), "File path contains a null character");
        for unit in std::ffi::OsStr::new(path).encode_wide().chain(Some(0)) {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&0u16.to_le_bytes());
    Ok(bytes)
}

struct Clipboard;

impl Clipboard {
    fn open() -> Result<Self> {
        let owner = unsafe { GetActiveWindow() };
        ensure!(
            !owner.0.is_null(),
            "No active window for the file clipboard"
        );
        unsafe { OpenClipboard(Some(owner)) }.context("Could not open the file clipboard")?;
        Ok(Self)
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        unsafe { CloseClipboard() }.log_err();
    }
}

struct Memory(HGLOBAL);

impl Memory {
    fn new(bytes: &[u8]) -> Result<Self> {
        let memory = Self(unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len())? });
        let target = unsafe { GlobalLock(memory.0) };
        ensure!(!target.is_null(), "Could not lock file clipboard memory");
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.cast::<u8>(), bytes.len());
            SetLastError(ERROR_SUCCESS);
            if GlobalUnlock(memory.0).is_err() && GetLastError() != ERROR_SUCCESS {
                return Err(windows::core::Error::from_win32().into());
            }
        }
        Ok(memory)
    }

    fn publish(self, format: u32) -> Result<()> {
        unsafe { SetClipboardData(format, Some(HANDLE(self.0.0))) }?;
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        unsafe { GlobalFree(Some(self.0)) }.log_err();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_drop_has_a_wide_header_and_separate_null_terminated_paths() {
        let paths = vec![
            "C:\\项目\\文件 a.txt".into(),
            "\\\\server\\share\\folder".into(),
        ];
        let bytes = file_drop_bytes(&paths).unwrap();
        let header = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<DROPFILES>()) };
        assert_eq!(std::mem::size_of::<DROPFILES>(), header.pFiles as usize);
        assert!(header.fWide.as_bool());
        let names: Vec<u16> = bytes[header.pFiles as usize..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(
            "C:\\项目\\文件 a.txt\0\\\\server\\share\\folder\0\0",
            String::from_utf16(&names).unwrap()
        );
    }

    #[test]
    fn file_drop_rejects_empty_relative_and_embedded_null_paths() {
        for paths in [
            vec![],
            vec!["relative.txt".into()],
            vec!["C:\\a\0b.txt".into()],
        ] {
            assert!(file_drop_bytes(&paths).is_err());
        }
    }

    #[test]
    fn copy_and_cut_publish_distinct_shell_effects() {
        assert_eq!(DROPEFFECT_COPY.0, drop_effect(ClipMode::Copy));
        assert_eq!(DROPEFFECT_MOVE.0, drop_effect(ClipMode::Cut));
    }

    #[test]
    fn native_memory_preserves_the_file_drop_payload() {
        let bytes = file_drop_bytes(&["C:\\资料\\a.txt".into()]).unwrap();
        let memory = Memory::new(&bytes).unwrap();
        let pointer = unsafe { GlobalLock(memory.0) };
        assert!(!pointer.is_null());
        let stored = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), bytes.len()) };
        assert_eq!(bytes.as_slice(), stored);
        unsafe {
            SetLastError(ERROR_SUCCESS);
            let result = GlobalUnlock(memory.0);
            assert!(result.is_ok() || GetLastError() == ERROR_SUCCESS);
        }
        let _writer: fn(&[String], ClipMode) -> Result<()> = write;
    }
}
