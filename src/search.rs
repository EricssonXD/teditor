//! Standalone content search and result highlighting, matching Herdr Sidebar.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ratatui::style::Style;
use ratatui::text::Span;
use regex::{Regex, RegexBuilder};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchFocus {
    #[default]
    Query,
    Replace,
    Include,
    Exclude,
    Results,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentHit {
    pub path: PathBuf,
    pub label: String,
    /// One-based source line number.
    pub line: usize,
    pub context: String,
    /// UTF-8 byte ranges into `context`.
    pub matches: Vec<(usize, usize)>,
}

/// The result cap used by callers; `collect_content_hits` honors its `limit` argument.
pub const CONTENT_SEARCH_MATCH_LIMIT: usize = 1_000;
pub const CONTENT_SEARCH_FILE_LIMIT: usize = 20_000;
pub const CONTENT_SEARCH_MAX_BYTES: u64 = 1024 * 1024;

struct SearchFile {
    path: PathBuf,
    label: String,
    label_lower: String,
}

fn collect_search_files(root: &Path, show_hidden: bool) -> (Vec<SearchFile>, bool) {
    let mut files = Vec::new();
    let mut truncated = false;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(!show_hidden)
        .follow_links(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    for entry in builder.build().flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.into_path();
        let label = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let label_lower = label.to_lowercase();
        files.push(SearchFile {
            path,
            label,
            label_lower,
        });
        if files.len() >= CONTENT_SEARCH_FILE_LIMIT {
            truncated = true;
            break;
        }
    }
    files.sort_by(|left, right| left.label_lower.cmp(&right.label_lower));
    (files, truncated)
}

/// Search matching lines in text files. The flag reports reaching either cap,
/// including when the last available hit exactly fills `limit`.
pub fn collect_content_hits(
    root: &Path,
    show_hidden: bool,
    query: &str,
    include: &str,
    exclude: &str,
    options: SearchOptions,
    limit: usize,
) -> Result<(Vec<ContentHit>, bool), String> {
    if query.is_empty() || limit == 0 {
        return Ok((Vec::new(), false));
    }
    let pattern = if options.regex {
        let pattern = if options.whole_word {
            format!(r"\b(?:{query})\b")
        } else {
            query.to_string()
        };
        Some(
            RegexBuilder::new(&pattern)
                .case_insensitive(!options.match_case)
                .build()
                .map_err(|error| format!("Invalid regular expression: {error}"))?,
        )
    } else {
        None
    };
    let includes = build_search_globs(include, "include")?;
    let excludes = build_search_globs(exclude, "exclude")?;
    let (files, file_limit_reached) = collect_search_files(root, show_hidden);
    let mut hits = Vec::new();
    for file in files {
        if includes
            .as_ref()
            .is_some_and(|patterns| !patterns.is_match(&file.label))
            || excludes
                .as_ref()
                .is_some_and(|patterns| patterns.is_match(&file.label))
        {
            continue;
        }
        if std::fs::metadata(&file.path)
            .map(|metadata| metadata.len() > CONTENT_SEARCH_MAX_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(&file.path) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            let ranges = content_line_match_ranges(line, query, options, pattern.as_ref());
            let matched = pattern
                .as_ref()
                .is_some_and(|pattern| pattern.is_match(line))
                || !ranges.is_empty();
            if !matched {
                continue;
            }
            let (context, matches) = content_hit_context(line, &ranges);
            hits.push(ContentHit {
                path: file.path.clone(),
                label: file.label.clone(),
                line: index + 1,
                context,
                matches,
            });
            if hits.len() >= limit {
                return Ok((hits, true));
            }
        }
    }
    Ok((hits, file_limit_reached))
}

pub fn build_search_globs(raw: &str, label: &str) -> Result<Option<GlobSet>, String> {
    let patterns = raw
        .split(',')
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .collect::<Vec<_>>();
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            Glob::new(pattern).map_err(|error| format!("Invalid {label} pattern: {error}"))?;
        builder.add(glob);
        if !pattern.contains(['*', '?', '[', '{']) {
            let directory = pattern.trim_end_matches(['/', '\\']);
            let descendant = format!("{directory}/**");
            builder.add(
                Glob::new(&descendant)
                    .map_err(|error| format!("Invalid {label} pattern: {error}"))?,
            );
            if !directory.contains(['/', '\\']) {
                let nested_descendant = format!("**/{directory}/**");
                builder.add(
                    Glob::new(&nested_descendant)
                        .map_err(|error| format!("Invalid {label} pattern: {error}"))?,
                );
            }
        }
    }
    builder
        .build()
        .map(Some)
        .map_err(|error| format!("Invalid {label} pattern: {error}"))
}

