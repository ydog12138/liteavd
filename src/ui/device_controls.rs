//! 单设备卡片的快捷控制。
//!
//! 每次操作先固化 exact route；同一卡片的按键通过一个 FIFO mutex 串行发送，
//! 避免快速点击音量/导航键时由并发 unary RPC 改变顺序。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use glib::SendWeakRef;
use gtk4::prelude::*;
use libadwaita as adw;

use crate::core::input::DeviceKey;
use crate::core::instance::DeviceRuntime;
use crate::core::operation::send_route_keypress;
use crate::core::settings::{AppLogLevel, emit};
use crate::ui::microphone::{MicrophoneController, MicrophoneStatus, SourceKind};

pub const CONTROLS_WIDGET: &str = "liteavd-device-controls";
pub const BACK_WIDGET: &str = "liteavd-device-back";
pub const HOME_WIDGET: &str = "liteavd-device-home";
pub const OVERVIEW_WIDGET: &str = "liteavd-device-overview";
pub const POWER_WIDGET: &str = "liteavd-device-power";
pub const VOLUME_DOWN_WIDGET: &str = "liteavd-device-volume-down";
pub const VOLUME_MUTE_WIDGET: &str = "liteavd-device-volume-mute";
pub const VOLUME_UP_WIDGET: &str = "liteavd-device-volume-up";
pub const MICROPHONE_WIDGET: &str = "liteavd-device-microphone";
pub const MICROPHONE_FILE_WIDGET: &str = "liteavd-device-microphone-file";
pub const MICROPHONE_STOP_WIDGET: &str = "liteavd-device-microphone-stop";
pub const SCREENSHOT_WIDGET: &str = "liteavd-device-screenshot";

