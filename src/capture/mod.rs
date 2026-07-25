mod dxgi;
mod gdi;

use anyhow::Result;
use image::DynamicImage;

pub use gdi::DisplayInfo;

/// Selection rectangle in **physical pixels**, relative to the target display origin.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

pub fn display_under_cursor() -> DisplayInfo {
    gdi::display_under_cursor()
}

/// Capture a full frame of `display`, preferring DXGI Desktop Duplication and
/// falling back to GDI BitBlt immediately when DXGI is unavailable.
///
/// 返回物理像素尺寸的整屏帧；选区遮罩显示与后续裁剪都基于这一帧，
/// 不需要再次截屏，也就不存在“遮罩未隐藏干净”的竞态。
pub fn capture_display_frame(display: &DisplayInfo) -> Result<DynamicImage> {
    let (device, width, height, scale) = (
        display.device_name.as_str(),
        display.width,
        display.height,
        display.scale_factor,
    );
    let (x, y) = (display.x, display.y);
    tracing::info!(device, x, y, width, height, scale, "capturing display frame");

    // 等 DWM 把挂起的合成请求全部落地：刚移出屏幕的遮罩可能还留在
    // 合成画面里一两帧，直接 BitBlt 会把它冻进新底图（残影）。
    // DwmFlush 最多等一个垂直同步周期，代价远小于一次错误截屏。
    dwm_flush();

    // GDI 优先：BitBlt 读取 DWM 合成后的会话桌面，远程桌面/虚拟显卡/多显卡
    // 环境下始终是“用户实际看到的画面”。DXGI Desktop Duplication 仅作兜底——
    // 它可能枚举到空白的虚拟显示输出（如远程控制软件的 IDD 设备），返回
    // 尺寸正确但内容为空/全黑的帧。
    let frame = match gdi::capture_display(display.clone()) {
        Ok(frame) => {
            let ratio = black_ratio(&frame);
            if ratio < 0.98 {
                tracing::info!(source = "gdi", black_ratio = ratio, "frame captured");
                frame
            } else {
                tracing::warn!(black_ratio = ratio, "GDI 帧接近全黑，尝试 DXGI");
                dxgi::capture_monitor(&display.device_name)
                    .map_err(|error| anyhow::anyhow!("GDI 全黑且 DXGI 失败：{error}"))?
            }
        }
        Err(gdi_error) => {
            tracing::warn!(error = %gdi_error, "GDI 截屏失败，尝试 DXGI");
            dxgi::capture_monitor(&display.device_name).map_err(|dxgi_error| {
                anyhow::anyhow!("GDI 失败（{gdi_error:#}）且 DXGI 失败：{dxgi_error}")
            })?
        }
    };
    anyhow::ensure!(
        frame.width() == display.width && frame.height() == display.height,
        "截屏帧尺寸 {}x{} 与显示器 {}x{} 不符",
        frame.width(),
        frame.height(),
        display.width,
        display.height
    );
    tracing::info!(
        width = frame.width(),
        height = frame.height(),
        black_ratio = black_ratio(&frame),
        "display frame ready"
    );
    Ok(frame)
}

/// 等待 DWM 完成所有挂起的合成呈现（见 capture_display_frame 注释）。
#[cfg(windows)]
fn dwm_flush() {
    use windows::Win32::Graphics::Dwm::DwmFlush;
    unsafe {
        let _ = DwmFlush();
    }
}

#[cfg(not(windows))]
fn dwm_flush() {}

/// 采样估算纯黑像素占比（诊断用，步长采样避免全量拷贝）。
pub fn black_ratio(image: &DynamicImage) -> f64 {
    let rgba = image.to_rgba8();
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    if width == 0 || height == 0 {
        return 1.0;
    }
    let step = 16usize;
    let mut black = 0usize;
    let mut total = 0usize;
    for y in (0..height).step_by(step) {
        for x in (0..width).step_by(step) {
            let pixel = rgba.get_pixel(x as u32, y as u32);
            if pixel.0[0] < 8 && pixel.0[1] < 8 && pixel.0[2] < 8 {
                black += 1;
            }
            total += 1;
        }
    }
    black as f64 / total.max(1) as f64
}

/// Crop `selection` (physical pixels, display-relative) out of a frame
/// previously produced by [`capture_display_frame`].
pub fn crop_selection(frame: &DynamicImage, selection: Selection) -> Result<DynamicImage> {
    let (frame_w, frame_h) = (frame.width(), frame.height());

    let x = (selection.x.round().max(0.0) as u32).min(frame_w);
    let y = (selection.y.round().max(0.0) as u32).min(frame_h);
    let width = (selection.width.round().max(1.0) as u32).min(frame_w.saturating_sub(x));
    let height = (selection.height.round().max(1.0) as u32).min(frame_h.saturating_sub(y));

    anyhow::ensure!(width > 0 && height > 0, "选择区域超出屏幕范围");
    Ok(frame.crop_imm(x, y, width, height))
}

