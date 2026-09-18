//! Recoverable event caches. A replacement uses a new LMDB path: renaming or
//! deleting an environment while another process has it mapped is unsafe.
//! The old files remain available for diagnosis; only an atomic pointer
//! changes.

use std::{
    collections::{BTreeSet, HashMap},
    fs::{File, OpenOptions, TryLockError},
    future::Future,
    io::Write,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use nostr::prelude::{Event, EventId, Filter, Timestamp};
use nostr_database::{
    NostrDatabase, SaveEventStatus,
    error::{Error as DatabaseError, ErrorKind},
};
use nostr_lmdb::NostrLmdb;

#[derive(Default)]
struct Observation {
    path: Option<PathBuf>,
    generation: u64,
    replaced: bool,
}

static RECOVERIES: OnceLock<Mutex<HashMap<PathBuf, Observation>>> = OnceLock::new();

/// Changes when this process replaces (or observes replacement of) a cache.
/// Fetch planning uses this to discard assumptions about previously cached IDs.
pub(crate) fn generation(path: &Path) -> u64 {
    RECOVERIES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .get(path)
        .map_or(0, |entry| entry.generation)
}

fn observe(root: &Path, path: &Path, replaced: bool) {
    let mut observations = RECOVERIES.get_or_init(Mutex::default).lock().unwrap();
    let entry = observations.entry(root.to_path_buf()).or_default();
    if replaced || entry.path.as_ref().is_some_and(|previous| previous != path) {
        entry.generation += 1;
    }
    entry.path = Some(path.to_path_buf());
    entry.replaced |= replaced;
}

fn already_replaced(root: &Path) -> bool {
    RECOVERIES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .get(root)
        .is_some_and(|entry| entry.replaced)
}

pub(crate) enum Database {
    Persistent {
        root: PathBuf,
        state: Box<tokio::sync::Mutex<State>>,
    },
    Memory(Arc<dyn NostrDatabase>),
}

pub(crate) struct State {
    path: PathBuf,
    database: NostrLmdb,
}

impl State {
    async fn follow_generation(&mut self, root: &Path) -> Result<()> {
        let current = current_path(root)?;
        if self.path != current {
            self.database = NostrLmdb::open(&current).await.with_context(|| {
                format!("failed to open replacement cache at {}", current.display())
            })?;
            self.path = current;
        }
        observe(root, &self.path, false);
        Ok(())
    }
}

impl Database {
    pub(crate) async fn open(root: &Path) -> Result<Self> {
        let lock = recovery_lock(root, true).await?;
        let path = current_path(root)?;
        let opened = NostrLmdb::open(&path).await;
        observe(root, &path, false);
        drop(lock);
        let state = match opened {
            Ok(database) => State { path, database },
            Err(error) if is_corrupt(&error) => replace(root, &path, &error).await?,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to open event cache at {}", path.display()));
            }
        };
        Ok(Self::Persistent {
            root: root.to_path_buf(),
            state: Box::new(tokio::sync::Mutex::new(state)),
        })
    }

    async fn run<T>(
        &self,
        name: &str,
        corruption_probe: Option<Filter>,
        operation: impl for<'a> Fn(
            &'a dyn NostrDatabase,
        ) -> Pin<
            Box<dyn Future<Output = std::result::Result<T, DatabaseError>> + Send + 'a>,
        >,
    ) -> Result<T> {
        match self {
            Self::Memory(database) => operation(database.as_ref()).await.map_err(Into::into),
            Self::Persistent { root, state } => {
                let mut state = state.lock().await;
                // Keep the selected environment alive and prevent another ngit
                // process from switching generations during this operation.
                let lock = recovery_lock(root, true).await?;
                state.follow_generation(root).await?;
                let mut result = operation(&state.database).await;
                if result.as_ref().err().is_some_and(|error| {
                    error.kind() == ErrorKind::Storage
                        && error.to_string() == "Batched transaction failed"
                }) {
                    // The ingester replaces the original write error with this
                    // generic failure. Reset only if a read independently
                    // confirms corruption, never merely because a write failed.
                    if let Some(filter) = corruption_probe {
                        if let Err(error) = state.database.query(filter).await {
                            if is_corrupt(&error) {
                                result = Err(error);
                            }
                        }
                    }
                }
                drop(lock);
                match result {
                    Ok(result) => Ok(result),
                    Err(error) if is_corrupt(&error) => {
                        *state = replace(root, &state.path, &error).await?;
                        let _lock = recovery_lock(root, true).await?;
                        state.follow_generation(root).await?;
                        operation(&state.database).await.with_context(|| {
                            format!(
                                "failed to {name} rebuilt event cache at {}",
                                state.path.display()
                            )
                        })
                    }
                    Err(error) => Err(error).with_context(|| {
                        format!("failed to {name} event cache at {}", state.path.display())
                    }),
                }
            }
        }
    }

    pub(crate) async fn query(&self, filter: Filter) -> Result<BTreeSet<Event>> {
        self.run("query", None, |db| db.query(filter.clone())).await
    }

    pub(crate) async fn negentropy_items(
        &self,
        filter: Filter,
    ) -> Result<Vec<(EventId, Timestamp)>> {
        self.run("read event IDs from", None, |db| {
            db.negentropy_items(filter.clone())
        })
        .await
    }

    pub(crate) async fn save_event(&self, event: &Event) -> Result<SaveEventStatus> {
        let probe = if event.kind.is_addressable() {
            Filter::new()
                .author(event.pubkey)
                .identifier(event.tags.identifier().unwrap_or_default())
        } else {
            Filter::new().author(event.pubkey).kind(event.kind)
        };
        self.run("save an event in", Some(probe), |db| {
            let event = event.clone();
            Box::pin(async move { db.save_event(&event).await })
        })
        .await
    }

    pub(crate) async fn delete(&self, filter: Filter) -> Result<()> {
        self.run("delete from", Some(filter.clone()), |db| {
            db.delete(filter.clone())
        })
        .await
    }
}

