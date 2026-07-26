use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::journal::{
    DurableRelocation, JournalEntry, JournalRecord, JournalSequence, RecoveredSession,
    SessionStore, StoreError, StoreFuture, replay_after, replay_session,
};
use super::types::{ParentSessionRef, SessionEntryName, SessionId, SessionLocator};

const SCHEMA_VERSION: u32 = 1;
const HEADER_BYTES: usize = 18;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairReport {
    pub backup_path: PathBuf,
    pub removed_bytes: u64,
}

pub struct LocalSessionStore {
    project_root: PathBuf,
    sessions_root: PathBuf,
    writers: Mutex<HashMap<SessionId, Arc<Mutex<SessionWriter>>>>,
}

impl LocalSessionStore {
    pub fn discover_project_root(start: impl AsRef<Path>) -> Result<PathBuf, StoreError> {
        let start = start.as_ref().canonicalize().map_err(io_error)?;
        if !start.is_dir() {
            return Err(StoreError::Backend(format!(
                "project discovery start is not a directory: {}",
                start.display()
            )));
        }
        Ok(start
            .ancestors()
            .find(|ancestor| ancestor.join(".rua").is_dir())
            .unwrap_or(&start)
            .to_path_buf())
    }

    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let project_root = project_root.into();
        Self {
            sessions_root: project_root.join(".rua").join("sessions"),
            project_root,
            writers: Mutex::new(HashMap::new()),
        }
    }

    pub fn sessions_root(&self) -> &Path {
        &self.sessions_root
    }

    pub fn default_locator(&self, session_id: &SessionId) -> Result<SessionLocator, StoreError> {
        Ok(SessionLocator {
            project_root: self.project_root.clone(),
            entry_name: SessionEntryName::try_new(session_id.as_str())
                .map_err(|error| StoreError::InvalidSessionId(error.to_string()))?,
        })
    }

    pub fn locate(&self, session_id: &SessionId) -> Result<SessionLocator, StoreError> {
        let directory = find_session_directory(&self.sessions_root, session_id)?;
        locator_from_directory(&directory)
    }

    pub async fn relocate_session(
        &self,
        session_id: &SessionId,
        mut to: SessionLocator,
    ) -> Result<SessionLocator, StoreError> {
        to.project_root = to.project_root.canonicalize().map_err(io_error)?;
        if !to.project_root.is_dir() {
            return Err(StoreError::Backend(format!(
                "relocation project root is not a directory: {}",
                to.project_root.display()
            )));
        }
        let from = self.locate(session_id)?;
        if from == to {
            return Ok(to);
        }
        if from.project_root == to.project_root {
            return self.rename_session(session_id, to.entry_name).await;
        }
        self.move_session_across_projects(session_id, from, to)
            .await
    }

    async fn move_session_across_projects(
        &self,
        session_id: &SessionId,
        from: SessionLocator,
        to: SessionLocator,
    ) -> Result<SessionLocator, StoreError> {
        let destination_root = to.project_root.join(".rua").join("sessions");
        std::fs::create_dir_all(&destination_root).map_err(io_error)?;
        set_private_directory_permissions(&destination_root)?;
        let target = destination_root.join(to.entry_name.as_str());
        if target.exists() {
            return Err(StoreError::Backend(format!(
                "session entry already exists: {}",
                target.display()
            )));
        }
        let source = from
            .project_root
            .join(".rua")
            .join("sessions")
            .join(from.entry_name.as_str());
        let source_manifest_path = source.join("manifest.json");
        let source_manifest = read_manifest(&source_manifest_path, session_id)?;
        let generation = source_manifest
            .locator_generation
            .checked_add(1)
            .ok_or_else(|| StoreError::InvalidManifest("locator generation overflow".to_owned()))?;
        self.append(
            session_id,
            JournalRecord::SessionRelocationPrepared {
                from: from.clone(),
                to: to.clone(),
                locator_generation: generation,
            },
        )
        .await?;
        let relocation = RelocationPending {
            from: from.clone(),
            to: to.clone(),
            locator_generation: generation,
        };
        write_relocation_claim(&self.sessions_root, session_id, &relocation)?;
        let writer = self.take_writer(session_id).await?.ok_or_else(|| {
            StoreError::Backend("relocation lost the active session writer".to_owned())
        })?;
        writer.journal.sync_all().map_err(io_error)?;
        let staging = destination_root.join(format!(
            ".relocating-{}-{}-{}",
            session_id,
            std::process::id(),
            generation
        ));
        if staging.exists() {
            return Err(StoreError::Backend(format!(
                "relocation staging path already exists: {}",
                staging.display()
            )));
        }
        copy_relocation_staging(&source, &staging, session_id, &relocation)?;
        validate_session_artifact(&staging, session_id, true)?;
        publish_directory(&staging, &target)?;

        let source_manifest_path = source.join("manifest.json");
        let mut source_manifest = read_manifest(&source_manifest_path, session_id)?;
        source_manifest.redirect = Some(RelocationRedirect {
            to: to.clone(),
            locator_generation: generation,
        });
        source_manifest.relocation_pending = None;
        source_manifest.locator_generation = generation;
        write_json_atomic(&source_manifest_path, &source_manifest)?;

        let target_manifest_path = target.join("manifest.json");
        let mut target_manifest = read_manifest(&target_manifest_path, session_id)?;
        target_manifest.relocation_pending = None;
        target_manifest.locator_generation = generation;
        write_json_atomic(&target_manifest_path, &target_manifest)?;
        drop(writer);

        self.append(
            session_id,
            JournalRecord::SessionRelocationCommitted {
                to: to.clone(),
                locator_generation: generation,
            },
        )
        .await?;
        remove_relocation_claim(&self.sessions_root, session_id)?;
        Ok(to)
    }

    pub async fn recover_relocation(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionLocator, StoreError> {
        self.close_writer(session_id).await?;
        let local_entry = find_local_session_entry(&self.sessions_root, session_id)?;
        let tip = relocation_tip(&local_entry, session_id, None, &mut HashSet::new())?;
        let tip_manifest = read_manifest(&tip.join("manifest.json"), session_id)?;
        let pending = if let Some(pending) = tip_manifest.relocation_pending.clone() {
            pending
        } else {
            let Some(pending) = pending_relocation_from_journal(&tip, session_id)? else {
                remove_relocation_claim(&self.sessions_root, session_id)?;
                return locator_from_directory(&tip);
            };
            pending
        };
        let origin = pending
            .from
            .project_root
            .join(".rua")
            .join("sessions")
            .join(pending.from.entry_name.as_str());
        let target = pending
            .to
            .project_root
            .join(".rua")
            .join("sessions")
            .join(pending.to.entry_name.as_str());
        if target.is_dir() && pending_relocation_from_journal(&target, session_id)?.is_none() {
            remove_relocation_claim(
                &pending.from.project_root.join(".rua").join("sessions"),
                session_id,
            )?;
            return Ok(pending.to);
        }
        let _origin_lock = if pending.from.project_root != pending.to.project_root {
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .open(origin.join("lock"))
                .map_err(io_error)?;
            fs2::FileExt::try_lock_exclusive(&lock).map_err(|error| StoreError::SessionLocked {
                session_id: session_id.clone(),
                message: error.to_string(),
            })?;
            Some(lock)
        } else {
            None
        };
        if pending.from.project_root == pending.to.project_root {
            if !target.exists() {
                if !origin.exists() {
                    return Err(StoreError::InvalidManifest(
                        "relocation has neither source nor target artifact".to_owned(),
                    ));
                }
                publish_directory(&origin, &target)?;
            } else if origin.exists() && origin != target {
                return Err(StoreError::InvalidManifest(
                    "both rename source and target artifacts exist".to_owned(),
                ));
            }
            let target_manifest_path = target.join("manifest.json");
            let mut target_manifest = read_manifest(&target_manifest_path, session_id)?;
            target_manifest.relocation_pending = None;
            target_manifest.locator_generation = pending.locator_generation;
            write_json_atomic(&target_manifest_path, &target_manifest)?;
            self.append(
                session_id,
                JournalRecord::SessionRelocationCommitted {
                    to: pending.to.clone(),
                    locator_generation: pending.locator_generation,
                },
            )
            .await?;
            remove_relocation_claim(
                &pending.from.project_root.join(".rua").join("sessions"),
                session_id,
            )?;
            return Ok(pending.to);
        }
        if !origin.is_dir() {
            return Err(StoreError::InvalidManifest(format!(
                "relocation source artifact is missing: {}",
                origin.display()
            )));
        }
        if !target.exists() {
            let destination_root = target.parent().ok_or_else(|| {
                StoreError::InvalidManifest("relocation target has no sessions parent".to_owned())
            })?;
            let mut published = false;
            if let Some(staging) =
                find_pending_relocation_artifact(destination_root, session_id, &pending)?
            {
                if validate_session_artifact(&staging, session_id, true).is_ok() {
                    publish_directory(&staging, &target)?;
                    published = true;
                } else {
                    quarantine_relocation_staging(&staging, &pending.to.project_root)?;
                }
            }
            if !published {
                let staging = destination_root.join(format!(
                    ".relocating-recovery-{}-{}",
                    session_id, pending.locator_generation
                ));
                copy_relocation_staging(&origin, &staging, session_id, &pending)?;
                validate_session_artifact(&staging, session_id, true)?;
                publish_directory(&staging, &target)?;
            }
        }
        let mut target_manifest = read_manifest(&target.join("manifest.json"), session_id)?;
        if let Some(target_pending) = &target_manifest.relocation_pending {
            if target_pending.locator_generation != pending.locator_generation
                || target_pending.to != pending.to
            {
                return Err(StoreError::InvalidManifest(
                    "relocation target generation does not match source".to_owned(),
                ));
            }
        } else if target_manifest.redirect.is_some() {
            return Err(StoreError::InvalidManifest(
                "relocation target is itself a redirect".to_owned(),
            ));
        }
        let origin_manifest_path = origin.join("manifest.json");
        let mut origin_manifest = read_manifest(&origin_manifest_path, session_id)?;
        if origin_manifest.redirect.is_none() {
            origin_manifest.redirect = Some(RelocationRedirect {
                to: pending.to.clone(),
                locator_generation: pending.locator_generation,
            });
        }
        origin_manifest.locator_generation = pending.locator_generation;
        write_json_atomic(&origin_manifest_path, &origin_manifest)?;
        target_manifest.relocation_pending = None;
        target_manifest.locator_generation = pending.locator_generation;
        write_json_atomic(&target.join("manifest.json"), &target_manifest)?;
        self.append(
            session_id,
            JournalRecord::SessionRelocationCommitted {
                to: pending.to.clone(),
                locator_generation: pending.locator_generation,
            },
        )
        .await?;
        remove_relocation_claim(
            &pending.from.project_root.join(".rua").join("sessions"),
            session_id,
        )?;
        Ok(pending.to)
    }

    async fn take_writer(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionWriter>, StoreError> {
        let writer = self.writers.lock().await.remove(session_id);
        let Some(writer) = writer else {
            return Ok(None);
        };
        if Arc::strong_count(&writer) != 1 {
            self.writers.lock().await.insert(session_id.clone(), writer);
            return Err(StoreError::SessionLocked {
                session_id: session_id.clone(),
                message: "session writer is active".to_owned(),
            });
        }
        let mutex = Arc::try_unwrap(writer).map_err(|_| StoreError::SessionLocked {
            session_id: session_id.clone(),
            message: "session writer ownership changed during relocation".to_owned(),
        })?;
        Ok(Some(mutex.into_inner()))
    }

    pub async fn rename_session(
        &self,
        session_id: &SessionId,
        new_name: SessionEntryName,
    ) -> Result<SessionLocator, StoreError> {
        let from = self.locate(session_id)?;
        let to = SessionLocator {
            project_root: from.project_root.clone(),
            entry_name: new_name,
        };
        if from == to {
            return Ok(to);
        }
        let source = find_session_directory(&self.sessions_root, session_id)?;
        let target = self.sessions_root.join(to.entry_name.as_str());
        if target.exists() {
            return Err(StoreError::Backend(format!(
                "session entry already exists: {}",
                target.display()
            )));
        }
        let source_manifest_path = source.join("manifest.json");
        let mut manifest = read_manifest(&source_manifest_path, session_id)?;
        let generation = manifest
            .locator_generation
            .checked_add(1)
            .ok_or_else(|| StoreError::InvalidManifest("locator generation overflow".to_owned()))?;
        self.append(
            session_id,
            JournalRecord::SessionRelocationPrepared {
                from: from.clone(),
                to: to.clone(),
                locator_generation: generation,
            },
        )
        .await?;
        let pending = RelocationPending {
            from: from.clone(),
            to: to.clone(),
            locator_generation: generation,
        };
        write_relocation_claim(&self.sessions_root, session_id, &pending)?;
        manifest.relocation_pending = Some(pending);
        write_json_atomic(&source_manifest_path, &manifest)?;
        self.close_writer(session_id).await?;
        publish_directory(&source, &target)?;
        let target_manifest_path = target.join("manifest.json");
        let mut manifest = read_manifest(&target_manifest_path, session_id)?;
        manifest.relocation_pending = None;
        manifest.locator_generation = generation;
        write_json_atomic(&target_manifest_path, &manifest)?;
        self.append(
            session_id,
            JournalRecord::SessionRelocationCommitted {
                to: to.clone(),
                locator_generation: generation,
            },
        )
        .await?;
        remove_relocation_claim(&self.sessions_root, session_id)?;
        Ok(to)
    }

    async fn close_writer(&self, session_id: &SessionId) -> Result<(), StoreError> {
        let Some(writer) = self.take_writer(session_id).await? else {
            return Ok(());
        };
        writer.journal.sync_all().map_err(io_error)?;
        Ok(())
    }

    /// Return only direct, valid session directory names. Listing never opens a
    /// session and therefore cannot repair, lock, or otherwise mutate it.
    pub fn list_session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
        if !self.sessions_root.exists() {
            return Ok(Vec::new());
        }
        let mut sessions = std::fs::read_dir(&self.sessions_root)
            .map_err(io_error)?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(|kind| kind.is_dir())
                    .map(|_| entry)
            })
            .filter_map(|entry| {
                let bytes = std::fs::read(entry.path().join("manifest.json")).ok()?;
                let manifest: Manifest = serde_json::from_slice(&bytes).ok()?;
                (manifest.schema_version == SCHEMA_VERSION
                    && manifest.redirect.is_none()
                    && manifest.relocation_pending.is_none())
                .then_some(manifest.session_id)
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        Ok(sessions)
    }

    pub async fn validate_session(
        &self,
        session_id: &SessionId,
    ) -> Result<RecoveredSession, StoreError> {
        let session_dir = find_session_directory(&self.sessions_root, session_id)?;
        if !session_dir.is_dir() {
            return Err(StoreError::SessionNotFound(session_id.clone()));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(session_dir.join("lock"))
            .map_err(io_error)?;
        fs2::FileExt::try_lock_shared(&lock).map_err(|error| StoreError::SessionLocked {
            session_id: session_id.clone(),
            message: error.to_string(),
        })?;
        let manifest = validate_manifest(&session_dir.join("manifest.json"), session_id)?;
        let snapshot = load_snapshot(&session_dir, &manifest, session_id)?;
        let mut journal = OpenOptions::new()
            .read(true)
            .open(session_dir.join("journal.log"))
            .map_err(io_error)?;
        let (entries, valid_bytes, had_incomplete_tail) = read_wal(&mut journal)?;
        if had_incomplete_tail {
            let total_bytes = journal.metadata().map_err(io_error)?.len();
            return Err(StoreError::IncompleteTail {
                valid_bytes,
                total_bytes,
            });
        }
        recover_from_parts(session_id, snapshot, &entries)
    }

    pub fn export_raw(
        &self,
        session_id: &SessionId,
        destination: impl AsRef<Path>,
    ) -> Result<PathBuf, StoreError> {
        let source = find_session_directory(&self.sessions_root, session_id)?;
        if !source.is_dir() {
            return Err(StoreError::SessionNotFound(session_id.clone()));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(source.join("lock"))
            .map_err(io_error)?;
        fs2::FileExt::try_lock_shared(&lock).map_err(|error| StoreError::SessionLocked {
            session_id: session_id.clone(),
            message: error.to_string(),
        })?;

        std::fs::create_dir_all(destination.as_ref()).map_err(io_error)?;
        let target = destination.as_ref().join(session_id.as_str());
        std::fs::create_dir(&target).map_err(|error| {
            StoreError::Backend(format!(
                "failed to create export directory {}: {error}",
                target.display()
            ))
        })?;
        set_private_directory_permissions(&target)?;
        for name in ["manifest.json", "snapshot.json", "journal.log"] {
            let source_file = source.join(name);
            if source_file.is_file() {
                std::fs::copy(&source_file, target.join(name)).map_err(io_error)?;
            }
        }
        Ok(target)
    }

    pub fn repair_incomplete_tail(
        &self,
        session_id: &SessionId,
    ) -> Result<RepairReport, StoreError> {
        let session_dir = find_session_directory(&self.sessions_root, session_id)?;
        if !session_dir.is_dir() {
            return Err(StoreError::SessionNotFound(session_id.clone()));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(session_dir.join("lock"))
            .map_err(io_error)?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|error| StoreError::SessionLocked {
            session_id: session_id.clone(),
            message: error.to_string(),
        })?;

        let manifest = validate_manifest(&session_dir.join("manifest.json"), session_id)?;
        let snapshot = load_snapshot(&session_dir, &manifest, session_id)?;
        let journal_path = session_dir.join("journal.log");
        let mut journal = OpenOptions::new()
            .read(true)
            .open(&journal_path)
            .map_err(io_error)?;
        let total_bytes = journal.metadata().map_err(io_error)?.len();
        let (entries, valid_bytes, had_incomplete_tail) = read_wal(&mut journal)?;
        if !had_incomplete_tail {
            return Err(StoreError::Backend(
                "journal has no safely repairable incomplete tail".to_owned(),
            ));
        }
        recover_from_parts(session_id, snapshot, &entries)?;

        let nonce = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let backup_path = session_dir.join(format!("journal.log.pre-repair-{nonce}"));
        let mut backup = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup_path)
            .map_err(io_error)?;
        set_private_file_permissions(&backup)?;
        journal.seek(SeekFrom::Start(0)).map_err(io_error)?;
        std::io::copy(&mut journal, &mut backup).map_err(io_error)?;
        backup.sync_all().map_err(io_error)?;
        drop(backup);

        let repaired_path = session_dir.join(format!("journal.log.repair-{nonce}"));
        let mut repaired = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&repaired_path)
            .map_err(io_error)?;
        set_private_file_permissions(&repaired)?;
        journal.seek(SeekFrom::Start(0)).map_err(io_error)?;
        std::io::copy(
            &mut std::io::Read::by_ref(&mut journal).take(valid_bytes),
            &mut repaired,
        )
        .map_err(io_error)?;
        repaired.sync_all().map_err(io_error)?;
        drop(repaired);
        drop(journal);
        atomic_replace(&repaired_path, &journal_path)?;

        Ok(RepairReport {
            backup_path,
            removed_bytes: total_bytes - valid_bytes,
        })
    }

    async fn writer(
        &self,
        session_id: &SessionId,
        create: bool,
    ) -> Result<Arc<Mutex<SessionWriter>>, StoreError> {
        if let Some(writer) = self.writers.lock().await.get(session_id).cloned() {
            return Ok(writer);
        }

        let writer = Arc::new(Mutex::new(SessionWriter::open(
            &self.sessions_root,
            session_id,
            create,
        )?));
        let mut writers = self.writers.lock().await;
        Ok(writers
            .entry(session_id.clone())
            .or_insert_with(|| writer.clone())
            .clone())
    }
}

