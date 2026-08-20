use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::Path,
};

use crate::{
    config::Config,
    herdr::{herdr_json, notify_done, notify_error, run_herdr, run_herdr_quiet},
    integrations::{command, herdr_plus, sessions},
    matcher::Scorer,
    model::{Entry, EntryAction, Source, WorkspaceKind, WorkspaceRef},
    navigator_state::NavigatorSnapshot,
    paths::{canonical_str, herdr_plus_quick_actions_dir, home, plugin_config_dir},
    sources::{collect_agents, collect_roots, collect_workspaces, collect_zoxide},
    theme::Theme,
};

/// One entry that survived filtering, with its sort keys already resolved.
struct Candidate {
    previous_pinned: bool,
    user_pinned: bool,
    score: i64,
    source_rank: usize,
    idx: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputMode {
    Normal,
    Search,
    Help,
}

pub(crate) struct App {
    pub(crate) config: Config,
    pub(crate) theme: Theme,
    pub(crate) entries: Vec<Entry>,
    pub(crate) filtered: Vec<usize>,
    pub(crate) filtered_scores: Vec<i64>,
    pub(crate) selected: usize,
    pub(crate) query: String,
    pub(crate) input_mode: InputMode,
    pub(crate) source_filter: Option<Source>,
    pub(crate) preview: bool,
    pub(crate) path_to_workspaces: HashMap<String, Vec<WorkspaceRef>>,
    navigator_snapshot: NavigatorSnapshot,
    pub(crate) previous_workspace_id: Option<String>,
    pub(crate) pinned_entries: HashSet<String>,
    pub(crate) spinner_tick: u32,
    pub(crate) update_available: Option<String>,
    pub(crate) list_height: u16,
    /// Tree child entries (tabs/panes) fetched when a workspace/tab is expanded.
    /// Indexed by `filtered` values >= `entries.len()`.
    pub(crate) child_entries: Vec<Entry>,
    /// Expanded workspace/tab IDs in the tree view.
    pub(crate) expanded: std::collections::HashSet<String>,
}

impl App {
    pub(crate) fn new(config: Config, theme: Theme) -> Self {
        let preview = config.picker.preview;
        Self {
            config,
            theme,
            entries: vec![],
            filtered: vec![],
            filtered_scores: vec![],
            selected: 0,
            query: String::new(),
            input_mode: InputMode::Normal,
            source_filter: None,
            preview,
            path_to_workspaces: HashMap::new(),
            navigator_snapshot: NavigatorSnapshot::load(),
            previous_workspace_id: None,
            pinned_entries: HashSet::new(),
            spinner_tick: 0,
            update_available: None,
            list_height: 0,
            child_entries: vec![],
            expanded: std::collections::HashSet::new(),
        }
    }

    pub(crate) fn refresh(&mut self) {
        // Workspaces/tabs/panes may have changed; clear tree children and
        // expansion state so stale children are never shown.
        self.child_entries.clear();
        self.expanded.clear();
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        let (workspace_entries, path_to_workspaces, migrated_legacy, has_live_workspace_list) =
            collect_workspaces(&mut self.navigator_snapshot);
        let reconciled = has_live_workspace_list
            && self.navigator_snapshot.reconcile(
                workspace_entries
                    .iter()
                    .filter_map(|entry| entry.workspace_id.as_deref()),
            );
        if migrated_legacy || reconciled {
            if let Err(error) = self.navigator_snapshot.save() {
                eprintln!("warning: failed to save Navigator workspace metadata: {error}");
            }
        }
        self.path_to_workspaces = path_to_workspaces;

        if self.config.sources.open_workspaces {
            push_unique(&mut entries, &mut seen, workspace_entries.clone());
        }
        if self.config.sources.herdr_plus_projects {
            push_unique(&mut entries, &mut seen, herdr_plus::collect_projects());
        }
        if self.config.sources.zoxide {
            push_unique(&mut entries, &mut seen, collect_zoxide());
        }
        if self.config.sources.roots {
            push_unique(&mut entries, &mut seen, collect_roots(&self.config));
        }
        if self.config.sources.servers {
            push_unique(
                &mut entries,
                &mut seen,
                sessions::collect_remotes(&self.config),
            );
        }
        if self.config.sources.sessions {
            push_unique(
                &mut entries,
                &mut seen,
                sessions::collect_sessions(&self.config),
            );
        }
        if self.config.sources.agents {
            entries.extend(collect_agents(
                &workspace_entries,
                &self.config.agent_aliases,
            ));
        }
        if self.config.sources.herdr_plus_quick_actions && herdr_plus_quick_actions_dir().is_dir() {
            entries.push(herdr_plus::quick_actions_entry());
        }
        push_unique(
            &mut entries,
            &mut seen,
            command::collect(&self.config.integrations),
        );

        self.entries = entries;
        self.pinned_entries =
            read_pinned_entries(&plugin_config_dir().join(PINNED_ENTRIES_STATE_FILE))
                .unwrap_or_default();
        self.previous_workspace_id =
            if self.config.jump_back.enabled && self.config.jump_back.pin_previous {
                read_previous_workspace().ok()
            } else {
                None
            };
        self.apply_filter();
    }

    pub(crate) fn apply_filter(&mut self) {
        let query = Query::parse(&self.query);
        let empty_query = query.plain.is_empty();
        let agent_view =
            query.all_agents || (self.source_filter == Some(Source::Agent) && empty_query);
        let use_agent_priority = empty_query
            && (agent_view || self.source_filter.is_none())
            && agent_sort(&self.config.picker.agent_sort) == "priority";
        let pin_previous = self.config.jump_back.enabled
            && self.config.jump_back.pin_previous
            && self.query.trim().is_empty()
            && self.source_filter.is_none();
        // Everything the comparator needs is resolved here, once per candidate.
        // Deriving it inside `sort_by` instead costs O(n log n) pin lookups, and
        // a pin lookup canonicalizes a path.
        let mut scorer = Scorer::new(&self.config.picker.engine, &query.plain);
        let mut scored: Vec<Candidate> = Vec::new();
        for (idx, e) in self.entries.iter().enumerate() {
            if let Some(sf) = &self.source_filter {
                if &e.source != sf {
                    continue;
                }
            }
            if !query.filters_match(e) {
                continue;
            }
            let bonus = self.config.picker.source_bonus(&e.source)
                + query.score_bonus(e, use_agent_priority);
            let score = if query.plain.is_empty() {
                bonus
            } else if let Some(score) = scorer.score(&e.haystack()) {
                score + bonus
            } else {
                continue;
            };
            scored.push(Candidate {
                previous_pinned: pin_previous
                    && e.source == Source::Workspace
                    && e.workspace_id.as_deref() == self.previous_workspace_id.as_deref(),
                user_pinned: self.is_pinned(e),
                score,
                source_rank: self.config.picker.source_rank(&e.source),
                idx,
            });
        }
        scored.sort_by(|a, b| {
            b.previous_pinned
                .cmp(&a.previous_pinned)
                .then_with(|| b.user_pinned.cmp(&a.user_pinned))
                .then_with(|| b.score.cmp(&a.score))
                .then_with(|| a.source_rank.cmp(&b.source_rank))
                .then_with(|| a.idx.cmp(&b.idx))
        });
        let (scores, filtered): (Vec<_>, Vec<_>) =
            scored.into_iter().map(|c| (c.score, c.idx)).unzip();
        self.filtered = filtered;
        self.filtered_scores = scores;
        self.selected = 0;

        // When the view is unfiltered, interleave expanded tree children
        // (tabs/panes) under their parent entries.
        if self.query.trim().is_empty() && self.source_filter.is_none() {
            self.interleave_children();
        }
    }

