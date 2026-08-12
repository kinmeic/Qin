use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
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
    pub id: String,
    pub kind: String,
    pub title: String,
    pub source_uri: Option<String>,
    pub importance: f32,
    pub chunk_content: String,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct SequencedMessage {
    pub seq: i64,
    pub message: StoredMessage,
}

pub struct SummaryUpdate {
    pub content: String,
    pub through_seq: i64,
}

pub type EncodedChunk = (String, String, Vec<u8>, String, usize, usize);

pub struct KnowledgeInsert {
    pub item: KnowledgeRow,
    pub content_hash: String,
    pub chunks: Vec<EncodedChunk>,
}

pub struct StateStore {
    connection: Connection,
    path: PathBuf,
    pending_audits: Vec<PendingAudit>,
}

pub struct SessionLock {
    _file: fs::File,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self._file);
    }
}

#[derive(Clone)]
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
        let requested_path = resolver.database_path(config)?;
        let parent = requested_path
            .parent()
            .context("The database path has no parent directory")?;
        let parent_existed = parent.exists();
        fs::create_dir_all(parent)
            .with_context(|| format!("Unable to create data directory {}", parent.display()))?;
        if !parent_existed && resolver.scope() != ConfigScope::Explicit {
            set_directory_permissions(parent, resolver.scope())?;
        }
        let canonical_parent = parent.canonicalize()?;
        validate_database_directory(&canonical_parent)?;
        let path = canonical_parent.join(
            requested_path
                .file_name()
                .context("The database path has no file name")?,
        );
        prepare_database_file(&path)?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::default() | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .with_context(|| format!("Unable to open database {}", path.display()))?;
        connection.busy_timeout(Duration::from_millis(config.storage.busy_timeout_ms))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let journal = choose_journal(config, &path);
        let actual_journal: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if !actual_journal.eq_ignore_ascii_case(journal) {
            connection.pragma_update(None, "journal_mode", journal)?;
        }
        connection.pragma_update(
            None,
            "synchronous",
            if config.storage.write_profile == "durable" {
                "FULL"
            } else {
                "NORMAL"
            },
        )?;
        let mut store = Self {
            connection,
            path,
            pending_audits: Vec::new(),
        };
        store.migrate()?;
        store.secure_database_files()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lock_session(&self, session_id: &str) -> Result<SessionLock> {
        let identity = sha256(&format!("{}\0{session_id}", self.path.display()));
        let directory = std::env::temp_dir().join(format!("qin-locks-{}", effective_uid()));
        ensure_private_lock_directory(&directory)?;
        let path = directory.join(format!("{}.lock", &identity[..40]));
        let file = open_private_lock(&path)?;
        file.try_lock_exclusive().with_context(|| {
            format!(
                "Session {session_id} is in use by another qin process; lock file: {}",
                path.display()
            )
        })?;
        Ok(SessionLock { _file: file })
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
            CREATE INDEX IF NOT EXISTS idx_messages_session_role ON messages(session_id, role);
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
            CREATE INDEX IF NOT EXISTS idx_knowledge_items_kind ON knowledge_items(kind, enabled);
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
        let version: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(version),0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;
        if version < 2 {
            let transaction = self.connection.transaction()?;
            transaction.execute(
                "ALTER TABLE sessions ADD COLUMN compacted_through_seq INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
            transaction.execute("INSERT INTO schema_migrations(version) VALUES (2)", [])?;
            transaction.commit()?;
        }
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
        self.secure_database_files()?;
        Ok(id)
    }

    pub fn use_session(&mut self, id_or_prefix: &str) -> Result<String> {
        let id = self.resolve_session_id(id_or_prefix)?;
        self.connection.execute(
            "INSERT INTO app_state(key,value) VALUES ('current_session',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [&id],
        )?;
        Ok(id)
    }

    fn session_exists(&self, id: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?)
    }

    pub fn resolve_session_id(&self, id_or_prefix: &str) -> Result<String> {
        let value = id_or_prefix.trim();
        if value.is_empty() {
            bail!("The session identifier cannot be empty");
        }
        let mut statement = self.connection.prepare(
            "SELECT id FROM sessions WHERE id=?1 OR substr(id,1,length(?1))=?1 ORDER BY id LIMIT 2",
        )?;
        let matches = statement
            .query_map([value], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match matches.as_slice() {
            [] => bail!("Session does not exist: {value}"),
            [id] => Ok(id.clone()),
            _ => bail!("Session identifier is ambiguous: {value}"),
        }
    }

    pub fn delete_session(
        &mut self,
        id_or_prefix: &str,
        cwd: &Path,
    ) -> Result<(String, Option<String>)> {
        let id = self.resolve_session_id(id_or_prefix)?;
        let was_current = self.current_session()?.as_deref() == Some(id.as_str());
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM tool_executions WHERE session_id=?1", [&id])?;
        transaction.execute("DELETE FROM sessions WHERE id=?1", [&id])?;
        let mut new_current = None;
        if was_current {
            let replacement = Uuid::new_v4().to_string();
            let cwd = cwd.to_string_lossy();
            transaction.execute(
                "INSERT INTO sessions(id,title,initial_cwd,last_cwd) VALUES (?1,'New session',?2,?2)",
                params![replacement, cwd],
            )?;
            transaction.execute(
                "INSERT INTO app_state(key,value) VALUES ('current_session',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [&replacement],
            )?;
            new_current = Some(replacement);
        }
        transaction.commit()?;
        self.secure_database_files()?;
        Ok((id, new_current))
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

    pub fn load_context_messages(&self, session_id: &str) -> Result<Vec<SequencedMessage>> {
        let mut statement = self.connection.prepare(
            "SELECT seq,role,content,tool_calls,tool_call_id FROM messages WHERE session_id=?1 AND seq > COALESCE((SELECT compacted_through_seq FROM sessions WHERE id=?1),0) ORDER BY seq",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok(SequencedMessage {
                seq: row.get(0)?,
                message: StoredMessage {
                    role: row.get(1)?,
                    content: row.get(2)?,
                    tool_calls: row.get(3)?,
                    tool_call_id: row.get(4)?,
                },
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    #[cfg(test)]
    pub fn append_messages(
        &mut self,
        session_id: &str,
        messages: &[StoredMessage],
        cwd: &Path,
    ) -> Result<()> {
        self.append_messages_with_summary(session_id, messages, cwd, None)
    }

    pub fn append_messages_with_summary(
        &mut self,
        session_id: &str,
        messages: &[StoredMessage],
        cwd: &Path,
        summary: Option<&SummaryUpdate>,
    ) -> Result<()> {
        if messages.is_empty() && self.pending_audits.is_empty() && summary.is_none() {
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
        let audits = self.pending_audits.clone();
        for audit in &audits {
            transaction.execute(
                "INSERT INTO tool_executions(session_id,tool_call_id,name,args_redacted_json,result_text,status,risk,exit_code,duration_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![audit.session_id,audit.call_id,audit.name,audit.args,audit.result,audit.status,audit.risk,audit.exit_code,audit.duration_ms as i64],
            )?;
        }
        transaction.execute(
            "UPDATE sessions SET updated_at=CURRENT_TIMESTAMP,last_cwd=?2 WHERE id=?1",
            params![session_id, cwd.to_string_lossy()],
        )?;
        if let Some(summary) = summary {
            transaction.execute(
                "UPDATE sessions SET compacted_summary=?2,compacted_through_seq=MAX(compacted_through_seq,?3) WHERE id=?1",
                params![session_id, summary.content, summary.through_seq],
            )?;
        }
        transaction.commit()?;
        self.pending_audits.clear();
        self.secure_database_files()?;
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

    pub fn upsert_knowledge_batch(&mut self, inserts: &[KnowledgeInsert]) -> Result<usize> {
        if inserts.is_empty() {
            return Ok(0);
        }
        let transaction = self.connection.transaction()?;
        let mut added = 0;
        for insert in inserts {
            let existing: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM knowledge_items WHERE kind=?1 AND content_hash=?2)",
                params![insert.item.kind, insert.content_hash],
                |row| row.get(0),
            )?;
            if existing {
                continue;
            }
            transaction.execute(
                "INSERT INTO knowledge_items(id,kind,title,source_uri,content,content_hash,importance) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![insert.item.id,insert.item.kind,insert.item.title,insert.item.source_uri,insert.item.content,insert.content_hash,insert.item.importance],
            )?;
            for (index, (id, content, blob, encoding, dimensions, tokens)) in
                insert.chunks.iter().enumerate()
            {
                transaction.execute(
                    "INSERT INTO knowledge_chunks(id,item_id,chunk_no,content,content_hash,embedding_blob,vector_encoding,dimensions,token_count) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![id,insert.item.id,index as i64,content,sha256(content),blob,encoding,*dimensions as i64,*tokens as i64],
                )?;
            }
            added += 1;
        }
        transaction.commit()?;
        self.secure_database_files()?;
        Ok(added)
    }

    pub fn has_knowledge_hash(&self, kind: &str, hash: &str) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_items WHERE kind=?1 AND content_hash=?2)",
            params![kind, hash],
            |row| row.get(0),
        )?)
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
            "SELECT id,kind,title,source_uri,'' AS content,importance FROM knowledge_items WHERE enabled=1 ORDER BY updated_at DESC"
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

    pub fn visit_vector_rows(
        &self,
        kind: Option<&str>,
        mut visitor: impl FnMut(VectorRow),
    ) -> Result<()> {
        let mut statement = self.connection.prepare(
            "SELECT i.id,i.kind,i.title,i.source_uri,i.importance,c.content,c.embedding_blob,c.vector_encoding,c.dimensions FROM knowledge_items i JOIN knowledge_chunks c ON c.item_id=i.id WHERE i.enabled=1 AND (?1 IS NULL OR i.kind=?1)"
        )?;
        let rows = statement.query_map([kind], |row| {
            let blob: Vec<u8> = row.get(6)?;
            let encoding: String = row.get(7)?;
            let dimensions: usize = row.get::<_, i64>(8)? as usize;
            Ok(VectorRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                source_uri: row.get(3)?,
                importance: row.get(4)?,
                chunk_content: row.get(5)?,
                embedding: decode_vector(&blob, &encoding, dimensions),
            })
        })?;
        for row in rows {
            visitor(row?);
        }
        Ok(())
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        if !self.pending_audits.is_empty() {
            let transaction = self.connection.transaction()?;
            let audits = self.pending_audits.clone();
            for audit in &audits {
                transaction.execute(
                    "INSERT INTO tool_executions(session_id,tool_call_id,name,args_redacted_json,result_text,status,risk,exit_code,duration_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![audit.session_id,audit.call_id,audit.name,audit.args,audit.result,audit.status,audit.risk,audit.exit_code,audit.duration_ms as i64],
                )?;
            }
            transaction.commit()?;
            self.pending_audits.clear();
        }
        let journal: String = self
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if journal.eq_ignore_ascii_case("wal") {
            self.connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        self.connection.cache_flush()?;
        self.secure_database_files()?;
        Ok(())
    }

    fn secure_database_files(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [
                self.path.clone(),
                PathBuf::from(format!("{}-wal", self.path.display())),
                PathBuf::from(format!("{}-shm", self.path.display())),
            ] {
                match fs::symlink_metadata(&path) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() || !metadata.is_file() {
                            bail!(
                                "Refusing to change permissions on a non-regular database file: {}",
                                path.display()
                            );
                        }
                        if metadata.permissions().mode() & 0o777 != 0o600 {
                            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("Unable to inspect {}", path.display()));
                    }
                }
            }
        }
        Ok(())
    }
}

