use crate::{
    AboutWindow, AppTray, HistoryItem, HistoryWindow, OverlayWindow, ResultWindow, SettingsWindow,
    StatusToast,
    capture::{self, DisplayInfo, Selection},
    config::{AppConfig, TranslationEngine},
    history::HistoryDb,
    ocr::OcrEngine,
    platform::{
        autostart::Autostart,
        clipboard,
        hotkey::{Hotkey, HotkeyManager, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN},
        single::{InstanceCommand, SingleInstance},
        update::{self, UpdateInfo},
    },
    render,
    translate::{OllamaTranslator, OpenAiTranslator, Translator},
};
use anyhow::{Context, Result};
use image::{DynamicImage, Rgba};
use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer, SharedString};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

thread_local! {
    static RESULT_WINDOW: RefCell<Option<ResultWindow>> = const { RefCell::new(None) };
    static HISTORY_WINDOW: RefCell<Option<HistoryWindow>> = const { RefCell::new(None) };
    static ABOUT_WINDOW: RefCell<Option<AboutWindow>> = const { RefCell::new(None) };
    static STATUS_TOAST: RefCell<Option<StatusToast>> = const { RefCell::new(None) };
}

#[derive(Clone)]
struct ResultData {
    translation: String,
    image: DynamicImage,
    window_x: i32,
    window_y: i32,
}

pub fn run(instance: SingleInstance) -> Result<()> {
    let settings = SettingsWindow::new().context("创建设置窗口失败")?;
    let tray = AppTray::new().context("创建托盘图标失败")?;
    let mut loaded_config = AppConfig::load().unwrap_or_default();
    if let Ok(autostart) = Autostart::for_current_exe("ScreenTranslator")
        && let Ok(enabled) = autostart.is_enabled()
    {
        loaded_config.autostart = enabled;
    }
    let config = Arc::new(Mutex::new(loaded_config));
    apply_config(&settings, &config.lock().expect("config lock"));
    let ocr = Arc::new(Mutex::new(OcrEngine::default()));
    if config.lock().expect("config lock").prewarm_ocr {
        let ocr_for_prewarm = Arc::clone(&ocr);
        thread::spawn(move || {
            let _ = ocr_for_prewarm.lock().expect("ocr lock").prewarm();
        });
    }

    let overlay_slot = Rc::new(RefCell::new(None::<OverlayWindow>));
    let result_data = Arc::new(Mutex::new(None::<ResultData>));
    let current_task = Arc::new(Mutex::new(None::<CancellationToken>));
    let available_update = Arc::new(Mutex::new(None::<UpdateInfo>));

    install_save_handler(&settings, Arc::clone(&config));
    install_test_handler(&settings, Arc::clone(&config));
    install_capture_handler(
        &settings,
        Arc::clone(&config),
        Arc::clone(&ocr),
        Rc::clone(&overlay_slot),
        Arc::clone(&result_data),
        Arc::clone(&current_task),
        tray.as_weak(),
    );

    install_settings_extras(
        &settings,
        Arc::clone(&config),
        Arc::clone(&ocr),
        Arc::clone(&available_update),
    );
    check_for_updates(settings.as_weak(), Arc::clone(&available_update), false);

    let capture_weak = settings.as_weak();
    tray.on_capture(move || {
        if let Some(settings) = capture_weak.upgrade() {
            settings.invoke_start_capture();
        }
    });
    let settings_weak = settings.as_weak();
    tray.on_open_settings(move || {
        if let Some(settings) = settings_weak.upgrade() {
            let _ = settings.show();
        }
    });
    let history_settings = settings.as_weak();
    tray.on_open_history(move || {
        if let Some(settings) = history_settings.upgrade() {
            settings.invoke_open_history();
        }
    });
    tray.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    let configured_hotkey =
        parse_hotkey(&config.lock().expect("config lock").hotkey).unwrap_or_default();
    let hotkey_weak = settings.as_weak();
    let hotkey = HotkeyManager::with_hotkey(configured_hotkey, move || {
        let weak = hotkey_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(settings) = weak.upgrade() {
                settings.invoke_start_capture();
            }
        });
    });
    if let Err(error) = &hotkey {
        settings.set_status_text(format!("全局热键注册失败：{error:#}").into());
    }

    let instance_weak = settings.as_weak();
    let instance_listener = instance.listen(move |command| {
        if command == InstanceCommand::ShowSettings {
            let weak = instance_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(settings) = weak.upgrade() {
                    let _ = settings.show();
                }
            });
        }
    })?;

    tray.show().context("显示托盘图标失败")?;
    let should_show_settings = config
        .lock()
        .expect("config lock")
        .api_key()
        .ok()
        .flatten()
        .is_none();
    if should_show_settings {
        settings.show().context("显示设置窗口失败")?;
    }
    let result = slint::run_event_loop_until_quit().context("Slint 事件循环异常");
    drop(instance_listener);
    drop(hotkey);
    drop(instance);
    result
}

