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
    io::Write,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

mod protocol {
    use serde::{Deserialize, Serialize};

    #[derive(Serialize)]
    pub enum Request {
        EventStream,
        Action(Action),
    }

    #[derive(Serialize)]
    pub enum Action {
        FocusWorkspace { reference: WorkspaceReference },
        Spawn { command: Vec<String> },
    }

    #[derive(Serialize)]
    pub enum WorkspaceReference {
        Id(u64),
    }

    pub type Reply = Result<Response, String>;

    #[derive(Deserialize)]
    pub enum Response {
        Handled,
        #[serde(other)]
        Other,
    }

    #[derive(Deserialize)]
    pub struct WorkspacesChangedEvent {
        pub workspaces: Vec<Workspace>,
    }

    #[derive(Deserialize)]
    pub struct WorkspaceActivatedEvent {
        pub id: u64,
        pub focused: bool,
    }

    #[derive(Deserialize)]
    pub struct KeyboardLayouts {
        pub names: Vec<String>,
        pub current_idx: u8,
    }

    #[derive(Deserialize)]
    pub struct KeyboardLayoutsChangedEvent {
        pub keyboard_layouts: KeyboardLayouts,
    }

    #[derive(Deserialize)]
    pub struct KeyboardLayoutSwitchedEvent {
        pub idx: u8,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    #[allow(dead_code)]
    pub enum Event {
        WorkspacesChanged {
            #[serde(rename = "WorkspacesChanged")]
            data: WorkspacesChangedEvent,
        },
        WorkspaceActivated {
            #[serde(rename = "WorkspaceActivated")]
            data: WorkspaceActivatedEvent,
        },
        KeyboardLayoutsChanged {
            #[serde(rename = "KeyboardLayoutsChanged")]
            data: KeyboardLayoutsChangedEvent,
        },
        KeyboardLayoutSwitched {
            #[serde(rename = "KeyboardLayoutSwitched")]
            data: KeyboardLayoutSwitchedEvent,
        },
        Other(serde_json::Value),
    }

    #[derive(Deserialize)]
    pub struct Workspace {
        pub id: u64,
        #[serde(rename = "idx")]
        pub index: u8,
        pub is_focused: bool,
    }
}

#[derive(Debug, Clone)]
struct WorkspaceState {
    id: u64,
    index: u8,
    active: bool,
}

pub struct NiriIpc {
    workspaces: Vec<WorkspaceState>,
    layout: String,
    layout_names: Vec<String>,
    socket_path: PathBuf,
}

impl NiriIpc {
    fn new(socket_path: PathBuf, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            let mut backoff = std::time::Duration::from_secs(1);
            loop {
                if this.upgrade().is_none() {
                    break;
                }
                if let Err(error) = Self::run(this.clone(), cx).await {
                    log::error!("NiriIpc error: {}", error);
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
            layout_names: Vec::new(),
            socket_path,
        }
    }

    fn workspaces(&self) -> Vec<SharedWorkspaceState> {
        self.workspaces
            .iter()
            .map(|workspace| SharedWorkspaceState {
                id: workspace.id,
                index: workspace.index as u32,
                active: workspace.active,
                occupied: true,
            })
            .collect()
    }

    fn layout(&self) -> SharedString {
        self.layout.clone().into()
    }

    fn switch_workspace(&self, id: u64, cx: &mut Context<Self>) {
        let socket_path = self.socket_path.clone();

        cx.spawn(async move |_this, _cx| {
            if let Err(error) = send_request(
                &socket_path,
                protocol::Action::FocusWorkspace {
                    reference: protocol::WorkspaceReference::Id(id),
                },
            )
            .await
            {
                log::error!("Failed to switch workspace: {}", error);
            }
        })
        .detach();
    }

    async fn run(this: WeakEntity<Self>, cx: &mut AsyncApp) -> anyhow::Result<()> {
        let socket_path = this.update(cx, |model, _| model.socket_path.clone())?;

        let stream = smol::unblock({
            let socket_path = socket_path.clone();
            move || -> anyhow::Result<UnixStream> {
                let mut stream = UnixStream::connect(&socket_path)?;
                let mut buffer = serde_json::to_string(&protocol::Request::EventStream)?;
                buffer.push('\n');
                stream.write_all(buffer.as_bytes())?;
                stream.flush()?;
                Ok(stream)
            }
        })
        .await?;

        let stream = async_io::Async::new(stream)?;
        let reader = BufReader::new(stream);
        let mut lines = reader.lines();

        let first_line = lines
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("event stream closed"))??;
        let reply: protocol::Reply = serde_json::from_str(&first_line)?;
        reply.map_err(|error| anyhow::anyhow!("niri error: {}", error))?;

        while let Some(line) = lines.next().await {
            let line = line?;
            let event: protocol::Event = serde_json::from_str(&line)?;

            match event {
                protocol::Event::WorkspacesChanged { data } => {
                    let states = build_workspace_states(&data.workspaces);

                    this.update(cx, |model, cx| {
                        model.workspaces = states;
                        cx.notify();
                    })?;
                }
                protocol::Event::WorkspaceActivated { data } => {
                    if data.focused {
                        this.update(cx, |model, cx| {
                            for workspace in &mut model.workspaces {
                                workspace.active = workspace.id == data.id;
                            }
                            cx.notify();
                        })?;
                    }
                }
                protocol::Event::KeyboardLayoutsChanged { data } => {
                    this.update(cx, |model, cx| {
                        model.layout_names = data.keyboard_layouts.names;
                        model.layout = current_layout_label(
                            &model.layout_names,
                            data.keyboard_layouts.current_idx,
                        );
                        cx.notify();
                    })?;
                }
                protocol::Event::KeyboardLayoutSwitched { data } => {
                    this.update(cx, |model, cx| {
                        model.layout = current_layout_label(&model.layout_names, data.idx);
                        cx.notify();
                    })?;
                }
                protocol::Event::Other(_) => {}
            }
        }

        Ok(())
    }
}

struct NiriBarProvider {
    model: Entity<NiriIpc>,
    _subscription: Subscription,
}

impl BarProvider for NiriBarProvider {
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

