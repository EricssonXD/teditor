# VS Code Feature Gaps for teditor

Use this as a selection backlog: remove anything you don't want to implement. These are options, not commitments.

## File editing

- [x] Undo and redo
- [x] Text selection and clipboard copy/paste
- [x] Keep multiple files open in editor tabs

## File browsing

- [x] Create files and folders
- [x] Rename and delete files and folders
- [x] File and folder context menus
- [x] Drag and drop files in the tree
- [x] Show Git-status decorations in the tree
- [ ] Mark unsaved editor buffers in the Explorer tree

## Git / Source Control

- [x] Discard or revert changes
- [x] Stage or unstage individual diff hunks
- [x] Create, switch, and delete branches
- [x] Push, pull, and fetch
- [ ] Resolve merge conflicts in the editor
- [x] Add actions to history references, rather than only showing their details
- [x] Add colors to the Git tree and status

## Workspace and visual presentation

- [x] Keep the Explorer visible beside the editor instead of replacing the current view when a file opens
- [x] Add visible labels to the activity navigation
- [x] Add a status bar for cursor position, language, encoding, and line endings
- [x] Make Search and Git actions more visible and discoverable
- [x] Add a richer diff-review presentation with inline actions

## Current capabilities (reference)

- Explorer: nested file tree, icons, hidden-file toggle, refresh, collapse-all, file operations, context menus, drag-and-drop, and Git decorations.
- Search: case, whole-word, regex, include/exclude filters, background search, grouped results, highlighted matches, and open-at-line.
- Git: status, staged/unstaged lists, file and hunk stage/unstage, confirmed discard, commits, fetch/push/fast-forward pull, branches, stashes, and checkout/apply actions from history references and drawers.
- Editing: multi-tab inline editing, undo/redo, selection and clipboard support, line numbers, save/external-change protection, and cursor/language/encoding status.

> Merge-conflict resolution remains outside the current scope; keep or remove that gap based on product direction.