fn parse_hotkey(value: &str) -> Option<Hotkey> {
    let parts: Vec<String> = value
        .split('+')
        .map(|part| part.trim().to_ascii_uppercase())
        .filter(|part| !part.is_empty())
        .collect();
    let key = parts.last()?;
    let mut modifiers = MOD_NOREPEAT;
    for modifier in &parts[..parts.len().saturating_sub(1)] {
        modifiers |= match modifier.as_str() {
            "CTRL" | "CONTROL" => MOD_CONTROL,
            "ALT" => MOD_ALT,
            "SHIFT" => MOD_SHIFT,
            "WIN" | "WINDOWS" => MOD_WIN,
            _ => return None,
        };
    }
    let virtual_key = if key.len() == 1 {
        key.as_bytes()[0] as u32
    } else if let Some(number) = key
        .strip_prefix('F')
        .and_then(|value| value.parse::<u32>().ok())
    {
        if !(1..=24).contains(&number) {
            return None;
        }
        0x70 + number - 1
    } else {
        return None;
    };
    Some(Hotkey::new(modifiers, virtual_key))
}

fn parse_hex_color(value: &str) -> Option<Rgba<u8>> {
    let value = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if value.len() != 6 {
        return None;
    }
    Some(Rgba([
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
        255,
    ]))
}

fn apply_config(window: &SettingsWindow, config: &AppConfig) {
    window.set_api_base(config.api_base.clone().into());
    window.set_model(config.model.clone().into());
    window.set_target_language(config.target_lang.clone().into());
    window.set_proxy(config.proxy.clone().unwrap_or_default().into());
    window.set_copy_source(config.copy_source);
    window.set_copy_translation(config.copy_translation);
    window.set_save_history(config.save_history);
    window.set_launch_at_login(config.autostart);
    window.set_capture_shortcut(config.hotkey.clone().into());
    window.set_ocr_prewarm(config.prewarm_ocr);
    window.set_use_multimodal(config.multimodal_fallback);
    window.set_result_font_family(config.translation_font.clone().into());
    window.set_smart_result_colors(config.smart_text_color);
    window.set_result_text_color_hex(config.translation_text_color.clone().into());
    window.set_result_background_color_hex(config.translation_background_color.clone().into());
    match config.engine {
        TranslationEngine::OpenAiCompatible => {
            let deepseek = config.api_base.contains("deepseek");
            window.set_translation_engine(if deepseek {
                "DeepSeek".into()
            } else {
                "自定义 OpenAI 兼容".into()
            });
            window.set_service_preset(if deepseek {
                "DeepSeek".into()
            } else {
                "自定义".into()
            });
        }
        TranslationEngine::Ollama => {
            window.set_translation_engine("Ollama".into());
            window.set_service_preset("Ollama".into());
            window.set_ollama_host(config.api_base.clone().into());
        }
    }
}

fn config_from_window(window: &SettingsWindow, previous: &AppConfig) -> Result<AppConfig> {
    let mut config = previous.clone();
    config.api_base = window.get_api_base().to_string();
    config.model = window.get_model().to_string();
    config.target_lang = window.get_target_language().to_string();
    let proxy = window.get_proxy().trim().to_owned();
    config.proxy = (!proxy.is_empty()).then_some(proxy);
    config.copy_source = window.get_copy_source();
    config.copy_translation = window.get_copy_translation();
    config.save_history = window.get_save_history();
    config.autostart = window.get_launch_at_login();
    config.hotkey = window.get_capture_shortcut().replace(' ', "");
    config.prewarm_ocr = window.get_ocr_prewarm();
    config.multimodal_fallback = window.get_use_multimodal();
    if config.multimodal_fallback {
        config.multimodal_privacy_confirmed = true;
    }
    config.translation_font = window.get_result_font_family().to_string();
    config.smart_text_color = window.get_smart_result_colors();
    config.translation_text_color = window.get_result_text_color_hex().to_string();
    config.translation_background_color = window.get_result_background_color_hex().to_string();
    config.engine = if window.get_translation_engine() == "Ollama" {
        config.api_base = window.get_ollama_host().to_string();
        TranslationEngine::Ollama
    } else {
        TranslationEngine::OpenAiCompatible
    };
    let key = window.get_api_key();
    if !key.trim().is_empty() {
        config.set_api_key(key.as_str())?;
    }
    Ok(config)
}

fn install_save_handler(window: &SettingsWindow, config: Arc<Mutex<AppConfig>>) {
    let weak = window.as_weak();
    window.on_save_settings(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let previous = config.lock().expect("config lock").clone();
        match config_from_window(&window, &previous).and_then(|updated| {
            let hotkey_changed = updated.hotkey != previous.hotkey;
            Autostart::for_current_exe("ScreenTranslator")?.set_enabled(updated.autostart)?;
            updated.save()?;
            *config.lock().expect("config lock") = updated;
            Ok(hotkey_changed)
        }) {
            Ok(hotkey_changed) => {
                window.set_api_key(SharedString::default());
                window.set_status_text(
                    if hotkey_changed {
                        "设置已加密保存；新快捷键将在下次启动生效"
                    } else {
                        "设置已加密保存"
                    }
                    .into(),
                );
            }
            Err(error) => window.set_status_text(format!("保存失败：{error:#}").into()),
        }
    });
}

