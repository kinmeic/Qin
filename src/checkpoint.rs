//! File checkpoints: pre-mutation snapshots taken by typed file tools so a
//! later `qin undo` can restore the previous state. Snapshot payloads live in
//! `<data dir>/checkpoints/<checkpoint id>/`; metadata lives in SQLite.
//! Checkpoints require the SQLite storage backend and are silently skipped
//! when `storage.enabled = false` or `checkpoints.enabled = false`.
//!
//! The `shell` tool is not covered: an arbitrary command's blast radius
//! cannot be known in advance, so it keeps relying on approvals and the
//! trash directory instead.

use std::cell::Cell;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::state::{CheckpointEntryRow, StateStore};
use crate::tools::ToolContext;

pub const KIND_OVERWRITE: &str = "overwrite";
pub const KIND_CREATE: &str = "create";
pub const KIND_MOVE: &str = "move";
pub const KIND_DELETE: &str = "delete";

/// Records pre-mutation snapshots for a single tool call. Rows and snapshot
/// files are written eagerly so a crash or failure mid-mutation never loses
/// the pre-mutation state: a recorder that is dropped without `commit`
/// (tool error) keeps its checkpoint, so `qin undo` can still repair a
/// partially applied mutation.
pub struct Recorder {
    id: String,
    directory: PathBuf,
    session_id: String,
    tool_call_id: String,
    tool: String,
    max_file_bytes: u64,
    keep: u32,
    next_seq: Cell<i64>,
    row_created: Cell<bool>,
}

impl Recorder {
    pub fn new(ctx: &ToolContext<'_>, tool: &str) -> Result<Option<Self>> {
        if ctx.dry_run || !ctx.config.checkpoints.enabled {
            return Ok(None);
        }
        let Some(root) = ctx.store.checkpoints_dir() else {
            return Ok(None);
        };
        let id = uuid::Uuid::new_v4().to_string();
        Ok(Some(Self {
            directory: root.join(&id),
            id,
            session_id: ctx.session_id.to_string(),
            tool_call_id: ctx.tool_call_id.to_string(),
            tool: tool.to_string(),
            max_file_bytes: ctx.config.checkpoints.max_file_bytes,
            keep: ctx.config.checkpoints.keep,
            next_seq: Cell::new(0),
            row_created: Cell::new(false),
        }))
    }

    fn ensure_row(&self, store: &StateStore) -> Result<()> {
        if !self.row_created.get() {
            store.insert_checkpoint(&self.id, &self.session_id, &self.tool_call_id, &self.tool)?;
            self.row_created.set(true);
        }
        Ok(())
    }

    fn insert(&self, store: &StateStore, mut entry: CheckpointEntryRow) -> Result<()> {
        self.ensure_row(store)?;
        entry.seq = self.next_seq.get();
        self.next_seq.set(entry.seq + 1);
        store.insert_checkpoint_entry(&self.id, &entry)
    }

    /// The tool created `path` (which did not exist before); undo removes it.
    pub fn created(&self, store: &StateStore, path: &Path) -> Result<()> {
        self.insert(
            store,
            CheckpointEntryRow {
                seq: 0,
                path: path.display().to_string(),
                kind: KIND_CREATE.into(),
                related_path: None,
                existed_before: false,
                snapshot_file: None,
                original_sha256: None,
            },
        )
    }

    /// The tool overwrote an existing file; undo restores the snapshot.
    pub fn overwrite(&self, store: &StateStore, path: &Path) -> Result<()> {
        let (snapshot_file, original_sha256) = self.snapshot(path)?;
        self.insert(
            store,
            CheckpointEntryRow {
                seq: 0,
                path: path.display().to_string(),
                kind: KIND_OVERWRITE.into(),
                related_path: None,
                existed_before: true,
                snapshot_file,
                original_sha256,
            },
        )
    }

    /// The tool moved `src` to `dst`; undo renames it back.
    pub fn moved(&self, store: &StateStore, src: &Path, dst: &Path) -> Result<()> {
        self.insert(
            store,
            CheckpointEntryRow {
                seq: 0,
                path: src.display().to_string(),
                kind: KIND_MOVE.into(),
                related_path: Some(dst.display().to_string()),
                existed_before: true,
                snapshot_file: None,
                original_sha256: None,
            },
        )
    }

