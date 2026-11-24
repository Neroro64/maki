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
    pub base_oid: Option<String>,
    pub base_submodule: bool,
    pub head_oid: Option<String>,
    pub head_submodule: bool,
}

pub struct Blob {
    pub path: String,
    pub oid: String,
    pub size: usize,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevCandidate {
    pub rev: String,
    pub detail: String,
    pub is_worktree: bool,
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
            "logidiff",
        ])?;
        let path = String::from_utf8(out).context("git returned a non-UTF-8 cache path")?;
        Ok(PathBuf::from(path.trim_end_matches(['\r', '\n'])))
    }

    /// Includes staged, unstaged, and untracked files for a worktree head.
    /// Commit arguments must already be resolved to SHAs.
    pub fn changed_files(&self, base: &str, head: Option<&str>) -> Result<Vec<FileChange>> {
        let mut args = vec![
            "diff",
            "--raw",
            "--no-abbrev",
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
        while let Some(meta) = records.next() {
            let meta_str = std::str::from_utf8(meta).context("invalid git diff --raw metadata")?;
            let meta_fields: Vec<&str> = meta_str
                .trim_start_matches(':')
                .split_ascii_whitespace()
                .collect();
            if meta_fields.len() < 5 {
                bail!("unexpected git diff --raw metadata: {meta_str}");
            }
            let base_mode = meta_fields[0];
            let head_mode = meta_fields[1];
            let base_oid_raw = meta_fields[2];
            let head_oid_raw = meta_fields[3];
            let status = meta_fields[4]
                .bytes()
                .next()
                .context("empty git diff status")? as char;
            let first = records.next().context("git diff omitted a file path")?;
            let (path, old_path) = if matches!(status, 'R' | 'C') {
                let new = records
                    .next()
                    .context("git diff omitted a rename/copy destination")?;
                (decode_path(new), Some(decode_path(first)))
            } else {
                (decode_path(first), None)
            };
            let non_zero = |oid: &str| !oid.bytes().all(|b| b == b'0');
            files.push(FileChange {
                path,
                old_path,
                status,
                base_oid: non_zero(base_oid_raw).then(|| base_oid_raw.to_string()),
                base_submodule: base_mode == "160000",
                head_oid: (head.is_some() && non_zero(head_oid_raw))
                    .then(|| head_oid_raw.to_string()),
                head_submodule: head_mode == "160000",
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
                        base_oid: None,
                        base_submodule: false,
                        head_oid: None,
                        head_submodule: false,
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

    pub fn untracked_files(&self) -> Result<Vec<String>> {
        let out = self.run(&["ls-files", "-z", "--others", "--exclude-standard", "--"])?;
        Ok(nul_records(&out)?.into_iter().map(decode_path).collect())
    }

    /// Dirty tracked paths relative to HEAD (staged or unstaged).
    pub fn dirty_tracked_files(&self) -> Result<Vec<String>> {
        let Ok(head) = self.resolve_commit("HEAD") else {
            return self.tracked_files();
        };
        let out = self.run(&[
            "diff-index",
            "-z",
            "--name-only",
            "--end-of-options",
            &head,
            "--",
        ])?;
        Ok(nul_records(&out)?.into_iter().map(decode_path).collect())
    }

    /// Fast candidate file prefilter via `git grep` at `rev` (or worktree when `None`).
    /// Returns matching repository-relative paths, or `None` if `git grep` cannot run.
    pub fn grep_candidate_files(
        &self,
        rev: Option<&str>,
        terms: &[String],
        pathspecs: &[&str],
    ) -> Result<Option<HashSet<String>>> {
        if terms.is_empty() {
            return Ok(Some(HashSet::new()));
        }
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.root).args([
            "-c",
            "core.quotepath=false",
            "grep",
            "--full-name",
            "-l",
            "-z",
            "-w",
            "-F",
            "-f",
            "-",
        ]);
        if let Some(rev) = rev {
            cmd.arg(rev);
        }
        cmd.arg("--");
        cmd.args(pathspecs);
        let mut child = match cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return Ok(None),
        };
        let stdin = child.stdin.take().context("git grep stdin unavailable")?;
        let mut stdout = child.stdout.take().context("git grep stdout unavailable")?;
        let mut stderr = child.stderr.take().context("git grep stderr unavailable")?;
        let (out, _err, status) = std::thread::scope(
            |scope| -> Result<(Vec<u8>, Vec<u8>, std::process::ExitStatus)> {
                let writer = scope.spawn(move || -> std::io::Result<()> {
                    let mut stdin = stdin;
                    for term in terms {
                        writeln!(stdin, "{term}")?;
                    }
                    stdin.flush()
                });
                let err_reader = scope.spawn(move || -> std::io::Result<Vec<u8>> {
                    let mut buf = Vec::new();
                    stderr.read_to_end(&mut buf)?;
                    Ok(buf)
                });
                let mut out_buf = Vec::new();
                stdout.read_to_end(&mut out_buf)?;
                let _ = writer.join();
                let err_buf = err_reader
                    .join()
                    .map_err(|_| anyhow::anyhow!("git grep stderr reader panicked"))??;
                let status = child.wait()?;
                Ok((out_buf, err_buf, status))
            },
        )?;
        match status.code() {
            Some(0) => {}
            Some(1) => return Ok(Some(HashSet::new())),
            _ => return Ok(None),
        }
        let prefix = rev.map(|r| format!("{r}:"));
        let mut paths = HashSet::new();
        for record in nul_records(&out)? {
            let decoded = decode_path(record);
            let path = match &prefix {
                Some(p) => decoded
                    .strip_prefix(p.as_str())
                    .unwrap_or(&decoded)
                    .to_string(),
                None => decoded,
            };
            paths.insert(path);
        }
        Ok(Some(paths))
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

    /// Discover revision candidates for the interactive TUI selector:
    /// worktree, HEAD, HEAD~N, local/remote branches, and tags.
    pub fn rev_candidates(&self) -> Result<Vec<RevCandidate>> {
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();

        let worktree_detail = match self.resolve_commit("HEAD") {
            Ok(head_sha) => match self.changed_files(&head_sha, None) {
                Ok(changes) if changes.is_empty() => "clean working tree".to_string(),
                Ok(changes) => format!(
                    "{} changed file{}",
                    changes.len(),
                    if changes.len() == 1 { "" } else { "s" }
                ),
                Err(_) => "working tree on disk".to_string(),
            },
            Err(_) => "working tree on disk".to_string(),
        };
        seen.insert(WORKTREE.to_string());
        candidates.push(RevCandidate {
            rev: WORKTREE.to_string(),
            detail: worktree_detail,
            is_worktree: true,
        });

        if let Ok(out) = self.run(&["log", "-n", "10", "--format=%h%x09%s"]) {
            for (idx, line) in String::from_utf8_lossy(&out).lines().enumerate() {
                let rev = if idx == 0 {
                    "HEAD".to_string()
                } else {
                    format!("HEAD~{idx}")
                };
                if seen.insert(rev.clone()) {
                    let detail = match line.split_once('\t') {
                        Some((sha, subject)) if !subject.is_empty() => format!("{sha} {subject}"),
                        Some((sha, _)) => sha.to_string(),
                        None => line.trim().to_string(),
                    };
                    candidates.push(RevCandidate {
                        rev,
                        detail,
                        is_worktree: false,
                    });
                }
            }
        }

        if let Ok(out) = self.run(&[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)\t%(refname:short)\t%(symref)\t%(objectname:short)\t%(subject)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ]) {
            for line in String::from_utf8_lossy(&out).lines() {
                let parts: Vec<&str> = line.splitn(5, '\t').collect();
                if parts.len() < 5 {
                    continue;
                }
                let (_full, short, symref, sha, subject) =
                    (parts[0], parts[1], parts[2], parts[3], parts[4]);
                if !symref.is_empty() || short.is_empty() {
                    continue;
                }
                if seen.insert(short.to_string()) {
                    let detail = if subject.is_empty() {
                        sha.to_string()
                    } else {
                        format!("{sha} {subject}")
                    };
                    candidates.push(RevCandidate {
                        rev: short.to_string(),
                        detail,
                        is_worktree: false,
                    });
                }
            }
        }

        Ok(candidates)
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
    if bytes.contains(&0) {
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
    fn sources_decode_lossily_strip_bom_and_summarize_nul_binary() {
        assert_eq!(
            decode_source(b"\xef\xbb\xbffn run() {}".to_vec()),
            "fn run() {}"
        );
        assert_eq!(
            decode_source(b"// caf\xe9\nfn run() {}".to_vec()),
            "// caf\u{fffd}\nfn run() {}"
        );
        assert!(decode_source(vec![0, 1, 2]).starts_with(BINARY_SOURCE_PREFIX));
        assert_eq!(decode_source(b"x\xef\xbb\xbf".to_vec()), "x\u{feff}");
    }
}
