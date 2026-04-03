use gpui::{
    AnyElement, App, Bounds, Context, Entity, FocusHandle, Focusable, MouseButton, MouseDownEvent,
    Pixels, Point, SharedString, Size, Window, WindowBounds, WindowDecorations, WindowKind,
    WindowOptions, div, img,
    layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
    prelude::*,
    px,
};
use ipc::TrayProvider;
use ipc_zbus::SystemTray;
use ordinary_theme::Theme;

use crate::{DismissMenu, StatusBar};

pub(super) struct TrayMenu {
    items: Vec<ipc::MenuItem>,
    tray: Entity<SystemTray>,
    address: String,
    click_x: Pixels,
    focus_handle: FocusHandle,
}

impl TrayMenu {
    fn new(
        items: Vec<ipc::MenuItem>,
        tray: Entity<SystemTray>,
        address: String,
        click_x: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            items,
            tray,
            address,
            click_x,
            focus_handle,
        }
    }

    fn dismiss(&mut self, _: &DismissMenu, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }

    fn set_items(&mut self, items: Vec<ipc::MenuItem>, cx: &mut Context<Self>) {
        self.items = items;
        cx.notify();
    }
}

impl Focusable for TrayMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TrayMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::DEFAULT;

        let visible_items: Vec<_> = self.items.iter().filter(|item| item.visible).collect();

        let background = theme.background;
        let foreground = theme.foreground;
        let dimmed = theme.dimmed;
        let accent = theme.accent;
        let separator = theme.separator;

        let menu_content = div()
            .flex()
            .flex_col()
            .bg(background)
            .rounded_b(theme.radius_medium)
            .border_1()
            .border_color(separator)
            .py(theme.spacing_extra_small)
            .min_w(theme.menu_minimum_width)
            .max_w(theme.menu_maximum_width)
            .children(visible_items.iter().enumerate().map(|(item_index, item)| {
                if item.is_separator {
                    return div()
                        .id(("menu-sep", item_index))
                        .mx(theme.spacing_small)
                        .my(theme.spacing_extra_small)
                        .h(theme.separator_thickness)
                        .bg(separator)
                        .into_any_element();
                }

                let item_id = item.id;
                let tray = self.tray.clone();
                let address = self.address.clone();
                let enabled = item.enabled;
                let label = item.label.clone();

                let mut toggle_prefix = SharedString::from("");
                if item.toggle_type == "checkmark" {
                    toggle_prefix = if item.toggle_state == 1 {
                        "\u{2713} ".into()
                    } else {
                        "    ".into()
                    };
                } else if item.toggle_type == "radio" {
                    toggle_prefix = if item.toggle_state == 1 {
                        "\u{25C9} ".into()
                    } else {
                        "\u{25CB} ".into()
                    };
                }

                let text_color = if enabled { foreground } else { dimmed };

                let mut row = div()
                    .id(("menu-item", item_index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .px(theme.spacing_small)
                    .py(theme.spacing_extra_small)
                    .text_size(theme.text_extra_small)
                    .text_color(text_color);

                if enabled {
                    row = row
                        .cursor_pointer()
                        .hover(move |style| style.bg(accent).text_color(background))
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            tray.update(cx, |tray, cx| {
                                tray.activate_menu_item(&address, item_id, cx);
                            });
                            window.remove_window();
                        });
                }

                row.child(format!("{}{}", toggle_prefix, label))
                    .into_any_element()
            }));

        let menu_width = theme.menu_maximum_width;
        let window_width = window.bounds().size.width;
        let max_left = (window_width - menu_width).max(px(0.0));
        let menu_left = (self.click_x - menu_width / 2.0).max(px(0.0)).min(max_left);

        div()
            .size_full()
            .relative()
            .track_focus(&self.focus_handle(cx))
            .key_context("TrayMenu")
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down(MouseButton::Left, |_, window, _cx| {
                window.remove_window();
            })
            .child(
                div()
                    .absolute()
                    .left(menu_left)
                    .child(menu_content.on_mouse_down(MouseButton::Left, |_, _, _| {})),
            )
    }
}

