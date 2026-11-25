//! Persistence backends for daemon sessions and chat history.

pub(crate) mod sqlite;

pub(crate) use sqlite::{
    SqliteSessionStore, StoredDaemonSession, derive_session_title, unix_timestamp_secs,
};
