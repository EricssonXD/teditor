//! Standalone Git core for Source Control and its eight drawers.
//!
//! Commands run at the repository root, so porcelain's repo-relative paths
//! work even when discovery starts in a subdirectory. No UI or IPC is needed.

use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

/// The reference sidebar shows at most 30 commits in history drawers.
/// Other drawers list all entries; graph connector lines are not truncated.
pub const DRAWER_LIMIT: usize = 30;

/// One entry in the staged or unstaged list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    /// Repo-relative destination path, as reported by Git.
    pub path: String,
    /// Rename/copy source. Staging or resetting a rename must include both paths.
    pub orig: Option<String>,
    /// M, A, D, R, C, U (untracked), or ! (merge conflict).
    pub letter: char,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: String,
    pub staged: Vec<FileEntry>,
    pub unstaged: Vec<FileEntry>,
    pub ahead: usize,
    pub behind: usize,
    pub has_upstream: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drawer {
    Graph,
    Commits,
    FileHistory,
    Branches,
    Worktrees,
    Remotes,
    Stashes,
    Tags,
}

impl Drawer {
    pub const ALL: [Self; 8] = [
        Self::Graph,
        Self::Commits,
        Self::FileHistory,
        Self::Branches,
        Self::Worktrees,
        Self::Remotes,
        Self::Stashes,
        Self::Tags,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Graph => "Graph",
            Self::Commits => "Commits",
            Self::FileHistory => "File History",
            Self::Branches => "Branches",
            Self::Worktrees => "Worktrees",
            Self::Remotes => "Remotes",
            Self::Stashes => "Stashes",
            Self::Tags => "Tags",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Git {
    root: PathBuf,
}

impl Git {
    /// Find the repository containing `dir`, including worktrees/submodules.
    pub fn discover(dir: &Path) -> Result<Self, String> {
        let out = run_in(dir, &["rev-parse", "--show-toplevel"])?;
        let root = out.trim();
        if root.is_empty() {
            return Err("not inside a git repository".to_string());
        }
        Ok(Self {
            root: PathBuf::from(root),
        })
    }

    /// The containing repo first, then child repos up to two levels below
    /// `dir`, sorted by path and deduplicated by root. `.git` files count too.
    pub fn discover_all(dir: &Path) -> Vec<Self> {
        let mut repos = Vec::new();
        if let Ok(git) = Self::discover(dir) {
            repos.push(git);
        }
        let mut children = child_dirs(dir, 2);
        children.sort();
        for child in children {
            if child.join(".git").exists()
                && let Ok(git) = Self::discover(&child)
                && !repos.iter().any(|repo| repo.root == git.root)
            {
                repos.push(git);
            }
        }
        repos
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    pub fn status(&self) -> Result<Status, String> {
        let mut command = git_command(
            &self.root,
            &[
                "status",
                "--porcelain",
                "-z",
                "--branch",
                "--renames",
                "--untracked-files=all",
            ],
        );
        // Polling status need not lock/write the index, like the reference UI.
        command.env("GIT_OPTIONAL_LOCKS", "0");
        Ok(parse_status(&command_output(command)?))
    }

    /// `add -A` records additions, modifications and deletions alike.
    pub fn stage(&self, entry: &FileEntry) -> Result<(), String> {
        let mut args = vec!["add", "-A", "--", entry.path.as_str()];
        if let Some(original) = entry.orig.as_deref() {
            args.push(original);
        }
        run_in(&self.root, &args).map(drop)
    }

    pub fn stage_all(&self) -> Result<(), String> {
        run_in(&self.root, &["add", "-A"]).map(drop)
    }

    fn has_head(&self) -> bool {
        run_in(&self.root, &["rev-parse", "--verify", "HEAD"]).is_ok()
    }

    /// Reset both sides of a rename. On an unborn branch only, remove the
    /// entry from the index instead; doing that with a HEAD stages a deletion.
    pub fn unstage(&self, entry: &FileEntry) -> Result<(), String> {
        let mut args = vec!["reset", "-q", "--", entry.path.as_str()];
        if let Some(original) = entry.orig.as_deref() {
            args.push(original);
        }
        match run_in(&self.root, &args) {
            Ok(_) => Ok(()),
            Err(error) if self.has_head() => Err(error),
            Err(_) => run_in(
                &self.root,
                &["rm", "--cached", "-r", "-q", "--", &entry.path],
            )
            .map(drop),
        }
    }

    pub fn unstage_all(&self) -> Result<(), String> {
        match run_in(&self.root, &["reset", "-q"]) {
            Ok(_) => Ok(()),
            Err(error) if self.has_head() => Err(error),
            Err(_) => run_in(&self.root, &["rm", "--cached", "-r", "-q", "--", "."]).map(drop),
        }
    }

    /// Commit staged changes, returning Git's first summary line.
    pub fn commit(&self, message: &str) -> Result<String, String> {
        let out = run_in(&self.root, &["commit", "-m", message])?;
        Ok(out.lines().next().unwrap_or("committed").to_string())
    }

    pub fn diff(&self, path: &str, staged: bool) -> Result<String, String> {
        self.safe_repo_path(path)?;
        let mut args = vec!["diff", "--no-ext-diff", "--unified=3"];
        if staged {
            args.push("--cached");
        }
        args.extend(["--", path]);
        run_in(&self.root, &args)
    }

    pub fn apply_hunk(&self, patch: &str, reverse: bool) -> Result<(), String> {
        let mut args = vec!["apply", "--cached"];
        if reverse {
            args.push("--reverse");
        }
        run_with_input(&self.root, &args, patch).map(drop)
    }

    pub fn discard_path(&self, path: &str, staged: bool) -> Result<(), String> {
        self.safe_repo_path(path)?;
        let status = self.status()?;
        let entries = if staged {
            &status.staged
        } else {
            &status.unstaged
        };
        let entry = entries
            .iter()
            .find(|entry| entry.path == path)
            .ok_or_else(|| "File is no longer changed".to_string())?;
        let mut paths = vec![entry.path.as_str()];
        if let Some(original) = entry.orig.as_deref() {
            paths.push(original);
            self.safe_repo_path(original)?;
        }
        if !staged && entry.letter == 'U' {
            let file = self.safe_repo_path(path)?;
            return std::fs::remove_file(file)
                .map_err(|error| format!("Cannot delete untracked file: {error}"));
        }
        if staged && !self.has_head() {
            let mut args = vec!["rm", "-f", "--"];
            args.extend(paths);
            return run_in(&self.root, &args).map(drop);
        }
        if staged {
            let mut args = vec!["restore", "--source=HEAD", "--staged", "--worktree", "--"];
            args.extend(paths);
            run_in(&self.root, &args).map(drop)
        } else {
            let mut args = vec!["restore", "--worktree", "--"];
            args.extend(paths);
            run_in(&self.root, &args).map(drop)
        }
    }

    pub fn create_branch(&self, name: &str) -> Result<(), String> {
        self.validate_branch(name)?;
        run_in(&self.root, &["switch", "-c", name]).map(drop)
    }

    pub fn switch_branch(&self, name: &str) -> Result<(), String> {
        self.validate_branch(name)?;
        if run_in(&self.root, &["switch", "--track", name]).is_ok() {
            return Ok(());
        }
        run_in(&self.root, &["switch", name]).map(drop)
    }

    pub fn delete_branch(&self, name: &str) -> Result<(), String> {
        self.validate_branch(name)?;
        run_in(&self.root, &["branch", "-d", "--", name]).map(drop)
    }

    pub fn checkout_tag(&self, name: &str) -> Result<(), String> {
        let reference = format!("refs/tags/{name}");
        self.validate_ref(&reference)?;
        run_in(&self.root, &["switch", "--detach", name]).map(drop)
    }

    pub fn checkout_commit(&self, hash: &str) -> Result<(), String> {
        if hash.len() < 4 || hash.len() > 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("Invalid commit id".into());
        }
        let expression = format!("{hash}^{{commit}}");
        let resolved = run_in(&self.root, &["rev-parse", "--verify", &expression])?;
        run_in(&self.root, &["switch", "--detach", resolved.trim()]).map(drop)
    }

    pub fn fetch(&self) -> Result<String, String> {
        run_in(&self.root, &["fetch", "--all"])
    }

    pub fn pull(&self) -> Result<String, String> {
        run_in(&self.root, &["pull", "--ff-only"])
    }

    pub fn push(&self) -> Result<String, String> {
        if self.status()?.has_upstream {
            run_in(&self.root, &["push"])
        } else {
            run_in(&self.root, &["push", "--set-upstream", "origin", "HEAD"])
        }
    }

    pub fn stash_push(&self, message: &str) -> Result<String, String> {
        if message.trim().is_empty() {
            return Err("Stash message cannot be empty".into());
        }
        run_in(
            &self.root,
            &["stash", "push", "--include-untracked", "-m", message],
        )
    }

    pub fn stash_apply(&self, index: usize) -> Result<String, String> {
        let reference = format!("stash@{{{index}}}");
        run_in(&self.root, &["stash", "apply", &reference])
    }

    pub fn stash_drop(&self, index: usize) -> Result<String, String> {
        let reference = format!("stash@{{{index}}}");
        run_in(&self.root, &["stash", "drop", &reference])
    }

    fn validate_branch(&self, name: &str) -> Result<(), String> {
        if name.starts_with('-') || name.trim() != name || name.is_empty() {
            return Err("Invalid branch name".into());
        }
        run_in(&self.root, &["check-ref-format", "--branch", name]).map(drop)
    }

    fn validate_ref(&self, name: &str) -> Result<(), String> {
        if name.starts_with('-') || name.trim() != name || name.is_empty() {
            return Err("Invalid Git reference".into());
        }
        run_in(&self.root, &["check-ref-format", name]).map(drop)
    }

    fn safe_repo_path(&self, path: &str) -> Result<PathBuf, String> {
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| component == Component::ParentDir)
        {
            return Err("Git path must stay inside the repository".into());
        }
        Ok(self.root.join(relative))
    }

    /// History reachable from HEAD only, exactly like the reference graph.
    /// `limit` limits commits, NOT output lines (connectors are preserved).
    pub fn graph(&self, limit: usize) -> Result<Vec<String>, String> {
        let n = format!("-{limit}");
        run_in(
            &self.root,
            &["log", "--graph", "--oneline", "--decorate=short", &n],
        )
        .map(lines)
    }

    pub fn commits(&self, limit: usize) -> Result<Vec<String>, String> {
        let n = format!("-{limit}");
        run_in(
            &self.root,
            &["log", "--oneline", "--decorate=short", "--date=short", &n],
        )
        .map(lines)
    }

    /// Follow renames of the selected repo-relative path.
    pub fn file_history(&self, path: &str, limit: usize) -> Result<Vec<String>, String> {
        let n = format!("-{limit}");
        run_in(
            &self.root,
            &["log", "--oneline", "--follow", &n, "--", path],
        )
        .map(lines)
    }

    /// Local and remote branches, with the current branch starred by Git.
    pub fn branches(&self) -> Result<Vec<String>, String> {
        run_in(
            &self.root,
            &[
                "branch",
                "-a",
                "--sort=-committerdate",
                "--format=%(HEAD) %(refname:short)",
            ],
        )
        .map(lines)
    }

    /// Primary checkout first; each line includes path, short HEAD and branch.
    pub fn worktrees(&self) -> Result<Vec<String>, String> {
        run_in(&self.root, &["worktree", "list"]).map(lines)
    }

    /// One fetch URL per remote, omitting duplicate push lines.
    pub fn remotes(&self) -> Result<Vec<String>, String> {
        let out = run_in(&self.root, &["remote", "-v"])?;
        Ok(out
            .lines()
            .filter_map(|line| line.strip_suffix(" (fetch)"))
            .map(|line| line.replace('\t', "  "))
            .collect())
    }

    pub fn stashes(&self) -> Result<Vec<String>, String> {
        run_in(&self.root, &["stash", "list"]).map(lines)
    }

    pub fn tags(&self) -> Result<Vec<String>, String> {
        run_in(&self.root, &["tag", "--sort=-creatordate"]).map(lines)
    }

    /// Dispatch for drawer integration. Only the three history drawers apply
    /// `limit`. An unselected file has the reference sidebar's placeholder;
    /// empty results/errors are otherwise left to the caller to render.
    pub fn drawer_lines(
        &self,
        drawer: Drawer,
        history_target: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, String> {
        match drawer {
            Drawer::Graph => self.graph(limit),
            Drawer::Commits => self.commits(limit),
            Drawer::FileHistory => match history_target {
                Some(path) => self.file_history(path, limit),
                None => Ok(vec!["(select a file above)".to_string()]),
            },
            Drawer::Branches => self.branches(),
            Drawer::Worktrees => self.worktrees(),
            Drawer::Remotes => self.remotes(),
            Drawer::Stashes => self.stashes(),
            Drawer::Tags => self.tags(),
        }
    }
}

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .args(["-c", "color.ui=false"])
        .args(args)
        .current_dir(dir);
    command
}

fn run_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    command_output(git_command(dir, args))
}

