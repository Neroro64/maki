//! Git checkpoints of a working tree, kept in a git dir of our own.
//!
//! The repository lives entirely under the state dir and only ever points its
//! work tree at the project, so the user's own `.git` is never read, written,
//! or moved. A checkpoint is one commit of the whole tree; a rewind restores
//! the commit that matches the history it is truncating to.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde::{Deserialize, Serialize};

/// Where every session's checkpoints live under the state dir.
pub const CHECKPOINTS_DIR: &str = "checkpoints";
const GIT_SUBDIR: &str = "git";
const INDEX_FILE: &str = "index";
/// `core.hooksPath` points here and nothing is ever written to it, so no hook
/// from any repository can run during a checkpoint.
const HOOKS_SUBDIR: &str = "hooks-empty";
const COMMIT_MESSAGE: &str = "maki checkpoint";
const AUTHOR_NAME: &str = "maki";
const AUTHOR_EMAIL: &str = "checkpoint@maki.local";
/// `ext::` hands its argument to a shell. A `-c` outranks every config file,
/// so a hostile repo config cannot re-enable it.
const EXT_TRANSPORT_DENIED: &str = "protocol.ext.allow=never";

/// One commit of the working tree, taken when the turn ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCheckpoint {
    pub messages_len: usize,
    pub sha: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("git {command} failed: {stderr}")]
    Git { command: String, stderr: String },
    #[error("checkpoint not found: {0}")]
    NotFound(String),
}

/// The checkpoint repository for one session.
pub struct Checkpoints {
    git_dir: PathBuf,
    work_tree: PathBuf,
    index_file: PathBuf,
}

/// The per-session directory checkpoints are kept in.
pub fn session_subdir(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir.join(CHECKPOINTS_DIR).join(session_id)
}

impl Checkpoints {
    pub fn new(state_subdir: &Path, work_tree: &Path) -> Self {
        Self {
            git_dir: state_subdir.join(GIT_SUBDIR),
            work_tree: work_tree.to_path_buf(),
            index_file: state_subdir.join(INDEX_FILE),
        }
    }

    /// Commit the whole work tree. `None` when it already matches the last
    /// commit, so a turn that touched no file costs nothing and the caller
    /// keeps the sha it already has.
    pub fn commit(&mut self, messages_len: usize) -> Result<Option<String>, CheckpointError> {
        self.init()?;
        if self.head()?.is_some() && self.run(&["status", "--porcelain"])?.stdout.is_empty() {
            return Ok(None);
        }
        self.run(&["add", "-A"])?;
        self.run(&[
            "commit",
            "--allow-empty",
            "-m",
            &format!("{COMMIT_MESSAGE} {messages_len}"),
        ])?;
        self.head()?
            .ok_or_else(|| CheckpointError::NotFound(COMMIT_MESSAGE.to_owned()))
            .map(Some)
    }

