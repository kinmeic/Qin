use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{Config, ConfigPathResolver, ConfigScope};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_cwd: String,
}

#[derive(Debug, Clone)]
pub struct KnowledgeRow {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub source_uri: Option<String>,
    pub content: String,
    pub importance: f32,
}

#[derive(Debug, Clone)]
pub struct VectorRow {
    pub item: KnowledgeRow,
    pub chunk_content: String,
    pub embedding: Vec<f32>,
}

pub type EncodedChunk = (String, String, Vec<u8>, String, usize, usize);

pub struct StateStore {
    connection: Connection,
    path: PathBuf,
    pending_audits: Vec<PendingAudit>,
}

pub struct SessionLock {
    path: PathBuf,
}
impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct PendingAudit {
    session_id: String,
    call_id: String,
    name: String,
    args: String,
    result: String,
    status: String,
    risk: String,
    exit_code: Option<i32>,
    duration_ms: u64,
}

impl StateStore {
    pub fn open(config: &Config, resolver: &ConfigPathResolver) -> Result<Self> {
        let path = resolver.database_path(config)?;
        let parent = path
            .parent()
            .context("The database path has no parent directory")?;
        let parent_existed = parent.exists();
        fs::create_dir_all(parent)
            .with_context(|| format!("Unable to create data directory {}", parent.display()))?;
        if !parent_existed && resolver.scope() != ConfigScope::Explicit {
            set_directory_permissions(parent, resolver.scope())?;
        }
        let connection = Connection::open(&path)
            .with_context(|| format!("Unable to open database {}", path.display()))?;
        connection.busy_timeout(Duration::from_millis(config.storage.busy_timeout_ms))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let journal = choose_journal(config, &path);
        connection.pragma_update(None, "journal_mode", journal)?;
        connection.pragma_update(
            None,
            "synchronous",
            if journal == "WAL" { "NORMAL" } else { "FULL" },
        )?;
        let mut store = Self {
            connection,
            path,
            pending_audits: Vec::new(),
        };
        store.migrate()?;
        store.secure_database_files();
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lock_session(&self, session_id: &str) -> Result<SessionLock> {
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("qin.db");
        let path = self
            .path
            .with_file_name(format!("{file_name}.session-{session_id}.lock"));
        let open = || OpenOptions::new().write(true).create_new(true).open(&path);
        let mut file = match open() {
            Ok(file) => file,
            Err(first_error) => {
                let stale = fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| {
                        text.lines()
                            .find_map(|line| line.strip_prefix("pid=")?.parse::<u32>().ok())
                    })
                    .is_some_and(|pid| !pid_alive(pid));
                if stale {
                    fs::remove_file(&path)?;
                    open()?
                } else {
                    return Err(first_error).with_context(|| {
                        format!(
                            "Session {session_id} is in use by another qin process; lock file: {}",
                            path.display()
                        )
                    });
                }
            }
        };
        writeln!(
            file,
            "pid={}\ncreated_at={}",
            std::process::id(),
            chrono::Utc::now().to_rfc3339()
        )?;
        Ok(SessionLock { path })
    }

