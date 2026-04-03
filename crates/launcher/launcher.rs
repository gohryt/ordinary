mod text_input;

use clap::{Parser, ValueEnum};
use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, Hsla, KeyBinding, Point, ScrollHandle,
    SharedString, Size, Window, WindowBounds, WindowDecorations, WindowKind, WindowOptions,
    actions, div, img,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    prelude::*,
    px,
};
use gpui_platform::application;
use ipc::LauncherProvider;
use ordinary_theme::Theme;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, rc::Rc};
use text_input::TextInput;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompositorKind {
    Hyprland,
    Niri,
    River,
}

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long, value_enum)]
    compositor: CompositorKind,
}

#[derive(Clone)]
struct AppEntry {
    name: SharedString,
    name_lowercase: String,
    exec: SharedString,
    icon: Option<PathBuf>,
}

fn load_desktop_entries() -> Vec<AppEntry> {
    let mut entries = Vec::new();
    let dirs = [
        "/usr/share/applications".into(),
        format!(
            "{}/.local/share/applications",
            std::env::var("HOME").unwrap_or_default()
        ),
    ];

    for dir in &dirs {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            continue;
        };

        for entry in read_dir.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "desktop")
                && let Some(app) = parse_desktop_entry(&path)
            {
                entries.push(app);
            }
        }
    }

    entries.sort_by_cached_key(|entry| entry.name_lowercase.clone());
    entries.dedup_by(|current, previous| current.name == previous.name);
    entries
}

fn parse_desktop_entry(path: &std::path::Path) -> Option<AppEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut no_display = false;
    let mut entry_type = None;
    let mut in_desktop_entry = false;

    for line in content.lines() {
        let line = line.trim();

        if line.starts_with('[') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }

        if !in_desktop_entry {
            continue;
        }

        if let Some(value) = line.strip_prefix("Name=") {
            if name.is_none() {
                name = Some(value.to_string());
            }
        } else if let Some(value) = line.strip_prefix("Exec=") {
            exec = Some(strip_field_codes(value));
        } else if let Some(value) = line.strip_prefix("Icon=") {
            if icon.is_none() {
                icon = Some(value.to_string());
            }
        } else if line == "NoDisplay=true" {
            no_display = true;
        } else if let Some(value) = line.strip_prefix("Type=") {
            entry_type = Some(value.to_string());
        }
    }

    if no_display {
        return None;
    }

    if entry_type.as_deref() != Some("Application") {
        return None;
    }

    let icon_path =
        icon.and_then(|icon_name| ordinary_theme::resolve_icon(&icon_name, "", &["apps"]));

    let name = name?;
    Some(AppEntry {
        name_lowercase: name.to_lowercase(),
        name: name.into(),
        exec: exec?.into(),
        icon: icon_path,
    })
}

fn strip_field_codes(exec: &str) -> String {
    let tokens = shell_words::split(exec)
        .unwrap_or_else(|_| exec.split_whitespace().map(str::to_string).collect());
    let filtered: Vec<String> = tokens
        .into_iter()
        .filter(|token| {
            !matches!(
                token.as_str(),
                "%u" | "%U"
                    | "%f"
                    | "%F"
                    | "%d"
                    | "%D"
                    | "%n"
                    | "%N"
                    | "%i"
                    | "%c"
                    | "%k"
                    | "%v"
                    | "%m"
            )
        })
        .collect();
    shell_words::join(filtered)
}

#[derive(Clone)]
struct DraggedApp {
    app_index: usize,
    name: SharedString,
    icon: Option<PathBuf>,
}

impl Render for DraggedApp {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::DEFAULT;
        let mut cell = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(theme.spacing_extra_small)
            .p(theme.spacing_small)
            .w(px(96.0))
            .bg(theme.surface)
            .rounded(theme.radius_small)
            .opacity(0.8)
            .text_color(theme.foreground)
            .text_size(theme.text_extra_small);

        if let Some(icon_path) = &self.icon {
            cell = cell.child(
                img(icon_path.clone())
                    .w(theme.app_icon_size)
                    .h(theme.app_icon_size),
            );
        } else {
            cell = cell.child(
                div()
                    .w(theme.app_icon_size)
                    .h(theme.app_icon_size)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.dimmed)
                    .child("?"),
            );
        }

        cell.child(
            div()
                .w_full()
                .text_center()
                .truncate()
                .child(self.name.clone()),
        )
    }
}

