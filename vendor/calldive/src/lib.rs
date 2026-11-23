pub mod analysis;
mod cache;
mod contracts;
pub mod diffview;
pub mod git;
mod sources;
pub mod syntax;
pub mod ui;

use analysis::{Analysis, Inputs};
use anyhow::{Context, Result, bail};
pub use cache::CacheStatus;
use git::{FileChange, GitRepo, WORKTREE};
use std::path::Path;

pub fn review(repo: &Path, base: Option<&str>, head: Option<&str>) -> Result<ui::App> {
    let (app, _) = review_with_cache_status(repo, base, head)?;
    Ok(app)
}

pub fn review_with_cache_status(
    repo: &Path,
    base: Option<&str>,
    head: Option<&str>,
) -> Result<(ui::App, CacheStatus)> {
    let repo = GitRepo::open(repo)?;
    let base = base.unwrap_or("HEAD");
    let head = head.unwrap_or(WORKTREE);
    if git::is_worktree(base) {
        bail!("base ref '{base}' cannot be worktree; pass a commit revision as base");
    }
    let base_rev = repo.resolve_base(base)?;
    let head_rev = git::resolve_rev(&repo, head)?;
    let changes = repo.changed_files(&base_rev, head_rev.as_deref())?;
    let (analysis, cache_status) =
        load_analysis_with_status(&repo, &changes, &base_rev, head_rev.as_deref(), true, false)?;
    let roots = analysis
        .entry_points
        .iter()
        .filter(|key| analysis.nodes.get(*key).is_some_and(|node| node.relevant))
        .cloned()
        .collect();
    let head_label = if git::is_worktree(head) {
        WORKTREE
    } else {
        head
    };
    let mut app = ui::App::new(analysis, roots, false);
    app.flash = Some(match cache_status {
        CacheStatus::Hit => format!("reviewing {base} → {head_label} (cached)"),
        CacheStatus::Miss => format!("reviewing {base} → {head_label}"),
    });
    Ok((app, cache_status))
}

pub fn load_analysis(
    repo: &GitRepo,
    changes: &[FileChange],
    base: &str,
    head: Option<&str>,
    include_tests: bool,
) -> Result<Analysis> {
    let (analysis, _) = load_analysis_with_status(repo, changes, base, head, include_tests, true)?;
    Ok(analysis)
}

pub fn load_analysis_with_status(
    repo: &GitRepo,
    changes: &[FileChange],
    base: &str,
    head: Option<&str>,
    include_tests: bool,
    log_stderr: bool,
) -> Result<(Analysis, CacheStatus)> {
    let cache = match cache::Cache::new(repo, changes, base, head, include_tests) {
        Ok(cache) => Some(cache),
        Err(error) => {
            if log_stderr {
                eprintln!("warning: analysis cache unavailable: {error:#}");
            }
            None
        }
    };
    if let Some(analysis) = cache.as_ref().and_then(|cache| match cache.load() {
        Ok(analysis) => analysis,
        Err(error) => {
            if log_stderr {
                eprintln!("warning: ignoring analysis cache: {error:#}");
            }
            None
        }
    }) {
        if log_stderr {
            eprintln!("using cached analysis");
        }
        return Ok((analysis, CacheStatus::Hit));
    }
    let (files, extra_head) =
        sources::gather_sources(repo, changes, base, head, base, head.unwrap_or(WORKTREE))
            .context("loading review sources")?;
    let mut analysis = analysis::analyze(&Inputs {
        files,
        extra_head,
        include_tests,
    });
    for change in changes {
        analysis.ensure_changed_file(&change.path);
        if let Some(old_path) = &change.old_path {
            analysis.rename_file_origin(&change.path, old_path, change.status == 'R');
        }
    }
    if let Some(cache) = cache {
        let save = || -> Result<()> {
            let current_changes = repo.changed_files(base, head)?;
            let current = cache::Cache::new(repo, &current_changes, base, head, include_tests)?;
            if cache.same_snapshot(&current) {
                cache.store(&analysis)?;
            }
            Ok(())
        };
        if let Err(error) = save()
            && log_stderr
        {
            eprintln!("warning: could not save analysis cache: {error:#}");
        }
    }
    Ok((analysis, CacheStatus::Miss))
}

#[cfg(test)]
mod tests {
    use super::{CacheStatus, review, review_with_cache_status};
    use crate::analysis::{Status, node_key};
    use crate::git::BINARY_SOURCE_PREFIX;
    use std::{fs, path::Path, process::Command};
    use tempfile::{TempDir, tempdir};
    use test_case::test_case;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> TempDir {
        let directory = tempdir().unwrap();
        git(directory.path(), &["init", "--quiet"]);
        git(directory.path(), &["config", "core.autocrlf", "false"]);
        git(
            directory.path(),
            &["config", "core.hooksPath", ".git/no-hooks"],
        );
        git(directory.path(), &["config", "user.name", "Calldive Tests"]);
        git(
            directory.path(),
            &["config", "user.email", "calldive@example.invalid"],
        );
        directory
    }