impl StatusBar {
    pub(super) fn open_menu(&mut self, address: String, click_x: Pixels, cx: &mut Context<Self>) {
        self.close_menu_window(cx);
        self.menu_address = Some(address.clone());

        let tray_entity = self.tray.clone();
        let loading_items = vec![menu_message_item("Loading menu...")];

        match cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Point::default(),
                    Size::new(px(0.), px(0.)),
                ))),
                titlebar: None,
                window_decorations: Some(WindowDecorations::Client),
                kind: WindowKind::LayerShell(LayerShellOptions {
                    namespace: "ordinary-tray-menu".to_string(),
                    layer: Layer::Overlay,
                    anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                    exclusive_zone: None,
                    exclusive_edge: None,
                    margin: None,
                    keyboard_interactivity: KeyboardInteractivity::Exclusive,
                }),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    TrayMenu::new(
                        loading_items,
                        tray_entity.clone(),
                        address.clone(),
                        click_x,
                        window,
                        cx,
                    )
                })
            },
        ) {
            Ok(handle) => {
                self.menu_window = Some(handle);
            }
            Err(error) => {
                log::error!("Failed to open tray menu window: {}", error);
                return;
            }
        }

        let menu_address = address.clone();
        let menu_address_for_fetch = menu_address.clone();
        let menu_window_handle = self.menu_window;
        self.tray.update(cx, |tray_model, cx| {
            tray_model.fetch_menu(
                &menu_address_for_fetch,
                Box::new(move |result, cx| {
                    let items = match result {
                        Ok(items) if !items.is_empty() => items,
                        Ok(_) => vec![menu_message_item("No menu items")],
                        Err(error) => {
                            log::error!("Menu fetch failed for {}: {}", menu_address, error);
                            vec![menu_message_item("Failed to load menu")]
                        }
                    };

                    if let Some(handle) = menu_window_handle {
                        let _ = handle.update(cx, |menu, _window, cx| {
                            menu.set_items(items, cx);
                        });
                    }
                }),
                cx,
            );
        });
    }

    pub(super) fn close_menu_window(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.menu_window.take() {
            let _ = handle.update(cx, |_, window, _| {
                window.remove_window();
            });
        }
        self.menu_address = None;
    }

    pub(super) fn render_tray_state(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::DEFAULT;
        let tray = self.tray.read(cx);
        let button_padding = theme.spacing_tiny;

        let visible_items: Vec<_> = tray
            .items()
            .iter()
            .filter(|item| item.status != "Passive")
            .collect();

        if visible_items.is_empty() {
            return None;
        }

        let mut tray_row = div()
            .flex()
            .flex_row()
            .gap(theme.button_gap_m)
            .items_center();

        for (item_index, item) in visible_items.iter().enumerate() {
            let address = item.address.clone();
            let address_left = address.clone();
            let address_right = address;
            let tray_entity = self.tray.clone();
            let has_menu = item.menu_path.is_some();

            let mut icon_div = div()
                .id(("tray", item_index))
                .flex()
                .items_center()
                .justify_center()
                .h(theme.button_size_m)
                .min_w(theme.button_size_m)
                .p(theme.button_padding_m)
                .rounded(theme.button_radius_m)
                .text_color(theme.foreground)
                .text_size(theme.text_m)
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_, event: &MouseDownEvent, window, cx| {
                        let (x_position, y_position) = screen_position(window, event.position);
                        tray_entity.update(cx, |tray, cx| {
                            tray.activate_at(&address_left, x_position, y_position, cx);
                        });
                    }),
                );

            if has_menu {
                icon_div = icon_div.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.open_menu(address_right.clone(), event.position.x, cx);
                    }),
                );
            } else {
                let tray_right = self.tray.clone();
                icon_div = icon_div.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |_, event: &MouseDownEvent, window, cx| {
                        let (x_position, y_position) = screen_position(window, event.position);
                        tray_right.update(cx, |tray, cx| {
                            tray.context_menu_at(&address_right, x_position, y_position, cx);
                        });
                    }),
                );
            }

            if let Some(icon_path) = &item.icon_path {
                icon_div = icon_div.child(
                    div().px(button_padding).py(button_padding).child(
                        img(icon_path.clone())
                            .w(theme.tray_icon_image_size)
                            .h(theme.tray_icon_image_size),
                    ),
                );
            } else {
                icon_div = icon_div.child(div().px(button_padding).py(button_padding).child("•"));
            }

            tray_row = tray_row.child(icon_div);
        }

        Some(tray_row.into_any_element())
    }
}

fn menu_message_item(label: &str) -> ipc::MenuItem {
    ipc::MenuItem {
        id: 0,
        label: label.to_string(),
        enabled: false,
        visible: true,
        is_separator: false,
        toggle_type: String::new(),
        toggle_state: 0,
        children: Vec::new(),
    }
}

fn pixels_to_i32(value: Pixels) -> i32 {
    f32::from(value.round()) as i32
}

fn screen_position(window: &Window, local_position: Point<Pixels>) -> (i32, i32) {
    let window_bounds = window.bounds();
    let screen_x = window_bounds.origin.x + local_position.x;
    let screen_y = window_bounds.origin.y + local_position.y;
    (pixels_to_i32(screen_x), pixels_to_i32(screen_y))
}
