//! Application services: filesystem listing, full-text/regex search, and
//! optional syntax highlighting.

/// Synchronous filesystem listing services.
pub mod fs;
/// Starting what a quick-launch item points at, and its path variables.
pub mod launch;
/// Full-text and regex file search services.
pub mod search;
/// Syntax highlighting backed by `syntect`, mapped to GPUI colors.
#[cfg(feature = "gui")]
pub mod syntax;