impl Drop for StateStore {
    fn drop(&mut self) {
        if !self.pending_audits.is_empty() {
            if let Ok(transaction) = self.connection.transaction() {
                for audit in std::mem::take(&mut self.pending_audits) {
                    let _ = transaction.execute("INSERT INTO tool_executions(session_id,tool_call_id,name,args_redacted_json,result_text,status,risk,exit_code,duration_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![audit.session_id,audit.call_id,audit.name,audit.args,audit.result,audit.status,audit.risk,audit.exit_code,audit.duration_ms as i64]);
                }
                let _ = transaction.commit();
            }
        }
        let _ = self.secure_database_files();
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

fn prepare_database_file(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "Refusing to open a database through a symbolic link: {}",
            path.display()
        );
    }
    if !path.exists() {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path) {
            Ok(file) => file.sync_all()?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Unable to create database {}", path.display()));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_database_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o022 != 0 {
        bail!(
            "The database directory must not be writable by group or other users: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_database_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn open_private_lock(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    options.open(path)
}

fn ensure_private_lock_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "The session-lock path is not a private directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn effective_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and does not modify memory.
        unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        0
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
        if blob.len() != dimensions.saturating_mul(2) {
            return Vec::new();
        }
        blob.chunks_exact(2)
            .take(dimensions)
            .map(|bytes| half::f16::from_le_bytes([bytes[0], bytes[1]]).to_f32())
            .collect()
    } else {
        if encoding != "f32" || blob.len() != dimensions.saturating_mul(4) {
            return Vec::new();
        }
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
    fn deletes_sessions_by_displayed_prefix_and_creates_a_new_active_session() {
        let (_dir, mut store) = store();
        let first = store.new_session(Path::new("/tmp"), Some("first")).unwrap();
        let second = store
            .new_session(Path::new("/tmp"), Some("second"))
            .unwrap();
        store
            .append_messages(
                &second,
                &[StoredMessage {
                    role: "user".into(),
                    content: Some("remove me".into()),
                    tool_calls: None,
                    tool_call_id: None,
                }],
                Path::new("/tmp"),
            )
            .unwrap();

        let (deleted, replacement) = store
            .delete_session(&second[..8], Path::new("/replacement"))
            .unwrap();
        assert_eq!(deleted, second);
        let replacement = replacement.unwrap();
        assert_ne!(replacement, first);
        assert_eq!(
            store.current_session().unwrap().as_deref(),
            Some(replacement.as_str())
        );
        assert!(store.load_messages(&second).unwrap().is_empty());
        let (deleted, new_current) = store
            .delete_session(&first[..8], Path::new("/tmp"))
            .unwrap();
        assert_eq!(deleted, first);
        assert_eq!(new_current, None);
        assert_eq!(store.current_session().unwrap(), Some(replacement));
        assert_eq!(store.list_sessions(10).unwrap().len(), 1);
        assert!(store.delete_session("missing", Path::new("/tmp")).is_err());
    }

    #[test]
    fn session_lock_is_exclusive_and_released() {
        let (_dir, store) = store();
        let first = store.lock_session("abc").unwrap();
        assert!(store.lock_session("abc").is_err());
        drop(first);
        assert!(store.lock_session("abc").is_ok());
    }

    #[test]
    fn summary_boundary_keeps_full_history_but_limits_context() {
        let (_dir, mut store) = store();
        let id = store
            .new_session(Path::new("/tmp"), Some("summary"))
            .unwrap();
        let messages = ["one", "two", "three"]
            .into_iter()
            .map(|content| StoredMessage {
                role: "user".into(),
                content: Some(content.into()),
                tool_calls: None,
                tool_call_id: None,
            })
            .collect::<Vec<_>>();
        store
            .append_messages_with_summary(
                &id,
                &messages,
                Path::new("/tmp"),
                Some(&SummaryUpdate {
                    content: "first two".into(),
                    through_seq: 2,
                }),
            )
            .unwrap();
        assert_eq!(store.load_messages(&id).unwrap().len(), 3);
        let context = store.load_context_messages(&id).unwrap();
        assert_eq!(context.len(), 1);
        assert_eq!(context[0].message.content.as_deref(), Some("three"));
    }

    #[test]
    fn rejects_database_symlinks() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target.db");
            fs::write(&target, []).unwrap();
            symlink(&target, dir.path().join("link.db")).unwrap();
            let resolver =
                ConfigPathResolver::new(Some(dir.path().join("config.toml")), false).unwrap();
            let mut config = Config::default();
            config.storage.database = "link.db".into();
            assert!(StateStore::open(&config, &resolver).is_err());
        }
    }
}
