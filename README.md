# teditor

A standalone terminal sidebar based on the filesystem tree, Search, and Source Control views from [Herdr Sidebar](https://github.com/alexarthurs/herdr-sidebar). It has no Herdr integration or pane management; previews open inline. Search includes case, whole-word, regex, and file-glob filters, while Source Control contains the collapsible Git Graph and other history drawers.

```sh
teditor                 # browse the current directory
teditor ~/src/project   # browse a directory
teditor README.md       # focus a file in its parent directory
```

Use `1`/`2`/`3` or the activity bar for Explorer, Search, and Source Control. In Search, `Ctrl+F` focuses the query; `Tab` moves between fields and results. Press `Enter` on a result or Explorer file to edit it inline. In Source Control, expand the `Graph` drawer to see current branch history; changed files and Git references open read-only inline diffs/details. `Ctrl+S` saves, `Esc` closes a clean editor, and `Ctrl+Q` discards edits and exits it. In Explorer, use `↑/↓` or `j/k` to move, `Enter` or `→` to expand/collapse folders, `.` to toggle hidden files, `r` to refresh, `c` to collapse all, `i` to switch icon themes, and `q` to quit.

Build and install the command with Rust 1.89+:

```sh
cargo install --path .
```

The copied `tree.rs` and `icons.rs` are from Herdr Sidebar and remain MIT-licensed; see [LICENSE](LICENSE).
