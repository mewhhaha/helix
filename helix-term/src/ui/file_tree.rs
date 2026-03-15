use crate::{filter_picker_entry, ui::get_excluded_types};
use helix_core::unicode::width::{UnicodeWidthChar, UnicodeWidthStr};
use helix_lsp::lsp::DiagnosticSeverity;
use helix_stdx::path::normalize;
use helix_vcs::{DiffProviderRegistry, FileChange};
use helix_view::{
    graphics::{Color, Modifier, Rect, Style, UnderlineStyle},
    input::KeyEvent,
    keyboard::KeyCode,
    Editor,
};
use ignore::WalkBuilder;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tui::{
    buffer::Buffer as Surface,
    text::{Span, Spans},
};

const DEFAULT_SIDEBAR_WIDTH: u16 = 32;
const MIN_SIDEBAR_WIDTH: u16 = 18;
const MIN_EDITOR_WIDTH: u16 = 20;
const DOUBLE_CLICK_THRESHOLD: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileTreeOptions {
    pub hidden: bool,
    pub follow_symlinks: bool,
    pub parents: bool,
    pub ignore: bool,
    pub git_ignore: bool,
    pub git_global: bool,
    pub git_exclude: bool,
    pub dedup_symlinks: bool,
}

impl FileTreeOptions {
    pub(crate) fn from_editor(editor: &Editor) -> Self {
        let config = editor.config();
        Self {
            hidden: config.file_explorer.hidden,
            follow_symlinks: config.file_explorer.follow_symlinks,
            parents: config.file_explorer.parents,
            ignore: config.file_explorer.ignore,
            git_ignore: config.file_explorer.git_ignore,
            git_global: config.file_explorer.git_global,
            git_exclude: config.file_explorer.git_exclude,
            dedup_symlinks: config.file_picker.deduplicate_links,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileChangeKind {
    Untracked,
    Modified,
    Conflict,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DiagnosticCounts {
    warnings: u32,
    errors: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileTreeEntry {
    path: PathBuf,
    depth: usize,
    kind: EntryKind,
    expanded: bool,
    prefix: String,
    name: String,
}

pub(crate) enum FileTreeEvent {
    Consumed,
    Ignored,
    Close,
    Open(PathBuf),
}

#[derive(Debug)]
pub(crate) struct FileTreeSidebar {
    root: PathBuf,
    options: FileTreeOptions,
    expanded_dirs: BTreeSet<PathBuf>,
    entries: Vec<FileTreeEntry>,
    selected: usize,
    scroll: usize,
    width: u16,
    git_statuses: Arc<Mutex<BTreeMap<PathBuf, FileChangeKind>>>,
    cached_diagnostics: BTreeMap<PathBuf, DiagnosticCounts>,
    diagnostics_generation: u64,
    last_click: Option<(PathBuf, Instant)>,
}

impl FileTreeSidebar {
    pub(crate) fn new(root: PathBuf, options: FileTreeOptions) -> io::Result<Self> {
        let root = normalize(root);
        if !root.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} does not exist", root.display()),
            ));
        }
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a directory", root.display()),
            ));
        }

        let mut expanded_dirs = BTreeSet::new();
        expanded_dirs.insert(root.clone());

        let mut sidebar = Self {
            root,
            options,
            expanded_dirs,
            entries: Vec::new(),
            selected: 0,
            scroll: 0,
            width: DEFAULT_SIDEBAR_WIDTH,
            git_statuses: Arc::new(Mutex::new(BTreeMap::new())),
            cached_diagnostics: BTreeMap::new(),
            diagnostics_generation: 0,
            last_click: None,
        };
        sidebar.rebuild();
        Ok(sidebar)
    }

    pub(crate) fn clamp_width(area: Rect, requested: u16) -> Option<u16> {
        let max_width = area.width.saturating_sub(MIN_EDITOR_WIDTH);
        if max_width < MIN_SIDEBAR_WIDTH {
            return None;
        }
        Some(requested.clamp(MIN_SIDEBAR_WIDTH, max_width))
    }

    pub(crate) fn width_for(&self, area: Rect) -> Option<u16> {
        Self::clamp_width(area, self.width)
    }

    pub(crate) fn set_width_from_column(&mut self, area: Rect, column: u16) {
        let desired = column.saturating_sub(area.x).saturating_add(1);
        if let Some(width) = Self::clamp_width(area, desired) {
            self.width = width;
        }
    }

    pub(crate) fn refresh_git_statuses(&mut self, diff_providers: &DiffProviderRegistry) {
        {
            let mut statuses = lock(&self.git_statuses);
            statuses.clear();
        }

        let statuses = Arc::clone(&self.git_statuses);
        let root = self.root.clone();
        diff_providers
            .clone()
            .for_each_changed_file(root, move |result| {
                let Ok(change) = result else {
                    return false;
                };

                let kind = match change {
                    FileChange::Untracked { .. } => FileChangeKind::Untracked,
                    FileChange::Modified { .. } => FileChangeKind::Modified,
                    FileChange::Conflict { .. } => FileChangeKind::Conflict,
                    FileChange::Deleted { .. } => FileChangeKind::Deleted,
                    FileChange::Renamed { .. } => FileChangeKind::Renamed,
                };

                lock(&statuses).insert(normalize(change.path().to_path_buf()), kind);
                true
            });
    }

    pub(crate) fn reveal_path(&mut self, path: &Path) {
        let path = normalize(path);
        if !path.starts_with(&self.root) {
            return;
        }

        let mut current = path.as_path();
        while let Some(parent) = current.parent() {
            if !parent.starts_with(&self.root) {
                break;
            }
            self.expanded_dirs.insert(parent.to_path_buf());
            if parent == self.root {
                break;
            }
            current = parent;
        }

        self.rebuild();
        if let Some(index) = self.entries.iter().position(|entry| entry.path == path) {
            self.selected = index;
        }
    }

    pub(crate) fn click_at_row(&mut self, area: Rect, row: u16) -> FileTreeEvent {
        let Some(index) = self.entry_index_at_row(area, row) else {
            return FileTreeEvent::Consumed;
        };

        self.selected = index;
        let Some(entry) = self.entries.get(index) else {
            return FileTreeEvent::Consumed;
        };

        let now = Instant::now();
        let is_double_click = self.last_click.as_ref().is_some_and(|(path, instant)| {
            path == &entry.path && now.duration_since(*instant) <= DOUBLE_CLICK_THRESHOLD
        });

        self.last_click = Some((entry.path.clone(), now));

        if is_double_click {
            self.activate_selected()
        } else {
            FileTreeEvent::Consumed
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> FileTreeEvent {
        match key.code {
            KeyCode::Esc => FileTreeEvent::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                FileTreeEvent::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                FileTreeEvent::Consumed
            }
            KeyCode::Home => {
                self.selected = 0;
                FileTreeEvent::Consumed
            }
            KeyCode::End => {
                self.selected = self.entries.len().saturating_sub(1);
                FileTreeEvent::Consumed
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.collapse_or_select_parent();
                FileTreeEvent::Consumed
            }
            KeyCode::Right | KeyCode::Char('l') => self.expand_or_descend(),
            KeyCode::Enter => self.activate_selected(),
            _ => FileTreeEvent::Ignored,
        }
    }

    pub(crate) fn render(
        &mut self,
        area: Rect,
        surface: &mut Surface,
        editor: &Editor,
        focused: bool,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let background = editor.theme.get("ui.background");
        surface.clear_with(area, background);

        let divider_x = area.right().saturating_sub(1);
        let divider_style = editor.theme.get("ui.window");
        for y in area.top()..area.bottom() {
            surface[(divider_x, y)]
                .set_symbol(tui::symbols::line::VERTICAL)
                .set_style(divider_style);
        }

        let content_area = area.clip_right(1);
        if content_area.width == 0 {
            return;
        }

        let title_style = editor
            .theme
            .get("ui.text.directory")
            .add_modifier(Modifier::BOLD);
        let directory_style = editor.theme.get("ui.text.directory");
        let file_style = editor.theme.get("ui.text");
        let selected_style = if focused {
            editor
                .theme
                .try_get("ui.menu.selected")
                .or_else(|| editor.theme.try_get("ui.selection"))
                .unwrap_or(file_style)
        } else {
            file_style.add_modifier(Modifier::BOLD)
        };
        let added_style = editor
            .theme
            .try_get("diff.plus")
            .unwrap_or_else(|| editor.theme.get("hint"));
        let modified_style = editor
            .theme
            .try_get("diff.delta")
            .unwrap_or_else(|| editor.theme.get("warning"));
        let removed_style = editor
            .theme
            .try_get("diff.minus")
            .unwrap_or_else(|| editor.theme.get("error"));
        let warning_style = editor.theme.get("warning");
        let error_style = editor.theme.get("error");
        self.refresh_diagnostics(editor);
        let active_path = current_ref!(editor).1.path().map(normalize);

        let title = format!("󰉋 {}", self.root_label());
        surface.set_stringn(
            content_area.x,
            content_area.y,
            &title,
            content_area.width as usize,
            title_style,
        );

        let visible_rows = content_area.height.saturating_sub(1) as usize;
        self.ensure_selected_visible(visible_rows);
        let statuses = lock(&self.git_statuses);

        for (row, entry) in self
            .entries
            .iter()
            .skip(self.scroll)
            .take(visible_rows)
            .enumerate()
        {
            let y = content_area.y + 1 + row as u16;
            let is_selected = self.selected == self.scroll + row;
            let is_active = active_path.as_ref().is_some_and(|path| path == &entry.path);
            let change = statuses.get(&entry.path).copied();
            let counts = self
                .cached_diagnostics
                .get(&entry.path)
                .copied()
                .unwrap_or_default();
            let spans = self.entry_spans(
                entry,
                content_area.width as usize,
                directory_style,
                file_style,
                selected_style,
                added_style,
                modified_style,
                removed_style,
                warning_style,
                error_style,
                change,
                counts,
                is_selected,
                is_active,
            );
            surface.set_spans(content_area.x, y, &spans, content_area.width);
        }
    }

    fn rebuild(&mut self) {
        self.entries.clear();
        for (child_path, child_kind) in self.read_children(&self.root) {
            match child_kind {
                EntryKind::Directory => self.collect_directory(child_path, 0),
                EntryKind::File => {
                    self.entries
                        .push(FileTreeEntry::new(child_path, 0, EntryKind::File, false))
                }
            }
        }
        if self.selected >= self.entries.len() {
            self.selected = self.entries.len().saturating_sub(1);
        }
    }

    fn collect_directory(&mut self, path: PathBuf, depth: usize) {
        let expanded = self.expanded_dirs.contains(&path);
        self.entries.push(FileTreeEntry::new(
            path.clone(),
            depth,
            EntryKind::Directory,
            expanded,
        ));

        if !expanded {
            return;
        }

        for (child_path, child_kind) in self.read_children(&path) {
            match child_kind {
                EntryKind::Directory => self.collect_directory(child_path, depth + 1),
                EntryKind::File => self.entries.push(FileTreeEntry::new(
                    child_path,
                    depth + 1,
                    EntryKind::File,
                    false,
                )),
            }
        }
    }

    fn read_children(&self, dir: &Path) -> Vec<(PathBuf, EntryKind)> {
        let absolute_root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let dedup_symlinks = self.options.dedup_symlinks;
        let mut walk_builder = WalkBuilder::new(dir);

        let mut content: Vec<_> = walk_builder
            .hidden(self.options.hidden)
            .parents(self.options.parents)
            .ignore(self.options.ignore)
            .follow_links(self.options.follow_symlinks)
            .git_ignore(self.options.git_ignore)
            .git_global(self.options.git_global)
            .git_exclude(self.options.git_exclude)
            .max_depth(Some(1))
            .filter_entry(move |entry| filter_picker_entry(entry, &absolute_root, dedup_symlinks))
            .add_custom_ignore_filename(helix_loader::config_dir().join("ignore"))
            .add_custom_ignore_filename(".helix/ignore")
            .types(get_excluded_types())
            .build()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let file_type = entry.file_type()?;
                if entry.path() == dir {
                    return None;
                }
                let kind = if file_type.is_dir() {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                };
                Some((normalize(entry.into_path()), kind))
            })
            .collect();

        content.sort_by(|(left_path, left_kind), (right_path, right_kind)| {
            let left_rank = matches!(left_kind, EntryKind::File);
            let right_rank = matches!(right_kind, EntryKind::File);
            left_rank
                .cmp(&right_rank)
                .then_with(|| left_path.cmp(right_path))
        });

        content
    }

    #[allow(clippy::too_many_arguments)]
    fn entry_spans(
        &self,
        entry: &FileTreeEntry,
        max_width: usize,
        directory_style: Style,
        file_style: Style,
        selected_style: Style,
        added_style: Style,
        modified_style: Style,
        removed_style: Style,
        warning_style: Style,
        error_style: Style,
        change: Option<FileChangeKind>,
        counts: DiagnosticCounts,
        is_selected: bool,
        is_active: bool,
    ) -> Spans<'static> {
        let mut prefix_style = match entry.kind {
            EntryKind::Directory => directory_style,
            EntryKind::File => file_style,
        };
        let mut name_style = prefix_style;

        if let Some(change) = change {
            name_style = name_style.patch(change_style(
                change,
                added_style,
                modified_style,
                removed_style,
                error_style,
            ));
        }
        if let Some(underline_style) =
            diagnostic_underline_style(counts, warning_style, error_style)
        {
            name_style = name_style.patch(underline_style);
        }

        if is_active {
            prefix_style = prefix_style.add_modifier(Modifier::BOLD);
            name_style = name_style.add_modifier(Modifier::BOLD);
        }
        if is_selected {
            prefix_style = selected_style.patch(prefix_style);
            name_style = selected_style.patch(name_style);
        }

        let mut suffix = Vec::new();
        if counts.warnings > 0 {
            let style = if is_selected {
                selected_style.patch(warning_style)
            } else {
                warning_style
            };
            suffix.push(Span::styled(format!(" {}", counts.warnings), style));
        }
        if counts.errors > 0 {
            let style = if is_selected {
                selected_style.patch(error_style)
            } else {
                error_style
            };
            suffix.push(Span::styled(format!(" {}", counts.errors), style));
        }

        let suffix_width: usize = suffix.iter().map(|span| span.content.width()).sum();
        let label_width = max_width.saturating_sub(suffix_width);
        let prefix_width = entry.prefix.width();
        let mut spans = Vec::new();

        if label_width <= prefix_width {
            let label = truncate_to_width(&format!("{}{}", entry.prefix, entry.name), label_width);
            spans.push(Span::styled(label, prefix_style));
        } else {
            let name_width = label_width.saturating_sub(prefix_width);
            spans.push(Span::styled(entry.prefix.clone(), prefix_style));
            spans.push(Span::styled(
                truncate_to_width(&entry.name, name_width),
                name_style,
            ));
        }
        spans.extend(suffix);
        Spans::from(spans)
    }

    fn entry_index_at_row(&self, area: Rect, row: u16) -> Option<usize> {
        if row <= area.y {
            return None;
        }

        let visible_index = self.scroll + row.saturating_sub(area.y + 1) as usize;
        (visible_index < self.entries.len()).then_some(visible_index)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }

        if delta.is_negative() {
            self.selected = self.selected.saturating_sub(delta.unsigned_abs());
        } else {
            self.selected = (self.selected + delta as usize).min(self.entries.len() - 1);
        }
    }

    fn collapse_or_select_parent(&mut self) {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return;
        };

        if entry.kind == EntryKind::Directory && entry.expanded {
            self.expanded_dirs.remove(&entry.path);
            self.rebuild();
            if let Some(index) = self
                .entries
                .iter()
                .position(|candidate| candidate.path == entry.path)
            {
                self.selected = index;
            }
            return;
        }

        if let Some(parent) = self.parent_index(self.selected) {
            self.selected = parent;
        }
    }

    fn expand_or_descend(&mut self) -> FileTreeEvent {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return FileTreeEvent::Consumed;
        };

        match entry.kind {
            EntryKind::File => FileTreeEvent::Open(entry.path),
            EntryKind::Directory if !entry.expanded => {
                self.expanded_dirs.insert(entry.path.clone());
                self.rebuild();
                if let Some(index) = self
                    .entries
                    .iter()
                    .position(|candidate| candidate.path == entry.path)
                {
                    self.selected = index;
                }
                FileTreeEvent::Consumed
            }
            EntryKind::Directory => {
                if let Some(child) = self.first_child_index(self.selected) {
                    self.selected = child;
                }
                FileTreeEvent::Consumed
            }
        }
    }

    fn activate_selected(&mut self) -> FileTreeEvent {
        let Some(entry) = self.entries.get(self.selected).cloned() else {
            return FileTreeEvent::Consumed;
        };

        match entry.kind {
            EntryKind::File => FileTreeEvent::Open(entry.path),
            EntryKind::Directory => {
                if entry.expanded {
                    self.expanded_dirs.remove(&entry.path);
                } else {
                    self.expanded_dirs.insert(entry.path.clone());
                }
                self.rebuild();
                if let Some(index) = self
                    .entries
                    .iter()
                    .position(|candidate| candidate.path == entry.path)
                {
                    self.selected = index;
                }
                FileTreeEvent::Consumed
            }
        }
    }

    fn parent_index(&self, index: usize) -> Option<usize> {
        let depth = self.entries.get(index)?.depth;
        if depth == 0 {
            return None;
        }

        (0..index)
            .rev()
            .find(|candidate| self.entries[*candidate].depth < depth)
    }

    fn first_child_index(&self, index: usize) -> Option<usize> {
        let depth = self.entries.get(index)?.depth;
        let child = index + 1;
        self.entries
            .get(child)
            .filter(|entry| entry.depth > depth)
            .map(|_| child)
    }

    fn ensure_selected_visible(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            self.scroll = self.selected;
            return;
        }

        if self.selected < self.scroll {
            self.scroll = self.selected;
        }

        let last_visible = self.scroll.saturating_add(visible_rows);
        if self.selected >= last_visible {
            self.scroll = self.selected + 1 - visible_rows;
        }
    }

    fn refresh_diagnostics(&mut self, editor: &Editor) {
        let diagnostics_generation = editor.diagnostics_generation();
        if self.diagnostics_generation == diagnostics_generation {
            return;
        }
        self.cached_diagnostics = diagnostic_counts_by_path(editor);
        self.diagnostics_generation = diagnostics_generation;
    }

    fn root_label(&self) -> String {
        file_name(&self.root)
    }
}