#[derive(Serialize, Deserialize, Default)]
struct LauncherConfig {
    pinned: Vec<Option<String>>,
}

fn config_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .unwrap_or_else(|_| format!("{}/.config", std::env::var("HOME").unwrap_or_default()));
    PathBuf::from(config_dir).join("ordinary/launcher.json")
}

fn load_config() -> LauncherConfig {
    let path = config_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(config: &LauncherConfig) {
    let path = config_path();
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }

    let Ok(json) = serde_json::to_string_pretty(config) else {
        return;
    };

    let temporary_path = parent.join(format!(".launcher.{}.tmp", std::process::id()));
    if std::fs::write(&temporary_path, &json).is_ok() {
        let _ = std::fs::rename(&temporary_path, &path);
    }
}

const MAX_PINNED: usize = 8;

fn load_pinned_apps(apps: &[AppEntry]) -> [Option<usize>; MAX_PINNED] {
    let config = load_config();
    let mut pinned = [None; MAX_PINNED];
    for (slot, entry) in config.pinned.iter().enumerate().take(MAX_PINNED) {
        if let Some(name) = entry {
            pinned[slot] = apps
                .iter()
                .position(|app| app.name.as_ref() == name.as_str());
        }
    }
    pinned
}

fn save_pinned_apps(apps: &[AppEntry], pinned: &[Option<usize>; MAX_PINNED]) {
    let mut slots: Vec<Option<String>> = pinned
        .iter()
        .map(|slot| slot.and_then(|index| apps.get(index).map(|app| app.name.to_string())))
        .collect();
    while slots.last() == Some(&None) {
        slots.pop();
    }
    save_config(&LauncherConfig { pinned: slots });
}

#[derive(Clone, Copy, PartialEq)]
enum Selection {
    Pinned(usize),
    Grid(usize),
}

actions!(
    launcher,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Copy,
        Cut,
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
        Launch,
        Dismiss,
        Pin,
        Unpin,
        MovePinnedLeft,
        MovePinnedRight,
        Pin1,
        Pin2,
        Pin3,
        Pin4,
        Pin5,
        Pin6,
        Pin7,
        Pin8,
    ]
);

struct Launcher {
    text_input: Entity<TextInput>,
    provider: Rc<dyn LauncherProvider>,
    apps: Vec<AppEntry>,
    filtered: Vec<usize>,
    pinned: [Option<usize>; MAX_PINNED],
    selected: Selection,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
}

impl Launcher {
    fn new(window: &mut Window, compositor_kind: CompositorKind, cx: &mut Context<Self>) -> Self {
        let apps = load_desktop_entries();
        let pinned = load_pinned_apps(&apps);
        let filtered: Vec<usize> = (0..apps.len())
            .filter(|app_index| !pinned.contains(&Some(*app_index)))
            .collect();

        let entity = cx.entity().downgrade();
        let text_input = cx.new(|cx| {
            TextInput::new(
                "Search applications...",
                Some(Box::new(move |query, cx| {
                    entity
                        .update(cx, |launcher, cx| {
                            launcher.filter(query);
                            cx.notify();
                        })
                        .ok();
                })),
                cx,
            )
        });

        let provider = match compositor_kind {
            CompositorKind::Hyprland => ipc_hyprland::create_launcher_provider(),
            CompositorKind::Niri => ipc_niri::create_launcher_provider(),
            CompositorKind::River => ipc_river::create_launcher_provider(),
        };
        let focus_handle = cx.focus_handle();
        window.focus(&text_input.focus_handle(cx), cx);

        Self {
            text_input,
            provider,
            apps,
            filtered,
            selected: if let Some(first) = pinned.iter().position(|s| s.is_some()) {
                Selection::Pinned(first)
            } else {
                Selection::Grid(0)
            },
            pinned,
            focus_handle,
            scroll_handle: ScrollHandle::new(),
        }
    }

    fn filter(&mut self, query: &str) {
        let query = query.to_lowercase();
        self.filtered = self.build_filtered(&query);
        if !query.is_empty() {
            self.selected = Selection::Grid(0);
            self.scroll_handle.scroll_to_item(0);
        } else if let Some(first) = self.pinned.iter().position(|s| s.is_some()) {
            self.selected = Selection::Pinned(first);
        } else {
            self.selected = Selection::Grid(0);
            self.scroll_handle.scroll_to_item(0);
        }
    }

