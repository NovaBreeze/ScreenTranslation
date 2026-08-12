//! 端到端黑闪复现探针：注入真实输入驱动正在运行的 ScreenTranslator，
//! 全程 GDI 高速连拍，统计各阶段全黑帧。
//!
//! 用法：先启动 target\release\screen-translator.exe，再运行
//!   cargo run --release --example flash_probe

use std::thread::sleep;
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ, ReleaseDC, SRCCOPY,
    SelectObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT, SendInput, VIRTUAL_KEY, VK_CONTROL,
    VK_ESCAPE, VK_MENU, VK_T,
};
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

const W: i32 = 1920;
const H: i32 = 1080;

/// 区域截图并采样统计纯黑像素占比。
fn black_ratio_region(rx: i32, ry: i32, rw: i32, rh: i32) -> f64 {
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, rw, rh);
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        let _ = BitBlt(mem, 0, 0, rw, rh, Some(screen), rx, ry, SRCCOPY);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: rw,
                biHeight: -rh,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (rw * rh * 4) as usize];
        GetDIBits(
            mem,
            bmp,
            0,
            rh as u32,
            Some(buf.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        let mut black = 0usize;
        let mut total = 0usize;
        for px in buf.chunks_exact(4) {
            if px[0] < 8 && px[1] < 8 && px[2] < 8 {
                black += 1;
            }
            total += 1;
        }
        black as f64 / total as f64
    }
}

/// 全屏截图并采样统计纯黑像素占比。
fn black_ratio() -> f64 {
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, W, H);
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        let _ = BitBlt(mem, 0, 0, W, H, Some(screen), 0, 0, SRCCOPY);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: W,
                biHeight: -H,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (W * H * 4) as usize];
        GetDIBits(
            mem,
            bmp,
            0,
            H as u32,
            Some(buf.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        let mut black = 0usize;
        let mut total = 0usize;
        for px in buf.chunks_exact(4).step_by(16) {
            if px[0] < 8 && px[1] < 8 && px[2] < 8 {
                black += 1;
            }
            total += 1;
        }
        black as f64 / total as f64
    }
}

fn send_keys(down_up: &[(VIRTUAL_KEY, bool)]) {
    let inputs: Vec<INPUT> = down_up
        .iter()
        .map(|&(vk, up)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        })
        .collect();
    unsafe {
        SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}

fn hotkey() {
    send_keys(&[
        (VK_CONTROL, false),
        (VK_MENU, false),
        (VK_T, false),
        (VK_T, true),
        (VK_MENU, true),
        (VK_CONTROL, true),
    ]);
}

fn esc() {
    send_keys(&[(VK_ESCAPE, false), (VK_ESCAPE, true)]);
}

fn mouse(x: i32, y: i32, flags: MOUSE_EVENT_FLAGS) {
    unsafe {
        let _ = SetCursorPos(x, y);
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        SendInput(&[input], size_of::<INPUT>() as i32);
    }
}

fn burst(label: &str, frames: usize, interval_ms: u64) {
    let start = Instant::now();
    for i in 0..frames {
        let ratio = black_ratio();
        let t = start.elapsed().as_millis();
        let marker = if ratio > 0.5 { "  <== 黑帧" } else { "" };
        println!(
            "[{label}] frame {i:02} t={t:4}ms black={:.1}%{marker}",
            ratio * 100.0
        );
        sleep(Duration::from_millis(interval_ms));
    }
}

/// 小区域高速连拍：抓单帧级黑闪。
fn fast_burst(label: &str, frames: usize) {
    let start = Instant::now();
    for i in 0..frames {
        let ratio = black_ratio_region(640, 360, 640, 360);
        let t = start.elapsed().as_millis();
        let marker = if ratio > 0.5 { "  <== 黑帧" } else { "" };
        println!(
            "[{label}] frame {i:03} t={t:4}ms black={:.1}%{marker}",
            ratio * 100.0
        );
    }
}

fn main() {
    println!("== 阶段 0：发送热键，等待遮罩出现 ==");
    hotkey();
    sleep(Duration::from_millis(1200));
    burst("after-hotkey", 5, 20);

    println!("== 阶段 1：左键按下（开始框选） ==");
    mouse(960, 540, MOUSEEVENTF_LEFTDOWN);
    fast_burst("mouse-down", 120);

    println!("== 阶段 2：拖动 ==");
    mouse(1200, 700, MOUSE_EVENT_FLAGS(0));
    sleep(Duration::from_millis(100));
    mouse(1400, 800, MOUSEEVENTF_LEFTUP);
    sleep(Duration::from_millis(800));

    println!("== 阶段 3：ESC ==");
    esc();
    fast_burst("esc", 120);
    println!("== 完成 ==");
}
