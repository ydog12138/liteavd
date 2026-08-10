use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Once};

use glib::SendWeakRef;
use gtk4::{Label, ScrolledWindow};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::core::advertisement::AdvertisementMonitor;
use crate::core::settings::{AppLogLevel, emit};
use crate::core::{emulator, instance::DeviceRuntime, recovery};
use crate::ui::microphone::MicrophoneController;
use crate::ui::{audio, operations, workspace};

static INVALID_FLATPAK_SDK_OVERRIDE_WARNING: Once = Once::new();

/// SDK 根目录：可持久使用的 AVDM_SDK_ROOT 环境变量 > 设置文件 > 当前运行环境默认值。
pub fn sdk_root() -> std::path::PathBuf {
    if let Some(root) = std::env::var_os("AVDM_SDK_ROOT") {
        let root = std::path::PathBuf::from(root);
        if crate::core::paths::sdk_override_allowed(&root) {
            return root;
        }
        INVALID_FLATPAK_SDK_OVERRIDE_WARNING.call_once(|| {
            emit(
                AppLogLevel::Warn,
                format_args!(
                    "忽略 Flatpak 内不可持久访问的 AVDM_SDK_ROOT：{}；请为已有 SDK 授予精确 filesystem 权限，或使用应用私有托管目录",
                    root.display()
                ),
            );
        });
    }
    if let Some(root) = crate::core::settings::load().settings.sdk_root {
        return std::path::PathBuf::from(root);
    }
    crate::core::paths::default_sdk_root()
}

/// 重建 AVD 清单（必须主线程）。设备运行状态在行内投影，不调用此函数。
fn rebuild_into(
    holder: &gtk4::Box,
    runtime: Arc<DeviceRuntime>,
    microphone: Arc<MicrophoneController>,
) {
    let sdk = sdk_root();
    while let Some(child) = holder.first_child() {
        holder.remove(&child);
    }
    if !sdk.join("emulator/emulator").exists() {
        let warn = Label::new(Some(&format!(
            "未找到 SDK（emulator/emulator）。请在镜像管理中安装托管组件，或在设置中选择已有 SDK（当前：{}）",
            sdk.display()
        )));
        warn.add_css_class("error");
        warn.set_wrap(true);
        warn.set_margin_top(24);
        holder.append(&warn);
        return;
    }
    let workspace = workspace::build(sdk, runtime, microphone);
    holder.append(&workspace);
}

/// 广告事件只刷新现有设备行；SDK 或 AVD 结构变化时才重建容器。
fn refresh_into(
    holder: &gtk4::Box,
    runtime: Arc<DeviceRuntime>,
    microphone: Arc<MicrophoneController>,
) {
    let sdk = sdk_root();
    if sdk.join("emulator/emulator").exists()
        && let Some(workspace) = holder
            .first_child()
            .and_then(|child| child.downcast::<gtk4::FlowBox>().ok())
    {
        workspace::refresh(&workspace, sdk, runtime, microphone);
    } else {
        rebuild_into(holder, runtime, microphone);
    }
}

