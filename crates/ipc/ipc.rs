use gpui::{App, Context};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WorkspaceState {
    pub id: u64,
    pub index: u32,
    pub active: bool,
    pub occupied: bool,
}

pub trait BarProvider {
    fn switch_workspace(&self, id: u64, cx: &mut App);
}

pub trait LauncherProvider {
    fn spawn(&self, command: &str);
}

#[derive(Clone, Debug)]
pub struct TrayItem {
    pub id: String,
    pub title: String,
    pub status: String,
    pub icon_path: Option<PathBuf>,
    pub address: String,
    pub menu_path: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub is_separator: bool,
    pub toggle_type: String,
    pub toggle_state: i32,
    pub children: Vec<MenuItem>,
}

pub type MenuFetchResult = Result<Vec<MenuItem>, String>;
pub type MenuFetchCallback = Box<dyn FnOnce(MenuFetchResult, &mut App) + 'static>;

pub struct NoopBarProvider;

impl BarProvider for NoopBarProvider {
    fn switch_workspace(&self, _id: u64, _cx: &mut App) {}
}

pub struct NoopSpawner;

impl LauncherProvider for NoopSpawner {
    fn spawn(&self, _command: &str) {}
}

pub trait TrayProvider: Sized {
    fn items(&self) -> &[TrayItem];
    fn activate_at(&self, address: &str, x: i32, y: i32, cx: &mut Context<Self>);
    fn context_menu_at(&self, address: &str, x: i32, y: i32, cx: &mut Context<Self>);
    fn fetch_menu(&self, address: &str, callback: MenuFetchCallback, cx: &mut Context<Self>);
    fn activate_menu_item(&self, address: &str, menu_item_id: i32, cx: &mut Context<Self>);
}