    fn rebuild_filtered(&mut self, cx: &App) {
        let query = self.text_input.read(cx).value().to_lowercase();
        self.filtered = self.build_filtered(&query);
        if self.filtered.is_empty()
            && self.has_pinned()
            && let Some(first) = self.pinned.iter().position(|s| s.is_some())
        {
            self.selected = Selection::Pinned(first);
        }
    }

    fn build_filtered(&self, query: &str) -> Vec<usize> {
        self.apps
            .iter()
            .enumerate()
            .filter(|(app_index, app)| {
                !self.pinned.contains(&Some(*app_index))
                    && (query.is_empty() || app.name_lowercase.contains(query))
            })
            .map(|(app_index, _)| app_index)
            .collect()
    }

    const COLUMNS: usize = 8;

    fn has_pinned(&self) -> bool {
        self.pinned.iter().any(|s| s.is_some())
    }

    fn move_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected {
            Selection::Grid(index) => {
                if index >= Self::COLUMNS {
                    self.selected = Selection::Grid(index - Self::COLUMNS);
                    self.scroll_handle
                        .scroll_to_item(index.saturating_sub(Self::COLUMNS) / Self::COLUMNS);
                } else if self.has_pinned() {
                    let target = index.min(MAX_PINNED - 1);
                    if let Some(prev) = (0..=target)
                        .rev()
                        .find(|slot_index| self.pinned[*slot_index].is_some())
                    {
                        self.selected = Selection::Pinned(prev);
                    } else if let Some(next) = (target + 1..MAX_PINNED)
                        .find(|slot_index| self.pinned[*slot_index].is_some())
                    {
                        self.selected = Selection::Pinned(next);
                    }
                }
            }
            Selection::Pinned(_) => {}
        }
        cx.notify();
    }

    fn move_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected {
            Selection::Pinned(index) => {
                if !self.filtered.is_empty() {
                    let grid_index = index.min(self.filtered.len().saturating_sub(1));
                    self.selected = Selection::Grid(grid_index);
                    self.scroll_handle
                        .scroll_to_item(grid_index / Self::COLUMNS);
                }
            }
            Selection::Grid(index) => {
                if index + Self::COLUMNS < self.filtered.len() {
                    self.selected = Selection::Grid(index + Self::COLUMNS);
                    self.scroll_handle
                        .scroll_to_item((index + Self::COLUMNS) / Self::COLUMNS);
                }
            }
        }
        cx.notify();
    }

    fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected {
            Selection::Pinned(index) => {
                if index > 0
                    && let Some(prev) = (0..index)
                        .rev()
                        .find(|slot_index| self.pinned[*slot_index].is_some())
                {
                    self.selected = Selection::Pinned(prev);
                }
            }
            Selection::Grid(index) => {
                if index > 0 {
                    self.selected = Selection::Grid(index - 1);
                    self.scroll_handle
                        .scroll_to_item(index.saturating_sub(1) / Self::COLUMNS);
                } else if self.has_pinned()
                    && let Some(last) = (0..MAX_PINNED)
                        .rev()
                        .find(|slot_index| self.pinned[*slot_index].is_some())
                {
                    self.selected = Selection::Pinned(last);
                }
            }
        }
        cx.notify();
    }

    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected {
            Selection::Pinned(index) => {
                if index + 1 < MAX_PINNED {
                    if let Some(next) = (index + 1..MAX_PINNED)
                        .find(|slot_index| self.pinned[*slot_index].is_some())
                    {
                        self.selected = Selection::Pinned(next);
                    } else if !self.filtered.is_empty() {
                        self.selected = Selection::Grid(0);
                        self.scroll_handle.scroll_to_item(0);
                    }
                } else if !self.filtered.is_empty() {
                    self.selected = Selection::Grid(0);
                    self.scroll_handle.scroll_to_item(0);
                }
            }
            Selection::Grid(index) => {
                if index + 1 < self.filtered.len() {
                    self.selected = Selection::Grid(index + 1);
                    self.scroll_handle
                        .scroll_to_item((index + 1) / Self::COLUMNS);
                }
            }
        }
        cx.notify();
    }

    fn launch(&mut self, _: &Launch, _: &mut Window, cx: &mut Context<Self>) {
        let app_index = match self.selected {
            Selection::Pinned(index) => self.pinned[index],
            Selection::Grid(index) => self.filtered.get(index).copied(),
        };
        if let Some(app_index) = app_index {
            let app = &self.apps[app_index];
            self.provider.spawn(&app.exec);
            cx.quit();
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    fn move_pinned_left(&mut self, _: &MovePinnedLeft, _: &mut Window, cx: &mut Context<Self>) {
        if let Selection::Pinned(index) = self.selected
            && index > 0
            && self.pinned[index].is_some()
        {
            self.pinned.swap(index, index - 1);
            save_pinned_apps(&self.apps, &self.pinned);
            self.selected = Selection::Pinned(index - 1);
            cx.notify();
        }
    }

    fn move_pinned_right(&mut self, _: &MovePinnedRight, _: &mut Window, cx: &mut Context<Self>) {
        if let Selection::Pinned(index) = self.selected
            && index + 1 < MAX_PINNED
            && self.pinned[index].is_some()
        {
            self.pinned.swap(index, index + 1);
            save_pinned_apps(&self.apps, &self.pinned);
            self.selected = Selection::Pinned(index + 1);
            cx.notify();
        }
    }

    fn pin(&mut self, _: &Pin, _: &mut Window, cx: &mut Context<Self>) {
        if let Selection::Grid(index) = self.selected
            && let Some(&app_index) = self.filtered.get(index)
            && !self.pinned.contains(&Some(app_index))
            && let Some(slot) = self.pinned.iter().position(|s| s.is_none())
        {
            self.pinned[slot] = Some(app_index);
            save_pinned_apps(&self.apps, &self.pinned);
            self.rebuild_filtered(cx);
            self.selected = Selection::Pinned(slot);
            cx.notify();
        }
    }

    fn unpin(&mut self, _: &Unpin, _: &mut Window, cx: &mut Context<Self>) {
        if let Selection::Pinned(index) = self.selected
            && let Some(app_index) = self.pinned[index]
        {
            self.pinned[index] = None;
            save_pinned_apps(&self.apps, &self.pinned);
            self.rebuild_filtered(cx);
            if let Some(new_position) = self.filtered.iter().position(|&value| value == app_index) {
                self.selected = Selection::Grid(new_position);
                self.scroll_handle
                    .scroll_to_item(new_position / Self::COLUMNS);
            } else if let Some(first) = self.pinned.iter().position(|s| s.is_some()) {
                self.selected = Selection::Pinned(first);
            } else {
                self.selected = Selection::Grid(0);
            }
            cx.notify();
        }
    }

    fn activate_pinned(&mut self, slot: usize, cx: &mut Context<Self>) {
        if self.pinned[slot].is_none() {
            return;
        }
        if self.selected == Selection::Pinned(slot) {
            let app = &self.apps[self.pinned[slot].unwrap()];
            self.provider.spawn(&app.exec);
            cx.quit();
        } else {
            self.selected = Selection::Pinned(slot);
            cx.notify();
        }
    }

    fn pin1(&mut self, _: &Pin1, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_pinned(0, cx);
    }
    fn pin2(&mut self, _: &Pin2, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_pinned(1, cx);
    }
    fn pin3(&mut self, _: &Pin3, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_pinned(2, cx);
    }
    fn pin4(&mut self, _: &Pin4, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_pinned(3, cx);
    }
    fn pin5(&mut self, _: &Pin5, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_pinned(4, cx);
    }
    fn pin6(&mut self, _: &Pin6, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_pinned(5, cx);
    }
    fn pin7(&mut self, _: &Pin7, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_pinned(6, cx);
    }
    fn pin8(&mut self, _: &Pin8, _: &mut Window, cx: &mut Context<Self>) {
        self.activate_pinned(7, cx);
    }

    fn pin_app(&mut self, app_index: usize, slot: usize, cx: &mut Context<Self>) {
        if self.pinned.contains(&Some(app_index)) {
            return;
        }
        if slot < MAX_PINNED {
            self.pinned[slot] = Some(app_index);
            save_pinned_apps(&self.apps, &self.pinned);
            self.rebuild_filtered(cx);
            cx.notify();
        }
    }

    fn reorder_pinned(&mut self, app_index: usize, new_slot: usize, cx: &mut Context<Self>) {
        if let Some(old_position) = self.pinned.iter().position(|s| *s == Some(app_index)) {
            if old_position != new_slot {
                self.pinned.swap(old_position, new_slot);
            }
            save_pinned_apps(&self.apps, &self.pinned);
            cx.notify();
        }
    }
}

impl Focusable for Launcher {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Launcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::DEFAULT;
        let background = theme.background;
        let foreground = theme.foreground;
        let dimmed = theme.dimmed;
        let accent = theme.accent;

        // Pinned row
        let mut pinned_row = div().flex().flex_row().gap(theme.spacing_extra_small);

        for slot in 0..MAX_PINNED {
            if let Some(app_index) = self.pinned[slot] {
                let app = &self.apps[app_index];
                let is_selected = self.selected == Selection::Pinned(slot);

                let drag_data = DraggedApp {
                    app_index,
                    name: app.name.clone(),
                    icon: app.icon.clone(),
                };

                let mut cell = div()
                    .id(("pinned", slot))
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .items_center()
                    .gap(theme.spacing_extra_small)
                    .p(theme.spacing_small)
                    .rounded(theme.radius_small)
                    .overflow_hidden()
                    .when(is_selected, |s| s.text_color(background).bg(accent))
                    .when(!is_selected, |s| s.text_color(foreground))
                    .cursor_pointer()
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.selected != Selection::Pinned(slot) {
                            this.selected = Selection::Pinned(slot);
                            cx.notify();
                        }
                    }))
                    .on_click({
                        let exec = app.exec.clone();
                        let provider = self.provider.clone();
                        move |_, _, cx| {
                            provider.spawn(&exec);
                            cx.quit();
                        }
                    })
                    .on_drag(drag_data, |drag, _, _, cx| cx.new(|_| drag.clone()))
                    .drag_over::<DraggedApp>(move |style, _, _, _| {
                        style.bg(Hsla::from(accent).opacity(0.3))
                    })
                    .on_drop(cx.listener(move |this, drag: &DraggedApp, _, cx| {
                        if this.pinned.contains(&Some(drag.app_index)) {
                            this.reorder_pinned(drag.app_index, slot, cx);
                        } else {
                            this.pin_app(drag.app_index, slot, cx);
                        }
                    }))
                    .child(
                        div()
                            .absolute()
                            .top(theme.spacing_tiny)
                            .left(theme.spacing_small)
                            .text_size(theme.text_extra_small)
                            .text_color(dimmed)
                            .text_left()
                            .child(format!("{}", slot + 1)),
                    );

                if let Some(icon_path) = &app.icon {
                    cell = cell.child(
                        img(icon_path.clone())
                            .w(theme.app_icon_size)
                            .h(theme.app_icon_size),
                    );
                } else {
                    cell = cell.child(
                        div()
                            .w(theme.app_icon_size)
                            .h(theme.app_icon_size)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(dimmed)
                            .child("?"),
                    );
                }

                pinned_row = pinned_row.child(
                    cell.child(
                        div()
                            .w_full()
                            .text_size(theme.text_extra_small)
                            .text_center()
                            .truncate()
                            .child(app.name.clone()),
                    ),
                );
            } else {
                // Empty slot
                pinned_row = pinned_row.child(
                    div()
                        .id(("pinned-empty", slot))
                        .relative()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .items_center()
                        .gap(theme.spacing_extra_small)
                        .p(theme.spacing_small)
                        .rounded(theme.radius_small)
                        .child(
                            div()
                                .absolute()
                                .top(theme.spacing_tiny)
                                .left(theme.spacing_small)
                                .text_size(theme.text_extra_small)
                                .text_color(dimmed)
                                .text_left()
                                .child(format!("{}", slot + 1)),
                        )
                        .child(div().w(theme.app_icon_size).h(theme.app_icon_size))
                        .child(div().w_full().h(theme.text_extra_small))
                        .drag_over::<DraggedApp>(move |style, _, _, _| {
                            style.bg(Hsla::from(accent).opacity(0.2))
                        })
                        .on_drop(cx.listener(move |this, drag: &DraggedApp, _, cx| {
                            if this.pinned.contains(&Some(drag.app_index)) {
                                this.reorder_pinned(drag.app_index, slot, cx);
                            } else {
                                this.pin_app(drag.app_index, slot, cx);
                            }
                        })),
                );
            }
        }

        // Grid
        let grid_rows: Vec<_> = (0..self.filtered.len())
            .step_by(Self::COLUMNS)
            .map(|row_start| {
                let row_end = (row_start + Self::COLUMNS).min(self.filtered.len());
                let cells: Vec<_> = (row_start..row_end)
                    .map(|visual_index| {
                        let app_index = self.filtered[visual_index];
                        let app = &self.apps[app_index];
                        let is_selected = self.selected == Selection::Grid(visual_index);

                        let drag_data = DraggedApp {
                            app_index,
                            name: app.name.clone(),
                            icon: app.icon.clone(),
                        };

                        let mut cell = div()
                            .id(("app", app_index))
                            .flex()
                            .flex_col()
                            .flex_1()
                            .items_center()
                            .gap(theme.spacing_extra_small)
                            .p(theme.spacing_small)
                            .rounded(theme.radius_small)
                            .overflow_hidden()
                            .when(is_selected, |style| style.text_color(background))
                            .when(!is_selected, |style| style.text_color(foreground))
                            .when(is_selected, |style| style.bg(accent))
                            .cursor_pointer()
                            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                if this.selected != Selection::Grid(visual_index) {
                                    this.selected = Selection::Grid(visual_index);
                                    cx.notify();
                                }
                            }))
                            .on_click({
                                let exec = app.exec.clone();
                                let provider = self.provider.clone();
                                move |_, _, cx| {
                                    provider.spawn(&exec);
                                    cx.quit();
                                }
                            })
                            .on_drag(drag_data, |drag, _, _, cx| cx.new(|_| drag.clone()));

                        if let Some(icon_path) = &app.icon {
                            cell = cell.child(
                                img(icon_path.clone())
                                    .w(theme.app_icon_size)
                                    .h(theme.app_icon_size),
                            );
                        } else {
                            cell = cell.child(
                                div()
                                    .w(theme.app_icon_size)
                                    .h(theme.app_icon_size)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(dimmed)
                                    .child("?"),
                            );
                        }

                        cell.child(
                            div()
                                .w_full()
                                .text_size(theme.text_extra_small)
                                .text_center()
                                .truncate()
                                .child(app.name.clone()),
                        )
                    })
                    .collect();

                let padding = Self::COLUMNS - cells.len();
                let mut row_div = div()
                    .flex()
                    .flex_row()
                    .gap(theme.spacing_extra_small)
                    .children(cells);
                for _ in 0..padding {
                    row_div = row_div.child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .items_center()
                            .gap(theme.spacing_extra_small)
                            .p(theme.spacing_small)
                            .child(div().w(theme.app_icon_size).h(theme.app_icon_size))
                            .child(div().w_full().h(theme.text_extra_small)),
                    );
                }
                row_div
            })
            .collect();

        div()
            .flex()
            .size_full()
            .items_center()
            .justify_center()
            .track_focus(&self.focus_handle(cx))
            .key_context("Launcher")
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::launch))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::pin))
            .on_action(cx.listener(Self::unpin))
            .on_action(cx.listener(Self::move_pinned_left))
            .on_action(cx.listener(Self::move_pinned_right))
            .on_action(cx.listener(Self::pin1))
            .on_action(cx.listener(Self::pin2))
            .on_action(cx.listener(Self::pin3))
            .on_action(cx.listener(Self::pin4))
            .on_action(cx.listener(Self::pin5))
            .on_action(cx.listener(Self::pin6))
            .on_action(cx.listener(Self::pin7))
            .on_action(cx.listener(Self::pin8))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_1_2()
                    .h(theme.launcher_height)
                    .bg(theme.background)
                    .rounded(theme.radius_medium)
                    .p(theme.spacing_medium)
                    .gap(theme.spacing_small)
                    .child(self.text_input.clone())
                    .child(pinned_row)
                    .child(
                        div()
                            .w_full()
                            .h(theme.separator_thickness)
                            .bg(theme.separator),
                    )
                    .child(
                        div()
                            .id("grid")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .gap(theme.spacing_extra_small)
                            .on_drop(cx.listener(|this, drag: &DraggedApp, _, cx| {
                                if let Some(pos) =
                                    this.pinned.iter().position(|s| *s == Some(drag.app_index))
                                {
                                    this.pinned[pos] = None;
                                    save_pinned_apps(&this.apps, &this.pinned);
                                    this.rebuild_filtered(cx);
                                    this.selected = Selection::Grid(0);
                                    this.scroll_handle.scroll_to_item(0);
                                    cx.notify();
                                }
                            }))
                            .children(grid_rows)
                            .when(self.filtered.is_empty(), |grid| {
                                grid.child(
                                    div()
                                        .flex()
                                        .flex_1()
                                        .items_center()
                                        .justify_center()
                                        .text_color(dimmed)
                                        .text_size(theme.text_small)
                                        .child("No applications found"),
                                )
                            }),
                    ),
            )
    }
}