    /// Insert tree children (tabs/panes) into `filtered` after their expanded
    /// parents, recursively so expanded tabs get their panes too. Children
    /// are stored in `child_entries`; their filtered index is
    /// `entries.len() + child_index`.
    fn interleave_children(&mut self) {
        if self.expanded.is_empty() || self.filtered.is_empty() {
            return;
        }
        let entries_len = self.entries.len();
        let mut new_filtered = Vec::with_capacity(self.filtered.len());
        let mut new_scores = Vec::with_capacity(self.filtered.len());
        // Recursive walk: for each entry in the base list, push it, then if
        // expanded, push its children (and recurse into each child).
        fn walk(
            idx: usize,
            score: i64,
            app: &App,
            entries_len: usize,
            out: &mut Vec<usize>,
            scores: &mut Vec<i64>,
        ) {
            out.push(idx);
            scores.push(score);
            let Some(entry) = app.get_entry(idx) else {
                return;
            };
            let (expand_key, parent_id) = match &entry.action {
                EntryAction::FocusWorkspace { id } => (format!("ws:{id}"), Some(id.clone())),
                EntryAction::FocusTab { id } => (format!("tab:{id}"), Some(id.clone())),
                _ => return,
            };
            if !app.expanded.contains(&expand_key) {
                return;
            }
            let Some(parent_id) = parent_id else {
                return;
            };
            for (ci, child) in app.child_entries.iter().enumerate() {
                if child.parent_id.as_deref() == Some(&parent_id) {
                    walk(entries_len + ci, 0, app, entries_len, out, scores);
                }
            }
        }
        for (i, &idx) in self.filtered.iter().enumerate() {
            let score = self.filtered_scores.get(i).copied().unwrap_or(0);
            walk(
                idx,
                score,
                self,
                entries_len,
                &mut new_filtered,
                &mut new_scores,
            );
        }
        self.filtered = new_filtered;
        self.filtered_scores = new_scores;
    }

    /// Fetch tabs for a workspace and add them to `child_entries`.
    fn fetch_tabs(&mut self, workspace_id: &str) {
        let json = crate::herdr::herdr_json(["tab", "list", "--workspace", workspace_id])
            .unwrap_or(serde_json::Value::Null);
        let Some(tabs) = json.pointer("/result/tabs").and_then(|v| v.as_array()) else {
            return;
        };
        for t in tabs {
            let tab_id = t.get("tab_id").and_then(|v| v.as_str()).unwrap_or("");
            let label = t.get("label").and_then(|v| v.as_str()).unwrap_or("");
            let focused = t.get("focused").and_then(|v| v.as_bool()).unwrap_or(false);
            let pane_count = t.get("pane_count").and_then(|v| v.as_u64()).unwrap_or(0);
            let status = t
                .get("agent_status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let title = if label.is_empty() {
                format!("tab {}", tab_id)
            } else {
                label.to_string()
            };
            let subtitle = format!(
                "{}{} · {} panes",
                if focused { "focused · " } else { "" },
                status,
                pane_count
            );
            self.child_entries.push(Entry {
                source: Source::Tab,
                title,
                subtitle,
                path: std::path::PathBuf::new(),
                workspace_id: Some(workspace_id.into()),
                workspace_label: None,
                agent_target: None,
                project: None,
                action: EntryAction::FocusTab { id: tab_id.into() },
                source_label: None,
                search_terms: vec![tab_id.into(), label.into()],
                parent_id: Some(workspace_id.into()),
                canonical: std::sync::OnceLock::new(),
            });
        }
    }

    /// Fetch panes for a tab and add them to `child_entries`.
    fn fetch_panes(&mut self, workspace_id: &str, tab_id: &str) {
        let json = crate::herdr::herdr_json(["pane", "list", "--workspace", workspace_id])
            .unwrap_or(serde_json::Value::Null);
        let Some(panes) = json.pointer("/result/panes").and_then(|v| v.as_array()) else {
            return;
        };
        for p in panes {
            let pane_tab = p.get("tab_id").and_then(|v| v.as_str()).unwrap_or("");
            if pane_tab != tab_id {
                continue;
            }
            let pane_id = p.get("pane_id").and_then(|v| v.as_str()).unwrap_or("");
            let label = p.get("label").and_then(|v| v.as_str()).unwrap_or("");
            let focused = p.get("focused").and_then(|v| v.as_bool()).unwrap_or(false);
            let cwd = p.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
            let agent = p.get("agent").and_then(|v| v.as_str());
            let title = if let Some(a) = agent {
                a.to_string()
            } else if !label.is_empty() {
                label.to_string()
            } else {
                pane_id.to_string()
            };
            let subtitle = format!(
                "{}{}{}",
                if focused { "focused · " } else { "" },
                if !cwd.is_empty() {
                    format!("{cwd} · ")
                } else {
                    String::new()
                },
                pane_id
            );
            self.child_entries.push(Entry {
                source: Source::Pane,
                title,
                subtitle,
                path: std::path::PathBuf::from(cwd),
                workspace_id: Some(workspace_id.into()),
                workspace_label: None,
                agent_target: Some(pane_id.into()),
                project: None,
                action: EntryAction::FocusPane { id: pane_id.into() },
                source_label: None,
                search_terms: vec![pane_id.into(), label.into(), cwd.into()],
                parent_id: Some(tab_id.into()),
                canonical: std::sync::OnceLock::new(),
            });
        }
    }

    /// Re-apply the filter while preserving the current selection. Used by
    /// expand/collapse so the cursor doesn't jump to the top of the list.
    fn reapply_filter_preserving_selection(&mut self) {
        let selected_key = self
            .filtered
            .get(self.selected)
            .and_then(|&idx| self.get_entry(idx))
            .map(entry_identity_key);
        self.apply_filter();
        if let Some(key) = selected_key {
            self.selected = self
                .filtered
                .iter()
                .position(|&idx| {
                    self.get_entry(idx)
                        .map(|e| entry_identity_key(e) == key)
                        .unwrap_or(false)
                })
                .unwrap_or(0);
        }
    }

    /// Expand the selected workspace or tab entry, fetching children if needed.
    pub(crate) fn expand_selected(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let (key, workspace_id, tab_id) = match &entry.action {
            EntryAction::FocusWorkspace { id } => (format!("ws:{id}"), Some(id.clone()), None),
            EntryAction::FocusTab { id } => (
                format!("tab:{id}"),
                entry.workspace_id.clone(),
                Some(id.clone()),
            ),
            _ => return,
        };
        if self.expanded.insert(key) {
            // Fetch children if not already present.
            let parent_id = match &entry.action {
                EntryAction::FocusWorkspace { id } => id.clone(),
                EntryAction::FocusTab { id } => id.clone(),
                _ => return,
            };
            let already = self
                .child_entries
                .iter()
                .any(|c| c.parent_id.as_deref() == Some(&parent_id));
            if !already {
                if let Some(tab_id) = tab_id {
                    if let Some(ws_id) = &workspace_id {
                        self.fetch_panes(ws_id, &tab_id);
                    }
                } else if let Some(ws_id) = &workspace_id {
                    self.fetch_tabs(ws_id);
                }
            }
            self.reapply_filter_preserving_selection();
        }
    }