/// 把选区大小的内容（通常是渲染了译文的选区图）按选区原点贴回整屏帧，
/// 供遮罩原地展示翻译结果。原点钳制规则与 [`crop_selection`] 一致。
pub fn composite_selection(
    frame: &DynamicImage,
    selection: Selection,
    content: &DynamicImage,
) -> DynamicImage {
    let mut output = frame.clone();
    let x = (selection.x.round().max(0.0) as u32).min(frame.width());
    let y = (selection.y.round().max(0.0) as u32).min(frame.height());
    image::imageops::overlay(&mut output, content, i64::from(x), i64::from(y));
    output
}

#[cfg(all(test, windows))]
mod probe {
    /// 真实渲染链路 + 交互黑帧探针（winit 每进程只允许一个事件循环，故合并为单测试）：
    /// 全屏红色遮罩 → 读回验证渲染正常 → 模拟按下/拖动/ESC，全程连拍检测黑帧。
    /// 需要可见桌面会话，CI/无头环境请跳过：-- --ignored 手动运行。
    #[test]
    #[ignore = "需要可见桌面会话"]
    fn overlay_render_and_interaction() {
        use image::{DynamicImage, Rgba, RgbaImage};
        use slint::ComponentHandle;
        use std::time::{Duration, Instant};

        let red = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            1920,
            1080,
            Rgba([220, 30, 30, 255]),
        ));
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let (report_tx, report_rx) = std::sync::mpsc::channel();
        let sampler = std::thread::spawn(move || {
            let start = Instant::now();
            let mut samples = Vec::new();
            while stop_rx.try_recv().is_err() {
                let info = super::DisplayInfo {
                    device_name: String::new(),
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                    scale_factor: 1.0,
                };
                if let Ok(shot) = super::gdi::capture_display(info) {
                    samples.push((start.elapsed().as_millis(), super::black_ratio(&shot)));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = report_tx.send(samples);
        });

        let overlay = crate::OverlayWindow::new().expect("overlay");
        overlay.set_frame_image(crate::app::to_slint_image_for_test(&red));
        overlay
            .window()
            .set_position(slint::PhysicalPosition::new(0, 0));
        overlay
            .window()
            .set_size(slint::PhysicalSize::new(1920, 1080));
        let weak = overlay.as_weak();

        // t=1200ms：读回中心区域验证渲染（压暗后红色约 (117,16,16)，按“红色主导”判定）
        let (red_tx, red_rx) = std::sync::mpsc::channel::<f64>();
        slint::Timer::single_shot(Duration::from_millis(1200), move || {
            let shot = super::gdi::capture_display(super::DisplayInfo {
                device_name: String::new(),
                x: 300,
                y: 250,
                width: 400,
                height: 300,
                scale_factor: 1.0,
            })
            .expect("recapture");
            let full = super::gdi::capture_display(super::DisplayInfo {
                device_name: String::new(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale_factor: 1.0,
            })
            .expect("fullscreen");
            full.save("target/probe-merged-full.png").ok();
            let rgba = shot.to_rgba8();
            let total = (rgba.width() * rgba.height()) as f64;
            let red = rgba
                .pixels()
                .filter(|p| {
                    let (r, g, b) = (p.0[0] as u32, p.0[1] as u32, p.0[2] as u32);
                    r > 60 && r > g * 2 && r > b * 2
                })
                .count() as f64;
            let _ = red_tx.send(red / total);
        });
        // t=700ms：模拟左键按下（选区归零）
        slint::Timer::single_shot(Duration::from_millis(700), {
            let weak = weak.clone();
            move || {
                if let Some(o) = weak.upgrade() {
                    o.set_sel_x(100.0);
                    o.set_sel_y(100.0);
                    o.set_sel_w(0.0);
                    o.set_sel_h(0.0);
                }
            }
        });
        // t=850/1000ms：模拟拖动
        for (ms, w, h) in [(850u64, 300.0f32, 200.0f32), (1000, 600.0, 400.0)] {
            slint::Timer::single_shot(Duration::from_millis(ms), {
                let weak = weak.clone();
                move || {
                    if let Some(o) = weak.upgrade() {
                        o.set_sel_w(w);
                        o.set_sel_h(h);
                    }
                }
            });
        }
        // t=1400ms：模拟 ESC（离屏停靠，与生产行为一致）
        slint::Timer::single_shot(Duration::from_millis(1400), {
            let weak = weak.clone();
            move || {
                if let Some(o) = weak.upgrade() {
                    o.window()
                        .set_position(slint::PhysicalPosition::new(-32000, -32000));
                }
            }
        });
        slint::Timer::single_shot(Duration::from_millis(2000), || {
            let _ = slint::quit_event_loop();
        });
        overlay.show().expect("show");
        slint::run_event_loop_until_quit().expect("event loop");

        let red_ratio = red_rx.recv().expect("red ratio");
        eprintln!("overlay red pixel ratio = {:.2}%", red_ratio * 100.0);
        assert!(
            red_ratio > 0.5,
            "遮罩应渲染出冻结帧，实测红色占比 {:.2}%",
            red_ratio * 100.0
        );

        let _ = stop_tx.send(());
        let samples: Vec<(u128, f64)> = report_rx.recv().expect("samples");
        sampler.join().ok();
        let black_frames = samples.iter().filter(|(_, r)| *r > 0.5).count();
        eprintln!("samples = {}, black frames = {}", samples.len(), black_frames);
        assert_eq!(black_frames, 0, "交互过程不应出现全黑帧");
    }
}
