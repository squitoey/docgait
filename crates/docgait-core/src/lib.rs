//! docgait-core: libraries, sources, scanning and the index. No UI code lives here; the CLI and
//! the desktop app are thin layers on top.

pub mod app;
pub mod db;
pub mod exclude;
pub mod fsmeta;
pub mod kind;
pub mod library;
pub mod scan;
pub mod schema;
pub mod search;
pub mod watch;

pub use app::{App, LibraryInfo};
pub use library::{
    Cursor, FileEntry, Folder, IndexLevel, Library, Listing, Role, SortBy, Source, Status,
};
pub use scan::{ScanProgress, ScanSummary};
pub use search::{SearchHit, SearchResults};
