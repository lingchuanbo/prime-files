//! Embeds the Windows resources: the icon and the version information.
//!
//! The icon has to be resource 1: gpui loads that one for the window class,
//! so the same icon shows on the window, the taskbar, Alt+Tab, the shortcuts
//! and in the list of installed apps. The version block is what File
//! Explorer's Properties and Task Manager read the name "LumaFiles" from,
//! instead of the file name.
//!
//! The `.rc` is written here, into `OUT_DIR`, rather than kept in the tree,
//! so the version always comes from Cargo and the icon's path is absolute.
//!
//! A build script has no UI thread, and a failure to write the resource script
//! must fail the build rather than be ignored (spec §4.2); `OUT_DIR` and the
//! resource compiler are build invariants.
#![allow(clippy::disallowed_methods, clippy::expect_used)]
use std::path::Path;

fn main() {
    let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/icon/prime-files.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let version = env!("CARGO_PKG_VERSION");
    let numbers = format!(
        "{},{},{},0",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH")
    );
    // Forward slashes: rc.exe takes them, and a backslash in an rc string
    // is an escape.
    let icon = icon.display().to_string().replace('\\', "/");
    let rc = format!(
        r#"#include <winver.h>
1 ICON "{icon}"
VS_VERSION_INFO VERSIONINFO
FILEVERSION {numbers}
PRODUCTVERSION {numbers}
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "LumaFiles"
      VALUE "FileDescription", "LumaFiles"
      VALUE "ProductName", "LumaFiles"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
      VALUE "OriginalFilename", "LumaFiles.exe"
      VALUE "LegalCopyright", "MIT License"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let out =
        Path::new(&std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("prime-files.rc");
    std::fs::write(&out, rc).expect("writing the resource script");
    embed_resource::compile(&out, embed_resource::NONE)
        .manifest_optional()
        .expect("compiling the Windows resources");
}