    /// The tool deleted `path`. With `trash_dest` the item was moved to the
    /// trash directory and undo moves it back; otherwise a snapshot is taken
    /// first (regular files within the size limit only).
    pub fn deleted(
        &self,
        store: &StateStore,
        path: &Path,
        trash_dest: Option<&Path>,
    ) -> Result<()> {
        let (snapshot_file, original_sha256) = if trash_dest.is_some() {
            (None, None)
        } else {
            self.snapshot(path)?
        };
        self.insert(
            store,
            CheckpointEntryRow {
                seq: 0,
                path: path.display().to_string(),
                kind: KIND_DELETE.into(),
                related_path: trash_dest.map(|dest| dest.display().to_string()),
                existed_before: true,
                snapshot_file,
                original_sha256,
            },
        )
    }

    /// Copies a regular file into the checkpoint directory, returning the
    /// snapshot file name and SHA-256. Oversized files and directories are
    /// recorded without a snapshot (their content cannot be restored).
    fn snapshot(&self, path: &Path) -> Result<(Option<String>, Option<String>)> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.len() > self.max_file_bytes {
            return Ok((None, None));
        }
        fs::create_dir_all(&self.directory)
            .with_context(|| format!("Unable to create {}", self.directory.display()))?;
        crate::tools::set_private_directory(&self.directory)?;
        let mut source = crate::tools::open_read_no_follow(path)?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.directory)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 65_536];
        let mut total = 0_u64;
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if total > self.max_file_bytes {
                // The file grew past the limit while being read.
                return Ok((None, None));
            }
            hasher.update(&buffer[..read]);
            std::io::Write::write_all(&mut temp, &buffer[..read])?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        temp.as_file().sync_all()?;
        let name = self.next_seq.get().to_string();
        temp.persist(self.directory.join(&name))
            .map_err(|error| error.error)?;
        Ok((Some(name), Some(hex::encode(hasher.finalize()))))
    }

    pub fn commit(&self, store: &StateStore) -> Result<()> {
        if !self.row_created.get() {
            return Ok(());
        }
        for pruned in store.prune_checkpoints(self.keep)? {
            if let Some(root) = store.checkpoints_dir() {
                let directory = root.join(&pruned);
                if directory != self.directory && directory.exists() {
                    let _ = fs::remove_dir_all(&directory);
                }
            }
        }
        Ok(())
    }
}

/// A single planned undo step, in application order.
#[derive(Debug)]
pub struct UndoStep {
    pub description: String,
    action: UndoAction,
}

#[derive(Debug)]
enum UndoAction {
    RestoreSnapshot { path: PathBuf, snapshot: PathBuf },
    RemoveCreated { path: PathBuf },
    MoveBack { from: PathBuf, to: PathBuf },
    Unrecoverable { reason: String },
}

/// Builds the ordered undo steps for a checkpoint, newest entry first.
pub fn plan_undo(store: &StateStore, checkpoint_id: &str) -> Result<Vec<UndoStep>> {
    let entries = store.checkpoint_entries(checkpoint_id)?;
    if entries.is_empty() {
        bail!("The checkpoint contains no restorable entries");
    }
    let root = store
        .checkpoints_dir()
        .context("Checkpoints require the SQLite storage backend")?
        .join(checkpoint_id);
    let mut steps = Vec::new();
    for entry in entries.into_iter().rev() {
        let path = PathBuf::from(&entry.path);
        let step = match entry.kind.as_str() {
            KIND_OVERWRITE => match entry.snapshot_file {
                Some(file) => UndoStep {
                    description: format!("Restore the previous content of {}", path.display()),
                    action: UndoAction::RestoreSnapshot {
                        path,
                        snapshot: root.join(file),
                    },
                },
                None => UndoStep {
                    description: format!(
                        "Cannot restore {}: no snapshot was captured (oversized or not a regular file)",
                        path.display()
                    ),
                    action: UndoAction::Unrecoverable {
                        reason: "no snapshot".into(),
                    },
                },
            },
            KIND_CREATE => UndoStep {
                description: format!("Remove the created path {}", path.display()),
                action: UndoAction::RemoveCreated { path },
            },
            KIND_MOVE => {
                let destination = entry
                    .related_path
                    .as_deref()
                    .context("A move checkpoint entry is missing its destination")?;
                UndoStep {
                    description: format!("Move {} back to {}", destination, path.display()),
                    action: UndoAction::MoveBack {
                        from: PathBuf::from(destination),
                        to: path,
                    },
                }
            }
            KIND_DELETE => match (entry.related_path, entry.snapshot_file) {
                (Some(trash), _) => UndoStep {
                    description: format!(
                        "Move {} back from the trash to {}",
                        trash,
                        path.display()
                    ),
                    action: UndoAction::MoveBack {
                        from: PathBuf::from(trash),
                        to: path,
                    },
                },
                (None, Some(file)) => UndoStep {
                    description: format!("Restore the deleted file {}", path.display()),
                    action: UndoAction::RestoreSnapshot {
                        path,
                        snapshot: root.join(file),
                    },
                },
                (None, None) => UndoStep {
                    description: format!(
                        "Cannot restore {}: no snapshot was captured (directory, oversized, or non-regular file)",
                        path.display()
                    ),
                    action: UndoAction::Unrecoverable {
                        reason: "no snapshot".into(),
                    },
                },
            },
            other => bail!("Unknown checkpoint entry kind: {other}"),
        };
        steps.push(step);
    }
    Ok(steps)
}

