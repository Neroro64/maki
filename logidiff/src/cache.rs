//! Cache the complete review model, including revision source text.

use crate::{analysis::Analysis, git};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

// Changing the executable also invalidates entries, including local rebuilds.
const FORMAT: &[u8] = b"logidiff-analysis-v4";
const MAX_ENTRIES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheStatus {
    Hit,
    Miss,
}

pub struct Cache {
    directory: PathBuf,
    path: PathBuf,
    key: String,
}

#[derive(Deserialize)]
struct Entry {
    key: String,
    analysis: Analysis,
}

#[derive(Serialize)]
struct EntryRef<'a> {
    key: &'a str,
    analysis: &'a Analysis,
}

impl Cache {
    pub fn new(
        repo: &git::GitRepo,
        changes: &[git::FileChange],
        base: &str,
        head: Option<&str>,
        include_tests: bool,
    ) -> Result<Self> {
        let mut hash = blake3::Hasher::new();
        field(&mut hash, FORMAT);
        // Include resolved commit IDs of the compared pair and current HEAD so
        // moving HEAD or changing either revision's commit ID invalidates entries.
        let current_head = repo.resolve_commit("HEAD").unwrap_or_default();
        field(&mut hash, current_head.as_bytes());
        field(&mut hash, base.as_bytes());
        field(&mut hash, head.unwrap_or(git::WORKTREE).as_bytes());
        field(&mut hash, &[u8::from(include_tests)]);
        field(&mut hash, executable_digest()?);
        for change in changes {
            field(&mut hash, &[change.status as u8]);
            field(&mut hash, change.path.as_bytes());
            field(
                &mut hash,
                change.old_path.as_deref().unwrap_or("").as_bytes(),
            );
        }
        if head.is_none() {
            // `current_head` already locks clean tracked files at HEAD. Hash only
            // changed, dirty-tracked, and untracked worktree files so cache hits
            // do not read thousands of clean tracked files from disk.
            let mut paths = repo.dirty_tracked_files()?;
            paths.extend(repo.untracked_files()?);
            paths.extend(changes.iter().map(|change| change.path.clone()));
            paths.sort();
            paths.dedup();
            for path in paths {
                field(&mut hash, path.as_bytes());
                match repo.read_worktree(&path) {
                    Ok(source) => {
                        field(&mut hash, b"present");
                        field(&mut hash, blake3::hash(source.as_bytes()).as_bytes());
                    }
                    Err(error)
                        if error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                    {
                        field(&mut hash, b"missing");
                    }
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("hashing worktree source '{path}'"));
                    }
                }
            }
        }
        let key = hash.finalize().to_hex().to_string();
        let directory = repo.cache_dir()?;
        let path = directory.join(format!("analysis-{}.json", &key[..32]));
        Ok(Self {
            directory,
            path,
            key,
        })
    }

    pub fn load(&self) -> Result<Option<Analysis>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("opening analysis cache"),
        };
        let entry: Entry =
            serde_json::from_reader(BufReader::new(file)).context("reading analysis cache")?;
        Ok((entry.key == self.key).then_some(entry.analysis))
    }

    /// Do not cache a model if files changed while it was being computed.
    pub fn same_snapshot(&self, other: &Self) -> bool {
        self.key == other.key
    }

    pub fn store(&self, analysis: &Analysis) -> Result<()> {
        fs::create_dir_all(&self.directory).context("creating analysis cache directory")?;
        let temporary = self.directory.join(format!(
            "analysis-{}-{}.tmp",
            &self.key[..16],
            std::process::id()
        ));
        let result = (|| -> Result<()> {
            let mut writer = BufWriter::new(File::create(&temporary)?);
            serde_json::to_writer(
                &mut writer,
                &EntryRef {
                    key: &self.key,
                    analysis,
                },
            )?;
            writer.flush()?;
            drop(writer);
            if self.path.exists() {
                let _ = fs::remove_file(&self.path);
            }
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        self.prune_stale();
        result.context("writing analysis cache")
    }

    fn prune_stale(&self) {
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return;
        };
        let mut cached_files = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name == "analysis.json" {
                let _ = fs::remove_file(&path);
                continue;
            }
            if name.starts_with("analysis-") && name.ends_with(".json") && path != self.path {
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                cached_files.push((modified, path));
            }
        }
        if cached_files.len() < MAX_ENTRIES {
            return;
        }
        cached_files.sort_by_key(|(modified, _)| *modified);
        let excess = cached_files.len() + 1 - MAX_ENTRIES;
        for (_, path) in cached_files.into_iter().take(excess) {
            let _ = fs::remove_file(path);
        }
    }
}

fn executable_digest() -> Result<&'static [u8; 32]> {
    static DIGEST: OnceLock<[u8; 32]> = OnceLock::new();
    if let Some(digest) = DIGEST.get() {
        return Ok(digest);
    }
    let executable = std::env::current_exe().context("locating logidiff executable")?;
    let mut binary = File::open(executable).context("reading logidiff executable")?;
    let mut executable_hash = blake3::Hasher::new();
    executable_hash.update_reader(&mut binary)?;
    let digest = *executable_hash.finalize().as_bytes();
    let _ = DIGEST.set(digest);
    Ok(DIGEST.get().expect("executable digest initialized"))
}

fn field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}