    /// Collapse the selected workspace or tab entry.
    pub(crate) fn collapse_selected(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let key = match &entry.action {
            EntryAction::FocusWorkspace { id } => format!("ws:{id}"),
            EntryAction::FocusTab { id } => format!("tab:{id}"),
            _ => return,
        };
        if self.expanded.remove(&key) {
            self.reapply_filter_preserving_selection();
        }
    }

    /// Toggle expansion of the selected workspace or tab.
    pub(crate) fn toggle_expand_selected(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let key = match &entry.action {
            EntryAction::FocusWorkspace { id } => format!("ws:{id}"),
            EntryAction::FocusTab { id } => format!("tab:{id}"),
            _ => return,
        };
        if self.expanded.contains(&key) {
            self.collapse_selected();
        } else {
            self.expand_selected();
        }
    }

    /// Whether the selected entry is expandable (workspace or tab).
    pub(crate) fn selected_is_expandable(&self) -> bool {
        self.selected_entry().is_some_and(|e| {
            matches!(
                e.action,
                EntryAction::FocusWorkspace { .. } | EntryAction::FocusTab { .. }
            )
        })
    }

    /// Whether the selected entry is currently expanded.
    pub(crate) fn selected_is_expanded(&self) -> bool {
        let Some(entry) = self.selected_entry() else {
            return false;
        };
        let key = match &entry.action {
            EntryAction::FocusWorkspace { id } => format!("ws:{id}"),
            EntryAction::FocusTab { id } => format!("tab:{id}"),
            _ => return false,
        };
        self.expanded.contains(&key)
    }

    /// Drop the trailing word of the query, plus any whitespace before it.
    pub(crate) fn delete_query_word(&mut self) {
        let trimmed = self.query.trim_end();
        let cut = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(idx, c)| idx + c.len_utf8())
            .unwrap_or(0);
        self.query.truncate(cut);
    }

    pub(crate) fn set_filter(&mut self, source: Option<Source>) {
        if source
            .as_ref()
            .is_some_and(|source| !self.config.sources.enabled(source))
        {
            return;
        }
        self.source_filter = if self.source_filter == source {
            None
        } else {
            source
        };
        self.selected = 0;
    }

    pub(crate) fn cycle_filter(&mut self) {
        let sources = self.config.enabled_sources_in_order();
        self.source_filter = match self.source_filter.as_ref() {
            None => sources.first().cloned(),
            Some(cur) => match sources.iter().position(|source| source == cur) {
                Some(pos) => sources.get(pos + 1).cloned(),
                None => sources.first().cloned(),
            },
        };
        self.selected = 0;
        self.apply_filter();
    }