    fn commit(repo: &Path) {
        git(repo, &["add", "."]);
        git(
            repo,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "baseline",
            ],
        );
    }

    #[test_case(false; "unborn_head")]
    #[test_case(true; "head_to_worktree")]
    fn default_review_includes_all_changed_file_kinds(has_commit: bool) {
        let directory = repository();
        let repo = directory.path();
        if has_commit {
            fs::write(repo.join("deleted.txt"), "deleted\n").unwrap();
            fs::write(repo.join("modified.txt"), "before\n").unwrap();
            fs::write(repo.join("supported.rs"), "// before\n").unwrap();
            commit(repo);
            fs::remove_file(repo.join("deleted.txt")).unwrap();
        }
        fs::write(repo.join("modified.txt"), "after!\n").unwrap();
        fs::write(repo.join("supported.rs"), "// after\n").unwrap();
        fs::write(repo.join("untracked.txt"), "untracked\n").unwrap();
        fs::write(repo.join("image.bin"), [0, 1, 2]).unwrap();
        fs::write(repo.join("staged.txt"), "staged\n").unwrap();
        git(repo, &["add", "staged.txt"]);
        let app = review(repo, None, None).unwrap();
        for path in [
            "modified.txt",
            "supported.rs",
            "untracked.txt",
            "image.bin",
            "staged.txt",
        ] {
            assert!(
                app.a.nodes.contains_key(&node_key(path, "<file>")),
                "{path}"
            );
        }
        if has_commit {
            assert_eq!(
                app.a.nodes[&node_key("deleted.txt", "<file>")].status,
                Status::Removed
            );
            assert_eq!(
                app.a.nodes[&node_key("modified.txt", "<file>")].status,
                Status::BodyChanged
            );
        }
        assert!(app.a.head_srcs["image.bin"].starts_with(BINARY_SOURCE_PREFIX));
        fs::write(repo.join("modified.txt"), "newest\n").unwrap();
        let updated = review(repo, None, None).unwrap();
        assert_eq!(updated.a.head_srcs["modified.txt"], "newest\n");
    }

    #[test]
    fn renamed_file_keeps_original_revision_path() {
        let directory = repository();
        let repo = directory.path();
        fs::write(repo.join("before.txt"), "unchanged\n").unwrap();
        commit(repo);
        git(repo, &["mv", "before.txt", "after.txt"]);
        let app = review(repo, None, None).unwrap();
        let node = &app.a.nodes[&node_key("after.txt", "<file>")];
        assert_eq!(node.base.as_ref().unwrap().file, "before.txt");
        assert_eq!(node.head.as_ref().unwrap().file, "after.txt");
        assert_eq!(node.moved_from.as_deref(), Some("before.txt"));
        assert_eq!(node.status, Status::Moved);
    }

    #[test]
    fn unchanged_callers_are_discovered_by_the_shared_source_loader() {
        let directory = repository();
        let repo = directory.path();
        fs::write(repo.join("changed.rs"), "pub fn target() { before(); }\n").unwrap();
        fs::write(repo.join("unchanged.rs"), "fn caller() { target(); }\n").unwrap();
        commit(repo);
        fs::write(repo.join("changed.rs"), "pub fn target() { after(); }\n").unwrap();
        let app = review(repo, None, None).unwrap();
        let node = &app.a.nodes[&node_key("changed.rs", "target")];
        assert_eq!(
            node.external_callers,
            vec![node_key("unchanged.rs", "caller")]
        );
    }

    #[test]
    fn staged_deletion_and_untracked_replacement_reviews_both_sides() {
        let directory = repository();
        let repo = directory.path();
        fs::write(repo.join("replaced.txt"), "before\n").unwrap();
        commit(repo);
        git(repo, &["rm", "replaced.txt"]);
        fs::write(repo.join("replaced.txt"), "after\n").unwrap();
        let app = review(repo, None, None).unwrap();
        let node = &app.a.nodes[&node_key("replaced.txt", "<file>")];
        assert_eq!(node.status, Status::BodyChanged);
        assert_eq!(app.a.base_body(node).as_deref(), Some("before\n"));
        assert_eq!(app.a.head_body(node).as_deref(), Some("after\n"));
    }

    #[test]
    fn review_caches_branch_and_worktree_pairs_and_invalidates_on_change() {
        let directory = repository();
        let repo = directory.path();
        fs::write(repo.join("lib.rs"), "pub fn run() -> u32 { 1 }\n").unwrap();
        commit(repo);
        git(repo, &["branch", "-M", "main"]);
        git(repo, &["checkout", "-qb", "dev"]);
        fs::write(
            repo.join("lib.rs"),
            "pub fn run() -> u32 { 2 }\npub fn extra() -> u32 { 3 }\n",
        )
        .unwrap();
        commit(repo);
        git(repo, &["checkout", "-q", "main"]);

        let (cold, status) = review_with_cache_status(repo, Some("main"), Some("dev")).unwrap();
        assert_eq!(status, CacheStatus::Miss);
        assert!(cold.a.nodes.contains_key(&node_key("lib.rs", "extra")));

        let (warm, status) = review_with_cache_status(repo, Some("main"), Some("dev")).unwrap();
        assert_eq!(status, CacheStatus::Hit);
        assert_eq!(warm.a.summary, cold.a.summary);

        fs::write(repo.join("work.rs"), "pub fn dirty() {}\n").unwrap();
        let (_, wt_miss) = review_with_cache_status(repo, None, None).unwrap();
        assert_eq!(wt_miss, CacheStatus::Miss);
        let (_, wt_hit) = review_with_cache_status(repo, None, None).unwrap();
        assert_eq!(wt_hit, CacheStatus::Hit);

        // Branch pair entry remains cached alongside worktree entry
        let (_, branch_still_hit) =
            review_with_cache_status(repo, Some("main"), Some("dev")).unwrap();
        assert_eq!(branch_still_hit, CacheStatus::Hit);

        // Modifying worktree invalidates worktree cache
        fs::write(repo.join("work.rs"), "pub fn dirty_v2() {}\n").unwrap();
        let (edited, wt_invalidated) = review_with_cache_status(repo, None, None).unwrap();
        assert_eq!(wt_invalidated, CacheStatus::Miss);
        assert!(
            edited
                .a
                .nodes
                .contains_key(&node_key("work.rs", "dirty_v2"))
        );
    }
}