pub(crate) fn build(
    avd_name: &str,
    enabled: bool,
    runtime: Arc<DeviceRuntime>,
    microphone_controller: Arc<MicrophoneController>,
) -> gtk4::FlowBox {
    let controls = gtk4::FlowBox::new();
    controls.set_widget_name(CONTROLS_WIDGET);
    controls.set_selection_mode(gtk4::SelectionMode::None);
    controls.set_activate_on_single_click(false);
    controls.set_homogeneous(true);
    controls.set_min_children_per_line(4);
    controls.set_max_children_per_line(5);
    controls.set_column_spacing(4);
    controls.set_row_spacing(4);
    controls.set_halign(gtk4::Align::Center);
    controls.set_sensitive(enabled);

    let queue = Arc::new(tokio::sync::Mutex::new(()));
    for button in [
        key_button(
            "go-previous-symbolic",
            BACK_WIDGET,
            "返回（Alt+← / Esc）",
            DeviceKey::Back,
            avd_name,
            runtime.clone(),
            queue.clone(),
        ),
        key_button(
            "go-home-symbolic",
            HOME_WIDGET,
            "主屏幕（Alt+Home / Home）",
            DeviceKey::Home,
            avd_name,
            runtime.clone(),
            queue.clone(),
        ),
        key_button(
            "view-grid-symbolic",
            OVERVIEW_WIDGET,
            "最近任务（Alt+O / Menu）",
            DeviceKey::AppSwitch,
            avd_name,
            runtime.clone(),
            queue.clone(),
        ),
        key_button(
            "system-shutdown-symbolic",
            POWER_WIDGET,
            "电源键（Alt+P / Power）",
            DeviceKey::Power,
            avd_name,
            runtime.clone(),
            queue.clone(),
        ),
        key_button(
            "audio-volume-low-symbolic",
            VOLUME_DOWN_WIDGET,
            "降低设备音量（Ctrl+↓ / 音量减）",
            DeviceKey::VolumeDown,
            avd_name,
            runtime.clone(),
            queue.clone(),
        ),
        key_button(
            "audio-volume-muted-symbolic",
            VOLUME_MUTE_WIDGET,
            "切换设备静音（Ctrl+M / 静音键）",
            DeviceKey::VolumeMute,
            avd_name,
            runtime.clone(),
            queue.clone(),
        ),
        key_button(
            "audio-volume-high-symbolic",
            VOLUME_UP_WIDGET,
            "提高设备音量（Ctrl+↑ / 音量加）",
            DeviceKey::VolumeUp,
            avd_name,
            runtime.clone(),
            queue,
        ),
    ] {
        controls.insert(&button, -1);
    }

    let microphone = gtk4::ToggleButton::new();
    microphone.set_widget_name(MICROPHONE_WIDGET);
    microphone.set_icon_name("microphone-hardware-disabled-symbolic");
    microphone.set_tooltip_text(Some("将宿主麦克风接入此设备（默认关闭）"));
    let syncing = Rc::new(Cell::new(false));
    let microphone_avd = avd_name.to_owned();
    let controller_for_microphone = microphone_controller.clone();
    let syncing_for_toggle = syncing.clone();
    microphone.connect_toggled(move |button| {
        if syncing_for_toggle.get() {
            return;
        }
        if button.is_active() {
            if let Err(error) = controller_for_microphone.start_host(&microphone_avd) {
                syncing_for_toggle.set(true);
                button.set_active(false);
                syncing_for_toggle.set(false);
                show_control_error(button, &error);
            }
        } else {
            controller_for_microphone.stop_for(&microphone_avd);
        }
    });
    controls.insert(&microphone, -1);

    let microphone_file = gtk4::Button::from_icon_name("media-playback-start-symbolic");
    microphone_file.set_widget_name(MICROPHONE_FILE_WIDGET);
    microphone_file.set_tooltip_text(Some("向此设备的虚拟麦克风播放 PCM WAV"));
    let file_avd = avd_name.to_owned();
    let controller_for_file = microphone_controller.clone();
    microphone_file.connect_clicked(move |button| {
        if controller_for_file.toggle_wav_pause(&file_avd).is_some() {
            return;
        }
        let Some(parent) = application_window(button) else {
            show_control_error(button, "WAV 文件选择器不在应用窗口中");
            return;
        };
        let avd_name = file_avd.clone();
        let controller = controller_for_file.clone();
        glib::spawn_future_local(async move {
            let filter = gtk4::FileFilter::new();
            filter.set_name(Some("PCM WAV 音频"));
            filter.add_suffix("wav");
            let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
            filters.append(&filter);
            let dialog = gtk4::FileDialog::builder()
                .title("选择要注入虚拟麦克风的 WAV")
                .filters(&filters)
                .default_filter(&filter)
                .build();
            let Ok(file) = dialog.open_future(Some(&parent)).await else {
                return;
            };
            let Some(path) = file.path() else {
                crate::ui::operations::show_error(&parent, "WAV 必须是本地文件系统路径");
                return;
            };
            if let Err(error) = controller.start_wav(&avd_name, path) {
                crate::ui::operations::show_error(&parent, &error);
            }
        });
    });
    let wav_drop = gtk4::DropTarget::new(
        gtk4::gdk::FileList::static_type(),
        gtk4::gdk::DragAction::COPY,
    );
    let drop_avd = avd_name.to_owned();
    let controller_for_drop = microphone_controller.clone();
    wav_drop.connect_drop(move |target, value, _, _| {
        let Ok(files) = value.get::<gtk4::gdk::FileList>() else {
            return false;
        };
        let selected = files.files();
        if selected.len() != 1 {
            return false;
        }
        let file = &selected[0];
        let Some(path) = file.path() else {
            return false;
        };
        if let Err(error) = controller_for_drop.start_wav(&drop_avd, path) {
            if let Some(widget) = target.widget() {
                show_control_error(&widget, &error);
            }
            return false;
        }
        true
    });
    microphone_file.add_controller(wav_drop);
    controls.insert(&microphone_file, -1);

    let microphone_stop = gtk4::Button::from_icon_name("media-playback-stop-symbolic");
    microphone_stop.set_widget_name(MICROPHONE_STOP_WIDGET);
    microphone_stop.set_tooltip_text(Some("停止当前虚拟麦克风来源"));
    microphone_stop.set_sensitive(false);
    let stop_avd = avd_name.to_owned();
    let controller_for_stop = microphone_controller.clone();
    microphone_stop.connect_clicked(move |_| controller_for_stop.stop_for(&stop_avd));
    controls.insert(&microphone_stop, -1);

    let microphone_weak = microphone.downgrade();
    let file_weak = microphone_file.downgrade();
    let stop_weak = microphone_stop.downgrade();
    let status_avd = avd_name.to_owned();
    glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
        let (Some(microphone), Some(file), Some(stop)) = (
            microphone_weak.upgrade(),
            file_weak.upgrade(),
            stop_weak.upgrade(),
        ) else {
            return glib::ControlFlow::Break;
        };
        let available = microphone_controller.available(&status_avd);
        let status = microphone_controller.status_for(&status_avd);
        let source_active = source_is_active(&status);
        let host_active = host_toggle_is_active(&status);
        syncing.set(true);
        microphone.set_active(host_active);
        syncing.set(false);
        microphone.set_sensitive(available);
        file.set_sensitive(available);
        stop.set_sensitive(source_active);
        microphone.set_icon_name(match &status {
            MicrophoneStatus::Active {
                source: SourceKind::Host,
                ..
            } => "microphone-sensitivity-high-symbolic",
            MicrophoneStatus::Error { .. } => "dialog-warning-symbolic",
            _ if available => "microphone-sensitivity-muted-symbolic",
            _ => "microphone-hardware-disabled-symbolic",
        });
        microphone.set_tooltip_text(Some(&microphone_tooltip(&status, available)));
        if let MicrophoneStatus::Active {
            source: SourceKind::Wav { .. },
            ..
        } = &status
        {
            file.set_icon_name("media-playback-pause-symbolic");
            file.set_tooltip_text(Some("暂停 WAV；停止按钮可取消当前来源"));
        } else if let MicrophoneStatus::Paused {
            source: SourceKind::Wav { .. },
            ..
        } = &status
        {
            file.set_icon_name("media-playback-start-symbolic");
            file.set_tooltip_text(Some("继续 WAV；停止按钮可取消当前来源"));
        } else {
            file.set_icon_name("media-playback-start-symbolic");
            file.set_tooltip_text(Some("向此设备的虚拟麦克风播放 PCM WAV（也可拖放）"));
        }
        glib::ControlFlow::Continue
    });

    let screenshot = gtk4::Button::from_icon_name("camera-photo-symbolic");
    screenshot.set_widget_name(SCREENSHOT_WIDGET);
    screenshot.set_tooltip_text(Some("截取此设备屏幕（Ctrl+Shift+S）"));
    let screenshot_avd = avd_name.to_owned();
    screenshot.connect_clicked(move |button| {
        let Some(parent) = application_window(button) else {
            emit(
                AppLogLevel::Warn,
                format_args!("截图控件不在应用窗口中：{screenshot_avd}"),
            );
            return;
        };
        crate::ui::operations::choose_device_screenshot(parent, runtime.clone(), &screenshot_avd);
    });
    controls.insert(&screenshot, -1);

    controls
}