    pub(crate) fn next(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1).min(self.filtered.len() - 1);
        }
    }
    pub(crate) fn prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
    /// Page size in entries: the number of rows the last render could show.
    /// Falls back to a sane default before the first frame is drawn.
    fn page_size(&self) -> usize {
        self.list_height.max(1) as usize
    }
    pub(crate) fn page_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len() - 1;
        self.selected = self.selected.saturating_add(self.page_size()).min(max);
    }
    pub(crate) fn page_up(&mut self) {
        self.selected = self.selected.saturating_sub(self.page_size());
    }
    /// Resolve a filtered index to an entry. Indices < `entries.len()` map to
    /// top-level entries; indices >= `entries.len()` map to tree children.
    pub(crate) fn get_entry(&self, idx: usize) -> Option<&Entry> {
        if idx < self.entries.len() {
            self.entries.get(idx)
        } else {
            self.child_entries.get(idx - self.entries.len())
        }
    }

    pub(crate) fn selected_entry(&self) -> Option<&Entry> {
        self.filtered
            .get(self.selected)
            .and_then(|&idx| self.get_entry(idx))
    }

    pub(crate) fn is_pinned(&self, entry: &Entry) -> bool {
        self.pinned_entries.contains(&pin_key(entry))
    }

    pub(crate) fn toggle_selected_pin(&mut self) -> Result<(), String> {
        let key = self
            .selected_entry()
            .map(pin_key)
            .ok_or("nothing selected")?;
        let mut pinned = self.pinned_entries.clone();
        if !pinned.remove(&key) {
            pinned.insert(key);
        }
        save_pinned_entries(
            &plugin_config_dir().join(PINNED_ENTRIES_STATE_FILE),
            &pinned,
        )?;
        self.pinned_entries = pinned;
        self.apply_filter();
        Ok(())
    }

    pub(crate) fn directory_template_for_selected(&self) -> Option<&str> {
        let template = self
            .config
            .picker
            .directory_template
            .as_deref()
            .map(str::trim)
            .filter(|template| !template.is_empty())?;
        matches!(
            &self.selected_entry()?.action,
            EntryAction::FocusOrCreateDir
        )
        .then_some(template)
    }

    pub(crate) fn open_selected(&mut self, use_directory_template: bool) -> Result<(), String> {
        let e = self.selected_entry().cloned().ok_or("nothing selected")?;
        let tracks_workspace_transition = self.config.jump_back.enabled
            && matches!(
                &e.action,
                EntryAction::FocusAgent { .. }
                    | EntryAction::FocusWorkspace { .. }
                    | EntryAction::FocusTab { .. }
                    | EntryAction::FocusPane { .. }
                    | EntryAction::OpenProject
                    | EntryAction::FocusOrCreateDir
            );
        let origin_workspace = if tracks_workspace_transition {
            launch_workspace_id().or_else(|| current_workspace_id().ok())
        } else {
            None
        };
        let (result, notify_success, notify_failure) = match &e.action {
            EntryAction::FocusAgent { target } => {
                (run_herdr(["agent", "focus", target]), true, true)
            }
            EntryAction::FocusWorkspace { id } => {
                (run_herdr(["workspace", "focus", id]), true, true)
            }
            EntryAction::FocusTab { id } => (run_herdr(["tab", "focus", id]), true, true),
            EntryAction::FocusPane { id } => (run_herdr(["pane", "focus", id]), true, true),
            EntryAction::OpenProject => (self.open_project(&e), true, true),
            EntryAction::OpenRemote { target } => (sessions::open_remote(target), false, true),
            EntryAction::AttachSession { name, .. } => {
                (sessions::attach_session(name), false, true)
            }
            EntryAction::InvokePluginAction { action } => (
                run_herdr(["plugin", "action", "invoke", action]),
                true,
                true,
            ),
            EntryAction::FocusOrCreateDir => (
                self.focus_or_create_dir(&e.path, &e.title, use_directory_template),
                true,
                true,
            ),
            EntryAction::RunCommand {
                command,
                notify_success,
                notify_error,
            } => (
                command::run_command(command),
                *notify_success,
                *notify_error,
            ),
        };

        match result {
            Ok(()) => {
                if tracks_workspace_transition {
                    let destination = current_workspace_id().ok();
                    if let Some(previous) = previous_workspace_to_record(
                        origin_workspace.as_deref(),
                        destination.as_deref(),
                    ) {
                        let _ = save_previous_workspace(previous);
                    }
                }
                if notify_success {
                    notify_done(&format!("Opened {}", e.title), &self.config.notifications);
                }
                Ok(())
            }
            Err(err) => {
                if notify_failure {
                    notify_error(
                        &format!("Failed {}: {}", e.title, err.trim()),
                        &self.config.notifications,
                    );
                }
                Err(err)
            }
        }
    }

    pub(crate) fn close_selected_workspace(&mut self) -> Result<(), String> {
        let (id, title) = {
            let e = self.selected_entry().ok_or("nothing selected")?;
            let id = self
                .workspace_to_close(e)
                .ok_or("no open workspace for selected item")?;
            (id, e.title.clone())
        };
        let origin_workspace = launch_workspace_id();
        if let Some(err) = close_current_workspace_error(&id, origin_workspace.as_deref()) {
            return Err(err);
        }
        run_herdr_quiet(["workspace", "close", &id])?;
        let focus_result = workspace_focus_to_restore(&id, origin_workspace.as_deref())
            .map_or(Ok(()), |origin| {
                run_herdr_quiet(["workspace", "focus", origin])
            });
        self.refresh();
        if let Err(error) = focus_result {
            notify_error(
                &format!("Closed {title}, but failed to restore focus: {error}"),
                &self.config.notifications,
            );
            return Ok(());
        }
        notify_done(&format!("Closed {title}"), &self.config.notifications);
        Ok(())
    }

    fn workspace_to_close(&self, e: &Entry) -> Option<String> {
        match e.source {
            Source::Workspace | Source::Agent => e.workspace_id.clone(),
            Source::Project => self.matching_project_workspace(e).map(|ws| ws.id.clone()),
            Source::Zoxide | Source::Root => self.matching_dir_workspace(e).map(|ws| ws.id.clone()),
            Source::Server | Source::Session | Source::QuickAction | Source::Integration => None,
            Source::Tab | Source::Pane => None,
        }
    }

    pub(crate) fn open_project(&mut self, e: &Entry) -> Result<(), String> {
        if self.config.picker.reuse_existing {
            if let Some(ws) = self.matching_project_workspace(e) {
                return run_herdr(["workspace", "focus", &ws.id]);
            }
        }
        if !self.config.picker.create_missing {
            return Err("create_missing=false and no workspace exists".into());
        }
        let project = e.project.as_ref();
        let label = created_workspace_label(
            WorkspaceKind::Project,
            project.map(|p| p.name.as_str()).unwrap_or(&e.title),
            self.config.picker.prefix_workspace_labels,
        );
        let json = herdr_json([
            "workspace",
            "create",
            "--cwd",
            &e.path.display().to_string(),
            "--label",
            &label,
            "--focus",
        ])?;
        self.record_created_workspace(&json, WorkspaceKind::Project);
        if let Some(p) = project {
            herdr_plus::bootstrap_project_tabs(p, &json, &e.path)?;
        }
        Ok(())
    }

    pub(crate) fn focus_or_create_dir(
        &mut self,
        path: &Path,
        label: &str,
        use_directory_template: bool,
    ) -> Result<(), String> {
        let key = canonical_str(path).unwrap_or_else(|| path.display().to_string());
        if use_directory_template {
            let template_name = self
                .config
                .picker
                .directory_template
                .as_deref()
                .ok_or("no directory_template configured")?;
            let template = herdr_plus::load_project_template(template_name)?;
            if let Some(workspace_id) = self
                .matching_template_workspace_by_key(&key)
                .map(|workspace| workspace.id.clone())
            {
                run_herdr(["workspace", "focus", &workspace_id])?;
                return herdr_plus::append_project_tabs(&template, &workspace_id, path);
            }
            let json = herdr_json([
                "workspace",
                "create",
                "--cwd",
                &path.display().to_string(),
                "--label",
                &created_workspace_label(
                    WorkspaceKind::Dir,
                    label,
                    self.config.picker.prefix_workspace_labels,
                ),
                "--focus",
            ])?;
            self.record_created_workspace(&json, WorkspaceKind::Dir);
            return herdr_plus::bootstrap_project_tabs(&template, &json, path);
        }

        if self.config.picker.reuse_existing {
            if let Some(ws) = self.matching_dir_workspace_by_key(&key) {
                return run_herdr(["workspace", "focus", &ws.id]);
            }
        }
        if !self.config.picker.create_missing {
            return Err("create_missing=false and no workspace exists".into());
        }
        let json = herdr_json([
            "workspace",
            "create",
            "--cwd",
            &path.display().to_string(),
            "--label",
            &created_workspace_label(
                WorkspaceKind::Dir,
                label,
                self.config.picker.prefix_workspace_labels,
            ),
            "--focus",
        ])?;
        self.record_created_workspace(&json, WorkspaceKind::Dir);
        Ok(())
    }

    fn record_created_workspace(&mut self, response: &serde_json::Value, kind: WorkspaceKind) {
        let Some(id) = workspace_created_id(response) else {
            eprintln!("warning: Herdr created a workspace but did not return its workspace ID");
            return;
        };
        self.navigator_snapshot.record(id, kind);
        if let Err(error) = self.navigator_snapshot.save() {
            eprintln!("warning: failed to save Navigator workspace metadata: {error}");
        }
    }

    pub(crate) fn workspaces_for_entry(&self, e: &Entry) -> &[WorkspaceRef] {
        self.path_to_workspaces
            .get(e.key())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub(crate) fn matching_project_workspace(&self, e: &Entry) -> Option<&WorkspaceRef> {
        self.workspaces_for_entry(e)
            .iter()
            .find(|ws| ws.kind == WorkspaceKind::Project)
    }

    pub(crate) fn matching_dir_workspace(&self, e: &Entry) -> Option<&WorkspaceRef> {
        self.matching_dir_workspace_by_key(e.key())
    }

    fn matching_dir_workspace_by_key(&self, key: &str) -> Option<&WorkspaceRef> {
        self.path_to_workspaces
            .get(key)?
            .iter()
            .find(|ws| ws.kind == WorkspaceKind::Dir)
    }

    fn matching_template_workspace_by_key(&self, key: &str) -> Option<&WorkspaceRef> {
        let workspaces = self.path_to_workspaces.get(key)?;
        workspaces
            .iter()
            .find(|workspace| workspace.kind == WorkspaceKind::Dir)
            .or_else(|| workspaces.first())
    }
}

struct Query {
    plain: String,
    agent: Vec<String>,
    workspace_or_status: Vec<String>,
    path: Vec<String>,
    status: Vec<String>,
    all_agents: bool,
}

impl Query {
    fn parse(input: &str) -> Self {
        let mut query = Self {
            plain: String::new(),
            agent: vec![],
            workspace_or_status: vec![],
            path: vec![],
            status: vec![],
            all_agents: false,
        };
        let mut plain = Vec::new();
        for raw in input.split_whitespace() {
            let token = raw.to_lowercase();
            if let Some(rest) = token.strip_prefix('!') {
                push_token(&mut query.agent, rest);
            } else if let Some(rest) = token.strip_prefix('@') {
                if rest.is_empty() {
                    query.all_agents = true;
                } else {
                    push_token(&mut query.workspace_or_status, rest);
                }
            } else if let Some(rest) = token.strip_prefix('/') {
                push_token(&mut query.path, rest);
            } else if let Some(rest) = token.strip_prefix('#') {
                push_token(&mut query.status, rest);
            } else {
                plain.push(token);
            }
        }
        query.plain = plain.join(" ");
        query
    }

    fn filters_match(&self, entry: &Entry) -> bool {
        let agent_query = self.all_agents
            || !self.agent.is_empty()
            || !self.workspace_or_status.is_empty()
            || !self.status.is_empty();
        if agent_query && entry.source != Source::Agent {
            return false;
        }
        all_match(&self.agent, &agent_text(entry))
            && all_match_either(
                &self.workspace_or_status,
                &workspace_text(entry),
                &status_text(entry),
            )
            && all_match(&self.path, &entry.path.display().to_string())
            && all_match(&self.status, &status_text(entry))
    }

    fn score_bonus(&self, entry: &Entry, use_agent_priority: bool) -> i64 {
        if entry.source == Source::Agent && use_agent_priority {
            agent_status_bonus(entry)
        } else {
            0
        }
    }
}