    if let Ok(socket_path) = std::env::var("NIRI_SOCKET") {
        let socket_path = PathBuf::from(socket_path);
        let model = cx.new(|cx| NiriIpc::new(socket_path, cx));
        let update = on_update.clone();
        let subscription = cx.observe(&model, move |this: &mut T, model, cx| {
            let ipc = model.read(cx);
            update(this, ipc.workspaces(), ipc.layout(), cx);
            cx.notify();
        });
        (
            Box::new(NiriBarProvider {
                model,
                _subscription: subscription,
            }),
            true,
        )
    } else {
        log::warn!("--compositor niri selected, but NIRI_SOCKET is not set");
        (Box::new(NoopBarProvider), true)
    }
}

struct NiriSpawner {
    socket_path: PathBuf,
}

impl LauncherProvider for NiriSpawner {
    fn spawn(&self, command: &str) {
        let command_parts = shell_words::split(command)
            .unwrap_or_else(|_| command.split_whitespace().map(str::to_string).collect());
        if let Err(error) = send_request_sync(
            &self.socket_path,
            protocol::Action::Spawn {
                command: command_parts,
            },
        ) {
            log::error!("Failed to spawn: {}", error);
        }
    }
}

pub fn create_launcher_provider() -> std::rc::Rc<dyn LauncherProvider> {
    if let Ok(socket_path) = std::env::var("NIRI_SOCKET") {
        std::rc::Rc::new(NiriSpawner {
            socket_path: PathBuf::from(socket_path),
        })
    } else {
        log::warn!("--compositor niri selected, but NIRI_SOCKET is not set");
        std::rc::Rc::new(NoopSpawner)
    }
}

fn build_workspace_states(workspaces: &[protocol::Workspace]) -> Vec<WorkspaceState> {
    let mut states: Vec<WorkspaceState> = workspaces
        .iter()
        .map(|workspace| WorkspaceState {
            id: workspace.id,
            index: workspace.index,
            active: workspace.is_focused,
        })
        .collect();

    states.sort_by_key(|workspace| workspace.index);
    states
}

fn current_layout_label(names: &[String], index: u8) -> String {
    names
        .get(index as usize)
        .cloned()
        .unwrap_or_default()
        .to_uppercase()
}

fn send_request_sync(path: &Path, action: protocol::Action) -> anyhow::Result<protocol::Response> {
    let mut stream = UnixStream::connect(path)?;

    let mut buffer = serde_json::to_string(&protocol::Request::Action(action))?;
    buffer.push('\n');
    stream.write_all(buffer.as_bytes())?;
    stream.flush()?;

    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut line)?;

    let reply: protocol::Reply = serde_json::from_str(&line)?;
    reply.map_err(|error| anyhow::anyhow!("niri error: {}", error))
}

async fn send_request(path: &Path, action: protocol::Action) -> anyhow::Result<protocol::Response> {
    let path = path.to_path_buf();
    smol::unblock(move || send_request_sync(&path, action)).await
}
