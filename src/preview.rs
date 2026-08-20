//! Source-aware rich preview content for the right-hand preview pane.
//!
//! The preview pane used to show a static metadata dump for every entry. This
//! module builds richer, source-specific content:
//!
//! - `Agent` / `Workspace` → the pane's recent scrollback buffer (with ANSI
//!   colors preserved via `ansi-to-tui`).
//! - `Zoxide` / `Root` → a depth-limited directory tree, prefixed with a
//!   `git status` block when the path is inside a git repository.
//!
//! Other sources fall back to the legacy metadata preview in `tui::preview_text`.
//!
//! All subprocess work (Herdr pane read, `git status`) happens here and is
//! cached by `App` keyed on the selected entry, so it runs once per selection
//! change rather than every render. Missing tools or non-repo paths degrade
//! quietly to a smaller preview.

use std::path::{Path, PathBuf};
use std::process::Command;

use ansi_to_tui::IntoText as _;
use ratatui::text::{Line, Span, Text};

use crate::config::Config;
use crate::herdr::{herdr_json, herdr_text};
use crate::model::{Entry, Source};

/// Build the rich preview for an entry. Returns owned `Text` so it can be
/// cached on `App` and cloned into the render cheaply.
pub(crate) fn build_preview(entry: &Entry, config: &Config) -> Text<'static> {
    let mut lines = header_lines(entry);
    match entry.source {
        Source::Agent | Source::Workspace => {
            if let Some(text) = pane_scrollback(entry, config) {
                lines.push(Line::from(""));
                lines.extend(text.lines);
            } else {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::raw(
                    "no scrollback available for this pane",
                )));
            }
        }
        Source::Zoxide | Source::Root => {
            if config.picker.preview_git_status {
                if let Some(block) = git_status_block(&entry.path) {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::raw("git status")));
                    lines.push(Line::from(Span::raw("──────────")));
                    lines.extend(block.lines);
                }
            }
            lines.push(Line::from(""));
            let tree = directory_tree(
                &entry.path,
                config.picker.preview_tree_depth,
                config.picker.preview_tree_depth as usize * 40,
            );
            if tree.is_empty() {
                lines.push(Line::from(Span::raw("(empty or unreadable directory)")));
            } else {
                lines.extend(tree);
            }
        }
        // Other sources keep the legacy metadata preview; nothing rich here.
        _ => {}
    }
    Text::from(lines)
}

fn header_lines(entry: &Entry) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::raw("type: "),
            Span::raw(entry.source_name().to_string()),
        ]),
        Line::from(vec![Span::raw("title: "), Span::raw(entry.title.clone())]),
        Line::from(vec![
            Span::raw("path: "),
            Span::raw(entry.path.display().to_string()),
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

/// Run `git status --short --branch --show-stash` in `path` and return the
/// output as owned text. Returns `None` if `git` is missing or `path` is not a
/// repository, so non-repo directories degrade quietly.
fn git_status_block(path: &Path) -> Option<Text<'static>> {
    let path_str = path.to_str()?;
    let out = Command::new("git")
        .args([
            "-C",
            path_str,
            "status",
            "--short",
            "--branch",
            "--show-stash",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    Some(Text::from(trimmed.to_string()))
}

/// Build a depth-limited directory tree as plain text lines. Directories are
/// listed before files; entries beyond `max_entries` are summarized with a
/// `… (N more)` line. Symlinks and unreadable entries are skipped quietly.
fn directory_tree(root: &Path, max_depth: u32, max_entries: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut count = 0usize;
    walk(root, "", 0, max_depth, max_entries, &mut lines, &mut count);
    lines
}

fn walk(
    dir: &Path,
    prefix: &str,
    depth: u32,
    max_depth: u32,
    max_entries: usize,
    lines: &mut Vec<Line<'static>>,
    count: &mut usize,
) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut entries: Vec<(String, PathBuf, bool)> = read
        .filter_map(Result::ok)
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().ok().map(|t| t.is_dir()).unwrap_or(false);
            (name, e.path(), is_dir)
        })
        .collect();
    // Directories first, then files; both alphabetical.
    entries.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));

    let total = entries.len();
    for (i, (name, path, is_dir)) in entries.iter().enumerate() {
        if *count >= max_entries {
            lines.push(Line::from(Span::raw(format!(
                "… ({} more)",
                total.saturating_sub(i)
            ))));
            return;
        }
        *count += 1;
        let last = i + 1 == total;
        let marker = if last { "└── " } else { "├── " };
        let suffix = if *is_dir { "/" } else { "" };
        lines.push(Line::from(Span::raw(format!(
            "{prefix}{marker}{name}{suffix}"
        ))));

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
                max_entries,
                lines,
                count,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

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
        let text = build_preview(&e, &Config::default());
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

        let tree = directory_tree(&dir, 2, 40);
        let joined: String = tree
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // Directories first: sub/ before a.txt and b.txt.
        assert!(joined.contains("sub/"));
        assert!(joined.contains("a.txt"));
        assert!(joined.contains("b.txt"));
        assert!(joined.find("sub/").unwrap() < joined.find("a.txt").unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_tree_respects_max_entries() {
        let dir = std::env::temp_dir().join(format!("nav-tree-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..5 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        let tree = directory_tree(&dir, 2, 2);
        let joined: String = tree
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("… (3 more)"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn directory_tree_empty_dir_yields_no_lines() {
        let dir = std::env::temp_dir().join(format!("nav-tree-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tree = directory_tree(&dir, 2, 40);
        assert!(tree.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_status_block_returns_none_for_non_repo() {
        let dir = std::env::temp_dir().join(format!("nav-nogit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(git_status_block(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pane_id_for_entry_returns_none_without_workspace_or_agent() {
        let e = entry(Source::Workspace, "/tmp", "x");
        // No workspace_id and no agent_target -> None (no Herdr call needed to
        // know it cannot resolve).
        assert!(pane_id_for_entry(&e).is_none());
    }
}