fn bind_single_instance(socket_path: &str) -> Option<std::os::unix::net::UnixListener> {
    match std::os::unix::net::UnixListener::bind(socket_path) {
        Ok(listener) => Some(listener),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            if std::os::unix::net::UnixStream::connect(socket_path).is_ok() {
                return None;
            }
            let _ = std::fs::remove_file(socket_path);
            std::os::unix::net::UnixListener::bind(socket_path).ok()
        }
        Err(_) => None,
    }
}

fn main() {
    let cli = Cli::parse();
    let compositor_kind = cli.compositor;

    let socket_path = format!(
        "{}/ordinary-launcher.sock",
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())
    );

    let Some(listener) = bind_single_instance(&socket_path) else {
        return;
    };

    let socket_path_cleanup = socket_path.clone();

    env_logger::init();

    application().run(move |cx: &mut App| {
        cx.spawn(async move |cx| {
            let listener = match async_io::Async::new(listener) {
                Ok(async_listener) => async_listener,
                Err(_) => return,
            };

            if listener.accept().await.is_ok() {
                cx.update(|cx| cx.quit());
            }
        })
        .detach();
        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, Some("TextInput")),
            KeyBinding::new("delete", Delete, Some("TextInput")),
            KeyBinding::new("left", Left, Some("TextInput")),
            KeyBinding::new("right", Right, Some("TextInput")),
            KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
            KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
            KeyBinding::new("ctrl-a", SelectAll, Some("TextInput")),
            KeyBinding::new("home", Home, Some("TextInput")),
            KeyBinding::new("end", End, Some("TextInput")),
            KeyBinding::new("ctrl-v", Paste, Some("TextInput")),
            KeyBinding::new("ctrl-c", Copy, Some("TextInput")),
            KeyBinding::new("ctrl-x", Cut, Some("TextInput")),
            KeyBinding::new("up", MoveUp, Some("Launcher")),
            KeyBinding::new("down", MoveDown, Some("Launcher")),
            KeyBinding::new("right", MoveRight, Some("Launcher")),
            KeyBinding::new("left", MoveLeft, Some("Launcher")),
            KeyBinding::new("enter", Launch, Some("Launcher")),
            KeyBinding::new("escape", Dismiss, Some("Launcher")),
            KeyBinding::new("ctrl-up", Pin, Some("Launcher")),
            KeyBinding::new("ctrl-down", Unpin, Some("Launcher")),
            KeyBinding::new("ctrl-left", MovePinnedLeft, Some("Launcher")),
            KeyBinding::new("ctrl-right", MovePinnedRight, Some("Launcher")),
            KeyBinding::new("1", Pin1, Some("Launcher")),
            KeyBinding::new("2", Pin2, Some("Launcher")),
            KeyBinding::new("3", Pin3, Some("Launcher")),
            KeyBinding::new("4", Pin4, Some("Launcher")),
            KeyBinding::new("5", Pin5, Some("Launcher")),
            KeyBinding::new("6", Pin6, Some("Launcher")),
            KeyBinding::new("7", Pin7, Some("Launcher")),
            KeyBinding::new("8", Pin8, Some("Launcher")),
        ]);

        let theme = Theme::DEFAULT;
        let launcher_height = theme.launcher_height;

        if let Err(error) = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Point::default(),
                    Size::new(px(0.0), launcher_height),
                ))),
                titlebar: None,
                window_decorations: Some(WindowDecorations::Client),
                kind: WindowKind::LayerShell(LayerShellOptions {
                    namespace: "ordinary-launcher".to_string(),
                    layer: Layer::Overlay,
                    anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                    exclusive_zone: None,
                    exclusive_edge: None,
                    margin: None,
                    keyboard_interactivity: KeyboardInteractivity::Exclusive,
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Launcher::new(window, compositor_kind, cx)),
        ) {
            log::error!("Failed to open launcher window: {}", error);
        }
    });

    let _ = std::fs::remove_file(&socket_path_cleanup);
}
