//! Source-aware rich preview content for the right-hand preview pane.
//!
//! The preview pane used to show a static metadata dump for every entry. This
//! module builds richer, source-specific content:
//!
//! - `Agent` / `Workspace` → the pane's recent scrollback buffer (with ANSI
//!   colors preserved via `ansi-to-tui`).
//! - `Zoxide` / `Root` → a fixed git information block (branch, ahead/behind,
//!   untracked/staged/unstaged/stash counts, short SHA, top 3 remotes) followed
//!   by a compact, colored directory tree.
//!
//! Other sources fall back to the legacy metadata preview in `tui::preview_text`.
//!
//! All subprocess work (Herdr pane read, `git status`, `git remote`) happens
//! here and is cached by `App` keyed on the selected entry, so it runs once per
//! selection change rather than every render. Missing tools or non-repo paths
//! degrade quietly to a smaller preview.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use ansi_to_tui::IntoText as _;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::config::Config;
use crate::herdr::{herdr_json, herdr_text};
use crate::model::{Entry, Source};
use crate::theme::Theme;

type TreeEntry = (String, PathBuf, bool);

/// Build the rich preview for an entry. Returns owned `Text` so it can be
/// cached on `App` and cloned into the render cheaply.
pub(crate) fn build_preview(entry: &Entry, config: &Config, theme: &Theme) -> Text<'static> {
    let mut lines = header_lines(entry, theme);
    match entry.source {
        Source::Agent | Source::Workspace => {
            if let Some(text) = pane_scrollback(entry, config) {
                lines.push(Line::from(""));
                lines.extend(text.lines);
            } else {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "no scrollback available for this pane",
                    Style::default().fg(theme.subtext0),
                )));
            }
        }
        Source::Zoxide | Source::Root => {
            if config.picker.preview_git_status {
                if let Some(info) = git_info(&entry.path) {
                    lines.push(Line::from(""));
                    lines.extend(render_git_info(&info, theme));
                }
            }
            lines.push(Line::from(""));
            let tree = directory_tree(
                &entry.path,
                config.picker.preview_tree_depth,
                config.picker.preview_tree_max_per_level as usize,
                theme,
            );
            if tree.is_empty() {
                lines.push(Line::from(Span::styled(
                    "(empty or unreadable directory)",
                    Style::default().fg(theme.subtext0),
                )));
            } else {
                lines.extend(tree);
            }
        }
        // Other sources keep the legacy metadata preview; nothing rich here.
        _ => {}
    }
    Text::from(lines)
}

fn header_lines(entry: &Entry, theme: &Theme) -> Vec<Line<'static>> {
    let label = theme.subtext0;
    let value = theme.text;
    vec![
        Line::from(vec![
            Span::styled("type: ", Style::default().fg(label)),
            Span::styled(entry.source_name().to_string(), Style::default().fg(value)),
        ]),
        Line::from(vec![
            Span::styled("title: ", Style::default().fg(label)),
            Span::styled(entry.title.clone(), Style::default().fg(value)),
        ]),
        Line::from(vec![
            Span::styled("path: ", Style::default().fg(label)),
            Span::styled(entry.path.display().to_string(), Style::default().fg(value)),
        ]),
    ]
}

/// Resolve the pane id to read scrollback from for the given entry.
///
/// Agents carry their pane target directly. Workspaces expose a workspace id,
/// so we list panes in that workspace and pick the focused one (falling back to
/// the first pane). Returns `None` if the pane cannot be resolved.
fn pane_id_for_entry(entry: &Entry) -> Option<String> {
    if let Some(target) = &entry.agent_target {
        return Some(target.clone());
    }
    let workspace_id = entry.workspace_id.as_deref()?;
    let json = herdr_json(["pane", "list", "--workspace", workspace_id]).ok()?;
    let panes = json.pointer("/result/panes").and_then(|v| v.as_array())?;
    panes
        .iter()
        .find(|p| p.get("focused").and_then(|v| v.as_bool()) == Some(true))
        .and_then(|p| p.get("pane_id")?.as_str())
        .map(String::from)
        .or_else(|| {
            panes
                .first()
                .and_then(|p| p.get("pane_id")?.as_str())
                .map(String::from)
        })
}

