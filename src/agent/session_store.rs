use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::journal::{
    JournalEntry, JournalRecord, JournalSequence, RecoveredSession, SessionStore, StoreError,
    StoreFuture, replay_after, replay_session,
};
use super::types::SessionId;

const SCHEMA_VERSION: u32 = 1;
const HEADER_BYTES: usize = 18;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairReport {
    pub backup_path: PathBuf,
    pub removed_bytes: u64,
}

pub struct LocalSessionStore {
    sessions_root: PathBuf,
    writers: Mutex<HashMap<SessionId, Arc<Mutex<SessionWriter>>>>,
}

impl LocalSessionStore {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            sessions_root: project_root.into().join(".rua").join("sessions"),
            writers: Mutex::new(HashMap::new()),
        }
    }

    pub fn sessions_root(&self) -> &Path {
        &self.sessions_root
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
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| session_directory(&self.sessions_root, &SessionId::new(name)).is_ok())
            .map(SessionId::new)
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        Ok(sessions)
    }

    pub async fn validate_session(
        &self,
        session_id: &SessionId,
    ) -> Result<RecoveredSession, StoreError> {
        let session_dir = session_directory(&self.sessions_root, session_id)?;
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
        let source = session_directory(&self.sessions_root, session_id)?;
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
        let session_dir = session_directory(&self.sessions_root, session_id)?;
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
        let session_dir = session_directory(sessions_root, session_id)?;
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
    Ok(manifest)
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

fn json_error(error: serde_json::Error) -> StoreError {
    StoreError::InvalidJournal(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::journal::JournalRecord;
    use crate::agent::types::InstructionSet;

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
    async fn rejects_session_ids_that_escape_the_sessions_directory() {
        let root = temp_root("session-id");
        let store = LocalSessionStore::new(&root);
        let session_id: SessionId = "../outside".into();

        let error = store
            .append(
                &session_id,
                JournalRecord::SessionCreated {
                    instructions: InstructionSet::new("system"),
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