fn companion(root: &Path, suffix: &str) -> PathBuf {
    let mut path = root.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn current_path(root: &Path) -> Result<PathBuf> {
    let pointer = companion(root, ".current");
    let name = match std::fs::read_to_string(&pointer) {
        Ok(name) => name,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(root.to_path_buf()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read cache generation at {}", pointer.display())
            });
        }
    };
    let name = name.trim();
    let path = Path::new(name);
    let prefix = format!(
        "{}.recovered-",
        root.file_name()
            .context("cache path has no filename")?
            .to_string_lossy()
    );
    if !name.starts_with(&prefix)
        || !matches!(path.components().next(), Some(Component::Normal(_)))
        || path.components().count() != 1
    {
        bail!("invalid cache generation in {}", pointer.display());
    }
    Ok(root
        .parent()
        .context("cache path has no parent")?
        .join(path))
}

async fn recovery_lock(root: &Path, shared: bool) -> Result<File> {
    let path = companion(root, ".recovery-lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open cache recovery lock at {}", path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        } {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to lock cache recovery at {}", path.display())
                });
            }
        }
    }
}

async fn replace(root: &Path, failed_path: &Path, cause: &DatabaseError) -> Result<State> {
    let _lock = recovery_lock(root, false).await?;
    let current = current_path(root)?;
    if current != failed_path {
        // Another process has already recovered it. Never discard its new data.
        let database = NostrLmdb::open(&current).await.with_context(|| {
            format!(
                "failed to open concurrently rebuilt cache at {}",
                current.display()
            )
        })?;
        observe(root, &current, false);
        return Ok(State {
            path: current,
            database,
        });
    }
    if already_replaced(root) {
        bail!(
            "event cache at {} failed again after recovery: {cause}; refusing repeated resets",
            failed_path.display()
        );
    }
    let parent = root.parent().context("cache path has no parent")?;
    let prefix = format!(
        "{}.recovered-",
        root.file_name()
            .context("cache path has no filename")?
            .to_string_lossy()
    );
    let directory = tempfile::Builder::new()
        .prefix(&prefix)
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "failed to create replacement for cache at {}",
                failed_path.display()
            )
        })?;
    let database = NostrLmdb::open(directory.path())
        .await
        .context("failed to initialize replacement event cache")?;
    // Keep the directory before publishing the pointer, including if a later
    // filesystem error occurs: an LMDB environment may still be mapped.
    let path = directory.keep();
    let mut pointer = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(
        pointer,
        "{}",
        path.file_name()
            .context("replacement cache has no filename")?
            .to_string_lossy()
    )?;
    pointer.as_file().sync_all()?;
    pointer
        .persist(companion(root, ".current"))
        .context("failed to publish replacement cache generation")?;
    observe(root, &path, true);
    eprintln!(
        "warning: corrupt or incompatible event cache at {}: {cause}; using fresh cache at {}. The old files were preserved; online commands will fetch events again.",
        failed_path.display(),
        path.display()
    );
    Ok(State { path, database })
}

