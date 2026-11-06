//! Extract the native icon embedded in a tool's `.exe` (Windows) as a Slint image.
//! Cached by path. Returns `None` off Windows, for non-exe files, or when the file
//! carries no usable icon — the UI then falls back to the monogram tile.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use slint::Image;

thread_local! {
    static CACHE: RefCell<HashMap<String, Option<Image>>> = RefCell::new(HashMap::new());
}

/// The icon embedded in `exe`, cached. Only real `.exe` files carry one.
pub fn icon_for(exe: &Path) -> Option<Image> {
    let is_exe = exe.extension().map(|e| e.eq_ignore_ascii_case("exe")).unwrap_or(false);
    if !is_exe {
        return None;
    }
    let key = exe.to_string_lossy().to_ascii_lowercase();
    if let Some(cached) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return cached;
    }
    let img = extract(exe);
    CACHE.with(|c| c.borrow_mut().insert(key, img.clone()));
    img
}

#[cfg(windows)]
fn extract(exe: &Path) -> Option<Image> {
    use std::os::windows::ffi::OsStrExt;

    use slint::{Rgba8Pixel, SharedPixelBuffer};
    use windows_sys::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DestroyIcon, GetIconInfo, PrivateExtractIconsW, ICONINFO,
    };

    let wide: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let size = 64i32;

    unsafe {
        // Pull the best-matching 64×64 icon out of the executable.
        let mut hicon: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut icon_id: u32 = 0;
        let n = PrivateExtractIconsW(wide.as_ptr(), 0, size, size, &mut hicon, &mut icon_id, 1, 0);
        if n == 0 || hicon.is_null() {
            return None;
        }

        let mut ii = ICONINFO::default();
        if GetIconInfo(hicon, &mut ii) == 0 {
            DestroyIcon(hicon);
            return None;
        }

        // Size of the color bitmap.
        let mut bm = BITMAP::default();
        let got = GetObjectW(
            ii.hbmColor,
            std::mem::size_of::<BITMAP>() as i32,
            &mut bm as *mut _ as *mut core::ffi::c_void,
        );
        if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
            DeleteObject(ii.hbmColor);
            DeleteObject(ii.hbmMask);
            DestroyIcon(hicon);
            return None;
        }
        let w = bm.bmWidth;
        let h = bm.bmHeight;

        // Ask for 32-bit top-down BGRA pixels.
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h; // negative => top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;

        let mut buf = vec![0u8; (w * h * 4) as usize];
        let hdc = GetDC(std::ptr::null_mut());
        let scan = GetDIBits(
            hdc,
            ii.hbmColor,
            0,
            h as u32,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(std::ptr::null_mut(), hdc);
        DeleteObject(ii.hbmColor);
        DeleteObject(ii.hbmMask);
        DestroyIcon(hicon);
        if scan == 0 {
            return None;
        }

        // BGRA -> RGBA. If the icon has no alpha channel at all, make it opaque.
        let has_alpha = buf.chunks_exact(4).any(|px| px[3] != 0);
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            if !has_alpha {
                px[3] = 255;
            }
        }

        let mut pb = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
        pb.make_mut_bytes().copy_from_slice(&buf);
        Some(Image::from_rgba8(pb))
    }
}

#[cfg(not(windows))]
fn extract(_exe: &Path) -> Option<Image> {
    None
}