impl SessionStore for LocalSessionStore {
    fn append<'a>(
        &'a self,
        session_id: &'a SessionId,
        record: JournalRecord,
    ) -> StoreFuture<'a, JournalSequence> {
        Box::pin(async move {
            let writer = self.writer(session_id, true).await?;
            writer.lock().await.append(record)
        })
    }

    fn load<'a>(&'a self, session_id: &'a SessionId) -> StoreFuture<'a, RecoveredSession> {
        Box::pin(async move {
            let writer = self.writer(session_id, false).await?;
            writer.lock().await.recover()
        })
    }

    fn checkpoint<'a>(&'a self, session_id: &'a SessionId) -> StoreFuture<'a, JournalSequence> {
        Box::pin(async move {
            let writer = self.writer(session_id, false).await?;
            writer.lock().await.checkpoint()
        })
    }

    fn locator<'a>(&'a self, session_id: &'a SessionId) -> StoreFuture<'a, Option<SessionLocator>> {
        Box::pin(async move { self.locate(session_id).map(Some) })
    }

    fn relocate<'a>(
        &'a self,
        session_id: &'a SessionId,
        target: SessionLocator,
    ) -> StoreFuture<'a, SessionLocator> {
        Box::pin(async move { self.relocate_session(session_id, target).await })
    }

    fn complete_relocation<'a>(
        &'a self,
        session_id: &'a SessionId,
        pending: DurableRelocation,
    ) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.append(
                session_id,
                JournalRecord::SessionRelocationCommitted {
                    to: pending.to,
                    locator_generation: pending.locator_generation,
                },
            )
            .await?;
            remove_relocation_claim(
                &pending.from.project_root.join(".rua").join("sessions"),
                session_id,
            )?;
            Ok(())
        })
    }
}