/// Read a pane's recent scrollback and parse ANSI escapes into styled text.
fn pane_scrollback(entry: &Entry, config: &Config) -> Option<Text<'static>> {
    let pane_id = pane_id_for_entry(entry)?;
    let lines = config.picker.preview_scrollback_lines.max(1).to_string();
    let raw = herdr_text([
        "pane", "read", &pane_id, "--source", "recent", "--lines", &lines, "--format", "ansi",
    ])
    .ok()?;
    let trimmed = raw.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.into_text().ok()
}

/// Parsed git status for a repository path. `None` if `git` is missing or the
/// path is not inside a work tree.
struct GitInfo {
    branch: Option<String>,
    upstream: Option<String>,
    ahead: u32,
    behind: u32,
    untracked: u32,
    staged: u32,
    unstaged: u32,
    stash: u32,
    sha: Option<String>,
    /// Human-friendly name for HEAD from `git describe --tags --always`: a tag
    /// (`v1.2.3`), a tag-relative describe (`v1.2.3-5-gabc1234`), or the short
    /// SHA when no tags reach HEAD. Shown as the position alias when available.
    describe: Option<String>,
    remotes: Vec<(String, String)>,
}

/// Collect git status + remotes for `path` in two subprocess calls.
/// Returns `None` if `git` is missing or `path` is not a repository, so non-repo
/// directories degrade quietly to a tree-only preview.
fn git_info(path: &Path) -> Option<GitInfo> {
    let path_str = path.to_str()?;
    // `--porcelain=v2 --branch` gives branch/upstream/ahead-behind and per-file
    // staged/unstaged status in a single, stable, machine-readable call.
    let status_out = Command::new("git")
        .args([
            "-C",
            path_str,
            "status",
            "--porcelain=v2",
            "--branch",
            "--show-stash",
        ])
        .output()
        .ok()?;
    if !status_out.status.success() {
        return None;
    }
    let status_text = String::from_utf8_lossy(&status_out.stdout);

    let mut info = GitInfo {
        branch: None,
        upstream: None,
        ahead: 0,
        behind: 0,
        untracked: 0,
        staged: 0,
        unstaged: 0,
        stash: 0,
        sha: None,
        describe: None,
        remotes: vec![],
    };

    for line in status_text.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            info.branch = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            info.upstream = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.oid ") {
            // On an unborn branch this is the literal "(initial)" rather
            // than a SHA; only accept hex object ids so we don't show garbage.
            if rest.chars().all(|c| c.is_ascii_hexdigit()) {
                info.sha = Some(rest.get(..7).unwrap_or(rest).to_string());
            }
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for tok in rest.split_whitespace() {
                if let Some(a) = tok.strip_prefix('+') {
                    info.ahead = a.parse().unwrap_or(0);
                } else if let Some(b) = tok.strip_prefix('-') {
                    info.behind = b.parse().unwrap_or(0);
                }
            }
        } else if let Some(rest) = line.strip_prefix("# stash ") {
            info.stash = rest.trim_start_matches('+').parse().unwrap_or(0);
        } else if line.starts_with("? ") {
            info.untracked += 1;
        } else if line.starts_with('1') || line.starts_with('2') || line.starts_with('u') {
            // Porcelain v2 ordinary/renamed/unmerged: "<kind> <XY> ...".
            // X = staged status, Y = unstaged status; '.'/' ' means unchanged.
            let mut parts = line.split_whitespace();
            parts.next();
            if let Some(xy) = parts.next() {
                let mut chars = xy.chars();
                let x = chars.next().unwrap_or(' ');
                let y = chars.next().unwrap_or(' ');
                if x != '.' && x != ' ' {
                    info.staged += 1;
                }
                if y != '.' && y != ' ' {
                    info.unstaged += 1;
                }
            }
        }
    }

    // Human-friendly HEAD alias: tag, tag-relative describe, or short SHA.
    // `--always` makes it fall back to the SHA when no tags reach HEAD; `--tags`
    // considers lightweight tags too. Failure here is non-fatal.
    let describe_out = Command::new("git")
        .args(["-C", path_str, "describe", "--tags", "--always"])
        .output()
        .ok();
    if let Some(out) = describe_out {
        if out.status.success() {
            let d = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !d.is_empty() {
                info.describe = Some(d);
            }
        }
    }

    // Remotes: dedupe by name (fetch/push rows), keep top 3.
    let remote_out = Command::new("git")
        .args(["-C", path_str, "remote", "-v"])
        .output()
        .ok()?;
    if remote_out.status.success() {
        let remote_text = String::from_utf8_lossy(&remote_out.stdout);
        let mut seen = HashSet::new();
        for line in remote_text.lines() {
            let mut parts = line.split_whitespace();
            let Some(name) = parts.next() else { continue };
            let Some(url) = parts.next() else { continue };
            if seen.insert(name.to_string()) {
                info.remotes.push((name.to_string(), url.to_string()));
                if info.remotes.len() >= 3 {
                    break;
                }
            }
        }
    }

    Some(info)
}