fn microphone_tooltip(status: &MicrophoneStatus, available: bool) -> String {
    match status {
        MicrophoneStatus::Inactive if available => "将宿主麦克风接入此设备（默认关闭）".into(),
        MicrophoneStatus::Inactive => {
            "此 session 没有虚拟麦克风端点；确认 PipeWire/Pulse 与 pactl 后重启设备".into()
        }
        MicrophoneStatus::Active { source, .. } => match source {
            SourceKind::Host => "宿主麦克风正接入此设备；点击立即关闭".into(),
            SourceKind::Wav { name } => {
                format!("正在注入 {name}；点击可直接切换为宿主麦克风")
            }
        },
        MicrophoneStatus::Paused { source, .. } => match source {
            SourceKind::Host => "宿主麦克风已暂停".into(),
            SourceKind::Wav { name } => {
                format!("{name} 已暂停；点击可直接切换为宿主麦克风")
            }
        },
        MicrophoneStatus::Finished { source, .. } => match source {
            SourceKind::Host => "宿主麦克风已停止".into(),
            SourceKind::Wav { name } => format!("{name} 已播放完毕"),
        },
        MicrophoneStatus::Error { message, .. } => format!("虚拟麦克风失败：{message}"),
    }
}

fn source_is_active(status: &MicrophoneStatus) -> bool {
    matches!(
        status,
        MicrophoneStatus::Active { .. } | MicrophoneStatus::Paused { .. }
    )
}

