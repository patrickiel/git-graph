//! Git Graph for den, after mhutchie's vscode-git-graph: the repository's
//! commits as a graph in a tab of its own, with its branches, remote branches
//! and tags, a commit's details and files underneath, and the branch and
//! commit actions in right-click menus.
//!
//! den draws the tab from data (`den_extension::view`): this extension runs
//! git, lays out the lanes (`graph.rs`) and sends rows, menus and details;
//! clicks and menu picks come back as `view_action`, the answers to its
//! questions as `prompt_answered`. A worker thread owns every repository's
//! state and takes den's events over a channel; between them it looks for
//! changes to the repository (a commit, a checkout, a fetch) and redraws.

mod git;
mod graph;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use den_extension::view::{Column, Content, Diff, Field, Line, MenuItem, Prompt, Row, SelectOption, Span, ToolbarItem, actions};
use den_extension::{Button, ButtonLocation, Context, Extension, Host, events, register};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::git::{Change, Commit, Ref, RefKind};

/// The view's id in the manifest.
const VIEW: &str = "graph";
/// The row of the uncommitted changes.
const UNCOMMITTED: &str = "*";
/// How often an open graph looks for changes to its repository.
const POLL: Duration = Duration::from_secs(3);
/// "All branches" in the branch filter.
const ALL: &str = "";

// -- Settings ------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct Config {
    initial_commits: u64,
    show_remote_branches: bool,
    show_tags: bool,
    show_uncommitted: bool,
    /// `relative` or `absolute`.
    date_format: String,
    dim_unrelated: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            initial_commits: 300,
            show_remote_branches: true,
            show_tags: true,
            show_uncommitted: true,
            date_format: "relative".into(),
            dim_unrelated: false,
        }
    }
}

fn config(settings: serde_json::Map<String, Value>) -> Config {
    serde_json::from_value(Value::Object(settings)).unwrap_or_default()
}

// -- The extension ---------------------------------------------------------------

enum Job {
    Event(String, Value),
    /// A git command run off the worker (fetch, push) finished.
    Done { root: String, what: String, result: Result<String, String> },
    Stop,
}

struct GitGraph {
    jobs: Sender<Job>,
    thread: Option<JoinHandle<()>>,
}

impl Extension for GitGraph {
    fn activate(host: Host, context: Context) -> Self {
        let (jobs, rx) = mpsc::channel();
        let config = config(context.settings);
        let worker_jobs = jobs.clone();
        let thread = std::thread::Builder::new()
            .name("git-graph".into())
            .spawn(move || Worker::new(host, config, worker_jobs).run(rx))
            .ok();
        GitGraph { jobs, thread }
    }

    fn event(&mut self, name: &str, data: Value) {
        _ = self.jobs.send(Job::Event(name.to_string(), data));
    }

    fn deactivate(&mut self) {
        _ = self.jobs.send(Job::Stop);
        if let Some(thread) = self.thread.take() {
            _ = thread.join();
        }
    }
}

register!(GitGraph);

// -- The worker ----------------------------------------------------------------

/// A repository as its graph shows it, for the window on `root`.
struct Repo {
    /// The folder as den named it.
    root: String,
    top: PathBuf,
    /// A graph tab is open on it.
    open: bool,
    limit: usize,
    /// A branch's full ref name, or [`ALL`].
    branch: String,
    show_remotes: bool,
    commits: Vec<Commit>,
    more: bool,
    refs: Vec<Ref>,
    head: Option<String>,
    head_branch: Option<String>,
    changes: Vec<Change>,
    remotes: Vec<String>,
    /// Each row's lane, for its badges' colour.
    lanes: Vec<usize>,
    search: String,
    matches: Vec<String>,
    match_ix: usize,
    selected: Option<String>,
    fingerprint: String,
    checked: Instant,
}

/// A question asked, waiting for its answer.
struct Pending {
    root: String,
    op: String,
    /// The commit, branch, tag or file it is about.
    target: String,
}

/// A file of the details, as its line's data.
#[derive(Serialize, Deserialize)]
struct FileRef {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    old: Option<String>,
    /// The commit, none for an uncommitted change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(default)]
    staged: bool,
}

struct Worker {
    host: Host,
    config: Config,
    repos: HashMap<String, Repo>,
    pending: HashMap<String, Pending>,
    next_prompt: u64,
    /// For work done on threads of its own to report back.
    jobs: Sender<Job>,
}

impl Worker {
    fn new(host: Host, config: Config, jobs: Sender<Job>) -> Self {
        Worker { host, config, repos: HashMap::new(), pending: HashMap::new(), next_prompt: 0, jobs }
    }