pub fn build(app: &adw::Application) {
    let loaded_settings = crate::core::settings::load();
    crate::core::settings::configure_log_level(loaded_settings.settings.log_level);
    if let Some(warning) = loaded_settings.status.warning() {
        emit(AppLogLevel::Warn, format_args!("{warning}"));
    }
    let runtime = Arc::new(
        DeviceRuntime::with_runtime_policy(
            loaded_settings.settings.scheduler_config(),
            loaded_settings.settings.managed_gpu_policy,
        )
        .expect("validated settings must produce a valid runtime policy"),
    );
    let microphone_controller = MicrophoneController::new(runtime.clone());
    let workspace_state_path = recovery::workspace_state_path();
    let persisted_intent = match recovery::load_workspace(&workspace_state_path) {
        Ok(intent) => intent,
        Err(error) => {
            emit(
                AppLogLevel::Warn,
                format_args!("忽略不可读的工作区恢复状态：{error:#}"),
            );
            Default::default()
        }
    };
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("liteavd")
        .default_width(1180)
        .default_height(780)
        .build();

    let header = adw::HeaderBar::new();
    let new_btn = gtk4::Button::from_icon_name("list-add-symbolic");
    new_btn.set_tooltip_text(Some("新建 AVD"));
    header.pack_start(&new_btn);
    let images_btn = gtk4::Button::from_icon_name("folder-download-symbolic");
    images_btn.set_tooltip_text(Some("镜像管理"));
    header.pack_end(&images_btn);
    let settings_btn = gtk4::Button::from_icon_name("open-menu-symbolic");
    settings_btn.set_tooltip_text(Some("设置"));
    header.pack_end(&settings_btn);
    let refresh = gtk4::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("刷新"));
    header.pack_end(&refresh);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);

    let list_holder = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    rebuild_into(&list_holder, runtime.clone(), microphone_controller.clone());
    runtime.restore_workspace_intent(&persisted_intent);
    refresh_into(&list_holder, runtime.clone(), microphone_controller.clone());

    let last_persisted = Rc::new(RefCell::new(Some(persisted_intent)));
    let window_for_persistence = window.downgrade();
    let runtime_for_persistence = runtime.clone();
    let path_for_persistence = workspace_state_path.clone();
    let last_for_persistence = last_persisted.clone();
    let holder_for_health = list_holder.downgrade();
    let last_projection_revision = Cell::new(runtime.projection_revision());
    glib::timeout_add_seconds_local(1, move || {
        if window_for_persistence.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        let intent = runtime_for_persistence.workspace_intent();
        if last_for_persistence.borrow().as_ref() != Some(&intent) {
            match recovery::save_workspace(&path_for_persistence, &intent) {
                Ok(()) => *last_for_persistence.borrow_mut() = Some(intent),
                Err(error) => emit(
                    AppLogLevel::Warn,
                    format_args!("保存工作区恢复状态失败：{error:#}"),
                ),
            }
        }
        let revision = runtime_for_persistence.projection_revision();
        if revision != last_projection_revision.get() {
            if let Some(workspace) = holder_for_health
                .upgrade()
                .and_then(|holder| holder.first_child())
                .and_then(|child| child.downcast::<gtk4::FlowBox>().ok())
            {
                workspace::refresh_runtime_projection(&workspace, runtime_for_persistence.clone());
            }
            last_projection_revision.set(revision);
        }
        glib::ControlFlow::Continue
    });

    let holder_for_operations = SendWeakRef::from(list_holder.downgrade());
    let runtime_for_operations = runtime.clone();
    let microphone_for_operations = microphone_controller.clone();
    let operation_refresh: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let holder = holder_for_operations.clone();
        let runtime = runtime_for_operations.clone();
        let microphone = microphone_for_operations.clone();
        glib::MainContext::default().invoke(move || {
            if let Some(holder) = holder.upgrade() {
                refresh_into(&holder, runtime, microphone);
            }
        });
    });
    let operation_controls =
        operations::build_controls(&window, runtime.clone(), operation_refresh);
    let audio_controller = audio::AudioController::new(runtime.clone());
    let audio_controls = audio::build_controls(audio_controller.clone());
    let header_controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header_controls.append(&operation_controls);
    let controls_separator = gtk4::Separator::new(gtk4::Orientation::Vertical);
    header_controls.append(&controls_separator);
    header_controls.append(&audio_controls);
    header.set_title_widget(Some(&header_controls));

    // Workspace focus is UI intent and does not necessarily change the runtime projection
    // revision. Poll it independently so the old route is silenced well inside the 250ms
    // focused-audio handoff target.
    let window_for_audio = window.downgrade();
    let audio_for_focus = audio_controller.clone();
    let last_audio_error = Rc::new(RefCell::new(None::<(String, String)>));
    let last_error_for_audio = last_audio_error.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
        if window_for_audio.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        audio_for_focus.sync_focus();
        match audio_for_focus.status() {
            audio::AudioStatus::Error { avd_name, message } => {
                let error = (avd_name, message);
                if last_error_for_audio.borrow().as_ref() != Some(&error) {
                    audio::log_status(&audio::AudioStatus::Error {
                        avd_name: error.0.clone(),
                        message: error.1.clone(),
                    });
                    *last_error_for_audio.borrow_mut() = Some(error);
                }
            }
            audio::AudioStatus::Connecting { .. } => {}
            _ => {
                last_error_for_audio.borrow_mut().take();
            }
        }
        glib::ControlFlow::Continue
    });

    let holder2 = list_holder.clone();
    let runtime_for_refresh = runtime.clone();
    let microphone_for_refresh = microphone_controller.clone();
    refresh.connect_clicked(move |_| {
        rebuild_into(
            &holder2,
            runtime_for_refresh.clone(),
            microphone_for_refresh.clone(),
        );
    });

    let holder3 = list_holder.clone();
    let runtime_for_wizard = runtime.clone();
    let microphone_for_wizard = microphone_controller.clone();
    let win_for_wizard = window.downgrade();
    new_btn.connect_clicked(move |_| {
        let Some(win) = win_for_wizard.upgrade() else {
            return;
        };
        // 审计 #13：回调内重读 sdk_root()，避免沿用 build 时的旧值
        let sdk = sdk_root();
        let on_created = {
            let holder = holder3.clone();
            let runtime = runtime_for_wizard.clone();
            let microphone = microphone_for_wizard.clone();
            move || {
                rebuild_into(&holder, runtime.clone(), microphone.clone());
            }
        };
        crate::ui::create_wizard::open(&win, sdk, on_created);
    });

    let holder4 = list_holder.clone();
    let runtime_for_settings = runtime.clone();
    let microphone_for_settings = microphone_controller.clone();
    let win_for_settings = window.downgrade();
    settings_btn.connect_clicked(move |_| {
        let Some(win) = win_for_settings.upgrade() else {
            return;
        };
        let holder = holder4.clone();
        let runtime = runtime_for_settings.clone();
        let microphone = microphone_for_settings.clone();
        crate::ui::settings_page::open(&win, move |saved| {
            if !runtime.try_update_managed_gpu_policy(saved.managed_gpu_policy) {
                emit(
                    AppLogLevel::Warn,
                    format_args!(
                        "当前存在运行或排队设备，托管 GPU 策略已保存但将在下次启动 liteavd 时生效"
                    ),
                );
            }
            rebuild_into(&holder, runtime.clone(), microphone.clone());
        });
    });

    let holder5 = SendWeakRef::from(list_holder.downgrade());
    let runtime_for_images = runtime.clone();
    let microphone_for_images = microphone_controller.clone();
    let win_for_images = window.downgrade();
    images_btn.connect_clicked(move |_| {
        let Some(win) = win_for_images.upgrade() else {
            return;
        };
        let ctx = glib::MainContext::default();
        let runtime = runtime_for_images.clone();
        let microphone = microphone_for_images.clone();
        let holder = holder5.clone();
        let cb: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
            let runtime = runtime.clone();
            let microphone = microphone.clone();
            let holder = holder.clone();
            ctx.invoke(move || {
                if let Some(holder) = holder.upgrade() {
                    rebuild_into(&holder, runtime, microphone);
                }
            });
        });
        crate::ui::images_page::open(&win, cb);
    });

    let scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .child(&list_holder)
        .build();
    toolbar.set_content(Some(&scroll));
    window.set_content(Some(&toolbar));

    let holder_for_monitor = SendWeakRef::from(list_holder.downgrade());
    let runtime_for_monitor = runtime.clone();
    let microphone_for_monitor = microphone_controller.clone();
    let monitor = AdvertisementMonitor::start(move || {
        let sdk = sdk_root();
        runtime_for_monitor.reconcile_running_for_sdk_with_demands(
            emulator::list_running_for_sdk(&sdk),
            std::collections::HashMap::new(),
            &sdk,
        );
        let holder = holder_for_monitor.clone();
        let runtime = runtime_for_monitor.clone();
        let microphone = microphone_for_monitor.clone();
        glib::MainContext::default().invoke(move || {
            if let Some(holder) = holder.upgrade() {
                refresh_into(&holder, runtime, microphone);
            }
        });
    });
    let monitor = match monitor {
        Ok(monitor) => Some(monitor),
        Err(error) => {
            emit(
                AppLogLevel::Warn,
                format_args!("广告目录事件监听不可用，将保留手工刷新：{error:#}"),
            );
            None
        }
    };
    let monitor = Rc::new(RefCell::new(monitor));
    let monitor_on_close = monitor.clone();
    let audio_on_close = audio_controller;
    let microphone_on_close = microphone_controller;
    let runtime_on_close = runtime;
    let path_on_close = workspace_state_path;
    let last_on_close = last_persisted;
    window.connect_close_request(move |_| {
        let intent = runtime_on_close.workspace_intent();
        if last_on_close.borrow().as_ref() != Some(&intent) {
            match recovery::save_workspace(&path_on_close, &intent) {
                Ok(()) => *last_on_close.borrow_mut() = Some(intent),
                Err(error) => emit(
                    AppLogLevel::Warn,
                    format_args!("关闭窗口前保存工作区状态失败：{error:#}"),
                ),
            }
        }
        audio_on_close.set_enabled(false);
        microphone_on_close.stop_all();
        monitor_on_close.borrow_mut().take();
        glib::Propagation::Proceed
    });

    window.present();
}
