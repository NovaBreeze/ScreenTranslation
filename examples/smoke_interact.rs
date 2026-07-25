//! 交互冒烟探针：单步注入输入驱动运行中的 ScreenTranslator，时序由外部控制。
//! 用法：cargo run --release --example smoke_interact -- <hotkey|drag|toggle|copy|close|click X Y|esc|shot PATH>

use std::thread::sleep;
use std::time::Duration;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ, ReleaseDC, SRCCOPY,
    SelectObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, SendInput, VIRTUAL_KEY, VK_CONTROL,
    VK_ESCAPE, VK_MENU, VK_T,
};
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

/// GDI 全屏截图保存为 PNG（BitBlt 读 DWM 合成结果，所见即所得）。
fn shot(path: &str) {
    const W: i32 = 1920;
    const H: i32 = 1080;
    let mut buf = vec![0u8; (W * H * 4) as usize];
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
    }
    // BGRA → RGBA
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let img: image::RgbaImage =
        image::RgbaImage::from_raw(W as u32, H as u32, buf).expect("image buffer");
    img.save(path).expect("save screenshot");
    eprintln!("saved {path}");
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
                    dwFlags: if up { KEYEVENTF_KEYUP } else { Default::default() },
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

fn click(x: i32, y: i32) {
    mouse(x, y, MOUSEEVENTF_LEFTDOWN);
    sleep(Duration::from_millis(40));
    mouse(x, y, MOUSEEVENTF_LEFTUP);
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("hotkey") => send_keys(&[
            (VK_CONTROL, false),
            (VK_MENU, false),
            (VK_T, false),
            (VK_T, true),
            (VK_MENU, true),
            (VK_CONTROL, true),
        ]),
        // 框选设置窗口左上区域（冻结帧上该区域是设置窗口文本）。
        Some("drag") => {
            mouse(120, 150, MOUSEEVENTF_LEFTDOWN);
            for step in 1..=10 {
                let _ = unsafe { SetCursorPos(120 + step * 48, 150 + step * 25) };
                sleep(Duration::from_millis(20));
            }
            mouse(600, 400, MOUSEEVENTF_LEFTUP);
        }
        // 结果工具条在选框(120,150)-(600,400)下方：x 348-600, y 408-448。
        // 分段中心：切换 ≈ (419,428)，复制 ≈ (523,428)，关闭 ≈ (578,428)。
        Some("toggle") => click(419, 428),
        Some("copy") => click(523, 428),
        Some("close") => click(578, 428),
        Some("click") => {
            let args: Vec<i32> = std::env::args()
                .skip(2)
                .filter_map(|arg| arg.parse().ok())
                .collect();
            if let [x, y] = args[..] {
                click(x, y);
            }
        }
        Some("esc") => send_keys(&[(VK_ESCAPE, false), (VK_ESCAPE, true)]),
        Some("shot") => {
            if let Some(path) = std::env::args().nth(2) {
                shot(&path);
            }
        }
        other => eprintln!("unknown step: {other:?}"),
    }
}
