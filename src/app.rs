use crate::{
    AboutWindow, AppTray, HistoryItem, HistoryWindow, OverlayWindow, SettingsWindow, StatusToast,
    capture::{self, DisplayInfo, Selection},
    config::{AppConfig, ProviderConfig, TranslationEngine},
    history::HistoryDb,
    ocr::OcrEngine,
    platform::{
        autostart::Autostart,
        foreground,
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
use slint::{ComponentHandle, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

thread_local! {
    static HISTORY_WINDOW: RefCell<Option<HistoryWindow>> = const { RefCell::new(None) };
    static ABOUT_WINDOW: RefCell<Option<AboutWindow>> = const { RefCell::new(None) };
    static STATUS_TOAST: RefCell<Option<StatusToast>> = const { RefCell::new(None) };
    // 最近一次翻译结果：原文/译文整屏图与文本，供遮罩工具栏切换显示与复制。
    // 仅 UI 线程访问（结果由 invoke_from_event_loop 写入）。
    static OVERLAY_RESULT: RefCell<Option<OverlayResult>> = const { RefCell::new(None) };
}

/// 翻译器缓存：reqwest 连接池随 Client 复用，避免每次翻译重新 TCP+TLS 握手。
/// 指纹是整份配置的 TOML 序列化（含加密 key），配置一变即重建。
static TRANSLATOR_CACHE: Mutex<Option<(String, Translator)>> = Mutex::new(None);

fn cached_translator(config: &AppConfig) -> Result<Translator> {
    let fingerprint = toml::to_string(config).unwrap_or_default();
    let mut cache = TRANSLATOR_CACHE.lock().expect("translator cache lock");
    if let Some((cached, translator)) = cache.as_ref()
        && *cached == fingerprint
    {
        return Ok(translator.clone());
    }
    let translator = create_translator(config)?;
    *cache = Some((fingerprint, translator.clone()));
    Ok(translator)
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
    let provider_drafts: ProviderDrafts = Rc::new(RefCell::new(Vec::new()));
    let editing_provider = Rc::new(Cell::new(0usize));
    apply_config(
        &settings,
        &config.lock().expect("config lock"),
        &provider_drafts,
        &editing_provider,
    );
    let ocr = Arc::new(Mutex::new(OcrEngine::default()));
    if config.lock().expect("config lock").prewarm_ocr {
        let ocr_for_prewarm = Arc::clone(&ocr);
        thread::spawn(move || {
            let _ = ocr_for_prewarm.lock().expect("ocr lock").prewarm();
        });
    }

    let overlay_slot = Rc::new(RefCell::new(None::<OverlayWindow>));
    let current_task = Arc::new(Mutex::new(None::<CancellationToken>));
    let available_update = Arc::new(Mutex::new(None::<UpdateInfo>));

    install_save_handler(
        &settings,
        Arc::clone(&config),
        Rc::clone(&provider_drafts),
        Rc::clone(&editing_provider),
    );
    install_test_handler(
        &settings,
        Arc::clone(&config),
        Rc::clone(&provider_drafts),
        Rc::clone(&editing_provider),
    );
    install_capture_handler(
        &settings,
        Arc::clone(&config),
        Arc::clone(&ocr),
        Rc::clone(&overlay_slot),
        Arc::clone(&current_task),
        tray.as_weak(),
    );

    install_settings_extras(
        &settings,
        Arc::clone(&config),
        Arc::clone(&ocr),
        Arc::clone(&available_update),
        Rc::clone(&provider_drafts),
        Rc::clone(&editing_provider),
    );
    // 每日后台自动更新：启动时先跑一轮，此后每 24 小时一轮。
    auto_update_tick(
        settings.as_weak(),
        Arc::clone(&available_update),
        Arc::clone(&current_task),
    );
    let daily_window = settings.as_weak();
    let daily_state = Arc::clone(&available_update);
    let daily_task = Arc::clone(&current_task);
    let daily_timer = slint::Timer::default();
    daily_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(24 * 60 * 60),
        move || {
            auto_update_tick(
                daily_window.clone(),
                Arc::clone(&daily_state),
                Arc::clone(&daily_task),
            );
        },
    );
    // 定时器须活到进程结束。
    std::mem::forget(daily_timer);

    let capture_weak = settings.as_weak();
    tray.on_capture(move || {
        if let Some(settings) = capture_weak.upgrade() {
            settings.invoke_start_capture();
        }
    });
    let settings_weak = settings.as_weak();
    tray.on_open_settings(move || {
        if let Some(settings) = settings_weak.upgrade() {
            show_and_focus_settings(&settings);
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
                    show_and_focus_settings(&settings);
                }
            });
        }
    })?;

    tray.show().context("显示托盘图标失败")?;
    let should_show_settings = !config
        .lock()
        .expect("config lock")
        .has_configured_cloud_provider();
    if should_show_settings {
        settings.show().context("显示设置窗口失败")?;
    }
    let result = slint::run_event_loop_until_quit().context("Slint 事件循环异常");
    drop(instance_listener);
    drop(hotkey);
    drop(instance);
    result
}

/// 显示设置窗口并拉到前台：程序驻留托盘时不是前台进程，
/// 仅 show() 窗口会落在其它窗口后面，用户以为“点了没反应”。
fn show_and_focus_settings(settings: &SettingsWindow) {
    let _ = settings.show();
    let hwnd = foreground::hwnd_of(settings.window());
    if hwnd != 0 {
        foreground::force_foreground(hwnd);
    }
}

fn parse_hotkey(value: &str) -> Option<Hotkey> {    let parts: Vec<String> = value
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

/// 设置页编辑中的供应商草稿：已保存的加密 key 原样保留，输入框只承载新 key。
#[derive(Clone)]
struct ProviderDraft {
    name: String,
    engine: TranslationEngine,
    api_base: String,
    models: Vec<String>,
    new_api_key: String,
    protected_api_key: String,
}

type ProviderDrafts = Rc<RefCell<Vec<ProviderDraft>>>;

const PRESETS: [&str; 6] = [
    "DeepSeek",
    "OpenAI",
    "OpenCode Go",
    "OpenCode Zen 免费",
    "Ollama",
    "自定义",
];

impl Default for ProviderDraft {
    fn default() -> Self {
        Self {
            name: "新供应商".to_owned(),
            engine: TranslationEngine::OpenAiCompatible,
            api_base: String::new(),
            models: Vec::new(),
            new_api_key: String::new(),
            protected_api_key: String::new(),
        }
    }
}

fn drafts_from_config(config: &AppConfig) -> Vec<ProviderDraft> {
    config
        .providers
        .iter()
        .map(|provider| ProviderDraft {
            name: provider.name.clone(),
            engine: provider.engine,
            api_base: provider.api_base.clone(),
            models: provider.models.clone(),
            new_api_key: String::new(),
            protected_api_key: provider.protected_api_key.clone(),
        })
        .collect()
}

fn preset_of(draft: &ProviderDraft) -> &'static str {
    if draft.engine == TranslationEngine::Ollama {
        return "Ollama";
    }
    let base = &draft.api_base;
    if base.contains("opencode.ai/zen/go") {
        "OpenCode Go"
    } else if base.contains("opencode.ai") {
        "OpenCode Zen 免费"
    } else if base.contains("deepseek") {
        "DeepSeek"
    } else if base.contains("api.openai.com") {
        "OpenAI"
    } else {
        "自定义"
    }
}

fn load_provider_fields(window: &SettingsWindow, drafts: &ProviderDrafts, editing: &Rc<Cell<usize>>) {
    // 先复制出字段再调用 setter：持有 borrow 期间若 setter 同步触发任何
    // 读取 drafts 的回调会造成 RefCell 双重借用 panic。
    let Some((name, preset, base, models, has_key)) = ({
        let drafts = drafts.borrow();
        drafts.get(editing.get()).map(|draft| {
            (
                draft.name.clone(),
                preset_of(draft),
                draft.api_base.clone(),
                draft.models.join(", "),
                !draft.protected_api_key.is_empty(),
            )
        })
    }) else {
        return;
    };
    window.set_provider_name(name.into());
    window.set_service_preset(preset.into());
    window.set_api_base(base.into());
    window.set_models(models.into());
    window.set_api_key(SharedString::default());
    window.set_provider_has_key(has_key);
}