fn is_corrupt(error: &DatabaseError) -> bool {
    // nostr-lmdb hides its StoreError and does not expose its source chain.
    // Match only known storage/format diagnostics from the pinned backend.
    // Never reset for permissions, disk exhaustion, locks or generic I/O errors.
    let message = error.to_string();
    if error.kind() == ErrorKind::Migration {
        return message.starts_with("Database version ")
            && message.contains(" is newer than supported version ");
    }
    error.kind() == ErrorKind::Storage
        && (message == "Not found"
            || [
                "MDB_CORRUPTED",
                "MDB_PAGE_NOTFOUND",
                "MDB_INVALID",
                "MDB_VERSION_MISMATCH",
                "MDB_INCOMPATIBLE",
            ]
            .iter()
            .any(|prefix| message.starts_with(prefix))
            || message.starts_with("flatbuffer '")
            || [
                "Missing required field `",
                "Exactly one of union discriminant (",
                "Utf8 error for string in ",
                "String in range [",
                "Type `",
                "Range [",
                "Signed offset at position ",
                "Too many tables.",
                "Apparent size too large.",
                "Nested table depth limit reached.",
            ]
            .iter()
            .any(|prefix| message.starts_with(prefix)))
}

#[cfg(test)]
mod tests {
    use heed::{
        Env, EnvFlags, EnvOpenOptions,
        byteorder::NativeEndian,
        types::{Bytes, U64},
    };
    use nostr::prelude::*;

    use super::*;

    fn environment(path: &Path) -> Result<Env> {
        // Match nostr-lmdb's options so heed shares an already-open environment.
        // All mutations below use LMDB transactions, never edits of mapped bytes.
        Ok(unsafe {
            EnvOpenOptions::new()
                .flags(EnvFlags::NO_TLS)
                .max_dbs(12)
                .max_readers(126)
                .map_size(if usize::BITS == 64 {
                    32 * 1024 * 1024 * 1024
                } else {
                    0xFFFFF000
                })
                .open(path)?
        })
    }

    fn event(keys: &Keys, timestamp: u64) -> Event {
        EventBuilder::new(Kind::GitRepoAnnouncement, "")
            .tags([Tag::identifier("cache-recovery")])
            .custom_created_at(Timestamp::from_secs(timestamp))
            .finalize(keys)
            .unwrap()
    }

    fn filter(event: &Event) -> Filter {
        Filter::new()
            .author(event.pubkey)
            .kind(event.kind)
            .identifier("cache-recovery")
    }