impl FileTreeEntry {
    fn new(path: PathBuf, depth: usize, kind: EntryKind, expanded: bool) -> Self {
        let (prefix, name) = entry_label_parts(&path, depth, kind, expanded);
        Self {
            path,
            depth,
            kind,
            expanded,
            prefix,
            name,
        }
    }
}

fn entry_label_parts(
    path: &Path,
    depth: usize,
    kind: EntryKind,
    expanded: bool,
) -> (String, String) {
    let disclosure = match kind {
        EntryKind::Directory if expanded => "▾",
        EntryKind::Directory => "▸",
        EntryKind::File => " ",
    };
    let icon = match kind {
        EntryKind::Directory if expanded => "󰷏",
        EntryKind::Directory => "󰉋",
        EntryKind::File => file_icon(path),
    };
    let indent = "  ".repeat(depth);
    let name = file_name(path);
    (format!("{indent}{disclosure} {icon} "), name)
}

fn diagnostic_counts_by_path(editor: &Editor) -> BTreeMap<PathBuf, DiagnosticCounts> {
    let mut counts: BTreeMap<PathBuf, DiagnosticCounts> = BTreeMap::new();

    for (uri, diagnostics) in &editor.diagnostics {
        let Some(path) = uri.as_path() else {
            continue;
        };
        let entry = counts.entry(normalize(path)).or_default();
        for (diag, _) in diagnostics {
            match diag.severity {
                Some(DiagnosticSeverity::WARNING) => entry.warnings += 1,
                Some(DiagnosticSeverity::ERROR) => entry.errors += 1,
                _ => {}
            }
        }
    }

    counts
}

