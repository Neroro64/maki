use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use maki_storage::snapshot::Snapshots;
use serde::Serialize;
use tracing::{debug, warn};

const STALE_READ_MSG: &str = "file changed since last read";
const TASK_FINISHED_MSG: &str = "task finished; this task can no longer modify files";
const REWOUND_MSG: &str = "session was rewound; this task can no longer modify files";
const FILE_SYMBOL: &str = "<file>";
const MAX_READ_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_REWIND_TRAJECTORY_BYTES: usize = 12 * 1024;
const MAX_MUTATION_PATHS: usize = 512;

/// Dead `Weak`s are only ever cleared once the map grows to this many entries,
/// so the map is bounded by construction instead of by a drop hook.
const LOCK_PRUNE_AT: usize = 512;

/// A path that has been through [`maki_storage::paths::canonical_key`]. All
/// maps here are keyed by one, so no caller can key them with a raw path. Two
/// spellings of one file would get two entries, and for the lock map that
/// means no mutual exclusion at all.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileKey(PathBuf);

impl FileKey {
    pub fn new(path: &Path) -> Self {
        Self(maki_storage::paths::canonical_key(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

type FileLock = Arc<smol::lock::Mutex<()>>;
pub type FileGuard = smol::lock::MutexGuardArc<()>;
type FileLocks = Mutex<HashMap<FileKey, Weak<smol::lock::Mutex<()>>>>;
#[derive(Serialize)]
pub struct DiffSnapshot {
    pub captured: bool,
    pub mutated: bool,
    pub root: Option<PathBuf>,
    pub reference: Option<String>,
    pub attributes: Option<String>,
    pub paths: Option<Vec<PathBuf>>,
}

#[derive(Default)]
struct DiffBase {
    root: Option<PathBuf>,
    reference: Option<String>,
    attributes: Option<String>,
}

#[derive(Default)]
struct Mutations {
    attempted: bool,
    unscoped: bool,
    paths: HashSet<FileKey>,
    baseline: Option<Arc<DiffBase>>,
}

#[derive(Default)]
struct SnapshotState {
    session: String,
    turn: usize,
    captured: bool,
    repository: Option<Snapshots>,
    base: Option<Arc<DiffBase>>,
}

struct SymbolOwner {
    id: Arc<str>,
    intent: Arc<str>,
    active: AtomicBool,
}

pub struct SymbolLock {
    pub holder_subagent_id: Arc<str>,
    pub intent_description: Arc<str>,
    pub acquired_at: SystemTime,
    owner: Arc<SymbolOwner>,
}

#[derive(Default)]
struct SymbolState {
    tasks: Vec<Weak<SymbolOwner>>,
    files: HashMap<FileKey, BTreeMap<String, SymbolLock>>,
}

/// Session-wide claims acquired atomically and retained beyond individual writes.
#[derive(Default)]
pub struct SymbolLockTable {
    state: Mutex<SymbolState>,
}

impl SymbolLockTable {
    fn register(&self, owner: &Arc<SymbolOwner>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.tasks.retain(|task| task.strong_count() > 0);
        state.tasks.push(Arc::downgrade(owner));
    }

    fn release(&self, owner: &Arc<SymbolOwner>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !owner.active.swap(false, Ordering::AcqRel) {
            return;
        }
        state.files.retain(|_, symbols| {
            symbols.retain(|_, held| !Arc::ptr_eq(&held.owner, owner));
            !symbols.is_empty()
        });
        state.tasks.retain(|registered| {
            registered
                .upgrade()
                .is_some_and(|registered| !Arc::ptr_eq(&registered, owner))
        });
    }

    fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for owner in state.tasks.iter().filter_map(Weak::upgrade) {
            owner.active.store(false, Ordering::Release);
        }
        state.tasks.clear();
        state.files.clear();
    }

    fn claim(
        &self,
        key: &FileKey,
        owner: Option<&Arc<SymbolOwner>>,
        symbols: &[String],
        generation: &AtomicU64,
        observed_generation: u64,
    ) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if observed_generation != generation.load(Ordering::Acquire) {
            return Err(REWOUND_MSG.to_owned());
        }
        if owner.is_some_and(|owner| !owner.active.load(Ordering::Acquire)) {
            return Err(TASK_FINISHED_MSG.to_owned());
        }
        if let Some(held) = state.files.get(key) {
            for requested in symbols {
                for (symbol, lock) in held {
                    if owner.is_some_and(|owner| Arc::ptr_eq(owner, &lock.owner)) {
                        continue;
                    }
                    if requested == symbol || requested == FILE_SYMBOL || symbol == FILE_SYMBOL {
                        return Err(format!(
                            "Conflict: Subagent {} ({}) currently holds lock on symbol {} in {}",
                            lock.holder_subagent_id,
                            lock.intent_description,
                            symbol,
                            key.as_path().display(),
                        ));
                    }
                }
            }
        }
        if let Some(owner) = owner {
            let held = state.files.entry(key.clone()).or_default();
            for symbol in symbols {
                held.entry(symbol.clone()).or_insert_with(|| SymbolLock {
                    holder_subagent_id: Arc::clone(&owner.id),
                    intent_description: Arc::clone(&owner.intent),
                    acquired_at: SystemTime::now(),
                    owner: Arc::clone(owner),
                });
            }
        }
        Ok(())
    }
}

/// Retiring this lease releases its claims and permanently revokes its task scope.
pub struct SymbolLease {
    table: Arc<SymbolLockTable>,
    owner: Arc<SymbolOwner>,
}

impl SymbolLease {
    pub fn release(&self) {
        self.table.release(&self.owner);
    }
}

impl Drop for SymbolLease {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Default)]
struct ReadState {
    mtimes: HashMap<FileKey, SystemTime>,
    sources: HashMap<FileKey, Arc<str>>,
    source_bytes: usize,
}

/// What one agent read and when, plus the per-file write locks that make one
/// tool's read-modify-write safe against another's. The locks can be shared
/// with agents working next to this one, see [`FileAccess::sharing_locks`].
/// Subagent sessions use [`FileAccess::task_scope`] to retain symbol claims
/// until the session ends while keeping their read baselines separate.
#[derive(Default)]
pub struct FileAccess {
    reads: Mutex<ReadState>,
    locks: Arc<FileLocks>,
    snapshots: Arc<Mutex<SnapshotState>>,
    mutation_gate: Arc<smol::lock::RwLock<()>>,
    generation: Arc<AtomicU64>,
    observed_generation: u64,
    symbols: Arc<SymbolLockTable>,
    owner: Option<Arc<SymbolOwner>>,
    rewind_trajectory: Arc<Mutex<Option<Arc<str>>>>,
    mutations: Mutex<Mutations>,
}

fn get_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl FileAccess {
    pub fn fresh() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// For an agent that runs next to this one instead of under it. Its reads
    /// stay its own. Say the user changes a file we read earlier: if its fresh
    /// read counted for us too, we would never hear our copy is stale, and our
    /// next edit would quietly undo the user's. The locks are shared, so two
    /// edits of one file still take turns.
    pub fn sharing_locks(&self) -> Arc<Self> {
        Arc::new(Self {
            reads: Mutex::default(),
            locks: Arc::clone(&self.locks),
            snapshots: Arc::clone(&self.snapshots),
            mutation_gate: Arc::clone(&self.mutation_gate),
            generation: Arc::clone(&self.generation),
            observed_generation: self.generation.load(Ordering::Acquire),
            symbols: Arc::clone(&self.symbols),
            rewind_trajectory: Arc::clone(&self.rewind_trajectory),
            owner: None,
            mutations: Mutex::default(),
        })
    }

    /// Create isolated task reads with a lease on the session's shared symbol table.
    pub fn task_scope(&self, id: &str, intent: &str) -> (Arc<Self>, SymbolLease) {
        let owner = Arc::new(SymbolOwner {
            id: Arc::from(id),
            intent: Arc::from(intent),
            active: AtomicBool::new(true),
        });
        self.symbols.register(&owner);
        let access = Arc::new(Self {
            reads: Mutex::default(),
            locks: Arc::clone(&self.locks),
            snapshots: Arc::clone(&self.snapshots),
            mutation_gate: Arc::clone(&self.mutation_gate),
            generation: Arc::clone(&self.generation),
            observed_generation: self.observed_generation,
            symbols: Arc::clone(&self.symbols),
            rewind_trajectory: Arc::clone(&self.rewind_trajectory),
            owner: Some(Arc::clone(&owner)),
            mutations: Mutex::default(),
        });
        let lease = SymbolLease {
            table: Arc::clone(&self.symbols),
            owner,
        };
        (access, lease)
    }
    pub fn set_rewind_trajectory(&self, trajectory: Option<&str>) {
        let trajectory = trajectory.map(|text| {
            let end = text.floor_char_boundary(text.len().min(MAX_REWIND_TRAJECTORY_BYTES));
            Arc::from(&text[..end])
        });
        *self
            .rewind_trajectory
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = trajectory;
    }

    pub fn rewind_trajectory(&self) -> Option<Arc<str>> {
        self.rewind_trajectory
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub fn note_mutation(&self, path: Option<&FileKey>) {
        let mut mutations = self
            .mutations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        mutations.attempted = true;
        if let Some(path) = path {
            if self.owner.is_some() && !mutations.unscoped {
                if mutations.paths.len() >= MAX_MUTATION_PATHS && !mutations.paths.contains(path) {
                    mutations.unscoped = true;
                    mutations.paths.clear();
                } else {
                    mutations.paths.insert(path.clone());
                }
            }
        } else {
            mutations.unscoped = true;
            mutations.paths.clear();
        }
    }

    pub fn diff_snapshot(&self) -> DiffSnapshot {
        let snapshots = self
            .snapshots
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mutations = self
            .mutations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let base = if self.owner.is_some() {
            mutations.baseline.as_ref().or(snapshots.base.as_ref())
        } else {
            snapshots.base.as_ref()
        };
        let paths = if self.owner.is_some() && !mutations.unscoped {
            let mut paths: Vec<_> = mutations
                .paths
                .iter()
                .map(|key| key.as_path().to_path_buf())
                .collect();
            paths.sort_unstable();
            Some(paths)
        } else {
            None
        };
        DiffSnapshot {
            captured: base.is_some(),
            mutated: mutations.attempted || (self.owner.is_none() && snapshots.captured),
            root: base.and_then(|base| base.root.clone()),
            reference: base.and_then(|base| base.reference.clone()),
            attributes: base.and_then(|base| base.attributes.clone()),
            paths,
        }
    }

    pub fn claim_symbols(
        &self,
        key: &FileKey,
        holder: &str,
        symbols: &[String],
    ) -> Result<(), String> {
        self.check_active()?;
        if self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.id.as_ref() != holder)
        {
            return Err("symbol holder does not match this task".to_owned());
        }
        self.symbols.claim(
            key,
            self.owner.as_ref(),
            symbols,
            &self.generation,
            self.observed_generation,
        )
    }

    fn check_active(&self) -> Result<(), String> {
        if self.observed_generation != self.generation.load(Ordering::Acquire) {
            return Err(REWOUND_MSG.to_owned());
        }
        if self
            .owner
            .as_ref()
            .is_some_and(|owner| !owner.active.load(Ordering::Acquire))
        {
            return Err(TASK_FINISHED_MSG.to_owned());
        }
        Ok(())
    }

    pub async fn mutation_guard(&self) -> Result<smol::lock::RwLockReadGuard<'_, ()>, String> {
        let guard = self.mutation_gate.read().await;
        self.check_active()?;
        Ok(guard)
    }