    fn remove_record_leaving_indexes(
        path: &Path,
        event: &Event,
        legacy_schema: bool,
    ) -> Result<()> {
        let env = environment(path)?;
        let mut txn = env.write_txn()?;
        let events = env.open_database::<Bytes, Bytes>(&txn, None)?.unwrap();
        assert!(events.delete(&mut txn, event.id.as_bytes())?);
        if legacy_schema {
            let metadata = env
                .open_database::<Bytes, U64<NativeEndian>>(&txn, Some("metadata"))?
                .unwrap();
            metadata.put(&mut txn, b"db_version", &0)?;
        }
        txn.commit()?;
        Ok(())
    }

    #[tokio::test]
    async fn legacy_migration_leaves_dangling_indexes_but_cache_recovers() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("nostr-cache.lmdb");
        let event = event(&Keys::generate(), 1);
        let original = NostrLmdb::open(&root).await?;
        original.save_event(&event).await?;
        remove_record_leaving_indexes(&root, &event, true)?;
        // The backend's upgrade rebuilds kci, but leaves stale akci/atci entries.
        let upgraded = NostrLmdb::open(&root).await?;
        assert_eq!(
            upgraded
                .query(filter(&event))
                .await
                .unwrap_err()
                .to_string(),
            "Not found"
        );
        let cache = Database::open(&root).await?;
        assert!(cache.query(filter(&event)).await?.is_empty());
        let replacement = current_path(&root)?;
        assert_ne!(replacement, root);
        assert!(root.join("data.mdb").exists());
        assert!(upgraded.query(filter(&event)).await.is_err());
        cache.save_event(&event).await?;
        assert_eq!(
            Database::open(&root)
                .await?
                .query(filter(&event))
                .await?
                .len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn event_id_reads_and_replacement_writes_recover() -> Result<()> {
        for write in [false, true] {
            let dir = tempfile::tempdir()?;
            let root = dir.path().join("nostr-cache.lmdb");
            let keys = Keys::generate();
            let old = event(&keys, 1);
            let original = NostrLmdb::open(&root).await?;
            original.save_event(&old).await?;
            remove_record_leaving_indexes(&root, &old, false)?;
            let cache = Database::open(&root).await?;
            if write {
                let new = event(&keys, 2);
                assert!(cache.save_event(&new).await?.is_success());
                assert_eq!(
                    cache.query(filter(&new)).await?.iter().next().unwrap().id,
                    new.id
                );
            } else {
                assert!(cache.negentropy_items(filter(&old)).await?.is_empty());
            }
            assert_ne!(current_path(&root)?, root);
        }
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_recovery_keeps_one_replacement_and_refreshes_old_handles() -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let dir = tempfile::tempdir()?;
            let root = dir.path().join("nostr-cache.lmdb");
            let keys = Keys::generate();
            let old = event(&keys, 1);
            let original = NostrLmdb::open(&root).await?;
            original.save_event(&old).await?;
            remove_record_leaving_indexes(&root, &old, false)?;
            let mut caches = Vec::new();
            for _ in 0..8 {
                caches.push(Database::open(&root).await?);
            }
            for result in
                futures::future::join_all(caches.iter().map(|cache| cache.query(filter(&old))))
                    .await
            {
                assert!(result?.is_empty());
            }
            assert_eq!(generation(&root), 1);
            let new = event(&keys, 2);
            caches[0].save_event(&new).await?;
            for cache in &caches {
                assert_eq!(cache.query(filter(&new)).await?.len(), 1);
            }
            assert_eq!(
                std::fs::read_dir(dir.path())?
                    .filter_map(std::result::Result::ok)
                    .filter(|entry| entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("nostr-cache.lmdb.recovered-"))
                    .count(),
                1
            );
            Ok::<_, anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn healthy_legacy_cache_migrates_without_replacement() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("nostr-cache.lmdb");
        let event = event(&Keys::generate(), 1);
        let original = NostrLmdb::open(&root).await?;
        original.save_event(&event).await?;
        let env = environment(&root)?;
        let mut txn = env.write_txn()?;
        let metadata = env
            .open_database::<Bytes, U64<NativeEndian>>(&txn, Some("metadata"))?
            .unwrap();
        metadata.put(&mut txn, b"db_version", &0)?;
        txn.commit()?;
        let cache = Database::open(&root).await?;
        assert_eq!(cache.query(filter(&event)).await?.len(), 1);
        assert_eq!(current_path(&root)?, root);
        assert_eq!(generation(&root), 0);
        Ok(())
    }

    #[tokio::test]
    async fn malformed_event_records_are_replaced() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("nostr-cache.lmdb");
        let event = event(&Keys::generate(), 1);
        let original = NostrLmdb::open(&root).await?;
        original.save_event(&event).await?;
        let env = environment(&root)?;
        let mut txn = env.write_txn()?;
        let events = env.open_database::<Bytes, Bytes>(&txn, None)?.unwrap();
        events.put(&mut txn, event.id.as_bytes(), &[0])?;
        txn.commit()?;
        let cache = Database::open(&root).await?;
        assert!(cache.query(filter(&event)).await?.is_empty());
        assert_ne!(current_path(&root)?, root);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_lmdb_file_is_preserved_and_replaced() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("nostr-cache.lmdb");
        std::fs::create_dir(&root)?;
        let bytes = vec![0xff; 8192];
        std::fs::write(root.join("data.mdb"), &bytes)?;
        let cache = Database::open(&root).await?;
        assert!(cache.query(Filter::new()).await?.is_empty());
        assert_eq!(std::fs::read(root.join("data.mdb"))?, bytes);
        assert_ne!(current_path(&root)?, root);
        Ok(())
    }

