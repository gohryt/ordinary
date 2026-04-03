use gpui::{App, Context, Entity, SharedString, Subscription, prelude::*};
use ipc::{
    BarProvider, LauncherProvider, NoopBarProvider, NoopSpawner,
    WorkspaceState as SharedWorkspaceState,
};
use std::{
    collections::HashMap,
    process::Command,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};
use wayland_backend::client::ObjectId;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_output, wl_registry, wl_seat},
};

mod river_status {
    use wayland_client::protocol::__interfaces::*;

    wayland_scanner::generate_interfaces!("../../protocols/river-status-unstable-v1.xml");

    pub mod client {
        use super::*;
        use wayland_client;
        use wayland_client::protocol::wl_output;
        use wayland_client::protocol::wl_seat;

        wayland_scanner::generate_client_code!("../../protocols/river-status-unstable-v1.xml");
    }
}

use river_status::client::{
    zriver_output_status_v1::{self, ZriverOutputStatusV1},
    zriver_seat_status_v1::{self, ZriverSeatStatusV1},
    zriver_status_manager_v1::ZriverStatusManagerV1,
};

#[derive(Clone, Debug)]
struct RiverSnapshot {
    focused_tags: u32,
    view_tags: u32,
    layout: String,
}

impl Default for RiverSnapshot {
    fn default() -> Self {
        Self {
            focused_tags: 1,
            view_tags: 0,
            layout: String::new(),
        }
    }
}

pub struct RiverIpc {
    snapshot: RiverSnapshot,
    receiver: mpsc::Receiver<RiverSnapshot>,
    running: Arc<AtomicBool>,
}

impl RiverIpc {
    fn new(cx: &mut Context<Self>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let running_thread = running.clone();

        std::thread::spawn(move || {
            let mut backoff = Duration::from_secs(1);
            while running_thread.load(Ordering::Relaxed) {
                if let Err(error) = run_status_loop(sender.clone(), &running_thread) {
                    if !running_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    log::error!("River status loop error: {}", error);
                }
                if !running_thread.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(backoff);
                let next = (backoff.as_secs().saturating_mul(2)).clamp(1, 30);
                backoff = Duration::from_secs(next);
            }
        });

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;

                let result = this.update(cx, |this, cx| {
                    let mut changed = false;
                    while let Ok(snapshot) = this.receiver.try_recv() {
                        this.snapshot = snapshot;
                        changed = true;
                    }
                    if changed {
                        cx.notify();
                    }
                });

                if result.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            snapshot: RiverSnapshot::default(),
            receiver,
            running,
        }
    }

    fn workspaces(&self) -> Vec<SharedWorkspaceState> {
        tags_to_workspace_states(self.snapshot.focused_tags, self.snapshot.view_tags)
    }

    fn layout(&self) -> SharedString {
        self.snapshot.layout.clone().into()
    }

    fn switch_workspace(&self, id: u64, _cx: &mut Context<Self>) {
        let Some(tags) = workspace_id_to_tag_mask(id) else {
            return;
        };

        if let Err(error) =
            spawn_and_reap(Command::new("riverctl").args(["set-focused-tags", &tags.to_string()]))
        {
            log::error!("Failed to switch workspace: {}", error);
        }
    }
}

impl Drop for RiverIpc {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

struct RiverBarProvider {
    model: Entity<RiverIpc>,
    _subscription: Subscription,
}

impl BarProvider for RiverBarProvider {
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
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        log::warn!("--compositor river selected, but WAYLAND_DISPLAY is not set");
        return (Box::new(NoopBarProvider), true);
    }

    let on_update = Rc::new(on_update);
    let model = cx.new(RiverIpc::new);
    let update = on_update.clone();
    let subscription = cx.observe(&model, move |this: &mut T, model, cx| {
        let ipc = model.read(cx);
        update(this, ipc.workspaces(), ipc.layout(), cx);
        cx.notify();
    });

    (
        Box::new(RiverBarProvider {
            model,
            _subscription: subscription,
        }),
        false,
    )
}

struct RiverSpawner;

impl LauncherProvider for RiverSpawner {
    fn spawn(&self, command: &str) {
        if let Err(error) = spawn_and_reap(Command::new("riverctl").args(["spawn", command])) {
            log::error!("Failed to spawn: {}", error);
        }
    }
}

pub fn create_launcher_provider() -> Rc<dyn LauncherProvider> {
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        log::warn!("--compositor river selected, but WAYLAND_DISPLAY is not set");
        Rc::new(NoopSpawner)
    } else {
        Rc::new(RiverSpawner)
    }
}

#[derive(Default)]
struct OutputState {
    focused_tags: u32,
    view_tags: u32,
    layout: String,
}

struct RiverStatusState {
    sender: mpsc::Sender<RiverSnapshot>,
    manager: Option<ZriverStatusManagerV1>,
    manager_global_name: Option<u32>,
    seat: Option<wl_seat::WlSeat>,
    seat_global_name: Option<u32>,
    seat_status: Option<ZriverSeatStatusV1>,
    outputs: HashMap<ObjectId, wl_output::WlOutput>,
    output_global_names: HashMap<u32, ObjectId>,
    output_statuses: HashMap<ObjectId, ZriverOutputStatusV1>,
    output_state: HashMap<ObjectId, OutputState>,
    focused_output: Option<ObjectId>,
}