struct SessionWriter {
    session_id: SessionId,
    session_dir: PathBuf,
    _lock: File,
    journal: File,
    entries: Vec<JournalEntry>,
    snapshot: Option<RecoveredSession>,
}

impl SessionWriter {
    fn open(
        sessions_root: &Path,
        session_id: &SessionId,
        create: bool,
    ) -> Result<Self, StoreError> {
        let session_dir = if create {
            match find_session_directory(sessions_root, session_id) {
                Ok(existing) => existing,
                Err(StoreError::SessionNotFound(_)) => {
                    session_directory(sessions_root, session_id)?
                }
                Err(error) => return Err(error),
            }
        } else {
            find_session_directory(sessions_root, session_id)?
        };
        if create {
            std::fs::create_dir_all(&session_dir).map_err(io_error)?;
        } else if !session_dir.is_dir() {
            return Err(StoreError::SessionNotFound(session_id.clone()));
        }
        set_private_directory_permissions(&session_dir)?;

        let lock_path = session_dir.join("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(&lock_path)
            .map_err(io_error)?;
        set_private_file_permissions(&lock)?;
        fs2::FileExt::try_lock_exclusive(&lock).map_err(|error| StoreError::SessionLocked {
            session_id: session_id.clone(),
            message: error.to_string(),
        })?;

        let manifest_path = session_dir.join("manifest.json");
        let manifest = if manifest_path.exists() {
            validate_manifest(&manifest_path, session_id)?
        } else if create {
            let manifest = Manifest::new(session_id.clone());
            write_json_atomic(&manifest_path, &manifest)?;
            manifest
        } else {
            return Err(StoreError::InvalidManifest(
                "manifest.json is missing".to_owned(),
            ));
        };
        set_private_path_permissions(&manifest_path)?;

        let journal_path = session_dir.join("journal.log");
        let mut journal = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(&journal_path)
            .map_err(io_error)?;
        set_private_file_permissions(&journal)?;
        let (entries, valid_bytes, had_incomplete_tail) = read_wal(&mut journal)?;
        if had_incomplete_tail {
            journal.set_len(valid_bytes).map_err(io_error)?;
            journal.sync_data().map_err(io_error)?;
        }
        let snapshot = load_snapshot(&session_dir, &manifest, session_id)?;
        if snapshot.is_some() || !entries.is_empty() {
            recover_from_parts(session_id, snapshot.clone(), &entries)?;
        }
        journal.seek(SeekFrom::End(0)).map_err(io_error)?;

        Ok(Self {
            session_id: session_id.clone(),
            session_dir,
            _lock: lock,
            journal,
            entries,
            snapshot,
        })
    }

    fn append(&mut self, record: JournalRecord) -> Result<JournalSequence, StoreError> {
        let sequence = JournalSequence(
            u64::try_from(self.entries.len())
                .map_err(|_| StoreError::SequenceOverflow)?
                .checked_add(1)
                .ok_or(StoreError::SequenceOverflow)?,
        );
        let entry = JournalEntry {
            schema_version: SCHEMA_VERSION,
            session_id: self.session_id.clone(),
            sequence,
            record,
        };
        let frame = encode_frame(&entry)?;
        self.journal.write_all(&frame).map_err(io_error)?;
        self.journal.sync_data().map_err(io_error)?;
        self.entries.push(entry);
        Ok(sequence)
    }

    fn recover(&self) -> Result<RecoveredSession, StoreError> {
        recover_from_parts(&self.session_id, self.snapshot.clone(), &self.entries)
    }

    fn checkpoint(&mut self) -> Result<JournalSequence, StoreError> {
        let recovered = self.recover()?;
        let snapshot_sequence = recovered.last_sequence;
        let snapshot = SnapshotFile {
            schema_version: SCHEMA_VERSION,
            session_id: self.session_id.clone(),
            last_sequence: snapshot_sequence,
            state: recovered.clone(),
        };
        write_json_atomic(&self.session_dir.join("snapshot.json"), &snapshot)?;
        let marker_sequence = self.append(JournalRecord::SnapshotCreated {
            last_sequence: snapshot_sequence,
        })?;

        let manifest_path = self.session_dir.join("manifest.json");
        let mut manifest = validate_manifest(&manifest_path, &self.session_id)?;
        manifest.snapshot_sequence = Some(snapshot_sequence);
        write_json_atomic(&manifest_path, &manifest)?;
        self.snapshot = Some(recovered);
        Ok(marker_sequence)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    schema_version: u32,
    session_id: SessionId,
    created_unix_ms: u128,
    snapshot_sequence: Option<JournalSequence>,
    #[serde(default)]
    redirect: Option<RelocationRedirect>,
    #[serde(default)]
    relocation_pending: Option<RelocationPending>,
    #[serde(default)]
    parent_session: Option<ParentSessionRef>,
    #[serde(default)]
    locator_generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelocationRedirect {
    to: SessionLocator,
    locator_generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelocationPending {
    from: SessionLocator,
    to: SessionLocator,
    locator_generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelocationClaim {
    schema_version: u32,
    session_id: SessionId,
    pending: RelocationPending,
}

impl Manifest {
    fn new(session_id: SessionId) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            session_id,
            created_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            snapshot_sequence: None,
            redirect: None,
            relocation_pending: None,
            parent_session: None,
            locator_generation: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotFile {
    schema_version: u32,
    session_id: SessionId,
    last_sequence: JournalSequence,
    state: RecoveredSession,
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(json_error)?;
    let temp_path = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut temp = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .map_err(io_error)?;
    set_private_file_permissions(&temp)?;
    temp.write_all(&bytes).map_err(io_error)?;
    temp.sync_all().map_err(io_error)?;
    drop(temp);
    atomic_replace(&temp_path, path)?;
    Ok(())
}

fn validate_manifest(path: &Path, session_id: &SessionId) -> Result<Manifest, StoreError> {
    let manifest = read_manifest(path, session_id)?;
    if let Some(redirect) = &manifest.redirect {
        return Err(StoreError::InvalidManifest(format!(
            "session moved to {}/{} (generation {})",
            redirect.to.project_root.display(),
            redirect.to.entry_name,
            redirect.locator_generation
        )));
    }
    if manifest.relocation_pending.is_some() {
        return Err(StoreError::InvalidManifest(
            "session relocation is pending recovery".to_owned(),
        ));
    }
    Ok(manifest)
}

fn read_manifest(path: &Path, session_id: &SessionId) -> Result<Manifest, StoreError> {
    let bytes = std::fs::read(path).map_err(io_error)?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(json_error)?;
    if manifest.schema_version != SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(manifest.schema_version));
    }
    if manifest.session_id != *session_id {
        return Err(StoreError::SessionMismatch {
            expected: session_id.clone(),
            actual: manifest.session_id,
        });
    }
    validate_manifest_metadata(&manifest)?;
    Ok(manifest)
}

fn validate_manifest_metadata(manifest: &Manifest) -> Result<(), StoreError> {
    if manifest.redirect.is_some() && manifest.relocation_pending.is_some() {
        return Err(StoreError::InvalidManifest(
            "manifest cannot be both a relocation redirect and pending relocation".to_owned(),
        ));
    }
    if let Some(redirect) = &manifest.redirect
        && redirect.locator_generation != manifest.locator_generation
    {
        return Err(StoreError::InvalidManifest(
            "redirect generation does not match manifest locator generation".to_owned(),
        ));
    }
    if let Some(pending) = &manifest.relocation_pending {
        let expected = manifest
            .locator_generation
            .checked_add(1)
            .ok_or_else(|| StoreError::InvalidManifest("locator generation overflow".to_owned()))?;
        if pending.locator_generation != expected {
            return Err(StoreError::InvalidManifest(
                "pending relocation generation does not advance the manifest".to_owned(),
            ));
        }
    }
    Ok(())
}

fn validate_session_artifact(
    session_dir: &Path,
    session_id: &SessionId,
    allow_pending_relocation: bool,
) -> Result<(), StoreError> {
    let manifest = read_manifest(&session_dir.join("manifest.json"), session_id)?;
    if manifest.redirect.is_some()
        || (!allow_pending_relocation && manifest.relocation_pending.is_some())
    {
        return Err(StoreError::InvalidManifest(
            "session artifact is not an active relocation owner".to_owned(),
        ));
    }
    let snapshot = load_snapshot(session_dir, &manifest, session_id)?;
    let mut journal = OpenOptions::new()
        .read(true)
        .open(session_dir.join("journal.log"))
        .map_err(io_error)?;
    let (entries, valid_bytes, had_incomplete_tail) = read_wal(&mut journal)?;
    if had_incomplete_tail {
        return Err(StoreError::IncompleteTail {
            valid_bytes,
            total_bytes: journal.metadata().map_err(io_error)?.len(),
        });
    }
    recover_from_parts(session_id, snapshot, &entries)?;
    Ok(())
}

fn copy_session_directory(source: &Path, destination: &Path) -> Result<(), StoreError> {
    std::fs::create_dir(destination).map_err(io_error)?;
    set_private_directory_permissions(destination)?;
    for item in std::fs::read_dir(source).map_err(io_error)? {
        let item = item.map_err(io_error)?;
        let target = destination.join(item.file_name());
        if item.file_type().map_err(io_error)?.is_dir() {
            copy_session_directory(&item.path(), &target)?;
        } else {
            std::fs::copy(item.path(), &target).map_err(io_error)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&target)
                .map_err(io_error)?;
            set_private_file_permissions(&file)?;
            file.sync_all().map_err(io_error)?;
        }
    }
    Ok(())
}

fn copy_relocation_staging(
    source: &Path,
    destination: &Path,
    session_id: &SessionId,
    pending: &RelocationPending,
) -> Result<(), StoreError> {
    std::fs::create_dir(destination).map_err(io_error)?;
    set_private_directory_permissions(destination)?;
    let mut manifest = read_manifest(&source.join("manifest.json"), session_id)?;
    manifest.redirect = None;
    manifest.relocation_pending = Some(pending.clone());
    write_json_atomic(&destination.join("manifest.json"), &manifest)?;
    for item in std::fs::read_dir(source).map_err(io_error)? {
        let item = item.map_err(io_error)?;
        if item.file_name() == "manifest.json" {
            continue;
        }
        let target = destination.join(item.file_name());
        if item.file_type().map_err(io_error)?.is_dir() {
            copy_session_directory(&item.path(), &target)?;
        } else {
            std::fs::copy(item.path(), &target).map_err(io_error)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&target)
                .map_err(io_error)?;
            set_private_file_permissions(&file)?;
            file.sync_all().map_err(io_error)?;
        }
    }
    Ok(())
}

fn load_snapshot(
    session_dir: &Path,
    manifest: &Manifest,
    session_id: &SessionId,
) -> Result<Option<RecoveredSession>, StoreError> {
    let Some(expected_sequence) = manifest.snapshot_sequence else {
        return Ok(None);
    };
    let bytes = std::fs::read(session_dir.join("snapshot.json")).map_err(io_error)?;
    let snapshot: SnapshotFile = serde_json::from_slice(&bytes).map_err(json_error)?;
    if snapshot.schema_version != SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(snapshot.schema_version));
    }
    if snapshot.session_id != *session_id || snapshot.state.session_id != *session_id {
        return Err(StoreError::SessionMismatch {
            expected: session_id.clone(),
            actual: snapshot.session_id,
        });
    }
    if snapshot.last_sequence != expected_sequence
        || snapshot.state.last_sequence != expected_sequence
    {
        return Err(StoreError::InvalidManifest(
            "snapshot sequence does not match manifest".to_owned(),
        ));
    }
    Ok(Some(snapshot.state))
}

