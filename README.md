# Git Graph

The repository's commits as a graph in a tab of den, after [mhutchie's Git Graph](https://github.com/mhutchie/vscode-git-graph) for VS Code. Open it with the **Git Graph** button in the title bar, *Git Graph: Show Git Graph* in den's menu, or <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Shift</kbd>+<kbd>G</kbd>.

- **The graph:** every branch in a coloured lane, merges and forks as curves, HEAD as a ring, and the uncommitted changes as a dashed row on top. Branches, remote branches and tags are badges on their commits.
- **Details:** click a commit for its hash, parents (click one to go there), author, date, message and files with their line counts. Click a file for its diff; right-click it to open it or copy its path. The uncommitted changes list their staged and unstaged files.
- **Actions,** on right-click:
  - a commit: add a tag, create a branch, check out, cherry pick, revert, merge, rebase onto it, reset to it (soft, mixed, hard), copy its hash or subject;
  - a branch: check out, rename, delete, merge, rebase onto it, push, pull (the checked-out one), copy its name;
  - a remote branch: check out as a local branch, merge, delete on the remote;
  - a tag: push, delete (on the remote too), copy its name;
  - the uncommitted changes: stash, reset, clean untracked files.
- **Toolbar:** show one branch or all, remote branches on or off, find commits by message, hash, author or ref (Enter or the arrows step through the matches), fetch from all remotes, refresh.
- It loads 300 commits, with **Load more** at the end, and redraws by itself when the repository changes: a commit, a checkout, a fetch from the terminal.

Push, pull, fetch and deleting on a remote run in the background; a login git asks for shows its own window.

## Settings

| Setting | |
| --- | --- |
| **Commits to load** | How many at first, and how many more each Load more adds (300) |
| **Show remote branches** | Whether a new graph shows them (the Remotes button switches it per graph) |
| **Show tags** | Include commits only tags reach |
| **Show uncommitted changes** | The row on top for them |
| **Date format** | `relative` (3 days ago) or `absolute` |
| **Dim commits HEAD doesn't reach** | Fade the commits that aren't ancestors of HEAD |
| **Title bar button** | The Git Graph button (after a restart) |

## How it works

den draws the tab from data (`den_extension::view`); this extension runs the git CLI, lays out the lanes (`src/graph.rs`) and sends rows, menus and details. Clicks and menu picks come back as `view_action`, answers to its dialogs as `prompt_answered`. A worker thread owns each repository's state and polls for changes while a graph is open.

```sh
cargo test
cargo build --release
```

Copy `extension.json`, `icon.svg` and `target/release/git_graph.dll` into `%APPDATA%\den\extensions\git-graph\` and restart den. It needs a den with extension views.
