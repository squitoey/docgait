//! docgait-core: libraries, sources, scanning and the index. No UI code lives here; the CLI and
//! the desktop app are thin layers on top.

pub mod app;
pub mod cleanup;
pub mod conflicts;
pub mod db;
pub mod dupes;
pub mod exclude;
pub mod excluded;
pub mod fsmeta;
pub mod fsops;
pub mod hash;
pub mod import;
pub mod kind;
pub mod library;
pub mod obsidian;
pub mod organise;
pub mod scan;
pub mod schema;
pub mod search;
pub mod tags;
mod trash;
pub mod watch;

pub use app::{App, LibraryInfo};
pub use cleanup::{CleanupResult, Coverage, Match};
pub use conflicts::{ConflictVariant, ConflictView, MergeStart, Resolution};
pub use dupes::{
    DedupeItem, DedupeOp, DedupePlan, DedupeResult, DupeFilter, DupeGroup, DupeMember, DupePage,
    DupeScan,
};
pub use excluded::{ExcludedEntry, ExcludedSummary};
pub use import::{
    BatchInfo, ImportItem, ImportPlan, ImportProgress, ImportResult, Layout, UndoResult,
};
pub use library::{
    Cursor, FileEntry, Folder, IndexLevel, Library, Listing, Role, SortBy, Source, Status,
};
pub use organise::{OrganiseOp, OrganisePlan, OrganiseResult, PlannedStep};
pub use scan::{ScanProgress, ScanSummary};
pub use search::{SearchHit, SearchResults};
pub use tags::TagCount;