fn recover_from_parts(
    session_id: &SessionId,
    snapshot: Option<RecoveredSession>,
    entries: &[JournalEntry],
) -> Result<RecoveredSession, StoreError> {
    if let Some(snapshot) = snapshot {
        let suffix = entries
            .iter()
            .position(|entry| entry.sequence.0 > snapshot.last_sequence.0)
            .map(|index| &entries[index..])
            .unwrap_or_default();
        replay_after(snapshot, suffix)
    } else {
        replay_session(session_id, entries)
    }
}

#[cfg(not(windows))]
fn publish_directory(source: &Path, target: &Path) -> Result<(), StoreError> {
    std::fs::rename(source, target).map_err(io_error)?;
    if let Some(parent) = target.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(io_error)?;
    }
    Ok(())
}

#[cfg(windows)]
fn publish_directory(source: &Path, target: &Path) -> Result<(), StoreError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if result == 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, target: &Path) -> Result<(), StoreError> {
    std::fs::rename(source, target).map_err(io_error)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, target: &Path) -> Result<(), StoreError> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn encode_frame(entry: &JournalEntry) -> Result<Vec<u8>, StoreError> {
    let payload = serde_json::to_vec(entry).map_err(json_error)?;
    let length = u32::try_from(payload.len()).map_err(|_| StoreError::FrameTooLarge)?;
    let checksum = crc32fast::hash(&payload);
    let mut frame = format!("{length:08x} {checksum:08x} ").into_bytes();
    frame.extend_from_slice(&payload);
    frame.push(b'\n');
    Ok(frame)
}