impl RiverStatusState {
    fn new(sender: mpsc::Sender<RiverSnapshot>) -> Self {
        Self {
            sender,
            manager: None,
            manager_global_name: None,
            seat: None,
            seat_global_name: None,
            seat_status: None,
            outputs: HashMap::new(),
            output_global_names: HashMap::new(),
            output_statuses: HashMap::new(),
            output_state: HashMap::new(),
            focused_output: None,
        }
    }

    fn bind_global(
        &mut self,
        registry: &wl_registry::WlRegistry,
        name: u32,
        interface: &str,
        version: u32,
        qh: &QueueHandle<Self>,
    ) {
        match interface {
            "zriver_status_manager_v1" => {
                let manager = registry.bind::<ZriverStatusManagerV1, _, _>(
                    name,
                    version
                        .min(2)
                        .min(ZriverStatusManagerV1::interface().version),
                    qh,
                    (),
                );
                self.manager = Some(manager);
                self.manager_global_name = Some(name);
                self.bind_status_objects(qh);
            }
            "wl_seat" => {
                let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, 1, qh, ());
                self.seat = Some(seat);
                self.seat_global_name = Some(name);
                self.bind_status_objects(qh);
            }
            "wl_output" => {
                let output = registry.bind::<wl_output::WlOutput, _, _>(name, 1, qh, ());
                let output_id = output.id();
                self.outputs.insert(output_id.clone(), output);
                self.output_global_names.insert(name, output_id);
                self.bind_status_objects(qh);
            }
            _ => {}
        }
    }

    fn bind_status_objects(&mut self, qh: &QueueHandle<Self>) {
        let Some(manager) = self.manager.as_ref() else {
            return;
        };

        if self.seat_status.is_none()
            && let Some(seat) = self.seat.as_ref()
        {
            let seat_status = manager.get_river_seat_status(seat, qh, ());
            self.seat_status = Some(seat_status);
        }

        for (id, output) in &self.outputs {
            if self.output_statuses.contains_key(id) {
                continue;
            }

            let status = manager.get_river_output_status(output, qh, id.clone());
            self.output_statuses.insert(id.clone(), status);
            self.output_state.entry(id.clone()).or_default();
        }
    }

    fn ensure_output_status_for_output(
        &mut self,
        output: &wl_output::WlOutput,
        qh: &QueueHandle<Self>,
    ) {
        let Some(manager) = self.manager.as_ref() else {
            return;
        };

        let output_id = output.id();
        if self.output_statuses.contains_key(&output_id) {
            return;
        }

        let status = manager.get_river_output_status(output, qh, output_id.clone());
        self.output_statuses.insert(output_id.clone(), status);
        self.output_state.entry(output_id).or_default();
    }

    fn emit_snapshot(&self) {
        let selected = self
            .focused_output
            .as_ref()
            .and_then(|id| self.output_state.get(id))
            .or_else(|| self.output_state.values().next());

        if let Some(state) = selected {
            let _ = self.sender.send(RiverSnapshot {
                focused_tags: if state.focused_tags == 0 {
                    1
                } else {
                    state.focused_tags
                },
                view_tags: state.view_tags,
                layout: state.layout.clone(),
            });
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for RiverStatusState {
    fn event(
        this: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                this.bind_global(registry, name, &interface, version, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if this.manager_global_name == Some(name) {
                    this.manager = None;
                    this.manager_global_name = None;
                }
                if this.seat_global_name == Some(name) {
                    this.seat = None;
                    this.seat_global_name = None;
                    this.seat_status = None;
                }
                if let Some(output_id) = this.output_global_names.remove(&name) {
                    this.outputs.remove(&output_id);
                    this.output_statuses.remove(&output_id);
                    this.output_state.remove(&output_id);
                    if this.focused_output == Some(output_id.clone()) {
                        this.focused_output = None;
                    }
                    this.emit_snapshot();
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(RiverStatusState: ignore wl_seat::WlSeat);
delegate_noop!(RiverStatusState: ignore wl_output::WlOutput);
delegate_noop!(RiverStatusState: ignore ZriverStatusManagerV1);

impl Dispatch<ZriverSeatStatusV1, ()> for RiverStatusState {
    fn event(
        this: &mut Self,
        _: &ZriverSeatStatusV1,
        event: zriver_seat_status_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zriver_seat_status_v1::Event::FocusedOutput { output } => {
                this.ensure_output_status_for_output(&output, qh);
                this.focused_output = Some(output.id());
                this.emit_snapshot();
            }
            zriver_seat_status_v1::Event::UnfocusedOutput { output } => {
                if this.focused_output == Some(output.id()) {
                    this.focused_output = None;
                    this.emit_snapshot();
                }
            }
            zriver_seat_status_v1::Event::FocusedView { title: _ } => {}
            zriver_seat_status_v1::Event::Mode { name: _ } => {}
        }
    }
}

impl Dispatch<ZriverOutputStatusV1, ObjectId> for RiverStatusState {
    fn event(
        this: &mut Self,
        _: &ZriverOutputStatusV1,
        event: zriver_output_status_v1::Event,
        output_id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let state = this.output_state.entry(output_id.clone()).or_default();

        match event {
            zriver_output_status_v1::Event::FocusedTags { tags } => {
                state.focused_tags = tags;
                this.emit_snapshot();
            }
            zriver_output_status_v1::Event::ViewTags { tags } => {
                state.view_tags = decode_view_tags_mask(&tags);
                this.emit_snapshot();
            }
            zriver_output_status_v1::Event::UrgentTags { tags: _ } => {}
            zriver_output_status_v1::Event::LayoutName { name } => {
                state.layout = name;
                this.emit_snapshot();
            }
            zriver_output_status_v1::Event::LayoutNameClear => {
                state.layout.clear();
                this.emit_snapshot();
            }
        }
    }
}

fn run_status_loop(
    sender: mpsc::Sender<RiverSnapshot>,
    running: &AtomicBool,
) -> anyhow::Result<()> {
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<RiverStatusState>(&connection)?;
    let qh = queue.handle();

    let mut state = RiverStatusState::new(sender);

    for global in globals.contents().clone_list() {
        state.bind_global(
            globals.registry(),
            global.name,
            &global.interface,
            global.version,
            &qh,
        );
    }

    queue.roundtrip(&mut state)?;

    loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }
        dispatch_with_timeout(&mut queue, &mut state, Duration::from_millis(250))?;
    }

    Ok(())
}

fn dispatch_with_timeout(
    queue: &mut EventQueue<RiverStatusState>,
    state: &mut RiverStatusState,
    timeout: Duration,
) -> anyhow::Result<()> {
    let dispatched = queue.dispatch_pending(state)?;
    if dispatched > 0 {
        return Ok(());
    }

    queue.flush()?;

    if let Some(guard) = queue.prepare_read() {
        let mut fds = [rustix::event::PollFd::new(
            queue,
            rustix::event::PollFlags::IN | rustix::event::PollFlags::ERR,
        )];
        let timeout_ts = rustix::event::Timespec::try_from(timeout).ok();

        let poll_result = loop {
            match rustix::event::poll(&mut fds, timeout_ts.as_ref()) {
                Ok(result) => break result as i32,
                Err(error) => {
                    if error == rustix::io::Errno::INTR {
                        continue;
                    }
                    return Err(std::io::Error::from(error).into());
                }
            }
        };

        if poll_result > 0
            && let Err(error) = guard.read()
        {
            // Guard against races where poll/read disagrees; try again next tick.
            if !matches!(
                error,
                wayland_backend::client::WaylandError::Io(ref io_error)
                    if io_error.kind() == std::io::ErrorKind::WouldBlock
            ) {
                return Err(error.into());
            }
        }
    }

    queue.dispatch_pending(state)?;
    Ok(())
}

fn spawn_and_reap(command: &mut Command) -> std::io::Result<()> {
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn workspace_id_to_tag_mask(id: u64) -> Option<u32> {
    let bit_index = id.checked_sub(1)?;
    if bit_index >= 32 {
        return None;
    }

    Some(1_u32 << bit_index)
}

fn tags_to_workspace_states(focused_tags: u32, occupied_tags: u32) -> Vec<SharedWorkspaceState> {
    (0..9)
        .map(|index| {
            let bit = 1_u32 << index;
            SharedWorkspaceState {
                id: (index + 1) as u64,
                index: (index + 1) as u32,
                active: (focused_tags & bit) != 0,
                occupied: (occupied_tags & bit) != 0,
            }
        })
        .collect()
}

fn decode_view_tags_mask(raw: &[u8]) -> u32 {
    raw.chunks_exact(4).fold(0_u32, |mask, chunk| {
        let tags = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        mask | tags
    })
}

#[cfg(test)]
mod tests {
    use super::{decode_view_tags_mask, tags_to_workspace_states, workspace_id_to_tag_mask};

    #[test]
    fn workspace_id_maps_to_expected_tag_mask() {
        assert_eq!(workspace_id_to_tag_mask(1), Some(1));
        assert_eq!(workspace_id_to_tag_mask(2), Some(2));
        assert_eq!(workspace_id_to_tag_mask(9), Some(256));
        assert_eq!(workspace_id_to_tag_mask(0), None);
    }

    #[test]
    fn tags_map_to_workspace_states() {
        let states = tags_to_workspace_states(0b0000_0101, 0b0000_1110);

        assert!(states[0].active);
        assert!(!states[1].active);
        assert!(states[2].active);

        assert!(!states[0].occupied);
        assert!(states[1].occupied);
        assert!(states[2].occupied);
        assert!(states[3].occupied);
    }

    #[test]
    fn view_tags_array_decodes_to_union_mask() {
        let bytes = [
            0b0000_0010_u8,
            0,
            0,
            0, // view 1: tag 2
            0b0000_1000_u8,
            0,
            0,
            0, // view 2: tag 4
        ];
        assert_eq!(decode_view_tags_mask(&bytes), 0b0000_1010);
    }
}
