use std::{
    collections::HashMap,
    os::unix::io::AsRawFd,
    sync::{Arc, Mutex},
};
use x11rb::{
    CURRENT_TIME,
    connection::Connection,
    protocol::{
        Event,
        composite::{self, Redirect},
        damage::{self, ReportLevel},
        xproto::*,
        xtest,
    },
    wrapper::ConnectionExt as _,
};
use zbus::{object_server::SignalEmitter, zvariant::ObjectPath};

const EMBED_SIZE: u16 = 32;
const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;
const XEMBED_EMBEDDED_NOTIFY: u32 = 0;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        _NET_SYSTEM_TRAY_S0,
        _NET_SYSTEM_TRAY_OPCODE,
        _NET_SYSTEM_TRAY_ORIENTATION,
        _NET_WM_WINDOW_OPACITY,
        _NET_WM_ICON,
        _XEMBED,
        _XEMBED_INFO,
        MANAGER,
    }
}

struct IconState {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
    last_updated: std::time::Instant,
}

/// Tracks a docked icon window and its container
struct DockedWindow {
    client_window: u32,
    container_window: u32,
    damage_id: u32,
    dock_time: std::time::Instant,
    registered: bool,
}

enum BridgeEvent {
    Docked(u32),
    Undocked(u32),
    IconUpdated(u32),
}

const MAX_ICON_ENTRIES: usize = 256;

struct ClickEvent {
    window_id: u32,
    button: u8,
    x_position: i32,
    y_position: i32,
}

