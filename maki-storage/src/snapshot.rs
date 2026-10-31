use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::checkpoint::CheckpointError;

const REF_PREFIX: &str = "refs/maki/snapshots";
const METADATA_FILE: &str = "metadata.json";
const INDEX_FILE: &str = "index";
const SNAPSHOT_MESSAGE: &str = "maki snapshot";

#[derive(Default, Serialize, Deserialize)]
struct Metadata {
    untracked: BTreeSet<PathBuf>,
    created: BTreeSet<PathBuf>,
    had_index: bool,
}

pub struct Snapshots {
    root: PathBuf,
    index: PathBuf,
    prefix: String,
    attributes: String,
}

impl Snapshots {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn reference(&self, turn: usize) -> String {
        format!("{}{turn}", self.prefix)
    }

    pub fn attributes(&self) -> &str {
        &self.attributes
    }

    pub fn discover(cwd: &Path, session: &str) -> Result<Option<Self>, CheckpointError> {
        if session.is_empty()
            || !session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(CheckpointError::NotFound(session.to_owned()));
        }
        let output = Self::command(cwd)
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if !output.status.success() {
            return Ok(None);
        }
        let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let output = Self::command(&root)
            .args(["rev-parse", "--path-format=absolute", "--git-path", "index"])
            .output()?;
        let index = PathBuf::from(Self::text(Self::checked(output, "rev-parse")?));
        let output = Self::command(&root)
            .args(["hash-object", "-w", "-t", "tree", "--stdin"])
            .stdin(Stdio::null())
            .output()?;
        let attributes = Self::text(Self::checked(output, "hash-object")?);
        Ok(Some(Self {
            root,
            index,
            prefix: format!("{REF_PREFIX}/{session}/"),
            attributes,
        }))
    }

    pub fn capture(&self, turn: usize) -> Result<String, CheckpointError> {
        let reference = self.reference(turn);
        let mut index = NamedTempFile::new()?;
        let original_index = match fs::read(&self.index) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(bytes) = &original_index {
            index.write_all(bytes)?;
        } else {
            self.run(&["read-tree", "--empty"], Some(index.path()), None)?;
        }
        let metadata = Metadata {
            untracked: self.untracked()?,
            created: BTreeSet::new(),
            had_index: original_index.is_some(),
        };
        self.run(&["add", "-A", "--", "."], Some(index.path()), None)?;
        let tree = Self::text(self.run(&["write-tree"], Some(index.path()), None)?);
        self.store(
            &reference,
            &tree,
            original_index.as_deref().unwrap_or_default(),
            &metadata,
        )
    }

    pub fn record_created(&self, turn: usize) -> Result<(), CheckpointError> {
        let reference = format!("{}{turn}", self.prefix);
        let sha = Self::text(self.run(&["rev-parse", "--verify", &reference], None, None)?);
        let mut metadata = self.metadata(&sha)?;
        let current = self.untracked()?;
        let added = current
            .difference(&metadata.untracked)
            .cloned()
            .collect::<Vec<_>>();
        let count = metadata.created.len();
        metadata.created.extend(added);
        if count == metadata.created.len() {
            return Ok(());
        }
        let tree = Self::text(self.run(&["rev-parse", &format!("{sha}^{{tree}}")], None, None)?);
        let index = self
            .run(&["show", &format!("{sha}^:{INDEX_FILE}")], None, None)?
            .stdout;
        self.store(&reference, &tree, &index, &metadata)?;
        Ok(())
    }

    pub fn restore_turn(&self, turn: usize) -> Result<bool, CheckpointError> {
        let records = self.records()?;
        let Some((_, sha)) = records.iter().find(|(number, _)| *number >= turn) else {
            return Ok(false);
        };
        let metadata = self.metadata(sha)?;
        let target = self.paths(sha)?;
        let mut removed = self.paths("")?;
        for (_, snapshot) in records.iter().filter(|(number, _)| *number >= turn) {
            let record = self.metadata(snapshot)?;
            removed.extend(record.created);
            removed.extend(self.paths(snapshot)?);
        }
        for path in removed.difference(&target) {
            let absolute = self.root.join(path);
            match fs::symlink_metadata(&absolute) {
                Ok(info) if !info.is_dir() => fs::remove_file(&absolute)?,
                Ok(_) => {
                    return Err(CheckpointError::Git {
                        command: "restore".to_owned(),
                        stderr: format!("refusing to remove directory {}", absolute.display()),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let index = NamedTempFile::new()?;
        self.run(&["read-tree", sha], Some(index.path()), None)?;
        self.run(
            &["checkout-index", "--all", "--force"],
            Some(index.path()),
            None,
        )?;
        if metadata.had_index {
            let original = self
                .run(&["show", &format!("{sha}^:{INDEX_FILE}")], None, None)?
                .stdout;
            crate::atomic_write(&self.index, &original).map_err(std::io::Error::other)?;
        } else if self.index.try_exists()? {
            fs::remove_file(&self.index)?;
        }
        for (number, _) in records.into_iter().filter(|(number, _)| *number > turn) {
            self.run(
                &["update-ref", "-d", &format!("{}{number}", self.prefix)],
                None,
                None,
            )?;
        }
        Ok(true)
    }

    fn records(&self) -> Result<Vec<(usize, String)>, CheckpointError> {
        let output = self.run(
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                &self.prefix,
            ],
            None,
            None,
        )?;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut records = text
            .lines()
            .filter_map(|line| {
                let (reference, sha) = line.split_once(' ')?;
                Some((
                    reference.strip_prefix(&self.prefix)?.parse().ok()?,
                    sha.to_owned(),
                ))
            })
            .collect::<Vec<_>>();
        records.sort_unstable_by_key(|(turn, _)| *turn);
        Ok(records)
    }

    fn metadata(&self, sha: &str) -> Result<Metadata, CheckpointError> {
        let output = self.run(&["show", &format!("{sha}^:{METADATA_FILE}")], None, None)?;
        serde_json::from_slice(&output.stdout).map_err(|error| CheckpointError::Git {
            command: "read snapshot metadata".to_owned(),
            stderr: error.to_string(),
        })
    }

    fn store(
        &self,
        reference: &str,
        tree: &str,
        index: &[u8],
        metadata: &Metadata,
    ) -> Result<String, CheckpointError> {
        let index_blob =
            Self::text(self.run(&["hash-object", "-w", "--stdin"], None, Some(index))?);
        let json = serde_json::to_vec(metadata).map_err(|error| CheckpointError::Git {
            command: "encode snapshot metadata".to_owned(),
            stderr: error.to_string(),
        })?;
        let metadata_blob =
            Self::text(self.run(&["hash-object", "-w", "--stdin"], None, Some(&json))?);
        let entries = format!(
            "100644 blob {index_blob}\t{INDEX_FILE}\n100644 blob {metadata_blob}\t{METADATA_FILE}\n"
        );
        let metadata_tree = Self::text(self.run(&["mktree"], None, Some(entries.as_bytes()))?);
        let parent = Self::text(self.run(
            &["commit-tree", &metadata_tree, "-m", SNAPSHOT_MESSAGE],
            None,
            None,
        )?);
        let sha = Self::text(self.run(
            &["commit-tree", tree, "-p", &parent, "-m", SNAPSHOT_MESSAGE],
            None,
            None,
        )?);
        self.run(&["update-ref", reference, &sha], None, None)?;
        Ok(sha)
    }

    fn untracked(&self) -> Result<BTreeSet<PathBuf>, CheckpointError> {
        self.path_output(&["ls-files", "--others", "--exclude-standard", "-z"])
    }

    fn paths(&self, sha: &str) -> Result<BTreeSet<PathBuf>, CheckpointError> {
        if sha.is_empty() {
            self.path_output(&["ls-files", "-z"])
        } else {
            self.path_output(&["ls-tree", "-r", "--name-only", "-z", sha])
        }
    }

    fn path_output(&self, args: &[&str]) -> Result<BTreeSet<PathBuf>, CheckpointError> {
        let output = self.run(args, None, None)?;
        Ok(output
            .stdout
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|bytes| {
                #[cfg(unix)]
                {
                    use std::ffi::OsStr;
                    use std::os::unix::ffi::OsStrExt;
                    PathBuf::from(OsStr::from_bytes(bytes))
                }
                #[cfg(not(unix))]
                {
                    PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
                }
            })
            .collect())
    }

    fn command(cwd: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", "0")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .args([
                "-c",
                "core.hooksPath=",
                "-c",
                "protocol.ext.allow=never",
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.attributesFile=",
                "-c",
                "user.name=maki",
                "-c",
                "user.email=snapshot@maki.local",
            ]);
        command
    }

    fn run(
        &self,
        args: &[&str],
        index: Option<&Path>,
        input: Option<&[u8]>,
    ) -> Result<Output, CheckpointError> {
        let mut command = Self::command(&self.root);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.env("GIT_ATTR_SOURCE", &self.attributes);
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        command.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = command.spawn()?;
        if let Some(input) = input
            && let Some(mut stdin) = child.stdin.take()
        {
            stdin.write_all(input)?;
        }
        Self::checked(child.wait_with_output()?, &args.join(" "))
    }

    fn checked(output: Output, command: &str) -> Result<Output, CheckpointError> {
        if output.status.success() {
            Ok(output)
        } else {
            Err(CheckpointError::Git {
                command: command.to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }

    fn text(output: Output) -> String {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::Snapshots;

    const ORIGINAL: &str = "original\n";
    const CHANGED: &str = "changed\n";
    const SESSION: &str = "acceptance";

    fn git(root: &std::path::Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .current_dir(root)
            .args(["-c", "user.name=test", "-c", "user.email=test@maki.local"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test_case(false ; "tracked_creation")]
    #[test_case(true ; "untracked_creation")]
    fn rewind_preserves_index_diff_and_preexisting_untracked(stage_created: bool) {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        git(root, &["init", "--quiet"]);
        for file in ["a.rs", "b.rs", "c.rs"] {
            fs::write(root.join(file), ORIGINAL).unwrap();
        }
        git(root, &["add", "."]);
        git(root, &["commit", "--quiet", "-m", "baseline"]);
        fs::write(root.join("a.rs"), CHANGED).unwrap();
        git(root, &["add", "a.rs"]);
        fs::write(root.join("b.rs"), CHANGED).unwrap();
        fs::write(root.join("existing.txt"), ORIGINAL).unwrap();
        let status = git(root, &["status", "--porcelain=v1", "-z"]);
        let diff = git(root, &["diff", "--binary"]);
        let staged = git(root, &["diff", "--cached", "--binary"]);
        let head = git(root, &["rev-parse", "HEAD"]);
        let snapshots = Snapshots::discover(root, SESSION).unwrap().unwrap();
        snapshots.capture(0).unwrap();
        for file in ["a.rs", "b.rs", "c.rs"] {
            fs::write(root.join(file), "turn two\n").unwrap();
        }
        fs::write(root.join("new.txt"), CHANGED).unwrap();
        snapshots.record_created(0).unwrap();
        if stage_created {
            git(root, &["add", "new.txt"]);
        }
        snapshots.capture(4).unwrap();
        assert!(snapshots.restore_turn(0).unwrap());
        assert_eq!(git(root, &["status", "--porcelain=v1", "-z"]), status);
        assert_eq!(git(root, &["diff", "--binary"]), diff);
        assert_eq!(git(root, &["diff", "--cached", "--binary"]), staged);
        assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            fs::read_to_string(root.join("existing.txt")).unwrap(),
            ORIGINAL
        );
        assert!(!root.join("new.txt").exists());
        assert!(
            snapshots
                .records()
                .unwrap()
                .iter()
                .all(|(turn, _)| *turn == 0)
        );
    }
}