fn install_test_handler(window: &SettingsWindow, config: Arc<Mutex<AppConfig>>) {
    let weak = window.as_weak();
    window.on_test_connection(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let previous = config.lock().expect("config lock").clone();
        let updated = match config_from_window(&window, &previous) {
            Ok(config) => config,
            Err(error) => {
                window.set_status_text(format!("配置错误：{error:#}").into());
                return;
            }
        };
        window.set_status_text("正在测试连接…".into());
        let weak = window.as_weak();
        thread::spawn(move || {
            let result = run_async(async move {
                let translator = create_translator(&updated)?;
                translator.translate(&["连接测试".to_owned()]).await
            });
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    match result {
                        Ok(lines) => {
                            window.set_status_text(format!("连接成功：{}", lines.join(" ")).into())
                        }
                        Err(error) => window.set_status_text(format!("连接失败：{error:#}").into()),
                    }
                }
            });
        });
    });
}

fn install_settings_extras(
    window: &SettingsWindow,
    config: Arc<Mutex<AppConfig>>,
    ocr: Arc<Mutex<OcrEngine>>,
    available_update: Arc<Mutex<Option<UpdateInfo>>>,
) {
    let cancel_weak = window.as_weak();
    let cancel_config = Arc::clone(&config);
    window.on_cancel_settings(move || {
        if let Some(window) = cancel_weak.upgrade() {
            apply_config(&window, &cancel_config.lock().expect("config lock"));
            let _ = window.hide();
        }
    });

    let reset_weak = window.as_weak();
    window.on_reset_shortcut(move || {
        if let Some(window) = reset_weak.upgrade() {
            window.set_capture_shortcut("Ctrl + Alt + T".into());
            window.set_status_text("快捷键已恢复为 Ctrl + Alt + T，保存后生效".into());
        }
    });
    let record_weak = window.as_weak();
    window.on_record_shortcut(move || {
        if let Some(window) = record_weak.upgrade() {
            window.set_shortcut_recording(false);
            window.set_status_text("当前版本支持 Ctrl + Alt + T 全局热键".into());
        }
    });

    let preset_weak = window.as_weak();
    window.on_service_preset_changed(move |preset| {
        let Some(window) = preset_weak.upgrade() else {
            return;
        };
        match preset.as_str() {
            "DeepSeek" => {
                window.set_translation_engine("DeepSeek".into());
                window.set_api_base("https://api.deepseek.com/v1".into());
                window.set_model("deepseek-v4-flash".into());
            }
            "OpenAI" => {
                window.set_translation_engine("OpenAI".into());
                window.set_api_base("https://api.openai.com/v1".into());
                window.set_model("gpt-4.1-mini".into());
            }
            "Ollama" => {
                window.set_translation_engine("Ollama".into());
                window.set_ollama_host("http://127.0.0.1:11434".into());
                window.set_model("qwen3".into());
            }
            _ => {}
        }
    });
    let engine_weak = window.as_weak();
    window.on_translation_engine_changed(move |engine| {
        if let Some(window) = engine_weak.upgrade() {
            window.set_service_preset(
                if matches!(engine.as_str(), "DeepSeek" | "OpenAI" | "Ollama") {
                    engine
                } else {
                    "自定义".into()
                },
            );
        }
    });

    let ocr_weak = window.as_weak();
    window.on_prewarm_ocr(move || {
        let weak = ocr_weak.clone();
        let ocr = Arc::clone(&ocr);
        if let Some(window) = weak.upgrade() {
            window.set_status_text("正在预热 OCR…".into());
        }
        thread::spawn(move || {
            let result = ocr.lock().expect("ocr lock").prewarm();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    window.set_status_text(match result {
                        Ok(()) => "OCR 预热完成".into(),
                        Err(error) => format!("OCR 预热失败：{error:#}").into(),
                    });
                }
            });
        });
    });

    let history_weak = window.as_weak();
    window.on_open_history(move || {
        if let Err(error) = show_history_window() {
            if let Some(window) = history_weak.upgrade() {
                window.set_status_text(format!("打开历史失败：{error:#}").into());
            }
        }
    });
    let clear_weak = window.as_weak();
    window.on_clear_history(move || {
        match HistoryDb::open().and_then(|db| db.clear().map(|_| ())) {
            Ok(()) => {
                if let Some(window) = clear_weak.upgrade() {
                    window.set_status_text("历史记录已清空".into());
                }
            }
            Err(error) => {
                if let Some(window) = clear_weak.upgrade() {
                    window.set_status_text(format!("清空失败：{error:#}").into());
                }
            }
        }
    });
    let export_weak = window.as_weak();
    window.on_export_history(move || match export_history() {
        Ok(path) => {
            if let Some(window) = export_weak.upgrade() {
                window.set_status_text(format!("历史已导出：{}", path.display()).into());
            }
        }
        Err(error) => {
            if let Some(window) = export_weak.upgrade() {
                window.set_status_text(format!("导出失败：{error:#}").into());
            }
        }
    });

    let privacy_weak = window.as_weak();
    window.on_open_privacy_policy(move || {
        if let Err(error) = open_project_document("docs/privacy.md")
            && let Some(window) = privacy_weak.upgrade()
        {
            window.set_status_text(format!("打开隐私说明失败：{error:#}").into());
        }
    });
    let licenses_weak = window.as_weak();
    window.on_open_third_party_licenses(move || {
        if let Err(error) = open_project_document("docs/licenses.md")
            && let Some(window) = licenses_weak.upgrade()
        {
            window.set_status_text(format!("打开许可说明失败：{error:#}").into());
        }
    });
    let about_weak = window.as_weak();
    window.on_open_about(move || {
        if let Err(error) = show_about_window()
            && let Some(window) = about_weak.upgrade()
        {
            window.set_status_text(format!("打开关于页失败：{error:#}").into());
        }
    });

    let update_weak = window.as_weak();
    let update_state = Arc::clone(&available_update);
    window.on_check_for_updates(move || {
        if let Some(window) = update_weak.upgrade() {
            window.set_status_text("正在检查 GitHub Releases…".into());
        }
        check_for_updates(update_weak.clone(), Arc::clone(&update_state), true);
    });
    let install_weak = window.as_weak();
    window.on_install_update(move || {
        let Some(info) = available_update.lock().expect("update lock").clone() else {
            return;
        };
        if let Some(window) = install_weak.upgrade() {
            window.set_status_text(format!("正在下载 {}…", info.version).into());
        }
        let weak = install_weak.clone();
        thread::spawn(move || {
            let result = run_async(update::download_and_schedule(&info));
            let _ = slint::invoke_from_event_loop(move || match result {
                Ok(()) => {
                    if let Some(window) = weak.upgrade() {
                        window.set_status_text("更新已下载，正在重启…".into());
                    }
                    let _ = slint::quit_event_loop();
                }
                Err(error) => {
                    if let Some(window) = weak.upgrade() {
                        window.set_status_text(format!("安装更新失败：{error:#}").into());
                    }
                }
            });
        });
    });
}