    /// Put the work tree back as it was at `sha`. Files added after it are
    /// deleted: `reset --hard` only touches tracked paths, and those are
    /// untracked from the checkpoint's point of view.
    pub fn restore(&mut self, sha: &str) -> Result<(), CheckpointError> {
        if sha.is_empty() || sha.starts_with('-') {
            return Err(CheckpointError::NotFound(sha.to_owned()));
        }
        self.init()?;
        let added = if self.head()?.is_some() {
            self.run(&["diff", "--name-only", "--diff-filter=A", "-z", sha, "HEAD"])?
                .stdout
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
                .map(path_from_bytes)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        self.run(&["reset", "--hard", sha])?;
        for path in added {
            match fs::remove_file(self.work_tree.join(path)) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn init(&mut self) -> Result<(), CheckpointError> {
        if self.git_dir.join("HEAD").try_exists()? {
            return Ok(());
        }
        fs::create_dir_all(&self.git_dir)?;
        fs::create_dir_all(self.git_dir.join(HOOKS_SUBDIR))?;
        self.run(&["init"])?;
        Ok(())
    }

    fn head(&self) -> Result<Option<String>, CheckpointError> {
        if !self.git_dir.join("HEAD").try_exists()? {
            return Ok(None);
        }
        Ok(self
            .run(&["rev-parse", "--verify", "--quiet", "HEAD"])
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .filter(|sha| !sha.is_empty()))
    }

    /// The one runner. Every invocation carries the same hardening, and none
    /// of them can prompt, read a password, or pick up config from outside.
    fn run(&self, args: &[&str]) -> Result<Output, CheckpointError> {
        let command = args.join(" ");
        let output = Command::new("git")
            .current_dir(&self.work_tree)
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.work_tree)
            .env("GIT_INDEX_FILE", &self.index_file)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("GIT_CONFIG_COUNT", "0")
            .arg("-c")
            .arg(format!(
                "core.hooksPath={}",
                self.git_dir.join(HOOKS_SUBDIR).display()
            ))
            .arg("-c")
            .arg(EXT_TRANSPORT_DENIED)
            .arg("-c")
            .arg(format!("user.name={AUTHOR_NAME}"))
            .arg("-c")
            .arg(format!("user.email={AUTHOR_EMAIL}"))
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(CheckpointError::Git {
                command,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(output)
    }
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(<std::ffi::OsStr as OsStrExt>::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use test_case::test_case;

    const FIRST: &str = "one";
    const SECOND: &str = "two";

    fn git_available() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    fn fixture() -> Option<(TempDir, Checkpoints, PathBuf)> {
        if !git_available() {
            return None;
        }
        let tmp = TempDir::new().expect("tempdir");
        let work_tree = tmp.path().join("work");
        fs::create_dir_all(&work_tree).expect("work tree");
        let checkpoints =
            Checkpoints::new(&session_subdir(&tmp.path().join("state"), "s"), &work_tree);
        Some((tmp, checkpoints, work_tree))
    }

    #[test]
    fn commit_captures_the_tree_and_restore_reverts_a_modified_file() {
        let Some((_tmp, mut checkpoints, work_tree)) = fixture() else {
            return;
        };
        fs::write(work_tree.join("a.txt"), FIRST).expect("write");
        let first = checkpoints.commit(1).expect("commit").expect("first sha");

        fs::write(work_tree.join("a.txt"), SECOND).expect("rewrite");
        checkpoints.commit(3).expect("second commit");

        checkpoints.restore(&first).expect("restore");
        assert_eq!(
            fs::read_to_string(work_tree.join("a.txt")).expect("read"),
            FIRST
        );
    }

    #[test]
    fn restore_deletes_a_file_added_after_the_checkpoint() {
        let Some((_tmp, mut checkpoints, work_tree)) = fixture() else {
            return;
        };
        fs::write(work_tree.join("a.txt"), FIRST).expect("write");
        let first = checkpoints.commit(1).expect("commit").expect("first sha");

        fs::write(work_tree.join("added.txt"), SECOND).expect("add");
        checkpoints.commit(2).expect("second commit");

        checkpoints.restore(&first).expect("restore");
        assert!(!work_tree.join("added.txt").exists());
        assert_eq!(
            fs::read_to_string(work_tree.join("a.txt")).expect("read"),
            FIRST
        );
    }

    #[test]
    fn commit_returns_none_while_the_tree_matches_the_last_one() {
        let Some((_tmp, mut checkpoints, work_tree)) = fixture() else {
            return;
        };
        fs::write(work_tree.join("a.txt"), FIRST).expect("write");
        assert!(checkpoints.commit(1).expect("commit").is_some());
        assert!(checkpoints.commit(2).expect("second commit").is_none());

        fs::write(work_tree.join("a.txt"), SECOND).expect("rewrite");
        assert!(checkpoints.commit(3).expect("third commit").is_some());
    }

    #[test]
    fn checkpoints_never_touch_the_users_own_git_dir() {
        let Some((_tmp, mut checkpoints, work_tree)) = fixture() else {
            return;
        };
        fs::create_dir_all(work_tree.join(".git")).expect("user git dir");
        fs::write(work_tree.join(".git").join("HEAD"), FIRST).expect("write");
        let first = checkpoints.commit(1).expect("commit").expect("first sha");

        fs::write(work_tree.join(".git").join("HEAD"), SECOND).expect("rewrite");
        checkpoints.commit(2).expect("second commit");
        checkpoints.restore(&first).expect("restore");

        assert_eq!(
            fs::read_to_string(work_tree.join(".git").join("HEAD")).expect("read"),
            SECOND
        );
    }

    #[test_case("" ; "empty_sha")]
    #[test_case("-rf"; "flag_like_sha")]
    fn restore_rejects_a_sha_git_cannot_take(sha: &str) {
        let Some((_tmp, mut checkpoints, _work_tree)) = fixture() else {
            return;
        };
        assert!(matches!(
            checkpoints.restore(sha),
            Err(CheckpointError::NotFound(_))
        ));
    }
}
