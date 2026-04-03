mod tray;

use clap::{Parser, ValueEnum};
use gpui::{
    App, Bounds, Context, Entity, KeyBinding, Point, SharedString, Size, Window, WindowBounds,
    WindowDecorations, WindowHandle, WindowKind, WindowOptions, actions, div,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    prelude::*,
    px,
};
use gpui_platform::application;
use ipc::{BarProvider, WorkspaceState};
use ipc_zbus::SystemTray;
use ordinary_system::{battery, clock};
use ordinary_theme::Theme;

actions!(tray_menu, [DismissMenu]);

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

struct StatusBar {
    provider: Box<dyn BarProvider>,
    workspaces: Vec<WorkspaceState>,
    layout: SharedString,
    show_empty_workspaces: bool,
    tray: Entity<SystemTray>,
    clock_text: SharedString,
    battery_text: SharedString,
    battery_charging: bool,
    battery_capacity: u8,
    battery_path: Option<std::path::PathBuf>,
    menu_window: Option<WindowHandle<tray::TrayMenu>>,
    menu_address: Option<String>,
}

impl StatusBar {
    fn new(compositor_kind: CompositorKind, cx: &mut Context<Self>) -> Self {
        let on_update = |this: &mut Self, workspaces, layout, _cx: &mut Context<Self>| {
            this.workspaces = workspaces;
            this.layout = layout;
        };

        let (provider, show_empty_workspaces) = match compositor_kind {
            CompositorKind::Hyprland => ipc_hyprland::create_bar_provider(cx, on_update),
            CompositorKind::Niri => ipc_niri::create_bar_provider(cx, on_update),
            CompositorKind::River => ipc_river::create_bar_provider(cx, on_update),
        };

        let tray = cx.new(SystemTray::new);

        let battery_path = battery::find_battery();
        let (battery_text, battery_charging, battery_capacity) =
            if let Some(info) = battery_path.as_deref().and_then(battery::read) {
                (
                    format_battery(info.capacity, info.charging),
                    info.charging,
                    info.capacity,
                )
            } else {
                (SharedString::from(""), false, 100)
            };

        let clock_text: SharedString = clock::now().into();

        cx.spawn(async move |this, cx| {
            let mut tick: u32 = 0;
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;

                let result = this.update(cx, |this, cx| {
                    this.clock_text = clock::now().into();

                    if tick.is_multiple_of(30)
                        && let Some(info) = this.battery_path.as_deref().and_then(battery::read)
                    {
                        this.battery_text = format_battery(info.capacity, info.charging);
                        this.battery_charging = info.charging;
                        this.battery_capacity = info.capacity;
                    }

                    cx.notify();
                });

                if result.is_err() {
                    break;
                }

                tick = tick.wrapping_add(1);
            }
        })
        .detach();

        Self {
            provider,
            workspaces: Vec::new(),
            layout: "".into(),
            show_empty_workspaces,
            tray,
            clock_text,
            battery_text,
            battery_charging,
            battery_capacity,
            battery_path,
            menu_window: None,
            menu_address: None,
        }
    }
}

fn format_battery(capacity: u8, charging: bool) -> SharedString {
    if charging {
        format!("{}%+", capacity).into()
    } else {
        format!("{}%", capacity).into()
    }
}

impl Render for StatusBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::DEFAULT;

        div()
            .flex()
            .flex_row()
            .justify_between()
            .w_full()
            .bg(theme.background)
            .child(self.render_workspaces(cx))
            .child(self.render_state(cx))
    }
}