fn check_for_updates(
    window: slint::Weak<SettingsWindow>,
    state: Arc<Mutex<Option<UpdateInfo>>>,
    report_no_update: bool,
) {
    thread::spawn(move || {
        let result = run_async(update::check(env!("CARGO_PKG_VERSION")));
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            match result {
                Ok(Some(info)) => {
                    window.set_update_available(true);
                    window.set_update_version(format!("v{}", info.version).into());
                    window.set_status_text(format!("发现新版本 v{}", info.version).into());
                    *state.lock().expect("update lock") = Some(info);
                }
                Ok(None) if report_no_update => {
                    window.set_update_available(false);
                    window.set_status_text("当前已是最新版本".into());
                    state.lock().expect("update lock").take();
                }
                Ok(None) => {}
                Err(error) if report_no_update => {
                    window.set_status_text(format!("检查更新失败：{error:#}").into());
                }
                Err(_) => {}
            }
        });
    });
}

fn install_capture_handler(
    settings: &SettingsWindow,
    config: Arc<Mutex<AppConfig>>,
    ocr: Arc<Mutex<OcrEngine>>,
    overlay_slot: Rc<RefCell<Option<OverlayWindow>>>,
    result_data: Arc<Mutex<Option<ResultData>>>,
    current_task: Arc<Mutex<Option<CancellationToken>>>,
    tray: slint::Weak<AppTray>,
) {
    let settings_weak = settings.as_weak();
    settings.on_start_capture(move || {
        let Some(settings) = settings_weak.upgrade() else {
            return;
        };
        if let Some(token) = current_task.lock().expect("task lock").take() {
            token.cancel();
            settings.set_status_text("正在取消当前任务…".into());
            return;
        }
        if let Some(overlay) = overlay_slot.borrow_mut().take() {
            let _ = overlay.hide();
            settings.set_status_text("已取消选区".into());
            return;
        }
        let overlay = match OverlayWindow::new() {
            Ok(window) => window,
            Err(error) => {
                settings.set_status_text(format!("无法打开选区：{error}").into());
                return;
            }
        };
        let display = capture::display_under_cursor();
        overlay
            .window()
            .set_position(slint::PhysicalPosition::new(display.x, display.y));
        overlay.window().set_size(
            slint::PhysicalSize::new(display.width, display.height)
                .to_logical(display.scale_factor),
        );

        let selected_weak = overlay.as_weak();
        let settings_for_pipeline = settings.as_weak();
        let config_for_pipeline = Arc::clone(&config);
        let ocr_for_pipeline = Arc::clone(&ocr);
        let result_data_for_pipeline = Arc::clone(&result_data);
        let task_for_pipeline = Arc::clone(&current_task);
        let tray_for_pipeline = tray.clone();
        let overlay_slot_for_pipeline = Rc::clone(&overlay_slot);
        overlay.on_selected(move |x, y, width, height| {
            if let Some(overlay) = selected_weak.upgrade() {
                let _ = overlay.hide();
            }
            overlay_slot_for_pipeline.borrow_mut().take();
            if let Some(settings) = settings_for_pipeline.upgrade() {
                settings.set_status_text("识别中：首次使用会加载 OCR 模型…".into());
            }
            show_status_toast("识别中：首次使用会加载 OCR 模型…", "info");

            let config = config_for_pipeline.lock().expect("config lock").clone();
            let settings_for_update = settings_for_pipeline.clone();
            let data_for_thread = Arc::clone(&result_data_for_pipeline);
            let task_for_thread = Arc::clone(&task_for_pipeline);
            let ocr_for_thread = Arc::clone(&ocr_for_pipeline);
            let selection = Selection {
                x,
                y,
                width,
                height,
            };
            let cancellation = CancellationToken::new();
            *task_for_thread.lock().expect("task lock") = Some(cancellation.clone());
            if let Some(tray) = tray_for_pipeline.upgrade() {
                tray.set_busy(true);
            }
            let tray_for_thread = tray_for_pipeline.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(140));
                let pipeline_result = execute_pipeline(
                    selection,
                    display,
                    &config,
                    &ocr_for_thread,
                    &cancellation,
                    settings_for_update.clone(),
                );
                let was_cancelled = cancellation.is_cancelled();
                let _ = slint::invoke_from_event_loop(move || {
                    task_for_thread.lock().expect("task lock").take();
                    if let Some(tray) = tray_for_thread.upgrade() {
                        tray.set_busy(false);
                    }
                    let Some(settings) = settings_for_update.upgrade() else {
                        return;
                    };
                    match pipeline_result {
                        Ok(data) => {
                            settings.set_status_text("翻译完成".into());
                            hide_status_toast();
                            *data_for_thread.lock().expect("result lock") = Some(data.clone());
                            show_result_window(data, settings.as_weak(), None);
                        }
                        Err(error) => {
                            if was_cancelled {
                                settings.set_status_text("已取消".into());
                                hide_status_toast();
                                RESULT_WINDOW.with(|slot| {
                                    if let Some(window) = slot.borrow().as_ref() {
                                        let _ = window.hide();
                                    }
                                    slot.borrow_mut().take();
                                });
                            } else {
                                let message = format!("处理失败：{error:#}");
                                show_status_toast(&message, "error");
                                settings.set_status_text(message.clone().into());
                                RESULT_WINDOW.with(|slot| {
                                    if let Some(window) = slot.borrow().as_ref() {
                                        window.set_is_translating(false);
                                        window.set_error_text(message.clone().into());
                                    } else {
                                        let _ = settings.show();
                                    }
                                });
                            }
                        }
                    }
                });
            });
        });

        *overlay_slot.borrow_mut() = Some(overlay.clone_strong());
        let _ = settings.hide();
        if let Err(error) = overlay.show() {
            settings.set_status_text(format!("显示选区失败：{error}").into());
            let _ = settings.show();
        }
        let settings_after = settings.as_weak();
        let overlay_for_restore = overlay.as_weak();
        let slot_for_cancel = Rc::clone(&overlay_slot);
        overlay.on_cancelled(move || {
            if let Some(overlay) = overlay_for_restore.upgrade() {
                let _ = overlay.hide();
            }
            slot_for_cancel.borrow_mut().take();
            if let Some(settings) = settings_after.upgrade() {
                let _ = settings.show();
                settings.set_status_text("已取消".into());
            }
        });
    });
}