struct BridgedIcon {
    window_id: u32,
    state: Arc<Mutex<HashMap<u32, IconState>>>,
    click_sender: smol::channel::Sender<ClickEvent>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl BridgedIcon {
    #[zbus(property)]
    fn id(&self) -> String {
        format!("xembed_{:x}", self.window_id)
    }

    #[zbus(property)]
    fn title(&self) -> String {
        format!("XEmbed Icon {:x}", self.window_id)
    }

    #[zbus(property)]
    fn status(&self) -> String {
        "Active".into()
    }

    #[zbus(property)]
    fn icon_name(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn icon_theme_path(&self) -> String {
        String::new()
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let state = self.state.lock().unwrap();
        if let Some(icon) = state.get(&self.window_id) {
            if icon.pixels.is_empty() {
                return Vec::new();
            }
            vec![(icon.width as i32, icon.height as i32, icon.pixels.clone())]
        } else {
            Vec::new()
        }
    }

    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::try_from("/").unwrap()
    }

    async fn activate(&self, x_position: i32, y_position: i32) {
        let _ = self
            .click_sender
            .send(ClickEvent {
                window_id: self.window_id,
                button: 1,
                x_position,
                y_position,
            })
            .await;
    }

    async fn secondary_activate(&self, x_position: i32, y_position: i32) {
        let _ = self
            .click_sender
            .send(ClickEvent {
                window_id: self.window_id,
                button: 2,
                x_position,
                y_position,
            })
            .await;
    }

    async fn context_menu(&self, x_position: i32, y_position: i32) {
        let _ = self
            .click_sender
            .send(ClickEvent {
                window_id: self.window_id,
                button: 3,
                x_position,
                y_position,
            })
            .await;
    }

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn new_title(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait StatusNotifierWatcher {
    fn register_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
    fn unregister_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
    fn refresh_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
}

struct CapturedIcon {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
}

/// Capture icon pixels from a redirected window via GetImage.
/// The window must be composite-redirected and mapped (viewable) for this to work.
fn capture_icon(connection: &impl Connection, window: u32) -> Option<CapturedIcon> {
    let geometry = match connection.get_geometry(window).ok()?.reply() {
        Ok(g) => g,
        Err(error) => {
            log::debug!("get_geometry failed for 0x{:x}: {}", window, error);
            return None;
        }
    };
    let width = geometry.width;
    let height = geometry.height;
    let depth = geometry.depth;

    if width == 0 || height == 0 {
        log::debug!("Window 0x{:x}: zero size ({}x{})", window, width, height);
        return None;
    }

    let reply = match connection.get_image(ImageFormat::Z_PIXMAP, window, 0, 0, width, height, !0) {
        Ok(cookie) => match cookie.reply() {
            Ok(reply) => reply,
            Err(error) => {
                log::info!(
                    "GetImage reply error for 0x{:x} ({}x{} depth={}): {}",
                    window,
                    width,
                    height,
                    depth,
                    error
                );
                return None;
            }
        },
        Err(error) => {
            log::info!("GetImage request error for 0x{:x}: {}", window, error);
            return None;
        }
    };

    let data = reply.data;
    let expected = width as usize * height as usize * 4;
    if data.len() < expected {
        log::debug!(
            "Window 0x{:x}: GetImage returned {} bytes, expected {}",
            window,
            data.len(),
            expected
        );
        return None;
    }

    let pixel_count = width as usize * height as usize;
    let total_bytes = pixel_count * 4;

    let has_content = data[..total_bytes]
        .chunks_exact(4)
        .any(|pixel| pixel[0] != 0 || pixel[1] != 0 || pixel[2] != 0);

    if !has_content {
        log::debug!(
            "Window 0x{:x}: GetImage returned all-zero pixels ({}x{})",
            window,
            width,
            height
        );
        return None;
    }

    // X11 ZPixmap 32-bit little-endian: [B, G, R, X/A]
    // For depth 24, the alpha byte is padding — force fully opaque
    let force_opaque = depth <= 24;
    let mut argb_pixels = Vec::with_capacity(pixel_count * 4);
    for pixel in data[..total_bytes].chunks_exact(4) {
        let blue = pixel[0];
        let green = pixel[1];
        let red = pixel[2];
        let alpha = if force_opaque { 255 } else { pixel[3] };
        argb_pixels.extend_from_slice(&[alpha, red, green, blue]);
    }

    log::debug!(
        "Window 0x{:x}: captured {}x{} icon (depth={})",
        window,
        width,
        height,
        depth
    );

    Some(CapturedIcon {
        width,
        height,
        pixels: argb_pixels,
    })
}

/// Capture icon from `_NET_WM_ICON` property (ARGB32 pixel data, native-endian u32 array).
/// Note: Wine sets this with the generic wine glass icon.
/// Format: [width, height, pixel_data...] repeated for each size.
fn capture_icon_from_property(
    connection: &impl Connection,
    window: u32,
    atoms: &Atoms,
) -> Option<CapturedIcon> {
    let reply = connection
        .get_property(
            false,
            window,
            atoms._NET_WM_ICON,
            AtomEnum::CARDINAL,
            0,
            u32::MAX,
        )
        .ok()?
        .reply()
        .ok()?;

    if reply.value_len == 0 {
        return None;
    }

    let data: Vec<u32> = reply
        .value32()
        .map(|iter| iter.collect())
        .unwrap_or_default();

    if data.len() < 3 {
        return None;
    }

    let mut best: Option<(u32, u32, &[u32], i32, u32)> = None;
    let target_size = EMBED_SIZE as i32;
    let max_size: u32 = 128;
    let mut offset = 0;

    while offset + 2 < data.len() {
        let width = data[offset];
        let height = data[offset + 1];
        let pixel_count = (width as usize) * (height as usize);

        if offset + 2 + pixel_count > data.len() {
            break;
        }

        let pixels = &data[offset + 2..offset + 2 + pixel_count];
        let size = width.max(height) as i32;
        let score = (size - target_size).abs();
        if width > max_size || height > max_size {
            offset += 2 + pixel_count;
            continue;
        }
        let area = width * height;

        if best
            .as_ref()
            .is_none_or(|&(_, _, _, best_score, best_area)| {
                score < best_score || (score == best_score && area > best_area)
            })
        {
            best = Some((width, height, pixels, score, area));
        }

        offset += 2 + pixel_count;
    }

    let (width, height, pixels, _, _) = best?;

    let mut argb_bytes = Vec::with_capacity(pixels.len() * 4);
    for &pixel in pixels {
        let alpha = ((pixel >> 24) & 0xFF) as u8;
        let red = ((pixel >> 16) & 0xFF) as u8;
        let green = ((pixel >> 8) & 0xFF) as u8;
        let blue = (pixel & 0xFF) as u8;
        argb_bytes.extend_from_slice(&[alpha, red, green, blue]);
    }

    log::debug!(
        "Window 0x{:x}: captured {}x{} icon from _NET_WM_ICON property",
        window,
        width,
        height
    );

    Some(CapturedIcon {
        width: width as u16,
        height: height as u16,
        pixels: argb_bytes,
    })
}

/// Capture icon from WM_HINTS icon_pixmap and icon_mask.
/// Note: Wine sets these with the generic wine glass, not the real app icon.
/// This is a fallback for non-Wine apps. Wine paints real icons into window pixels.
/// WM_HINTS is a list of 9 u32 values:
///   [flags, input, initial_state, icon_pixmap, icon_window, icon_x, icon_y, icon_mask, window_group]
/// Flag bit 2 (0x4) = IconPixmapHint, bit 5 (0x20) = IconMaskHint
fn capture_icon_from_hints(connection: &impl Connection, window: u32) -> Option<CapturedIcon> {
    let reply = connection
        .get_property(false, window, AtomEnum::WM_HINTS, AtomEnum::WM_HINTS, 0, 9)
        .ok()?
        .reply()
        .ok()?;

    let hints: Vec<u32> = reply
        .value32()
        .map(|iter| iter.collect())
        .unwrap_or_default();

    if hints.len() < 9 {
        return None;
    }

    let flags = hints[0];
    let icon_pixmap_hint = flags & 0x4 != 0;
    let icon_mask_hint = flags & 0x20 != 0;

    if !icon_pixmap_hint {
        return None;
    }

    let icon_pixmap = hints[3];
    let icon_mask = if icon_mask_hint { hints[7] } else { 0 };

    if icon_pixmap == 0 {
        return None;
    }

    let geometry = connection.get_geometry(icon_pixmap).ok()?.reply().ok()?;
    let width = geometry.width;
    let height = geometry.height;
    let depth = geometry.depth;

    if width == 0 || height == 0 {
        return None;
    }

    log::debug!(
        "Window 0x{:x}: WM_HINTS icon_pixmap=0x{:x} ({}x{} depth={}), icon_mask=0x{:x}",
        window,
        icon_pixmap,
        width,
        height,
        depth,
        icon_mask
    );

    let pixmap_reply = connection
        .get_image(ImageFormat::Z_PIXMAP, icon_pixmap, 0, 0, width, height, !0)
        .ok()?
        .reply()
        .ok()?;

    let pixmap_data = pixmap_reply.data;
    let pixel_count = width as usize * height as usize;

    let mask_data = if icon_mask != 0 {
        connection
            .get_image(ImageFormat::Z_PIXMAP, icon_mask, 0, 0, width, height, !0)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.data.to_vec())
    } else {
        None
    };

    let mut argb_pixels = Vec::with_capacity(pixel_count * 4);

    if depth == 1 {
        let bytes_per_row = (width as usize).div_ceil(8);
        for y in 0..height as usize {
            for x in 0..width as usize {
                let byte_idx = y * bytes_per_row + x / 8;
                let bit_idx = x % 8;
                let set = if byte_idx < pixmap_data.len() {
                    pixmap_data[byte_idx] & (1 << bit_idx) != 0
                } else {
                    false
                };
                let (red, green, blue) = if set { (255, 255, 255) } else { (0, 0, 0) };
                let alpha = 255u8;
                argb_pixels.extend_from_slice(&[alpha, red, green, blue]);
            }
        }
    } else if depth >= 24 {
        let total_bytes = pixel_count * 4;
        if pixmap_data.len() < total_bytes {
            return None;
        }
        for (i, pixel) in pixmap_data[..total_bytes].chunks_exact(4).enumerate() {
            let blue = pixel[0];
            let green = pixel[1];
            let red = pixel[2];

            let alpha = if let Some(ref mask) = mask_data {
                let bytes_per_row = (width as usize).div_ceil(8);
                let y_position = i / width as usize;
                let x_position = i % width as usize;
                let byte_idx = y_position * bytes_per_row + x_position / 8;
                let bit_idx = x_position % 8;
                if byte_idx < mask.len() && mask[byte_idx] & (1 << bit_idx) != 0 {
                    255
                } else {
                    0
                }
            } else if depth <= 24 {
                255
            } else {
                pixel[3]
            };

            argb_pixels.extend_from_slice(&[alpha, red, green, blue]);
        }
    } else {
        log::debug!(
            "Unsupported icon pixmap depth {} for window 0x{:x}",
            depth,
            window
        );
        return None;
    }

    log::info!(
        "Window 0x{:x}: captured {}x{} icon from WM_HINTS icon_pixmap",
        window,
        width,
        height
    );

    Some(CapturedIcon {
        width,
        height,
        pixels: argb_pixels,
    })
}

/// Forward a click to a docked window using XTest fake input:
/// 1. Move the container window to the click position
/// 2. Raise the container so the client window is under the pointer
/// 3. Warp the pointer into the client window
/// 4. Inject press+release via XTest (looks like real hardware input)
/// 5. Lower the container back
///
/// XTest is used instead of synthetic send_event because Wine and GTK apps
/// ignore synthetic X events for button clicks.
fn send_click(
    connection: &impl Connection,
    client_window: u32,
    container_window: u32,
    root: u32,
    button: u8,
    click_x_position: i32,
    click_y_position: i32,
) {
    let center = (EMBED_SIZE / 2) as i16;

    // Can't use query_pointer — the pointer is on a Wayland layer-shell
    // surface that XWayland doesn't track, so it returns stale coordinates.
    let (click_x_position, click_y_position) = if click_x_position > 0 || click_y_position > 0 {
        (click_x_position, click_y_position)
    } else {
        let screen_width = connection
            .get_geometry(root)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|geometry| geometry.width as i32)
            .unwrap_or(1920);

        let click_x_position = screen_width - 100;
        let click_y_position = EMBED_SIZE as i32 + 12;
        (click_x_position, click_y_position)
    };

    let container_x = click_x_position - center as i32;
    let container_y = click_y_position - center as i32;

    let _ = connection.configure_window(
        container_window,
        &ConfigureWindowAux::new()
            .x(container_x)
            .y(container_y)
            .stack_mode(StackMode::ABOVE),
    );
    let _ = connection.flush();

    let _ = connection.warp_pointer(x11rb::NONE, client_window, 0, 0, 0, 0, center, center);
    let _ = connection.flush();

    std::thread::sleep(std::time::Duration::from_millis(10));

    let _ = xtest::fake_input(
        connection,
        BUTTON_PRESS_EVENT,
        button,
        CURRENT_TIME,
        root,
        click_x_position as i16,
        click_y_position as i16,
        0,
    );
    let _ = connection.flush();

    std::thread::sleep(std::time::Duration::from_millis(10));

    let _ = xtest::fake_input(
        connection,
        BUTTON_RELEASE_EVENT,
        button,
        CURRENT_TIME,
        root,
        click_x_position as i16,
        click_y_position as i16,
        0,
    );
    let _ = connection.flush();

    std::thread::sleep(std::time::Duration::from_millis(50));

    let _ = connection.configure_window(
        container_window,
        &ConfigureWindowAux::new().stack_mode(StackMode::BELOW),
    );
    let _ = connection.flush();
}

fn setup_tray_manager(
    connection: &impl Connection,
    screen: &Screen,
    atoms: &Atoms,
) -> anyhow::Result<u32> {
    composite::query_version(connection, 0, 4)?.reply()?;
    damage::query_version(connection, 1, 1)?.reply()?;

    let tray_window = connection.generate_id()?;

    connection.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        tray_window,
        screen.root,
        -1,
        -1,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new()
            .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
    )?;