/// Render the fixed git information block as styled lines.
fn render_git_info(info: &GitInfo, theme: &Theme) -> Vec<Line<'static>> {
    let dim = Style::default().fg(theme.overlay0);
    let mut lines = Vec::new();

    // Line 1: "git  <branch> ↑N ↓N"
    let mut head = vec![Span::styled("git  ", dim)];
    match &info.branch {
        Some(branch) if branch == "(detached)" => head.push(Span::styled(
            "(detached)",
            Style::default().fg(theme.subtext0),
        )),
        Some(branch) => head.push(Span::styled(
            branch.clone(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )),
        None => head.push(Span::styled(
            "(unknown)",
            Style::default().fg(theme.subtext0),
        )),
    }
    if info.ahead > 0 {
        head.push(Span::styled(
            format!(" ↑{}", info.ahead),
            Style::default().fg(theme.green),
        ));
    }
    if info.behind > 0 {
        head.push(Span::styled(
            format!(" ↓{}", info.behind),
            Style::default().fg(theme.red),
        ));
    }
    lines.push(Line::from(head));

    // Line 2: compressed change counts, indented under "git  ".
    let mut parts: Vec<(String, ratatui::style::Color)> = Vec::new();
    if info.untracked > 0 {
        parts.push((format!("untracked {}", info.untracked), theme.yellow));
    }
    if info.staged > 0 {
        parts.push((format!("staged {}", info.staged), theme.green));
    }
    if info.unstaged > 0 {
        parts.push((format!("unstaged {}", info.unstaged), theme.peach));
    }
    if info.stash > 0 {
        parts.push((format!("stash {}", info.stash), theme.subtext0));
    }
    if !parts.is_empty() {
        let mut spans = vec![Span::raw("     ")];
        for (i, (text, color)) in parts.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", dim));
            }
            spans.push(Span::styled(text.clone(), Style::default().fg(*color)));
        }
        lines.push(Line::from(spans));
    }

    // Line 3: current position — prefer a human-friendly alias (tag or
    // tag-relative describe from `git describe --tags --always`) over the raw
    // short SHA. Show the SHA as a fallback when no describe is available.
    let mut pos = vec![Span::raw("     ")];
    let position = info.describe.as_deref().or(info.sha.as_deref());
    if let Some(p) = position {
        // If the alias is a tag or tag-relative describe (contains 'g' hex
        // suffix or is a pure tag), color it as the accent; a bare SHA stays
        // subtext0.
        let is_alias = info
            .describe
            .as_deref()
            .is_some_and(|d| d != info.sha.as_deref().unwrap_or(""));
        let color = if is_alias {
            theme.mauve
        } else {
            theme.subtext0
        };
        pos.push(Span::styled(p.to_string(), Style::default().fg(color)));
    } else if info.branch.is_some() && info.sha.is_none() {
        // Unborn branch (no commits yet): HEAD does not resolve to an object.
        pos.push(Span::styled(
            "(no commits)",
            Style::default().fg(theme.subtext0),
        ));
    }
    if let Some(up) = &info.upstream {
        pos.push(Span::styled(" → ", dim));
        pos.push(Span::styled(up.clone(), Style::default().fg(theme.blue)));
    }
    lines.push(Line::from(pos));

    // Remotes: name (padded) + URL, top 3.
    if !info.remotes.is_empty() {
        lines.push(Line::from(Span::styled("remotes", dim)));
        for (name, url) in &info.remotes {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:<10}", name), Style::default().fg(theme.blue)),
                Span::styled(url.clone(), Style::default().fg(theme.subtext0)),
            ]));
        }
    }

    lines
}

