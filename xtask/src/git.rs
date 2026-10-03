//! A thin, typed port to `git` for one repository (the commit wrapper's only git
//! access, §11.10). Every call runs `git -C <root>`; nothing here creates a branch or a
//! worktree.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::process;

const GIT_LIMIT: Duration = Duration::from_mins(2);

/// How long a command waits out another process's `index.lock` before failing.
const INDEX_LOCK_PATIENCE: Duration = Duration::from_secs(5);

/// The pause between attempts while the index is locked.
const INDEX_LOCK_POLL: Duration = Duration::from_millis(100);

/// A git object ID (SHA-1 or SHA-256 hex).
pub type Oid = String;

/// One file of a tree (`git ls-tree -r`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// Git file mode: `100644`, `100755` or `120000` (symlink).
    pub mode: String,
    /// Blob ID.
    pub oid: Oid,
    /// Path relative to the tree root.
    pub path: String,
}

/// A git repository, addressed by its working-tree root.
#[derive(Clone, Debug)]
pub struct Git {
    root: PathBuf,
    scrub_env: bool,
}

impl Git {
    /// The repository whose working tree is `root`, using the inherited environment.
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            scrub_env: false,
        }
    }

    /// The same repository, ignoring inherited `GIT_DIR`-style variables and the
    /// user's global config: for fixture repositories in tests, which must work even
    /// inside a git-isolated candidate (§11.10 step 4).
    #[cfg(test)]
    pub fn scrubbed(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            scrub_env: true,
        }
    }

    /// The working-tree root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.root).args(args);
        // Paths are names, never pathspec globs or magic: `*.rs` names a file `*.rs`.
        cmd.env("GIT_LITERAL_PATHSPECS", "1");
        if self.scrub_env {
            for var in [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_INDEX_FILE",
                "GIT_CEILING_DIRECTORIES",
                "GIT_OBJECT_DIRECTORY",
            ] {
                cmd.env_remove(var);
            }
            cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1");
        }
        cmd
    }

    /// Runs `git <args>` and returns stdout, or an error naming the command.
    ///
    /// Any git command another agent runs in the shared checkout, even `git status`,
    /// holds `index.lock` for a moment. A command that failed to take that lock changed
    /// nothing, so it is retried for up to [`INDEX_LOCK_PATIENCE`].
    pub fn run(&self, args: &[&str]) -> Result<String, String> {
        let mut waited = Duration::ZERO;
        loop {
            let captured = process::run(&mut self.command(args), GIT_LIMIT)
                .map_err(|err| format!("git {}: {err}", args.join(" ")))?;
            if captured.status.success() {
                return Ok(captured.stdout);
            }
            if index_lock_busy(&captured.stderr) && waited < INDEX_LOCK_PATIENCE {
                std::thread::sleep(INDEX_LOCK_POLL);
                waited += INDEX_LOCK_POLL;
                continue;
            }
            return Err(format!(
                "git {} exited with {}: {}",
                args.join(" "),
                captured.status,
                captured.stderr.trim()
            ));
        }
    }

    /// Runs `git <args>` with `input` on stdin and returns stdout.
    fn run_with_stdin(&self, args: &[&str], input: &str) -> Result<String, String> {
        let name = format!("git {}", args.join(" "));
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("{name}: {err}"))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("{name}: no stdin"))?;
        let input = input.to_owned();
        // Write from a thread so a large reply can never deadlock against the request.
        let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
        let output = child
            .wait_with_output()
            .map_err(|err| format!("{name}: {err}"))?;
        let written = writer.join();
        if !output.status.success() {
            return Err(format!(
                "{name} exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        match written {
            Ok(Ok(())) => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
            Ok(Err(err)) => Err(format!("{name}: writing stdin: {err}")),
            Err(_) => Err(format!("{name}: the stdin writer panicked")),
        }
    }

    /// The current branch, or `None` on a detached HEAD (or outside a repository).
    pub fn branch(&self) -> Option<String> {
        self.run(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok()
            .map(|name| name.trim().to_owned())
    }

    /// The commit ID of `rev`.
    pub fn rev_parse(&self, rev: &str) -> Result<Oid, String> {
        Ok(self.run(&["rev-parse", "--verify", rev])?.trim().to_owned())
    }

    /// The tree ID of the index. Writes the tree objects and, like any index update, takes
    /// `index.lock` briefly to refresh the cached tree; the staged content is unchanged.
    pub fn write_tree(&self) -> Result<Oid, String> {
        Ok(self.run(&["write-tree"])?.trim().to_owned())
    }

    /// Paths whose content differs between two trees (or commits), recursively.
    pub fn changed_between(&self, from: &str, to: &str) -> Result<Vec<String>, String> {
        Ok(split_nul(&self.run(&[
            "diff-tree",
            "-r",
            "--name-only",
            "-z",
            "--no-renames",
            from,
            to,
        ])?))
    }

    /// The content of the file `path` in `tree`, or `None` if the tree has no such file.
    pub fn file_at(&self, tree: &str, path: &str) -> Result<Option<String>, String> {
        let listed = self.run(&["ls-tree", "-r", "-z", "--full-tree", tree, "--", path])?;
        for record in split_nul(&listed) {
            let entry = parse_ls_tree(&record)?;
            if entry.path == path {
                return self.run(&["cat-file", "blob", &entry.oid]).map(Some);
            }
        }
        Ok(None)
    }

    /// Sets the index entries of `paths` to their state in `tree`: that tree's blob and
    /// mode where it has the path, and no entry where it does not. Working files are
    /// untouched.
    pub fn copy_entries(&self, tree: &str, paths: &[String]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", tree, "--"];
        args.extend(paths.iter().map(String::as_str));
        let mut present = BTreeSet::new();
        let mut info = String::new();
        for record in split_nul(&self.run(&args)?) {
            let entry = parse_ls_tree(&record)?;
            let _ = write!(info, "{} {}\t{}\0", entry.mode, entry.oid, entry.path);
            present.insert(entry.path);
        }
        // Mode 0 with the null object ID removes the entry (`git update-index --index-info`).
        let null = "0".repeat(tree.len());
        for path in paths.iter().filter(|path| !present.contains(*path)) {
            let _ = write!(info, "0 {null}\t{path}\0");
        }
        self.run_with_stdin(&["update-index", "-z", "--index-info"], &info)
            .map(drop)
    }

    /// Whether `paths` match HEAD in the index and the working tree, with no untracked
    /// file among them.
    pub fn unchanged_from_head(&self, paths: &[String]) -> Result<bool, String> {
        let mut args = vec![
            "--no-optional-locks",
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=all",
            "--",
        ];
        args.extend(paths.iter().map(String::as_str));
        Ok(self.run(&args)?.is_empty())
    }

    /// Paths staged relative to HEAD.
    pub fn staged_paths(&self) -> Result<Vec<String>, String> {
        Ok(split_nul(&self.run(&[
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--no-renames",
        ])?))
    }

    /// Stages exactly `paths` (modifications, additions and deletions).
    pub fn stage(&self, paths: &[String]) -> Result<(), String> {
        let mut args = vec!["add", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.run(&args).map(drop)
    }

    /// Unstages `paths`, leaving the working-tree files untouched.
    pub fn unstage(&self, paths: &[String]) -> Result<(), String> {
        let mut args = vec!["restore", "--staged", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.run(&args).map(drop)
    }

    /// A git operation that would make committing unsafe, if one is in progress. A held
    /// `index.lock` is not one: it is transient, and [`Git::run`] waits it out.
    pub fn operation_in_progress(&self) -> Result<Option<String>, String> {
        for (marker, what) in [
            ("MERGE_HEAD", "a merge is in progress"),
            ("CHERRY_PICK_HEAD", "a cherry-pick is in progress"),
            ("REVERT_HEAD", "a revert is in progress"),
            ("rebase-merge", "a rebase is in progress"),
            ("rebase-apply", "a rebase or am is in progress"),
            ("BISECT_LOG", "a bisect is in progress"),
        ] {
            let path = self.run(&["rev-parse", "--git-path", marker])?;
            let path = self.root.join(path.trim());
            if path.exists() {
                return Ok(Some(what.to_owned()));
            }
        }
        Ok(None)
    }

    /// Every file of `tree`, recursively.
    pub fn ls_tree(&self, tree: &str) -> Result<Vec<TreeEntry>, String> {
        let listing = self.run(&["ls-tree", "-r", "-z", "--full-tree", tree])?;
        split_nul(&listing)
            .iter()
            .map(|record| parse_ls_tree(record))
            .collect()
    }

    /// The contents of `oids`, in order, through one `git cat-file --batch`.
    pub fn read_blobs(&self, oids: &[Oid]) -> Result<Vec<Vec<u8>>, String> {
        let mut child = self
            .command(&["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("git cat-file --batch: {err}"))?;
        let mut stdin = child.stdin.take().ok_or("git cat-file: no stdin")?;
        let mut request = oids.join("\n");
        request.push('\n');
        // Write from a thread so a large reply can never deadlock against the request.
        let writer = std::thread::spawn(move || stdin.write_all(request.as_bytes()));
        let stdout = child.stdout.take().ok_or("git cat-file: no stdout")?;
        let mut reader = BufReader::new(stdout);
        let mut blobs = Vec::with_capacity(oids.len());
        for oid in oids {
            let mut header = String::new();
            reader
                .read_line(&mut header)
                .map_err(|err| format!("git cat-file: {err}"))?;
            let size: usize = header
                .trim_end()
                .rsplit(' ')
                .next()
                .and_then(|s| s.parse().ok())
                .filter(|_| header.contains(" blob "))
                .ok_or_else(|| format!("git cat-file: no blob {oid} (got {header:?})"))?;
            let mut content = vec![0_u8; size + 1];
            reader
                .read_exact(&mut content)
                .map_err(|err| format!("git cat-file: {err}"))?;
            content.truncate(size);
            blobs.push(content);
        }
        let _ = writer.join();
        child.wait().map_err(|err| format!("git cat-file: {err}"))?;
        Ok(blobs)
    }

    /// The blob IDs git would give the files at `paths` (no filters), in order.
    pub fn hash_files(&self, paths: &[PathBuf]) -> Result<Vec<Oid>, String> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut child = self
            .command(&["hash-object", "--no-filters", "--stdin-paths"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("git hash-object: {err}"))?;
        let mut stdin = child.stdin.take().ok_or("git hash-object: no stdin")?;
        let mut request = paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        request.push('\n');
        let writer = std::thread::spawn(move || stdin.write_all(request.as_bytes()));
        let output = child
            .wait_with_output()
            .map_err(|err| format!("git hash-object: {err}"))?;
        let _ = writer.join();
        if !output.status.success() {
            return Err(format!(
                "git hash-object: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let oids: Vec<Oid> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        if oids.len() == paths.len() {
            Ok(oids)
        } else {
            Err(format!(
                "git hash-object returned {} IDs for {} paths",
                oids.len(),
                paths.len()
            ))
        }
    }

    /// Creates a commit of exactly `tree` on top of `parent` (no hooks, no index use).
    pub fn commit_tree(&self, tree: &str, parent: &str, message: &str) -> Result<Oid, String> {
        self.run_with_stdin(&["commit-tree", tree, "-p", parent, "-F", "-"], message)
            .map(|out| out.trim().to_owned())
    }

    /// Moves `branch` from `expected` to `new` atomically; fails if it moved meanwhile.
    pub fn advance_branch(&self, branch: &str, new: &str, expected: &str) -> Result<(), String> {
        let reference = format!("refs/heads/{branch}");
        self.run(&[
            "update-ref",
            "-m",
            "keyval ./dev commit",
            &reference,
            new,
            expected,
        ])
        .map(drop)
    }

    /// Branch names and linked worktrees, for proving none were created.
    #[cfg(test)]
    pub fn branches_and_worktrees(&self) -> Result<(Vec<String>, usize), String> {
        let branches = self
            .run(&["for-each-ref", "--format=%(refname)", "refs/heads"])?
            .lines()
            .map(str::to_owned)
            .collect();
        let worktrees = self
            .run(&["worktree", "list", "--porcelain"])?
            .lines()
            .filter(|line| line.starts_with("worktree "))
            .count();
        Ok((branches, worktrees))
    }
}

/// Whether a failed command's stderr says another process holds `index.lock`.
fn index_lock_busy(stderr: &str) -> bool {
    stderr.contains("index.lock") && stderr.contains("File exists")
}

/// One `git ls-tree -r -z` record: `<mode> SP <kind> SP <oid> TAB <path>`, files only.
fn parse_ls_tree(record: &str) -> Result<TreeEntry, String> {
    let (meta, path) = record
        .split_once('\t')
        .ok_or_else(|| format!("unexpected ls-tree record {record:?}"))?;
    let mut fields = meta.split(' ');
    let (Some(mode), Some(kind), Some(oid)) = (fields.next(), fields.next(), fields.next()) else {
        return Err(format!("unexpected ls-tree record {record:?}"));
    };
    if kind != "blob" {
        return Err(format!(
            "{path} is a {kind} ({mode}); candidates hold only files and symlinks"
        ));
    }
    Ok(TreeEntry {
        mode: mode.to_owned(),
        oid: oid.to_owned(),
        path: path.to_owned(),
    })
}

fn split_nul(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
pub mod fixture {
    //! Scratch git repositories for tests.

    use std::fs;
    use std::path::Path;

    use super::Git;
    use crate::test_support::scratch_dir;

    /// A fresh repository on branch `main` with one commit containing `files`.
    pub fn repo(name: &str, files: &[(&str, &str)]) -> Git {
        let root = scratch_dir(name);
        let git = Git::scrubbed(&root);
        git.run(&["init", "--quiet", "--initial-branch=main"])
            .unwrap();
        git.run(&["config", "user.name", "keyval test"]).unwrap();
        git.run(&["config", "user.email", "test@keyval.invalid"])
            .unwrap();
        for (path, content) in files {
            write(git.root(), path, content);
        }
        git.run(&["add", "--all"]).unwrap();
        git.run(&["commit", "--quiet", "-m", "fixture"]).unwrap();
        git
    }

    /// Writes `content` to `root/path`, creating directories.
    pub fn write(root: &Path, path: &str, content: &str) {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_listing_blobs_and_hashes_agree() {
        let git = fixture::repo("git-port", &[("a.txt", "alpha\n"), ("dir/b.txt", "beta\n")]);
        let listed = git.rev_parse("HEAD^{tree}").unwrap();
        let entries = git.ls_tree(&listed).unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "dir/b.txt"]);
        let oids: Vec<Oid> = entries.iter().map(|e| e.oid.clone()).collect();
        let blobs = git.read_blobs(&oids).unwrap();
        assert_eq!(blobs, [b"alpha\n".to_vec(), b"beta\n".to_vec()]);
        let hashed = git
            .hash_files(&[git.root().join("a.txt"), git.root().join("dir/b.txt")])
            .unwrap();
        assert_eq!(
            hashed, oids,
            "rule: hash-object of the files equals the tree's blob IDs"
        );
    }

    #[test]
    fn staging_unstaging_and_branch_queries() {
        let git = fixture::repo("git-stage", &[("a.txt", "alpha\n")]);
        assert_eq!(git.branch().as_deref(), Some("main"));
        assert_eq!(git.operation_in_progress().unwrap(), None);
        fixture::write(git.root(), "a.txt", "changed\n");
        git.stage(&["a.txt".to_owned()]).unwrap();
        assert_eq!(git.staged_paths().unwrap(), ["a.txt"]);
        git.unstage(&["a.txt".to_owned()]).unwrap();
        assert_eq!(git.staged_paths().unwrap(), Vec::<String>::new());
        assert_eq!(
            std::fs::read_to_string(git.root().join("a.txt")).unwrap(),
            "changed\n",
            "rule: unstaging never touches the working file"
        );
    }

    #[test]
    fn index_entries_copy_from_a_tree_and_trees_diff_by_path() {
        let git = fixture::repo("git-copy", &[("a.txt", "alpha\n"), ("b.txt", "beta\n")]);
        let base = git.rev_parse("HEAD^{tree}").unwrap();
        fixture::write(git.root(), "a.txt", "alpha2\n");
        fixture::write(git.root(), "c.txt", "gamma\n");
        std::fs::remove_file(git.root().join("b.txt")).unwrap();
        git.run(&["add", "--all"]).unwrap();
        let edited = git.write_tree().unwrap();
        let all = ["a.txt", "b.txt", "c.txt"].map(str::to_owned);
        assert_eq!(git.changed_between(&base, &edited).unwrap(), all);
        git.run(&["read-tree", &base]).unwrap();
        git.copy_entries(&edited, &all).unwrap();
        assert_eq!(
            git.write_tree().unwrap(),
            edited,
            "rule: copying the changed paths' entries reproduces the tree, deletions included"
        );
        assert_eq!(
            std::fs::read_to_string(git.root().join("a.txt")).unwrap(),
            "alpha2\n",
            "rule: index-only; working files are untouched"
        );
        assert_eq!(
            git.file_at(&edited, "c.txt").unwrap().as_deref(),
            Some("gamma\n")
        );
        assert_eq!(git.file_at(&edited, "b.txt").unwrap(), None, "deleted");
        assert_eq!(
            git.file_at(&base, "a").unwrap(),
            None,
            "a prefix is no file"
        );
    }

    #[test]
    fn paths_are_literal_and_status_counts_untracked_files() {
        let git = fixture::repo("git-literal", &[("a.rs", "a\n")]);
        fixture::write(git.root(), "b.rs", "b\n");
        assert!(
            git.stage(&["*.rs".to_owned()]).is_err(),
            "rule: `*.rs` names a file, never a glob"
        );
        assert_eq!(git.staged_paths().unwrap(), Vec::<String>::new());
        assert!(git.unchanged_from_head(&["a.rs".to_owned()]).unwrap());
        assert!(
            !git.unchanged_from_head(&["b.rs".to_owned()]).unwrap(),
            "rule: an untracked file is a change"
        );
    }

    #[test]
    fn a_briefly_held_index_lock_is_waited_out() {
        let git = fixture::repo("git-index-lock", &[("a.txt", "alpha\n")]);
        let lock = git.root().join(".git/index.lock");
        std::fs::write(&lock, "").unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            std::fs::remove_file(lock).unwrap();
        });
        fixture::write(git.root(), "a.txt", "changed\n");
        let staged = git.stage(&["a.txt".to_owned()]);
        releaser.join().unwrap();
        assert_eq!(
            staged,
            Ok(()),
            "rule: another process's transient index.lock delays, never fails, a command"
        );
        assert_eq!(git.staged_paths().unwrap(), ["a.txt"]);
    }

    #[test]
    fn commit_tree_and_compare_and_swap_branch_update() {
        let git = fixture::repo("git-commit", &[("a.txt", "alpha\n")]);
        let parent = git.rev_parse("HEAD").unwrap();
        fixture::write(git.root(), "a.txt", "beta\n");
        git.stage(&["a.txt".to_owned()]).unwrap();
        let staged_tree = git.write_tree().unwrap();
        let commit = git.commit_tree(&staged_tree, &parent, "msg\n").unwrap();
        let stale = git.advance_branch("main", &commit, &"0".repeat(40));
        assert!(
            stale.is_err(),
            "rule: the update fails if the branch is not at `expected`"
        );
        git.advance_branch("main", &commit, &parent).unwrap();
        assert_eq!(git.rev_parse("HEAD^{tree}").unwrap(), staged_tree);
        let (branches, worktrees) = git.branches_and_worktrees().unwrap();
        assert_eq!((branches.len(), worktrees), (1, 1), "branches={branches:?}");
    }
}