    connection.change_property32(
        PropMode::REPLACE,
        tray_window,
        atoms._NET_SYSTEM_TRAY_ORIENTATION,
        AtomEnum::CARDINAL,
        &[0],
    )?;

    connection.set_selection_owner(tray_window, atoms._NET_SYSTEM_TRAY_S0, CURRENT_TIME)?;
    connection.flush()?;

    let owner = connection
        .get_selection_owner(atoms._NET_SYSTEM_TRAY_S0)?
        .reply()?;

    if owner.owner != tray_window {
        anyhow::bail!("Another tray manager already owns _NET_SYSTEM_TRAY_S0");
    }

    let event = ClientMessageEvent::new(
        32,
        screen.root,
        atoms.MANAGER,
        [CURRENT_TIME, atoms._NET_SYSTEM_TRAY_S0, tray_window, 0, 0],
    );

    connection.send_event(false, screen.root, EventMask::STRUCTURE_NOTIFY, event)?;
    connection.flush()?;

    log::info!(
        "Claimed _NET_SYSTEM_TRAY_S0 selection on window 0x{:x}",
        tray_window
    );

    Ok(tray_window)
}

/// Create a per-icon container window on the root, dock the client into it.
/// The container is mapped (so the icon is "viewable" for GetImage) but hidden
/// from the user via override-redirect, stacked below, and zero opacity.
fn dock_window(
    connection: &impl Connection,
    screen: &Screen,
    client_window: u32,
    atoms: &Atoms,
) -> anyhow::Result<(u32, u32)> {
    let container = connection.generate_id()?;

    connection.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        container,
        screen.root,
        0,
        0,
        EMBED_SIZE,
        EMBED_SIZE,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .override_redirect(1)
            .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_NOTIFY),
    )?;

    match dock_window_inner(connection, container, client_window, atoms) {
        Ok(damage_id) => Ok((container, damage_id)),
        Err(error) => {
            let _ = connection.destroy_window(container);
            let _ = connection.flush();
            Err(error)
        }
    }
}