fn execute_pipeline(
    selection: Selection,
    display: DisplayInfo,
    config: &AppConfig,
    ocr: &Arc<Mutex<OcrEngine>>,
    cancellation: &CancellationToken,
    status_window: slint::Weak<SettingsWindow>,
) -> Result<ResultData> {
    anyhow::ensure!(!cancellation.is_cancelled(), "任务已取消");
    let original = capture::capture_selection_on_display(selection, display)?;
    let mut blocks = ocr.lock().expect("ocr lock").recognize(&original)?;
    anyhow::ensure!(!cancellation.is_cancelled(), "任务已取消");
    anyhow::ensure!(!blocks.is_empty(), "选区中未识别到文字");
    if config.multimodal_fallback && config.multimodal_privacy_confirmed {
        for block in blocks.iter_mut().filter(|block| {
            block.confidence < config.multimodal_confidence_threshold.clamp(0.0, 1.0)
        }) {
            let rect = block.bounding_box();
            let width = rect.width.min(original.width().saturating_sub(rect.x));
            let height = rect.height.min(original.height().saturating_sub(rect.y));
            if width == 0 || height == 0 {
                continue;
            }
            let crop = original.crop_imm(rect.x, rect.y, width, height);
            if let Ok(recognized) =
                run_async(crate::translate::vision::recognize_text(&crop, config))
            {
                block.text = recognized;
            }
        }
    }

    let source_lines: Vec<String> = blocks.iter().map(|block| block.text.clone()).collect();
    let source = source_lines.join("\n");
    if config.copy_source {
        let _ = clipboard::copy_text(&source);
    }
    let (window_x, window_y) = result_position(selection, display);
    let initial_data = ResultData {
        translation: String::new(),
        image: original.clone(),
        window_x,
        window_y,
    };
    let initial_settings = status_window.clone();
    let initial_cancellation = cancellation.clone();
    let _ = slint::invoke_from_event_loop(move || {
        show_result_window(initial_data, initial_settings, Some(initial_cancellation));
    });

    let translator = create_translator(config)?;
    let line_count = source_lines.len();
    let translations_state = Arc::new(Mutex::new(vec![String::new(); line_count]));
    let stream_state = Arc::clone(&translations_state);
    let stream_original = original.clone();
    let stream_blocks = blocks.clone();
    let stream_font = config.translation_font.clone();
    let configured_text = (!config.smart_text_color)
        .then(|| parse_hex_color(&config.translation_text_color))
        .flatten();
    let configured_background = (!config.smart_text_color)
        .then(|| parse_hex_color(&config.translation_background_color))
        .flatten();
    let translations = run_async(translator.translate_stream(
        &source_lines,
        cancellation,
        move |index, translation| {
            if index >= line_count {
                return;
            }
            let current = {
                let mut state = stream_state.lock().expect("translation lock");
                state[index] = translation;
                state.clone()
            };
            let rendered = render::render_overlay_with_style(
                &stream_original,
                &stream_blocks,
                &current,
                Some(&stream_font),
                configured_text,
                configured_background,
            )
            .ok();
            let combined = current
                .iter()
                .filter(|line| !line.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let weak = status_window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    window.set_status_text(
                        format!("翻译中：{}/{}", index.saturating_add(1), line_count).into(),
                    );
                }
                show_status_toast(
                    &format!("翻译中：{}/{}", index.saturating_add(1), line_count),
                    "info",
                );
                RESULT_WINDOW.with(|slot| {
                    if let Some(window) = slot.borrow().as_ref() {
                        if let Some(rendered) = rendered {
                            window.set_result_image(to_slint_image(&rendered));
                        }
                        window.set_translation_text(combined.into());
                        window.set_status_text(
                            format!("翻译中：{}/{}", index.saturating_add(1), line_count).into(),
                        );
                    }
                });
            });
        },
    ))?;
    let rendered = render::render_overlay_with_style(
        &original,
        &blocks,
        &translations,
        Some(&config.translation_font),
        configured_text,
        configured_background,
    )?;
    let translation = translations.join("\n");
    if config.copy_translation {
        let _ = clipboard::copy_text(&translation);
    }

    if config.save_history
        && let Ok(history) = HistoryDb::open()
    {
        let _ = history.insert(&source, &translation);
    }
    Ok(ResultData {
        translation,
        image: rendered,
        window_x,
        window_y,
    })
}

