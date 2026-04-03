use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct BatteryInfo {
    pub capacity: u8,
    pub charging: bool,
}

pub fn find_battery() -> Option<PathBuf> {
    let power_supply = Path::new("/sys/class/power_supply");
    let entries = fs::read_dir(power_supply).ok()?;

    for entry in entries.flatten() {
        let path = entry.path();
        let type_path = path.join("type");

        if let Ok(supply_type) = fs::read_to_string(&type_path)
            && supply_type.trim() == "Battery"
            && path.join("capacity").exists()
        {
            return Some(path);
        }
    }

    None
}

pub fn read(path: &Path) -> Option<BatteryInfo> {
    let capacity = fs::read_to_string(path.join("capacity"))
        .ok()?
        .trim()
        .parse()
        .ok()?;

    let status = fs::read_to_string(path.join("status"))
        .ok()
        .unwrap_or_default();

    let charging = matches!(status.trim(), "Charging" | "Full");

    Some(BatteryInfo { capacity, charging })
}
