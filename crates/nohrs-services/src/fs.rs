//! Directory listing with stable, case-insensitive ordering and cursor paging,
//! plus synchronous file mutation operations (see [`ops`]) and the trash
//! (see [`trash`]).

/// Synchronous directory listing.
pub mod listing;
/// Synchronous file mutation operations with cross-volume and conflict handling.
pub mod ops;
/// Directory sizes as a tree, and the treemap layout that draws them.
pub mod space;
/// Progress and cancellation for long copy, move and delete runs.
pub mod transfer;
/// Listing, restoring from, and emptying the trash.
pub mod trash;