fn result_position(selection: Selection, display: DisplayInfo) -> (i32, i32) {
    let preferred_right =
        display.x + ((selection.x + selection.width) * display.scale_factor) as i32 + 12;
    let window_width = (760.0 * display.scale_factor) as i32;
    let window_x = if preferred_right + window_width <= display.x + display.width as i32 {
        preferred_right
    } else {
        (display.x + (selection.x * display.scale_factor) as i32 - window_width - 12).max(display.x)
    };
    let window_y = (display.y + (selection.y * display.scale_factor) as i32).max(display.y);
    (window_x, window_y)
}

fn create_translator(config: &AppConfig) -> Result<Translator> {
    match config.engine {
        TranslationEngine::OpenAiCompatible => Ok(OpenAiTranslator::from_config(config)?.into()),
        TranslationEngine::Ollama => Ok(OllamaTranslator::new(
            &config.api_base,
            &config.model,
            &config.target_lang,
            config.proxy.as_deref(),
        )?
        .into()),
    }
}

fn show_result_window(
    data: ResultData,
    settings: slint::Weak<SettingsWindow>,
    cancellation: Option<CancellationToken>,
) {
    let Ok(window) = ResultWindow::new() else {
        return;
    };
    window
        .window()
        .set_position(slint::PhysicalPosition::new(data.window_x, data.window_y));
    window.set_result_image(to_slint_image(&data.image));
    window.set_translation_text(data.translation.clone().into());
    window.set_is_translating(cancellation.is_some());
    if cancellation.is_some() {
        window.set_status_text("正在翻译…".into());
    }

    let text = data.translation.clone();
    let status_weak = window.as_weak();
    window.on_copy_text(move || {
        if let Err(error) = clipboard::copy_text(&text) {
            if let Some(window) = status_weak.upgrade() {
                window.set_status_text(format!("复制失败：{error:#}").into());
            }
        }
    });

    let image = data.image.clone();
    let status_weak = window.as_weak();
    window.on_copy_image(move || {
        if let Err(error) = clipboard::copy_image(&image) {
            if let Some(window) = status_weak.upgrade() {
                window.set_status_text(format!("复制失败：{error:#}").into());
            }
        }
    });

    let image = data.image.clone();
    let status_weak = window.as_weak();
    window.on_save_image(move || match save_result_image(&image) {
        Ok(path) => {
            if let Some(window) = status_weak.upgrade() {
                window.set_status_text(format!("已保存：{}", path.display()).into());
            }
        }
        Err(error) => {
            if let Some(window) = status_weak.upgrade() {
                window.set_status_text(format!("保存失败：{error:#}").into());
            }
        }
    });

    let result_settings = settings.clone();
    window.on_open_settings(move || {
        if let Some(settings) = result_settings.upgrade() {
            let _ = settings.show();
        }
    });
    window.on_open_history(move || {
        let _ = show_history_window();
    });
    let retry_settings = settings.clone();
    let retry_weak = window.as_weak();
    window.on_retry(move || {
        if let Some(window) = retry_weak.upgrade() {
            let _ = window.hide();
        }
        if let Some(settings) = retry_settings.upgrade() {
            settings.invoke_start_capture();
        }
    });

    if let Some(cancellation) = cancellation.clone() {
        let cancel_weak = window.as_weak();
        window.on_cancel_translation(move || {
            cancellation.cancel();
            if let Some(window) = cancel_weak.upgrade() {
                window.set_status_text("正在取消…".into());
            }
        });
    }

    let close_weak = window.as_weak();
    window.on_close_requested(move || {
        if let Some(cancellation) = cancellation.as_ref() {
            cancellation.cancel();
        }
        if let Some(window) = close_weak.upgrade() {
            let _ = window.hide();
        }
        RESULT_WINDOW.with(|slot| slot.borrow_mut().take());
    });
    RESULT_WINDOW.with(|slot| *slot.borrow_mut() = Some(window.clone_strong()));
    let _ = window.show();
}

