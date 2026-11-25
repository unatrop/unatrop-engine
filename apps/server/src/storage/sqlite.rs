//! Fast WAL-mode SQLite session store for `uno-daemon`.

use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, params};
use uno_protocol::{ContentPart, Message, Role, redact_message_secrets};

#[derive(Clone)]
pub(crate) struct StoredDaemonSession {
    pub id: String,
    pub title: String,
    pub model: String,
    pub workspace: String,
    pub messages: Vec<Message>,
    pub updated_at: u64,
}

#[derive(Clone)]
pub(crate) struct StoredDaemonSessionMeta {
    pub id: String,
    pub title: String,
    pub model: String,
    pub workspace: String,
    pub message_count: usize,
    pub updated_at: u64,
}

enum StoreCmd {
    Upsert(StoredDaemonSession),
    Delete(String),
}

pub(crate) struct SqliteSessionStore {
    db_path: PathBuf,
    tx: Option<mpsc::Sender<StoreCmd>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SqliteSessionStore {
    pub(crate) fn open(sessions_dir: &Path) -> Option<Self> {
        if std::fs::create_dir_all(sessions_dir).is_err() {
            return None;
        }
        let db_path = sessions_dir.join("daemon.db");
        let conn = open_connection(&db_path).ok()?;
        drop(conn);

        let (tx, rx) = mpsc::channel::<StoreCmd>();
        let writer_path = db_path.clone();
        let handle = std::thread::Builder::new()
            .name("daemon-sqlite-writer".to_string())
            .spawn(move || {
                let Ok(mut conn) = open_connection(&writer_path) else {
                    return;
                };
                while let Ok(cmd) = rx.recv() {
                    let mut batch = vec![cmd];
                    while let Ok(next) = rx.try_recv() {
                        batch.push(next);
                    }
                    let Ok(tx_sql) = conn.transaction() else {
                        continue;
                    };
                    for item in batch {
                        match item {
                            StoreCmd::Upsert(session) => {
                                let _ = upsert_row(&tx_sql, &session);
                            }
                            StoreCmd::Delete(id) => {
                                let _ = tx_sql
                                    .execute("DELETE FROM sessions WHERE id = ?1", params![id]);
                            }
                        }
                    }
                    let _ = tx_sql.commit();
                }
            })
            .ok();

        Some(Self {
            db_path,
            tx: Some(tx),
            handle,
        })
    }

    pub(crate) fn save_async(&self, session: StoredDaemonSession) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(StoreCmd::Upsert(session));
        }
    }

    pub(crate) fn save_sync(&self, session: &StoredDaemonSession) {
        if let Ok(conn) = open_connection(&self.db_path) {
            let _ = upsert_row(&conn, session);
        }
    }

    pub(crate) fn delete_sync(&self, id: &str) -> bool {
        if let Some(tx) = &self.tx {
            let _ = tx.send(StoreCmd::Delete(id.to_owned()));
        }
        let Ok(conn) = open_connection(&self.db_path) else {
            return false;
        };
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .is_ok_and(|rows| rows > 0)
    }

    pub(crate) fn load_history(&self, id: &str) -> Option<Vec<Message>> {
        let conn = open_connection(&self.db_path).ok()?;
        let json: String = conn
            .query_row(
                "SELECT messages_json FROM sessions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .ok()?;
        serde_json::from_str::<Vec<Message>>(&json).ok()
    }

    pub(crate) fn list_sessions(&self) -> Vec<StoredDaemonSessionMeta> {
        let Ok(conn) = open_connection(&self.db_path) else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, title, model, workspace, message_count, updated_at
             FROM sessions ORDER BY updated_at DESC, id ASC",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let title: String = row.get(1)?;
            let model: String = row.get(2)?;
            let workspace: String = row.get(3)?;
            let count: i64 = row.get(4)?;
            let updated_at: i64 = row.get(5)?;
            Ok(StoredDaemonSessionMeta {
                id,
                title,
                model,
                workspace,
                message_count: usize::try_from(count).unwrap_or(0),
                updated_at: u64::try_from(updated_at).unwrap_or(0),
            })
        }) else {
            return Vec::new();
        };
        rows.filter_map(Result::ok).collect()
    }
}

impl Drop for SqliteSessionStore {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(crate) fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn derive_session_title(messages: &[Message]) -> String {
    let Some(first_user) = messages.iter().find(|m| m.role == Role::User) else {
        return String::new();
    };
    let text = first_user
        .content
        .0
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.chars().count() > 64 {
        let mut s: String = trimmed.chars().take(63).collect();
        s.push('…');
        s
    } else {
        trimmed
    }
}

fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA busy_timeout = 5000;
         CREATE TABLE IF NOT EXISTS sessions (
             id TEXT PRIMARY KEY,
             title TEXT NOT NULL DEFAULT '',
             model TEXT NOT NULL,
             workspace TEXT NOT NULL,
             messages_json TEXT NOT NULL,
             message_count INTEGER NOT NULL DEFAULT 0,
             updated_at INTEGER NOT NULL
         );",
    )?;
    Ok(conn)
}

fn upsert_row(conn: &Connection, session: &StoredDaemonSession) -> rusqlite::Result<()> {
    let redacted = redact_message_secrets(&session.messages);
    let json = serde_json::to_string(&redacted).unwrap_or_else(|_| "[]".to_owned());
    let visible_count = session
        .messages
        .iter()
        .filter(|m| matches!(m.role, Role::User | Role::Assistant))
        .count();
    conn.execute(
        "INSERT INTO sessions (id, title, model, workspace, messages_json, message_count, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
             title = excluded.title,
             model = excluded.model,
             workspace = excluded.workspace,
             messages_json = excluded.messages_json,
             message_count = excluded.message_count,
             updated_at = excluded.updated_at",
        params![
            session.id,
            session.title,
            session.model,
            session.workspace,
            json,
            visible_count as i64,
            session.updated_at as i64
        ],
    )?;
    Ok(())
}
