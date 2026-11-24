//! Every icon the code names must be in the asset bundle (memory plan §6.3).
//!
//! gpui-component turns `IconName::ChevronDown` into `icons/chevron-down.svg`, and
//! the app asks for its own icons by the same spelling. A file that is not in
//! `crates/nohrs-ui/assets/` is not a compile error: it is an ERROR logged every
//! time the element is drawn, which filled a 549 MB log on 2026-11-15. This test
//! walks the sources for both spellings and checks the bundle, so a missing icon
//! fails here instead.
//!
//! `clippy.toml` bans the synchronous `std::fs` helpers so that blocking IO does
//! not reach the GPUI foreground thread; a test binary has no such thread, and the
//! files it reads are the sources and the bundle.

#![allow(clippy::disallowed_methods, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The repository root: this crate is `<root>/crates/nohrs-ui`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the workspace root")
        .to_path_buf()
}

/// The name of a file, from a variant name: `ALargeSmall` → `a-large-small`,
/// `Redo2` → `redo-2`, the way gpui-component names the files beside `icon.rs`.
fn kebab(variant: &str) -> String {
    let chars: Vec<char> = variant.chars().collect();
    let mut name = String::new();
    for (index, &character) in chars.iter().enumerate() {
        let previous = index.checked_sub(1).map(|before| chars[before]);
        if character.is_uppercase() {
            let next = chars.get(index + 1).copied();
            let starts_a_word = match previous {
                Some(before) if before.is_lowercase() || before.is_ascii_digit() => true,
                // A run of capitals ends where the next lowercase begins: the last
                // capital of `ALarge` is where `Large` starts.
                Some(before) if before.is_uppercase() => next.is_some_and(char::is_lowercase),
                _ => false,
            };
            if starts_a_word && !name.is_empty() && !name.ends_with('-') {
                name.push('-');
            }
            name.push(character.to_ascii_lowercase());
        } else if character.is_ascii_digit() {
            if previous.is_some_and(|before| before.is_ascii_alphabetic()) && !name.ends_with('-') {
                name.push('-');
            }
            name.push(character);
        } else {
            name.push(character);
        }
    }
    name
}

/// The bundle path of one `IconName` variant. `GitHub` is the one variant
/// gpui-component spells differently from its own naming rule.
fn bundle_path(variant: &str) -> String {
    match variant {
        "GitHub" => "icons/github.svg".to_string(),
        other => format!("icons/{}.svg", kebab(other)),
    }
}

/// Every `.rs` file under `<root>/crates`, without the build directories.
fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    let mut pending = vec![root.join("crates")];
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "target") {
                    pending.push(path);
                }
                continue;
            }
            if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }
    sources
}

/// The `IconName::X` references and `"icons/….svg"` literals in `source`.
/// Comment lines are skipped: a name mentioned in prose is not drawn.
fn names_in(source: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    const VARIANT: &str = "IconName::";
    const LITERAL: &str = "\"icons/";
    let mut variants = BTreeSet::new();
    let mut literal_paths = BTreeSet::new();
    for line in source.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut rest = line;
        while let Some(at) = rest.find(VARIANT) {
            rest = &rest[at + VARIANT.len()..];
            let name: String = rest
                .chars()
                .take_while(|character| character.is_ascii_alphanumeric())
                .collect();
            let length = name.len();
            if !name.is_empty() {
                variants.insert(name);
            }
            rest = &rest[length..];
        }
        let mut rest = line;
        while let Some(at) = rest.find(LITERAL) {
            rest = &rest[at + 1..];
            let name: String = rest
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/')
                })
                .collect();
            let length = name.len();
            if name.ends_with(".svg") {
                literal_paths.insert(name);
            }
            rest = &rest[length..];
        }
    }
    (variants, literal_paths)
}

#[test]
fn every_named_icon_is_in_the_asset_bundle() {
    let root = workspace_root();
    let assets = root.join("crates/nohrs-ui/assets");
    let own_test = Path::new(file!())
        .file_name()
        .expect("the test's file name")
        .to_owned();

    let mut named: BTreeSet<String> = BTreeSet::new();
    let mut missing: BTreeSet<String> = BTreeSet::new();
    let mut sources = 0;
    for path in rust_sources(&root) {
        if path.file_name() == Some(own_test.as_os_str()) {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        sources += 1;
        let (variants, literal_paths) = names_in(&source);
        for variant in variants {
            let bundle = bundle_path(&variant);
            if !assets.join(&bundle).exists() {
                missing.insert(format!(
                    "{} (IconName::{variant} at {})",
                    bundle,
                    path.strip_prefix(&root).unwrap_or(&path).display()
                ));
            }
            named.insert(bundle);
        }
        for literal in literal_paths {
            if !assets.join(&literal).exists() {
                missing.insert(format!(
                    "{literal} (literal at {})",
                    path.strip_prefix(&root).unwrap_or(&path).display()
                ));
            }
            named.insert(literal);
        }
    }

    assert!(sources > 0, "no sources were scanned");
    assert!(
        missing.is_empty(),
        "icons used by the code but not in crates/nohrs-ui/assets:\n{}",
        missing.into_iter().collect::<Vec<_>>().join("\n")
    );
    // The bundle also holds icons no source names directly (`Icon::empty().path`
    // with a computed path), so only a sanity floor is checked here.
    assert!(
        named.len() > 50,
        "only {} icon names were found; the scan is probably broken",
        named.len()
    );
}