fn commit_provider_fields(
    window: &SettingsWindow,
    drafts: &ProviderDrafts,
    editing: &Rc<Cell<usize>>,
) {
    // 先从窗口读出全部字段（getter 无回调），再一次性写草稿，
    // 最后才清空 key 输入框：borrow_mut 不跨越任何 setter。
    let name = window.get_provider_name().trim().to_owned();
    let engine = if window.get_service_preset() == "Ollama" {
        TranslationEngine::Ollama
    } else {
        TranslationEngine::OpenAiCompatible
    };
    let api_base = window.get_api_base().trim().to_owned();
    let models: Vec<String> = window
        .get_models()
        .split([',', ';', '，', '；'])
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
        .collect();
    let key = window.get_api_key().trim().to_owned();
    if let Some(draft) = drafts.borrow_mut().get_mut(editing.get()) {
        draft.name = name;
        draft.engine = engine;
        draft.api_base = api_base;
        draft.models = models;
        if !key.is_empty() {
            draft.new_api_key = key;
        }
    }
    window.set_api_key(SharedString::default());
}

fn refresh_provider_list(window: &SettingsWindow, drafts: &ProviderDrafts, editing: &Rc<Cell<usize>>) {
    let names: Vec<SharedString> = drafts
        .borrow()
        .iter()
        .enumerate()
        .map(|(index, draft)| {
            let name = if draft.name.is_empty() {
                preset_of(draft)
            } else {
                &draft.name
            };
            SharedString::from(format!("{}. {}", index + 1, name))
        })
        .collect();
    window.set_provider_names(ModelRc::new(VecModel::from(names)));
    window.set_current_provider(editing.get() as i32);
}

fn drafts_to_providers(drafts: &[ProviderDraft]) -> Result<Vec<ProviderConfig>> {
    let mut providers = Vec::with_capacity(drafts.len());
    for draft in drafts {
        anyhow::ensure!(
            !draft.api_base.is_empty(),
            "供应商「{}」缺少 API 地址",
            draft.name
        );
        anyhow::ensure!(!draft.models.is_empty(), "供应商「{}」缺少模型", draft.name);
        let mut provider = ProviderConfig {
            name: if draft.name.is_empty() {
                draft.api_base.clone()
            } else {
                draft.name.clone()
            },
            engine: draft.engine,
            api_base: draft.api_base.clone(),
            models: draft.models.clone(),
            protected_api_key: draft.protected_api_key.clone(),
        };
        if !draft.new_api_key.is_empty() {
            provider.set_api_key(&draft.new_api_key)?;
        }
        providers.push(provider);
    }
    Ok(providers)
}

fn apply_config(
    window: &SettingsWindow,
    config: &AppConfig,
    drafts: &ProviderDrafts,
    editing: &Rc<Cell<usize>>,
) {
    *drafts.borrow_mut() = drafts_from_config(config);
    if drafts.borrow().is_empty() {
        drafts.borrow_mut().push(ProviderDraft::default());
    }
    editing.set(0);
    refresh_provider_list(window, drafts, editing);
    load_provider_fields(window, drafts, editing);
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
}

fn config_from_window(window: &SettingsWindow, previous: &AppConfig) -> Result<AppConfig> {
    let mut config = previous.clone();
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
    Ok(config)
}