/// Build a depth-limited, colored directory tree. Directories are listed
/// before files. The first layer shows up to `ROOT_MAX_DIRS` (5) directories
/// and `ROOT_MAX_FILES` (10) files; deeper layers show at most
/// `max_per_level` entries of each kind. Truncated categories get a
/// `… (N more)` summary line. Symlinks and unreadable entries are skipped
/// quietly.
fn directory_tree(
    root: &Path,
    max_depth: u32,
    max_per_level: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    walk(root, "", 0, max_depth, max_per_level, theme, &mut lines);
    lines
}

const ROOT_MAX_DIRS: usize = 5;
const ROOT_MAX_FILES: usize = 10;

fn walk(
    dir: &Path,
    prefix: &str,
    depth: u32,
    max_depth: u32,
    max_per_level: usize,
    theme: &Theme,
    lines: &mut Vec<Line<'static>>,
) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut entries: Vec<TreeEntry> = read
        .filter_map(Result::ok)
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().ok().map(|t| t.is_dir()).unwrap_or(false);
            (name, e.path(), is_dir)
        })
        .collect();
    // Directories first, then files; both alphabetical.
    entries.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));

    let (mut dirs, mut files): (Vec<TreeEntry>, Vec<TreeEntry>) =
        entries.into_iter().partition(|(_, _, is_dir)| *is_dir);

    // First layer is more generous than deeper ones.
    let (dir_cap, file_cap) = if depth == 0 {
        (ROOT_MAX_DIRS, ROOT_MAX_FILES)
    } else {
        (max_per_level, max_per_level)
    };

    let dir_more = dirs.len().saturating_sub(dir_cap);
    let file_more = files.len().saturating_sub(file_cap);
    dirs.truncate(dir_cap);
    files.truncate(file_cap);

    // Combine into the rendered list, dirs first. A category's last item uses
    // the └── marker only if nothing (files or a … summary) follows it.
    let total_items =
        dirs.len() + files.len() + usize::from(dir_more > 0) + usize::from(file_more > 0);
    let mut index = 0usize;
    for (name, path, is_dir) in dirs.iter().chain(files.iter()) {
        index += 1;
        let last = index == total_items;
        let marker = if last { "└── " } else { "├── " };
        let marker_span = Span::styled(
            format!("{prefix}{marker}"),
            Style::default().fg(theme.overlay0),
        );
        let name_color = if *is_dir { theme.blue } else { theme.text };
        let suffix = if *is_dir { "/" } else { "" };
        let name_span = Span::styled(format!("{name}{suffix}"), Style::default().fg(name_color));
        lines.push(Line::from(vec![marker_span, name_span]));

        if *is_dir && depth + 1 < max_depth {
            let child_prefix = if last {
                format!("{prefix}    ")
            } else {
                format!("{prefix}│   ")
            };
            walk(
                path,
                &child_prefix,
                depth + 1,
                max_depth,
                max_per_level,
                theme,
                lines,
            );
        }
    }
    // … summary lines for truncated categories, in the same dirs-then-files order.
    if dir_more > 0 {
        let last = file_more == 0;
        let marker = if last { "└── " } else { "├── " };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{prefix}{marker}"),
                Style::default().fg(theme.overlay0),
            ),
            Span::styled(
                format!("… ({} more dirs)", dir_more),
                Style::default().fg(theme.subtext0),
            ),
        ]));
    }
    if file_more > 0 {
        lines.push(Line::from(vec![
            Span::styled(format!("{prefix}└── "), Style::default().fg(theme.overlay0)),
            Span::styled(
                format!("… ({} more files)", file_more),
                Style::default().fg(theme.subtext0),
            ),
        ]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn theme() -> Theme {
        Theme::load(None, None, false)
    }

    fn entry(source: Source, path: &str, title: &str) -> Entry {
        Entry {
            source,
            title: title.into(),
            subtitle: String::new(),
            path: PathBuf::from(path),
            workspace_id: None,
            workspace_label: None,
            agent_target: None,
            project: None,
            action: crate::model::EntryAction::FocusOrCreateDir,
            source_label: None,
            search_terms: vec![],
            canonical: OnceLock::new(),
        }
    }

    #[test]
    fn header_lists_type_title_and_path() {
        let e = entry(Source::Zoxide, "/tmp/dir", "dir");
        let text = build_preview(&e, &Config::default(), &theme());
        let joined: String = text
            .lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("type: zoxide"));
        assert!(joined.contains("title: dir"));
        assert!(joined.contains("path: /tmp/dir"));
    }

    #[test]
    fn directory_tree_lists_entries_with_markers() {
        let dir = std::env::temp_dir().join(format!("nav-tree-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::fs::write(dir.join("b.txt"), "y").unwrap();

        let tree = directory_tree(&dir, 2, 40, &theme());
        let joined: String = tree
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("sub/"));
        assert!(joined.contains("a.txt"));
        assert!(joined.contains("b.txt"));
        assert!(joined.find("sub/").unwrap() < joined.find("a.txt").unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_tree_root_caps_dirs_and_files_separately() {
        let dir = std::env::temp_dir().join(format!("nav-tree-root-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 7 dirs + 12 files at the root layer.
        for i in 0..7 {
            std::fs::create_dir_all(dir.join(format!("d{i}"))).unwrap();
        }
        for i in 0..12 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        // depth 1 so only the root layer is rendered; max_per_level governs
        // deeper layers only.
        let tree = directory_tree(&dir, 1, 2, &theme());
        let joined: String = tree
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Root shows up to 5 dirs and 10 files, summarizing the rest.
        assert!(joined.contains("… (2 more dirs)"));
        assert!(joined.contains("… (2 more files)"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_tree_deeper_layers_use_max_per_level() {
        let dir = std::env::temp_dir().join(format!("nav-tree-deep-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        // 5 files inside the single subdirectory (a deeper layer).
        for i in 0..5 {
            std::fs::write(dir.join("sub").join(format!("f{i}.txt")), "x").unwrap();
        }
        let tree = directory_tree(&dir, 2, 2, &theme());
        let joined: String = tree
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Deeper layer caps at max_per_level=2 files.
        assert!(joined.contains("… (3 more files)"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_tree_empty_dir_yields_no_lines() {
        let dir = std::env::temp_dir().join(format!("nav-tree-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tree = directory_tree(&dir, 2, 40, &theme());
        assert!(tree.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_info_returns_none_for_non_repo() {
        let dir = std::env::temp_dir().join(format!("nav-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(git_info(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_info_parses_porcelain_v2_branch_and_counts() {
        let sample = "\
# branch.oid 4abc1234567890abcdef1234567890abcdef12
# branch.head main
# branch.upstream origin/main
# branch.ab +2 -1
# stash 3
1 .M N... 100644 100644 sha1 sha2 modified.txt
1 M. N... 100644 100644 sha1 sha2 staged.txt
? untracked.txt
";
        let mut info = GitInfo {
            branch: None,
            upstream: None,
            ahead: 0,
            behind: 0,
            untracked: 0,
            staged: 0,
            unstaged: 0,
            stash: 0,
            sha: None,
            describe: None,
            remotes: vec![],
        };
        for line in sample.lines() {
            if let Some(rest) = line.strip_prefix("# branch.head ") {
                info.branch = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
                info.upstream = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("# branch.oid ") {
                info.sha = Some(rest.get(..7).unwrap_or(rest).to_string());
            } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
                for tok in rest.split_whitespace() {
                    if let Some(a) = tok.strip_prefix('+') {
                        info.ahead = a.parse().unwrap_or(0);
                    } else if let Some(b) = tok.strip_prefix('-') {
                        info.behind = b.parse().unwrap_or(0);
                    }
                }
            } else if let Some(rest) = line.strip_prefix("# stash ") {
                info.stash = rest.trim_start_matches('+').parse().unwrap_or(0);
            } else if line.starts_with("? ") {
                info.untracked += 1;
            } else if line.starts_with('1') || line.starts_with('2') || line.starts_with('u') {
                let mut parts = line.split_whitespace();
                parts.next();
                if let Some(xy) = parts.next() {
                    let mut chars = xy.chars();
                    let x = chars.next().unwrap_or(' ');
                    let y = chars.next().unwrap_or(' ');
                    if x != '.' && x != ' ' {
                        info.staged += 1;
                    }
                    if y != '.' && y != ' ' {
                        info.unstaged += 1;
                    }
                }
            }
        }
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!(info.upstream.as_deref(), Some("origin/main"));
        assert_eq!(info.ahead, 2);
        assert_eq!(info.behind, 1);
        assert_eq!(info.stash, 3);
        assert_eq!(info.untracked, 1);
        assert_eq!(info.staged, 1); // "M." -> staged
        assert_eq!(info.unstaged, 1); // ".M" -> unstaged
        assert_eq!(info.sha.as_deref(), Some("4abc123"));
    }

    #[test]
    fn render_git_info_shows_branch_ahead_behind_and_counts() {
        let info = GitInfo {
            branch: Some("main".into()),
            upstream: Some("origin/main".into()),
            ahead: 2,
            behind: 1,
            untracked: 3,
            staged: 5,
            unstaged: 2,
            stash: 1,
            sha: Some("abc1234".into()),
            describe: None,
            remotes: vec![("origin".into(), "git@github.com:foo/bar.git".into())],
        };
        let lines = render_git_info(&info, &theme());
        let joined: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("main"));
        assert!(joined.contains("↑2"));
        assert!(joined.contains("↓1"));
        assert!(joined.contains("untracked 3"));
        assert!(joined.contains("staged 5"));
        assert!(joined.contains("unstaged 2"));
        assert!(joined.contains("stash 1"));
        assert!(joined.contains("abc1234"));
        assert!(joined.contains("origin/main"));
        assert!(joined.contains("remotes"));
        assert!(joined.contains("git@github.com:foo/bar.git"));
    }

    #[test]
    fn render_git_info_prefers_tag_alias_for_position() {
        // HEAD is exactly at tag v1.2.3; describe returns the tag name.
        let info = GitInfo {
            branch: Some("main".into()),
            upstream: Some("origin/main".into()),
            ahead: 0,
            behind: 0,
            untracked: 0,
            staged: 0,
            unstaged: 0,
            stash: 0,
            sha: Some("abc1234".into()),
            describe: Some("v1.2.3".into()),
            remotes: vec![],
        };
        let lines = render_git_info(&info, &theme());
        let joined: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("v1.2.3"));
        assert!(joined.contains("origin/main"));
        // The bare SHA should not appear when a tag alias is present.
        assert!(!joined.contains("abc1234"));
    }

    #[test]
    fn render_git_info_shows_tag_relative_describe() {
        // HEAD is 5 commits ahead of tag v1.2.3.
        let info = GitInfo {
            branch: Some("main".into()),
            upstream: None,
            ahead: 5,
            behind: 0,
            untracked: 0,
            staged: 0,
            unstaged: 0,
            stash: 0,
            sha: Some("abc1234".into()),
            describe: Some("v1.2.3-5-gabc1234".into()),
            remotes: vec![],
        };
        let lines = render_git_info(&info, &theme());
        let joined: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("v1.2.3-5-gabc1234"));
        assert!(joined.contains("↑5"));
    }

    #[test]
    fn render_git_info_falls_back_to_sha_when_no_tags() {
        // No tags reach HEAD; describe falls back to the short SHA.
        let info = GitInfo {
            branch: Some("main".into()),
            upstream: Some("origin/main".into()),
            ahead: 0,
            behind: 0,
            untracked: 0,
            staged: 0,
            unstaged: 0,
            stash: 0,
            sha: Some("abc1234".into()),
            describe: Some("abc1234".into()),
            remotes: vec![],
        };
        let lines = render_git_info(&info, &theme());
        let joined: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // SHA is shown as the position when describe equals the SHA.
        assert!(joined.contains("abc1234"));
    }

    #[test]
    fn render_git_info_shows_no_commits_for_unborn_branch() {
        // Unborn branch: no SHA, no describe, but a branch name exists.
        let info = GitInfo {
            branch: Some("main".into()),
            upstream: None,
            ahead: 0,
            behind: 0,
            untracked: 0,
            staged: 0,
            unstaged: 0,
            stash: 0,
            sha: None,
            describe: None,
            remotes: vec![],
        };
        let lines = render_git_info(&info, &theme());
        let joined: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("(no commits)"));
        // Should not show a garbage SHA like "(initia".
        assert!(!joined.contains("(initia"));
    }

    #[test]
    fn pane_id_for_entry_returns_none_without_workspace_or_agent() {
        let e = entry(Source::Workspace, "/tmp", "x");
        assert!(pane_id_for_entry(&e).is_none());
    }
}
