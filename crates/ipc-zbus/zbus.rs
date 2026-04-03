use gpui::{App, AsyncApp, Context, WeakEntity};
use ipc::{MenuItem, TrayItem, TrayProvider};
use std::{
    collections::HashMap,
    ops::Deref,
    path::{Path, PathBuf},
};
use zbus::zvariant::OwnedValue;

type DBusMenuLayout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

#[zbus::proxy(
    interface = "org.kde.StatusNotifierItem",
    default_path = "/StatusNotifierItem"
)]
trait StatusNotifierItem {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn title(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn icon_name(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn icon_theme_path(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn icon_pixmap(&self) -> zbus::Result<Vec<(i32, i32, Vec<u8>)>>;

    #[zbus(property)]
    fn menu(&self) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;

    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;

    fn context_menu(&self, x: i32, y: i32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn new_icon(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn new_status(&self, status: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn new_title(&self) -> zbus::Result<()>;
}

#[zbus::proxy(interface = "com.canonical.dbusmenu")]
trait DBusMenu {
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<&str>,
    ) -> zbus::Result<(u32, DBusMenuLayout)>;

    fn event(
        &self,
        id: i32,
        event_id: &str,
        data: &zbus::zvariant::Value<'_>,
        timestamp: u32,
    ) -> zbus::Result<()>;

    fn about_to_show(&self, id: i32) -> zbus::Result<bool>;
}

struct Watcher {
    sender: smol::channel::Sender<WatcherEvent>,
}

enum WatcherEvent {
    Registered(String),
    Unregistered(String),
    Refresh(String),
}

fn resolve_service_address(service: &str, header: &zbus::message::Header<'_>) -> Option<String> {
    if service.starts_with('/') {
        header
            .sender()
            .map(|sender| format!("{}{}", sender, service))
    } else {
        Some(format!("{}/StatusNotifierItem", service))
    }
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        let Some(address) = resolve_service_address(service, &header) else {
            return;
        };

        log::info!("Tray item registered: {}", address);
        let _ = self
            .sender
            .send(WatcherEvent::Registered(address.clone()))
            .await;
        let _ = Self::status_notifier_item_registered(&emitter, &address).await;
    }

    async fn unregister_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        let Some(address) = resolve_service_address(service, &header) else {
            return;
        };

        log::info!("Tray item unregistered: {}", address);
        let _ = self
            .sender
            .send(WatcherEvent::Unregistered(address.clone()))
            .await;
        let _ = Self::status_notifier_item_unregistered(&emitter, &address).await;
    }

    async fn refresh_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let Some(address) = resolve_service_address(service, &header) else {
            return;
        };

        log::info!("Tray item refresh requested: {}", address);
        let _ = self.sender.send(WatcherEvent::Refresh(address)).await;
    }

    async fn register_status_notifier_host(
        &self,
        _service: &str,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        let _ = Self::status_notifier_host_registered(&emitter).await;
    }

    #[zbus(property)]
    async fn registered_status_notifier_items(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    async fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    async fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
}

const TRAY_ICON_CATEGORIES: &[&str] = &["apps", "status", "devices", "panel"];

struct IconDirectoryGuard {
    path: PathBuf,
}

impl Drop for IconDirectoryGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn clean_stale_icon_directories(runtime_path: &Path) {
    let Ok(entries) = std::fs::read_dir(runtime_path) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if let Some(process_id) = file_name
            .strip_prefix("ordinary-tray-icons-")
            .and_then(|value| value.parse::<i32>().ok())
            && process_id != std::process::id() as i32
            && !is_process_running(process_id)
        {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn is_process_running(process_id: i32) -> bool {
    if process_id <= 0 {
        return false;
    }
    let result = unsafe { libc::kill(process_id, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn pixmap_to_file(
    width: i32,
    height: i32,
    data: &[u8],
    id: &str,
    icon_directory: &Path,
) -> Option<PathBuf> {
    if width <= 0 || height <= 0 {
        return None;
    }

    let expected = (width as usize) * (height as usize) * 4;
    if data.len() < expected {
        return None;
    }

    let mut rgba = Vec::with_capacity(expected);
    for pixel in data[..expected].chunks_exact(4) {
        let alpha = pixel[0];
        let red = pixel[1];
        let green = pixel[2];
        let blue = pixel[3];
        rgba.extend_from_slice(&[red, green, blue, alpha]);
    }

    std::fs::create_dir_all(icon_directory).ok()?;

    let safe_id: String = id
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let path = icon_directory.join(format!("{}.png", safe_id));

    let file = std::fs::File::create(&path).ok()?;
    let mut encoder = png::Encoder::new(file, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&rgba).ok()?;

    Some(path)
}

pub struct SystemTray {
    items: Vec<TrayItem>,
    connection: Option<zbus::Connection>,
    icon_directory: PathBuf,
    _icon_directory_guard: IconDirectoryGuard,
}

impl SystemTray {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        let process_id = std::process::id();
        let runtime_path = PathBuf::from(runtime_dir);
        clean_stale_icon_directories(&runtime_path);
        let icon_directory = runtime_path.join(format!("ordinary-tray-icons-{}", process_id));
        let icon_directory_guard = IconDirectoryGuard {
            path: icon_directory.clone(),
        };

        cx.spawn(async move |this, cx| {
            if let Err(error) = Self::run(this, cx).await {
                log::error!("SystemTray error: {}", error);
            }
        })
        .detach();

        Self {
            items: Vec::new(),
            connection: None,
            icon_directory,
            _icon_directory_guard: icon_directory_guard,
        }
    }

    pub fn activate(&self, address: &str, cx: &mut Context<Self>) {
        TrayProvider::activate_at(self, address, 0, 0, cx);
    }

    pub fn context_menu(&self, address: &str, cx: &mut Context<Self>) {
        TrayProvider::context_menu_at(self, address, 0, 0, cx);
    }

    async fn run(this: WeakEntity<Self>, cx: &mut AsyncApp) -> anyhow::Result<()> {
        let (sender, receiver) = smol::channel::unbounded::<WatcherEvent>();

        let signal_sender = sender.clone();
        let watcher = Watcher { sender };

        let connection = zbus::conn::Builder::session()?
            .serve_at("/StatusNotifierWatcher", watcher)?
            .name("org.kde.StatusNotifierWatcher")?
            .build()
            .await?;

        let connection_clone = connection.clone();
        this.update(cx, |model, _cx| {
            model.connection = Some(connection_clone);
        })?;

        let pid = std::process::id();
        let host_name = format!("org.freedesktop.StatusNotifierHost-{}", pid);
        connection.request_name(host_name.as_str()).await?;

        log::info!("SystemTray: watcher and host registered on D-Bus");

        let dbus_proxy = zbus::fdo::DBusProxy::new(&connection).await?;
        let mut name_changed = dbus_proxy.receive_name_owner_changed().await?;

        let mut known_addresses: Vec<String> = Vec::new();

        loop {
            use futures_lite::StreamExt;

            enum Event {
                ItemRegistered(String),
                ItemUnregistered(String),
                ItemRefresh(String),
                NameChanged(zbus::fdo::NameOwnerChanged),
            }

            let event = futures_lite::future::or(
                async {
                    match receiver.recv().await {
                        Ok(WatcherEvent::Registered(address)) => Event::ItemRegistered(address),
                        Ok(WatcherEvent::Unregistered(address)) => Event::ItemUnregistered(address),
                        Ok(WatcherEvent::Refresh(address)) => Event::ItemRefresh(address),
                        Err(_) => futures_lite::future::pending().await,
                    }
                },
                async {
                    match name_changed.next().await {
                        Some(signal) => Event::NameChanged(signal),
                        None => futures_lite::future::pending().await,
                    }
                },
            )
            .await;

            match event {
                Event::ItemRegistered(address) => {
                    if known_addresses.contains(&address) {
                        continue;
                    }

                    let icon_directory =
                        this.update(cx, |model, _| model.icon_directory.clone())?;
                    let item = fetch_item(&connection, &address, &icon_directory).await;
                    if let Some(item) = item {
                        log::info!("Tray item added: {} (icon: {:?})", item.id, item.icon_path);
                        known_addresses.push(address.clone());

                        let signal_connection = connection.clone();
                        let signal_address = address.clone();
                        let sender = signal_sender.clone();
                        smol::spawn(async move {
                            let _ = watch_item_signals(&signal_connection, &signal_address, sender)
                                .await;
                        })
                        .detach();

                        this.update(cx, |model, cx| {
                            model.items.push(item);
                            cx.notify();
                        })?;
                    } else {
                        log::warn!("Failed to fetch tray item: {}", address);
                    }
                }
                Event::ItemUnregistered(address) => {
                    if known_addresses.contains(&address) {
                        known_addresses.retain(|a| a != &address);
                        log::info!("Tray item unregistered: {}", address);
                        this.update(cx, |model, cx| {
                            model.items.retain(|item| item.address != address);
                            cx.notify();
                        })?;
                    }
                }
                Event::ItemRefresh(address) => {
                    if known_addresses.contains(&address) {
                        let icon_directory =
                            this.update(cx, |model, _| model.icon_directory.clone())?;
                        let item = fetch_item(&connection, &address, &icon_directory).await;
                        if let Some(item) = item {
                            log::info!(
                                "Tray item refreshed: {} (icon: {:?})",
                                item.id,
                                item.icon_path
                            );
                            this.update(cx, |model, cx| {
                                if let Some(existing) = model
                                    .items
                                    .iter_mut()
                                    .find(|tray_item| tray_item.address == address)
                                {
                                    *existing = item;
                                }
                                cx.notify();
                            })?;
                        }
                    }
                }
                Event::NameChanged(signal) => {
                    let Ok(args) = signal.args() else {
                        continue;
                    };
                    let name = args.name.as_str();
                    let new_owner = args.new_owner.as_deref().unwrap_or("");

                    if new_owner.is_empty() {
                        let mut removed = Vec::new();
                        known_addresses.retain(|address| {
                            let bus_name = address.split('/').next().unwrap_or("");
                            if bus_name == name {
                                removed.push(address.clone());
                                false
                            } else {
                                true
                            }
                        });

                        for address in &removed {
                            log::info!("Tray item disconnected: {}", address);
                        }

                        if !removed.is_empty() {
                            this.update(cx, |model, cx| {
                                model.items.retain(|item| !removed.contains(&item.address));
                                cx.notify();
                            })?;
                        }
                    }
                }
            }
        }
    }
}

impl TrayProvider for SystemTray {
    fn items(&self) -> &[TrayItem] {
        &self.items
    }

    fn activate_at(&self, address: &str, x: i32, y: i32, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        let address = address.to_string();

        cx.spawn(async move |_this, _cx| {
            if let Err(error) = activate_item_at(&connection, &address, x, y).await {
                log::error!("Failed to activate tray item {}: {}", address, error);
            }
        })
        .detach();
    }

    fn context_menu_at(&self, address: &str, x: i32, y: i32, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        let address = address.to_string();

        cx.spawn(async move |_this, _cx| {
            if let Err(error) = context_menu_item_at(&connection, &address, x, y).await {
                log::error!(
                    "Failed to open context menu for tray item {}: {}",
                    address,
                    error
                );
            }
        })
        .detach();
    }

    fn fetch_menu(
        &self,
        address: &str,
        callback: Box<dyn FnOnce(Result<Vec<MenuItem>, String>, &mut App) + 'static>,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        let item = self
            .items
            .iter()
            .find(|tray_item| tray_item.address == address);
        let Some(menu_path) = item.and_then(|tray_item| tray_item.menu_path.clone()) else {
            log::warn!("No menu path for tray item: {}", address);
            return;
        };
        let bus_name = address.split('/').next().unwrap_or("").to_string();

        cx.spawn(async move |_this, cx| {
            let result = fetch_menu_items(&connection, &bus_name, &menu_path)
                .await
                .map_err(|error| error.to_string());
            if let Err(error) = &result {
                log::error!("Failed to fetch menu for {}: {}", bus_name, error);
            }
            cx.update(|cx| callback(result, cx));
        })
        .detach();
    }

    fn activate_menu_item(&self, address: &str, menu_item_id: i32, cx: &mut Context<Self>) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        let item = self
            .items
            .iter()
            .find(|tray_item| tray_item.address == address);
        let Some(menu_path) = item.and_then(|tray_item| tray_item.menu_path.clone()) else {
            return;
        };
        let bus_name = address.split('/').next().unwrap_or("").to_string();

        cx.spawn(async move |_this, _cx| {
            if let Err(error) =
                send_menu_event(&connection, &bus_name, &menu_path, menu_item_id).await
            {
                log::error!("Failed to activate menu item {}: {}", menu_item_id, error);
            }
        })
        .detach();
    }
}

async fn fetch_item(
    connection: &zbus::Connection,
    address: &str,
    icon_directory: &Path,
) -> Option<TrayItem> {
    let (bus_name, object_path) = address.split_once('/')?;
    let object_path = format!("/{}", object_path);

    let proxy = StatusNotifierItemProxy::builder(connection)
        .destination(bus_name.to_string())
        .ok()?
        .path(object_path)
        .ok()?
        .build()
        .await
        .ok()?;

    let id = proxy.id().await.unwrap_or_default();
    let title = proxy.title().await.unwrap_or_default();
    let status = proxy.status().await.unwrap_or_else(|_| "Active".into());
    let icon_name = proxy.icon_name().await.unwrap_or_default();
    let icon_theme_path = proxy.icon_theme_path().await.unwrap_or_default();

    let menu_path = proxy
        .menu()
        .await
        .ok()
        .map(|p| p.to_string())
        .filter(|p| !p.is_empty() && p != "/");

    let mut icon_path =
        ordinary_theme::resolve_icon(&icon_name, &icon_theme_path, TRAY_ICON_CATEGORIES);

    if icon_path.is_none()
        && let Ok(pixmaps) = proxy.icon_pixmap().await
    {
        let max_icon_size: i32 = 128;
        let capped = pixmaps.iter().filter(|(width, height, _)| {
            *width > 0 && *height > 0 && *width <= max_icon_size && *height <= max_icon_size
        });

        let selected = capped
            .max_by_key(|(width, height, _)| (*width as i64) * (*height as i64))
            .or_else(|| {
                pixmaps
                    .iter()
                    .max_by_key(|(width, height, _)| (*width as i64) * (*height as i64))
            });

        if let Some((width, height, data)) = selected {
            icon_path = pixmap_to_file(*width, *height, data, &id, icon_directory);
        }
    }

    log::debug!(
        "Fetched tray item: id={}, title={}, status={}, icon_name={}, icon_path={:?}, menu_path={:?}",
        id,
        title,
        status,
        icon_name,
        icon_path,
        menu_path
    );

    Some(TrayItem {
        id,
        title,
        status,
        icon_path,
        address: address.to_string(),
        menu_path,
    })
}

async fn activate_item_at(
    connection: &zbus::Connection,
    address: &str,
    x_position: i32,
    y_position: i32,
) -> anyhow::Result<()> {
    let Some((bus_name, object_path)) = address.split_once('/') else {
        anyhow::bail!("Invalid address: {}", address);
    };
    let object_path = format!("/{}", object_path);

    let proxy = StatusNotifierItemProxy::builder(connection)
        .destination(bus_name.to_string())?
        .path(object_path)?
        .build()
        .await?;

    proxy.activate(x_position, y_position).await?;
    Ok(())
}

async fn context_menu_item_at(
    connection: &zbus::Connection,
    address: &str,
    x_position: i32,
    y_position: i32,
) -> anyhow::Result<()> {
    let Some((bus_name, object_path)) = address.split_once('/') else {
        anyhow::bail!("Invalid address: {}", address);
    };
    let object_path = format!("/{}", object_path);

    let proxy = StatusNotifierItemProxy::builder(connection)
        .destination(bus_name.to_string())?
        .path(object_path)?
        .build()
        .await?;

    proxy.context_menu(x_position, y_position).await?;
    Ok(())
}

async fn fetch_menu_items(
    connection: &zbus::Connection,
    bus_name: &str,
    menu_path: &str,
) -> anyhow::Result<Vec<MenuItem>> {
    let proxy = DBusMenuProxy::builder(connection)
        .destination(bus_name.to_string())?
        .path(menu_path.to_string())?
        .build()
        .await?;

    let _ = proxy.about_to_show(0).await;

    let (_revision, layout) = proxy.get_layout(0, -1, vec![]).await?;
    let children = parse_menu_layout(layout);
    Ok(children)
}

fn property_string(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let value: &zbus::zvariant::Value = props.get(key)?.deref();
    value.downcast_ref::<String>().ok()
}

fn property_bool(props: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    let value: &zbus::zvariant::Value = props.get(key)?.deref();
    value.downcast_ref::<bool>().ok()
}

fn property_i32(props: &HashMap<String, OwnedValue>, key: &str) -> Option<i32> {
    let value: &zbus::zvariant::Value = props.get(key)?.deref();
    value.downcast_ref::<i32>().ok()
}

fn parse_menu_layout(layout: (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>)) -> Vec<MenuItem> {
    let (_id, _props, children_variants) = layout;

    children_variants
        .into_iter()
        .filter_map(|child_variant| {
            let child_value: zbus::zvariant::Value = child_variant.into();
            let zbus::zvariant::Value::Structure(structure) = child_value else {
                return None;
            };
            let fields = structure.into_fields();
            if fields.len() < 3 {
                return None;
            }

            let id: i32 = TryFrom::try_from(&fields[0]).ok()?;

            let props_value: zbus::zvariant::Value = fields[1].clone();
            let props: HashMap<String, OwnedValue> = match props_value {
                zbus::zvariant::Value::Dict(dict) => {
                    let map: HashMap<String, OwnedValue> = dict.try_into().ok()?;
                    map
                }
                _ => HashMap::new(),
            };

            let sub_children_value: zbus::zvariant::Value = fields[2].clone();
            let sub_children: Vec<OwnedValue> = match sub_children_value {
                zbus::zvariant::Value::Array(arr) => arr
                    .iter()
                    .map(|v| OwnedValue::try_from(v).ok())
                    .collect::<Option<Vec<_>>>()?,
                _ => Vec::new(),
            };

            let label = property_string(&props, "label")
                .unwrap_or_default()
                .replace('_', "");
            let enabled = property_bool(&props, "enabled").unwrap_or(true);
            let visible = property_bool(&props, "visible").unwrap_or(true);
            let item_type = property_string(&props, "type").unwrap_or_else(|| "standard".into());
            let is_separator = item_type == "separator";
            let toggle_type = property_string(&props, "toggle-type").unwrap_or_default();
            let toggle_state = property_i32(&props, "toggle-state").unwrap_or(0);

            let children = if !sub_children.is_empty() {
                parse_menu_layout((id, HashMap::new(), sub_children))
            } else {
                Vec::new()
            };

            Some(MenuItem {
                id,
                label,
                enabled,
                visible,
                is_separator,
                toggle_type,
                toggle_state,
                children,
            })
        })
        .collect()
}

async fn send_menu_event(
    connection: &zbus::Connection,
    bus_name: &str,
    menu_path: &str,
    item_id: i32,
) -> anyhow::Result<()> {
    let proxy = DBusMenuProxy::builder(connection)
        .destination(bus_name.to_string())?
        .path(menu_path.to_string())?
        .build()
        .await?;

    let data = zbus::zvariant::Value::I32(0);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as u32)
        .unwrap_or(0);

    proxy.event(item_id, "clicked", &data, timestamp).await?;
    Ok(())
}

/// Subscribe to NewIcon and NewStatus signals for a tray item and forward them
/// as refresh events so the main loop re-fetches the item's properties.
async fn watch_item_signals(
    connection: &zbus::Connection,
    address: &str,
    sender: smol::channel::Sender<WatcherEvent>,
) -> anyhow::Result<()> {
    use futures_lite::StreamExt;

    let Some((bus_name, object_path)) = address.split_once('/') else {
        anyhow::bail!("Invalid address: {}", address);
    };
    let object_path = format!("/{}", object_path);

    let proxy = StatusNotifierItemProxy::builder(connection)
        .destination(bus_name.to_string())?
        .path(object_path)?
        .build()
        .await?;

    let mut icon_stream = proxy.receive_new_icon().await?;
    let mut status_stream = proxy.receive_new_status().await?;

    let address = address.to_string();
    loop {
        let got_signal =
            futures_lite::future::or(async { icon_stream.next().await.map(|_| ()) }, async {
                status_stream.next().await.map(|_| ())
            })
            .await;

        if got_signal.is_none() {
            break;
        }

        log::debug!("Signal received for {}, requesting refresh", address);
        let _ = sender.send(WatcherEvent::Refresh(address.clone())).await;
    }

    Ok(())
}
