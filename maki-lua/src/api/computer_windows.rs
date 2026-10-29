use std::io;
use std::mem::size_of;
use std::ptr::null_mut;
use std::slice;

use image::RgbaImage;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC,
    SRCCOPY, SelectObject,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const PIXEL_BYTES: usize = 4;
const BITS_PER_PIXEL: u16 = 32;
const INVALID_DESKTOP: &str = "Windows has no capturable interactive desktop";

struct DpiContext(DPI_AWARENESS_CONTEXT);

impl Drop for DpiContext {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}

struct Capture {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_null() {
                SelectObject(self.memory, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.memory.is_null() {
                DeleteDC(self.memory);
            }
            ReleaseDC(null_mut(), self.screen);
        }
    }
}

fn failure(operation: &str) -> String {
    format!(
        "Windows desktop capture {operation} failed: {}",
        io::Error::last_os_error()
    )
}

pub(super) fn capture_screen() -> Result<(RgbaImage, i32, i32), String> {
    let previous_dpi =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    if previous_dpi.is_null() {
        return Err(failure("SetThreadDpiAwarenessContext"));
    }
    let _dpi = DpiContext(previous_dpi);
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if width <= 0 || height <= 0 {
        return Err(INVALID_DESKTOP.to_owned());
    }
    let length = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(PIXEL_BYTES))
        .ok_or_else(|| INVALID_DESKTOP.to_owned())?;
    let screen = unsafe { GetDC(null_mut()) };
    if screen.is_null() {
        return Err(failure("GetDC"));
    }
    let mut capture = Capture {
        screen,
        memory: null_mut(),
        bitmap: null_mut(),
        previous: null_mut(),
    };
    capture.memory = unsafe { CreateCompatibleDC(screen) };
    if capture.memory.is_null() {
        return Err(failure("CreateCompatibleDC"));
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: BITS_PER_PIXEL,
            biCompression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = null_mut();
    capture.bitmap =
        unsafe { CreateDIBSection(screen, &info, DIB_RGB_COLORS, &mut pixels, null_mut(), 0) };
    if capture.bitmap.is_null() || pixels.is_null() {
        return Err(failure("CreateDIBSection"));
    }
    capture.previous = unsafe { SelectObject(capture.memory, capture.bitmap) };
    if capture.previous.is_null() {
        return Err(failure("SelectObject"));
    }
    if unsafe {
        BitBlt(
            capture.memory,
            0,
            0,
            width,
            height,
            screen,
            left,
            top,
            SRCCOPY | CAPTUREBLT,
        )
    } == 0
    {
        return Err(failure("BitBlt"));
    }
    if unsafe { GdiFlush() } == 0 {
        return Err(failure("GdiFlush"));
    }
    let mut bytes = unsafe { slice::from_raw_parts(pixels.cast::<u8>(), length) }.to_vec();
    for pixel in bytes.as_chunks_mut::<PIXEL_BYTES>().0 {
        pixel.swap(0, 2);
        pixel[3] = u8::MAX;
    }
    let image = RgbaImage::from_raw(width as u32, height as u32, bytes)
        .ok_or_else(|| INVALID_DESKTOP.to_owned())?;
    Ok((image, left, top))
}
