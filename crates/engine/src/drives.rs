//! Detection of removable drives (USB sticks) so the app can find/select the stick.

/// A mounted drive that could hold the portable toolkit.
#[derive(Clone, Debug)]
pub struct DriveInfo {
    /// Root path, e.g. `E:\`.
    pub root: String,
    /// Volume label, e.g. `Lexar F35 PRO` (may be empty).
    pub label: String,
}

impl DriveInfo {
    /// Human label like `Lexar F35 PRO (E:)` or just `E:`.
    pub fn display(&self) -> String {
        let letter = self.root.trim_end_matches(['\\', '/']);
        if self.label.is_empty() {
            letter.to_string()
        } else {
            format!("{} ({})", self.label, letter)
        }
    }
}

/// List removable drives currently mounted (Windows). Empty on other platforms.
#[cfg(windows)]
pub fn list_removable() -> Vec<DriveInfo> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    const DRIVE_REMOVABLE: u32 = 2; // GetDriveType() return value for removable media

    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();

        if unsafe { GetDriveTypeW(wide.as_ptr()) } != DRIVE_REMOVABLE {
            continue;
        }

        let mut name = [0u16; 256];
        let ok = unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        };
        let label = if ok != 0 {
            let end = name.iter().position(|&c| c == 0).unwrap_or(0);
            String::from_utf16_lossy(&name[..end])
        } else {
            String::new()
        };
        out.push(DriveInfo { root, label });
    }
    out
}

#[cfg(not(windows))]
pub fn list_removable() -> Vec<DriveInfo> {
    Vec::new()
}

/// Free and total bytes for the volume that holds `root`, if obtainable.
#[cfg(windows)]
pub fn disk_usage(root: &std::path::Path) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    // GetDiskFreeSpaceExW wants a directory path; a drive root like `E:\` works.
    let mut s = root.to_string_lossy().into_owned();
    if !s.ends_with('\\') && !s.ends_with('/') {
        s.push('\\');
    }
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();

    let mut free_avail: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_avail,
            &mut total,
            &mut total_free,
        )
    };
    if ok != 0 && total > 0 {
        Some((free_avail, total))
    } else {
        None
    }
}

#[cfg(not(windows))]
pub fn disk_usage(_root: &std::path::Path) -> Option<(u64, u64)> {
    None
}
