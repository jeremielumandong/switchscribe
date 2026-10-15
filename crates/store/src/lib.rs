//! switchscribe's local store: where meetings live on disk, the SQLite index over them
//! (with FTS5 over transcript segments), and the settings file.
//!
//! Everything here is blocking I/O: the app calls it from background threads (or once at
//! startup), never from the UI thread.

pub mod db;
pub mod model;
pub mod paths;
pub mod settings;

pub use db::Store;
pub use model::{
    DateGroup, Meeting, MeetingStatus, NewMeeting, NewSegment, Person, SearchHit, Segment,
    group_by_date,
};
pub use paths::Paths;
pub use settings::{CaptureMode, CliKind, Settings};

/// Errors from the store.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The SQLite database failed.
    #[error("database: {0}")]
    Sql(#[from] rusqlite::Error),
    /// A file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file or folder involved.
        path: std::path::PathBuf,
        /// What went wrong.
        source: std::io::Error,
    },
    /// The settings file could not be written as JSON.
    #[error("settings: {0}")]
    Json(#[from] serde_json::Error),
    /// The database was written by a newer switchscribe.
    #[error("database schema {found} is newer than this build understands ({supported})")]
    NewerSchema {
        /// `user_version` in the file.
        found: i64,
        /// The newest version this build migrates to.
        supported: i64,
    },
}

/// Store result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn io_err(path: &std::path::Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}