    pub fn restore_snapshot(
        &self,
        cwd: &Path,
        session: &str,
        turn: usize,
    ) -> Result<Option<bool>, String> {
        let _guard = smol::block_on(self.mutation_gate.write());
        let result = Snapshots::discover(cwd, session)
            .map_err(|e| e.to_string())?
            .map(|snapshots| snapshots.restore_turn(turn))
            .transpose()
            .map_err(|e| e.to_string())?;
        if result.is_some() {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.symbols.clear();
            *self.snapshots.lock().unwrap() = SnapshotState::default();
        }
        Ok(result)
    }

    pub fn begin_snapshot_turn(&self, session: &str, turn: usize) {
        if self.observed_generation != self.generation.load(Ordering::Acquire) {
            return;
        }
        *self.snapshots.lock().unwrap() = SnapshotState {
            session: session.to_owned(),
            turn,
            captured: false,
            repository: None,
            base: None,
        };
        *self
            .mutations
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Mutations::default();
    }

    pub fn prepare_snapshot(&self) -> Result<(), String> {
        let mut state = self.snapshots.lock().unwrap();
        if state.session.is_empty() {
            return Ok(());
        }
        if !state.captured {
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            state.repository =
                Snapshots::discover(&cwd, &state.session).map_err(|e| e.to_string())?;
            let base = if let Some(snapshots) = &state.repository {
                DiffBase {
                    reference: Some(snapshots.capture(state.turn).map_err(|e| e.to_string())?),
                    root: Some(snapshots.root().to_path_buf()),
                    attributes: Some(snapshots.attributes().to_owned()),
                }
            } else {
                DiffBase::default()
            };
            state.base = Some(Arc::new(base));
            state.captured = true;
        }
        if self.owner.is_some() {
            let mut mutations = self
                .mutations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if mutations.baseline.is_none() {
                mutations.baseline = state.base.clone();
            }
        }
        Ok(())
    }