fn show_history_window() -> Result<()> {
    let entries = HistoryDb::open()?.list(0, 500)?;
    let window = HistoryWindow::new().context("创建历史窗口失败")?;
    set_history_entries(&window, &entries, "");

    let search_weak = window.as_weak();
    window.on_search_changed(move |query| {
        if let Some(window) = search_weak.upgrade()
            && let Ok(entries) = HistoryDb::open().and_then(|db| db.list(0, 500))
        {
            set_history_entries(&window, &entries, query.as_str());
        }
    });
    let source_weak = window.as_weak();
    window.on_copy_source(move |id| {
        if let Some(entry) = find_history_entry(id as i64) {
            let _ = clipboard::copy_text(&entry.source_text);
            if let Some(window) = source_weak.upgrade() {
                window.set_status_text("已复制原文".into());
            }
        }
    });
    let translation_weak = window.as_weak();
    window.on_copy_translation(move |id| {
        if let Some(entry) = find_history_entry(id as i64) {
            let _ = clipboard::copy_text(&entry.translated_text);
            if let Some(window) = translation_weak.upgrade() {
                window.set_status_text("已复制译文".into());
            }
        }
    });
    let clear_weak = window.as_weak();
    window.on_clear_history(move || {
        if HistoryDb::open()
            .and_then(|db| db.clear().map(|_| ()))
            .is_ok()
            && let Some(window) = clear_weak.upgrade()
        {
            set_history_entries(&window, &[], "");
            window.set_status_text("历史记录已清空".into());
        }
    });
    let export_weak = window.as_weak();
    window.on_export_history(move || {
        if let Some(window) = export_weak.upgrade() {
            window.set_status_text(match export_history() {
                Ok(path) => format!("已导出：{}", path.display()).into(),
                Err(error) => format!("导出失败：{error:#}").into(),
            });
        }
    });
    let close_weak = window.as_weak();
    window.on_close_requested(move || {
        if let Some(window) = close_weak.upgrade() {
            let _ = window.hide();
        }
        HISTORY_WINDOW.with(|slot| slot.borrow_mut().take());
    });
    HISTORY_WINDOW.with(|slot| *slot.borrow_mut() = Some(window.clone_strong()));
    window.show().context("显示历史窗口失败")
}