fn push_token(tokens: &mut Vec<String>, value: &str) {
    if !value.is_empty() {
        tokens.push(value.into());
    }
}

fn all_match(tokens: &[String], haystack: &str) -> bool {
    let haystack = haystack.to_lowercase();
    tokens.iter().all(|token| haystack.contains(token))
}

fn all_match_either(tokens: &[String], left: &str, right: &str) -> bool {
    let left = left.to_lowercase();
    let right = right.to_lowercase();
    tokens
        .iter()
        .all(|token| left.contains(token) || right.contains(token))
}

fn agent_status_bonus(entry: &Entry) -> i64 {
    let status = status_text(entry);
    if ["block", "fail", "error"]
        .iter()
        .any(|needle| status.contains(needle))
    {
        4
    } else if ["need", "attention", "review", "request", "question", "wait"]
        .iter()
        .any(|needle| status.contains(needle))
    {
        3
    } else if status.contains("done") || status.contains("complete") {
        2
    } else if status.contains("work") || status.contains("run") {
        1
    } else {
        0
    }
}

fn status_text(entry: &Entry) -> String {
    entry
        .subtitle
        .split('·')
        .next()
        .unwrap_or(&entry.subtitle)
        .trim()
        .to_lowercase()
}

fn agent_text(entry: &Entry) -> String {
    entry
        .title
        .split('·')
        .next()
        .unwrap_or(&entry.title)
        .to_string()
}

fn workspace_text(entry: &Entry) -> String {
    format!(
        "{} {} {}",
        entry.workspace_id.as_deref().unwrap_or(""),
        entry.workspace_label.as_deref().unwrap_or(""),
        entry.title
    )
}

const JUMP_BACK_STATE_FILE: &str = "jump-back-workspace";
const PINNED_ENTRIES_STATE_FILE: &str = "pinned-entries.json";

pub(crate) fn jump_back(config: &Config) -> Result<String, String> {
    if !config.jump_back.enabled {
        return Err("jump back is disabled in config".into());
    }
    let json = herdr_json(["workspace", "list"])?;
    let current = focused_workspace_id(&json)
        .ok_or("can't determine the current workspace")?
        .to_string();
    let previous = read_previous_workspace()?;
    if previous == current {
        return Err("no previous workspace yet".into());
    }
    let Some(label) = workspace_label(&json, &previous).map(str::to_string) else {
        let _ = fs::remove_file(plugin_config_dir().join(JUMP_BACK_STATE_FILE));
        return Err("previous workspace no longer exists".into());
    };

    run_herdr(["workspace", "focus", &previous])?;
    save_previous_workspace(&current)?;
    Ok(label)
}

fn workspace_created_id(json: &serde_json::Value) -> Option<&str> {
    json.pointer("/result/workspace/workspace_id")?.as_str()
}

fn created_workspace_label(
    kind: WorkspaceKind,
    label: &str,
    prefix_workspace_labels: bool,
) -> String {
    if !prefix_workspace_labels {
        return label.into();
    }
    let prefix = match kind {
        WorkspaceKind::Project => "project",
        WorkspaceKind::Dir => "dir",
        WorkspaceKind::Unknown => return label.into(),
    };
    format!("{prefix}: {label}")
}

fn focused_workspace_id(json: &serde_json::Value) -> Option<&str> {
    json.pointer("/result/workspaces")
        .and_then(|v| v.as_array())?
        .iter()
        .find(|workspace| workspace.get("focused").and_then(|v| v.as_bool()) == Some(true))?
        .get("workspace_id")?
        .as_str()
}

fn workspace_label<'a>(json: &'a serde_json::Value, id: &str) -> Option<&'a str> {
    json.pointer("/result/workspaces")
        .and_then(|v| v.as_array())?
        .iter()
        .find(|workspace| workspace.get("workspace_id").and_then(|v| v.as_str()) == Some(id))?
        .get("label")?
        .as_str()
}

fn current_workspace_id() -> Result<String, String> {
    let json = herdr_json(["workspace", "list"])?;
    focused_workspace_id(&json)
        .map(str::to_string)
        .ok_or_else(|| "can't determine the current workspace".into())
}

fn previous_workspace_to_record<'a>(
    origin: Option<&'a str>,
    destination: Option<&str>,
) -> Option<&'a str> {
    match (origin, destination) {
        (Some(origin), Some(destination)) if origin != destination => Some(origin),
        _ => None,
    }
}

fn read_previous_workspace() -> Result<String, String> {
    let path = plugin_config_dir().join(JUMP_BACK_STATE_FILE);
    let value = fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "no previous workspace yet".into()
        } else {
            format!("failed to read jump-back state: {error}")
        }
    })?;
    let value = value.trim();
    if value.is_empty() {
        Err("no previous workspace yet".into())
    } else {
        Ok(value.into())
    }
}

fn save_previous_workspace(id: &str) -> Result<(), String> {
    let dir = plugin_config_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("failed to create plugin config: {e}"))?;
    fs::write(dir.join(JUMP_BACK_STATE_FILE), id)
        .map_err(|e| format!("failed to save jump-back state: {e}"))
}

fn read_pinned_entries(path: &Path) -> Result<HashSet<String>, String> {
    let value =
        fs::read_to_string(path).map_err(|e| format!("failed to read pinned entries: {e}"))?;
    serde_json::from_str::<Vec<String>>(&value)
        .map(|entries| entries.into_iter().collect())
        .map_err(|e| format!("failed to parse pinned entries: {e}"))
}

fn save_pinned_entries(path: &Path, entries: &HashSet<String>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("failed to create plugin config: {e}"))?;
    }
    let mut entries = entries.iter().collect::<Vec<_>>();
    entries.sort_unstable();
    let value = serde_json::to_string_pretty(&entries)
        .map_err(|e| format!("failed to encode pinned entries: {e}"))?;
    fs::write(path, value).map_err(|e| format!("failed to save pinned entries: {e}"))
}

fn launch_workspace_id() -> Option<String> {
    env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("workspace_id")?.as_str().map(str::to_string))
        .filter(|s| !s.is_empty())
}

fn close_current_workspace_error(id: &str, current: Option<&str>) -> Option<String> {
    (current == Some(id))
        .then(|| "can't close the workspace that owns this picker; switch away first".into())
}

fn workspace_focus_to_restore<'a>(closed_id: &str, origin: Option<&'a str>) -> Option<&'a str> {
    origin.filter(|origin| *origin != closed_id)
}

fn agent_sort(configured: &str) -> String {
    match configured.to_lowercase().as_str() {
        "priority" => "priority".into(),
        "spaces" => "spaces".into(),
        _ => herdr_agent_panel_sort(),
    }
}

fn herdr_agent_panel_sort() -> String {
    let path = std::env::var("XDG_CONFIG_HOME")
        .map(|xdg| Path::new(&xdg).join("herdr/config.toml"))
        .unwrap_or_else(|_| home().join(".config/herdr/config.toml"));
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse::<toml::Value>().ok())
        .and_then(|v| {
            v.get("ui")
                .and_then(|x| x.as_table())
                .and_then(|x| x.get("agent_panel_sort"))
                .or_else(|| v.get("agent_panel_sort"))
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "spaces".into())
}