    pub fn record_snapshot_creations(&self) -> Result<(), String> {
        let state = self.snapshots.lock().unwrap();
        if state.session.is_empty() || !state.captured {
            return Ok(());
        }
        if let Some(snapshots) = &state.repository {
            snapshots
                .record_created(state.turn)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// The write lock for one file, held for a tool's entire execution so its
    /// read-modify-write cannot interleave with another tool's on the same
    /// file. The dispatcher is the only caller: a tool that declares
    /// `mutable_path` gets this for free and must not re-implement it.
    ///
    /// A tool that declares `mutable_path` and also dispatches a child tool
    /// onto that same path would wait on itself. None does, since `batch`,
    /// `task` and `code_execution` declare no mutable path, and nothing
    /// enforces it statically. Hence the log line when a wait starts: it is
    /// the only trace such a hang would leave.
    ///
    /// One path only. A tool mutating several would need a globally sorted
    /// acquire order to avoid lock-order inversion, and none exists yet.
    pub async fn acquire(&self, key: &FileKey) -> FileGuard {
        let lock = self.lock_for(key);
        match lock.try_lock_arc() {
            Some(guard) => guard,
            None => {
                debug!(path = %key.as_path().display(), "waiting for the file lock");
                lock.lock_arc().await
            }
        }
    }

    /// The guard and every waiter own an `Arc` clone, so an entry lives
    /// exactly as long as it is in use. [`LOCK_PRUNE_AT`] only bounds the
    /// residue of dead `Weak`s left behind.
    fn lock_for(&self, key: &FileKey) -> FileLock {
        let mut map = self.locks.lock().unwrap();
        if map.len() >= LOCK_PRUNE_AT {
            map.retain(|_, weak| weak.strong_count() > 0);
        }
        map.get(key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let created = FileLock::default();
            map.insert(key.clone(), Arc::downgrade(&created));
            created
        })
    }

    pub fn record_read(&self, key: &FileKey) {
        let Some(mtime) = get_mtime(key.as_path()) else {
            warn!(
                path = %key.as_path().display(),
                "record_read: could not get mtime, file will not be tracked"
            );
            return;
        };
        let source = self.owner.as_ref().and_then(|_| {
            let file = File::open(key.as_path()).ok()?;
            let metadata = file.metadata().ok()?;
            if metadata.len() > MAX_READ_SOURCE_BYTES as u64 || metadata.modified().ok()? != mtime {
                return None;
            }
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            file.take(MAX_READ_SOURCE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > MAX_READ_SOURCE_BYTES || get_mtime(key.as_path()) != Some(mtime) {
                return None;
            }
            String::from_utf8(bytes).ok().map(Arc::<str>::from)
        });
        let mut reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
        reads.mtimes.insert(key.clone(), mtime);
        if let Some(previous) = reads.sources.remove(key) {
            reads.source_bytes -= previous.len();
        }
        if let Some(source) = source {
            if reads.source_bytes + source.len() > MAX_READ_SOURCE_BYTES {
                reads.sources.clear();
                reads.source_bytes = 0;
            }
            reads.source_bytes += source.len();
            reads.sources.insert(key.clone(), source);
        }
    }

    pub fn edit_snapshot(&self, key: &FileKey) -> Result<Option<Arc<str>>, String> {
        if self.owner.is_none() {
            return Ok(None);
        }
        self.check_active()?;
        let reads = self.reads.lock().unwrap_or_else(|e| e.into_inner());
        if reads.mtimes.get(key).copied() != get_mtime(key.as_path()) {
            return Ok(reads.sources.get(key).cloned());
        }
        Ok(None)
    }

    pub fn check_before_targeted_edit(
        &self,
        key: &FileKey,
        requires_snapshot: bool,
    ) -> Result<(), String> {
        self.check_active()?;
        if self.owner.is_some()
            && (!requires_snapshot
                || self
                    .reads
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .sources
                    .contains_key(key))
        {
            return Ok(());
        }
        self.check_before_edit(key)
    }

    pub fn check_before_edit(&self, key: &FileKey) -> Result<(), String> {
        let mut guard = self.reads.lock().unwrap_or_else(|e| e.into_inner());
        let Some(&recorded) = guard.mtimes.get(key) else {
            return Ok(());
        };
        let Some(current) = get_mtime(key.as_path()) else {
            guard.mtimes.remove(key);
            return Ok(());
        };
        if recorded != current {
            return Err(format!(
                "{STALE_READ_MSG}: {} - re-read using read tool before editing",
                key.as_path().display(),
            ));
        }
        Ok(())
    }
}

/// Spelling-independence belongs to [`FileKey`], which is the only way to key
/// every map, so it is tested once over `canonical_key` in `maki-storage`
/// rather than again for every map.
#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::Barrier;
    use std::thread;

    use futures_lite::future::poll_once;
    use test_case::test_case;

    use super::*;
    use crate::tools::test_support::future_mtime;

    const FILE: &str = "f.rs";
    const CONTENT: &str = "content";
    const FIRST_ID: &str = "task-a";
    const SECOND_ID: &str = "task-b";
    const FIRST_INTENT: &str = "update parser";
    const SECOND_INTENT: &str = "update renderer";
    const FIRST_SYMBOL: &str = "Parser::parse";
    const SECOND_SYMBOL: &str = "Renderer::render";
    const CHANGED: &str = "changed";
    const SESSION: &str = "symbol-rewind";

    fn symbols(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn disjoint_claims_coexist_and_multisymbol_conflict_is_atomic() {
        let main = FileAccess::default();
        let key = FileKey::new(Path::new(FILE));
        let (first, _first_lease) = main.task_scope(FIRST_ID, FIRST_INTENT);
        let (second, _second_lease) = main.task_scope(SECOND_ID, SECOND_INTENT);
        first
            .claim_symbols(&key, FIRST_ID, &symbols(&[FIRST_SYMBOL]))
            .unwrap();
        second
            .claim_symbols(&key, SECOND_ID, &symbols(&[SECOND_SYMBOL]))
            .unwrap();
        let error = second
            .claim_symbols(&key, SECOND_ID, &symbols(&["unused", FIRST_SYMBOL]))
            .unwrap_err();
        assert_eq!(
            error,
            format!(
                "Conflict: Subagent {FIRST_ID} ({FIRST_INTENT}) currently holds lock on symbol {FIRST_SYMBOL} in {}",
                key.as_path().display(),
            )
        );
        assert!(
            main.claim_symbols(&key, "main", &symbols(&["unused"]))
                .is_ok()
        );
        assert!(
            main.claim_symbols(&key, "main", &symbols(&[FIRST_SYMBOL]))
                .is_err()
        );
        let state = main.symbols.state.lock().unwrap();
        let held = &state.files[&key][FIRST_SYMBOL];
        assert!(held.acquired_at <= SystemTime::now());
    }

    #[test]
    fn simultaneous_claims_have_one_owner_and_no_partial_acquisition() {
        let main = FileAccess::default();
        let key = FileKey::new(Path::new(FILE));
        let (first, _first_lease) = main.task_scope(FIRST_ID, FIRST_INTENT);
        let (second, _second_lease) = main.task_scope(SECOND_ID, SECOND_INTENT);
        let barrier = Barrier::new(2);
        let claims = symbols(&[FIRST_SYMBOL, SECOND_SYMBOL]);
        let (first_result, second_result) = thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                first.claim_symbols(&key, FIRST_ID, &claims)
            });
            let second = scope.spawn(|| {
                barrier.wait();
                second.claim_symbols(&key, SECOND_ID, &claims)
            });
            (first.join().unwrap(), second.join().unwrap())
        });
        assert_ne!(first_result.is_ok(), second_result.is_ok());
        let state = main.symbols.state.lock().unwrap();
        let locks = &state.files[&key];
        assert_eq!(locks.len(), claims.len());
        assert_eq!(
            locks[FIRST_SYMBOL].holder_subagent_id,
            locks[SECOND_SYMBOL].holder_subagent_id
        );
    }

    #[test_case(FILE_SYMBOL, FIRST_SYMBOL ; "wildcard_holder")]
    #[test_case(FIRST_SYMBOL, FILE_SYMBOL ; "wildcard_request")]
    fn wildcard_claim_conflicts_in_both_directions(held: &str, requested: &str) {
        let main = FileAccess::default();
        let key = FileKey::new(Path::new(FILE));
        let (first, _first_lease) = main.task_scope(FIRST_ID, FIRST_INTENT);
        let (second, _second_lease) = main.task_scope(SECOND_ID, SECOND_INTENT);
        first
            .claim_symbols(&key, FIRST_ID, &symbols(&[held]))
            .unwrap();
        assert!(
            second
                .claim_symbols(&key, SECOND_ID, &symbols(&[requested]))
                .is_err()
        );
        assert!(
            first
                .claim_symbols(&key, FIRST_ID, &symbols(&[requested]))
                .is_ok()
        );
    }

    #[test_case(false ; "explicit_release")]
    #[test_case(true ; "drop_release")]
    fn lease_retirement_releases_claims_and_permanently_blocks_old_task(drop_lease: bool) {
        let main = FileAccess::default();
        let key = FileKey::new(Path::new(FILE));
        let (first, lease) = main.task_scope(FIRST_ID, FIRST_INTENT);
        let (second, _second_lease) = main.task_scope(SECOND_ID, SECOND_INTENT);
        first
            .claim_symbols(&key, FIRST_ID, &symbols(&[FIRST_SYMBOL]))
            .unwrap();
        if drop_lease {
            drop(lease);
        } else {
            lease.release();
            lease.release();
        }
        second
            .claim_symbols(&key, SECOND_ID, &symbols(&[FIRST_SYMBOL]))
            .unwrap();
        assert_eq!(
            first
                .claim_symbols(&key, FIRST_ID, &symbols(&[SECOND_SYMBOL]))
                .unwrap_err(),
            TASK_FINISHED_MSG
        );
        assert_eq!(
            smol::block_on(first.mutation_guard()).err().unwrap(),
            TASK_FINISHED_MSG
        );
    }

    #[test]
    fn retiring_one_registration_does_not_unlock_another_with_the_same_ui_id() {
        let main = FileAccess::default();
        let key = FileKey::new(Path::new(FILE));
        let (first, first_lease) = main.task_scope(FIRST_ID, FIRST_INTENT);
        let (second, _second_lease) = main.task_scope(FIRST_ID, SECOND_INTENT);
        first
            .claim_symbols(&key, FIRST_ID, &symbols(&[FIRST_SYMBOL]))
            .unwrap();
        second
            .claim_symbols(&key, FIRST_ID, &symbols(&[SECOND_SYMBOL]))
            .unwrap();
        first_lease.release();
        assert!(
            main.claim_symbols(&key, "main", &symbols(&[FIRST_SYMBOL]))
                .is_ok()
        );
        assert!(
            main.claim_symbols(&key, "main", &symbols(&[SECOND_SYMBOL]))
                .is_err()
        );
    }

    #[test]
    fn rewind_clears_claims_under_mutation_gate_and_retires_previous_generation() {
        let dir = tempfile::TempDir::new().unwrap();
        let initialized = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(initialized.success());
        let path = dir.path().join(FILE);
        fs::write(&path, CONTENT).unwrap();
        let key = FileKey::new(&path);
        let snapshots = Snapshots::discover(dir.path(), SESSION).unwrap().unwrap();
        snapshots.capture(0).unwrap();
        let main = FileAccess::default();
        let (task, _lease) = main.task_scope(FIRST_ID, FIRST_INTENT);
        task.claim_symbols(&key, FIRST_ID, &symbols(&[FIRST_SYMBOL]))
            .unwrap();
        fs::write(&path, CHANGED).unwrap();
        assert_eq!(
            main.restore_snapshot(dir.path(), SESSION, 0).unwrap(),
            Some(true)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), CONTENT);
        assert!(main.symbols.state.lock().unwrap().files.is_empty());
        assert_eq!(
            task.claim_symbols(&key, FIRST_ID, &symbols(&[SECOND_SYMBOL]))
                .unwrap_err(),
            REWOUND_MSG
        );
        assert_eq!(
            smol::block_on(task.mutation_guard()).err().unwrap(),
            REWOUND_MSG
        );
        let fresh = main.sharing_locks();
        let (new_task, _new_lease) = fresh.task_scope(SECOND_ID, SECOND_INTENT);
        new_task
            .claim_symbols(&key, SECOND_ID, &symbols(&[FIRST_SYMBOL]))
            .unwrap();
    }

    #[test]
    fn task_cached_baseline_allows_numbered_reconciliation_but_main_stays_strict() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(FILE);
        fs::write(&path, CONTENT).unwrap();
        let key = FileKey::new(&path);
        let main = FileAccess::default();
        let (task, _lease) = main.task_scope(FIRST_ID, FIRST_INTENT);
        main.record_read(&key);
        task.record_read(&key);
        assert!(task.edit_snapshot(&key).unwrap().is_none());
        fs::write(&path, CHANGED).unwrap();
        future_mtime(&path);
        assert!(main.check_before_targeted_edit(&key, false).is_err());
        assert!(main.edit_snapshot(&key).unwrap().is_none());
        assert!(task.check_before_edit(&key).is_err());
        assert!(task.check_before_targeted_edit(&key, true).is_ok());
        assert_eq!(task.edit_snapshot(&key).unwrap().as_deref(), Some(CONTENT));
        task.record_read(&key);
        assert!(task.edit_snapshot(&key).unwrap().is_none());
        let reads = task.reads.lock().unwrap();
        assert_eq!(reads.sources[&key].as_ref(), CHANGED);
        assert!(main.reads.lock().unwrap().sources.is_empty());
    }

    #[test_case(vec![0xff] ; "non_utf8")]
    #[test_case(vec![b'x'; MAX_READ_SOURCE_BYTES + 1] ; "oversized")]
    fn uncached_task_baseline_allows_exact_edits_only(content: Vec<u8>) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(FILE);
        fs::write(&path, content).unwrap();
        let key = FileKey::new(&path);
        let (task, _lease) = FileAccess::default().task_scope(FIRST_ID, FIRST_INTENT);
        task.record_read(&key);
        future_mtime(&path);
        assert!(task.edit_snapshot(&key).unwrap().is_none());
        assert!(task.check_before_targeted_edit(&key, false).is_ok());
        assert!(task.check_before_targeted_edit(&key, true).is_err());
    }

    #[test]
    fn task_source_cache_is_bounded_across_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let (task, _lease) = FileAccess::default().task_scope(FIRST_ID, FIRST_INTENT);
        let content = vec![b'x'; MAX_READ_SOURCE_BYTES / 2 + 1];
        for name in [FILE, "other.rs"] {
            let path = dir.path().join(name);
            fs::write(&path, &content).unwrap();
            task.record_read(&FileKey::new(&path));
        }
        let reads = task.reads.lock().unwrap();
        assert_eq!(reads.source_bytes, content.len());
        assert_eq!(reads.sources.len(), 1);
        assert_eq!(reads.mtimes.len(), 2);
    }

    #[test]
    fn symbol_table_recovers_poisoned_mutex_without_losing_release_semantics() {
        let main = FileAccess::default();
        let key = FileKey::new(Path::new(FILE));
        let (task, lease) = main.task_scope(FIRST_ID, FIRST_INTENT);
        let table = Arc::clone(&main.symbols);
        assert!(
            thread::spawn(move || {
                let _state = table.state.lock().unwrap();
                panic!("poison symbol table");
            })
            .join()
            .is_err()
        );
        task.claim_symbols(&key, FIRST_ID, &symbols(&[FIRST_SYMBOL]))
            .unwrap();
        assert!(
            main.claim_symbols(&key, "main", &symbols(&[FIRST_SYMBOL]))
                .is_err()
        );
        lease.release();
        assert!(
            main.claim_symbols(&key, "main", &symbols(&[FIRST_SYMBOL]))
                .is_ok()
        );
    }

    /// `meanwhile` stands in for everything that can happen between a read and
    /// the edit that follows it. Only a file we read and someone else changed
    /// may be refused, since anything else would block an edit for no reason.
    #[test_case(|_, _, _| {} => true ; "never_read")]
    #[test_case(|access, key, _| access.record_read(key) => true ; "read_and_untouched")]
    #[test_case(|access, key, path| {
        access.record_read(key);
        future_mtime(path);
    } => false ; "changed_after_the_read")]
    #[test_case(|access, key, path| {
        access.record_read(key);
        future_mtime(path);
        access.record_read(key);
    } => true ; "changed_then_read_again")]
    #[test_case(|access, key, path| {
        access.record_read(key);
        fs::remove_file(path).unwrap();
    } => true ; "deleted_after_the_read")]
    #[test_case(|access, key, path| {
        fs::remove_file(path).unwrap();
        access.record_read(key);
    } => true ; "read_of_a_file_that_is_gone")]
    fn edit_is_allowed_unless_the_file_moved_under_us(
        meanwhile: fn(&FileAccess, &FileKey, &Path),
    ) -> bool {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(FILE);
        fs::write(&path, CONTENT).unwrap();
        let key = FileKey::new(&path);

        let access = FileAccess::default();
        meanwhile(&access, &key, &path);
        match access.check_before_edit(&key) {
            Ok(()) => true,
            Err(message) => {
                assert!(message.contains(STALE_READ_MSG), "{message}");
                false
            }
        }
    }

    #[test]
    fn one_lock_per_file_held_until_the_guard_drops() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = FileKey::new(&dir.path().join(FILE));
        let other = FileKey::new(&dir.path().join("other.rs"));

        let access = FileAccess::default();
        smol::block_on(async {
            let held = access.acquire(&path).await;
            assert!(
                poll_once(access.acquire(&path)).await.is_none(),
                "the same file must not be lockable twice"
            );
            assert!(
                poll_once(access.acquire(&other)).await.is_some(),
                "another file must not wait on this one"
            );
            drop(held);
            assert!(poll_once(access.acquire(&path)).await.is_some());
        });
    }

    #[test]
    fn dead_locks_do_not_accumulate() {
        let dir = tempfile::TempDir::new().unwrap();
        let access = FileAccess::default();
        smol::block_on(async {
            for i in 0..=LOCK_PRUNE_AT {
                let key = FileKey::new(&dir.path().join(format!("{i}.rs")));
                drop(access.acquire(&key).await);
            }
        });
        assert!(access.locks.lock().unwrap().len() <= LOCK_PRUNE_AT);
    }
}