pub fn content_line_match_ranges(
    line: &str,
    query: &str,
    options: SearchOptions,
    pattern: Option<&Regex>,
) -> Vec<(usize, usize)> {
    if let Some(pattern) = pattern {
        return pattern
            .find_iter(line)
            .filter(|found| !found.is_empty())
            .map(|found| (found.start(), found.end()))
            .collect();
    }
    literal_match_ranges(line, query, options)
}

pub fn literal_match_ranges(
    line: &str,
    query: &str,
    options: SearchOptions,
) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    if options.match_case {
        return line
            .match_indices(query)
            .filter_map(|(start, matched)| {
                let end = start + matched.len();
                word_boundary_matches(line, start, end, options.whole_word).then_some((start, end))
            })
            .collect();
    }

    let lowered = line.to_lowercase();
    let mut source_by_lowered_character = Vec::new();
    for (source_start, character) in line.char_indices() {
        let source_range = (source_start, source_start + character.len_utf8());
        source_by_lowered_character.extend(std::iter::repeat_n(
            source_range,
            character.to_lowercase().count(),
        ));
    }
    let mut lowered_boundaries = lowered
        .char_indices()
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    lowered_boundaries.push(lowered.len());
    if source_by_lowered_character.len() + 1 != lowered_boundaries.len() {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    let mut ranges = Vec::new();
    for (lowered_start, matched) in lowered.match_indices(&needle) {
        let lowered_end = lowered_start + matched.len();
        if !word_boundary_matches(&lowered, lowered_start, lowered_end, options.whole_word) {
            continue;
        }
        let Ok(first) = lowered_boundaries.binary_search(&lowered_start) else {
            continue;
        };
        let Ok(after_last) = lowered_boundaries.binary_search(&lowered_end) else {
            continue;
        };
        if first >= after_last {
            continue;
        }
        let range = (
            source_by_lowered_character[first].0,
            source_by_lowered_character[after_last - 1].1,
        );
        if ranges.last() != Some(&range) {
            ranges.push(range);
        }
    }
    ranges
}

fn word_boundary_matches(haystack: &str, start: usize, end: usize, whole_word: bool) -> bool {
    if !whole_word {
        return true;
    }
    let before = haystack[..start].chars().next_back();
    let after = haystack[end..].chars().next();
    !before.is_some_and(search_word_char) && !after.is_some_and(search_word_char)
}

pub fn content_hit_context(line: &str, ranges: &[(usize, usize)]) -> (String, Vec<(usize, usize)>) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return (String::new(), Vec::new());
    }
    let start = line.len() - line.trim_start().len();
    let end = start + trimmed.len();
    let ranges = ranges
        .iter()
        .filter_map(|&(match_start, match_end)| {
            if match_start >= end || match_end <= start {
                return None;
            }
            let match_start = match_start.max(start);
            let match_end = match_end.min(end);
            Some((match_start - start, match_end - start))
        })
        .collect();
    (trimmed.replace('\t', " "), ranges)
}

fn search_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