fn host_toggle_is_active(status: &MicrophoneStatus) -> bool {
    matches!(
        status,
        MicrophoneStatus::Active {
            source: SourceKind::Host,
            ..
        } | MicrophoneStatus::Paused {
            source: SourceKind::Host,
            ..
        }
    )
}

fn key_button(
    icon: &str,
    widget_name: &str,
    tooltip: &str,
    key: DeviceKey,
    avd_name: &str,
    runtime: Arc<DeviceRuntime>,
    queue: Arc<tokio::sync::Mutex<()>>,
) -> gtk4::Button {
    let button = gtk4::Button::from_icon_name(icon);
    button.set_widget_name(widget_name);
    button.set_tooltip_text(Some(tooltip));
    let avd_name = avd_name.to_owned();
    button.connect_clicked(move |button| {
        let Some(route) = runtime.input_route(&avd_name).map(|guard| {
            guard.focus();
            guard.route().clone()
        }) else {
            show_control_error(button, &format!("{avd_name} 没有可控的运行 session"));
            return;
        };
        let runtime = runtime.clone();
        let queue = queue.clone();
        let parent = application_window(button).map(|window| SendWeakRef::from(window.downgrade()));
        crate::ui::device_list::spawn_async(async move {
            let _turn = queue.lock().await;
            if let Err(error) = send_route_keypress(runtime, route, key).await {
                let message = error_message(error);
                glib::MainContext::default().invoke(move || {
                    if let Some(parent) = parent.and_then(|weak| weak.upgrade()) {
                        crate::ui::operations::show_error(&parent, &message);
                    } else {
                        emit(
                            AppLogLevel::Warn,
                            format_args!("设备快捷控制失败：{message}"),
                        );
                    }
                });
            }
        });
    });
    button
}

fn application_window(widget: &impl IsA<gtk4::Widget>) -> Option<adw::ApplicationWindow> {
    widget
        .as_ref()
        .root()
        .and_then(|root| root.downcast::<adw::ApplicationWindow>().ok())
}

fn show_control_error(widget: &impl IsA<gtk4::Widget>, message: &str) {
    if let Some(parent) = application_window(widget) {
        crate::ui::operations::show_error(&parent, message);
    } else {
        emit(
            AppLogLevel::Warn,
            format_args!("设备快捷控制失败：{message}"),
        );
    }
}

fn error_message(error: crate::core::operation::OperationRunError) -> String {
    match error {
        crate::core::operation::OperationRunError::Failed(error) => error,
        crate::core::operation::OperationRunError::Canceled => "操作已取消".into(),
        crate::core::operation::OperationRunError::StaleRoute => {
            "session 已变化，快捷操作已取消".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_source_never_presents_as_an_enabled_host_microphone_toggle() {
        let wav = MicrophoneStatus::Active {
            avd_name: "phone".into(),
            source: SourceKind::Wav {
                name: "tone.wav".into(),
            },
        };
        assert!(source_is_active(&wav));
        assert!(!host_toggle_is_active(&wav));
        assert!(microphone_tooltip(&wav, true).contains("切换为宿主麦克风"));

        let host = MicrophoneStatus::Active {
            avd_name: "phone".into(),
            source: SourceKind::Host,
        };
        assert!(source_is_active(&host));
        assert!(host_toggle_is_active(&host));
    }
}
