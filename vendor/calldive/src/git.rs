use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread::JoinHandle;

pub const BINARY_SOURCE_PREFIX: &str = "Binary file: ";

/// Thin wrapper over git plumbing, executed in the repo working dir.
pub struct GitRepo {
    pub root: PathBuf,
}

/// A file that differs between base and head, identified by its head path.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>,
    /// A = added, M = modified, D = deleted, R = renamed, C = copied
    pub status: char,
}

pub struct Blob {
    pub path: String,
    pub oid: String,
    pub size: usize,
    pub source: Option<String>,
}

impl GitRepo {
    pub fn open(path: &Path) -> Result<Self> {
        let out = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("failed to run git (is git installed?)")?;
        if !out.status.success() {
            bail!(
                "not a git repository: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let root =
            PathBuf::from(String::from_utf8_lossy(&out.stdout).trim_end_matches(['\r', '\n']));
        Ok(Self { root })
    }

    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .with_context(|| format!("git {:?} failed to spawn", args))?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    /// Resolve once, to an unambiguous commit object rather than a user ref.
    pub fn resolve_commit(&self, rev: &str) -> Result<String> {
        let spec = format!("{rev}^{{commit}}");
        let out = self.run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ])?;
        let sha = String::from_utf8(out).context("git returned a non-UTF-8 commit ID")?;
        let sha = sha.trim();
        if sha.is_empty() || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("git returned an invalid commit ID for ref '{rev}'");
        }
        Ok(sha.to_string())
    }

    pub fn resolve_base(&self, rev: &str) -> Result<String> {
        match self.resolve_commit(rev) {
            Ok(sha) => Ok(sha),
            Err(_) if rev == "HEAD" => {
                let output = self.run(&["hash-object", "-t", "tree", "-w", "--stdin"])?;
                Ok(String::from_utf8(output)
                    .context("git returned an invalid empty tree ID")?
                    .trim()
                    .to_string())
            }
            Err(error) => Err(error).with_context(|| format!("base ref '{rev}' does not resolve")),
        }
    }

    /// Git resolves this separately for each linked worktree.
    pub fn cache_dir(&self) -> Result<PathBuf> {
        let out = self.run(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "calldive",
        ])?;
        let path = String::from_utf8(out).context("git returned a non-UTF-8 cache path")?;
        Ok(PathBuf::from(path.trim_end_matches(['\r', '\n'])))
    }

    /// Includes staged, unstaged, and untracked files for a worktree head.
    /// Commit arguments must already be resolved to SHAs.
    pub fn changed_files(&self, base: &str, head: Option<&str>) -> Result<Vec<FileChange>> {
        let mut args = vec![
            "diff",
            "--name-status",
            "-z",
            "-M",
            "-C",
            "--find-copies-harder",
            "--end-of-options",
            base,
        ];
        if let Some(head) = head {
            args.push(head);
        }
        args.push("--");
        let out = self.run(&args)?;
        let records = nul_records(&out)?;
        let mut records = records.into_iter();
        let mut files = Vec::new();
        while let Some(status) = records.next() {
            let status = status.first().copied().context("empty git diff status")? as char;
            let first = records.next().context("git diff omitted a file path")?;
            let (path, old_path) = if matches!(status, 'R' | 'C') {
                let new = records
                    .next()
                    .context("git diff omitted a rename/copy destination")?;
                (decode_path(new), Some(decode_path(first)))
            } else {
                (decode_path(first), None)
            };
            files.push(FileChange {
                path,
                old_path,
                status,
            });
        }
        if head.is_none() {
            let mut paths: HashSet<String> = files.iter().map(|file| file.path.clone()).collect();
            for path in self.untracked_files()? {
                if let Some(deleted) = files
                    .iter_mut()
                    .find(|file| file.path == path && file.status == 'D')
                {
                    deleted.status = 'M';
                }
                if paths.insert(path.clone()) {
                    files.push(FileChange {
                        path,
                        old_path: None,
                        status: 'A',
                    });
                }
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    pub fn read_worktree(&self, path: &str) -> Result<String> {
        let full_path = self.root.join(path);
        let metadata = fs::symlink_metadata(&full_path)
            .with_context(|| format!("reading worktree metadata {}", full_path.display()))?;
        if metadata.file_type().is_symlink() {
            return Ok(fs::read_link(&full_path)?.to_string_lossy().into_owned());
        }
        if metadata.is_dir() {
            let out = Command::new("git")
                .arg("-C")
                .arg(&full_path)
                .args(["rev-parse", "--verify", "HEAD"])
                .output()?;
            if out.status.success() {
                return Ok(format!(
                    "Git submodule commit {}\n",
                    String::from_utf8_lossy(&out.stdout).trim()
                ));
            }
            return Ok("Git submodule (unavailable worktree commit)\n".to_string());
        }
        let bytes = fs::read(&full_path)
            .with_context(|| format!("reading worktree file {}", full_path.display()))?;
        Ok(decode_source(bytes))
    }

    pub fn tracked_files(&self) -> Result<Vec<String>> {
        let out = self.run(&["ls-files", "-z", "--cached", "--"])?;
        Ok(nul_records(&out)?.into_iter().map(decode_path).collect())
    }

    fn untracked_files(&self) -> Result<Vec<String>> {
        let out = self.run(&["ls-files", "-z", "--others", "--exclude-standard", "--"])?;
        Ok(nul_records(&out)?.into_iter().map(decode_path).collect())
    }

    /// Blob IDs avoid the line-oriented batch protocol's restrictions on paths.
    /// Sizes let callers exclude oversized unchanged files before reading them.
    pub fn blobs_at(&self, sha: &str) -> Result<Vec<Blob>> {
        let out = self.run(&["ls-tree", "-r", "-z", "-l", "--end-of-options", sha])?;
        let mut blobs = Vec::new();
        for record in nul_records(&out)? {
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .context("git ls-tree omitted the path separator")?;
            let metadata = std::str::from_utf8(&record[..tab])
                .context("git ls-tree returned invalid metadata")?;
            let fields: Vec<&str> = metadata.split_ascii_whitespace().collect();
            if fields.len() != 4 {
                bail!("git ls-tree returned invalid metadata: {metadata}");
            }
            if !matches!(fields[1], "blob" | "commit") {
                continue;
            }
            let submodule = fields[1] == "commit";
            blobs.push(Blob {
                path: decode_path(&record[tab + 1..]),
                oid: fields[2].to_string(),
                size: if submodule {
                    0
                } else {
                    fields[3]
                        .parse()
                        .context("git ls-tree returned an invalid blob size")?
                },
                source: submodule.then(|| format!("Git submodule commit {}\n", fields[2])),
            });
        }
        Ok(blobs)
    }

    /// Keep one batch process alive for both changed sources and caller candidates.
    pub fn with_blobs<T>(&self, read: impl FnOnce(&mut BlobReader) -> Result<T>) -> Result<T> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("git cat-file --batch failed to spawn")?;
        let stdin = child
            .stdin
            .take()
            .context("git cat-file stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("git cat-file stdout unavailable")?;
        let mut stderr = child
            .stderr
            .take()
            .context("git cat-file stderr unavailable")?;
        let stderr = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).map(|_| bytes)
        });
        let mut reader = BlobReader {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr: Some(stderr),
        };
        let result = read(&mut reader);
        reader.stdin.take();
        if result.is_err() {
            let _ = reader.child.kill();
        }
        let status = reader
            .child
            .wait()
            .context("waiting for git cat-file --batch")?;
        let stderr = reader
            .stderr
            .take()
            .context("git cat-file stderr reader unavailable")?
            .join()
            .map_err(|_| anyhow::anyhow!("git cat-file stderr reader panicked"))?
            .context("reading git cat-file stderr")?;
        let value = result?;
        if !status.success() {
            bail!(
                "git cat-file --batch failed: {}",
                String::from_utf8_lossy(&stderr).trim()
            );
        }
        Ok(value)
    }
}

