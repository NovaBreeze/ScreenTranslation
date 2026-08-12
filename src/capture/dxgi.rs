use anyhow::{Context, Result};
use image::{DynamicImage, RgbaImage};

#[cfg(windows)]
use std::sync::Mutex;

/// 缓存的 Desktop Duplication 管理器；模式切换或会话迁移导致失败时整体丢弃重建。
#[cfg(windows)]
static MANAGER: Mutex<Option<dxgi_capture_rs::DXGIManager>> = Mutex::new(None);

/// 解析显示器 GDI 设备名（如 `\\.\DISPLAY1`）到 dxgi-capture-rs 期望的
/// “适配器 0 上第 N 个桌面输出”索引。
///
/// dxgi-capture-rs 会遍历所有适配器并对每个适配器按同一索引取输出，只在首个
/// 成功的适配器上复制——只有当目标输出位于首个枚举到的适配器时结果才可预期，
/// 否则返回 None 让调用方回退 GDI。
#[cfg(windows)]
fn output_index_for_device(device_name: &str) -> Option<usize> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};

    if device_name.is_empty() {
        return None;
    }
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut adapter_ordinal = 0u32;
        loop {
            let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(adapter_ordinal) {
                Ok(adapter) => adapter,
                Err(_) => break,
            };
            let mut attached = 0usize;
            let mut output_ordinal = 0u32;
            loop {
                let output = match adapter.EnumOutputs(output_ordinal) {
                    Ok(output) => output,
                    Err(_) => break,
                };
                output_ordinal += 1;
                let Ok(desc) = output.GetDesc() else {
                    continue;
                };
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let name = widened_to_string(&desc.DeviceName);
                if name.eq_ignore_ascii_case(device_name) {
                    // 仅在首个适配器上信任该索引（见函数文档）。
                    return (adapter_ordinal == 0).then_some(attached);
                }
                attached += 1;
            }
            adapter_ordinal += 1;
        }
    }
    None
}

#[cfg(windows)]
fn widened_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

/// 通过 DXGI Desktop Duplication 截取指定显示器（按其 GDI 设备名定位）。
///
/// 单次尝试：任何失败都清理缓存并返回错误，由调用方立即回退 GDI，
/// 避免在远程桌面等 DXGI 不可用的环境里反复超时。
#[cfg(windows)]
pub fn capture_monitor(device_name: &str) -> Result<DynamicImage> {
    use dxgi_capture_rs::DXGIManager;

    let index =
        output_index_for_device(device_name).context("目标显示器不在主适配器上，跳过 DXGI")?;

    let mut guard = MANAGER.lock().expect("dxgi manager lock");
    if guard.is_none() {
        *guard = Some(
            DXGIManager::new(100).map_err(|error| anyhow::anyhow!("初始化 DXGI 失败：{error}"))?,
        );
    }
    let manager = guard.as_mut().expect("dxgi manager");
    manager.set_capture_source_index(index);
    let result = manager.capture_frame();
    match result {
        Ok((pixels, (width, height))) => {
            let mut rgba = Vec::with_capacity(pixels.len() * 4);
            for pixel in pixels {
                rgba.extend_from_slice(&[pixel.r, pixel.g, pixel.b, 255]);
            }
            let image = RgbaImage::from_raw(width as u32, height as u32, rgba)
                .context("DXGI 返回的帧尺寸无效")?;
            Ok(DynamicImage::ImageRgba8(image))
        }
        Err(error) => {
            // 会话迁移/模式切换后复制句柄会失效，整体丢弃下次重建。
            *guard = None;
            Err(anyhow::anyhow!("DXGI 捕获失败：{error}"))
        }
    }
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn capture_monitor(_device_name: &str) -> Result<DynamicImage> {
    anyhow::bail!("DXGI 截屏仅支持 Windows")
}