fn change_style(
    change: FileChangeKind,
    added_style: Style,
    modified_style: Style,
    removed_style: Style,
    error_style: Style,
) -> Style {
    match change {
        FileChangeKind::Untracked => added_style,
        FileChangeKind::Modified => modified_style,
        FileChangeKind::Conflict => error_style,
        FileChangeKind::Deleted => removed_style,
        FileChangeKind::Renamed => modified_style.add_modifier(Modifier::ITALIC),
    }
}

fn diagnostic_underline_style(
    counts: DiagnosticCounts,
    warning_style: Style,
    error_style: Style,
) -> Option<Style> {
    let underline_color = if counts.errors > 0 {
        error_style.fg.unwrap_or(Color::Red)
    } else if counts.warnings > 0 {
        warning_style.fg.unwrap_or(Color::Yellow)
    } else {
        return None;
    };

    Some(
        Style::default()
            .underline_color(underline_color)
            .underline_style(UnderlineStyle::Dotted),
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn truncate_to_width(text: &str, width: usize) -> String {
    if width == 0 || text.is_empty() {
        return String::new();
    }
    if text.width() <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }

    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width >= width {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

fn file_icon(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => "",
        Some("md") => "󰍔",
        Some("toml") => "",
        Some("json") | Some("jsonc") => "",
        Some("yml") | Some("yaml") => "",
        Some("js") | Some("mjs") | Some("cjs") => "",
        Some("ts") | Some("tsx") => "󰛦",
        Some("jsx") => "",
        Some("html") => "",
        Some("css") | Some("scss") => "",
        Some("sh") | Some("bash") | Some("zsh") => "",
        Some("lock") => "󰌾",
        _ => "󰈔",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DiagnosticCounts, FileChangeKind, FileTreeOptions, FileTreeSidebar, MIN_SIDEBAR_WIDTH,
    };
    use helix_view::{
        graphics::{Color, Rect, Style, UnderlineStyle},
        input::KeyEvent,
        keyboard::{KeyCode, KeyModifiers},
    };
    use std::{fs, path::Path};
    use tempfile::tempdir;

    fn options() -> FileTreeOptions {
        FileTreeOptions {
            hidden: false,
            follow_symlinks: false,
            parents: false,
            ignore: false,
            git_ignore: false,
            git_global: false,
            git_exclude: false,
            dedup_symlinks: false,
        }
    }

    fn setup_tree(root: &Path) {
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("src/nested")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/nested/lib.rs"), "pub fn demo() {}\n").unwrap();
    }

    #[test]
    fn initializes_with_first_level_entries_only() {
        let dir = tempdir().unwrap();
        setup_tree(dir.path());

        let sidebar = FileTreeSidebar::new(dir.path().to_path_buf(), options()).unwrap();

        assert_ne!(sidebar.entries[0].path, dir.path());
        assert_eq!(sidebar.entries[0].depth, 0);
        assert!(sidebar
            .entries
            .iter()
            .any(|entry| entry.path.ends_with("src")));
        assert!(sidebar
            .entries
            .iter()
            .any(|entry| entry.path.ends_with("Cargo.toml")));
    }

    #[test]
    fn reveal_path_expands_ancestors_and_selects_entry() {
        let dir = tempdir().unwrap();
        setup_tree(dir.path());
        let target = dir.path().join("src/nested/lib.rs");

        let mut sidebar = FileTreeSidebar::new(dir.path().to_path_buf(), options()).unwrap();
        sidebar.reveal_path(&target);

        let selected = &sidebar.entries[sidebar.selected];
        assert_eq!(selected.path, target);
        assert!(sidebar
            .entries
            .iter()
            .any(|entry| entry.path.ends_with("src/nested")));
    }

    #[test]
    fn left_key_collapses_directory_and_moves_to_parent() {
        let dir = tempdir().unwrap();
        setup_tree(dir.path());
        let nested_dir = dir.path().join("src/nested");

        let mut sidebar = FileTreeSidebar::new(dir.path().to_path_buf(), options()).unwrap();
        sidebar.reveal_path(&nested_dir);
        sidebar.handle_key(KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::NONE,
        });

        let selected = &sidebar.entries[sidebar.selected];
        assert!(selected.path.ends_with("src"));
    }

    #[test]
    fn unhandled_keys_are_ignored() {
        let dir = tempdir().unwrap();
        setup_tree(dir.path());

        let mut sidebar = FileTreeSidebar::new(dir.path().to_path_buf(), options()).unwrap();
        assert!(matches!(
            sidebar.handle_key(KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::SUPER,
            }),
            super::FileTreeEvent::Ignored
        ));
    }

    #[test]
    fn double_click_opens_file() {
        let dir = tempdir().unwrap();
        setup_tree(dir.path());
        let file = dir.path().join("Cargo.toml");

        let mut sidebar = FileTreeSidebar::new(dir.path().to_path_buf(), options()).unwrap();
        let area = Rect::new(0, 0, 32, 20);
        let row = sidebar
            .entries
            .iter()
            .position(|entry| entry.path == file)
            .map(|index| area.y + 1 + index as u16)
            .unwrap();

        assert!(matches!(
            sidebar.click_at_row(area, row),
            super::FileTreeEvent::Consumed
        ));
        assert!(
            matches!(sidebar.click_at_row(area, row), super::FileTreeEvent::Open(path) if path == file)
        );
    }

    #[test]
    fn clamp_width_honors_minimums() {
        let area = Rect::new(0, 0, 60, 20);
        assert_eq!(
            FileTreeSidebar::clamp_width(area, 5),
            Some(MIN_SIDEBAR_WIDTH)
        );
        assert_eq!(
            FileTreeSidebar::clamp_width(Rect::new(0, 0, 30, 20), 20),
            None
        );
    }

    #[test]
    fn entry_spans_only_style_filename_and_use_numeric_diagnostics() {
        let dir = tempdir().unwrap();
        setup_tree(dir.path());
        let sidebar = FileTreeSidebar::new(dir.path().to_path_buf(), options()).unwrap();
        let entry = sidebar
            .entries
            .iter()
            .find(|entry| entry.path.ends_with("Cargo.toml"))
            .unwrap();

        let spans = sidebar.entry_spans(
            entry,
            80,
            Style::default().fg(Color::Blue),
            Style::default().fg(Color::White),
            Style::default().bg(Color::Gray),
            Style::default().fg(Color::Green),
            Style::default().fg(Color::Yellow),
            Style::default().fg(Color::Red),
            Style::default().fg(Color::Yellow),
            Style::default().fg(Color::Red),
            Some(FileChangeKind::Modified),
            DiagnosticCounts {
                warnings: 2,
                errors: 1,
            },
            false,
            false,
        );

        assert_eq!(spans.0.len(), 4);
        assert_eq!(spans.0[0].style.fg, Some(Color::White));
        assert_eq!(spans.0[0].style.underline_style, None);
        assert_eq!(spans.0[1].content.as_ref(), "Cargo.toml");
        assert_eq!(spans.0[1].style.fg, Some(Color::Yellow));
        assert_eq!(
            spans.0[1].style.underline_style,
            Some(UnderlineStyle::Dotted)
        );
        assert_eq!(spans.0[1].style.underline_color, Some(Color::Red));
        assert_eq!(spans.0[2].content.as_ref(), " 2");
        assert_eq!(spans.0[2].style.fg, Some(Color::Yellow));
        assert_eq!(spans.0[3].content.as_ref(), " 1");
        assert_eq!(spans.0[3].style.fg, Some(Color::Red));
    }
}
