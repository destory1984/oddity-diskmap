//! Small Win32 helpers.

use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetLogicalDrives, GetVolumeInformationW,
};
use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[derive(Clone, Debug)]
pub struct Drive {
    pub root: String,
    pub fs: String,
    pub free: u64,
    pub total: u64,
}

pub fn drives() -> Vec<Drive> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| {
            let root = format!("{}:\\", (b'A' + i as u8) as char);
            let (free, total) = disk_space(&root);
            Drive { fs: fs_name(&root), root, free, total }
        })
        .filter(|d| d.total > 0)
        .collect()
}

pub fn fs_name(root: &str) -> String {
    let w = wide(root);
    let mut buf = [0u16; 32];
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    if ok == 0 {
        return String::new();
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// (free bytes available, total bytes) for the volume holding `path`.
pub fn disk_space(path: &str) -> (u64, u64) {
    let w = wide(path);
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, &mut total, &mut total_free) };
    (free, total)
}

pub fn cluster_size(root: &str) -> u64 {
    let w = wide(root);
    let (mut spc, mut bps, mut a, mut b) = (0u32, 0u32, 0u32, 0u32);
    let ok = unsafe { GetDiskFreeSpaceW(w.as_ptr(), &mut spc, &mut bps, &mut a, &mut b) };
    if ok == 0 { 4096 } else { spc as u64 * bps as u64 }
}

pub fn is_admin() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

/// Start this program again with the UAC prompt. Returns true if the new process launched.
pub fn relaunch_as_admin(args: &str) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let exe = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let args = wide(args);
    let h = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            args.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    h as isize > 32
}

pub fn show_in_explorer(path: &str) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{path}"))
        .spawn();
}

pub fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE,
    };
    // Output already redirected to a file or pipe: keep it, attaching would replace it.
    let h = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if h.is_null() || h as isize == -1 {
        unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    }
}
