pub fn now() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);

        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}