fn run_with_input(dir: &Path, args: &[&str], input: &str) -> Result<String, String> {
    let mut child = git_command(dir, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("git: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "git stdin unavailable".to_string())?
        .write_all(input.as_bytes())
        .map_err(|error| format!("git stdin: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("git failed")
            .trim()
            .to_string())
    }
}

fn command_output(mut command: Command) -> Result<String, String> {
    let out = command.output().map_err(|error| format!("git: {error}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(String::from_utf8_lossy(&out.stderr)
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("git failed")
        .trim()
        .to_string())
}

fn lines(out: String) -> Vec<String> {
    out.lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse porcelain v1 NUL records. Rename/copy destination comes first and
/// source occupies the next NUL record. Paths are never trimmed or unquoted.
pub fn parse_status(raw: &str) -> Status {
    let mut status = Status::default();
    let mut parts = raw.split('\0');
    while let Some(entry) = parts.next() {
        if let Some(header) = entry.strip_prefix("## ") {
            status.branch = parse_branch(header);
            (status.ahead, status.behind) = parse_ahead_behind(header);
            status.has_upstream = header.contains("...");
            continue;
        }
        let bytes = entry.as_bytes();
        if bytes.len() < 4 || !bytes[0].is_ascii() || !bytes[1].is_ascii() || bytes[2] != b' ' {
            continue;
        }
        let (x, y) = (bytes[0] as char, bytes[1] as char);
        let path = entry[3..].to_string();
        let orig = if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            parts
                .next()
                .filter(|part| !part.is_empty())
                .map(str::to_string)
        } else {
            None
        };
        if x == '?' && y == '?' {
            status.unstaged.push(FileEntry {
                path,
                orig: None,
                letter: 'U',
            });
            continue;
        }
        if x == '!' {
            continue;
        }
        if is_conflict(x, y) {
            status.unstaged.push(FileEntry {
                path,
                orig,
                letter: '!',
            });
            continue;
        }
        if x != ' ' {
            status.staged.push(FileEntry {
                path: path.clone(),
                orig: matches!(x, 'R' | 'C').then(|| orig.clone()).flatten(),
                letter: display_letter(x),
            });
        }
        if y != ' ' {
            status.unstaged.push(FileEntry {
                path,
                orig: matches!(y, 'R' | 'C').then_some(orig).flatten(),
                letter: display_letter(y),
            });
        }
    }
    status
}

fn is_conflict(x: char, y: char) -> bool {
    matches!(
        (x, y),
        ('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A') | ('U', 'U')
    )
}

fn display_letter(letter: char) -> char {
    if letter == 'T' { 'M' } else { letter }
}

fn parse_branch(header: &str) -> String {
    let head = header.split("...").next().unwrap_or(header);
    head.strip_prefix("No commits yet on ")
        .unwrap_or(head)
        .to_string()
}

fn parse_ahead_behind(header: &str) -> (usize, usize) {
    let Some((_, bracket)) = header.rsplit_once('[') else {
        return (0, 0);
    };
    let bracket = bracket.trim_end_matches(']');
    let count_after = |tag: &str| {
        bracket
            .split(',')
            .map(str::trim)
            .find_map(|part| part.strip_prefix(tag))
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or(0)
    };
    (count_after("ahead "), count_after("behind "))
}

fn child_dirs(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if depth == 0 {
        return out;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir()
            || matches!(
                entry.file_name().to_string_lossy().as_ref(),
                ".git" | "target" | "node_modules" | ".claude"
            )
        {
            continue;
        }
        out.push(path.clone());
        out.extend(child_dirs(&path, depth - 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn entry(path: &str, letter: char, orig: Option<&str>) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            letter,
            orig: orig.map(str::to_string),
        }
    }

    #[test]
    fn porcelain_preserves_paths_and_separates_status_sides() {
        let status = parse_status(concat!(
            "## main...origin/main [ahead 2, behind 3]\0",
            "MM both.rs\0",
            "A  added.rs\0",
            " D deleted.rs\0",
            "RM new name.rs\0old name.rs\0",
            " C copy.rs\0source.rs\0",
            "T  link\0",
            "?? dir/雪\t\n\"file\" \0",
            "!! ignored\0",
            "bad\0\0"
        ));
        assert_eq!(status.branch, "main");
        assert_eq!((status.ahead, status.behind), (2, 3));
        assert!(status.has_upstream);
        assert_eq!(
            status.staged,
            vec![
                entry("both.rs", 'M', None),
                entry("added.rs", 'A', None),
                entry("new name.rs", 'R', Some("old name.rs")),
                entry("link", 'M', None),
            ]
        );
        assert_eq!(
            status.unstaged,
            vec![
                entry("both.rs", 'M', None),
                entry("deleted.rs", 'D', None),
                entry("new name.rs", 'M', None),
                entry("copy.rs", 'C', Some("source.rs")),
                entry("dir/雪\t\n\"file\" ", 'U', None),
            ]
        );
    }

    #[test]
    fn porcelain_branch_states_and_conflicts() {
        for (raw, branch, upstream, ahead, behind) in [
            ("## No commits yet on trunk\0", "trunk", false, 0, 0),
            ("## HEAD (no branch)\0", "HEAD (no branch)", false, 0, 0),
            ("## topic\0", "topic", false, 0, 0),
            ("## topic...origin/topic [gone]\0", "topic", true, 0, 0),
            ("## topic...origin/topic [ahead 4]\0", "topic", true, 4, 0),
            ("## topic...origin/topic [behind 5]\0", "topic", true, 0, 5),
        ] {
            let status = parse_status(raw);
            assert_eq!(status.branch, branch);
            assert_eq!(status.has_upstream, upstream);
            assert_eq!((status.ahead, status.behind), (ahead, behind));
        }
        for xy in ["DD", "AU", "UD", "UA", "DU", "AA", "UU"] {
            let status = parse_status(&format!("{xy} conflict.rs\0"));
            assert!(status.staged.is_empty());
            assert_eq!(status.unstaged, vec![entry("conflict.rs", '!', None)]);
        }
        assert_eq!(parse_status("\0x\0é file\0!! ignored\0"), Status::default());
    }

    // Each test owns its directory, including during parallel test execution.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "teditor-scm-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn init(dir: &Path) -> Git {
        fs::create_dir_all(dir).unwrap();
        run_in(dir, &["init", "--initial-branch=main"]).unwrap();
        for (key, value) in [
            ("user.name", "SCM Test"),
            ("user.email", "scm@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
            ("core.hooksPath", ".git/no-test-hooks"),
        ] {
            run_in(dir, &["config", key, value]).unwrap();
        }
        Git::discover(dir).unwrap()
    }

    fn commit_file(git: &Git, path: &str, text: &str, message: &str) {
        fs::write(git.root().join(path), text).unwrap();
        git.stage(&entry(path, 'M', None)).unwrap();
        git.commit(message).unwrap();
    }

    #[test]
    fn graph_matches_head_only_git_log_in_a_branching_repo() {
        let temp = TempDir::new();
        let git = init(&temp.0);
        commit_file(&git, "base", "base", "base commit");
        run_in(git.root(), &["checkout", "-b", "topic"]).unwrap();
        commit_file(&git, "topic", "topic", "topic commit");
        run_in(git.root(), &["checkout", "main"]).unwrap();
        commit_file(&git, "main", "main", "main commit");
        run_in(
            git.root(),
            &["merge", "--no-ff", "topic", "-m", "merge topic"],
        )
        .unwrap();
        run_in(git.root(), &["checkout", "-b", "unmerged"]).unwrap();
        commit_file(&git, "hidden", "hidden", "unmerged-only commit");
        run_in(git.root(), &["checkout", "main"]).unwrap();

        for limit in [0, 1, 2, 3, 30] {
            let n = format!("-{limit}");
            let expected = lines(
                run_in(
                    git.root(),
                    &["log", "--graph", "--oneline", "--decorate=short", &n],
                )
                .unwrap(),
            );
            let graph = git.graph(limit).unwrap();
            assert_eq!(graph, expected);
            assert!(!graph.iter().any(|line| line.contains("unmerged-only")));
            assert_eq!(git.commits(limit).unwrap().len(), limit.min(4));
        }
        let graph = git.graph(30).unwrap();
        assert!(graph.iter().any(|line| line.contains("merge topic")));
        assert!(graph.iter().any(|line| line.contains("topic commit")));
        assert!(graph.iter().any(|line| line.contains("|\\")));
        assert!(git.graph(2).unwrap().len() > 2, "keep graph connectors");
        let all = run_in(git.root(), &["log", "--all", "--oneline"]).unwrap();
        assert!(
            all.contains("unmerged-only commit"),
            "fixture must detect --all"
        );
    }

    #[test]
    fn tag_and_commit_checkout_detach_and_validate_commit_ids() {
        let temp = TempDir::new();
        let git = init(&temp.0);
        commit_file(&git, "file.txt", "base\n", "base");
        let head = run_in(git.root(), &["rev-parse", "HEAD"]).unwrap();
        run_in(git.root(), &["tag", "v1"]).unwrap();

        git.checkout_tag("v1").unwrap();
        assert!(git.status().unwrap().branch.starts_with("HEAD"));
        assert_eq!(
            run_in(git.root(), &["rev-parse", "HEAD"]).unwrap().trim(),
            head.trim()
        );
        git.switch_branch("main").unwrap();
        git.checkout_commit(head.trim()).unwrap();
        assert!(git.status().unwrap().branch.starts_with("HEAD"));
        assert!(git.checkout_commit("not-a-commit").is_err());
        assert!(git.checkout_commit("deadbeef").is_err());
    }

    #[test]
    fn branch_discard_and_hunk_operations_are_safe() {
        let temp = TempDir::new();
        let git = init(&temp.0);
        commit_file(&git, "file.txt", "one\ntwo\nthree\n", "base");

        git.create_branch("topic").unwrap();
        assert_eq!(git.status().unwrap().branch, "topic");
        git.switch_branch("main").unwrap();
        git.delete_branch("topic").unwrap();
        assert!(git.create_branch("../outside").is_err());
        assert!(git.switch_branch("--help").is_err());

        fs::write(git.root().join("file.txt"), "ONE\ntwo\nthree\n").unwrap();
        let patch = git.diff("file.txt", false).unwrap();
        git.apply_hunk(&patch, false).unwrap();
        assert_eq!(git.status().unwrap().staged.len(), 1);
        let staged_patch = git.diff("file.txt", true).unwrap();
        git.apply_hunk(&staged_patch, true).unwrap();
        assert!(git.status().unwrap().staged.is_empty());
        assert!(!git.status().unwrap().unstaged.is_empty());
        git.discard_path("file.txt", false).unwrap();
        assert_eq!(
            fs::read_to_string(git.root().join("file.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );

        fs::write(git.root().join("file.txt"), "staged\n").unwrap();
        git.stage(&entry("file.txt", 'M', None)).unwrap();
        git.discard_path("file.txt", true).unwrap();
        assert_eq!(
            fs::read_to_string(git.root().join("file.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );

        fs::write(git.root().join("loose.txt"), "loose").unwrap();
        git.discard_path("loose.txt", false).unwrap();
        assert!(!git.root().join("loose.txt").exists());
    }

    #[test]
    fn local_remote_push_fetch_and_fast_forward_pull_work() {
        let temp = TempDir::new();
        let remote_path = temp.0.join("origin.git");
        run_in(
            &temp.0,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote_path.to_str().unwrap(),
            ],
        )
        .unwrap();
        let local_path = temp.0.join("local");
        let local = init(&local_path);
        commit_file(&local, "file.txt", "base\n", "base");
        run_in(
            local.root(),
            &["remote", "add", "origin", remote_path.to_str().unwrap()],
        )
        .unwrap();
        local.push().unwrap();

        let peer_path = temp.0.join("peer");
        run_in(
            &temp.0,
            &[
                "clone",
                remote_path.to_str().unwrap(),
                peer_path.to_str().unwrap(),
            ],
        )
        .unwrap();
        run_in(&peer_path, &["config", "user.name", "Peer"]).unwrap();
        run_in(
            &peer_path,
            &["config", "user.email", "peer@example.invalid"],
        )
        .unwrap();
        fs::write(peer_path.join("file.txt"), "peer\n").unwrap();
        run_in(&peer_path, &["add", "file.txt"]).unwrap();
        run_in(&peer_path, &["commit", "-qm", "peer update"]).unwrap();
        run_in(&peer_path, &["push"]).unwrap();

        local.fetch().unwrap();
        assert_eq!(local.status().unwrap().behind, 1);
        local.pull().unwrap();
        assert_eq!(
            fs::read_to_string(local.root().join("file.txt")).unwrap(),
            "peer\n"
        );
        fs::write(local.root().join("local.txt"), "local\n").unwrap();
        local.stage(&entry("local.txt", 'A', None)).unwrap();
        local.commit("local update").unwrap();
        local.push().unwrap();
        assert!(run_in(&peer_path, &["pull", "--ff-only"]).is_ok());
        assert!(peer_path.join("local.txt").exists());

        run_in(&peer_path, &["switch", "-c", "topic"]).unwrap();
        fs::write(peer_path.join("topic.txt"), "remote branch\n").unwrap();
        run_in(&peer_path, &["add", "topic.txt"]).unwrap();
        run_in(&peer_path, &["commit", "-qm", "topic branch"]).unwrap();
        run_in(&peer_path, &["push", "-u", "origin", "topic"]).unwrap();
        local.fetch().unwrap();
        local.switch_branch("origin/topic").unwrap();
        assert_eq!(local.status().unwrap().branch, "topic");
    }

    #[test]
    fn stash_actions_apply_and_drop_the_selected_entry() {
        let temp = TempDir::new();
        let git = init(&temp.0);
        commit_file(&git, "file.txt", "base\n", "base");
        fs::write(git.root().join("file.txt"), "changed\n").unwrap();
        git.stash_push("test stash").unwrap();
        assert_eq!(git.stashes().unwrap().len(), 1);
        git.stash_apply(0).unwrap();
        assert_eq!(
            fs::read_to_string(git.root().join("file.txt")).unwrap(),
            "changed\n"
        );
        git.stash_drop(0).unwrap();
        assert!(git.stashes().unwrap().is_empty());
    }

    #[test]
    fn stage_unstage_and_commit_on_unborn_and_existing_head() {
        let temp = TempDir::new();
        let git = init(&temp.0);
        fs::write(git.root().join("-option file"), "initial\n").unwrap();
        let file = entry("-option file", 'U', None);
        git.stage(&file).unwrap();
        assert_eq!(
            git.status().unwrap().staged,
            vec![entry("-option file", 'A', None)]
        );
        git.unstage(&file).unwrap();
        assert!(git.status().unwrap().staged.is_empty());
        assert!(git.root().join(&file.path).exists());
        git.stage_all().unwrap();
        git.unstage_all().unwrap();
        assert!(git.status().unwrap().staged.is_empty());
        git.stage_all().unwrap();
        let summary = git.commit("initial subject\n\nmessage body").unwrap();
        assert!(summary.contains("initial subject"));
        assert!(git.status().unwrap().unstaged.is_empty());

        fs::write(git.root().join(&file.path), "modified\n").unwrap();
        git.stage(&file).unwrap();
        git.unstage(&file).unwrap();
        assert_eq!(
            git.status().unwrap().unstaged,
            vec![entry("-option file", 'M', None)]
        );
        git.stage_all().unwrap();
        git.unstage_all().unwrap();
        assert!(git.status().unwrap().staged.is_empty());
        assert_eq!(
            fs::read_to_string(git.root().join(&file.path)).unwrap(),
            "modified\n"
        );
        assert!(git.commit("nothing staged").is_err());
    }

    #[test]
    fn rename_stage_and_unstage_include_both_paths() {
        let temp = TempDir::new();
        let git = init(&temp.0);
        commit_file(&git, "-old name", "unchanged\n", "initial");
        fs::rename(git.root().join("-old name"), git.root().join("-new name")).unwrap();
        let rename = entry("-new name", 'R', Some("-old name"));
        git.stage(&rename).unwrap();
        assert_eq!(git.status().unwrap().staged, vec![rename.clone()]);
        git.unstage(&rename).unwrap();
        let status = git.status().unwrap();
        assert!(status.staged.is_empty());
        assert!(status.unstaged.contains(&entry("-old name", 'D', None)));
        assert!(status.unstaged.contains(&entry("-new name", 'U', None)));
    }

    #[test]
    fn discovery_finds_root_children_and_git_files_only_to_depth_two() {
        let temp = TempDir::new();
        let git = init(&temp.0);
        commit_file(&git, "base", "base", "initial");
        init(&temp.0.join("a"));
        init(&temp.0.join("group/b"));
        init(&temp.0.join("group/deep/c"));
        init(&temp.0.join("target/skipped"));
        fs::create_dir_all(temp.0.join("plain")).unwrap();
        let worktree = temp.0.join("worktree");
        run_in(
            git.root(),
            &[
                "worktree",
                "add",
                "-b",
                "linked",
                worktree.to_str().unwrap(),
            ],
        )
        .unwrap();
        assert!(worktree.join(".git").is_file());
        assert_eq!(
            Git::discover(&temp.0.join("plain")).unwrap().root(),
            git.root()
        );
        let roots: Vec<PathBuf> = Git::discover_all(&temp.0)
            .into_iter()
            .map(|repo| repo.root)
            .collect();
        assert_eq!(
            roots,
            vec![git.root, temp.0.join("a"), temp.0.join("group/b"), worktree]
        );

        let nonrepo = TempDir::new();
        init(&nonrepo.0.join("group/child"));
        assert_eq!(Git::discover_all(&nonrepo.0).len(), 1);
        assert!(Git::discover(&nonrepo.0).is_err());
    }

    #[test]
    fn other_drawers_follow_upstream_commands_and_limit_semantics() {
        let temp = TempDir::new();
        let git = init(&temp.0);
        commit_file(&git, "-old file", "first\n", "first");
        run_in(git.root(), &["mv", "--", "-old file", "-new file"]).unwrap();
        git.commit("renamed").unwrap();
        commit_file(&git, "-new file", "second\n", "second");
        assert_eq!(git.file_history("-new file", 1).unwrap().len(), 1);
        assert_eq!(git.file_history("-new file", 0).unwrap().len(), 0);
        let history = git.file_history("-new file", 30).unwrap();
        assert_eq!(history.len(), 3);
        assert!(history.last().unwrap().ends_with("first"));
        assert!(git.file_history("missing", 30).unwrap().is_empty());

        run_in(git.root(), &["branch", "topic"]).unwrap();
        run_in(
            git.root(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        )
        .unwrap();
        run_in(git.root(), &["tag", "v1"]).unwrap();
        run_in(git.root(), &["tag", "v2"]).unwrap();
        fs::write(git.root().join("-new file"), "stash me\n").unwrap();
        run_in(git.root(), &["stash", "push", "-m", "saved changes"]).unwrap();
        for (drawer, args) in [
            (
                Drawer::Branches,
                vec![
                    "branch",
                    "-a",
                    "--sort=-committerdate",
                    "--format=%(HEAD) %(refname:short)",
                ],
            ),
            (Drawer::Worktrees, vec!["worktree", "list"]),
            (Drawer::Stashes, vec!["stash", "list"]),
            (Drawer::Tags, vec!["tag", "--sort=-creatordate"]),
        ] {
            let expected = lines(run_in(git.root(), &args).unwrap());
            assert_eq!(git.drawer_lines(drawer, None, 0).unwrap(), expected);
        }
        assert_eq!(
            git.tags().unwrap().len(),
            2,
            "non-history drawers are not limited"
        );
        assert!(git.branches().unwrap().contains(&"* main".to_string()));
        assert_eq!(
            git.remotes().unwrap(),
            vec!["origin  https://example.invalid/repo.git"]
        );
        assert_eq!(
            git.drawer_lines(Drawer::FileHistory, None, DRAWER_LIMIT)
                .unwrap(),
            vec!["(select a file above)"]
        );
        assert_eq!(
            Drawer::ALL.map(Drawer::title),
            [
                "Graph",
                "Commits",
                "File History",
                "Branches",
                "Worktrees",
                "Remotes",
                "Stashes",
                "Tags"
            ]
        );
    }
}