pub struct BlobReader {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr: Option<JoinHandle<std::io::Result<Vec<u8>>>>,
}

impl BlobReader {
    /// Write requests concurrently with reads so neither pipe can fill and deadlock.
    /// Missing objects are reported explicitly to the caller, never as empty text.
    pub fn read(
        &mut self,
        oids: &[&str],
        mut receive: impl FnMut(usize, Option<String>) -> Result<()>,
    ) -> Result<()> {
        if oids.is_empty() {
            return Ok(());
        }
        let stdin = self
            .stdin
            .as_mut()
            .context("git cat-file input is closed")?;
        let stdout = &mut self.stdout;
        let child = &mut self.child;
        std::thread::scope(|scope| {
            let writer = scope.spawn(move || -> std::io::Result<()> {
                for oid in oids {
                    writeln!(stdin, "{oid}")?;
                }
                stdin.flush()
            });
            let result = (|| -> Result<()> {
                let mut header = String::new();
                for (index, expected_oid) in oids.iter().enumerate() {
                    header.clear();
                    if stdout
                        .read_line(&mut header)
                        .context("reading git cat-file header")?
                        == 0
                    {
                        bail!("git cat-file ended before blob {expected_oid}");
                    }
                    let fields: Vec<&str> = header.split_ascii_whitespace().collect();
                    if fields.len() == 2 && fields[1] == "missing" {
                        receive(index, None)?;
                        continue;
                    }
                    if fields.len() != 3 || fields[0] != *expected_oid || fields[1] != "blob" {
                        bail!("unexpected git cat-file response: {}", header.trim_end());
                    }
                    let size: usize = fields[2]
                        .parse()
                        .context("invalid git cat-file blob size")?;
                    let mut bytes = vec![0; size];
                    stdout
                        .read_exact(&mut bytes)
                        .with_context(|| format!("reading git blob {expected_oid}"))?;
                    let mut terminator = [0];
                    stdout
                        .read_exact(&mut terminator)
                        .context("reading git cat-file separator")?;
                    if terminator != *b"\n" {
                        bail!("invalid git cat-file separator for blob {expected_oid}");
                    }
                    receive(index, Some(decode_source(bytes)))?;
                }
                Ok(())
            })();
            if result.is_err() {
                let _ = child.kill();
            }
            let written = writer
                .join()
                .map_err(|_| anyhow::anyhow!("git cat-file writer panicked"))?;
            result?;
            written.context("writing git cat-file requests")
        })
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

fn nul_records(bytes: &[u8]) -> Result<Vec<&[u8]>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.last() != Some(&0) {
        bail!("git returned an unterminated NUL-delimited record");
    }
    Ok(bytes[..bytes.len() - 1].split(|b| *b == 0).collect())
}

fn decode_path(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub fn decode_source(bytes: Vec<u8>) -> String {
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        return format!(
            "{BINARY_SOURCE_PREFIX}{} bytes (blake3 {})\n",
            bytes.len(),
            blake3::hash(&bytes)
        );
    }
    let mut source = match String::from_utf8(bytes) {
        Ok(source) => source,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    };
    if source.starts_with('\u{feff}') {
        source.drain(..3);
    }
    source
}

/// Sentinel head meaning "the working tree as it is on disk".
pub const WORKTREE: &str = "worktree";

pub fn is_worktree(s: &str) -> bool {
    s.eq_ignore_ascii_case(WORKTREE)
}

pub fn resolve_rev(repo: &GitRepo, rev: &str) -> Result<Option<String>> {
    if is_worktree(rev) {
        return Ok(None);
    }
    repo.resolve_commit(rev)
        .with_context(|| format!("ref '{rev}' does not resolve in this repository"))
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_preserve_utf8_strip_bom_and_summarize_binary() {
        assert_eq!(
            decode_source(b"\xef\xbb\xbffn run() {}".to_vec()),
            "fn run() {}"
        );
        assert!(
            decode_source(b"// caf\xe9\nfn run() {}".to_vec()).starts_with(BINARY_SOURCE_PREFIX)
        );
        assert_eq!(decode_source(b"x\xef\xbb\xbf".to_vec()), "x\u{feff}");
    }
}