impl StatusBar {
    fn render_workspaces(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::DEFAULT;

        div()
            .flex()
            .flex_row()
            .p(theme.button_gap_m)
            .gap(theme.button_gap_m)
            .children(self.workspaces.iter().filter_map(|workspace| {
                if !self.show_empty_workspaces && !workspace.occupied && !workspace.active {
                    return None;
                }

                let background = if workspace.active {
                    theme.accent
                } else {
                    theme.surface
                };

                let foreground = if workspace.active {
                    theme.background
                } else {
                    theme.foreground
                };

                let workspace_id = workspace.id;

                Some(
                    div()
                        .id(("workspace", workspace_id))
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(theme.button_size_m)
                        .min_w(theme.button_size_m)
                        .p(theme.button_padding_m)
                        .rounded(theme.button_radius_m)
                        .bg(background)
                        .text_color(foreground)
                        .text_size(theme.text_m)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _event, _window, cx| {
                            this.provider.switch_workspace(workspace_id, cx);
                        }))
                        .child(format!("{}", workspace.index)),
                )
            }))
    }

    fn render_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::DEFAULT;

        let separator_element = || {
            div()
                .flex()
                .items_center()
                .justify_center()
                .h(theme.button_size_m)
                .pt(theme.button_padding_m)
                .pb(theme.button_padding_m)
                .text_color(theme.separator)
                .text_size(theme.text_m)
                .child("|")
        };

        let mut state = div()
            .flex()
            .flex_row()
            .p(theme.button_gap_m)
            .gap(theme.button_gap_m);

        if !self.layout.is_empty() {
            state = state
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(theme.button_size_m)
                        .min_w(theme.button_size_m)
                        .p(theme.button_padding_m)
                        .text_color(theme.foreground)
                        .text_size(theme.text_m)
                        .child(self.layout.clone()),
                )
                .child(separator_element());
        }

        if let Some(tray_section) = self.render_tray_state(cx) {
            state = state.child(tray_section).child(separator_element());
        }

        state = state
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(theme.button_size_m)
                    .min_w(theme.button_size_m)
                    .p(theme.button_padding_m)
                    .text_color(theme.foreground)
                    .text_size(theme.text_m)
                    .child(self.clock_text.clone()),
            )
            .child(separator_element());

        if self.battery_path.is_some() {
            let battery_color = if self.battery_charging {
                theme.good
            } else if self.battery_capacity <= 10 {
                theme.danger
            } else if self.battery_capacity <= 25 {
                theme.warning
            } else {
                theme.foreground
            };

            state = state
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(theme.button_size_m)
                        .min_w(theme.button_size_m)
                        .p(theme.button_padding_m)
                        .text_color(battery_color)
                        .text_size(theme.text_m)
                        .child(self.battery_text.clone()),
                )
                .child(separator_element());
        }

        state = state.child(self.render_logout_button(cx));

        state
    }

    fn render_logout_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::DEFAULT;

        let danger = theme.danger;

        div()
            .id("logout")
            .flex()
            .items_center()
            .justify_center()
            .h(theme.button_size_m)
            .min_w(theme.button_size_m)
            .p(theme.button_padding_m)
            .rounded(theme.button_radius_m)
            .text_color(danger)
            .text_size(theme.text_m)
            .hover(move |style| style.bg(danger).text_color(theme.background))
            .cursor_pointer()
            .on_click(cx.listener(|_this, _event, _window, _cx| {
                logout();
            }))
            .child("Logout")
    }
}

fn logout() {
    let (subcommand, arg) = if let Some(session_id) = std::env::var("XDG_SESSION_ID")
        .ok()
        .filter(|value| !value.is_empty())
    {
        ("terminate-session", session_id)
    } else {
        ("terminate-user", std::env::var("USER").unwrap_or_default())
    };

    if let Err(error) = std::process::Command::new("loginctl")
        .args([subcommand, &arg])
        .spawn()
    {
        log::error!("Failed to logout: {}", error);
    }
}

fn main() {
    let cli = Cli::parse();
    let compositor_kind = cli.compositor;

    env_logger::init();

    application().run(move |cx: &mut App| {
        cx.bind_keys([KeyBinding::new("escape", DismissMenu, Some("TrayMenu"))]);

        let theme = Theme::DEFAULT;
        let bar_height = theme.bar_height;

        if let Err(error) = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Point::default(),
                    Size::new(px(0.), bar_height),
                ))),
                titlebar: None,
                window_decorations: Some(WindowDecorations::Client),
                kind: WindowKind::LayerShell(LayerShellOptions {
                    namespace: "ordinary-bar".to_string(),
                    layer: Layer::Top,
                    anchor: Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
                    exclusive_zone: Some(bar_height),
                    // zwlr_layer_surface_v1 currently is v4 in river and labwc, v5 required for exclusive edges
                    exclusive_edge: None,
                    margin: None,
                    keyboard_interactivity: KeyboardInteractivity::OnDemand,
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| StatusBar::new(compositor_kind, cx)),
        ) {
            log::error!("Failed to open status bar window: {}", error);
        }
    });
}