fn install_save_handler(
    window: &SettingsWindow,
    config: Arc<Mutex<AppConfig>>,
    drafts: ProviderDrafts,
    editing: Rc<Cell<usize>>,
) {
    let weak = window.as_weak();
    window.on_save_settings(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        commit_provider_fields(&window, &drafts, &editing);
        // 先取出 providers 再进闭包：`drafts.borrow()` 的临时 Ref 会活到整条语句
        // 结束，若在闭包内再 borrow_mut 会直接 panic（0xc0000409 崩溃的根因）。
        let providers = match drafts_to_providers(&drafts.borrow()) {
            Ok(providers) => providers,
            Err(error) => {
                window.set_status_text(format!("保存失败：{error:#}").into());
                return;
            }
        };
        let previous = config.lock().expect("config lock").clone();
        let result = (|| -> Result<bool> {
            let mut updated = config_from_window(&window, &previous)?;
            updated.providers = providers;
            let hotkey_changed = updated.hotkey != previous.hotkey;
            Autostart::for_current_exe("ScreenTranslator")?.set_enabled(updated.autostart)?;
            updated.save()?;
            *config.lock().expect("config lock") = updated.clone();
            // 新输入的 key 已加密并入配置，草稿以落盘结果重建。
            *drafts.borrow_mut() = drafts_from_config(&updated);
            Ok(hotkey_changed)
        })();
        match result {
            Ok(hotkey_changed) => {
                refresh_provider_list(&window, &drafts, &editing);
                load_provider_fields(&window, &drafts, &editing);
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

fn install_test_handler(
    window: &SettingsWindow,
    config: Arc<Mutex<AppConfig>>,
    drafts: ProviderDrafts,
    editing: Rc<Cell<usize>>,
) {
    let weak = window.as_weak();
    window.on_test_connection(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        commit_provider_fields(&window, &drafts, &editing);
        let providers = match drafts_to_providers(&drafts.borrow()) {
            Ok(providers) => providers,
            Err(error) => {
                window.set_status_text(format!("配置错误：{error:#}").into());
                return;
            }
        };
        let previous = config.lock().expect("config lock").clone();
        let updated = match config_from_window(&window, &previous) {
            Ok(mut updated) => {
                updated.providers = providers;
                updated
            }
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
    drafts: ProviderDrafts,
    editing: Rc<Cell<usize>>,
) {
    let cancel_weak = window.as_weak();
    let cancel_config = Arc::clone(&config);
    let cancel_drafts = Rc::clone(&drafts);
    let cancel_editing = Rc::clone(&editing);
    window.on_cancel_settings(move || {
        if let Some(window) = cancel_weak.upgrade() {
            apply_config(
                &window,
                &cancel_config.lock().expect("config lock"),
                &cancel_drafts,
                &cancel_editing,
            );
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

    // ---- 供应商列表：选择 / 增删 / 排序 ----
    let select_weak = window.as_weak();
    let select_drafts = Rc::clone(&drafts);
    let select_editing = Rc::clone(&editing);
    window.on_provider_selected(move |index| {
        let Some(window) = select_weak.upgrade() else {
            return;
        };
        commit_provider_fields(&window, &select_drafts, &select_editing);
        let len = select_drafts.borrow().len();
        select_editing.set((index.max(0) as usize).min(len.saturating_sub(1)));
        load_provider_fields(&window, &select_drafts, &select_editing);
    });

    let add_weak = window.as_weak();
    let add_drafts = Rc::clone(&drafts);
    let add_editing = Rc::clone(&editing);
    window.on_add_provider(move || {
        let Some(window) = add_weak.upgrade() else {
            return;
        };
        commit_provider_fields(&window, &add_drafts, &add_editing);
        add_drafts.borrow_mut().push(ProviderDraft::default());
        add_editing.set(add_drafts.borrow().len() - 1);
        refresh_provider_list(&window, &add_drafts, &add_editing);
        load_provider_fields(&window, &add_drafts, &add_editing);
        window.set_status_text("已添加供应商，请选择预设并填写 API Key".into());
    });

    let remove_weak = window.as_weak();
    let remove_drafts = Rc::clone(&drafts);
    let remove_editing = Rc::clone(&editing);
    window.on_remove_provider(move |index| {
        let Some(window) = remove_weak.upgrade() else {
            return;
        };
        let index = index.max(0) as usize;
        {
            let mut drafts = remove_drafts.borrow_mut();
            if index >= drafts.len() {
                return;
            }
            drafts.remove(index);
            if drafts.is_empty() {
                drafts.push(ProviderDraft::default());
            }
            let len = drafts.len();
            let current = remove_editing.get();
            remove_editing.set(if current == index {
                current.min(len - 1)
            } else if current > index {
                current - 1
            } else {
                current
            });
        }
        refresh_provider_list(&window, &remove_drafts, &remove_editing);
        load_provider_fields(&window, &remove_drafts, &remove_editing);
    });

    let move_weak = window.as_weak();
    let move_drafts = Rc::clone(&drafts);
    let move_editing = Rc::clone(&editing);
    window.on_move_provider(move |index, delta| {
        let Some(window) = move_weak.upgrade() else {
            return;
        };
        commit_provider_fields(&window, &move_drafts, &move_editing);
        let index = index.max(0) as usize;
        {
            let mut drafts = move_drafts.borrow_mut();
            let len = drafts.len();
            let Some(target) = index.checked_add_signed(delta as isize) else {
                return;
            };
            if index >= len || target >= len {
                return;
            }
            drafts.swap(index, target);
            let current = move_editing.get();
            move_editing.set(if current == index {
                target
            } else if current == target {
                index
            } else {
                current
            });
        }
        refresh_provider_list(&window, &move_drafts, &move_editing);
        load_provider_fields(&window, &move_drafts, &move_editing);
    });

    let preset_weak = window.as_weak();
    let preset_drafts = Rc::clone(&drafts);
    let preset_editing = Rc::clone(&editing);
    window.on_service_preset_changed(move |preset| {
        let Some(window) = preset_weak.upgrade() else {
            return;
        };
        let (name, base, models) = match preset.as_str() {
            "DeepSeek" => ("DeepSeek", "https://api.deepseek.com/v1", "deepseek-v4-flash"),
            "OpenAI" => ("OpenAI", "https://api.openai.com/v1", "gpt-4.1-mini"),
            "OpenCode Go" => (
                "OpenCode Go",
                "https://opencode.ai/zen/go/v1",
                "deepseek-v4-flash, glm-5.1",
            ),
            "OpenCode Zen 免费" => (
                "OpenCode Zen 免费",
                "https://opencode.ai/zen/v1",
                "big-pickle, mimo-v2.5-free, deepseek-v4-flash-free",
            ),
            "Ollama" => ("Ollama", "http://127.0.0.1:11434", "qwen3"),
            _ => return,
        };
        window.set_api_base(base.into());
        window.set_models(models.into());
        let current = window.get_provider_name();
        if current.trim().is_empty() || PRESETS.contains(&current.as_str()) || current == "新供应商" {
            window.set_provider_name(name.into());
        }
        commit_provider_fields(&window, &preset_drafts, &preset_editing);
        refresh_provider_list(&window, &preset_drafts, &preset_editing);
        if preset == "OpenCode Go" || preset == "OpenCode Zen 免费" {
            window.set_status_text("在 opencode.ai 控制台复制同一把 API Key 填入即可".into());
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

/// 每日后台自动更新：静默检查 → 有新版自动下载 → 空闲时重启应用完成替换。
/// 检查/下载失败静默跳过，下个周期重试。有翻译任务在跑时不打断：更新脚本
/// 已在等待进程退出，会在下次自然退出/重启时生效，同时走设置页手动安装路径。
fn auto_update_tick(
    window: slint::Weak<SettingsWindow>,
    state: Arc<Mutex<Option<UpdateInfo>>>,
    current_task: Arc<Mutex<Option<CancellationToken>>>,
) {
    thread::spawn(move || {
        let Ok(Some(info)) = run_async(update::check(env!("CARGO_PKG_VERSION"))) else {
            return;
        };
        if let Err(error) = run_async(update::download_and_schedule(&info)) {
            tracing::warn!(error = %format!("{error:#}"), "auto update download failed");
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            *state.lock().expect("update lock") = Some(info.clone());
            if let Some(window) = window.upgrade() {
                window.set_update_available(true);
                window.set_update_version(format!("v{}", info.version).into());
            }
            if current_task.lock().expect("task lock").is_some() {
                if let Some(window) = window.upgrade() {
                    window.set_status_text(
                        format!("新版本 v{} 已下载，将在下次重启时生效", info.version).into(),
                    );
                }
                return;
            }
            tracing::info!(version = %info.version, "auto update downloaded, restarting");
            if let Some(window) = window.upgrade() {
                window.set_status_text(
                    format!("已自动下载 v{}，正在重启完成更新…", info.version).into(),
                );
            }
            let _ = slint::quit_event_loop();
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
    current_task: Arc<Mutex<Option<CancellationToken>>>,
    tray: slint::Weak<AppTray>,
) {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

    // 遮罩是否正显示在屏幕上（常驻实例本身一直活在 overlay_slot 里）。
    let overlay_active = Arc::new(AtomicBool::new(false));
    // 流水线代次：每次确认选区/重新拖选时 +1。旧任务的流式回调与完成回调
    // 发现代次过期必须整体丢弃——否则会 park 遮罩、抢走新任务的取消令牌、
    // 或把过期译文刷到已恢复的原始冻结帧上。
    let pipeline_epoch = Arc::new(AtomicU64::new(0));
    // 按下热键时的前台窗口，关闭遮罩时归还焦点（0 = 无记录）。
    let previous_foreground = Arc::new(AtomicIsize::new(0));
    // 每次截屏的上下文（显示器信息 + 冻结帧），供 on_selected 读取。
    let capture_ctx = Rc::new(RefCell::new(None::<(DisplayInfo, Arc<DynamicImage>)>));

    let settings_weak = settings.as_weak();
    {
        let overlay_slot = Rc::clone(&overlay_slot);
        let overlay_active = Arc::clone(&overlay_active);
        let capture_ctx = Rc::clone(&capture_ctx);
        let current_task = Arc::clone(&current_task);
        let previous_foreground = Arc::clone(&previous_foreground);
        let config = Arc::clone(&config);
        let ocr = Arc::clone(&ocr);
        settings.on_start_capture(move || {
            let Some(settings) = settings_weak.upgrade() else {
                return;
            };
            if let Some(token) = current_task.lock().expect("task lock").take() {
                token.cancel();
                // 流水线取消后遮罩一并收起（流水线完成回调里也会兜底）。
                if overlay_active.swap(false, Ordering::SeqCst)
                    && let Some(overlay) = overlay_slot.borrow().as_ref()
                {
                    park_overlay(overlay);
                    foreground::restore(previous_foreground.swap(0, Ordering::SeqCst));
                    *capture_ctx.borrow_mut() = None;
                }
                settings.set_status_text("正在取消当前任务…".into());
                return;
            }
            if overlay_active.load(Ordering::SeqCst)
                && let Some(overlay) = overlay_slot.borrow().as_ref()
            {
                let was_showing_result = overlay.get_locked();
                park_overlay(overlay);
                overlay_active.store(false, Ordering::SeqCst);
                if !was_showing_result {
                    // 仍在框选阶段：热键视为取消选区。
                    foreground::restore(previous_foreground.swap(0, Ordering::SeqCst));
                    settings.set_status_text("已取消选区".into());
                    // 热键预热的模型用不上了，卸载归还内存（worker 在跑则拿不到
                    // 锁，由流水线结束兜底卸载）。
                    if let Ok(mut engine) = ocr.try_lock() {
                        engine.unload();
                        trim_working_set();
                    }
                    // 冻结帧（8MB+）不再使用，随取消一并释放。
                    *capture_ctx.borrow_mut() = None;
                    return;
                }
                // 结果展示态：收起旧结果后延时重新进入截屏——
                // 立即截屏会把还没离开合成画面的旧结果冻结进新底图（残影）。
                foreground::restore(previous_foreground.swap(0, Ordering::SeqCst));
                let restart_weak = settings.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(120), move || {
                    if let Some(settings) = restart_weak.upgrade() {
                        settings.invoke_start_capture();
                    }
                });
                return;
            }
            let display = capture::display_under_cursor();
            // 冻结帧：先截取当前屏幕，遮罩底图与后续裁剪共用这一帧，
            // 不再需要“隐藏遮罩 → 等待 → 重新截屏”。
            let frame = match capture::capture_display_frame(&display) {
                Ok(frame) => Arc::new(frame),
                Err(error) => {
                    let message = format!("截屏失败：{error:#}");
                    tracing::error!("{message}");
                    settings.set_status_text(message.clone().into());
                    show_status_toast(&message, "error");
                    return;
                }
            };
            // 转储冻结帧用于诊断“遮罩黑屏”类问题（后台线程，不阻塞遮罩显示）。
            {
                let frame_for_dump = Arc::clone(&frame);
                thread::spawn(move || dump_frame_for_diagnostics(&frame_for_dump));
            }
            // 连接预热：TCP+TLS 握手藏进用户拖选的几秒，翻译开始时连接已就绪。
            // 60 秒内重复截屏不重复预热（reqwest 连接池空闲保活约 90 秒）。
            {
                static LAST_PREWARM: AtomicU64 = AtomicU64::new(0);
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or(0);
                if now.saturating_sub(LAST_PREWARM.load(Ordering::SeqCst)) > 60 {
                    LAST_PREWARM.store(now, Ordering::SeqCst);
                    let config = config.lock().expect("config lock").clone();
                    thread::spawn(move || {
                        if let Ok(translator) = cached_translator(&config) {
                            translator.prewarm();
                        }
                    });
                }
            }
            // OCR 模型预热：会话在每次流水线结束后即卸载（见 worker 线程），
            // 重载约 0.4s 藏进拖选时间，识别开始时模型已就绪。
            {
                let ocr = Arc::clone(&ocr);
                thread::spawn(move || {
                    if let Ok(mut engine) = ocr.lock() {
                        let _ = engine.prewarm();
                    }
                });
            }
            let Some(overlay) = overlay_slot.borrow().as_ref().map(|o| o.clone_strong()) else {
                // 预热失败等极端情况下才走到这里。
                settings.set_status_text("遮罩未初始化，请重启程序".into());
                return;
            };
            // 复用常驻实例前重置选区状态。
            overlay.set_sel_x(0.0);
            overlay.set_sel_y(0.0);
            overlay.set_sel_w(0.0);
            overlay.set_sel_h(0.0);
            overlay.set_locked(false);
            overlay.set_has_result(false);
            overlay.set_show_original(false);
            overlay.set_notice_text("".into());
            OVERLAY_RESULT.with(|state| *state.borrow_mut() = None);
            overlay.set_status_text("拖动框选 · Esc / 右键 / × 取消".into());
            overlay.set_frame_image(to_slint_image(&frame));
            *capture_ctx.borrow_mut() = Some((display.clone(), Arc::clone(&frame)));
            // 无动画上场：窗口先以最终尺寸显示在屏幕外，几何（含 DPI 缩放）
            // 稳定后再一次性移入目标显示器，避免“先小窗再放大”的跳变。
            overlay.window().set_size(slint::PhysicalSize::new(
                display.width.max(1),
                display.height.max(1) + 1,
            ));
            overlay
                .window()
                .set_position(slint::PhysicalPosition::new(-32000, -32000));

            let _ = settings.hide();
            if let Err(error) = overlay.show() {
                settings.set_status_text(format!("显示选区失败：{error}").into());
                let _ = settings.show();
                return;
            }
            overlay_active.store(true, Ordering::SeqCst);
            // 趁窗口还在屏幕外完成前台激活：过渡帧不可见。
            // 否则首次点击才激活，全屏无边框窗口的前台切换会闪黑一帧。
            let own_hwnd = foreground::hwnd_of(overlay.window());
            if own_hwnd != 0 {
                let current = foreground::current();
                if current != own_hwnd {
                    previous_foreground.store(current, Ordering::SeqCst);
                }
                foreground::force_foreground(own_hwnd);
            }
            // 等新冻结帧渲染完成后再移入屏幕：窗口离屏时 Windows 不发绘制消息，
            // 若按固定延时移入，第一帧会短暂呈现上一次的内容。
            overlay.window().request_redraw();
            let reveal_weak = overlay.as_weak();
            let reveal_display = display.clone();
            let reveal_once = Rc::new(std::cell::Cell::new(false));
            let reveal = {
                let reveal_once = Rc::clone(&reveal_once);
                move || {
                    if reveal_once.replace(true) {
                        return;
                    }
                    if let Some(overlay) = reveal_weak.upgrade() {
                        apply_overlay_geometry(&overlay, &reveal_display);
                    }
                }
            };
            {
                let mut reveal_once_cb = Some(reveal.clone());
                let _ = overlay.window().set_rendering_notifier(move |state, _api| {
                    if matches!(state, slint::RenderingState::AfterRendering)
                        && let Some(reveal) = reveal_once_cb.take()
                    {
                        reveal();
                    }
                });
            }
            // 兜底：离屏渲染被跳过时也能上场（此时底图已在上次 park 时清空，
            // 即使先上场也只会看到黑底而非旧内容）。
            slint::Timer::single_shot(std::time::Duration::from_millis(400), move || reveal());
        });
    }

    // ---- 常驻遮罩实例：创建 + 回调安装 + 离屏预热（仅启动时执行一次）----
    let overlay = match OverlayWindow::new() {
        Ok(window) => window,
        Err(error) => {
            settings.set_status_text(format!("无法初始化选区遮罩：{error}").into());
            return;
        }
    };

    // on_selected 的 move 闭包会取走 current_task/tray 所有权；
    // 重选处理器需在其移动前自备克隆。
    let reselect_task = Arc::clone(&current_task);
    let reselect_tray = tray.clone();
    let ocr_for_cancel = Arc::clone(&ocr);
    {
        let selected_weak = overlay.as_weak();
        let settings_for_pipeline = settings.as_weak();
        let capture_ctx = Rc::clone(&capture_ctx);
        let overlay_active = Arc::clone(&overlay_active);
        let previous_foreground = Arc::clone(&previous_foreground);
        let pipeline_epoch = Arc::clone(&pipeline_epoch);
        overlay.on_selected(move |x, y, width, height| {
            let Some((display, frame)) = capture_ctx.borrow().clone() else {
                return;
            };
            let epoch_id = pipeline_epoch.fetch_add(1, Ordering::SeqCst) + 1;
            let scale = selected_weak
                .upgrade()
                .map(|overlay| overlay.window().scale_factor() as f32)
                .unwrap_or(display.scale_factor)
                .max(1.0);
            // 不隐藏遮罩：锁定选区，进度直接写在遮罩提示条上。
            if let Some(overlay) = selected_weak.upgrade() {
                overlay.set_locked(true);
                overlay.set_notice_text("".into());
                overlay.set_status_text("识别中：首次使用会加载 OCR 模型…".into());
            }
            hide_status_toast();
            if let Some(settings) = settings_for_pipeline.upgrade() {
                settings.set_status_text("识别中：首次使用会加载 OCR 模型…".into());
            }

            let config = config.lock().expect("config lock").clone();
            let settings_for_update = settings_for_pipeline.clone();
            let task_for_thread = Arc::clone(&current_task);
            let ocr_for_thread = Arc::clone(&ocr);
            // Slint reports logical coordinates; capture expects physical pixels.
            let selection = Selection {
                x: x * scale,
                y: y * scale,
                width: width * scale,
                height: height * scale,
            };
            tracing::info!(
                logical = ?[x, y, width, height],
                scale,
                physical = ?[selection.x, selection.y, selection.width, selection.height],
                "selection confirmed"
            );
            // OCR 裁片外扩：紧贴文字的小选区会让检测器丢失上下文（识别率
            // 下降、单行被切碎）。扩大裁剪范围供检测，识别后按块中心过滤回
            // 原选区（content）；渲染/复制/合成仍只针对原选区内容。
            let (ocr_selection, content) = expand_for_ocr(selection, frame.width(), frame.height());
            let original = match capture::crop_selection(&frame, ocr_selection) {
                Ok(image) => image,
                Err(error) => {
                    let message = format!("裁剪选区失败：{error:#}");
                    tracing::error!("{message}");
                    if let Some(overlay) = selected_weak.upgrade() {
                        overlay.set_notice_text(message.clone().into());
                        overlay.set_status_text(message.clone().into());
                    }
                    if let Some(settings) = settings_for_pipeline.upgrade() {
                        settings.set_status_text(message.into());
                    }
                    return;
                }
            };
            tracing::info!(width = original.width(), height = original.height(), "selection cropped");
            let cancellation = CancellationToken::new();
            *task_for_thread.lock().expect("task lock") = Some(cancellation.clone());
            if let Some(tray) = tray.upgrade() {
                tray.set_busy(true);
            }
            let tray_for_thread = tray.clone();
            let overlay_for_thread = selected_weak.clone();
            let active_for_thread = Arc::clone(&overlay_active);
            let foreground_for_thread = Arc::clone(&previous_foreground);
            let frame_for_result = Arc::clone(&frame);
            let epoch_for_thread = Arc::clone(&pipeline_epoch);
            thread::spawn(move || {
                let pipeline_result = execute_pipeline(
                    original,
                    ocr_selection,
                    content,
                    frame,
                    &config,
                    &ocr_for_thread,
                    &cancellation,
                    Arc::clone(&epoch_for_thread),
                    epoch_id,
                    overlay_for_thread.clone(),
                    settings_for_update.clone(),
                );
                // 推理结束即卸载 OCR 会话：ORT 会话持有全部推理缓冲（常驻时
                // 进程私有内存顶在推理高水位不归还，实测大裁片两轮后 850MB），
                // 卸载即全量归还；重载仅约 0.4s，由热键预热掩盖。
                if let Ok(mut engine) = ocr_for_thread.lock() {
                    engine.unload();
                }
                trim_working_set();
                let was_cancelled = cancellation.is_cancelled();
                let _ = slint::invoke_from_event_loop(move || {
                    // 代次过期（用户已重新拖选）：整体丢弃，不得取走新任务的
                    // 取消令牌、park 遮罩或写入任何 UI 状态。
                    if epoch_for_thread.load(Ordering::SeqCst) != epoch_id {
                        return;
                    }
                    task_for_thread.lock().expect("task lock").take();
                    if let Some(tray) = tray_for_thread.upgrade() {
                        tray.set_busy(false);
                    }
                    match pipeline_result {
                        Ok(output) => {
                            let PipelineOutput {
                                composited,
                                source,
                                translation,
                            } = output;
                            // 结果原地贴回遮罩：冻结帧上选区位置已替换为译文渲染图。
                            // 原文/译文各缓存一份整屏图，工具栏切换显示零重渲染。
                            let translated = to_slint_image(&composited);
                            let original_frame = to_slint_image(&frame_for_result);
                            if let Some(overlay) = overlay_for_thread.upgrade() {
                                overlay.set_frame_image(translated.clone());
                                overlay.set_has_result(true);
                                overlay.set_show_original(false);
                                overlay.set_status_text(
                                    "翻译完成 · 选框下方工具条：切换/复制/×关闭".into(),
                                );
                            }
                            OVERLAY_RESULT.with(|state| {
                                *state.borrow_mut() = Some(OverlayResult {
                                    original: original_frame,
                                    translated,
                                    source,
                                    translation,
                                });
                            });
                            if let Some(settings) = settings_for_update.upgrade() {
                                settings.set_status_text("翻译完成".into());
                            }
                        }
                        Err(error) => {
                            if was_cancelled {
                                if let Some(overlay) = overlay_for_thread.upgrade() {
                                    park_overlay(&overlay);
                                }
                                active_for_thread.store(false, Ordering::SeqCst);
                                foreground::restore(foreground_for_thread.swap(0, Ordering::SeqCst));
                                if let Some(settings) = settings_for_update.upgrade() {
                                    settings.set_status_text("已取消".into());
                                }
                            } else {
                                let message = format!("处理失败：{error:#}");
                                tracing::error!("{message}");
                                // 不弹独立 toast：提示落在选框下方，遮罩保持
                                // 可交互（Esc/右键关闭，可直接重新拖选）。
                                if let Some(overlay) = overlay_for_thread.upgrade() {
                                    overlay.set_notice_text(message.clone().into());
                                    overlay.set_status_text(message.clone().into());
                                }
                                if let Some(settings) = settings_for_update.upgrade() {
                                    settings.set_status_text(message.into());
                                }
                            }
                        }
                    }
                });
            });
        });
    }

    {
        // 锁定态（识别中/结果展示）下用户按下左键重新拖选：取消旧任务、
        // 恢复原始冻结帧、清空结果态；松开后 on_selected 走正常流水线。
        let reselect_weak = overlay.as_weak();
        let reselect_epoch = Arc::clone(&pipeline_epoch);
        let reselect_ctx = Rc::clone(&capture_ctx);
        overlay.on_reselect_started(move || {
            reselect_epoch.fetch_add(1, Ordering::SeqCst);
            if let Some(token) = reselect_task.lock().expect("task lock").take() {
                token.cancel();
                if let Some(tray) = reselect_tray.upgrade() {
                    tray.set_busy(false);
                }
            }
            let Some(overlay) = reselect_weak.upgrade() else {
                return;
            };
            overlay.set_locked(false);
            overlay.set_has_result(false);
            overlay.set_show_original(false);
            overlay.set_notice_text("".into());
            OVERLAY_RESULT.with(|state| *state.borrow_mut() = None);
            // 结果态的 frame-image 已把译文烙进去，必须从截屏上下文恢复原始帧。
            if let Some((_, frame)) = reselect_ctx.borrow().as_ref() {
                overlay.set_frame_image(to_slint_image(frame));
            }
            overlay.set_status_text("拖动框选 · Esc / 右键 / × 取消".into());
        });
    }

    {
        let settings_after = settings.as_weak();
        let overlay_for_restore = overlay.as_weak();
        let overlay_active = Arc::clone(&overlay_active);
        let previous_foreground = Arc::clone(&previous_foreground);
        let capture_ctx = Rc::clone(&capture_ctx);
        overlay.on_cancelled(move || {
            // 框选阶段取消（Esc/×/右键）：热键预热的模型用不上了，卸载归还
            // 内存。worker 在跑则拿不到锁——流水线结束兜底卸载，不阻塞 UI。
            if let Ok(mut engine) = ocr_for_cancel.try_lock() {
                engine.unload();
                trim_working_set();
            }
            // 遮罩已收起，冻结帧与结果图（各 8MB+）一并释放，不留到下次截屏。
            *capture_ctx.borrow_mut() = None;
            OVERLAY_RESULT.with(|state| *state.borrow_mut() = None);
            if let Some(overlay) = overlay_for_restore.upgrade() {
                park_overlay(&overlay);
            }
            overlay_active.store(false, Ordering::SeqCst);
            foreground::restore(previous_foreground.swap(0, Ordering::SeqCst));
            if let Some(settings) = settings_after.upgrade() {
                // Stay in tray mode; do not force the settings window open.
                settings.set_status_text("已关闭".into());
            }
        });
    }

    {
        let view_weak = overlay.as_weak();
        overlay.on_view_toggled(move |show_original| {
            OVERLAY_RESULT.with(|state| {
                let state = state.borrow();
                let (Some(overlay), Some(result)) = (view_weak.upgrade(), state.as_ref()) else {
                    return;
                };
                overlay.set_frame_image(if show_original {
                    result.original.clone()
                } else {
                    result.translated.clone()
                });
                overlay.set_status_text(
                    if show_original {
                        "显示：原文 · Esc / 右键 / × 关闭"
                    } else {
                        "显示：译文 · Esc / 右键 / × 关闭"
                    }
                    .into(),
                );
            });
        });
    }

    {
        let copy_weak = overlay.as_weak();
        overlay.on_copy_requested(move || {
            OVERLAY_RESULT.with(|state| {
                let state = state.borrow();
                let (Some(overlay), Some(result)) = (copy_weak.upgrade(), state.as_ref()) else {
                    return;
                };
                // 一键复制跟随当前显示态：显示原文复制原文，显示译文复制译文。
                let (text, what) = if overlay.get_show_original() {
                    (&result.source, "原文")
                } else {
                    (&result.translation, "译文")
                };
                let message = match clipboard::copy_text(text) {
                    Ok(()) => format!("已复制{what}"),
                    Err(error) => format!("复制失败：{error:#}"),
                };
                overlay.set_status_text(message.into());
            });
        });
    }

    // 预热：首个 winit 窗口 show 时才初始化渲染上下文/表面，
    // 启动时即离屏显示并保持常驻，避免用户首次截屏时的首帧黑闪。
    overlay
        .window()
        .set_size(slint::PhysicalSize::new(64, 64));
    overlay
        .window()
        .set_position(slint::PhysicalPosition::new(-32000, -32000));
    let _ = overlay.show();
    // 常驻遮罩不能占任务栏按钮。window_handle 要等事件循环跑过一轮才可用，
    // 所以延迟补设；此后每次上场（apply_overlay_geometry）也会幂等重设。
    let tool_weak = overlay.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(250), move || {
        if let Some(overlay) = tool_weak.upgrade() {
            foreground::make_tool_window(foreground::hwnd_of(overlay.window()));
        }
    });
    *overlay_slot.borrow_mut() = Some(overlay);
}

/// 把常驻遮罩停到屏幕外：窗口保持映射不销毁，避免 hide/show 的 DWM 过渡黑帧。
/// 同时清空底图：下次上场若渲染兜底先触发，呈现的是与窗口底色一致的黑，
/// 而不是上一次的内容（残影）。
fn park_overlay(overlay: &OverlayWindow) {
    overlay.set_frame_image(slint::Image::default());
    overlay
        .window()
        .set_position(slint::PhysicalPosition::new(-32000, -32000));
}

/// 推理/渲染高峰后主动把工作集还给系统：任务管理器的“内存”列立即回落，
/// 而不是等 OS 在内存压力下懒回收。页面进 standby，下次使用软缺页召回，
/// 开销可忽略。仅在空闲时调用（worker 在跑时调用会拖慢推理）。
#[cfg(windows)]
fn trim_working_set() {
    use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let _ = EmptyWorkingSet(GetCurrentProcess());
    }
}

fn apply_overlay_geometry(overlay: &OverlayWindow, display: &DisplayInfo) {
    // Win32 monitor rect is already in physical pixels under Per-Monitor DPI awareness.
    // Pass PhysicalSize directly so winit does not multiply by scale again.
    //
    // 高度故意 +1px：NVIDIA 驱动会把“无边框 + 置顶 + 精确全屏”的 GL 窗口提升为
    // direct-flip（绕开 DWM 合成直接上屏），首次交互/关闭的切换瞬间会闪黑一帧。
    // 高度多出 1px（底部被屏幕边缘裁掉，不可见）让窗口不再满足全屏提升条件。
    foreground::make_tool_window(foreground::hwnd_of(overlay.window()));
    overlay
        .window()
        .set_position(slint::PhysicalPosition::new(display.x, display.y));
    overlay.window().set_size(slint::PhysicalSize::new(
        display.width.max(1),
        display.height.max(1) + 1,
    ));
}

/// 遮罩结果态缓存：原始冻结帧与译文合成图各一份，切换显示零重渲染。
struct OverlayResult {
    original: slint::Image,
    translated: slint::Image,
    source: String,
    translation: String,
}

/// OCR → 翻译流水线的产出：贴回冻结帧的整屏合成图 + 原文/译文文本。
/// 选区位置已被渲染了译文的选区图替换，可直接作为遮罩底图展示。
struct PipelineOutput {
    composited: DynamicImage,
    source: String,
    translation: String,
}

fn execute_pipeline(
    original: DynamicImage,
    selection: Selection,
    content: crate::ocr::Rect,
    frame: Arc<DynamicImage>,
    config: &AppConfig,
    ocr: &Arc<Mutex<OcrEngine>>,
    cancellation: &CancellationToken,
    epoch: Arc<AtomicU64>,
    epoch_id: u64,
    overlay_window: slint::Weak<OverlayWindow>,
    status_window: slint::Weak<SettingsWindow>,
) -> Result<PipelineOutput> {
    anyhow::ensure!(!cancellation.is_cancelled(), "任务已取消");
    let ocr_started = std::time::Instant::now();
    let raw_blocks = ocr.lock().expect("ocr lock").recognize(&original)?;
    // 裁片已外扩：丢弃扩边带进来的相邻行，只留中心落在原选区内的块。
    let mut blocks = crate::ocr::filter_to_region(&raw_blocks, &content);
    tracing::info!(
        blocks = blocks.len(),
        raw = raw_blocks.len(),
        elapsed_ms = ocr_started.elapsed().as_millis(),
        "ocr finished"
    );
    anyhow::ensure!(!cancellation.is_cancelled(), "任务已取消");
    anyhow::ensure!(!blocks.is_empty(), "选区中未识别到文字");
    if config.multimodal_fallback && config.multimodal_privacy_confirmed {
        let vision_provider = config.providers.iter().find(|provider| {
            provider.engine == TranslationEngine::OpenAiCompatible
                && provider.api_key().ok().flatten().is_some()
        });
        if let Some(vision_provider) = vision_provider {
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
                if let Ok(recognized) = run_async(crate::translate::vision::recognize_text(
                    &crop,
                    vision_provider,
                    config.proxy.as_deref(),
                )) {
                    block.text = recognized;
                }
            }
        }
    }

    // 同一视觉行被检测器切碎时并回一行：紧贴单行的小裁片会在图标/大间距处
    // 断行，碎块独立翻译丢上下文，小框渲染还会压缩字号。行与行之间不合并。
    let blocks = crate::ocr::merge_line_fragments(&blocks);
    tracing::info!(blocks = blocks.len(), "ocr blocks after line merge");
    let source_lines: Vec<String> = blocks.iter().map(|block| block.text.clone()).collect();
    let source = source_lines.join("\n");
    if config.copy_source {
        let _ = clipboard::copy_text(&source);
    }

    let translator = cached_translator(config)?;
    let translate_started = std::time::Instant::now();
    let first_line_logged = Arc::new(AtomicBool::new(false));
    let line_count = source_lines.len();
    let translations_state = Arc::new(Mutex::new(vec![String::new(); line_count]));
    let stream_state = Arc::clone(&translations_state);
    let stream_original = original.clone();
    let stream_blocks = blocks.clone();
    let stream_font = config.translation_font.clone();
    let stream_frame = Arc::clone(&frame);
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
            // 用户已重新拖选（代次过期）：丢弃旧任务的流式输出，
            // 不再渲染/刷写遮罩。
            if epoch.load(Ordering::SeqCst) != epoch_id || index >= line_count {
                return;
            }
            if !first_line_logged.swap(true, Ordering::SeqCst) {
                tracing::info!(
                    elapsed_ms = translate_started.elapsed().as_millis(),
                    "first translated line"
                );
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
            let progress = format!("翻译中：{}/{}", index.saturating_add(1), line_count);
            // 流式中间结果也原地贴回遮罩，用户实时看到译文逐行填充。
            let overlay_for_update = overlay_window.clone();
            let frame_for_update = Arc::clone(&stream_frame);
            let settings_for_update = status_window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(overlay) = overlay_for_update.upgrade() {
                    if let Some(rendered) = rendered {
                        let composited =
                            capture::composite_selection(&frame_for_update, selection, &rendered);
                        overlay.set_frame_image(to_slint_image(&composited));
                    }
                    overlay.set_status_text(progress.clone().into());
                }
                if let Some(window) = settings_for_update.upgrade() {
                    window.set_status_text(progress.into());
                }
            });
        },
    ))?;
    tracing::info!(
        elapsed_ms = translate_started.elapsed().as_millis(),
        lines = line_count,
        "translation finished"
    );
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
    Ok(PipelineOutput {
        composited: capture::composite_selection(&frame, selection, &rendered),
        source,
        translation,
    })
}

/// OCR 裁片外扩：上下左右各扩「选区半高」（钳制 16-48px），给检测器上下文。
/// 返回（外扩后的选区，原选区在外扩裁片坐标系中的区域）。钳制规则与
/// `crop_selection` 一致：不越出帧边界。
fn expand_for_ocr(
    selection: Selection,
    frame_width: u32,
    frame_height: u32,
) -> (Selection, crate::ocr::Rect) {
    let pad = (selection.height / 2.0).clamp(16.0, 48.0);
    let frame_w = frame_width as f32;
    let frame_h = frame_height as f32;
    let x = (selection.x - pad).max(0.0);
    let y = (selection.y - pad).max(0.0);
    let right = (selection.x + selection.width + pad).min(frame_w);
    let bottom = (selection.y + selection.height + pad).min(frame_h);
    let expanded = Selection {
        x,
        y,
        width: (right - x).max(1.0),
        height: (bottom - y).max(1.0),
    };
    let content = crate::ocr::Rect {
        x: (selection.x - x).round().max(0.0) as u32,
        y: (selection.y - y).round().max(0.0) as u32,
        width: selection.width.round().max(1.0) as u32,
        height: selection.height.round().max(1.0) as u32,
    };
    (expanded, content)
}

/// 按「供应商顺序 × 模型顺序」构建失败回退链；构建失败的节点跳过并记录，
/// 全部不可用时才报错。
fn create_translator(config: &AppConfig) -> Result<Translator> {
    let mut chain = Vec::new();
    let mut skipped = Vec::new();
    for provider in &config.providers {
        for model in &provider.models {
            let label = format!("{}·{}", provider.display_name(), model);
            let built = match provider.engine {
                TranslationEngine::OpenAiCompatible => OpenAiTranslator::from_provider(
                    provider,
                    model,
                    &config.target_lang,
                    config.proxy.as_deref(),
                )
                .map(Translator::from),
                TranslationEngine::Ollama => OllamaTranslator::new(
                    &provider.api_base,
                    model,
                    &config.target_lang,
                    config.proxy.as_deref(),
                )
                .map(Translator::from),
            };
            match built {
                Ok(translator) => chain.push((label, translator)),
                Err(error) => {
                    tracing::warn!(label, error = %format!("{error:#}"), "skipping translator");
                    skipped.push(format!("{label}: {error:#}"));
                }
            }
        }
    }
    anyhow::ensure!(
        !chain.is_empty(),
        "没有可用的翻译供应商{}",
        if skipped.is_empty() {
            String::new()
        } else {
            format!("（{}）", skipped.join("；"))
        }
    );
    Ok(Translator::Chain(chain))
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
            created_at: format_timestamp(entry.created_at).into(),
            source_text: entry.source_text.clone().into(),
            translated_text: entry.translated_text.clone().into(),
        })
        .collect();
    window.set_entries(Rc::new(slint::VecModel::from(items)).into());
    window.set_status_text(format!("共 {} 条", entries.len()).into());
}

/// 将 Unix 秒转换为本地时间 `YYYY-MM-DD HH:MM:SS`，失败时退回原始时间戳。
fn format_timestamp(epoch_secs: i64) -> String {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
        use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
        const UNIX_EPOCH_AS_FILETIME: i64 = 116_444_736_000_000_000;
        let ticks = UNIX_EPOCH_AS_FILETIME + epoch_secs.saturating_mul(10_000_000);
        if ticks >= 0 {
            let filetime = FILETIME {
                dwLowDateTime: (ticks as u64 & 0xffff_ffff) as u32,
                dwHighDateTime: ((ticks as u64) >> 32) as u32,
            };
            let mut utc = SYSTEMTIME::default();
            let mut local = SYSTEMTIME::default();
            unsafe {
                if FileTimeToSystemTime(&filetime, &mut utc).is_ok()
                    && SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_ok()
                {
                    return format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                        local.wYear,
                        local.wMonth,
                        local.wDay,
                        local.wHour,
                        local.wMinute,
                        local.wSecond
                    );
                }
            }
        }
    }
    format!("Unix {epoch_secs}")
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

/// 把最近一次冻结帧写到日志目录，供“遮罩黑屏”类问题排查。
fn dump_frame_for_diagnostics(frame: &Arc<DynamicImage>) {
    let path = crate::logging::log_dir().join("last-frame.png");
    match frame.save(&path) {
        Ok(()) => tracing::info!(path = %path.display(), "frozen frame dumped"),
        Err(error) => tracing::warn!("冻结帧转储失败：{error:#}"),
    }
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

#[cfg(test)]
pub(crate) fn to_slint_image_for_test(image: &DynamicImage) -> slint::Image {
    to_slint_image(image)
}

fn run_async<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("创建异步运行时失败")?
        .block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(name: &str, base: &str, models: &[&str]) -> ProviderDraft {
        ProviderDraft {
            name: name.to_owned(),
            engine: TranslationEngine::OpenAiCompatible,
            api_base: base.to_owned(),
            models: models.iter().map(|m| m.to_string()).collect(),
            new_api_key: String::new(),
            protected_api_key: String::new(),
        }
    }

    #[test]
    fn preset_detection_covers_opencode_endpoints() {
        let go = draft("", "https://opencode.ai/zen/go/v1", &["deepseek-v4-flash"]);
        assert_eq!(preset_of(&go), "OpenCode Go");
        let zen = draft("", "https://opencode.ai/zen/v1", &["big-pickle"]);
        assert_eq!(preset_of(&zen), "OpenCode Zen 免费");
        let mut ollama = draft("", "http://127.0.0.1:11434", &["qwen3"]);
        ollama.engine = TranslationEngine::Ollama;
        assert_eq!(preset_of(&ollama), "Ollama");
        let custom = draft("", "https://example.com/v1", &["m"]);
        assert_eq!(preset_of(&custom), "自定义");
    }

    #[test]
    fn drafts_to_providers_rejects_missing_base_or_models() {
        let no_base = draft("坏节点", "", &["m"]);
        let error = drafts_to_providers(&[no_base]).expect_err("missing base");
        assert!(format!("{error:#}").contains("坏节点"));
        let no_models = draft("坏节点", "https://x/v1", &[]);
        let error = drafts_to_providers(&[no_models]).expect_err("missing models");
        assert!(format!("{error:#}").contains("坏节点"));
    }

    #[test]
    fn expand_for_ocr_pads_and_reports_content_rect() {
        let selection = Selection {
            x: 100.0,
            y: 100.0,
            width: 200.0,
            height: 40.0,
        };
        let (expanded, content) = expand_for_ocr(selection, 1920, 1080);
        // pad = 40/2 = 20。
        assert_eq!(
            [expanded.x, expanded.y, expanded.width, expanded.height],
            [80.0, 80.0, 240.0, 80.0]
        );
        assert_eq!(
            (content.x, content.y, content.width, content.height),
            (20, 20, 200, 40)
        );
    }

    #[test]
    fn expand_for_ocr_clamps_to_frame_and_pad_range() {
        // 左上角钳制 + 最小 pad 16（半高 7 被钳到 16）。
        let selection = Selection {
            x: 10.0,
            y: 10.0,
            width: 100.0,
            height: 14.0,
        };
        let (expanded, content) = expand_for_ocr(selection, 1920, 1080);
        assert_eq!(
            [expanded.x, expanded.y, expanded.width, expanded.height],
            [0.0, 0.0, 126.0, 40.0]
        );
        assert_eq!((content.x, content.y), (10, 10));
        // 最大 pad 48（半高 100 被钳到 48）。
        let tall = Selection {
            x: 100.0,
            y: 100.0,
            width: 300.0,
            height: 200.0,
        };
        let (expanded, _) = expand_for_ocr(tall, 1920, 1080);
        assert_eq!([expanded.x, expanded.y], [52.0, 52.0]);
        // 右下角不越出帧。
        let corner = Selection {
            x: 1900.0,
            y: 1060.0,
            width: 30.0,
            height: 30.0,
        };
        let (expanded, _) = expand_for_ocr(corner, 1920, 1080);
        assert!(expanded.x + expanded.width <= 1920.0);
        assert!(expanded.y + expanded.height <= 1080.0);
    }

    #[test]
    fn create_translator_skips_keyless_and_keeps_order() {
        let mut config = AppConfig::default();
        config.providers = vec![
            // 无 key 的云端供应商：跳过。
            ProviderConfig {
                name: "无key".into(),
                engine: TranslationEngine::OpenAiCompatible,
                api_base: "https://x/v1".into(),
                models: vec!["m1".into()],
                protected_api_key: String::new(),
            },
            // Ollama 不需要 key：两个模型按序入链。
            ProviderConfig {
                name: "本地".into(),
                engine: TranslationEngine::Ollama,
                api_base: "http://127.0.0.1:11434".into(),
                models: vec!["qwen3".into(), "llama3.2".into()],
                protected_api_key: String::new(),
            },
        ];
        let Translator::Chain(chain) = create_translator(&config).expect("chain builds") else {
            panic!("expected chain");
        };
        let labels: Vec<&str> = chain.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["本地·qwen3", "本地·llama3.2"]);
    }

    /// 回归：保存处理器曾在同一语句里先 borrow 再于闭包内 borrow_mut，
    /// 任何一次成功保存都会 panic（release 下表现为 0xc0000409 崩溃）。
    /// 此测试走真实 handler：添加供应商草稿 → invoke_save_settings → 校验落盘。
    #[cfg(windows)]
    #[test]
    fn save_settings_with_added_provider_does_not_panic() {
        /// 测试会写真实配置文件，结束后必须还原。
        struct ConfigGuard(std::path::PathBuf, Option<Vec<u8>>);
        impl Drop for ConfigGuard {
            fn drop(&mut self) {
                match &self.1 {
                    Some(bytes) => {
                        let _ = std::fs::write(&self.0, bytes);
                    }
                    None => {
                        let _ = std::fs::remove_file(&self.0);
                    }
                }
            }
        }
        let config_path = crate::config::config_path().expect("config path");
        let _guard = ConfigGuard(config_path.clone(), std::fs::read(&config_path).ok());

        let window = SettingsWindow::new().expect("settings window");
        let mut initial = AppConfig::default();
        initial.providers[0]
            .set_api_key("sk-existing")
            .expect("DPAPI encrypt");
        let config = Arc::new(Mutex::new(initial));
        let drafts: ProviderDrafts = Rc::new(RefCell::new(Vec::new()));
        let editing = Rc::new(Cell::new(0usize));
        apply_config(&window, &config.lock().expect("config lock"), &drafts, &editing);
        install_save_handler(
            &window,
            Arc::clone(&config),
            Rc::clone(&drafts),
            Rc::clone(&editing),
        );
        // 自启动复选框与注册表现状对齐：set_enabled 写入同值，不改变用户设置。
        if let Ok(autostart) = Autostart::for_current_exe("ScreenTranslator")
            && let Ok(enabled) = autostart.is_enabled()
        {
            window.set_launch_at_login(enabled);
        }

        // 模拟用户：在原配置基础上添加第二个供应商，选中它（字段载入窗口）后保存。
        drafts.borrow_mut().push(ProviderDraft {
            name: "OpenCode Zen 免费".to_owned(),
            engine: TranslationEngine::OpenAiCompatible,
            api_base: "https://opencode.ai/zen/v1".to_owned(),
            models: vec!["big-pickle".to_owned()],
            new_api_key: "sk-new-provider".to_owned(),
            protected_api_key: String::new(),
        });
        editing.set(1);
        load_provider_fields(&window, &drafts, &editing);
        window.invoke_save_settings();

        let status = window.get_status_text();
        assert!(
            status.contains("已加密保存"),
            "保存未成功，状态栏：{status}"
        );
        let saved = config.lock().expect("config lock").clone();
        assert_eq!(saved.providers.len(), 2);
        assert_eq!(saved.providers[1].name, "OpenCode Zen 免费");
        assert_eq!(
            saved.providers[1].api_key().expect("decrypt").as_deref(),
            Some("sk-new-provider")
        );
        // 原供应商的 key 不得丢失。
        assert_eq!(
            saved.providers[0].api_key().expect("decrypt").as_deref(),
            Some("sk-existing")
        );
        let on_disk = AppConfig::load_from(&config_path).expect("reload saved config");
        assert_eq!(on_disk.providers.len(), 2);
    }

    #[test]
    fn create_translator_fails_when_nothing_usable() {        let mut config = AppConfig::default();
        config.providers = vec![ProviderConfig {
            name: "无key".into(),
            engine: TranslationEngine::OpenAiCompatible,
            api_base: "https://x/v1".into(),
            models: vec!["m1".into()],
            protected_api_key: String::new(),
        }];
        let error = match create_translator(&config) {
            Err(error) => error,
            Ok(_) => panic!("no usable provider"),
        };
        assert!(format!("{error:#}").contains("没有可用的翻译供应商"));
    }

    /// 真实数据诊断：应用转储的冻结帧 + 用户实测选区 → 外扩裁剪 → OCR →
    /// 区域过滤 → 行内合并。复现「紧贴文字小选区识别不准/单行被切碎」并
    /// 验证外扩+过滤后的结果。块数随转储帧变化，只打印不断言具体值。
    #[test]
    #[ignore = "依赖应用转储帧，诊断用"]
    fn line_merge_on_real_frame() {
        let frame = image::open(crate::logging::log_dir().join("last-frame.png"))
            .expect("last-frame.png should exist");
        // 用户 2026-07-24 10:24 实测选区：终端状态行。
        let selection = crate::capture::Selection {
            x: 10.0,
            y: 686.0,
            width: 967.0,
            height: 72.0,
        };
        let (ocr_selection, content) =
            expand_for_ocr(selection, frame.width(), frame.height());
        let crop = crate::capture::crop_selection(&frame, ocr_selection).expect("crop");
        let raw = crate::ocr::OcrEngine::default()
            .recognize(&crop)
            .expect("ocr");
        eprintln!("raw blocks: {}", raw.len());
        for block in &raw {
            let rect = block.bounding_box();
            eprintln!(
                "  [{},{} {}x{}] {}",
                rect.x, rect.y, rect.width, rect.height, block.text
            );
        }
        let blocks = crate::ocr::filter_to_region(&raw, &content);
        eprintln!("after region filter: {}", blocks.len());
        let lines = crate::ocr::merge_line_fragments(&blocks);
        eprintln!("merged lines: {}", lines.len());
        for line in &lines {
            eprintln!("  line: {}", line.text);
        }
        assert!(lines.len() <= blocks.len());
        assert!(blocks.len() <= raw.len());
        assert!(!lines.is_empty());
    }

    /// 提示词规则验证：普通技术词汇（含单独单词）必须翻译；专有名词/代码保留。
    #[test]
    #[ignore = "真实 API 调用，需本机配置与网络"]
    fn prompt_translates_ordinary_words_keeps_proper_nouns() {
        let config = AppConfig::load().expect("load real config");
        let translator = create_translator(&config).expect("translator");
        let lines = vec![
            "embedded".to_owned(),
            "command".to_owned(),
            "API".to_owned(),
            "Windows Terminal".to_owned(),
        ];
        let translations = run_async(translator.translate(&lines)).expect("translate");
        for (source, translation) in lines.iter().zip(&translations) {
            eprintln!("{source} => {translation}");
        }
        assert!(
            translations[0].contains("嵌入"),
            "embedded 应翻译：{}",
            translations[0]
        );
        assert!(
            translations[1].contains("命令"),
            "command 应翻译：{}",
            translations[1]
        );
        assert!(
            translations[2].contains("API"),
            "API 应保留：{}",
            translations[2]
        );
        assert!(
            translations[3].contains("Windows Terminal"),
            "Windows Terminal 应保留：{}",
            translations[3]
        );
    }

    /// Zen 网关直连验证：接受 `thinking: {"type": "disabled"}` 参数（无回退链兜底）。
    #[test]
    #[ignore = "真实 API 调用，需本机配置与网络"]
    fn zen_accepts_thinking_disabled() {
        let config = AppConfig::load().expect("load real config");
        let provider = config
            .providers
            .iter()
            .find(|provider| provider.api_base.contains("opencode.ai"))
            .expect("config should contain an OpenCode provider");
        let translator = OpenAiTranslator::from_provider(
            provider,
            &provider.models[0],
            &config.target_lang,
            config.proxy.as_deref(),
        )
        .expect("translator");
        let lines = vec!["hello".to_owned()];
        let started = std::time::Instant::now();
        let translations = run_async(translator.translate(&lines)).expect("zen direct call");
        eprintln!(
            "zen direct ({}ms): {} => {}",
            started.elapsed().as_millis(),
            lines[0],
            translations[0]
        );
        assert!(!translations[0].trim().is_empty());
    }
}
