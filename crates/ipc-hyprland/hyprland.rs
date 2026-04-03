use gpui::{App, AsyncApp, Context, Entity, SharedString, Subscription, WeakEntity, prelude::*};
use ipc::{
    BarProvider, LauncherProvider, NoopBarProvider, NoopSpawner,
    WorkspaceState as SharedWorkspaceState,
};
use smol::{
    io::{AsyncBufReadExt, BufReader},
    stream::StreamExt,
};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

mod protocol {
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub struct Workspace {
        pub id: i32,
        pub windows: u32,
    }

    #[derive(Deserialize)]
    pub struct ActiveWorkspace {
        pub id: i32,
    }

    #[derive(Deserialize)]
    pub struct Keyboard {
        pub name: String,
        pub active_keymap: String,
    }

    #[derive(Deserialize)]
    pub struct Devices {
        pub keyboards: Vec<Keyboard>,
    }
}

#[derive(Debug, Clone)]
struct WorkspaceState {
    id: u32,
    active: bool,
    occupied: bool,
}

pub struct HyprlandIpc {
    workspaces: Vec<WorkspaceState>,
    layout: String,
    socket_dir: PathBuf,
}

impl HyprlandIpc {
    fn new(socket_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            let mut backoff = std::time::Duration::from_secs(1);
            loop {
                if this.upgrade().is_none() {
                    break;
                }
                if let Err(error) = Self::run(this.clone(), cx).await {
                    log::error!("HyprlandIpc error: {}", error);
                }
                if this.upgrade().is_none() {
                    break;
                }
                cx.background_executor().timer(backoff).await;
                let next_backoff_seconds = backoff.as_secs().saturating_mul(2).max(1);
                backoff = std::time::Duration::from_secs(next_backoff_seconds.min(30));
            }
        })
        .detach();

        Self {
            workspaces: Vec::new(),
            layout: String::new(),
            socket_dir,
        }
    }

    fn workspaces(&self) -> Vec<SharedWorkspaceState> {
        self.workspaces
            .iter()
            .map(|workspace| SharedWorkspaceState {
                id: workspace.id as u64,
                index: workspace.id,
                active: workspace.active,
                occupied: workspace.occupied,
            })
            .collect()
    }

    fn layout(&self) -> SharedString {
        self.layout.clone().into()
    }

    fn switch_workspace(&self, id: u64, cx: &mut Context<Self>) {
        let command_path = self.socket_dir.join(".socket.sock");

        cx.spawn(async move |_this, _cx| {
            if let Err(error) =
                send_command(&command_path, &format!("dispatch workspace {}", id)).await
            {
                log::error!("Failed to switch workspace: {}", error);
            }
        })
        .detach();
    }

    async fn run(this: WeakEntity<Self>, cx: &mut AsyncApp) -> anyhow::Result<()> {
        let socket_dir = this.update(cx, |model, _| model.socket_dir.clone())?;
        let command_path = socket_dir.join(".socket.sock");
        let event_path = socket_dir.join(".socket2.sock");

        Self::fetch_and_update(&this, cx, &command_path).await?;
        Self::fetch_layout(&this, cx, &command_path).await?;

        let stream = async_io::Async::new(UnixStream::connect(&event_path)?)?;
        let reader = BufReader::new(stream);
        let mut lines = reader.lines();

        while let Some(line) = lines.next().await {
            let line = line?;

            let Some((event, data)) = line.split_once(">>") else {
                continue;
            };

            match event {
                "workspace" | "workspacev2" | "createworkspace" | "createworkspacev2"
                | "destroyworkspace" | "destroyworkspacev2" | "moveworkspace"
                | "moveworkspacev2" | "renameworkspace" => {
                    if let Err(error) = Self::fetch_and_update(&this, cx, &command_path).await {
                        log::error!("Failed to refresh workspaces: {}", error);
                    }
                }
                "activelayout" => {
                    if let Some((_, layout)) = data.split_once(',') {
                        let layout = layout.to_string();
                        this.update(cx, |model, cx| {
                            model.layout = layout;
                            cx.notify();
                        })?;
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn fetch_layout(
        this: &WeakEntity<Self>,
        cx: &mut AsyncApp,
        command_path: &Path,
    ) -> anyhow::Result<()> {
        let devices_json = send_command(command_path, "j/devices").await?;
        let devices: protocol::Devices = serde_json::from_str(&devices_json)?;

        let layout = devices
            .keyboards
            .iter()
            .find(|keyboard| keyboard.name == "at-translated-set-2-keyboard")
            .or_else(|| devices.keyboards.first())
            .map(|keyboard| keyboard.active_keymap.clone())
            .unwrap_or_default();

        this.update(cx, |model, cx| {
            model.layout = layout;
            cx.notify();
        })?;

        Ok(())
    }

    async fn fetch_and_update(
        this: &WeakEntity<Self>,
        cx: &mut AsyncApp,
        command_path: &Path,
    ) -> anyhow::Result<()> {
        let workspaces_json = send_command(command_path, "j/workspaces").await?;
        let active_json = send_command(command_path, "j/activeworkspace").await?;

        let workspaces: Vec<protocol::Workspace> = serde_json::from_str(&workspaces_json)?;
        let active: protocol::ActiveWorkspace = serde_json::from_str(&active_json)?;

        let mut states: Vec<WorkspaceState> = workspaces
            .iter()
            .filter(|workspace| workspace.id > 0)
            .map(|workspace| WorkspaceState {
                id: workspace.id as u32,
                active: workspace.id == active.id,
                occupied: workspace.windows > 0,
            })
            .collect();

        states.sort_by_key(|workspace| workspace.id);

        this.update(cx, |model, cx| {
            model.workspaces = states;
            cx.notify();
        })?;

        Ok(())
    }
}

struct HyprlandBarProvider {
    model: Entity<HyprlandIpc>,
    _subscription: Subscription,
}

impl BarProvider for HyprlandBarProvider {
    fn switch_workspace(&self, id: u64, cx: &mut App) {
        self.model
            .update(cx, |ipc, cx| ipc.switch_workspace(id, cx));
    }
}

pub fn create_bar_provider<T>(
    cx: &mut Context<T>,
    on_update: impl Fn(&mut T, Vec<SharedWorkspaceState>, SharedString, &mut Context<T>) + 'static,
) -> (Box<dyn BarProvider>, bool)
where
    T: 'static,
{
    let on_update = std::rc::Rc::new(on_update);

    if let (Ok(signature), Ok(runtime_dir)) = (
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE"),
        std::env::var("XDG_RUNTIME_DIR"),
    ) {
        let socket_dir = PathBuf::from(runtime_dir).join("hypr").join(&signature);
        let model = cx.new(|cx| HyprlandIpc::new(socket_dir, cx));
        let update = on_update.clone();
        let subscription = cx.observe(&model, move |this: &mut T, model, cx| {
            let ipc = model.read(cx);
            update(this, ipc.workspaces(), ipc.layout(), cx);
            cx.notify();
        });
        (
            Box::new(HyprlandBarProvider {
                model,
                _subscription: subscription,
            }),
            false,
        )
    } else {
        log::warn!(
            "--compositor hyprland selected, but HYPRLAND_INSTANCE_SIGNATURE and/or XDG_RUNTIME_DIR are not set"
        );
        (Box::new(NoopBarProvider), false)
    }
}

struct HyprlandSpawner {
    socket_dir: PathBuf,
}

impl LauncherProvider for HyprlandSpawner {
    fn spawn(&self, command: &str) {
        let command_path = self.socket_dir.join(".socket.sock");
        if let Err(error) = send_command_sync(&command_path, &format!("dispatch exec {}", command))
        {
            log::error!("Failed to spawn: {}", error);
        }
    }
}

pub fn create_launcher_provider() -> std::rc::Rc<dyn LauncherProvider> {
    if let (Ok(signature), Ok(runtime_dir)) = (
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE"),
        std::env::var("XDG_RUNTIME_DIR"),
    ) {
        let socket_dir = PathBuf::from(runtime_dir).join("hypr").join(&signature);
        std::rc::Rc::new(HyprlandSpawner { socket_dir })
    } else {
        log::warn!(
            "--compositor hyprland selected, but HYPRLAND_INSTANCE_SIGNATURE and/or XDG_RUNTIME_DIR are not set"
        );
        std::rc::Rc::new(NoopSpawner)
    }
}

fn send_command_sync(path: &Path, command: &str) -> anyhow::Result<String> {
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(command.as_bytes())?;
    stream.flush()?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

async fn send_command(path: &Path, command: &str) -> anyhow::Result<String> {
    let path = path.to_path_buf();
    let command = command.to_string();
    smol::unblock(move || send_command_sync(&path, &command)).await
}