fn read_wal(file: &mut File) -> Result<(Vec<JournalEntry>, u64, bool), StoreError> {
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(io_error)?;
    let mut entries = Vec::new();
    let mut offset = 0usize;

    while offset < bytes.len() {
        let Some(relative_newline) = bytes[offset..].iter().position(|byte| *byte == b'\n') else {
            return Ok((entries, offset as u64, true));
        };
        let line_end = offset + relative_newline;
        let line = &bytes[offset..line_end];
        if line.len() < HEADER_BYTES {
            return Err(StoreError::CorruptFrame {
                offset: offset as u64,
                message: "frame header is truncated".to_owned(),
            });
        }
        if line[8] != b' ' || line[17] != b' ' {
            return Err(StoreError::CorruptFrame {
                offset: offset as u64,
                message: "invalid frame header separators".to_owned(),
            });
        }
        let length = parse_hex_u32(&line[..8], offset)? as usize;
        let checksum = parse_hex_u32(&line[9..17], offset)?;
        let payload = &line[HEADER_BYTES..];
        if payload.len() != length {
            return Err(StoreError::CorruptFrame {
                offset: offset as u64,
                message: format!(
                    "payload length mismatch: expected {length}, got {}",
                    payload.len()
                ),
            });
        }
        let actual_checksum = crc32fast::hash(payload);
        if actual_checksum != checksum {
            return Err(StoreError::CorruptFrame {
                offset: offset as u64,
                message: format!(
                    "checksum mismatch: expected {checksum:08x}, got {actual_checksum:08x}"
                ),
            });
        }
        let entry: JournalEntry = serde_json::from_slice(payload).map_err(json_error)?;
        entries.push(entry);
        offset = line_end + 1;
    }

    Ok((entries, offset as u64, false))
}

fn parse_hex_u32(bytes: &[u8], offset: usize) -> Result<u32, StoreError> {
    let text = std::str::from_utf8(bytes).map_err(|error| StoreError::CorruptFrame {
        offset: offset as u64,
        message: error.to_string(),
    })?;
    u32::from_str_radix(text, 16).map_err(|error| StoreError::CorruptFrame {
        offset: offset as u64,
        message: error.to_string(),
    })
}

fn io_error(error: std::io::Error) -> StoreError {
    StoreError::Backend(error.to_string())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> Result<(), StoreError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(io_error)
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &File) -> Result<(), StoreError> {
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(io_error)
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &File) -> Result<(), StoreError> {
    Ok(())
}

fn set_private_path_permissions(path: &Path) -> Result<(), StoreError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(io_error)?;
    set_private_file_permissions(&file)
}

fn session_directory(sessions_root: &Path, session_id: &SessionId) -> Result<PathBuf, StoreError> {
    let mut components = Path::new(session_id.as_str()).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) if !name.is_empty() => Ok(sessions_root.join(name)),
        _ => Err(StoreError::InvalidSessionId(session_id.as_str().to_owned())),
    }
}

fn relocation_claim_path(
    sessions_root: &Path,
    session_id: &SessionId,
) -> Result<PathBuf, StoreError> {
    session_directory(sessions_root, session_id)?;
    Ok(sessions_root.join(format!(".relocation-{}.json", session_id.as_str())))
}

fn write_relocation_claim(
    sessions_root: &Path,
    session_id: &SessionId,
    pending: &RelocationPending,
) -> Result<(), StoreError> {
    let path = relocation_claim_path(sessions_root, session_id)?;
    if path.exists() {
        return Err(StoreError::InvalidManifest(format!(
            "session relocation claim already exists: {}",
            path.display()
        )));
    }
    write_json_atomic(
        &path,
        &RelocationClaim {
            schema_version: SCHEMA_VERSION,
            session_id: session_id.clone(),
            pending: pending.clone(),
        },
    )
}

fn read_relocation_claim(
    path: &Path,
    session_id: &SessionId,
) -> Result<RelocationClaim, StoreError> {
    let bytes = std::fs::read(path).map_err(io_error)?;
    let claim: RelocationClaim = serde_json::from_slice(&bytes).map_err(json_error)?;
    if claim.schema_version != SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchema(claim.schema_version));
    }
    if claim.session_id != *session_id {
        return Err(StoreError::SessionMismatch {
            expected: session_id.clone(),
            actual: claim.session_id,
        });
    }
    Ok(claim)
}

fn remove_relocation_claim(sessions_root: &Path, session_id: &SessionId) -> Result<(), StoreError> {
    let path = relocation_claim_path(sessions_root, session_id)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

fn locator_from_directory(directory: &Path) -> Result<SessionLocator, StoreError> {
    let canonical = directory.canonicalize().map_err(io_error)?;
    let entry_name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| StoreError::InvalidSessionId(canonical.display().to_string()))?;
    let sessions = canonical.parent().ok_or_else(|| {
        StoreError::InvalidManifest("session entry has no sessions parent".to_owned())
    })?;
    let rua = sessions.parent().ok_or_else(|| {
        StoreError::InvalidManifest("sessions directory has no .rua parent".to_owned())
    })?;
    let project_root = rua.parent().ok_or_else(|| {
        StoreError::InvalidManifest(".rua directory has no project parent".to_owned())
    })?;
    Ok(SessionLocator {
        project_root: project_root.to_path_buf(),
        entry_name: SessionEntryName::try_new(entry_name)
            .map_err(|error| StoreError::InvalidSessionId(error.to_string()))?,
    })
}

