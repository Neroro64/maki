//! Cache the complete review model, including revision source text.

use crate::{analysis::Analysis, git, syntax};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;

// Changing the executable also invalidates entries, including local rebuilds.
const FORMAT: &[u8] = b"calldive-analysis-v1";

pub struct Cache {
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
        field(&mut hash, base.as_bytes());
        field(&mut hash, head.unwrap_or(git::WORKTREE).as_bytes());
        field(&mut hash, &[u8::from(include_tests)]);
        let executable = std::env::current_exe().context("locating calldive executable")?;
        let mut binary = File::open(executable).context("reading calldive executable")?;
        // Fixed-size digest separates executable bytes from the other fields.
        let mut executable_hash = blake3::Hasher::new();
        executable_hash.update_reader(&mut binary)?;
        field(&mut hash, executable_hash.finalize().as_bytes());
        for change in changes {
            field(&mut hash, &[change.status as u8]);
            field(&mut hash, change.path.as_bytes());
            field(&mut hash, change.old_path.as_deref().unwrap_or("").as_bytes());
        }
        if head.is_none() {
            // Include unchanged callers as well as edited and untracked sources.
            // Hash contents, not timestamps: same-size edits must invalidate too.
            let mut paths = repo.tracked_files()?;
            paths.extend(changes.iter().map(|change| change.path.clone()));
            paths.retain(|path| syntax::lang_for_path(path).is_some());
            paths.sort();
            paths.dedup();
            for path in paths {
                field(&mut hash, path.as_bytes());
                match File::open(repo.root.join(&path)) {
                    Ok(mut file) => {
                        field(&mut hash, b"present");
                        let mut content = blake3::Hasher::new();
                        content.update_reader(&mut file)
                            .with_context(|| format!("hashing worktree source '{path}'"))?;
                        field(&mut hash, content.finalize().as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        field(&mut hash, b"missing");
                    }
                    Err(error) => return Err(error).with_context(|| format!("opening worktree source '{path}'")),
                }
            }
        }
        Ok(Self {
            path: repo.cache_dir()?.join("analysis.json"),
            key: hash.finalize().to_hex().to_string(),
        })
    }

    pub fn load(&self) -> Result<Option<Analysis>> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("opening analysis cache"),
        };
        let entry: Entry = serde_json::from_reader(BufReader::new(file))
            .context("reading analysis cache")?;
        Ok((entry.key == self.key).then_some(entry.analysis))
    }

    /// Do not cache a model if files changed while it was being computed.
    pub fn same_snapshot(&self, other: &Self) -> bool {
        self.key == other.key
    }

    pub fn store(&self, analysis: &Analysis) -> Result<()> {
        let directory = self.path.parent().context("cache directory missing")?;
        fs::create_dir_all(directory).context("creating analysis cache directory")?;
        let temporary = directory.join(format!("analysis-{}.tmp", std::process::id()));
        let result = (|| -> Result<()> {
            let mut writer = BufWriter::new(File::create(&temporary)?);
            serde_json::to_writer(&mut writer, &EntryRef { key: &self.key, analysis })?;
            writer.flush()?;
            drop(writer);
            fs::rename(&temporary, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result.context("writing analysis cache")
    }
}

fn field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}
