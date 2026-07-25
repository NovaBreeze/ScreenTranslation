//! 无头渲染 OverlayWindow：MinimalSoftwareWindow + 软件渲染器输出 PNG，
//! 不依赖交互桌面即可验证结果工具条、帮助提示条悬停隐藏与回调接线。
//! 用法：cargo run --release --example render_overlay -- <OUT_DIR>

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};

slint::include_modules!();

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;

struct HeadlessPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

/// 蓝灰渐变帧：压暗层与选区“洞”的对比肉眼可辨。
fn frame_image() -> slint::Image {
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(WIDTH as u32, HEIGHT as u32);
    for (i, px) in buffer.make_mut_slice().iter_mut().enumerate() {
        let col = (i % WIDTH) as u8;
        *px = slint::Rgb8Pixel::new(70, 110, col);
    }
    slint::Image::from_rgb8(buffer)
}

fn render_png(window: &MinimalSoftwareWindow, path: &Path) {
    let mut buffer = vec![slint::Rgb8Pixel::default(); WIDTH * HEIGHT];
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, WIDTH);
    });
    let mut img = image::RgbImage::new(WIDTH as u32, HEIGHT as u32);
    for (i, px) in buffer.iter().enumerate() {
        img.put_pixel(
            (i % WIDTH) as u32,
            (i / WIDTH) as u32,
            image::Rgb([px.r, px.g, px.b]),
        );
    }
    img.save(path).expect("save png");
    println!("saved {}", path.display());
}

fn move_to(window: &MinimalSoftwareWindow, x: f32, y: f32) {
    window.dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(x, y),
    });
}

fn click_at(window: &MinimalSoftwareWindow, x: f32, y: f32) {
    let position = slint::LogicalPosition::new(x, y);
    window.dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| "probe".into());
    std::fs::create_dir_all(&out_dir).expect("create out dir");
    let out = |name: &str| Path::new(&out_dir).join(name);

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    window.set_size(slint::PhysicalSize::new(WIDTH as u32, HEIGHT as u32));
    slint::platform::set_platform(Box::new(HeadlessPlatform {
        window: window.clone(),
    }))
    .expect("set platform");

    let overlay = OverlayWindow::new().expect("create overlay");
    overlay.set_frame_image(frame_image());
    overlay.show().expect("show overlay");

    // 框选进行态：帮助提示条可见。
    overlay.set_sel_x(120.0);
    overlay.set_sel_y(150.0);
    overlay.set_sel_w(480.0);
    overlay.set_sel_h(250.0);
    render_png(&window, &out("ui-selection.png"));

    // 结果态：工具条出现在选框下方。
    overlay.set_locked(true);
    // 失败提示条：选框下方红字提示（识别失败时不弹独立窗口）。
    overlay.set_notice_text("处理失败：选区中未识别到文字".into());
    render_png(&window, &out("ui-notice.png"));
    overlay.set_notice_text("".into());
    overlay.set_has_result(true);
    overlay.set_status_text("翻译完成 · 选框下方工具条：切换/复制/×关闭".into());
    render_png(&window, &out("ui-result.png"));

    // 鼠标进入帮助提示条区域（x 18-538, y 1014-1062）→ 提示条隐藏；移开 → 恢复。
    move_to(&window, 100.0, 1040.0);
    assert!(!overlay.get_help_shown(), "鼠标悬停提示条时应隐藏");
    render_png(&window, &out("ui-help-hover.png"));
    move_to(&window, 960.0, 540.0);
    assert!(overlay.get_help_shown(), "鼠标移开后提示条应恢复");
    render_png(&window, &out("ui-help-away.png"));

    // 工具条分段回调：切换 / 复制 / 关闭。
    let toggled = Rc::new(Cell::new(false));
    let copied = Rc::new(Cell::new(false));
    let cancelled = Rc::new(Cell::new(false));
    {
        let toggled = Rc::clone(&toggled);
        overlay.on_view_toggled(move |show_original| {
            assert!(show_original, "首次切换应进入原文态");
            toggled.set(true);
        });
    }
    {
        let copied = Rc::clone(&copied);
        overlay.on_copy_requested(move || copied.set(true));
    }
    {
        let cancelled = Rc::clone(&cancelled);
        overlay.on_cancelled(move || cancelled.set(true));
    }
    click_at(&window, 419.0, 428.0);
    assert!(toggled.get(), "切换分段未触发 view-toggled");
    assert!(overlay.get_show_original(), "切换后 show-original 应为 true");
    render_png(&window, &out("ui-original.png"));

    click_at(&window, 523.0, 428.0);
    assert!(copied.get(), "复制分段未触发 copy-requested");

    click_at(&window, 578.0, 428.0);
    assert!(cancelled.get(), "关闭分段未触发 cancelled");

    // 锁定态重新拖选：按下应触发 reselect-started，松开应携带新选区触发 selected。
    let reselect = Rc::new(Cell::new(false));
    let reselected: Rc<Cell<Option<(f32, f32, f32, f32)>>> = Rc::new(Cell::new(None));
    {
        let reselect = Rc::clone(&reselect);
        overlay.on_reselect_started(move || reselect.set(true));
    }
    {
        let reselected = Rc::clone(&reselected);
        overlay.on_selected(move |x, y, w, h| reselected.set(Some((x, y, w, h))));
    }
    window.dispatch_event(WindowEvent::PointerPressed {
        position: slint::LogicalPosition::new(700.0, 500.0),
        button: PointerEventButton::Left,
    });
    move_to(&window, 800.0, 550.0);
    move_to(&window, 900.0, 600.0);
    window.dispatch_event(WindowEvent::PointerReleased {
        position: slint::LogicalPosition::new(900.0, 600.0),
        button: PointerEventButton::Left,
    });
    assert!(reselect.get(), "锁定态按下未触发 reselect-started");
    assert_eq!(
        reselected.get(),
        Some((700.0, 500.0, 200.0, 100.0)),
        "重新拖选后 selected 选区不符"
    );

    println!("ok: toolbar segments + help hover + reselect verified");
}