fn dock_window_inner(
    connection: &impl Connection,
    container: u32,
    client_window: u32,
    atoms: &Atoms,
) -> anyhow::Result<u32> {
    connection.change_property32(
        PropMode::REPLACE,
        container,
        atoms._NET_WM_WINDOW_OPACITY,
        AtomEnum::CARDINAL,
        &[0],
    )?;

    // Container must be viewable (mapped) for GetImage to work on children
    connection.map_window(container)?;

    connection.configure_window(
        container,
        &ConfigureWindowAux::new().stack_mode(StackMode::BELOW),
    )?;

    composite::redirect_window(connection, client_window, Redirect::MANUAL)?;

    connection.change_window_attributes(
        client_window,
        &ChangeWindowAttributesAux::new().event_mask(
            EventMask::STRUCTURE_NOTIFY | EventMask::EXPOSURE | EventMask::PROPERTY_CHANGE,
        ),
    )?;

    connection.reparent_window(client_window, container, 0, 0)?;

    connection.configure_window(
        client_window,
        &ConfigureWindowAux::new()
            .width(EMBED_SIZE as u32)
            .height(EMBED_SIZE as u32),
    )?;

    connection.map_window(client_window)?;

    // Critical for Wine: without clear_area, the backing pixmap may remain empty
    // and GetImage returns all-zero pixels.
    connection.clear_area(false, client_window, 0, 0, EMBED_SIZE, EMBED_SIZE)?;

    let event = ClientMessageEvent::new(
        32,
        client_window,
        atoms._XEMBED,
        [0, XEMBED_EMBEDDED_NOTIFY, 0, container, 0],
    );

    connection.send_event(false, client_window, EventMask::NO_EVENT, event)?;

    // DamageNotify fires when window content changes — detects Wine
    // finishing its GDI icon render.
    let damage_id = connection.generate_id()?;
    damage::create(connection, damage_id, client_window, ReportLevel::NON_EMPTY)?;

    connection.flush()?;

    log::info!(
        "Docked window 0x{:x} in container 0x{:x}, damage=0x{:x}",
        client_window,
        container,
        damage_id
    );

    Ok(damage_id)
}

