# teditor

A standalone terminal Explorer sidebar based on the filesystem tree and file icons from [Herdr Sidebar](https://github.com/alexarthurs/herdr-sidebar). It has no Herdr integration or editor buffer; `Search` finds matching file lines and `Git Graph` shows commit history across refs.

```sh
teditor                 # browse the current directory
teditor ~/src/project   # browse a directory
teditor README.md       # focus a file in its parent directory
```

Use `1`/`2`/`3` or the activity bar for Explorer, Search, and Git Graph. Search updates live; press `Tab` or `Enter` to navigate results. The graph loads asynchronously and `r` refreshes it. In Explorer, use `↑/↓` or `j/k` to move, `Enter` or `→` to expand/collapse folders, `.` to toggle hidden files, `r` to refresh, `c` to collapse all, `i` to switch icon themes, and `q` to quit. Search results are display-only; selecting a file does not open it yet.

Build and install the command with Rust 1.89+:

```sh
cargo install --path .
```

The copied `tree.rs` and `icons.rs` are from Herdr Sidebar and remain MIT-licensed; see [LICENSE](LICENSE).