    fn run(mut self, rx: Receiver<Job>) {
        loop {
            match rx.recv_timeout(POLL) {
                Ok(Job::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Job::Event(name, data)) => self.event(&name, data),
                Ok(Job::Done { root, what, result }) => {
                    if let Err(err) = result {
                        self.host.toast(format!("{what} failed: {}", first_lines(&err)));
                    }
                    self.reload(&root);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.poll();
        }
    }

    fn event(&mut self, name: &str, mut data: Value) {
        let root = data["root"].as_str().unwrap_or_default().to_string();
        let text = |key: &str| data[key].as_str().map(str::to_string);
        match name {
            events::WORKSPACE_OPENED => self.set_button(&root),
            events::BUTTON_CLICKED | events::COMMAND => match text("id").as_deref() {
                Some("show") => self.show(&root),
                Some("fetch") => self.fetch(&root),
                _ => {}
            },
            events::VIEW_OPENED if text("view").as_deref() == Some(VIEW) => self.opened(&root),
            events::VIEW_CLOSED if text("view").as_deref() == Some(VIEW) => {
                // Its state goes; the next tab starts afresh.
                self.repos.remove(&root);
            }
            events::VIEW_ACTION if text("view").as_deref() == Some(VIEW) => {
                let (action, row, target, value) = (text("action").unwrap_or_default(), text("row"), text("data"), text("value"));
                self.action(&root, &action, row, target, value);
            }
            events::PROMPT_ANSWERED => {
                let Some(pending) = text("id").and_then(|id| self.pending.remove(&id)) else { return };
                if let Value::Object(values) = data["values"].take() {
                    self.answered(pending, values);
                }
            }
            events::SETTINGS_CHANGED => {
                if let Value::Object(settings) = data["settings"].take() {
                    self.config = config(settings);
                    let roots: Vec<String> = self.repos.keys().cloned().collect();
                    for root in roots {
                        self.reload(&root);
                    }
                }
            }
            _ => {}
        }
    }

    fn set_button(&self, root: &str) {
        if git::toplevel(Path::new(root)).is_none() {
            return;
        }
        let button = Button {
            id: "show".into(),
            label: "Git Graph".into(),
            icon: "git-fork".into(),
            tooltip: "Show the Git Graph".into(),
            location: ButtonLocation::SourceControl,
            ..Default::default()
        };
        _ = self.host.set_buttons(root, &[button]);
    }

    fn show(&self, root: &str) {
        if git::toplevel(Path::new(root)).is_none() {
            return self.host.toast("This folder isn't in a git repository.");
        }
        if let Err(err) = self.host.open_view(root, VIEW) {
            self.host.log(format!("open_view: {err}"));
        }
    }

    /// A graph tab opened: read the repository and show it.
    fn opened(&mut self, root: &str) {
        let Some(top) = git::toplevel(Path::new(root)) else {
            let content = Content { message: Some("This folder isn't in a git repository.".into()), rows: Some(Vec::new()), ..Default::default() };
            _ = self.host.set_view(root, VIEW, &content);
            return;
        };
        let repo = self.repos.entry(root.to_string()).or_insert_with(|| Repo {
            root: root.to_string(),
            top,
            open: true,
            limit: self.config.initial_commits.max(1) as usize,
            branch: ALL.into(),
            show_remotes: self.config.show_remote_branches,
            commits: Vec::new(),
            more: false,
            refs: Vec::new(),
            head: None,
            head_branch: None,
            changes: Vec::new(),
            remotes: Vec::new(),
            lanes: Vec::new(),
            search: String::new(),
            matches: Vec::new(),
            match_ix: 0,
            selected: None,
            fingerprint: String::new(),
            checked: Instant::now(),
        });
        repo.open = true;
        let content = Content { message: Some("Loading…".into()), rows: Some(Vec::new()), ..Default::default() };
        _ = self.host.set_view(root, VIEW, &content);
        self.reload(root);
    }

    /// Read the repository again and redraw all of it.
    fn reload(&mut self, root: &str) {
        let config = self.config.clone();
        let Some(repo) = self.repos.get_mut(root) else { return };
        if let Err(err) = load(repo, &config) {
            let content = Content { message: Some(format!("Couldn't read the repository: {}", first_lines(&err))), rows: Some(Vec::new()), ..Default::default() };
            _ = self.host.set_view(root, VIEW, &content);
            return;
        }
        let mut content = full_content(repo, &config);
        if let Some(selected) = &repo.selected {
            content.detail = Some(detail(repo, selected));
        }
        _ = self.host.set_view(root, VIEW, &content);
    }

    /// Redraw the open graphs whose repository changed.
    fn poll(&mut self) {
        let due: Vec<String> = self.repos.values().filter(|r| r.open && r.checked.elapsed() >= POLL).map(|r| r.root.clone()).collect();
        for root in due {
            let Some(repo) = self.repos.get_mut(&root) else { continue };
            repo.checked = Instant::now();
            if git::fingerprint(&repo.top) != repo.fingerprint {
                self.reload(&root);
            }
        }
    }

    fn action(&mut self, root: &str, action: &str, row: Option<String>, target: Option<String>, value: Option<String>) {
        let config = self.config.clone();
        let Some(repo) = self.repos.get_mut(root) else { return };
        let target = target.unwrap_or_default();
        match action {
            actions::SELECT | actions::OPEN => {
                repo.selected = row.filter(|r| !r.is_empty());
                let content = Content {
                    selected: Some(repo.selected.clone().unwrap_or_default()),
                    detail: Some(repo.selected.as_ref().map(|s| detail(repo, s)).unwrap_or_default()),
                    ..Default::default()
                };
                _ = self.host.set_view(root, VIEW, &content);
            }
            actions::MORE => {
                repo.limit += config.initial_commits.max(1) as usize;
                self.reload(root);
            }
            "branch" => {
                repo.branch = value.unwrap_or_default();
                self.reload(root);
            }
            "remotes" => {
                repo.show_remotes = !repo.show_remotes;
                self.reload(root);
            }
            "refresh" => self.reload(root),
            "fetch" => self.fetch(root),
            "find" => {
                let value = value.unwrap_or_default();
                // Enter sends the text again: that is the next match.
                if value == repo.search && !value.is_empty() {
                    self.step_match(root, 1);
                } else {
                    repo.search = value;
                    find(repo);
                    self.show_match(root);
                }
            }
            "find-next" => self.step_match(root, 1),
            "find-prev" => self.step_match(root, -1),
            "goto" => {
                if repo.commits.iter().any(|c| c.hash == target) {
                    self.action(root, actions::SELECT, Some(target), None, None);
                } else {
                    self.host.toast("That commit isn't loaded; load more commits to see it.");
                }
            }
            "diff" | "file-diff" => self.open_diff(root, &target),
            "file-open" => {
                if let Ok(file) = serde_json::from_str::<FileRef>(&target) {
                    let path = repo.top.join(file.path.replace('/', std::path::MAIN_SEPARATOR_STR));
                    _ = self.host.open_file(root, &path.to_string_lossy(), None);
                }
            }
            "copy-hash" => _ = self.host.copy(&target),
            "file-copy-path" => {
                if let Ok(file) = serde_json::from_str::<FileRef>(&target) {
                    _ = self.host.copy(&file.path);
                }
            }
            _ => self.menu_action(root, action, row.unwrap_or_default(), target),
        }
    }

    fn step_match(&mut self, root: &str, step: isize) {
        let Some(repo) = self.repos.get_mut(root) else { return };
        if repo.matches.is_empty() {
            return;
        }
        let n = repo.matches.len() as isize;
        repo.match_ix = ((repo.match_ix as isize + step).rem_euclid(n)) as usize;
        self.show_match(root);
    }

    /// Select the current match and show how far through them it is.
    fn show_match(&mut self, root: &str) {
        let config = self.config.clone();
        let Some(repo) = self.repos.get_mut(root) else { return };
        if let Some(hash) = repo.matches.get(repo.match_ix).cloned() {
            repo.selected = Some(hash);
        }
        let content = Content {
            toolbar: Some(toolbar(repo, &config)),
            highlighted: Some(repo.matches.clone()),
            selected: Some(repo.selected.clone().unwrap_or_default()),
            detail: Some(repo.selected.as_ref().map(|s| detail(repo, s)).unwrap_or_default()),
            ..Default::default()
        };
        _ = self.host.set_view(root, VIEW, &content);
    }

    fn fetch(&mut self, root: &str) {
        let Some(top) = git::toplevel(Path::new(root)) else { return };
        self.host.toast("Fetching from all remotes…");
        self.background(root, "Fetch", top, vec!["fetch".into(), "--all".into(), "--prune".into()]);
    }

    /// Run git on a thread of its own (it may take a while, or ask for a
    /// login); the graph redraws when it is done.
    fn background(&self, root: &str, what: &str, top: PathBuf, args: Vec<String>) {
        let (jobs, root, what) = (self.jobs.clone(), root.to_string(), what.to_string());
        std::thread::spawn(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let result = git::run(&top, &args);
            _ = jobs.send(Job::Done { root, what, result });
        });
    }

    fn open_diff(&self, root: &str, target: &str) {
        let Ok(file) = serde_json::from_str::<FileRef>(target) else { return };
        let diff = Diff { path: file.path, hash: file.hash, parent: file.parent, old_path: file.old, staged: file.staged };
        if let Err(err) = self.host.open_diff(root, &diff) {
            self.host.log(format!("open_diff: {err}"));
        }
    }

    /// Run git now, then redraw; a failure is shown.
    fn git(&mut self, root: &str, what: &str, args: &[&str]) {
        let Some(top) = self.repos.get(root).map(|r| r.top.clone()) else { return };
        if let Err(err) = git::run(&top, args) {
            self.host.toast(format!("{what} failed: {}", first_lines(&err)));
        }
        self.reload(root);
    }

    fn ask(&mut self, root: &str, op: &str, target: &str, mut prompt: Prompt) {
        self.next_prompt += 1;
        prompt.id = format!("{op}-{}", self.next_prompt);
        self.pending.insert(prompt.id.clone(), Pending { root: root.to_string(), op: op.to_string(), target: target.to_string() });
        if let Err(err) = self.host.prompt(root, &prompt) {
            self.host.log(format!("prompt: {err}"));
        }
    }

    /// A menu pick: what needs nothing more runs now, the rest asks first.
    fn menu_action(&mut self, root: &str, op: &str, row: String, data: String) {
        let Some(repo) = self.repos.get(root) else { return };
        let commit = repo.commits.iter().find(|c| c.hash == row);
        let short = short(&row);
        let current = repo.head_branch.clone().unwrap_or_else(|| "HEAD".into());
        let remote_choice = |default: &str| {
            let options = repo.remotes.iter().map(|r| SelectOption { value: r.clone(), label: r.clone() }).collect();
            let default = if repo.remotes.iter().any(|r| r == default) { default.to_string() } else { repo.remotes.first().cloned().unwrap_or_default() };
            Field::choice("remote", "Remote", default, options)
        };
        let prompt = |title: String, message: String, fields: Vec<Field>, ok: &str| Prompt { title, message, fields, ok: ok.into(), ..Default::default() };
        let danger = |p: Prompt| Prompt { danger: true, ..p };
        let question = match op {
            // A commit's menu (the row is the commit).
            "commit-tag" => Some(prompt(
                "Add Tag".into(),
                format!("A tag on commit {short}."),
                vec![Field::text("name", "Name", "").required(), Field::text("message", "Message (makes it an annotated tag)", ""), Field::checkbox("push", "Push it to the remote", false)],
                "Add Tag",
            )),
            "commit-branch" => Some(prompt(
                "Create Branch".into(),
                format!("A branch at commit {short}."),
                vec![Field::text("name", "Name", "").required(), Field::checkbox("checkout", "Check it out", true)],
                "Create Branch",
            )),
            "commit-checkout" => Some(prompt("Checkout Commit".into(), format!("Check out commit {short}? HEAD will be detached."), Vec::new(), "Checkout")),
            "commit-cherry-pick" => Some(prompt(
                "Cherry Pick".into(),
                format!("Cherry pick commit {short} onto {current}?"),
                vec![Field::checkbox("no_commit", "Don't commit (leave the changes staged)", false)],
                "Cherry Pick",
            )),
            "commit-revert" => Some(prompt("Revert".into(), format!("Revert commit {short} on {current}?"), Vec::new(), "Revert")),
            "commit-merge" | "branch-merge" | "remote-merge" => {
                let what = if op == "commit-merge" { format!("commit {short}") } else { format!("branch {data}") };
                Some(prompt(
                    "Merge".into(),
                    format!("Merge {what} into {current}?"),
                    vec![Field::checkbox("no_ff", "Create a new commit even if fast-forward is possible", true), Field::checkbox("squash", "Squash commits", false)],
                    "Merge",
                ))
            }
            "commit-rebase" | "branch-rebase" => {
                let onto = if op == "commit-rebase" { format!("commit {short}") } else { format!("branch {data}") };
                Some(prompt("Rebase".into(), format!("Rebase {current} on {onto}?"), Vec::new(), "Rebase"))
            }
            "commit-reset" => {
                let modes = [("soft", "Soft: keep all changes, staged"), ("mixed", "Mixed: keep the working tree, reset the index"), ("hard", "Hard: discard all changes")];
                let options = modes.iter().map(|(v, l)| SelectOption { value: v.to_string(), label: l.to_string() }).collect();
                Some(danger(prompt("Reset".into(), format!("Reset {current} to commit {short}?"), vec![Field::choice("mode", "", "mixed", options)], "Reset")))
            }
            "commit-copy-hash" => return drop(self.host.copy(&row)),
            "commit-copy-subject" => return drop(self.host.copy(commit.map_or("", |c| c.subject.as_str()))),

            // A branch's menu (the data is its name).
            "branch-checkout" => return self.git(root, "Checkout", &["checkout", &data]),
            "branch-rename" => Some(prompt("Rename Branch".into(), format!("Rename branch {data}."), vec![Field::text("name", "New name", &data).required()], "Rename")),
            "branch-delete" => Some(danger(prompt(
                "Delete Branch".into(),
                format!("Delete branch {data}?"),
                vec![Field::checkbox("force", "Force delete (even if not merged)", false)],
                "Delete",
            ))),
            "branch-push" => Some(prompt(
                "Push Branch".into(),
                format!("Push branch {data}."),
                vec![remote_choice("origin"), Field::checkbox("upstream", "Set upstream", true), Field::checkbox("force", "Force (with lease)", false)],
                "Push",
            )),
            "branch-pull" => {
                let Some(top) = self.repos.get(root).map(|r| r.top.clone()) else { return };
                return self.background(root, "Pull", top, vec!["pull".into()]);
            }
            "branch-copy" | "remote-copy" | "tag-copy" => return drop(self.host.copy(&data)),

            // A remote branch's menu (the data is `remote/branch`).
            "remote-checkout" => {
                let local = data.split_once('/').map_or(data.as_str(), |(_, b)| b).to_string();
                Some(prompt("Checkout Remote Branch".into(), format!("Check out {data} as a local branch."), vec![Field::text("name", "Local branch", local).required()], "Checkout"))
            }
            "remote-delete" => Some(danger(prompt("Delete Remote Branch".into(), format!("Delete {data} on its remote?"), Vec::new(), "Delete"))),

            // A tag's menu.
            "tag-delete" => {
                let mut fields = Vec::new();
                if !repo.remotes.is_empty() {
                    fields.push(Field::checkbox("remote", "Delete it on the remote too", false));
                }
                Some(danger(prompt("Delete Tag".into(), format!("Delete tag {data}?"), fields, "Delete")))
            }
            "tag-push" => Some(prompt("Push Tag".into(), format!("Push tag {data}."), vec![remote_choice("origin")], "Push")),

            // The uncommitted changes' menu.
            "uncommitted-stash" => Some(prompt(
                "Stash Changes".into(),
                "Stash the uncommitted changes.".into(),
                vec![Field::text("message", "Message", ""), Field::checkbox("untracked", "Include untracked files", true)],
                "Stash",
            )),
            "uncommitted-reset" => {
                let options = vec![
                    SelectOption { value: "mixed".into(), label: "Mixed: unstage the changes, keep them".into() },
                    SelectOption { value: "hard".into(), label: "Hard: discard the changes".into() },
                ];
                Some(danger(prompt("Reset Changes".into(), "Reset the uncommitted changes?".into(), vec![Field::choice("mode", "", "mixed", options)], "Reset")))
            }
            "uncommitted-clean" => Some(danger(prompt(
                "Clean Untracked Files".into(),
                "Delete every untracked file? This can't be undone.".into(),
                vec![Field::checkbox("dirs", "Untracked folders too", true)],
                "Clean",
            ))),
            _ => None,
        };
        let Some(question) = question else { return };
        let target = if op.starts_with("commit-") || op.starts_with("uncommitted-") { row } else { data };
        self.ask(root, op, &target, question);
    }

    /// Do what the answered question asked about.
    fn answered(&mut self, pending: Pending, values: serde_json::Map<String, Value>) {
        let Pending { root, op, target } = pending;
        let text = |key: &str| values.get(key).and_then(Value::as_str).unwrap_or_default().trim().to_string();
        let on = |key: &str| values.get(key).and_then(Value::as_bool).unwrap_or(false);
        let Some(repo) = self.repos.get(&root) else { return };
        let top = repo.top.clone();
        let merge_commit = repo.commits.iter().find(|c| c.hash == target).is_some_and(|c| c.parents.len() > 1);
        match op.as_str() {
            "commit-tag" => {
                let (name, message) = (text("name"), text("message"));
                let mut args = vec!["tag"];
                if !message.is_empty() {
                    args.extend(["-a", "-m", message.as_str()]);
                }
                args.extend([name.as_str(), target.as_str()]);
                self.git(&root, "Add Tag", &args);
                if on("push")
                    && let Some(remote) = self.repos.get(&root).and_then(|r| r.remotes.first().cloned())
                {
                    self.background(&root, "Push Tag", top, vec!["push".into(), remote, name]);
                }
            }
            "commit-branch" => {
                let name = text("name");
                if on("checkout") {
                    self.git(&root, "Create Branch", &["checkout", "-b", &name, &target]);
                } else {
                    self.git(&root, "Create Branch", &["branch", &name, &target]);
                }
            }
            "commit-checkout" => self.git(&root, "Checkout", &["checkout", "--detach", &target]),
            "commit-cherry-pick" => {
                let mut args = vec!["cherry-pick"];
                if merge_commit {
                    args.extend(["-m", "1"]);
                }
                if on("no_commit") {
                    args.push("--no-commit");
                }
                args.push(&target);
                self.git(&root, "Cherry Pick", &args);
            }
            "commit-revert" => {
                let mut args = vec!["revert", "--no-edit"];
                if merge_commit {
                    args.extend(["-m", "1"]);
                }
                args.push(&target);
                self.git(&root, "Revert", &args);
            }
            "commit-merge" | "branch-merge" | "remote-merge" => {
                let mut args = vec!["merge", "--no-edit"];
                if on("squash") {
                    args.push("--squash");
                } else if on("no_ff") {
                    args.push("--no-ff");
                }
                args.push(&target);
                self.git(&root, "Merge", &args);
            }
            "commit-rebase" | "branch-rebase" => self.git(&root, "Rebase", &["rebase", &target]),
            "commit-reset" => {
                let mode = format!("--{}", text("mode"));
                self.git(&root, "Reset", &["reset", &mode, &target]);
            }
            "branch-rename" => self.git(&root, "Rename Branch", &["branch", "-m", &target, &text("name")]),
            "branch-delete" => self.git(&root, "Delete Branch", &["branch", if on("force") { "-D" } else { "-d" }, &target]),
            "branch-push" => {
                let mut args = vec!["push".to_string()];
                if on("upstream") {
                    args.push("--set-upstream".into());
                }
                if on("force") {
                    args.push("--force-with-lease".into());
                }
                args.extend([text("remote"), target]);
                self.background(&root, "Push", top, args);
            }
            "remote-checkout" => {
                let name = text("name");
                let exists = self.repos.get(&root).is_some_and(|r| r.refs.iter().any(|r| r.kind == RefKind::Local && r.name == name));
                if exists {
                    self.git(&root, "Checkout", &["checkout", &name]);
                } else {
                    self.git(&root, "Checkout", &["checkout", "-b", &name, "--track", &target]);
                }
            }
            "remote-delete" => {
                let Some((remote, branch)) = target.split_once('/') else { return };
                self.background(&root, "Delete Remote Branch", top, vec!["push".into(), remote.into(), "--delete".into(), branch.into()]);
            }
            "tag-delete" => {
                self.git(&root, "Delete Tag", &["tag", "-d", &target]);
                if on("remote")
                    && let Some(remote) = self.repos.get(&root).and_then(|r| r.remotes.first().cloned())
                {
                    self.background(&root, "Delete Remote Tag", top, vec!["push".into(), remote, "--delete".into(), format!("refs/tags/{target}")]);
                }
            }
            "tag-push" => self.background(&root, "Push Tag", top, vec!["push".into(), text("remote"), format!("refs/tags/{target}")]),
            "uncommitted-stash" => {
                let message = text("message");
                let mut args = vec!["stash", "push"];
                if on("untracked") {
                    args.push("--include-untracked");
                }
                if !message.is_empty() {
                    args.extend(["-m", message.as_str()]);
                }
                self.git(&root, "Stash", &args);
            }
            "uncommitted-reset" => {
                let mode = format!("--{}", text("mode"));
                self.git(&root, "Reset", &["reset", &mode, "HEAD"]);
            }
            "uncommitted-clean" => self.git(&root, "Clean", &["clean", "-f", if on("dirs") { "-d" } else { "-q" }]),
            _ => {}
        }
    }
}

// -- Reading and showing a repository ------------------------------------------

/// Read the repository into `repo`: refs, HEAD, commits, uncommitted changes.
fn load(repo: &mut Repo, config: &Config) -> Result<(), String> {
    repo.fingerprint = git::fingerprint(&repo.top);
    repo.checked = Instant::now();
    repo.refs = git::refs(&repo.top)?;
    (repo.head, repo.head_branch) = git::head(&repo.top);
    repo.remotes = git::remotes(&repo.top);
    // A filter on a branch that's gone shows them all again.
    if repo.branch != ALL && !repo.refs.iter().any(|r| r.full == repo.branch) {
        repo.branch = ALL.into();
    }
    let revs: Vec<String> = if repo.branch == ALL {
        let mut revs = vec!["--branches".to_string()];
        if config.show_tags {
            revs.push("--tags".into());
        }
        if repo.show_remotes {
            revs.push("--remotes".into());
        }
        if repo.head.is_some() {
            revs.push("HEAD".into());
        }
        revs
    } else {
        vec![repo.branch.clone()]
    };
    let mut commits = git::log(&repo.top, &revs, repo.limit + 1)?;
    repo.more = commits.len() > repo.limit;
    commits.truncate(repo.limit);
    repo.commits = commits;
    repo.changes = if config.show_uncommitted && repo.head.is_some() { git::status(&repo.top)? } else { Vec::new() };
    if repo.selected.as_ref().is_some_and(|s| !(s == UNCOMMITTED && !repo.changes.is_empty()) && !repo.commits.iter().any(|c| &c.hash == s)) {
        repo.selected = None;
    }
    find(repo);
    Ok(())
}

/// The commits the search matches: in the subject, hash, author or a ref's name.
fn find(repo: &mut Repo) {
    let search = repo.search.trim().to_lowercase();
    repo.matches = if search.is_empty() {
        Vec::new()
    } else {
        let refs = &repo.refs;
        repo.commits
            .iter()
            .filter(|c| {
                c.subject.to_lowercase().contains(&search)
                    || c.hash.starts_with(&search)
                    || c.author.to_lowercase().contains(&search)
                    || c.email.to_lowercase().contains(&search)
                    || refs.iter().any(|r| r.target == c.hash && r.name.to_lowercase().contains(&search))
            })
            .map(|c| c.hash.clone())
            .collect()
    };
    if repo.match_ix >= repo.matches.len() {
        repo.match_ix = 0;
    }
}

fn toolbar(repo: &Repo, config: &Config) -> Vec<ToolbarItem> {
    let mut options = vec![SelectOption { value: ALL.into(), label: "All branches".into() }];
    options.extend(repo.refs.iter().filter(|r| r.kind == RefKind::Local).map(|r| SelectOption { value: r.full.clone(), label: r.name.clone() }));
    if repo.show_remotes {
        options.extend(repo.refs.iter().filter(|r| r.kind == RefKind::Remote).map(|r| SelectOption { value: r.full.clone(), label: r.name.clone() }));
    }
    let mut items = vec![
        ToolbarItem::select("branch", repo.branch.clone(), options).tooltip("Show one branch, or all"),
        ToolbarItem::button("remotes", "Remotes", "cloud").tooltip("Show remote branches").active(repo.show_remotes),
        ToolbarItem::spacer(),
        ToolbarItem::search("find", "Find commits"),
    ];
    if !repo.search.trim().is_empty() {
        let found = if repo.matches.is_empty() { "No results".to_string() } else { format!("{} of {}", repo.match_ix + 1, repo.matches.len()) };
        items.push(ToolbarItem::label(found));
        items.push(ToolbarItem::button("find-prev", "", "chevron-up").tooltip("Previous match"));
        items.push(ToolbarItem::button("find-next", "", "chevron-down").tooltip("Next match (Enter)"));
    }
    if !repo.remotes.is_empty() {
        items.push(ToolbarItem::button("fetch", "", "cloud-download").tooltip("Fetch from all remotes"));
    }
    items.push(ToolbarItem::button("refresh", "", "refresh-cw").tooltip("Refresh"));
    let _ = config;
    items
}

/// Everything the view shows but the details.
fn full_content(repo: &mut Repo, config: &Config) -> Content {
    let uncommitted = !repo.changes.is_empty() && repo.head.is_some();
    // The graph's nodes: the uncommitted changes on top of HEAD, then the commits.
    let head_parent: Vec<String> = repo.head.iter().cloned().collect();
    let mut nodes = Vec::with_capacity(repo.commits.len() + 1);
    if uncommitted {
        nodes.push(graph::Node { hash: UNCOMMITTED, parents: &head_parent, hollow: true, uncommitted: true });
    }
    for commit in &repo.commits {
        let is_head = repo.head.as_deref() == Some(commit.hash.as_str());
        nodes.push(graph::Node { hash: &commit.hash, parents: &commit.parents, hollow: is_head, uncommitted: false });
    }
    let layout = graph::layout(&nodes);
    let mut graphs = layout.graphs.into_iter();
    repo.lanes = layout.lanes;

    let related = if config.dim_unrelated { Some(ancestors(&repo.commits, repo.head.as_deref())) } else { None };
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let mut rows = Vec::with_capacity(nodes.len());
    let mut lanes = repo.lanes.iter();
    if uncommitted {
        let graph = graphs.next().unwrap_or_default();
        lanes.next();
        let text = format!("Uncommitted Changes ({})", repo.changes.len());
        rows.push(Row {
            id: UNCOMMITTED.into(),
            cells: vec![graph.into(), Span::text(text).italic().into(), Span::text("").into(), Span::text("*").dim().into(), Span::text("*").dim().mono().into()],
            menu: "uncommitted".into(),
            ..Default::default()
        });
    }
    for commit in &repo.commits {
        let graph = graphs.next().unwrap_or_default();
        let lane = lanes.next().copied().unwrap_or(0);
        let mut description = badges(repo, &commit.hash, graph::color(lane));
        description.push(Span::text(commit.subject.clone()).tooltip(commit.subject.clone()));
        let date = if config.date_format == "absolute" { commit.date.clone() } else { relative(commit.time, now) };
        let is_head = repo.head.as_deref() == Some(commit.hash.as_str());
        rows.push(Row {
            id: commit.hash.clone(),
            cells: vec![
                graph.into(),
                description.into(),
                Span::text(date).dim().tooltip(commit.date.clone()).into(),
                Span::text(commit.author.clone()).tooltip(format!("{} <{}>", commit.author, commit.email)).into(),
                Span::text(short(&commit.hash)).mono().dim().into(),
            ],
            menu: "commit".into(),
            bold: is_head,
            dim: related.as_ref().is_some_and(|r| !r.contains(commit.hash.as_str())),
        });
    }
    let message = if rows.is_empty() { "No commits yet." } else { "" };
    Content {
        title: Some(format!("Git Graph: {}", repo.top.file_name().map_or_else(|| repo.top.display().to_string(), |n| n.to_string_lossy().into_owned()))),
        toolbar: Some(toolbar(repo, config)),
        columns: Some(vec![
            Column { title: "Graph".into(), fixed: true, ..Default::default() },
            Column { title: "Description".into(), ..Default::default() },
            // The widths are for dens without `fit`.
            Column { title: "Date".into(), width: Some(150.), fit: true, fixed: true },
            Column { title: "Author".into(), width: Some(160.), fit: true, fixed: true },
            Column { title: "Commit".into(), width: Some(80.), fit: true, fixed: true },
        ]),
        rows: Some(rows),
        menus: Some(menus(repo)),
        selected: Some(repo.selected.clone().unwrap_or_default()),
        highlighted: Some(repo.matches.clone()),
        detail: Some(Vec::new()),
        message: Some(message.into()),
        more: Some(repo.more),
    }
}

/// The commits HEAD reaches, of those loaded.
fn ancestors<'a>(commits: &'a [Commit], head: Option<&'a str>) -> HashSet<&'a str> {
    let by_hash: HashMap<&str, &Commit> = commits.iter().map(|c| (c.hash.as_str(), c)).collect();
    let mut seen = HashSet::new();
    let mut stack: Vec<&str> = head.into_iter().collect();
    while let Some(hash) = stack.pop() {
        if !seen.insert(hash) {
            continue;
        }
        if let Some(commit) = by_hash.get(hash) {
            stack.extend(commit.parents.iter().map(String::as_str));
        }
    }
    seen
}

/// The badges of the refs on a commit: HEAD's branch first, then local
/// branches, remote branches and tags.
fn badges(repo: &Repo, hash: &str, color: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let detached = repo.head_branch.is_none() && repo.head.as_deref() == Some(hash);
    if detached {
        spans.push(Span::text("HEAD").background(color).bold().icon("git-commit-horizontal").tooltip("HEAD (detached)").menu("head", ""));
    }
    let mut refs: Vec<&Ref> = repo.refs.iter().filter(|r| r.target == hash).collect();
    let current = repo.head_branch.as_deref();
    refs.sort_by_key(|r| (!(r.kind == RefKind::Local && Some(r.name.as_str()) == current), r.kind as u8));
    for r in refs {
        let span = match r.kind {
            RefKind::Local if Some(r.name.as_str()) == current => {
                Span::text(r.name.clone()).background(color).bold().icon("git-branch").tooltip(format!("{} (checked out)", r.name)).menu("branch-head", r.name.clone())
            }
            RefKind::Local => Span::text(r.name.clone()).background(color).icon("git-branch").tooltip(format!("Branch {}", r.name)).menu("branch", r.name.clone()),
            RefKind::Remote if !repo.show_remotes => continue,
            RefKind::Remote => Span::text(r.name.clone()).background("#4b5563").italic().icon("cloud").tooltip(format!("Remote branch {}", r.name)).menu("remote", r.name.clone()),
            RefKind::Tag => Span::text(r.name.clone()).background("#7c5e10").icon("tag").tooltip(format!("Tag {}", r.name)).menu("tag", r.name.clone()),
        };
        spans.push(span);
    }
    spans
}

fn menus(repo: &Repo) -> std::collections::BTreeMap<String, Vec<MenuItem>> {
    let remote = !repo.remotes.is_empty();
    let item = MenuItem::new;
    let sep = MenuItem::separator;
    let mut menus = std::collections::BTreeMap::new();
    menus.insert(
        "commit".into(),
        vec![
            item("commit-tag", "Add Tag…"),
            item("commit-branch", "Create Branch…"),
            sep(),
            item("commit-checkout", "Checkout…"),
            item("commit-cherry-pick", "Cherry Pick…"),
            item("commit-revert", "Revert…"),
            sep(),
            item("commit-merge", "Merge into Current Branch…"),
            item("commit-rebase", "Rebase Current Branch on this Commit…"),
            item("commit-reset", "Reset Current Branch to this Commit…"),
            sep(),
            item("commit-copy-hash", "Copy Commit Hash"),
            item("commit-copy-subject", "Copy Commit Subject"),
        ],
    );
    menus.insert(
        "branch".into(),
        vec![
            item("branch-checkout", "Checkout Branch"),
            item("branch-rename", "Rename Branch…"),
            item("branch-delete", "Delete Branch…"),
            sep(),
            item("branch-merge", "Merge into Current Branch…"),
            item("branch-rebase", "Rebase Current Branch on Branch…"),
            sep(),
            item("branch-push", "Push Branch…").disabled(!remote),
            sep(),
            item("branch-copy", "Copy Branch Name"),
        ],
    );
    menus.insert(
        "branch-head".into(),
        vec![
            item("branch-rename", "Rename Branch…"),
            sep(),
            item("branch-pull", "Pull").disabled(!remote),
            item("branch-push", "Push Branch…").disabled(!remote),
            sep(),
            item("branch-copy", "Copy Branch Name"),
        ],
    );
    menus.insert(
        "remote".into(),
        vec![
            item("remote-checkout", "Checkout Branch…"),
            item("remote-merge", "Merge into Current Branch…"),
            sep(),
            item("remote-delete", "Delete Remote Branch…"),
            sep(),
            item("remote-copy", "Copy Branch Name"),
        ],
    );
    menus.insert(
        "tag".into(),
        vec![item("tag-push", "Push Tag…").disabled(!remote), item("tag-delete", "Delete Tag…"), sep(), item("tag-copy", "Copy Tag Name")],
    );
    menus.insert("head".into(), vec![item("commit-branch", "Create Branch…"), item("commit-copy-hash", "Copy Commit Hash")]);
    menus.insert(
        "uncommitted".into(),
        vec![item("uncommitted-stash", "Stash Changes…"), sep(), item("uncommitted-reset", "Reset Changes…"), item("uncommitted-clean", "Clean Untracked Files…")],
    );
    menus.insert("file".into(), vec![item("file-diff", "View Diff"), item("file-open", "Open File"), sep(), item("file-copy-path", "Copy Path")]);
    menus
}

/// The details of the selected row: a commit's, or the uncommitted changes'.
fn detail(repo: &Repo, selected: &str) -> Vec<Line> {
    if selected == UNCOMMITTED {
        return uncommitted_detail(repo);
    }
    let Some(commit) = repo.commits.iter().find(|c| c.hash == selected) else { return Vec::new() };
    let parent = commit.parents.first().map(String::as_str);
    let details = match git::details(&repo.top, &commit.hash, parent) {
        Ok(details) => details,
        Err(err) => return vec![Line::new(vec![Span::text(format!("Couldn't read the commit: {}", first_lines(&err))).color("#f14c4c")])],
    };
    let field = |name: &str, value: Vec<Span>| {
        let mut spans = vec![Span::text(format!("{name}:")).dim()];
        spans.extend(value);
        Line::new(spans)
    };
    let mut lines = vec![field("Commit", vec![Span::text(commit.hash.clone()).mono().action("copy-hash", commit.hash.clone()).tooltip("Copy")])];
    if !commit.parents.is_empty() {
        lines.push(field("Parents", commit.parents.iter().map(|p| Span::text(short(p)).mono().color("#3794ff").action("goto", p.clone()).tooltip("Go to this commit")).collect()));
    }
    lines.push(field("Author", vec![Span::text(format!("{} <{}>", commit.author, commit.email))]));
    lines.push(field("Date", vec![Span::text(commit.date.clone())]));
    if details.committer != commit.author || details.committer_email != commit.email {
        lines.push(field("Committer", vec![Span::text(format!("{} <{}>, {}", details.committer, details.committer_email, details.commit_date))]));
    }
    let lane = repo.commits.iter().position(|c| c.hash == commit.hash).map(|ix| ix + usize::from(!repo.changes.is_empty() && repo.head.is_some()));
    let badges = badges(repo, &commit.hash, graph::color(lane.and_then(|ix| repo.lanes.get(ix)).copied().unwrap_or(0)));
    if !badges.is_empty() {
        lines.push(Line::new(badges));
    }
    lines.push(Line { spans: vec![Span::text(details.body.clone())], heading: true, ..Default::default() });
    let (added, deleted) = details.files.iter().fold((0, 0), |(a, d), f| (a + f.added.unwrap_or(0), d + f.deleted.unwrap_or(0)));
    let count = details.files.len();
    lines.push(Line::heading(format!("{count} file{} changed, +{added} −{deleted}", if count == 1 { "" } else { "s" })));
    for file in &details.files {
        let data = FileRef { path: file.path.clone(), old: file.old_path.clone(), hash: Some(commit.hash.clone()), parent: parent.map(str::to_string), staged: false };
        lines.push(file_line(file.status, &file.path, file.old_path.as_deref(), file.added.zip(file.deleted), data));
    }
    lines
}

fn uncommitted_detail(repo: &Repo) -> Vec<Line> {
    let mut lines = vec![Line::new(vec![Span::text("Uncommitted changes, on top of").dim(), Span::text(repo.head_branch.clone().unwrap_or_else(|| "HEAD".into())).bold()])];
    let staged: Vec<&Change> = repo.changes.iter().filter(|c| c.staged()).collect();
    let unstaged: Vec<&Change> = repo.changes.iter().filter(|c| c.unstaged()).collect();
    for (title, changes, is_staged) in [("Staged Changes", staged, true), ("Changes", unstaged, false)] {
        if changes.is_empty() {
            continue;
        }
        lines.push(Line::heading(format!("{title} ({})", changes.len())));
        for change in changes {
            let status = if is_staged { change.index } else { change.worktree };
            let status = if status == '?' { 'U' } else { status };
            let data = FileRef { path: change.path.clone(), old: change.old_path.clone(), hash: None, parent: None, staged: is_staged };
            lines.push(file_line(status, &change.path, change.old_path.as_deref(), None, data));
        }
    }
    lines
}

/// A changed file: its letter in git's colours, its path, its counts.
fn file_line(status: char, path: &str, old: Option<&str>, counts: Option<(u64, u64)>, data: FileRef) -> Line {
    let color = match status {
        'A' | 'U' => "#73c991",
        'D' => "#f14c4c",
        'R' | 'C' => "#3794ff",
        _ => "#e2c08d",
    };
    let mut spans = vec![Span::text(status.to_string()).color(color).mono().bold(), Span::text(path.to_string())];
    if let Some(old) = old {
        spans.push(Span::text(format!("← {old}")).dim());
    }
    if let Some((added, deleted)) = counts {
        spans.push(Span::text(format!("+{added}")).color("#73c991"));
        spans.push(Span::text(format!("−{deleted}")).color("#f14c4c"));
    }
    let data = serde_json::to_string(&data).unwrap_or_default();
    Line::new(spans).action("diff", data).menu("file")
}

fn short(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// The first few lines of git's complaint.
fn first_lines(text: &str) -> String {
    text.lines().filter(|l| !l.trim().is_empty()).take(3).collect::<Vec<_>>().join(" ")
}

/// `3d ago`: short, so the column stays narrow (the full date is its tooltip).
fn relative(time: i64, now: i64) -> String {
    let seconds = (now - time).max(0);
    let (n, unit) = match seconds {
        s if s < 60 => return "now".into(),
        s if s < 3600 => (s / 60, "m"),
        s if s < 86_400 => (s / 3600, "h"),
        s if s < 604_800 => (s / 86_400, "d"),
        s if s < 2_629_800 => (s / 604_800, "w"),
        s if s < 31_557_600 => (s / 2_629_800, "mo"),
        s => (s / 31_557_600, "y"),
    };
    format!("{n}{unit} ago")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_match_the_manifest() {
        let manifest = den_extension::Manifest::parse(include_str!("../extension.json")).unwrap();
        let from_manifest = config(manifest.settings_values(&Default::default()));
        let default = Config::default();
        assert_eq!(from_manifest.initial_commits, default.initial_commits);
        assert_eq!(from_manifest.show_remote_branches, default.show_remote_branches);
        assert_eq!(from_manifest.show_tags, default.show_tags);
        assert_eq!(from_manifest.show_uncommitted, default.show_uncommitted);
        assert_eq!(from_manifest.date_format, default.date_format);
        assert_eq!(from_manifest.dim_unrelated, default.dim_unrelated);
        assert!(manifest.views.iter().any(|v| v.id == VIEW));
    }

    #[test]
    fn dates_read_relatively() {
        assert_eq!(relative(100, 130), "now");
        assert_eq!(relative(0, 23 * 60), "23m ago");
        assert_eq!(relative(0, 3 * 3600), "3h ago");
        assert_eq!(relative(0, 86_400), "1d ago");
        assert_eq!(relative(0, 3 * 2_629_800), "3mo ago");
        assert_eq!(relative(0, 2 * 31_557_600), "2y ago");
    }

    #[test]
    fn ancestors_follow_every_parent() {
        let commit = |hash: &str, parents: &[&str]| Commit {
            hash: hash.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            author: String::new(),
            email: String::new(),
            time: 0,
            date: String::new(),
            subject: String::new(),
        };
        let commits = [commit("x", &["a"]), commit("m", &["a", "b"]), commit("b", &["a"]), commit("a", &[])];
        let reached = ancestors(&commits, Some("m"));
        assert!(reached.contains("b") && reached.contains("a") && !reached.contains("x"));
    }
}