/// Executes the undo plan. Returns one line per step describing the outcome.
pub fn execute_undo(store: &StateStore, checkpoint_id: &str) -> Result<Vec<String>> {
    let steps = plan_undo(store, checkpoint_id)?;
    let mut outcomes = Vec::new();
    for step in steps {
        match step.action {
            UndoAction::RestoreSnapshot { path, snapshot } => {
                reject_unsafe_target(&path)?;
                if !snapshot.is_file() {
                    bail!(
                        "The snapshot file is missing; cannot restore {}",
                        path.display()
                    );
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                crate::tools::atomic_copy(&snapshot, &path)?;
                outcomes.push(format!("Restored {}", path.display()));
            }
            UndoAction::RemoveCreated { path } => match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        fs::remove_file(&path)?;
                        outcomes.push(format!("Removed created file {}", path.display()));
                    } else {
                        bail!(
                            "Refusing to remove {}: it is a directory; remove it manually if intended",
                            path.display()
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    outcomes.push(format!("Already absent: {}", path.display()));
                }
                Err(error) => return Err(error.into()),
            },
            UndoAction::MoveBack { from, to } => {
                if !from.exists() && fs::symlink_metadata(&from).is_err() {
                    outcomes.push(format!("Skipped: {} no longer exists", from.display()));
                    continue;
                }
                if to.exists() || fs::symlink_metadata(&to).is_ok() {
                    bail!(
                        "Cannot move {} back: {} already exists",
                        from.display(),
                        to.display()
                    );
                }
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&from, &to).with_context(|| {
                    format!("Unable to move {} back to {}", from.display(), to.display())
                })?;
                outcomes.push(format!("Moved {} back to {}", from.display(), to.display()));
            }
            UndoAction::Unrecoverable { reason } => {
                outcomes.push(format!("Skipped ({reason}): {}", step.description));
            }
        }
    }
    store.mark_checkpoint_restored(checkpoint_id)?;
    Ok(outcomes)
}