    fn migrate(&mut self) -> Result<()> {
        self.connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                initial_cwd TEXT NOT NULL,
                last_cwd TEXT NOT NULL,
                compacted_summary TEXT
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                seq INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                tool_calls TEXT,
                tool_call_id TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(session_id, seq)
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session_seq ON messages(session_id, seq);
            CREATE TABLE IF NOT EXISTS app_state (
                key TEXT PRIMARY KEY, value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tool_executions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                tool_call_id TEXT NOT NULL,
                name TEXT NOT NULL,
                args_redacted_json TEXT NOT NULL,
                result_text TEXT,
                status TEXT NOT NULL,
                risk TEXT NOT NULL,
                exit_code INTEGER,
                duration_ms INTEGER,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE IF NOT EXISTS knowledge_items (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                source_uri TEXT,
                content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                importance REAL NOT NULL DEFAULT 0.5,
                enabled INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(kind, content_hash)
            );
            CREATE TABLE IF NOT EXISTS knowledge_chunks (
                id TEXT PRIMARY KEY,
                item_id TEXT NOT NULL REFERENCES knowledge_items(id) ON DELETE CASCADE,
                chunk_no INTEGER NOT NULL,
                content TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                embedding_blob BLOB NOT NULL,
                vector_encoding TEXT NOT NULL,
                dimensions INTEGER NOT NULL,
                token_count INTEGER NOT NULL,
                UNIQUE(item_id, chunk_no)
            );
            CREATE INDEX IF NOT EXISTS idx_knowledge_chunks_item ON knowledge_chunks(item_id);
            INSERT OR IGNORE INTO schema_migrations(version) VALUES (1);
            "#,
        )?;
        Ok(())
    }

