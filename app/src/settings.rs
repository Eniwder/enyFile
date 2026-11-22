//! The settings dialog, colors and font, bookmarks, and keeping the config on disk.
use super::*;
use crate::pathinput::Draft;

impl Controller {
    /// Settings dialog (File > 設定): edits a copy of the places until saved.
    pub(super) fn open_settings(&mut self) {
        let [desktop, documents, pictures, videos, music] = self.config.places.clone();
        let [left, right] = self.config.start.clone();
        self.settings =
            [desktop, documents, pictures, videos, music, left, right].map(Draft::Saved);
        self.settings_rows();
        self.settings_colors = self.config.colors.clone();
        self.settings_ext = self.config.extension_colors.clone();
        self.settings_font = (self.config.font_family.clone(), self.config.font_size);
        self.settings_color_rows();
        if let Some(ui) = self.ui.upgrade() {
            self.font_filter("");
            ui.set_settings_font_family(self.settings_font.0.clone().into());
            ui.set_settings_font_size(self.settings_font.1.round() as i32);
            ui.set_settings_opacity((self.config.opacity * 100.0).round() as i32);
            ui.set_settings_intra_pane_move(self.config.intra_pane_move);
            ui.set_settings_pane_drag_drop(self.config.pane_drag_drop);
            ui.set_settings_search_links(self.config.search_links);
            ui.set_settings_agent_cli(if self.config.agent_enabled { 0 } else { 1 });
            ui.set_settings_agent_path(
                self.config
                    .agent_path
                    .as_deref()
                    .map_or_else(String::new, pathinput::display)
                    .into(),
            );
            let access = agent::Access::ALL
                .iter()
                .position(|a| *a == self.config.agent_access)
                .unwrap_or(0);
            ui.set_settings_agent_access(access as i32);
            ui.set_settings_agent_panes(self.config.agent_panes);
            ui.set_settings_agent_memory(pathinput::display(&self.config.agent_memory()).into());
            ui.set_settings_agent_detected("空欄: 自動で検出".into());
        }
        self.settings_agent_folders = self.config.agent_folders.clone();
        self.settings_agent_rows();
        // Turned off: not even looked for.
        if self.config.agent_enabled {
            // Looking along PATH can touch slow drives: off the UI thread.
            std::thread::spawn(|| {
                post(Reply::AgentFound(match agent::locate(None) {
                    Ok(program) => format!("空欄: 自動で検出（{}）", pathinput::display(&program)),
                    Err(_) => {
                        "空欄: 自動で検出（今は見つかりません。codex.exe の場所を入力できます）"
                            .into()
                    }
                }));
            });
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_settings_open(true);
            ui.invoke_focus_settings();
        }
        self.apply_opacity();
    }
    /// Rebuilds the rows, so edited fields show new values (browse / reset).
    pub(super) fn settings_rows(&self) {
        let labels = [
            "デスクトップ",
            "ドキュメント",
            "画像",
            "動画",
            "音楽",
            "起動時の左",
            "起動時の右",
        ];
        let sandbox = sandbox_dir();
        let rows: Vec<PlaceRow> = (0..7)
            .map(|i| PlaceRow {
                label: labels[i].into(),
                value: self.settings[i].text().into(),
                fallback: match (i, &sandbox) {
                    (5, Some(d)) => pathinput::display(&d.join("left")),
                    (6, Some(d)) => pathinput::display(&d.join("right")),
                    // Released builds start at the Desktop / Documents (see `default_start`).
                    (5 | 6, None) => config::known_folder(i - 5)
                        .as_deref()
                        .map_or_else(String::new, pathinput::display),
                    _ => config::known_folder(i)
                        .as_deref()
                        .map_or_else(String::new, pathinput::display),
                }
                .into(),
            })
            .collect();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_settings_places(ModelRc::new(slint::VecModel::from(rows)));
        }
    }
    pub(super) fn setting_edited(&mut self, i: usize, text: &str) {
        self.settings[i] = Draft::Edited(text.to_string());
    }
    pub(super) fn setting_reset(&mut self, i: usize) {
        self.settings[i] = Draft::default();
        self.settings_rows();
    }
    /// The Windows folder picker runs on its own thread so the window keeps responding.
    pub(super) fn setting_browse(&mut self, i: usize) {
        let start = match self.settings[i].path() {
            Ok(path) => path.or_else(|| if i < 5 { config::known_folder(i) } else { None }),
            Err(error) => return self.fail(error),
        };
        std::thread::spawn(move || {
            if let Some(path) = shell::pick_folder(start.as_deref()) {
                post(Reply::Picked(i, path));
            }
        });
    }
    pub(super) fn font_filter(&self, query: &str) {
        if let Some(ui) = self.ui.upgrade() {
            let fonts: Vec<slint::SharedString> =
                theme::filter_fonts(&theme::font_families(), query)
                    .into_iter()
                    .map(Into::into)
                    .collect();
            ui.set_fonts(ModelRc::new(slint::VecModel::from(fonts)));
        }
    }

    /// Copies colors, extension colors and font from the config into the UI.
    pub(super) fn apply_appearance(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let theme = ui.global::<Theme>();
        for ((key, _, _), color) in theme::BASE.iter().zip(&self.config.colors) {
            let color = *color;
            match *key {
                "pane" => theme.set_pane(color),
                "text" => theme.set_text(color),
                "folder-text" => theme.set_folder_text(color),
                "drive-text" => theme.set_drive_text(color),
                "readonly-text" => theme.set_readonly_text(color),
                "hidden-text" => theme.set_hidden_text(color),
                "size-text" => theme.set_size_text(color),
                "time-text" => theme.set_time_text(color),
                "attr-text" => theme.set_attr_text(color),
                "diff-add-bg" => theme.set_diff_add_bg(color),
                "diff-del-bg" => theme.set_diff_del_bg(color),
                "diff-change-bg" => theme.set_diff_change_bg(color),
                "selected-text" => theme.set_selected_text(color),
                "cursor-text" => theme.set_cursor_text(color),
                "cursor-selected-bg" => theme.set_cursor_selected_bg(color),
                "cursor-inactive-bg" => theme.set_cursor_inactive_bg(color),
                "cursor-inactive-line" => theme.set_cursor_inactive_line(color),
                "dim" => theme.set_dim(color),
                "border" => theme.set_border(color),
                "dialog" => theme.set_dialog(color),
                "kind" => theme.set_kind(color),
                "selected-bg" => theme.set_selected_bg(color),
                "cursor-bg" => theme.set_cursor_bg(color),
                "accent" => theme.set_accent(color),
                "chrome" => theme.set_chrome(color),
                "bar" => theme.set_bar(color),
                _ => {}
            }
        }
        ui.set_font_family(self.config.font_family.clone().into());
        ui.set_font_size(self.config.font_size);
        let map = Arc::new(theme::extension_map(&self.config.extension_colors));
        for pane in &self.panes {
            *pane.model.ext_colors.borrow_mut() = map.clone();
            pane.model.notify.reset();
        }
    }
    pub(super) fn settings_color_rows(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let base: Vec<ColorRow> = theme::BASE
            .iter()
            .zip(&self.settings_colors)
            .map(|((_, label, _), color)| ColorRow {
                label: (*label).into(),
                color: *color,
                r: color.red() as i32,
                g: color.green() as i32,
                b: color.blue() as i32,
            })
            .collect();
        let ext: Vec<ExtRow> = self
            .settings_ext
            .iter()
            .map(|(exts, color)| ExtRow {
                exts: exts.clone().into(),
                color: *color,
                r: color.red() as i32,
                g: color.green() as i32,
                b: color.blue() as i32,
            })
            .collect();
        ui.set_settings_colors(ModelRc::new(slint::VecModel::from(base)));
        ui.set_settings_ext(ModelRc::new(slint::VecModel::from(ext)));
    }
    pub(super) fn color_base(&mut self, i: usize, color: slint::Color) {
        if let Some(c) = self.settings_colors.get_mut(i) {
            *c = color;
        }
        self.settings_color_rows();
    }
    /// `i` out of range adds a rule.
    pub(super) fn color_ext(&mut self, i: usize, exts: &str, color: slint::Color) {
        let exts = theme::split_extensions(exts)
            .iter()
            .map(|e| e.to_uppercase())
            .collect::<Vec<_>>()
            .join(";");
        if exts.is_empty() {
            self.message("拡張子を入力してください");
            return;
        }
        match self.settings_ext.get_mut(i) {
            Some(rule) => *rule = (exts, color),
            None => self.settings_ext.push((exts, color)),
        }
        self.settings_color_rows();
    }
    pub(super) fn color_ext_delete(&mut self, i: usize) {
        if i < self.settings_ext.len() {
            self.settings_ext.remove(i);
        }
        self.settings_color_rows();
    }
    pub(super) fn colors_reset(&mut self) {
        self.settings_colors = config::Config::default_colors();
        self.settings_ext = config::Config::default_extension_colors();
        self.settings_color_rows();
    }
    pub(super) fn font_set(&mut self, family: &str, size: f32) {
        if !family.trim().is_empty() {
            self.settings_font = (family.trim().to_string(), size.clamp(8.0, 32.0));
        }
    }
    pub(super) fn settings_done(&mut self, save: bool) {
        if save {
            let mut paths = [const { None }; 7];
            for (path, draft) in paths.iter_mut().zip(&self.settings) {
                *path = match draft.path() {
                    Ok(path) => path,
                    Err(error) => return self.fail(error),
                };
            }
            let ui = self.ui.upgrade();
            let program = if let Some(ui) = &ui {
                let text = ui.get_settings_agent_path();
                let saved = self.config.agent_path.as_deref();
                if saved.is_some_and(|p| text.as_str() == pathinput::display(p)) {
                    self.config.agent_path.clone()
                } else {
                    match pathinput::literal(&text) {
                        Ok(path) => path,
                        Err(error) => return self.fail(error),
                    }
                }
            } else {
                self.config.agent_path.clone()
            };
            let [desktop, documents, pictures, videos, music, left, right] = paths;
            self.config.places = [desktop, documents, pictures, videos, music];
            self.config.start = [left, right];
            if let Some(ui) = ui {
                self.config.intra_pane_move = ui.get_settings_intra_pane_move();
                self.config.pane_drag_drop = ui.get_settings_pane_drag_drop();
                self.config.opacity = ui.get_settings_opacity() as f32 / 100.0;
                let follow = ui.get_settings_search_links();
                if follow != self.config.search_links {
                    self.config.search_links = follow;
                    // The next search walks again; results on screen keep theirs until F5.
                    for pane in &mut self.panes {
                        if pane.found.is_none() {
                            pane.finder = None;
                        }
                    }
                }
                if program != self.config.agent_path {
                    self.config.agent_path = program;
                    self.agent.version = None;
                    self.agent.version_asked = false;
                    if ui.get_agent_open() {
                        self.agent_version();
                    }
                }
                let access = ui.get_settings_agent_access().clamp(0, 2) as usize;
                self.config.agent_access = agent::Access::ALL[access];
                self.config.agent_folders = self.settings_agent_folders.clone();
                self.config.agent_panes = ui.get_settings_agent_panes();
                self.config.agent_enabled = ui.get_settings_agent_cli() == 0;
                ui.set_agent_enabled(self.config.agent_enabled);
                if !self.config.agent_enabled {
                    // Off means off now: no run goes on behind a closed panel.
                    self.agent_cancel();
                    if ui.get_agent_open() {
                        self.close_agent();
                    }
                }
                self.sync_agent();
            }
            self.config.colors = self.settings_colors.clone();
            self.config.extension_colors = self.settings_ext.clone();
            (self.config.font_family, self.config.font_size) = self.settings_font.clone();
            self.apply_appearance();
            self.save_config();
            self.message("設定を保存しました");
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_settings_open(false);
            ui.invoke_focus_pane(self.active as i32);
        }
        self.apply_opacity();
    }
    /// A splitter was released: keep every layout's positions.
    pub(super) fn layout_committed(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        self.config.split = ui.get_split().clamp(0.15, 0.85);
        self.config.split3_columns = [ui.get_col_a(), ui.get_col_b()];
        self.config.split3_rows = [ui.get_row_a(), ui.get_row_b()];
        self.config.split4 = [ui.get_quad_x(), ui.get_quad_y()];
        self.config.agent_split = ui.get_agent_split().clamp(0.1, 0.85);
        self.save_config();
    }
    pub(super) fn sync_bookmarks(&self) {
        if let Some(ui) = self.ui.upgrade() {
            let rows: Vec<BookmarkRow> = self
                .config
                .bookmarks
                .iter()
                .map(|b| BookmarkRow {
                    name: b.name.clone().into(),
                    path: b.path.to_string_lossy().into_owned().into(),
                })
                .collect();
            ui.set_bookmarks(ModelRc::new(slint::VecModel::from(rows)));
        }
    }
    /// The window is closing: keep its placement for the next start. While maximized the
    /// restored rectangle is unknown, so the previous one is kept.
    pub(super) fn remember_window(&mut self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let window = ui.window();
        if window.is_minimized() {
            return;
        }
        let placement = if window.is_maximized() {
            match self.config.window {
                Some(previous) => config::Placement {
                    maximized: true,
                    ..previous
                },
                None => return,
            }
        } else {
            let (position, size) = (window.position(), window.size());
            config::Placement {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
                maximized: false,
            }
        };
        if self.config.window != Some(placement) {
            self.config.window = Some(placement);
        }
    }
    pub(super) fn save_config(&mut self) {
        if let Err(e) = self.config.save() {
            self.fail(format!("設定を保存できません: {e}"));
        }
        self.sync_bookmarks();
    }
    pub(super) fn open_bookmark(&mut self, i: usize) {
        if let Some(b) = self.config.bookmarks.get(i) {
            let path = b.path.clone();
            self.navigate(self.active, path, Mode::New(None));
        }
    }
    /// The active pane's folder, named after its last part.
    fn current_bookmark(&mut self) -> Option<config::Bookmark> {
        // The saved path has no `\\?\`, so it must still name this folder without it.
        let dir = match shell::os_path(&self.data(self.active).dir) {
            Ok(dir) => dir,
            Err(e) => {
                self.fail(e);
                return None;
            }
        };
        if dir.as_os_str().is_empty() {
            return None;
        }
        let name = dir
            .file_name()
            .map_or_else(|| display(&dir), |n| n.to_string_lossy().into_owned());
        Some(config::Bookmark { name, path: dir })
    }
    /// Adds the active pane's folder; it can be renamed or reordered in the edit dialog.
    pub(super) fn add_bookmark(&mut self) {
        let Some(bookmark) = self.current_bookmark() else {
            return;
        };
        self.message(format!("ブックマークに追加: {}", bookmark.name));
        self.config.bookmarks.push(bookmark);
        self.save_config();
    }
    /// Opens the editor on a copy of every bookmark, with row `i` focused.
    pub(super) fn edit_bookmark(&mut self, i: usize) {
        self.bookmark_draft = self
            .config
            .bookmarks
            .iter()
            .map(|b| (b.name.clone(), Draft::Saved(Some(b.path.clone()))))
            .collect();
        self.bookmark_rows(i, false);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_bookmark_editing(true);
        }
        self.apply_opacity();
    }
    /// Rebuilds the editor's rows from the draft; row `focus` takes the focus (its path if
    /// `path`), and no field does when it is out of range.
    fn bookmark_rows(&self, focus: usize, path: bool) {
        let Some(ui) = self.ui.upgrade() else { return };
        let rows: Vec<BookmarkRow> = self
            .bookmark_draft
            .iter()
            .map(|(name, path)| BookmarkRow {
                name: name.clone().into(),
                path: path.text().into(),
            })
            .collect();
        ui.set_bookmark_focus_row(if focus < rows.len() { focus as i32 } else { -1 });
        ui.set_bookmark_focus_path(path);
        ui.set_bookmark_draft(ModelRc::new(slint::VecModel::from(rows)));
    }
    pub(super) fn bookmark_edited(&mut self, i: usize, path: bool, text: &str) {
        if let Some((n, p)) = self.bookmark_draft.get_mut(i) {
            if path {
                *p = Draft::Edited(text.to_string());
            } else {
                *n = text.to_string();
            }
        }
    }
    pub(super) fn bookmark_browse(&self, i: usize) {
        let Some((_, draft)) = self.bookmark_draft.get(i) else {
            return;
        };
        let start = match draft.path() {
            Ok(path) => path,
            Err(error) => return self.fail(error),
        };
        std::thread::spawn(move || {
            if let Some(path) = shell::pick_folder(start.as_deref()) {
                post(Reply::Picked(BOOKMARK_FOLDER + i, path));
            }
        });
    }
    pub(super) fn bookmark_picked(&mut self, i: usize, path: &Path) {
        if !self
            .ui
            .upgrade()
            .is_some_and(|ui| ui.get_bookmark_editing())
        {
            return;
        }
        if let Some((_, p)) = self.bookmark_draft.get_mut(i) {
            *p = Draft::Saved(Some(path.to_path_buf()));
            self.bookmark_rows(i, true);
        }
    }
    pub(super) fn bookmark_add_current(&mut self) {
        let Some(b) = self.current_bookmark() else {
            return;
        };
        self.bookmark_draft
            .push((b.name, Draft::Saved(Some(b.path))));
        self.bookmark_rows(self.bookmark_draft.len() - 1, false);
    }
    pub(super) fn delete_bookmark(&mut self, i: usize) {
        if i < self.bookmark_draft.len() {
            self.bookmark_draft.remove(i);
            self.bookmark_rows(usize::MAX, false);
        }
    }
    /// `follow`: the moved row keeps the focus (Alt+↑↓); otherwise no field has it (▲▼).
    pub(super) fn move_bookmark(&mut self, i: usize, down: bool, follow: bool) {
        let j = if down { i + 1 } else { i.wrapping_sub(1) };
        if i < self.bookmark_draft.len() && j < self.bookmark_draft.len() {
            self.bookmark_draft.swap(i, j);
            self.bookmark_rows(if follow { j } else { usize::MAX }, false);
        }
    }
    pub(super) fn bookmark_done(&mut self, save: bool) {
        if save {
            let mut bookmarks = Vec::new();
            for (i, (name, draft)) in self.bookmark_draft.iter().enumerate() {
                let name = name.trim();
                let path = match draft.path() {
                    Ok(Some(path)) if !name.is_empty() => path,
                    Err(error) => {
                        self.fail(error);
                        self.bookmark_rows(i, true);
                        return;
                    }
                    _ => {
                        self.message("名前とパスを入力してください");
                        self.bookmark_rows(i, !name.is_empty());
                        return;
                    }
                };
                bookmarks.push(config::Bookmark {
                    name: name.into(),
                    path,
                });
            }
            self.config.bookmarks = bookmarks;
            self.save_config();
            self.message("ブックマークを保存しました");
        }
        self.bookmark_draft.clear();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_bookmark_editing(false);
            ui.invoke_focus_pane(self.active as i32);
        }
        self.apply_opacity();
    }
    pub(super) fn settings_agent_rows(&self) {
        if let Some(ui) = self.ui.upgrade() {
            let rows: Vec<slint::SharedString> = self
                .settings_agent_folders
                .iter()
                .map(|d| pathinput::display(d).into())
                .collect();
            ui.set_settings_agent_folders(ModelRc::new(slint::VecModel::from(rows)));
        }
    }
    pub(super) fn agent_folder_add(&mut self) {
        let start = self.settings_agent_folders.last().cloned();
        std::thread::spawn(move || {
            if let Some(path) = shell::pick_folder(start.as_deref()) {
                post(Reply::Picked(AGENT_FOLDER, path));
            }
        });
    }
    pub(super) fn agent_folder_remove(&mut self, i: usize) {
        if i < self.settings_agent_folders.len() {
            self.settings_agent_folders.remove(i);
        }
        self.settings_agent_rows();
    }
    /// The memory in the default editor for .md files (created first if missing).
    pub(super) fn agent_memory_open(&self) {
        let file = self.config.agent_memory();
        std::thread::spawn(move || {
            let result = agent::ensure_memory(&file)
                .map_err(|e| e.to_string())
                .and_then(|()| shell::open(&file));
            post(Reply::Opened(result));
        });
    }
}
