//! Shared foundations for the nohrs application: configuration, error types,
//! and telemetry, kept app-agnostic so they can be reused across binaries.

/// Configuration: UI layout constants and the user-facing `config.toml` system.
pub mod config;
/// Crate-wide error and `Result` types.
pub mod errors;
/// Interface language: `t!`/`tf!` and the translation table.
pub mod i18n;
/// Keyboard shortcuts: the commands, their default keys and `[keybindings]`.
pub mod shortcuts;
/// Logging and other telemetry setup.
pub mod telemetry;
/// The window classes Windows registers this program's windows under.
pub mod window_class;