    pub fn current_session(&self) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT value FROM app_state WHERE key='current_session'",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn ensure_current_session(&mut self, cwd: &Path) -> Result<String> {
        if let Some(id) = self.current_session()? {
            if self.session_exists(&id)? {
                return Ok(id);
            }
        }
        self.new_session(cwd, None)
    }

    pub fn new_session(&mut self, cwd: &Path, title: Option<&str>) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let title = title.unwrap_or("New session");
        let cwd = cwd.to_string_lossy();
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO sessions(id,title,initial_cwd,last_cwd) VALUES (?1,?2,?3,?3)",
            params![id, title, cwd],
        )?;
        transaction.execute(
            "INSERT INTO app_state(key,value) VALUES ('current_session',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![id],
        )?;
        transaction.commit()?;
        self.secure_database_files();
        Ok(id)
    }

    pub fn use_session(&mut self, id: &str) -> Result<()> {
        if !self.session_exists(id)? {
            bail!("Session does not exist: {id}");
        }
        self.connection.execute(
            "INSERT INTO app_state(key,value) VALUES ('current_session',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [id],
        )?;
        Ok(())
    }

    fn session_exists(&self, id: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?)
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionInfo>> {
        let mut statement = self.connection.prepare(
            "SELECT id,title,created_at,updated_at,last_cwd FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(SessionInfo {
                id: row.get(0)?,
                title: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
                last_cwd: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn load_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        let mut statement = self.connection.prepare(
            "SELECT role,content,tool_calls,tool_call_id FROM messages WHERE session_id=?1 ORDER BY seq",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok(StoredMessage {
                role: row.get(0)?,
                content: row.get(1)?,
                tool_calls: row.get(2)?,
                tool_call_id: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn append_messages(
        &mut self,
        session_id: &str,
        messages: &[StoredMessage],
        cwd: &Path,
    ) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let transaction = self.connection.transaction()?;
        let next: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM messages WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )?;
        for (index, message) in messages.iter().enumerate() {
            transaction.execute(
                "INSERT INTO messages(session_id,seq,role,content,tool_calls,tool_call_id) VALUES (?1,?2,?3,?4,?5,?6)",
                params![session_id, next + index as i64, message.role, message.content, message.tool_calls, message.tool_call_id],
            )?;
        }
        for audit in std::mem::take(&mut self.pending_audits) {
            transaction.execute(
                "INSERT INTO tool_executions(session_id,tool_call_id,name,args_redacted_json,result_text,status,risk,exit_code,duration_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![audit.session_id,audit.call_id,audit.name,audit.args,audit.result,audit.status,audit.risk,audit.exit_code,audit.duration_ms as i64],
            )?;
        }
        transaction.execute(
            "UPDATE sessions SET updated_at=CURRENT_TIMESTAMP,last_cwd=?2 WHERE id=?1",
            params![session_id, cwd.to_string_lossy()],
        )?;
        transaction.commit()?;
        self.secure_database_files();
        Ok(())
    }

    pub fn summary(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT compacted_summary FROM sessions WHERE id=?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn user_turn_count(&self, session_id: &str) -> Result<u32> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id=?1 AND role='user'",
            [session_id],
            |row| row.get::<_, i64>(0),
        )? as u32)
    }

    pub fn set_summary(&self, session_id: &str, summary: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE sessions SET compacted_summary=?2 WHERE id=?1",
            params![session_id, summary],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn audit_tool(
        &mut self,
        session_id: &str,
        call_id: &str,
        name: &str,
        args: &str,
        result: &str,
        status: &str,
        risk: &str,
        exit_code: Option<i32>,
        duration_ms: u64,
    ) -> Result<()> {
        self.pending_audits.push(PendingAudit {
            session_id: session_id.into(),
            call_id: call_id.into(),
            name: name.into(),
            args: args.into(),
            result: result.into(),
            status: status.into(),
            risk: risk.into(),
            exit_code,
            duration_ms,
        });
        Ok(())
    }

    pub fn upsert_knowledge(
        &mut self,
        item: &KnowledgeRow,
        hash: &str,
        chunks: &[EncodedChunk],
    ) -> Result<bool> {
        let existing: Option<String> = self
            .connection
            .query_row(
                "SELECT id FROM knowledge_items WHERE kind=?1 AND content_hash=?2",
                params![item.kind, hash],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_some() {
            return Ok(false);
        }
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO knowledge_items(id,kind,title,source_uri,content,content_hash,importance) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![item.id,item.kind,item.title,item.source_uri,item.content,hash,item.importance],
        )?;
        for (index, (id, content, blob, encoding, dimensions, tokens)) in chunks.iter().enumerate()
        {
            transaction.execute(
                "INSERT INTO knowledge_chunks(id,item_id,chunk_no,content,content_hash,embedding_blob,vector_encoding,dimensions,token_count) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![id,item.id,index as i64,content,sha256(content),blob,encoding,*dimensions as i64,*tokens as i64],
            )?;
        }
        transaction.commit()?;
        self.secure_database_files();
        Ok(true)
    }

    pub fn delete_knowledge(&self, id: &str) -> Result<bool> {
        Ok(self
            .connection
            .execute("DELETE FROM knowledge_items WHERE id=?1", [id])?
            > 0)
    }

    pub fn list_knowledge(&self, kind: Option<&str>) -> Result<Vec<KnowledgeRow>> {
        let sql = if kind.is_some() {
            "SELECT id,kind,title,source_uri,content,importance FROM knowledge_items WHERE enabled=1 AND kind=?1 ORDER BY updated_at DESC"
        } else {
            "SELECT id,kind,title,source_uri,content,importance FROM knowledge_items WHERE enabled=1 ORDER BY updated_at DESC"
        };
        let mut statement = self.connection.prepare(sql)?;
        let mapper = |row: &rusqlite::Row<'_>| {
            Ok(KnowledgeRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                source_uri: row.get(3)?,
                content: row.get(4)?,
                importance: row.get(5)?,
            })
        };
        let mut result = Vec::new();
        if let Some(kind) = kind {
            let rows = statement.query_map([kind], mapper)?;
            result.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        } else {
            let rows = statement.query_map([], mapper)?;
            result.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(result)
    }

    pub fn has_knowledge(&self) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_items WHERE enabled=1)",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn vector_rows(&self, kind: Option<&str>) -> Result<Vec<VectorRow>> {
        let mut statement = self.connection.prepare(
            "SELECT i.id,i.kind,i.title,i.source_uri,i.content,i.importance,c.content,c.embedding_blob,c.vector_encoding,c.dimensions FROM knowledge_items i JOIN knowledge_chunks c ON c.item_id=i.id WHERE i.enabled=1 AND (?1 IS NULL OR i.kind=?1)"
        )?;
        let rows = statement.query_map([kind], |row| {
            let blob: Vec<u8> = row.get(7)?;
            let encoding: String = row.get(8)?;
            let dimensions: usize = row.get::<_, i64>(9)? as usize;
            Ok(VectorRow {
                item: KnowledgeRow {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    title: row.get(2)?,
                    source_uri: row.get(3)?,
                    content: row.get(4)?,
                    importance: row.get(5)?,
                },
                chunk_content: row.get(6)?,
                embedding: decode_vector(&blob, &encoding, dimensions),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        if !self.pending_audits.is_empty() {
            let transaction = self.connection.transaction()?;
            for audit in std::mem::take(&mut self.pending_audits) {
                transaction.execute(
                    "INSERT INTO tool_executions(session_id,tool_call_id,name,args_redacted_json,result_text,status,risk,exit_code,duration_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![audit.session_id,audit.call_id,audit.name,audit.args,audit.result,audit.status,audit.risk,audit.exit_code,audit.duration_ms as i64],
                )?;
            }
            transaction.commit()?;
        }
        let _ = self
            .connection
            .pragma_update(None, "wal_checkpoint", "TRUNCATE");
        self.secure_database_files();
        Ok(())
    }

    fn secure_database_files(&self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [
                self.path.clone(),
                PathBuf::from(format!("{}-wal", self.path.display())),
                PathBuf::from(format!("{}-shm", self.path.display())),
            ] {
                if path.exists() {
                    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
                }
            }
        }
    }
}

impl Drop for StateStore {
    fn drop(&mut self) {
        for audit in std::mem::take(&mut self.pending_audits) {
            let _ = self.connection.execute("INSERT INTO tool_executions(session_id,tool_call_id,name,args_redacted_json,result_text,status,risk,exit_code,duration_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![audit.session_id,audit.call_id,audit.name,audit.args,audit.result,audit.status,audit.risk,audit.exit_code,audit.duration_ms as i64]);
        }
        self.secure_database_files();
    }
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path, scope: ConfigScope) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if scope == ConfigScope::User {
        0o700
    } else {
        0o755
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path, _scope: ConfigScope) -> Result<()> {
    Ok(())
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn choose_journal(config: &Config, path: &Path) -> &'static str {
    match config.storage.journal_mode.to_ascii_lowercase().as_str() {
        "wal" => "WAL",
        "persist" => "PERSIST",
        "delete" => "DELETE",
        _ if is_openwrt() || path.starts_with("/etc") => "PERSIST",
        _ => "WAL",
    }
}

fn is_openwrt() -> bool {
    Path::new("/etc/openwrt_release").exists() || Path::new("/etc/openwrt_version").exists()
}

pub fn sha256(content: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(content.as_bytes()))
}

fn decode_vector(blob: &[u8], encoding: &str, dimensions: usize) -> Vec<f32> {
    if encoding == "f16" {
        blob.chunks_exact(2)
            .take(dimensions)
            .map(|bytes| half::f16::from_le_bytes([bytes[0], bytes[1]]).to_f32())
            .collect()
    } else {
        blob.chunks_exact(4)
            .take(dimensions)
            .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigPathResolver;

    fn store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
        let mut config = Config::default();
        config.storage.database = "test.db".into();
        let store = StateStore::open(&config, &resolver).unwrap();
        (dir, store)
    }

    #[test]
    fn creates_and_restores_session() {
        let (_dir, mut store) = store();
        let id = store.new_session(Path::new("/tmp"), Some("test")).unwrap();
        store
            .append_messages(
                &id,
                &[StoredMessage {
                    role: "user".into(),
                    content: Some("hello".into()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                Path::new("/tmp"),
            )
            .unwrap();
        assert_eq!(
            store.current_session().unwrap().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(store.load_messages(&id).unwrap().len(), 1);
    }

    #[test]
    fn session_lock_is_exclusive_and_released() {
        let (_dir, store) = store();
        let first = store.lock_session("abc").unwrap();
        assert!(store.lock_session("abc").is_err());
        drop(first);
        assert!(store.lock_session("abc").is_ok());
    }
}