fn reject_unsafe_target(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path == Path::new("/") {
        bail!("Refusing to restore to an unsafe path: {}", path.display());
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "Refusing to restore through a symbolic link: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ConfigPathResolver};

    fn store_with_config() -> (tempfile::TempDir, Config, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut config = Config::default();
        let mut models = std::collections::BTreeMap::new();
        models.insert(
            "primary".to_string(),
            crate::config::ModelConfig {
                model: "test-model".to_string(),
                ..crate::config::ModelConfig::default()
            },
        );
        config.models = models;
        config.storage.enabled = true;
        let resolver = ConfigPathResolver::new(Some(config_path), false).unwrap();
        let store = StateStore::open(&config, &resolver).unwrap();
        (dir, config, store)
    }

    fn recorder(store: &StateStore, config: &Config) -> Recorder {
        let id = uuid::Uuid::new_v4().to_string();
        Recorder {
            directory: store.checkpoints_dir().unwrap().join(&id),
            id,
            session_id: "test-session".into(),
            tool_call_id: "call-1".into(),
            tool: "write_file".into(),
            max_file_bytes: config.checkpoints.max_file_bytes,
            keep: config.checkpoints.keep,
            next_seq: Cell::new(0),
            row_created: Cell::new(false),
        }
    }

    #[test]
    fn overwrite_snapshot_and_undo_restores_content() {
        let (dir, config, store) = store_with_config();
        let target = dir.path().join("note.txt");
        std::fs::write(&target, "original").unwrap();
        let recorder = recorder(&store, &config);
        recorder.overwrite(&store, &target).unwrap();
        std::fs::write(&target, "modified").unwrap();
        recorder.commit(&store).unwrap();

        let id = store.latest_checkpoint_id().unwrap().unwrap();
        let outcomes = execute_undo(&store, &id).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        assert!(store.checkpoint_restored(&id).unwrap());
    }

    #[test]
    fn create_undo_removes_new_file() {
        let (dir, config, store) = store_with_config();
        let target = dir.path().join("fresh.txt");
        let recorder = recorder(&store, &config);
        recorder.created(&store, &target).unwrap();
        std::fs::write(&target, "new content").unwrap();
        recorder.commit(&store).unwrap();

        let id = store.latest_checkpoint_id().unwrap().unwrap();
        execute_undo(&store, &id).unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn move_undo_renames_back() {
        let (dir, config, store) = store_with_config();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("b.txt");
        std::fs::write(&src, "data").unwrap();
        let recorder = recorder(&store, &config);
        recorder.moved(&store, &src, &dst).unwrap();
        std::fs::rename(&src, &dst).unwrap();
        recorder.commit(&store).unwrap();

        let id = store.latest_checkpoint_id().unwrap().unwrap();
        execute_undo(&store, &id).unwrap();
        assert!(src.exists());
        assert!(!dst.exists());
    }

    #[test]
    fn delete_undo_restores_snapshot() {
        let (dir, config, store) = store_with_config();
        let target = dir.path().join("gone.txt");
        std::fs::write(&target, "precious").unwrap();
        let recorder = recorder(&store, &config);
        recorder.deleted(&store, &target, None).unwrap();
        std::fs::remove_file(&target).unwrap();
        recorder.commit(&store).unwrap();

        let id = store.latest_checkpoint_id().unwrap().unwrap();
        execute_undo(&store, &id).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "precious");
    }

    #[test]
    fn trash_delete_undo_moves_back() {
        let (dir, config, store) = store_with_config();
        let target = dir.path().join("trashme.txt");
        let trash = dir.path().join(".qin-trash").join("uuid-trashme.txt");
        std::fs::write(&target, "soft").unwrap();
        std::fs::create_dir_all(trash.parent().unwrap()).unwrap();
        std::fs::rename(&target, &trash).unwrap();
        let recorder = recorder(&store, &config);
        recorder.deleted(&store, &target, Some(&trash)).unwrap();
        recorder.commit(&store).unwrap();

        let id = store.latest_checkpoint_id().unwrap().unwrap();
        execute_undo(&store, &id).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "soft");
        assert!(!trash.exists());
    }

    #[test]
    fn oversized_files_record_without_snapshot() {
        let (dir, config, store) = store_with_config();
        let target = dir.path().join("big.bin");
        std::fs::write(&target, vec![b'x'; 2048]).unwrap();
        let recorder = Recorder {
            max_file_bytes: 16,
            ..recorder(&store, &config)
        };
        recorder.overwrite(&store, &target).unwrap();
        recorder.commit(&store).unwrap();

        let id = store.latest_checkpoint_id().unwrap().unwrap();
        let steps = plan_undo(&store, &id).unwrap();
        assert!(matches!(steps[0].action, UndoAction::Unrecoverable { .. }));
    }

    #[test]
    fn uncommitted_checkpoint_survives_for_undo() {
        // A tool error after snapshotting (recorder dropped without commit)
        // must not lose the pre-mutation state.
        let (dir, config, store) = store_with_config();
        let target = dir.path().join("note.txt");
        std::fs::write(&target, "original").unwrap();
        {
            let recorder = recorder(&store, &config);
            recorder.overwrite(&store, &target).unwrap();
        }
        std::fs::write(&target, "partial").unwrap();

        let id = store.latest_checkpoint_id().unwrap().unwrap();
        execute_undo(&store, &id).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }

    #[test]
    fn prune_removes_oldest_checkpoints() {
        let (dir, config, store) = store_with_config();
        for index in 0..3 {
            let target = dir.path().join(format!("f{index}.txt"));
            std::fs::write(&target, "v1").unwrap();
            let recorder = Recorder {
                keep: 2,
                ..recorder(&store, &config)
            };
            recorder.overwrite(&store, &target).unwrap();
            recorder.commit(&store).unwrap();
        }
        let remaining = store.list_checkpoints(10).unwrap();
        assert_eq!(remaining.len(), 2);
        assert_eq!(
            remaining[0].paths,
            vec![dir.path().join("f2.txt").display().to_string()]
        );
    }
}