/// A stable identity for an entry, used to preserve selection across
/// filter rebuilds (expansion/collapse). Distinct from `pin_key` which is
/// for user pins; this includes the source so a workspace and a tab child
/// with the same id don't collide.
fn entry_identity_key(entry: &Entry) -> String {
    match &entry.action {
        EntryAction::FocusWorkspace { id } => format!("workspace:{id}"),
        EntryAction::FocusAgent { target } => format!("agent:{target}"),
        EntryAction::FocusTab { id } => format!("tab:{id}"),
        EntryAction::FocusPane { id } => format!("pane:{id}"),
        EntryAction::OpenProject => format!("project:{}", entry.key()),
        EntryAction::OpenRemote { target } => format!("remote:{target}"),
        EntryAction::AttachSession { name, remote } => {
            format!("session:{}:{name}", remote.as_deref().unwrap_or("local"))
        }
        EntryAction::InvokePluginAction { action } => {
            format!("plugin:{}:{action}", entry.source_name())
        }
        EntryAction::FocusOrCreateDir => format!("{}:{}", entry.source_name(), entry.key()),
        EntryAction::RunCommand { command, .. } => {
            format!("{}:{command}", entry.source_name())
        }
    }
}

fn pin_key(entry: &Entry) -> String {
    match &entry.action {
        EntryAction::FocusWorkspace { id } => format!("workspace:{id}"),
        EntryAction::FocusAgent { target } => format!("agent:{target}"),
        EntryAction::FocusTab { id } => format!("tab:{id}"),
        EntryAction::FocusPane { id } => format!("pane:{id}"),
        EntryAction::OpenProject => format!("project:{}", entry.key()),
        EntryAction::OpenRemote { target } => format!("remote:{target}"),
        EntryAction::AttachSession { name, remote } => {
            format!("session:{}:{name}", remote.as_deref().unwrap_or("local"))
        }
        EntryAction::InvokePluginAction { action } => {
            format!("plugin:{}:{action}", entry.source_name())
        }
        EntryAction::FocusOrCreateDir => format!("{}:{}", entry.source_name(), entry.key()),
        EntryAction::RunCommand { command, .. } => {
            format!("{}:{command}", entry.source_name())
        }
    }
}