fn find_session_directory(
    sessions_root: &Path,
    session_id: &SessionId,
) -> Result<PathBuf, StoreError> {
    let claim_path = relocation_claim_path(sessions_root, session_id)?;
    if claim_path.exists() {
        let claim = read_relocation_claim(&claim_path, session_id)?;
        let target = claim
            .pending
            .to
            .project_root
            .join(".rua")
            .join("sessions")
            .join(claim.pending.to.entry_name.as_str());
        if let Some(target) = resolve_session_candidate(&target, session_id)? {
            return Ok(target);
        }
        return Err(StoreError::InvalidManifest(format!(
            "session relocation is in progress for {session_id}"
        )));
    }
    let default = session_directory(sessions_root, session_id)?;
    if default.is_dir()
        && let Some(resolved) = resolve_session_candidate(&default, session_id)?
    {
        return Ok(resolved);
    }
    if !sessions_root.is_dir() {
        return Err(StoreError::SessionNotFound(session_id.clone()));
    }
    let mut found = None;
    for entry in std::fs::read_dir(sessions_root).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        if !entry.file_type().map_err(io_error)?.is_dir() {
            continue;
        }
        let Some(resolved) = resolve_session_candidate(&entry.path(), session_id)? else {
            continue;
        };
        if found.is_some() {
            return Err(StoreError::InvalidManifest(format!(
                "multiple session entries claim id {session_id}"
            )));
        }
        found = Some(resolved);
    }
    found.ok_or_else(|| StoreError::SessionNotFound(session_id.clone()))
}

fn find_local_session_entry(
    sessions_root: &Path,
    session_id: &SessionId,
) -> Result<PathBuf, StoreError> {
    if !sessions_root.is_dir() {
        return Err(StoreError::SessionNotFound(session_id.clone()));
    }
    let mut found = None;
    for entry in std::fs::read_dir(sessions_root).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        if !entry.file_type().map_err(io_error)?.is_dir() {
            continue;
        }
        let Ok(manifest) = read_manifest(&entry.path().join("manifest.json"), session_id) else {
            continue;
        };
        if manifest.session_id != *session_id {
            continue;
        }
        if found.is_some() {
            return Err(StoreError::InvalidManifest(format!(
                "multiple local session entries claim id {session_id}"
            )));
        }
        found = Some(entry.path());
    }
    found.ok_or_else(|| StoreError::SessionNotFound(session_id.clone()))
}

fn pending_relocation_from_journal(
    session_dir: &Path,
    _session_id: &SessionId,
) -> Result<Option<RelocationPending>, StoreError> {
    let mut journal = OpenOptions::new()
        .read(true)
        .open(session_dir.join("journal.log"))
        .map_err(io_error)?;
    let (entries, valid_bytes, had_incomplete_tail) = read_wal(&mut journal)?;
    if had_incomplete_tail {
        return Err(StoreError::IncompleteTail {
            valid_bytes,
            total_bytes: journal.metadata().map_err(io_error)?.len(),
        });
    }
    let mut pending = None;
    for entry in entries {
        match entry.record {
            JournalRecord::SessionRelocationPrepared {
                from,
                to,
                locator_generation,
            } => {
                pending = Some(RelocationPending {
                    from,
                    to,
                    locator_generation,
                });
            }
            JournalRecord::SessionRelocationCommitted {
                locator_generation, ..
            } if pending
                .as_ref()
                .is_some_and(|item| item.locator_generation == locator_generation) =>
            {
                pending = None
            }
            _ => {}
        }
    }
    Ok(pending)
}

