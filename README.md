# teditor

A standalone terminal sidebar based on the filesystem tree, Search, and Source Control views from [Herdr Sidebar](https://github.com/alexarthurs/herdr-sidebar). It has no Herdr integration or pane management; previews open inline. Search includes case, whole-word, regex, and file-glob filters, while Source Control contains the collapsible Git Graph and other history drawers.

```sh
teditor                 # browse the current directory
teditor ~/src/project   # browse a directory
teditor README.md       # focus a file in its parent directory
```

Use `1`/`2`/`3` or the activity bar for Explorer, Search, and Source Control. In Search, `Ctrl+F` focuses the query; `Tab` moves between fields and results. Press `Enter` on a result or Explorer file to edit it inline. In Source Control, expand `Graph` for current-branch history. Use `a`/`u` to stage or unstage, `d` to discard with confirmation, `f`/`p`/`P` to fetch/push/pull, `b` to create a branch, and `z` to stash changes. On a branch/tag/commit row, `s` checks it out; on a stash row, `a` applies it; `x` deletes the branch or drops the stash. Git diff previews use `[`/`]` to select hunks, `s` to stage and `u` to unstage, and `d` to discard. Save or discard dirty editor tabs before branch switches, pulls, stash creation/application, or other worktree-changing actions; clean tabs reload after Git updates. `Ctrl+S` saves, `Esc` closes a clean editor, and `Ctrl+Q` discards edits and exits it. In Explorer, use `↑/↓` or `j/k` to move, `Enter` or `→` to expand/collapse folders, `.` to toggle hidden files, `r` to refresh, `c` to collapse all, `i` to switch icon themes, and `q` to quit.

Build and install the command with Rust 1.89+:

```sh
cargo install --path .
```

The copied `tree.rs` and `icons.rs` are from Herdr Sidebar and remain MIT-licensed; see [LICENSE](LICENSE).