fn push_unique(entries: &mut Vec<Entry>, seen: &mut HashSet<String>, incoming: Vec<Entry>) {
    for e in incoming {
        let key = match &e.action {
            EntryAction::FocusWorkspace { id } => format!("open:{id}"),
            EntryAction::OpenRemote { target } => format!("remote:{target}"),
            EntryAction::AttachSession { name, remote } => {
                format!("session:{}:{name}", remote.as_deref().unwrap_or("local"))
            }
            EntryAction::RunCommand { command, .. } => format!("{}:{command}", e.source_name()),
            _ => format!("{}:{}", e.source_name(), e.key()),
        };
        if seen.insert(key) {
            entries.push(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::OnceLock,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::{config::Config, model::Project, theme::Theme};

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
            action: EntryAction::FocusOrCreateDir,
            source_label: None,
            search_terms: vec![],
            parent_id: None,
            canonical: OnceLock::new(),
        }
    }

    fn workspace(id: &str, label: &str, kind: WorkspaceKind, path: &str) -> WorkspaceRef {
        WorkspaceRef {
            id: id.into(),
            label: label.into(),
            kind,
            path: PathBuf::from(path),
            tab_count: 1,
            pane_count: 1,
        }
    }

    #[test]
    fn created_workspace_labels_prefix_by_default_and_can_be_plain() {
        assert_eq!(
            created_workspace_label(WorkspaceKind::Project, "Navigator", true),
            "project: Navigator"
        );
        assert_eq!(
            created_workspace_label(WorkspaceKind::Dir, "tmp", false),
            "tmp"
        );
    }

    #[test]
    fn extracts_created_workspace_stable_id() {
        let response = serde_json::json!({
            "result": {"type": "workspace_created", "workspace": {"workspace_id": "w42"}}
        });
        assert_eq!(workspace_created_id(&response), Some("w42"));
        assert_eq!(workspace_created_id(&serde_json::Value::Null), None);
    }

    fn agent_entry() -> Entry {
        agent_entry_with_status("idle")
    }

    fn agent_entry_with_status(status: &str) -> Entry {
        Entry {
            source: Source::Agent,
            title: "claude · Dotfiles · dotfiles".into(),
            subtitle: format!("{status} · wF:p2 · wF:t2"),
            path: PathBuf::from("/home/fenix/dotfiles"),
            workspace_id: Some("wF".into()),
            workspace_label: Some("Dotfiles".into()),
            agent_target: Some("term_1".into()),
            project: None,
            action: EntryAction::FocusAgent {
                target: "term_1".into(),
            },
            source_label: None,
            search_terms: vec!["main ai dot".into()],
            parent_id: None,
            canonical: OnceLock::new(),
        }
    }

    #[test]
    fn agent_token_filters_match_identity_parts() {
        let agent = agent_entry();

        assert!(Query::parse("!claude @dot /dot #idle").filters_match(&agent));
        assert!(Query::parse("@wF").filters_match(&agent));
        assert!(!Query::parse("!codex").filters_match(&agent));
        assert!(!Query::parse("!dotfiles").filters_match(&agent));
        assert!(!Query::parse("!claude").filters_match(&entry(Source::Project, "/tmp", "claude")));
    }

    #[test]
    fn agent_shortcut_shows_all_agents_and_priority_is_configurable() {
        let idle = agent_entry_with_status("idle");
        let working = agent_entry_with_status("working");
        let blocked = agent_entry_with_status("blocking");
        let attention = agent_entry_with_status("needs attention");
        let done = agent_entry_with_status("done");

        assert!(Query::parse("@").filters_match(&idle));
        assert!(Query::parse("@").filters_match(&blocked));
        assert!(Query::parse("@idle").filters_match(&idle));
        assert!(Query::parse("@Dotfiles").filters_match(&idle));
        assert_eq!(agent_status_bonus(&blocked), 4);
        assert_eq!(agent_status_bonus(&attention), 3);
        assert_eq!(agent_status_bonus(&done), 2);
        assert_eq!(agent_status_bonus(&working), 1);
        assert_eq!(agent_status_bonus(&idle), 0);
        assert_eq!(agent_sort("priority"), "priority");
        assert_eq!(agent_sort("spaces"), "spaces");
    }

    #[test]
    fn agent_aliases_are_searchable_plain_text() {
        assert!(agent_entry().haystack().contains("main ai dot"));
    }

    #[test]
    fn default_empty_picker_prioritizes_agent_status() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        app.config.picker.agent_sort = "priority".into();
        app.entries = vec![
            agent_entry_with_status("idle"),
            agent_entry_with_status("done"),
        ];
        app.apply_filter();

        let first = app.get_entry(app.filtered[0]).unwrap();
        assert!(first.subtitle.starts_with("done"));
    }

    #[test]
    fn cycle_filter_follows_enabled_source_order() {
        let mut app = App::new(
            toml::from_str(
                r#"
                [picker]
                source_order = ["agent", "workspace", "project"]

                [sources]
                servers = false
                sessions = false
                "#,
            )
            .unwrap(),
            Theme::load(None, None, false),
        );

        app.cycle_filter();
        assert_eq!(app.source_filter, Some(Source::Agent));
        app.cycle_filter();
        assert_eq!(app.source_filter, Some(Source::Workspace));
        app.cycle_filter();
        assert_eq!(app.source_filter, Some(Source::Project));
    }

    #[test]
    fn page_down_advances_by_list_height_and_clamps_at_end() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        app.entries = (0..30)
            .map(|i| entry(Source::Workspace, &format!("/p{i}"), &format!("p{i}")))
            .collect();
        app.apply_filter();
        assert_eq!(app.selected, 0);

        // No recorded height yet -> page size falls back to 1.
        app.list_height = 0;
        app.page_down();
        assert_eq!(app.selected, 1);

        // A 10-row list pages by 10 entries.
        app.list_height = 10;
        app.page_down();
        assert_eq!(app.selected, 11);
        app.page_down();
        assert_eq!(app.selected, 21);
        // Clamps at the last entry (index 29).
        app.page_down();
        assert_eq!(app.selected, 29);
        app.page_down();
        assert_eq!(app.selected, 29);
    }

    #[test]
    fn page_up_moves_back_by_list_height_and_clamps_at_zero() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        app.entries = (0..30)
            .map(|i| entry(Source::Workspace, &format!("/p{i}"), &format!("p{i}")))
            .collect();
        app.apply_filter();
        app.selected = 29;

        app.list_height = 10;
        app.page_up();
        assert_eq!(app.selected, 19);
        app.page_up();
        assert_eq!(app.selected, 9);
        // Clamps at zero.
        app.page_up();
        assert_eq!(app.selected, 0);
        app.page_up();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn page_movement_is_noop_on_empty_results() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        app.entries = vec![];
        app.apply_filter();
        app.list_height = 10;
        app.page_down();
        assert_eq!(app.selected, 0);
        app.page_up();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn expand_and_collapse_workspace_toggles_expanded_set() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        let mut ws = entry(Source::Workspace, "/tmp", "x");
        ws.workspace_id = Some("w1".into());
        ws.action = EntryAction::FocusWorkspace { id: "w1".into() };
        app.entries = vec![ws];
        app.apply_filter();

        // Not expanded initially.
        assert!(!app.selected_is_expanded());
        assert!(app.selected_is_expandable());

        // Expand — adds to expanded set (no Herdr call since we test state only).
        app.expanded.insert("ws:w1".to_string());
        assert!(app.selected_is_expanded());

        // Collapse — removes from expanded set.
        app.expanded.remove("ws:w1");
        assert!(!app.selected_is_expanded());
    }

    #[test]
    fn interleave_children_inserts_tabs_after_expanded_workspace() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        let mut ws = entry(Source::Workspace, "/tmp", "x");
        ws.workspace_id = Some("w1".into());
        ws.action = EntryAction::FocusWorkspace { id: "w1".into() };
        app.entries = vec![ws];
        app.apply_filter();

        // Manually add a tab child (simulating fetch_tabs).
        app.child_entries.push(Entry {
            source: Source::Tab,
            title: "main".into(),
            subtitle: "1 panes".into(),
            path: PathBuf::new(),
            workspace_id: Some("w1".into()),
            workspace_label: None,
            agent_target: None,
            project: None,
            action: EntryAction::FocusTab { id: "w1:t1".into() },
            source_label: None,
            search_terms: vec![],
            parent_id: Some("w1".into()),
            canonical: OnceLock::new(),
        });

        // Without expansion, filtered is just the workspace.
        assert_eq!(app.filtered.len(), 1);

        // Expand and re-apply filter.
        app.expanded.insert("ws:w1".to_string());
        app.apply_filter();
        // Workspace + 1 tab child.
        assert_eq!(app.filtered.len(), 2);
        // The second entry should be the tab child.
        let child = app.get_entry(app.filtered[1]).unwrap();
        assert_eq!(child.source, Source::Tab);
        assert_eq!(child.title, "main");
    }

    #[test]
    fn interleave_children_excluded_when_query_active() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        let mut ws = entry(Source::Workspace, "/tmp", "x");
        ws.workspace_id = Some("w1".into());
        ws.action = EntryAction::FocusWorkspace { id: "w1".into() };
        app.entries = vec![ws];
        app.apply_filter();

        app.child_entries.push(Entry {
            source: Source::Tab,
            title: "main".into(),
            subtitle: String::new(),
            path: PathBuf::new(),
            workspace_id: Some("w1".into()),
            workspace_label: None,
            agent_target: None,
            project: None,
            action: EntryAction::FocusTab { id: "w1:t1".into() },
            source_label: None,
            search_terms: vec![],
            parent_id: Some("w1".into()),
            canonical: OnceLock::new(),
        });
        app.expanded.insert("ws:w1".to_string());

        // With a query, children are not interleaved (flat search).
        app.query = "x".into();
        app.apply_filter();
        assert_eq!(app.filtered.len(), 1); // just the workspace
    }

    #[test]
    fn expand_preserves_current_selection() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        let mut ws1 = entry(Source::Workspace, "/a", "alpha");
        ws1.workspace_id = Some("w1".into());
        ws1.action = EntryAction::FocusWorkspace { id: "w1".into() };
        let mut ws2 = entry(Source::Workspace, "/b", "bravo");
        ws2.workspace_id = Some("w2".into());
        ws2.action = EntryAction::FocusWorkspace { id: "w2".into() };
        app.entries = vec![ws1, ws2];
        app.apply_filter();

        // Select the second workspace.
        app.selected = 1;
        assert_eq!(app.selected_entry().unwrap().title, "bravo");

        // Expand it — selection must stay on bravo, not reset to alpha.
        app.expand_selected();
        assert_eq!(app.selected_entry().unwrap().title, "bravo");
        assert!(app.selected_is_expanded());

        // Collapse — selection still stays on bravo.
        app.collapse_selected();
        assert_eq!(app.selected_entry().unwrap().title, "bravo");
        assert!(!app.selected_is_expanded());
    }

    #[test]
    fn interleave_children_recurses_into_expanded_tabs() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        let mut ws = entry(Source::Workspace, "/tmp", "x");
        ws.workspace_id = Some("w1".into());
        ws.action = EntryAction::FocusWorkspace { id: "w1".into() };
        app.entries = vec![ws];
        app.apply_filter();

        // Add a tab child and a pane child (simulating fetch).
        app.child_entries.push(Entry {
            source: Source::Tab,
            title: "main".into(),
            subtitle: String::new(),
            path: PathBuf::new(),
            workspace_id: Some("w1".into()),
            workspace_label: None,
            agent_target: None,
            project: None,
            action: EntryAction::FocusTab { id: "w1:t1".into() },
            source_label: None,
            search_terms: vec![],
            parent_id: Some("w1".into()),
            canonical: OnceLock::new(),
        });
        app.child_entries.push(Entry {
            source: Source::Pane,
            title: "shell".into(),
            subtitle: String::new(),
            path: PathBuf::new(),
            workspace_id: Some("w1".into()),
            workspace_label: None,
            agent_target: None,
            project: None,
            action: EntryAction::FocusPane {
                id: "w1:t1:p1".into(),
            },
            source_label: None,
            search_terms: vec![],
            parent_id: Some("w1:t1".into()),
            canonical: OnceLock::new(),
        });

        // Expand both the workspace and its tab.
        app.expanded.insert("ws:w1".to_string());
        app.expanded.insert("tab:w1:t1".to_string());
        app.apply_filter();

        // Should see: workspace, tab, pane (3 entries).
        assert_eq!(app.filtered.len(), 3);
        assert_eq!(
            app.get_entry(app.filtered[0]).unwrap().source,
            Source::Workspace
        );
        assert_eq!(app.get_entry(app.filtered[1]).unwrap().source, Source::Tab);
        assert_eq!(app.get_entry(app.filtered[2]).unwrap().source, Source::Pane);
    }

    #[test]
    fn previous_workspace_is_pinned_only_on_initial_unfiltered_view() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        let mut alpha = entry(Source::Workspace, "/alpha", "alpha");
        alpha.workspace_id = Some("w1".into());
        alpha.action = EntryAction::FocusWorkspace { id: "w1".into() };
        let mut zulu = entry(Source::Workspace, "/zulu", "zulu");
        zulu.workspace_id = Some("w2".into());
        zulu.action = EntryAction::FocusWorkspace { id: "w2".into() };
        app.entries = vec![alpha, zulu];
        app.previous_workspace_id = Some("w2".into());

        app.apply_filter();
        assert_eq!(
            app.selected_entry().unwrap().workspace_id.as_deref(),
            Some("w2")
        );

        app.source_filter = Some(Source::Workspace);
        app.apply_filter();
        assert_eq!(
            app.selected_entry().unwrap().workspace_id.as_deref(),
            Some("w1")
        );

        app.source_filter = None;
        app.config.jump_back.pin_previous = false;
        app.apply_filter();
        assert_eq!(
            app.selected_entry().unwrap().workspace_id.as_deref(),
            Some("w1")
        );
    }

    #[test]
    fn equal_score_ties_preserve_insertion_order() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        app.entries = vec![
            entry(Source::Zoxide, "/zulu", "zulu"),
            entry(Source::Zoxide, "/alpha", "alpha"),
        ];

        app.apply_filter();

        assert_eq!(app.selected_entry().unwrap().title, "zulu");
    }

    #[test]
    fn pinned_entries_sort_first_and_persist() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        app.entries = vec![
            entry(Source::Root, "/alpha", "alpha"),
            entry(Source::Root, "/zulu", "zulu"),
        ];
        app.pinned_entries.insert(pin_key(&app.entries[1]));

        app.apply_filter();

        assert_eq!(app.selected_entry().unwrap().title, "zulu");

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("herdr-pins-{suffix}.json"));
        save_pinned_entries(&path, &app.pinned_entries).unwrap();
        assert_eq!(read_pinned_entries(&path).unwrap(), app.pinned_entries);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn previous_workspace_sorts_before_marked_entries() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        let marked = entry(Source::Root, "/marked", "marked");
        let mut previous = entry(Source::Workspace, "/previous", "previous");
        previous.workspace_id = Some("w2".into());
        previous.action = EntryAction::FocusWorkspace { id: "w2".into() };
        app.entries = vec![marked, previous];
        app.pinned_entries.insert(pin_key(&app.entries[0]));
        app.previous_workspace_id = Some("w2".into());

        app.apply_filter();

        assert_eq!(
            app.selected_entry().unwrap().workspace_id.as_deref(),
            Some("w2")
        );
    }

    #[test]
    fn source_specific_reuse_distinguishes_same_path_workspaces() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        app.path_to_workspaces.insert(
            "/tmp".into(),
            vec![
                workspace("w1", "project: tmp", WorkspaceKind::Project, "/tmp"),
                workspace("w2", "dir: tmp", WorkspaceKind::Dir, "/tmp"),
            ],
        );

        let mut project = entry(Source::Project, "/tmp", "tmp");
        project.project = Some(Project {
            name: "tmp".into(),
            description: String::new(),
            working_dir: "/tmp".into(),
            tabs: vec![],
        });
        let dir = entry(Source::Zoxide, "/tmp", "tmp");

        assert_eq!(app.matching_project_workspace(&project).unwrap().id, "w1");
        assert_eq!(app.matching_dir_workspace(&dir).unwrap().id, "w2");
    }

    #[test]
    fn offers_directory_template_for_new_and_existing_directory_workspaces() {
        let mut config = Config::default();
        config.picker.directory_template = Some("default.toml".into());
        let mut app = App::new(config, Theme::load(None, None, false));
        app.entries = vec![entry(Source::Zoxide, "/tmp", "tmp")];
        app.apply_filter();

        assert_eq!(app.directory_template_for_selected(), Some("default.toml"));

        app.path_to_workspaces.insert(
            "/tmp".into(),
            vec![workspace("w2", "dir: tmp", WorkspaceKind::Dir, "/tmp")],
        );
        assert_eq!(app.directory_template_for_selected(), Some("default.toml"));

        app.config.picker.create_missing = false;
        assert_eq!(app.directory_template_for_selected(), Some("default.toml"));

        app.path_to_workspaces.insert(
            "/tmp".into(),
            vec![workspace(
                "w1",
                "project: tmp",
                WorkspaceKind::Project,
                "/tmp",
            )],
        );
        assert_eq!(
            app.matching_template_workspace_by_key("/tmp").unwrap().id,
            "w1"
        );
    }

    #[test]
    fn close_target_matches_entry_kind() {
        let mut app = App::new(Config::default(), Theme::load(None, None, false));
        app.path_to_workspaces.insert(
            "/tmp".into(),
            vec![
                workspace("w1", "project: tmp", WorkspaceKind::Project, "/tmp"),
                workspace("w2", "dir: tmp", WorkspaceKind::Dir, "/tmp"),
            ],
        );

        let mut project = entry(Source::Project, "/tmp", "tmp");
        project.project = Some(Project {
            name: "tmp".into(),
            description: String::new(),
            working_dir: "/tmp".into(),
            tabs: vec![],
        });
        let dir = entry(Source::Root, "/tmp", "tmp");

        assert_eq!(app.workspace_to_close(&project), Some("w1".into()));
        assert_eq!(app.workspace_to_close(&dir), Some("w2".into()));
    }

    #[test]
    fn refuses_to_close_picker_owning_workspace() {
        assert_eq!(
            close_current_workspace_error("w1", Some("w1")),
            Some("can't close the workspace that owns this picker; switch away first".into())
        );
        assert_eq!(close_current_workspace_error("w1", Some("w2")), None);
        assert_eq!(close_current_workspace_error("w1", None), None);
    }

    #[test]
    fn restores_focus_to_picker_origin_after_closing_another_workspace() {
        assert_eq!(workspace_focus_to_restore("w2", Some("w1")), Some("w1"));
        assert_eq!(workspace_focus_to_restore("w1", Some("w1")), None);
        assert_eq!(workspace_focus_to_restore("w2", None), None);
    }

    #[test]
    fn workspace_rows_are_not_deduped_by_path() {
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        push_unique(
            &mut entries,
            &mut seen,
            vec![
                Entry {
                    workspace_id: Some("w1".into()),
                    action: EntryAction::FocusWorkspace { id: "w1".into() },
                    ..entry(Source::Workspace, "/tmp", "project: tmp")
                },
                Entry {
                    workspace_id: Some("w2".into()),
                    action: EntryAction::FocusWorkspace { id: "w2".into() },
                    ..entry(Source::Workspace, "/tmp", "dir: tmp")
                },
            ],
        );

        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn jump_back_records_only_real_workspace_transitions() {
        assert_eq!(
            previous_workspace_to_record(Some("w1"), Some("w2")),
            Some("w1")
        );
        assert_eq!(previous_workspace_to_record(Some("w1"), Some("w1")), None);
        assert_eq!(previous_workspace_to_record(None, Some("w2")), None);
        assert_eq!(previous_workspace_to_record(Some("w1"), None), None);
    }

    #[test]
    fn jump_back_resolves_focused_and_previous_workspaces() {
        let json = serde_json::json!({
            "result": {"workspaces": [
                {"workspace_id": "w1", "label": "one", "focused": false},
                {"workspace_id": "w2", "label": "two", "focused": true}
            ]}
        });

        assert_eq!(focused_workspace_id(&json), Some("w2"));
        assert_eq!(workspace_label(&json, "w1"), Some("one"));
        assert_eq!(workspace_label(&json, "missing"), None);
    }
}