    #[tokio::test]
    async fn newer_schema_is_isolated_and_repeated_corruption_stops() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("nostr-cache.lmdb");
        let original = NostrLmdb::open(&root).await?;
        let env = environment(&root)?;
        let mut txn = env.write_txn()?;
        let metadata = env
            .open_database::<Bytes, U64<NativeEndian>>(&txn, Some("metadata"))?
            .unwrap();
        metadata.put(&mut txn, b"db_version", &999)?;
        txn.commit()?;
        let cache = Database::open(&root).await?;
        let replacement = current_path(&root)?;
        assert_ne!(replacement, root);
        let event = event(&Keys::generate(), 1);
        cache.save_event(&event).await?;
        remove_record_leaving_indexes(&replacement, &event, false)?;
        let error = cache.query(filter(&event)).await.unwrap_err();
        assert!(error.to_string().contains("refusing repeated resets"));
        assert_eq!(current_path(&root)?, replacement);
        drop(original);
        Ok(())
    }

    #[test]
    fn operational_errors_are_not_corruption() {
        for message in [
            "Permission denied (os error 13)",
            "No space left on device (os error 28)",
            "MDB_MAP_FULL: Environment mapsize limit reached",
            "MDB_READERS_FULL: Environment maxreaders limit reached",
            "MDB_BAD_RSLOT: Invalid reuse of reader locktable slot",
        ] {
            assert!(!is_corrupt(&DatabaseError::storage(std::io::Error::other(
                message
            ))));
        }
        assert!(!is_corrupt(&DatabaseError::io(std::io::Error::other(
            "Not found"
        ))));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn permissions_do_not_replace_the_cache() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return Ok(());
        }
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("nostr-cache.lmdb");
        std::fs::create_dir(&root)?;
        let data = root.join("data.mdb");
        std::fs::write(&data, vec![0xff; 8192])?;
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o0))?;
        let result = Database::open(&root).await;
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o600))?;
        let error = result.err().context("permission failure was swallowed")?;
        assert!(format!("{error:#}").contains("Permission denied"));
        assert_eq!(current_path(&root)?, root);
        assert_eq!(generation(&root), 0);
        Ok(())
    }
}