fn run_x11_loop(
    state: Arc<Mutex<HashMap<u32, IconState>>>,
    bridge_sender: smol::channel::Sender<BridgeEvent>,
    click_receiver: smol::channel::Receiver<ClickEvent>,
) -> anyhow::Result<()> {
    let (connection, screen_num) = x11rb::connect(None)?;
    let screen = connection.setup().roots[screen_num].clone();
    let atoms = Atoms::new(&connection)?.reply()?;

    let _tray_window = setup_tray_manager(&connection, &screen, &atoms)?;

    let x11_fd = connection.stream().as_raw_fd();
    let mut docked: Vec<DockedWindow> = Vec::new();
    let mut last_capture = std::time::Instant::now();

    loop {
        while let Ok(click) = click_receiver.try_recv() {
            if let Some(entry) = docked
                .iter()
                .find(|docked_window| docked_window.client_window == click.window_id)
            {
                send_click(
                    &connection,
                    entry.client_window,
                    entry.container_window,
                    screen.root,
                    click.button,
                    click.x_position,
                    click.y_position,
                );
                log::info!(
                    "Forwarded button {} click to window 0x{:x}",
                    click.button,
                    click.window_id,
                );
            }
        }

        let event = connection.poll_for_event()?;

        if let Some(event) = event {
            match event {
                Event::ClientMessage(message) if message.type_ == atoms._NET_SYSTEM_TRAY_OPCODE => {
                    let data = message.data.as_data32();
                    let opcode = data[1];

                    if opcode == SYSTEM_TRAY_REQUEST_DOCK {
                        let client_window = data[2];

                        if client_window == 0 {
                            log::debug!("Ignoring dock request with window id 0");
                            continue;
                        }

                        let (container, damage_id) =
                            match dock_window(&connection, &screen, client_window, &atoms) {
                                Ok(result) => result,
                                Err(error) => {
                                    log::error!(
                                        "Failed to dock window 0x{:x}: {}",
                                        client_window,
                                        error
                                    );
                                    continue;
                                }
                            };

                        // Single non-blocking capture only — blocking stalls the
                        // event loop and misses Wine's dock→destroy→redock cycle.
                        // No WM_HINTS/_NET_WM_ICON fallback: Wine fills those with
                        // the generic wine glass. DamageNotify gets the real icon.
                        let captured = capture_icon(&connection, client_window);
                        let has_icon = captured.is_some();

                        if let Some(captured) = captured {
                            store_icon_state(&state, client_window, captured);
                        }

                        // Delay SNI registration until we have a valid icon to
                        // avoid a dot placeholder during Wine's dock→destroy→redock.
                        let registered = has_icon;
                        if registered {
                            let _ = bridge_sender.send_blocking(BridgeEvent::Docked(client_window));
                        }

                        docked.push(DockedWindow {
                            client_window,
                            container_window: container,
                            damage_id,
                            dock_time: std::time::Instant::now(),
                            registered,
                        });
                    }
                }
                Event::DestroyNotify(event) => {
                    let window = event.window;
                    if let Some(position) = docked
                        .iter()
                        .position(|docked_window| docked_window.client_window == window)
                    {
                        let entry = docked.remove(position);
                        let _ = damage::destroy(&connection, entry.damage_id);
                        let _ = connection.destroy_window(entry.container_window);
                        let _ = connection.flush();
                        state.lock().unwrap().remove(&window);
                        if entry.registered {
                            let _ = bridge_sender.send_blocking(BridgeEvent::Undocked(window));
                        }
                        log::info!("Undocked window 0x{:x} (destroyed)", window);
                    }
                }
                Event::ReparentNotify(event) => {
                    let window = event.window;
                    if let Some(position) = docked.iter().position(|docked_window| {
                        docked_window.client_window == window
                            && event.parent != docked_window.container_window
                    }) {
                        let entry = docked.remove(position);
                        let _ = damage::destroy(&connection, entry.damage_id);
                        let _ = connection.destroy_window(entry.container_window);
                        let _ = connection.flush();
                        state.lock().unwrap().remove(&window);
                        if entry.registered {
                            let _ = bridge_sender.send_blocking(BridgeEvent::Undocked(window));
                        }
                        log::info!("Undocked window 0x{:x} (reparented away)", window);
                    }
                }
                Event::DamageNotify(event) => {
                    let drawable = event.drawable;
                    if let Some(entry) = docked
                        .iter_mut()
                        .find(|docked_window| docked_window.client_window == drawable)
                    {
                        let _ = damage::subtract(
                            &connection,
                            entry.damage_id,
                            x11rb::NONE,
                            x11rb::NONE,
                        );

                        log::debug!("Damage on 0x{:x}, capturing icon", drawable);

                        let captured = capture_icon(&connection, drawable);

                        if let Some(captured) = captured {
                            let state_guard = state.lock().unwrap();
                            let changed = state_guard
                                .get(&drawable)
                                .map(|icon_state| icon_state.pixels != captured.pixels)
                                .unwrap_or(true);

                            if changed {
                                drop(state_guard);
                                store_icon_state(&state, drawable, captured);

                                if !entry.registered {
                                    entry.registered = true;
                                    let _ =
                                        bridge_sender.send_blocking(BridgeEvent::Docked(drawable));
                                    log::info!(
                                        "Registered 0x{:x} after damage icon capture",
                                        drawable
                                    );
                                } else {
                                    let _ = bridge_sender
                                        .send_blocking(BridgeEvent::IconUpdated(drawable));
                                    log::info!(
                                        "Updated icon for 0x{:x} from damage event",
                                        drawable
                                    );
                                }
                            }
                        }
                    }
                }
                Event::Expose(_) => {}
                Event::PropertyNotify(event) => {
                    let window = event.window;
                    if docked
                        .iter()
                        .any(|docked_window| docked_window.client_window == window)
                        && (event.atom == atoms._NET_WM_ICON
                            || event.atom == AtomEnum::WM_HINTS.into())
                    {
                        let captured = capture_icon(&connection, window)
                            .or_else(|| capture_icon_from_hints(&connection, window))
                            .or_else(|| capture_icon_from_property(&connection, window, &atoms));

                        if let Some(captured) = captured {
                            let state_guard = state.lock().unwrap();
                            let changed = state_guard
                                .get(&window)
                                .map(|icon_state| icon_state.pixels != captured.pixels)
                                .unwrap_or(true);

                            if changed {
                                drop(state_guard);
                                store_icon_state(&state, window, captured);
                                let _ =
                                    bridge_sender.send_blocking(BridgeEvent::IconUpdated(window));
                                log::info!("Updated icon for 0x{:x} from property change", window);
                            }
                        }
                    }
                }
                _ => {}
            }
        } else {
            let needs_fast_capture = docked.iter().any(|docked_window| {
                docked_window.dock_time.elapsed() < std::time::Duration::from_secs(2)
            });
            let capture_interval = if needs_fast_capture {
                std::time::Duration::from_millis(200)
            } else {
                std::time::Duration::from_secs(2)
            };
            let remaining = capture_interval.saturating_sub(last_capture.elapsed());
            let timeout_ms = remaining
                .as_millis()
                .clamp(10, capture_interval.as_millis()) as i32;

            let mut poll_fd = libc::pollfd {
                fd: x11_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        }

        let needs_fast_capture = docked.iter().any(|docked_window| {
            docked_window.dock_time.elapsed() < std::time::Duration::from_secs(2)
        });
        let capture_interval = if needs_fast_capture {
            std::time::Duration::from_millis(200)
        } else {
            std::time::Duration::from_secs(2)
        };

        if last_capture.elapsed() >= capture_interval {
            last_capture = std::time::Instant::now();

            for entry in &mut docked {
                if let Some(captured) = capture_icon(&connection, entry.client_window) {
                    let state_guard = state.lock().unwrap();
                    let changed = state_guard
                        .get(&entry.client_window)
                        .map(|icon_state| icon_state.pixels != captured.pixels)
                        .unwrap_or(true);

                    if changed {
                        drop(state_guard);
                        store_icon_state(&state, entry.client_window, captured);

                        if !entry.registered {
                            entry.registered = true;
                            let _ = bridge_sender
                                .send_blocking(BridgeEvent::Docked(entry.client_window));
                        } else {
                            let _ = bridge_sender
                                .send_blocking(BridgeEvent::IconUpdated(entry.client_window));
                        }
                    }
                }
            }
        }
    }
}

fn store_icon_state(
    state: &Arc<Mutex<HashMap<u32, IconState>>>,
    window_id: u32,
    captured: CapturedIcon,
) {
    let mut state_guard = state.lock().unwrap();

    if state_guard.len() >= MAX_ICON_ENTRIES
        && let Some((oldest_window, _)) = state_guard
            .iter()
            .min_by_key(|(_, icon_state)| icon_state.last_updated)
            .map(|(window_id, icon_state)| (*window_id, icon_state.last_updated))
    {
        state_guard.remove(&oldest_window);
    }

    state_guard.insert(
        window_id,
        IconState {
            width: captured.width,
            height: captured.height,
            pixels: captured.pixels,
            last_updated: std::time::Instant::now(),
        },
    );
}

fn main() {
    env_logger::init();

    if std::env::var("DISPLAY").is_err() {
        log::info!("No DISPLAY set, XWayland not running. Exiting.");
        return;
    }

    let state: Arc<Mutex<HashMap<u32, IconState>>> = Arc::new(Mutex::new(HashMap::new()));
    let (bridge_sender, bridge_receiver) = smol::channel::unbounded::<BridgeEvent>();
    let (click_sender, click_receiver) = smol::channel::unbounded::<ClickEvent>();

    let x11_state = state.clone();
    std::thread::spawn(move || {
        if let Err(error) = run_x11_loop(x11_state, bridge_sender, click_receiver) {
            log::error!("X11 loop error: {}", error);
        }
    });

    smol::block_on(async {
        if let Err(error) = run_dbus_loop(state, bridge_receiver, click_sender).await {
            log::error!("D-Bus loop error: {}", error);
        }
    });
}

async fn run_dbus_loop(
    state: Arc<Mutex<HashMap<u32, IconState>>>,
    bridge_receiver: smol::channel::Receiver<BridgeEvent>,
    click_sender: smol::channel::Sender<ClickEvent>,
) -> anyhow::Result<()> {
    let connection = zbus::conn::Builder::session()?.build().await?;

    let watcher = StatusNotifierWatcherProxy::new(&connection).await?;

    log::info!("D-Bus connection established, waiting for dock events");

    let mut registered_paths: HashMap<u32, String> = HashMap::new();

    while let Ok(event) = bridge_receiver.recv().await {
        match event {
            BridgeEvent::Docked(window_id) => {
                let object_path = format!("/StatusNotifierItem/xembed_{:x}", window_id);

                let icon = BridgedIcon {
                    window_id,
                    state: state.clone(),
                    click_sender: click_sender.clone(),
                };

                if let Err(error) = connection
                    .object_server()
                    .at(object_path.as_str(), icon)
                    .await
                {
                    log::error!(
                        "Failed to serve SNI for window 0x{:x}: {}",
                        window_id,
                        error
                    );
                    continue;
                }

                if let Err(error) = watcher.register_status_notifier_item(&object_path).await {
                    log::error!(
                        "Failed to register SNI for window 0x{:x}: {}",
                        window_id,
                        error
                    );
                    continue;
                }

                registered_paths.insert(window_id, object_path.clone());
                log::info!(
                    "Registered SNI item xembed_{:x} at {}",
                    window_id,
                    object_path
                );
            }
            BridgeEvent::Undocked(window_id) => {
                if let Some(path) = registered_paths.remove(&window_id) {
                    let _ = watcher.unregister_status_notifier_item(&path).await;
                    let _ = connection
                        .object_server()
                        .remove::<BridgedIcon, _>(path.as_str())
                        .await;
                    log::info!("Unregistered SNI item xembed_{:x}", window_id);
                }
            }
            BridgeEvent::IconUpdated(window_id) => {
                if let Some(path) = registered_paths.get(&window_id) {
                    let object_server = connection.object_server();
                    let iface_ref = object_server
                        .interface::<_, BridgedIcon>(path.as_str())
                        .await;

                    if let Ok(iface_ref) = iface_ref {
                        let emitter = iface_ref.signal_emitter();
                        let _ = BridgedIcon::new_icon(emitter).await;
                    }

                    let _ = watcher.refresh_status_notifier_item(path).await;
                    log::debug!("Refreshed icon for xembed_{:x}", window_id);
                }
            }
        }
    }

    Ok(())
}