pub fn highlighted_search_context(
    context: &str,
    matches: &[(usize, usize)],
    max_width: usize,
    match_style: Style,
) -> Vec<Span<'static>> {
    let context_width = Span::raw(context).width();
    let (visible_end, truncated) = if context_width <= max_width {
        (context.len(), false)
    } else if max_width < 2 {
        (0, false)
    } else {
        let mut width = 0;
        let mut end = 0;
        for (index, character) in context.char_indices() {
            let character_width = character.width().unwrap_or(0);
            if width + character_width + 1 > max_width {
                break;
            }
            width += character_width;
            end = index + character.len_utf8();
        }
        (end, true)
    };
    let mut spans = Vec::new();
    let mut cursor = 0;
    for &(start, end) in matches {
        if start >= visible_end {
            break;
        }
        let start = start.max(cursor);
        let end = end.min(visible_end);
        if start >= end {
            continue;
        }
        if start > cursor {
            spans.push(Span::raw(context[cursor..start].to_string()));
        }
        spans.push(Span::styled(context[start..end].to_string(), match_style));
        cursor = end;
    }
    if cursor < visible_end {
        spans.push(Span::raw(context[cursor..visible_end].to_string()));
    }
    if truncated {
        spans.push(Span::raw("…"));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    #[test]
    fn search_respects_ignore_hidden_and_globs() {
        let root = std::env::temp_dir().join(format!("teditor-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for directory in ["src", "target", ".git"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        for (path, text) in [
            ("src/main.rs", "first\nNeedle here\nneedle again\n"),
            (".secret", "needle\n"),
            ("target/generated.rs", "needle\n"),
            (".git/config", "needle\n"),
            (".gitignore", "target/\n"),
        ] {
            std::fs::write(root.join(path), text).unwrap();
        }
        let search = |hidden, include, exclude| {
            collect_content_hits(
                &root,
                hidden,
                "NEEDLE",
                include,
                exclude,
                SearchOptions::default(),
                20,
            )
            .unwrap()
        };
        let (hits, truncated) = search(false, "", "");
        assert!(!truncated);
        assert_eq!(
            hits.iter()
                .map(|hit| (hit.label.as_str(), hit.line))
                .collect::<Vec<_>>(),
            [("src/main.rs", 2), ("src/main.rs", 3)]
        );
        assert_eq!(hits[0].path, root.join("src/main.rs"));
        assert_eq!(hits[0].context, "Needle here");
        assert_eq!(hits[0].matches, [(0, 6)]);
        let regex_options = SearchOptions {
            regex: true,
            whole_word: true,
            ..SearchOptions::default()
        };
        let (regex_hits, _) =
            collect_content_hits(&root, false, "need(le|ful)", "", "", regex_options, 20).unwrap();
        assert_eq!(regex_hits, hits);
        let (case_hits, _) = collect_content_hits(
            &root,
            false,
            "Needle",
            "",
            "",
            SearchOptions {
                match_case: true,
                ..regex_options
            },
            20,
        )
        .unwrap();
        assert_eq!(case_hits.len(), 1);
        assert_eq!(case_hits[0].line, 2);
        let (empty_matches, _) =
            collect_content_hits(&root, false, "^", "src/**", "", regex_options, 20).unwrap();
        assert_eq!(empty_matches.len(), 3);
        assert!(empty_matches.iter().all(|hit| hit.matches.is_empty()));
        let (hidden, _) = search(true, "", "");
        assert_eq!(hidden.len(), 3);
        assert!(hidden.iter().any(|hit| hit.label == ".secret"));
        assert_eq!(search(true, "src/**", "").0.len(), 2);
        assert_eq!(search(true, "src, *.secret", "").0.len(), 3);
        let (excluded, _) = search(true, "", "src/**");
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].label, ".secret");
        assert_eq!(search(true, "", "src/").0.len(), 1);

        std::fs::write(root.join("binary"), b"needle\0").unwrap();
        std::fs::write(root.join("invalid-utf8"), b"needle\xff").unwrap();
        let mut oversized = vec![b'x'; CONTENT_SEARCH_MAX_BYTES as usize + 1];
        oversized[..6].copy_from_slice(b"needle");
        std::fs::write(root.join("oversized"), oversized).unwrap();
        assert_eq!(search(false, "", "").0.len(), 2);
        let (limited, truncated) =
            collect_content_hits(&root, false, "needle", "", "", SearchOptions::default(), 2)
                .unwrap();
        assert_eq!(limited.len(), 2);
        assert!(truncated);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn filters_and_empty_search_validation() {
        assert!(build_search_globs(" , ", "include").unwrap().is_none());
        assert!(
            build_search_globs("[", "include")
                .unwrap_err()
                .starts_with("Invalid include pattern:")
        );
        let directories = build_search_globs("node_modules, target/", "exclude")
            .unwrap()
            .unwrap();
        for path in [
            "web/node_modules/pkg/index.js",
            "node_modules/pkg/index.js",
            "target/generated.rs",
        ] {
            assert!(directories.is_match(path));
        }
        assert!(!directories.is_match("src/main.rs"));
        let options = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };
        for (query, limit) in [("", 20), ("[", 0)] {
            assert_eq!(
                collect_content_hits(Path::new("."), false, query, "[", "[", options, limit)
                    .unwrap(),
                (vec![], false)
            );
        }
        assert!(
            collect_content_hits(Path::new("."), false, "[", "", "", options, 20)
                .unwrap_err()
                .starts_with("Invalid regular expression:")
        );
    }

    #[test]
    fn case_whole_word_and_regex_ranges() {
        let options = SearchOptions::default();
        assert_eq!(
            literal_match_ranges("Needle needlebox", "needle", options),
            [(0, 6), (7, 13)]
        );
        assert!(
            literal_match_ranges(
                "Needle",
                "needle",
                SearchOptions {
                    match_case: true,
                    ..options
                }
            )
            .is_empty()
        );
        let whole_word = SearchOptions {
            whole_word: true,
            ..options
        };
        assert_eq!(
            literal_match_ranges("a needle here", "needle", whole_word),
            [(2, 8)]
        );
        assert!(literal_match_ranges("needlebox _needle needleé", "needle", whole_word).is_empty());
        assert_eq!(
            literal_match_ranges("use C++ here", "C++", whole_word),
            [(4, 7)]
        );
        let pattern = RegexBuilder::new("need(le|ful)")
            .case_insensitive(true)
            .build()
            .unwrap();
        assert_eq!(
            content_line_match_ranges(
                "NEEDFUL needle",
                "ignored",
                SearchOptions {
                    regex: true,
                    ..options
                },
                Some(&pattern)
            ),
            [(0, 7), (8, 14)]
        );
        assert!(
            content_line_match_ranges(
                "needle",
                "ignored",
                options,
                Some(&Regex::new("^").unwrap())
            )
            .is_empty()
        );
    }

    #[test]
    fn unicode_ranges_are_source_byte_offsets() {
        let options = SearchOptions::default();
        assert_eq!(literal_match_ranges("İ and s ſ", "i", options), [(0, 2)]);
        assert_eq!(literal_match_ranges("İ and s ſ", "s", options), [(7, 8)]);
        assert_eq!(literal_match_ranges("ΟΣ", "ΟΣ", options), [(0, 4)]);
        assert_eq!(literal_match_ranges("éNEEDLE", "needle", options), [(2, 8)]);
        assert_eq!(literal_match_ranges("İ", "\u{307}", options), [(0, 2)]);
        assert!(literal_match_ranges("anything", "", options).is_empty());
    }

    #[test]
    fn context_trims_rebases_and_replaces_tabs() {
        let options = SearchOptions::default();
        let line = " \tİ\tneedle \n";
        let ranges = literal_match_ranges(line, "needle", options);
        assert_eq!(
            content_hit_context(line, &ranges),
            ("İ needle".into(), vec![(3, 9)])
        );
        assert!(content_hit_context(" needle", &[(0, 1)]).1.is_empty());
        assert_eq!(
            content_hit_context("  needle  ", &[(0, 10)]),
            ("needle".into(), vec![(0, 6)])
        );
        assert_eq!(
            content_hit_context(" \t ", &[(0, 3)]),
            (String::new(), vec![])
        );
    }

    #[test]
    fn highlights_follow_ranges_unicode_and_clipping() {
        let options = SearchOptions::default();
        let style = Style::default().add_modifier(Modifier::BOLD);
        let line = "Needle plus needlebox";
        let ranges = literal_match_ranges(line, "needle", options);
        let spans = highlighted_search_context(line, &ranges, 80, style);
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            line
        );
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
                .count(),
            2
        );
        let whole_word = SearchOptions {
            whole_word: true,
            ..options
        };
        let ranges = literal_match_ranges(line, "needle", whole_word);
        assert_eq!(
            highlighted_search_context(line, &ranges, 80, style)
                .iter()
                .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
                .count(),
            1
        );
        let line = "needlebox needle";
        let ranges = literal_match_ranges(line, "needle", whole_word);
        assert!(
            highlighted_search_context(line, &ranges, 8, style)
                .iter()
                .all(|span| !span.style.add_modifier.contains(Modifier::BOLD))
        );
        let spans = highlighted_search_context("needle", &[(0, 6)], 5, style);
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "need…"
        );
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        let spans = highlighted_search_context("界needle", &[(3, 9)], 5, style);
        assert_eq!(
            spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "界ne…"
        );
        assert!(highlighted_search_context("needle", &[(0, 6)], 1, style).is_empty());
    }
}
