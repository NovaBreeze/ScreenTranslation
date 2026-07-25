use anyhow::{Context, Result};
use image::{DynamicImage, RgbaImage};

#[cfg(windows)]
use windows::Win32::{
    Foundation::{LPARAM, POINT, RECT},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetDIBits,
        GetMonitorInfoW, HDC, HGDIOBJ, HMONITOR, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
        MONITORINFOEXW, MonitorFromPoint, ReleaseDC, SRCCOPY, SelectObject,
    },
    UI::{
        HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        WindowsAndMessaging::GetCursorPos,
    },
};

#[derive(Debug, Clone)]
pub struct DisplayInfo {
    /// GDI 设备名（如 `\\.\DISPLAY1`），用于与 DXGI 输出匹配。
    pub device_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
}

#[cfg(windows)]
pub fn capture_display(display: DisplayInfo) -> Result<DynamicImage> {
    capture_region(display.x, display.y, display.width, display.height)
}

#[cfg(windows)]
fn capture_region(origin_x: i32, origin_y: i32, width: u32, height: u32) -> Result<DynamicImage> {
    unsafe {
        let screen_dc = GetDC(None);
        anyhow::ensure!(!screen_dc.is_invalid(), "GetDC 失败");
        let memory_dc = CreateCompatibleDC(Some(screen_dc));
        if memory_dc.is_invalid() {
            ReleaseDC(None, screen_dc);
            anyhow::bail!("CreateCompatibleDC 失败");
        }
        let bitmap = CreateCompatibleBitmap(screen_dc, width as i32, height as i32);
        if bitmap.is_invalid() {
            let _ = DeleteDC(memory_dc);
            ReleaseDC(None, screen_dc);
            anyhow::bail!("CreateCompatibleBitmap 失败");
        }

        let previous = SelectObject(memory_dc, HGDIOBJ(bitmap.0));
        let copied = BitBlt(
            memory_dc,
            0,
            0,
            width as i32,
            height as i32,
            Some(screen_dc),
            origin_x,
            origin_y,
            SRCCOPY,
        );

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; (width * height * 4) as usize];

        let read_rows = if copied.is_ok() {
            GetDIBits(
                memory_dc,
                bitmap,
                0,
                height,
                Some(bgra.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };

        SelectObject(memory_dc, previous);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory_dc);
        ReleaseDC(None, screen_dc);

        anyhow::ensure!(read_rows == height as i32, "读取屏幕位图失败");
        for pixel in bgra.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        let image = RgbaImage::from_raw(width, height, bgra).context("屏幕位图尺寸无效")?;
        Ok(DynamicImage::ImageRgba8(image))
    }
}

#[cfg(windows)]
pub fn display_under_cursor() -> DisplayInfo {
    let mut point = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    display_for_point(point)
}

#[cfg(windows)]
fn display_for_point(point: POINT) -> DisplayInfo {
    let target = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTOPRIMARY) };
    let monitors = enumerate_monitors();
    monitors
        .into_iter()
        .enumerate()
        .find(|(_, (handle, _))| *handle == target)
        .map(|(_, (handle, rect))| display_info(handle, rect))
        .unwrap_or(DisplayInfo {
            device_name: String::new(),
            x: 0,
            y: 0,
            width: 1280,
            height: 720,
            scale_factor: 1.0,
        })
}

#[cfg(windows)]
fn display_info(monitor: HMONITOR, rect: RECT) -> DisplayInfo {
    let mut dpi_x = 96;
    let mut dpi_y = 96;
    unsafe {
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
    }
    let mut info = MONITORINFOEXW {
        monitorInfo: Default::default(),
        szDevice: [0; 32],
    };
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let device_name = if unsafe { GetMonitorInfoW(monitor, std::ptr::from_mut(&mut info).cast()) }
        .as_bool()
    {
        let end = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        String::from_utf16_lossy(&info.szDevice[..end])
    } else {
        String::new()
    };
    DisplayInfo {
        device_name,
        x: rect.left,
        y: rect.top,
        width: (rect.right - rect.left).max(1) as u32,
        height: (rect.bottom - rect.top).max(1) as u32,
        scale_factor: (dpi_x as f32 / 96.0).max(1.0),
    }
}

#[cfg(windows)]
fn enumerate_monitors() -> Vec<(HMONITOR, RECT)> {
    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        let monitors = unsafe { &mut *(data.0 as *mut Vec<(HMONITOR, RECT)>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            monitors.push((monitor, info.rcMonitor));
        }
        true.into()
    }

    let mut monitors = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(callback),
            LPARAM((&mut monitors as *mut Vec<(HMONITOR, RECT)>) as isize),
        );
    }
    monitors
}

#[cfg(not(windows))]
pub fn display_under_cursor() -> DisplayInfo {
    DisplayInfo {
        device_name: String::new(),
        x: 0,
        y: 0,
        width: 1280,
        height: 720,
        scale_factor: 1.0,
    }
}

#[cfg(not(windows))]
pub fn capture_display(_display: DisplayInfo) -> Result<DynamicImage> {
    anyhow::bail!("截屏仅支持 Windows")
}