fn find_pending_relocation_artifact(
    sessions_root: &Path,
    session_id: &SessionId,
    pending: &RelocationPending,
) -> Result<Option<PathBuf>, StoreError> {
    if !sessions_root.is_dir() {
        return Ok(None);
    }
    for entry in std::fs::read_dir(sessions_root).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        if !entry.file_type().map_err(io_error)?.is_dir() {
            continue;
        }
        let Ok(manifest) = read_manifest(&entry.path().join("manifest.json"), session_id) else {
            continue;
        };
        if manifest
            .relocation_pending
            .as_ref()
            .is_some_and(|candidate| {
                candidate.locator_generation == pending.locator_generation
                    && candidate.from == pending.from
                    && candidate.to == pending.to
            })
        {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

fn quarantine_relocation_staging(
    staging: &Path,
    project_root: &Path,
) -> Result<PathBuf, StoreError> {
    let backups = project_root.join(".rua").join("relocation-backups");
    std::fs::create_dir_all(&backups).map_err(io_error)?;
    set_private_directory_permissions(&backups)?;
    let name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("relocation-staging");
    let target = backups.join(format!(
        "{name}-invalid-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::rename(staging, &target).map_err(io_error)?;
    Ok(target)
}

fn resolve_session_candidate(
    directory: &Path,
    session_id: &SessionId,
) -> Result<Option<PathBuf>, StoreError> {
    resolve_session_candidate_inner(directory, session_id, None, &mut HashSet::new())
}

fn resolve_session_candidate_inner(
    directory: &Path,
    session_id: &SessionId,
    minimum_generation: Option<u64>,
    visited: &mut HashSet<PathBuf>,
) -> Result<Option<PathBuf>, StoreError> {
    if !visited.insert(directory.to_path_buf()) {
        return Err(StoreError::InvalidManifest(
            "session relocation redirect chain contains a cycle".to_owned(),
        ));
    }
    let Ok(bytes) = std::fs::read(directory.join("manifest.json")) else {
        return Ok(None);
    };
    let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) else {
        return Ok(None);
    };
    if manifest.schema_version != SCHEMA_VERSION || manifest.session_id != *session_id {
        return Ok(None);
    }
    validate_manifest_metadata(&manifest)?;
    if minimum_generation.is_some_and(|minimum| manifest.locator_generation < minimum) {
        return Err(StoreError::InvalidManifest(
            "session relocation redirect chain regresses locator generation".to_owned(),
        ));
    }
    if manifest.relocation_pending.is_some() {
        return Ok(None);
    }
    if let Some(redirect) = manifest.redirect {
        let target = redirect
            .to
            .project_root
            .join(".rua")
            .join("sessions")
            .join(redirect.to.entry_name.as_str());
        return resolve_session_candidate_inner(
            &target,
            session_id,
            Some(redirect.locator_generation),
            visited,
        );
    }
    Ok(Some(directory.to_path_buf()))
}

fn relocation_tip(
    directory: &Path,
    session_id: &SessionId,
    minimum_generation: Option<u64>,
    visited: &mut HashSet<PathBuf>,
) -> Result<PathBuf, StoreError> {
    if !visited.insert(directory.to_path_buf()) {
        return Err(StoreError::InvalidManifest(
            "session relocation redirect chain contains a cycle".to_owned(),
        ));
    }
    let manifest = read_manifest(&directory.join("manifest.json"), session_id)?;
    if minimum_generation.is_some_and(|minimum| manifest.locator_generation < minimum) {
        return Err(StoreError::InvalidManifest(
            "session relocation redirect chain regresses locator generation".to_owned(),
        ));
    }
    let Some(redirect) = manifest.redirect else {
        return Ok(directory.to_path_buf());
    };
    let target = redirect
        .to
        .project_root
        .join(".rua")
        .join("sessions")
        .join(redirect.to.entry_name.as_str());
    relocation_tip(
        &target,
        session_id,
        Some(redirect.locator_generation),
        visited,
    )
}

fn json_error(error: serde_json::Error) -> StoreError {
    StoreError::InvalidJournal(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::journal::JournalRecord;
    use crate::agent::types::InstructionSet;
    use crate::agent::{
        ConversationHead, DirectoryChangeSource, DirectoryRevision, DirectorySnapshot, EntryId,
        HeadRevision, SessionEntry, SessionEntryPayload,
    };

    fn temp_root(name: &str) -> PathBuf {
        let unique = format!(
            "rua-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn discovers_the_nearest_rua_project_without_changing_the_starting_cwd() {
        let root = temp_root("discover");
        let nested = root.join("crates/runtime");
        std::fs::create_dir_all(root.join(".rua")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            LocalSessionStore::discover_project_root(&nested).unwrap(),
            root.canonicalize().unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn persists_and_loads_a_session() {
        let root = temp_root("persist");
        let store = LocalSessionStore::new(&root);
        let session_id: SessionId = "session-1".into();
        store
            .append(
                &session_id,
                JournalRecord::SessionCreated {
                    instructions: InstructionSet::new("system"),
                    initial_directory: None,
                },
            )
            .await
            .unwrap();

        let recovered = store.load(&session_id).await.unwrap();

        assert_eq!(recovered.last_sequence, JournalSequence(1));
        assert_eq!(recovered.conversation.instructions().text, "system");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn lists_sessions_in_stable_order_without_opening_them() {
        let root = temp_root("list");
        let store = LocalSessionStore::new(&root);
        for name in ["second", "first"] {
            store
                .append(
                    &SessionId::new(name),
                    JournalRecord::SessionCreated {
                        instructions: InstructionSet::new("system"),
                        initial_directory: None,
                    },
                )
                .await
                .unwrap();
        }

        assert_eq!(
            store
                .list_session_ids()
                .unwrap()
                .iter()
                .map(SessionId::as_str)
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn renames_an_entry_without_changing_the_stable_session_id() {
        let root = temp_root("rename");
        let store = LocalSessionStore::new(&root);
        let session_id = SessionId::new("stable-id");
        store
            .append(
                &session_id,
                JournalRecord::SessionCreated {
                    instructions: InstructionSet::new("system"),
                    initial_directory: None,
                },
            )
            .await
            .unwrap();

        let locator = store
            .rename_session(
                &session_id,
                SessionEntryName::try_new("friendly-name").unwrap(),
            )
            .await
            .unwrap();
        store.checkpoint(&session_id).await.unwrap();

        assert_eq!(locator.entry_name.as_str(), "friendly-name");
        assert_eq!(store.list_session_ids().unwrap(), vec![session_id.clone()]);
        assert_eq!(
            store.load(&session_id).await.unwrap().session_id,
            session_id
        );
        assert!(root.join(".rua/sessions/friendly-name").is_dir());
        assert_eq!(
            read_manifest(
                &root.join(".rua/sessions/friendly-name/manifest.json"),
                &session_id,
            )
            .unwrap()
            .locator_generation,
            1
        );
        assert!(!root.join(".rua/sessions/stable-id").exists());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn moves_a_session_between_project_stores_with_a_source_redirect() {
        let source_root = temp_root("move-source");
        let target_root = temp_root("move-target");
        std::fs::create_dir_all(&target_root).unwrap();
        let source = LocalSessionStore::new(&source_root);
        let session_id = SessionId::new("stable-id");
        source
            .append(
                &session_id,
                JournalRecord::SessionCreated {
                    instructions: InstructionSet::new("system"),
                    initial_directory: None,
                },
            )
            .await
            .unwrap();
        let target_locator = SessionLocator {
            project_root: target_root.canonicalize().unwrap(),
            entry_name: SessionEntryName::try_new("moved-session").unwrap(),
        };

        let locator = source
            .relocate_session(&session_id, target_locator.clone())
            .await
            .unwrap();

        assert_eq!(locator, target_locator);
        assert!(source.list_session_ids().unwrap().is_empty());
        assert_eq!(source.locate(&session_id).unwrap(), target_locator);
        assert_eq!(
            source.load(&session_id).await.unwrap().session_id,
            session_id
        );
        drop(source);
        let target = LocalSessionStore::new(&target_root);
        assert_eq!(target.list_session_ids().unwrap(), vec![session_id.clone()]);
        assert_eq!(
            target.load(&session_id).await.unwrap().session_id,
            session_id
        );
        assert_eq!(
            read_manifest(
                &target_root.join(".rua/sessions/moved-session/manifest.json"),
                &session_id,
            )
            .unwrap()
            .locator_generation,
            1
        );
        drop(target);
        std::fs::remove_dir_all(source_root).unwrap();
        std::fs::remove_dir_all(target_root).unwrap();
    }

    #[tokio::test]
    async fn rejects_redirect_cycles_without_a_depth_limit_or_fallback_owner() {
        let first_root = temp_root("redirect-cycle-first");
        let second_root = temp_root("redirect-cycle-second");
        std::fs::create_dir_all(&second_root).unwrap();
        let store = LocalSessionStore::new(&first_root);
        let session_id = SessionId::new("stable-id");
        store
            .append(
                &session_id,
                JournalRecord::SessionCreated {
                    instructions: InstructionSet::new("system"),
                    initial_directory: None,
                },
            )
            .await
            .unwrap();
        store.close_writer(&session_id).await.unwrap();

        let first = store.locate(&session_id).unwrap();
        let second = SessionLocator {
            project_root: second_root.canonicalize().unwrap(),
            entry_name: SessionEntryName::try_new("cycle-target").unwrap(),
        };
        let first_dir = first
            .project_root
            .join(".rua/sessions")
            .join(first.entry_name.as_str());
        let second_dir = second
            .project_root
            .join(".rua/sessions")
            .join(second.entry_name.as_str());
        std::fs::create_dir_all(&second_dir).unwrap();

        let mut first_manifest =
            read_manifest(&first_dir.join("manifest.json"), &session_id).unwrap();
        first_manifest.locator_generation = 2;
        first_manifest.redirect = Some(RelocationRedirect {
            to: second.clone(),
            locator_generation: 2,
        });
        write_json_atomic(&first_dir.join("manifest.json"), &first_manifest).unwrap();
        let mut second_manifest = Manifest::new(session_id.clone());
        second_manifest.locator_generation = 2;
        second_manifest.redirect = Some(RelocationRedirect {
            to: first,
            locator_generation: 2,
        });
        write_json_atomic(&second_dir.join("manifest.json"), &second_manifest).unwrap();

        let error = store.locate(&session_id).unwrap_err();
        assert!(error.to_string().contains("cycle"));

        drop(store);
        std::fs::remove_dir_all(first_root).unwrap();
        std::fs::remove_dir_all(second_root).unwrap();
    }

    #[tokio::test]
    async fn rejects_a_redirect_chain_that_rolls_back_locator_generation() {
        let first_root = temp_root("redirect-generation-first");
        let second_root = temp_root("redirect-generation-second");
        std::fs::create_dir_all(&second_root).unwrap();
        let store = LocalSessionStore::new(&first_root);
        let session_id = SessionId::new("stable-id");
        store
            .append(
                &session_id,
                JournalRecord::SessionCreated {
                    instructions: InstructionSet::new("system"),
                    initial_directory: None,
                },
            )
            .await
            .unwrap();
        store.close_writer(&session_id).await.unwrap();

        let first = store.locate(&session_id).unwrap();
        let second = SessionLocator {
            project_root: second_root.canonicalize().unwrap(),
            entry_name: SessionEntryName::try_new("older-owner").unwrap(),
        };
        let first_dir = first
            .project_root
            .join(".rua/sessions")
            .join(first.entry_name.as_str());
        let second_dir = second
            .project_root
            .join(".rua/sessions")
            .join(second.entry_name.as_str());
        std::fs::create_dir_all(&second_dir).unwrap();

        let mut first_manifest =
            read_manifest(&first_dir.join("manifest.json"), &session_id).unwrap();
        first_manifest.locator_generation = 2;
        first_manifest.redirect = Some(RelocationRedirect {
            to: second,
            locator_generation: 2,
        });
        write_json_atomic(&first_dir.join("manifest.json"), &first_manifest).unwrap();
        let mut second_manifest = Manifest::new(session_id.clone());
        second_manifest.locator_generation = 1;
        write_json_atomic(&second_dir.join("manifest.json"), &second_manifest).unwrap();

        let error = store.locate(&session_id).unwrap_err();
        assert!(error.to_string().contains("regresses locator generation"));

        drop(store);
        std::fs::remove_dir_all(first_root).unwrap();
        std::fs::remove_dir_all(second_root).unwrap();
    }

    #[tokio::test]
    async fn recovers_a_move_after_source_redirect_but_before_target_activation() {
        let source_root = temp_root("move-recover-source");
        let target_root = temp_root("move-recover-target");
        std::fs::create_dir_all(&target_root).unwrap();
        let source = LocalSessionStore::new(&source_root);
        let session_id = SessionId::new("stable-id");
        source
            .append(
                &session_id,
                JournalRecord::SessionCreated {
                    instructions: InstructionSet::new("system"),
                    initial_directory: None,
                },
            )
            .await
            .unwrap();
        let from = source.locate(&session_id).unwrap();
        let to = SessionLocator {
            project_root: target_root.canonicalize().unwrap(),
            entry_name: SessionEntryName::try_new("recovered-move").unwrap(),
        };
        let generation = 1;
        source
            .append(
                &session_id,
                JournalRecord::SessionRelocationPrepared {
                    from: from.clone(),
                    to: to.clone(),
                    locator_generation: generation,
                },
            )
            .await
            .unwrap();
        source.close_writer(&session_id).await.unwrap();
        let source_dir = find_local_session_entry(source.sessions_root(), &session_id).unwrap();
        let target_dir = to
            .project_root
            .join(".rua/sessions")
            .join(".relocating-crash-stable-id");
        std::fs::create_dir_all(target_dir.parent().unwrap()).unwrap();
        let pending = RelocationPending {
            from,
            to: to.clone(),
            locator_generation: generation,
        };
        copy_relocation_staging(&source_dir, &target_dir, &session_id, &pending).unwrap();
        std::fs::write(target_dir.join("journal.log"), b"incomplete").unwrap();
        assert!(
            LocalSessionStore::new(&target_root)
                .list_session_ids()
                .unwrap()
                .is_empty()
        );
        let target_store = LocalSessionStore::new(&target_root);
        assert!(target_store.load(&session_id).await.is_err());
        assert_eq!(
            target_store.recover_relocation(&session_id).await.unwrap(),
            to
        );
        assert!(target_root.join(".rua/relocation-backups").is_dir());
        drop(target_store);
        assert_eq!(
            source.load(&session_id).await.unwrap().pending_relocation,
            None
        );
        drop(source);
        std::fs::remove_dir_all(source_root).unwrap();
        std::fs::remove_dir_all(target_root).unwrap();
    }

    #[tokio::test]
    async fn rejects_session_ids_that_escape_the_sessions_directory() {
        let root = temp_root("session-id");
        let store = LocalSessionStore::new(&root);
        let session_id: SessionId = "../outside".into();

        let error = store
            .append(
                &session_id,
                JournalRecord::SessionCreated {
                    instructions: InstructionSet::new("system"),
                    initial_directory: None,
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(error, StoreError::InvalidSessionId(_)));
        assert!(!root.join(".rua/outside").exists());
    }

    #[tokio::test]
    async fn checkpoints_and_replays_the_wal_suffix() {
        let root = temp_root("checkpoint");
        let session_id: SessionId = "session-1".into();
        {
            let store = LocalSessionStore::new(&root);
            store
                .append(
                    &session_id,
                    JournalRecord::SessionCreated {
                        instructions: InstructionSet::new("system"),
                        initial_directory: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                store.checkpoint(&session_id).await.unwrap(),
                JournalSequence(2)
            );
            assert!(
                root.join(".rua/sessions")
                    .join(session_id.as_str())
                    .join("snapshot.json")
                    .is_file()
            );
        }

        let store = LocalSessionStore::new(&root);
        let recovered = store.load(&session_id).await.unwrap();

        assert_eq!(recovered.last_sequence, JournalSequence(2));
        assert_eq!(recovered.conversation.instructions().text, "system");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn checkpoint_replays_a_directory_established_from_unknown_state() {
        let root = temp_root("checkpoint-unknown-cwd");
        let session_id = SessionId::new("session-1");
        {
            let store = LocalSessionStore::new(&root);
            store
                .append(
                    &session_id,
                    JournalRecord::SessionCreated {
                        instructions: InstructionSet::new("system"),
                        initial_directory: None,
                    },
                )
                .await
                .unwrap();
            let directory = DirectorySnapshot {
                path: root.canonicalize().unwrap(),
                revision: DirectoryRevision(0),
            };
            store
                .append(
                    &session_id,
                    JournalRecord::SessionEntryAppended {
                        entry: SessionEntry {
                            id: EntryId::new("entry-1"),
                            parent_id: None,
                            timestamp_unix_ms: 0,
                            payload: SessionEntryPayload::CwdChanged {
                                input: "/cd <absolute>".to_owned(),
                                from: None,
                                to: directory.clone(),
                                source: DirectoryChangeSource::UserCommand,
                            },
                        },
                        expected_head: ConversationHead::default(),
                        resulting_head_revision: HeadRevision(1),
                    },
                )
                .await
                .unwrap();
            store.checkpoint(&session_id).await.unwrap();
        }

        let store = LocalSessionStore::new(&root);
        let recovered = store.load(&session_id).await.unwrap();

        assert_eq!(
            recovered.directory.as_ref().unwrap().path,
            root.canonicalize().unwrap()
        );
        assert!(matches!(
            recovered
                .tree
                .path_to_head()
                .unwrap()
                .first()
                .unwrap()
                .payload,
            SessionEntryPayload::CwdChanged { from: None, .. }
        ));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn truncates_an_incomplete_crash_tail_after_the_valid_prefix() {
        let root = temp_root("tail");
        let session_id: SessionId = "session-1".into();
        {
            let store = LocalSessionStore::new(&root);
            store
                .append(
                    &session_id,
                    JournalRecord::SessionCreated {
                        instructions: InstructionSet::new("system"),
                        initial_directory: None,
                    },
                )
                .await
                .unwrap();
        }
        let journal = root
            .join(".rua/sessions")
            .join(session_id.as_str())
            .join("journal.log");
        let valid_length = std::fs::metadata(&journal).unwrap().len();
        OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap()
            .write_all(b"00000020 deadbeef {\"partial\"")
            .unwrap();

        let store = LocalSessionStore::new(&root);
        let recovered = store.load(&session_id).await.unwrap();
        assert_eq!(recovered.last_sequence, JournalSequence(1));
        assert_eq!(std::fs::metadata(&journal).unwrap().len(), valid_length);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rejects_a_complete_frame_with_a_bad_checksum() {
        let root = temp_root("checksum");
        let session_id: SessionId = "session-1".into();
        {
            let store = LocalSessionStore::new(&root);
            store
                .append(
                    &session_id,
                    JournalRecord::SessionCreated {
                        instructions: InstructionSet::new("system"),
                        initial_directory: None,
                    },
                )
                .await
                .unwrap();
        }
        let journal = root
            .join(".rua/sessions")
            .join(session_id.as_str())
            .join("journal.log");
        let mut bytes = std::fs::read(&journal).unwrap();
        bytes[HEADER_BYTES] ^= 1;
        std::fs::write(&journal, bytes).unwrap();

        let store = LocalSessionStore::new(&root);
        let error = store.load(&session_id).await.unwrap_err();

        assert!(matches!(error, StoreError::CorruptFrame { .. }));
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn refuses_a_second_writer_for_the_same_session() {
        let root = temp_root("lock");
        let session_id: SessionId = "session-1".into();
        let first = LocalSessionStore::new(&root);
        first
            .append(
                &session_id,
                JournalRecord::SessionCreated {
                    instructions: InstructionSet::new("system"),
                    initial_directory: None,
                },
            )
            .await
            .unwrap();
        let second = LocalSessionStore::new(&root);

        let error = second.load(&session_id).await.unwrap_err();

        assert!(matches!(error, StoreError::SessionLocked { .. }));
        drop(second);
        drop(first);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn exports_raw_session_files_without_modifying_the_source() {
        let root = temp_root("export-source");
        let destination = temp_root("export-destination");
        let session_id: SessionId = "session-1".into();
        {
            let store = LocalSessionStore::new(&root);
            store
                .append(
                    &session_id,
                    JournalRecord::SessionCreated {
                        instructions: InstructionSet::new("system"),
                        initial_directory: None,
                    },
                )
                .await
                .unwrap();
        }
        let store = LocalSessionStore::new(&root);

        let exported = store.export_raw(&session_id, &destination).unwrap();

        assert!(exported.join("manifest.json").is_file());
        assert!(exported.join("journal.log").is_file());
        assert!(root.join(".rua/sessions/session-1/journal.log").is_file());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(destination).unwrap();
    }
}
