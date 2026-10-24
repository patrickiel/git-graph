//! The git CLI: what the graph reads (log, refs, status, a commit's files)
//! and running what its menus do.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Between a record's fields.
const FIELD: char = '\x1f';
/// After each record.
const RECORD: char = '\x1e';

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.current_dir(dir).args(["-c", "core.quotepath=false", "-c", "color.ui=false"]).args(args);
    // Never wait on a terminal prompt nobody sees; a credential manager's window still shows.
    command.env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Run git in `dir`: its output, or what it said went wrong.
pub fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = command(dir, args).output().map_err(|e| format!("Cannot run git: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let said = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
    Err(if said.is_empty() { format!("git {} failed", args.first().unwrap_or(&"")) } else { said.to_string() })
}

/// The repository's top folder, when `dir` is in one.
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    let out = run(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    let top = out.trim();
    (!top.is_empty()).then(|| PathBuf::from(top.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    /// Seconds since 1970.
    pub time: i64,
    /// As git formats it in local time.
    pub date: String,
    pub subject: String,
    /// The message past its subject.
    pub body: String,
}

/// The date format the graph shows (git's `format-local`).
const DATE: &str = "--date=format-local:%d %b %Y %H:%M";

/// Up to `limit` commits of `revs`, newest first (children before parents).
pub fn log(top: &Path, revs: &[String], limit: usize) -> Result<Vec<Commit>, String> {
    let count = format!("--max-count={limit}");
    let format = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%ad%x1f%s%x1f%b%x1e";
    let mut args = vec!["log", "--date-order", count.as_str(), format, DATE];
    args.extend(revs.iter().map(String::as_str));
    args.push("--");
    match run(top, &args) {
        Ok(out) => Ok(parse_log(&out)),
        // A repository without commits yet has nothing to show.
        Err(err) if err.contains("does not have any commits") || err.contains("bad default revision") || err.contains("bad revision") || err.contains("unknown revision") => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

pub fn parse_log(out: &str) -> Vec<Commit> {
    out.split(RECORD)
        .filter_map(|record| {
            let record = record.trim_start_matches(['\n', '\r']);
            let mut f = record.split(FIELD);
            let hash = f.next()?.trim().to_string();
            if hash.is_empty() {
                return None;
            }
            Some(Commit {
                hash,
                parents: f.next()?.split_whitespace().map(str::to_string).collect(),
                author: f.next()?.to_string(),
                email: f.next()?.to_string(),
                time: f.next()?.trim().parse().unwrap_or(0),
                date: f.next()?.to_string(),
                subject: f.next().unwrap_or_default().trim_end().to_string(),
                body: f.next().unwrap_or_default().trim().to_string(),
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    Local,
    Remote,
    Tag,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ref {
    pub kind: RefKind,
    /// `main`, `origin/main`, `v1.0`.
    pub name: String,
    /// `refs/heads/main`.
    pub full: String,
    /// The commit it points to (an annotated tag's commit, not the tag).
    pub target: String,
}

pub fn refs(top: &Path) -> Result<Vec<Ref>, String> {
    let out = run(top, &["for-each-ref", "--format=%(refname)%1f%(objectname)%1f%(*objectname)", "refs/heads", "refs/remotes", "refs/tags"])?;
    Ok(parse_refs(&out))
}

pub fn parse_refs(out: &str) -> Vec<Ref> {
    out.lines()
        .filter_map(|line| {
            let mut f = line.split(FIELD);
            let full = f.next()?.to_string();
            let object = f.next()?.to_string();
            let peeled = f.next().unwrap_or_default();
            let target = if peeled.is_empty() { object } else { peeled.to_string() };
            let (kind, name) = if let Some(name) = full.strip_prefix("refs/heads/") {
                (RefKind::Local, name)
            } else if let Some(name) = full.strip_prefix("refs/remotes/") {
                // `origin/HEAD` only points at another branch.
                if name.ends_with("/HEAD") {
                    return None;
                }
                (RefKind::Remote, name)
            } else {
                (RefKind::Tag, full.strip_prefix("refs/tags/")?)
            };
            Some(Ref { kind, name: name.to_string(), full: full.clone(), target })
        })
        .collect()
}

/// HEAD's commit, and its branch unless it is detached.
pub fn head(top: &Path) -> (Option<String>, Option<String>) {
    let hash = run(top, &["rev-parse", "--verify", "-q", "HEAD"]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let branch = run(top, &["symbolic-ref", "-q", "--short", "HEAD"]).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    (hash, branch)
}

pub fn remotes(top: &Path) -> Vec<String> {
    run(top, &["remote"]).map(|out| out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

/// A file changed and not committed.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// Relative to the top, `/` separators.
    pub path: String,
    pub old_path: Option<String>,
    /// Porcelain's X (index) and Y (working tree): `.`-less, so ' ' for unchanged.
    pub index: char,
    pub worktree: char,
}

impl Change {
    pub fn staged(&self) -> bool {
        self.index != ' ' && self.index != '?'
    }

    pub fn unstaged(&self) -> bool {
        self.worktree != ' '
    }
}

pub fn status(top: &Path) -> Result<Vec<Change>, String> {
    Ok(parse_status(&run(top, &["status", "--porcelain=v1", "-z", "-uall"])?))
}

pub fn parse_status(out: &str) -> Vec<Change> {
    let mut parts = out.split('\0');
    let mut changes = Vec::new();
    while let Some(entry) = parts.next() {
        if entry.len() < 4 {
            continue;
        }
        let mut chars = entry.chars();
        let (index, worktree) = (chars.next().unwrap_or(' '), chars.next().unwrap_or(' '));
        let path = entry[3..].to_string();
        // A rename names where it came from next.
        let old_path = matches!(index, 'R' | 'C').then(|| parts.next().unwrap_or_default().to_string());
        changes.push(Change { path, old_path, index, worktree });
    }
    changes
}

/// A file a commit changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileChange {
    /// A, M, D, R, C, T.
    pub status: char,
    pub path: String,
    pub old_path: Option<String>,
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}

/// What the details show of a commit beyond its log line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Details {
    pub body: String,
    pub committer: String,
    pub committer_email: String,
    pub commit_date: String,
    pub files: Vec<FileChange>,
}

pub fn details(top: &Path, hash: &str, parent: Option<&str>) -> Result<Details, String> {
    let format = "--format=%B%x1f%cn%x1f%ce%x1f%cd";
    let out = run(top, &["show", "-s", format, DATE, hash])?;
    let mut f = out.split(FIELD);
    let body = f.next().unwrap_or_default().trim().to_string();
    let committer = f.next().unwrap_or_default().to_string();
    let committer_email = f.next().unwrap_or_default().to_string();
    let commit_date = f.next().unwrap_or_default().trim().to_string();
    // Against the first parent, as vscode-git-graph shows a merge.
    let (names, numbers) = match parent {
        Some(parent) => (
            run(top, &["diff", "--name-status", "-M", "-z", parent, hash])?,
            run(top, &["diff", "--numstat", "-M", "-z", parent, hash])?,
        ),
        None => (
            run(top, &["diff-tree", "--root", "-r", "-M", "--no-commit-id", "--name-status", "-z", hash])?,
            run(top, &["diff-tree", "--root", "-r", "-M", "--no-commit-id", "--numstat", "-z", hash])?,
        ),
    };
    let mut files = parse_name_status(&names);
    apply_numstat(&mut files, &numbers);
    Ok(Details { body, committer, committer_email, commit_date, files })
}

/// `--name-status -z`: `M\0path\0`, or `R100\0old\0new\0`.
pub fn parse_name_status(out: &str) -> Vec<FileChange> {
    let mut parts = out.split('\0').filter(|p| !p.is_empty());
    let mut files = Vec::new();
    while let Some(code) = parts.next() {
        let status = code.chars().next().unwrap_or('M');
        let Some(first) = parts.next() else { break };
        let file = if matches!(status, 'R' | 'C') {
            let new = parts.next().unwrap_or_default();
            FileChange { status, path: new.to_string(), old_path: Some(first.to_string()), ..Default::default() }
        } else {
            FileChange { status, path: first.to_string(), ..Default::default() }
        };
        files.push(file);
    }
    files
}

/// `--numstat -z`: `added\tdeleted\tpath\0`, or for a rename
/// `added\tdeleted\t\0old\0new\0`; `-` counts for a binary file.
pub fn apply_numstat(files: &mut [FileChange], out: &str) {
    let mut parts = out.split('\0');
    while let Some(entry) = parts.next() {
        let mut f = entry.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (f.next(), f.next(), f.next()) else { continue };
        let path = if path.is_empty() {
            _ = parts.next();
            parts.next().unwrap_or_default()
        } else {
            path
        };
        if let Some(file) = files.iter_mut().find(|file| file.path == path) {
            file.added = added.parse().ok();
            file.deleted = deleted.parse().ok();
        }
    }
}

/// What tells that the repository changed: its refs, HEAD and the files
/// changed (uncommitted).
pub fn fingerprint(top: &Path) -> String {
    let refs = run(top, &["for-each-ref", "--format=%(objectname) %(refname)"]).unwrap_or_default();
    let head = run(top, &["rev-parse", "-q", "HEAD"]).unwrap_or_default();
    let symbolic = run(top, &["symbolic-ref", "-q", "HEAD"]).unwrap_or_default();
    let status = run(top, &["status", "--porcelain=v1", "-uall"]).unwrap_or_default();
    format!("{refs}\n{head}\n{symbolic}\n{status}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_records() {
        let out = "aaa\x1fbbb ccc\x1fAda\x1fada@x\x1f1700000000\x1f14 Nov 2023 22:13\x1fMerge it\x1e\nbbb\x1f\x1fBo\x1fbo@x\x1f1600000000\x1f13 Sep 2020 12:26\x1fFirst\x1e\n";
        let commits = parse_log(out);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].parents, ["bbb", "ccc"]);
        assert_eq!(commits[0].subject, "Merge it");
        assert!(commits[1].parents.is_empty());
        assert_eq!(commits[1].time, 1600000000);
    }

    #[test]
    fn parses_refs_and_peels_tags() {
        let out = "refs/heads/main\x1fa1\x1f\nrefs/remotes/origin/HEAD\x1fa1\x1f\nrefs/remotes/origin/main\x1fa1\x1f\nrefs/tags/v1\x1ft1\x1fc1\nrefs/tags/light\x1fc2\x1f\n";
        let refs = parse_refs(out);
        assert_eq!(refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["main", "origin/main", "v1", "light"]);
        assert_eq!(refs[2].target, "c1");
        assert_eq!(refs[2].kind, RefKind::Tag);
        assert_eq!(refs[1].kind, RefKind::Remote);
    }

    #[test]
    fn parses_status_with_renames() {
        let changes = parse_status("M  src/a.rs\0R  new.rs\0old.rs\0?? x y.txt\0");
        assert_eq!(changes.len(), 3);
        assert!(changes[0].staged() && !changes[0].unstaged());
        assert_eq!(changes[1].old_path.as_deref(), Some("old.rs"));
        assert_eq!(changes[2].path, "x y.txt");
        assert!(!changes[2].staged() && changes[2].unstaged());
    }

    #[test]
    fn parses_a_commits_files_and_counts() {
        let mut files = parse_name_status("M\0src/a.rs\0R087\0old.rs\0new.rs\0A\0img.png\0");
        // A rename's counts come before its two paths.
        apply_numstat(&mut files, "3\t1\tsrc/a.rs\x002\t2\t\0old.rs\0new.rs\0-\t-\timg.png\0");
        assert_eq!(files.len(), 3);
        assert_eq!((files[0].added, files[0].deleted), (Some(3), Some(1)));
        assert_eq!(files[1].old_path.as_deref(), Some("old.rs"));
        assert_eq!((files[1].added, files[1].deleted), (Some(2), Some(2)));
        assert_eq!(files[2].added, None);
    }

    #[test]
    fn reads_a_real_repository() {
        let dir = std::env::temp_dir().join(format!("git-graph-test-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| run(&dir, args).unwrap();
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "T"]);
        git(&["config", "user.email", "t@x"]);
        assert!(log(&dir, &["HEAD".into()], 10).unwrap().is_empty());
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "first"]);
        git(&["checkout", "-q", "-b", "side"]);
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "second"]);
        git(&["checkout", "-q", "main"]);
        git(&["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
        git(&["tag", "-a", "v1", "-m", "release"]);
        let commits = log(&dir, &["--branches".into(), "--tags".into(), "HEAD".into()], 10).unwrap();
        assert_eq!(commits.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), ["merge side", "second", "first"]);
        let refs = refs(&dir).unwrap();
        assert!(refs.iter().any(|r| r.name == "v1" && r.target == commits[0].hash));
        assert_eq!(head(&dir).1.as_deref(), Some("main"));
        let details = details(&dir, &commits[0].hash, Some(&commits[0].parents[0])).unwrap();
        assert_eq!(details.files.len(), 1);
        assert_eq!((details.files[0].status, details.files[0].added), ('A', Some(1)));
        let root = details_root(&dir, &commits[2].hash);
        assert_eq!(root.files[0].path, "a.txt");
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        assert_eq!(status(&dir).unwrap()[0].worktree, 'M');
        _ = std::fs::remove_dir_all(&dir);
    }

    fn details_root(dir: &Path, hash: &str) -> Details {
        details(dir, hash, None).unwrap()
    }
}