fn set_history_entries(
    window: &HistoryWindow,
    entries: &[crate::history::HistoryEntry],
    query: &str,
) {
    let query = query.to_lowercase();
    let items: Vec<HistoryItem> = entries
        .iter()
        .filter(|entry| {
            query.is_empty()
                || entry.source_text.to_lowercase().contains(&query)
                || entry.translated_text.to_lowercase().contains(&query)
        })
        .map(|entry| HistoryItem {
            id: entry.id as i32,
            created_at: format!("Unix {}", entry.created_at).into(),
            source_text: entry.source_text.clone().into(),
            translated_text: entry.translated_text.clone().into(),
        })
        .collect();
    window.set_entries(Rc::new(slint::VecModel::from(items)).into());
    window.set_status_text(format!("共 {} 条", entries.len()).into());
}

fn find_history_entry(id: i64) -> Option<crate::history::HistoryEntry> {
    HistoryDb::open()
        .and_then(|db| db.list(0, 500))
        .ok()?
        .into_iter()
        .find(|entry| entry.id == id)
}

fn export_history() -> Result<std::path::PathBuf> {
    let db = HistoryDb::open()?;
    let base = directories::UserDirs::new()
        .and_then(|dirs| dirs.document_dir().map(ToOwned::to_owned))
        .unwrap_or(std::env::current_dir()?);
    let directory = base.join("ScreenTranslator");
    std::fs::create_dir_all(&directory)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let json_path = directory.join(format!("history-{timestamp}.json"));
    let csv_path = directory.join(format!("history-{timestamp}.csv"));
    std::fs::write(&json_path, db.export_json()?)?;
    std::fs::write(csv_path, db.export_csv()?)?;
    Ok(json_path)
}

fn show_about_window() -> Result<()> {
    let window = AboutWindow::new().context("创建关于窗口失败")?;
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    window.set_build_info("本地 OCR · DeepSeek/OpenAI 兼容 · Ollama".into());
    window.set_copyright_text("MIT License · 2026 Screen Translator contributors".into());
    window.on_open_privacy_policy(|| {
        let _ = open_project_document("docs/privacy.md");
    });
    window.on_open_third_party_licenses(|| {
        let _ = open_project_document("docs/licenses.md");
    });
    window.on_open_project_homepage(|| {
        let _ = open_project_document("README.md");
    });
    let close_weak = window.as_weak();
    window.on_close_requested(move || {
        if let Some(window) = close_weak.upgrade() {
            let _ = window.hide();
        }
        ABOUT_WINDOW.with(|slot| slot.borrow_mut().take());
    });
    ABOUT_WINDOW.with(|slot| *slot.borrow_mut() = Some(window.clone_strong()));
    window.show().context("显示关于窗口失败")
}

fn open_project_document(relative: &str) -> Result<()> {
    let executable_root = std::env::current_exe()?
        .parent()
        .map(ToOwned::to_owned)
        .context("无法定位程序目录")?;
    let mut path = executable_root.join(relative);
    if !path.is_file() {
        path = std::env::current_dir()?.join(relative);
    }
    if !path.is_file()
        && let Some(name) = std::path::Path::new(relative).file_name()
    {
        path = executable_root.join(name);
    }
    anyhow::ensure!(path.is_file(), "文件不存在：{}", path.display());
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .context("无法打开文档")?;
    Ok(())
}

fn show_status_toast(message: &str, tone: &str) {
    STATUS_TOAST.with(|slot| {
        if slot.borrow().is_none() {
            let Ok(toast) = StatusToast::new() else {
                return;
            };
            let weak = toast.as_weak();
            toast.on_dismiss_requested(move || {
                if let Some(toast) = weak.upgrade() {
                    let _ = toast.hide();
                }
                STATUS_TOAST.with(|slot| slot.borrow_mut().take());
            });
            *slot.borrow_mut() = Some(toast);
        }
        if let Some(toast) = slot.borrow().as_ref() {
            toast.set_message(message.into());
            toast.set_tone(tone.into());
            let _ = toast.show();
        }
    });
}

fn hide_status_toast() {
    STATUS_TOAST.with(|slot| {
        if let Some(toast) = slot.borrow().as_ref() {
            let _ = toast.hide();
        }
        slot.borrow_mut().take();
    });
}

fn to_slint_image(image: &DynamicImage) -> slint::Image {
    let rgba = image.to_rgba8();
    let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    );
    slint::Image::from_rgba8(buffer)
}

fn save_result_image(image: &DynamicImage) -> Result<std::path::PathBuf> {
    let base = directories::UserDirs::new()
        .and_then(|dirs| dirs.picture_dir().map(ToOwned::to_owned))
        .unwrap_or(std::env::current_dir()?);
    let directory = base.join("ScreenTranslator");
    std::fs::create_dir_all(&directory)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = directory.join(format!("translation-{timestamp}.png"));
    image.save(&path)?;
    Ok(path)
}

fn run_async<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("创建异步运行时失败")?
        .block_on(future)
}
