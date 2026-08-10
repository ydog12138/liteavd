//! 设置页：版本化设置、资源策略与环境变量优先级。

use std::path::PathBuf;

use gtk4::prelude::*;
use gtk4::{Button, CheckButton, DropDown, Entry, Label, SpinButton, StringList, Window};

use crate::core::settings::{
    self, AppLogLevel, LoadedSettings, MAX_CONCURRENT_STARTS, MAX_DOWNLOAD_CACHE_LIMIT_MB,
    MIN_DOWNLOAD_CACHE_LIMIT_MB, ManagedGpuPolicy, Settings,
};

const SDK_ENTRY_WIDGET: &str = "liteavd-settings-sdk-entry";
const WARNING_WIDGET: &str = "liteavd-settings-warning";
const SAVE_WIDGET: &str = "liteavd-settings-save";
const GPU_POLICY_WIDGET: &str = "liteavd-settings-gpu-policy";
const GPU_POLICY_NOTE_WIDGET: &str = "liteavd-settings-gpu-policy-note";

fn section_label(text: &str) -> Label {
    let label = Label::new(Some(text));
    label.add_css_class("heading");
    label.set_xalign(0.0);
    label
}

fn labeled_row(label: &str, widget: &impl IsA<gtk4::Widget>) -> gtk4::Box {
    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    let title = Label::new(Some(label));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    row.append(&title);
    row.append(widget);
    row
}

/// 打开设置对话框。
pub fn open(parent: &impl IsA<Window>, on_saved: impl Fn(Settings) + 'static) {
    let win = Window::builder()
        .title("设置")
        .modal(true)
        .transient_for(parent)
        .default_width(620)
        .default_height(620)
        .build();
    let loaded = settings::load();
    let sdk_override = std::env::var_os("AVDM_SDK_ROOT").map(PathBuf::from);
    let content = build_content(&win, loaded, sdk_override, on_saved);
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .child(&content)
        .build();
    win.set_child(Some(&scroll));
    win.present();
}

fn build_content(
    win: &Window,
    loaded: LoadedSettings,
    sdk_override: Option<PathBuf>,
    on_saved: impl Fn(Settings) + 'static,
) -> gtk4::Box {
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);

    let warning = Label::new(loaded.status.warning().as_deref());
    warning.set_widget_name(WARNING_WIDGET);
    warning.add_css_class("warning");
    warning.set_wrap(true);
    warning.set_xalign(0.0);
    warning.set_visible(loaded.status.warning().is_some());
    content.append(&warning);

    content.append(&section_label("Android SDK"));
    let sdk_help = Label::new(Some(
        "SDK 根目录应包含 emulator/、platform-tools/ 和 system-images/。",
    ));
    sdk_help.set_xalign(0.0);
    sdk_help.set_wrap(true);
    content.append(&sdk_help);

    let entry = Entry::new();
    entry.set_widget_name(SDK_ENTRY_WIDGET);
    let displayed_sdk = sdk_override
        .as_ref()
        .map(|path| path.display().to_string())
        .or_else(|| loaded.settings.sdk_root.clone())
        .unwrap_or_else(|| crate::core::paths::default_sdk_root().display().to_string());
    entry.set_text(&displayed_sdk);
    entry.set_placeholder_text(Some("/path/to/sdk"));

    let browse = Button::with_label("浏览…");
    let sdk_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    sdk_row.append(&entry);
    sdk_row.append(&browse);
    content.append(&sdk_row);

    if let Some(path) = sdk_override.as_ref() {
        entry.set_sensitive(false);
        browse.set_sensitive(false);
        let override_label = Label::new(Some(&format!(
            "当前由 AVDM_SDK_ROOT={} 只读覆盖；设置文件中的 SDK 路径不会在本次进程生效。",
            path.display()
        )));
        override_label.add_css_class("dim-label");
        override_label.set_xalign(0.0);
        override_label.set_wrap(true);
        content.append(&override_label);
    } else if crate::core::paths::is_flatpak() {
        let sandbox_label = Label::new(Some(
            "Flatpak 默认把托管 SDK 与 AVD 保存在应用私有数据目录；接管宿主 SDK 需要通过文件选择器或 Flatpak override 显式授权。",
        ));
        sandbox_label.add_css_class("dim-label");
        sandbox_label.set_xalign(0.0);
        sandbox_label.set_wrap(true);
        content.append(&sandbox_label);
    }

    let entry_for_picker = entry.clone();
    browse.connect_clicked(move |button| {
        let picker = gtk4::FileDialog::builder().title("选择 SDK 目录").build();
        let parent_win = button
            .root()
            .and_then(|root| root.downcast::<Window>().ok());
        let entry = entry_for_picker.clone();
        picker.select_folder(
            parent_win.as_ref(),
            None::<&gtk4::gio::Cancellable>,
            move |result| {
                if let Ok(directory) = result
                    && let Some(path) = directory.path()
                {
                    entry.set_text(&path.display().to_string());
                }
            },
        );
    });

    content.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    content.append(&section_label("启动与资源"));

    let starts = SpinButton::with_range(1.0, MAX_CONCURRENT_STARTS as f64, 1.0);
    starts.set_value(loaded.settings.max_concurrent_starts as f64);
    content.append(&labeled_row("同时处于 launch/boot 的设备数", &starts));

    let memory_enabled = CheckButton::with_label("限制总内存预算");
    memory_enabled.set_active(loaded.settings.memory_budget_mb.is_some());
    let memory = SpinButton::with_range(1024.0, 262_144.0, 1024.0);
    memory.set_value(loaded.settings.memory_budget_mb.unwrap_or(8192) as f64);
    memory.set_sensitive(memory_enabled.is_active());
    let memory_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    memory_row.append(&memory_enabled);
    memory_row.append(&memory);
    memory_row.append(&Label::new(Some("MiB")));
    content.append(&memory_row);
    let memory_for_toggle = memory.clone();
    memory_enabled.connect_toggled(move |toggle| {
        memory_for_toggle.set_sensitive(toggle.is_active());
    });

    let gpu_slots_enabled = CheckButton::with_label("限制托管桌面 host 与接管实例的 GPU slot");
    gpu_slots_enabled.set_active(loaded.settings.host_gpu_slots.is_some());
    let gpu_slots = SpinButton::with_range(1.0, 16.0, 1.0);
    gpu_slots.set_value(loaded.settings.host_gpu_slots.unwrap_or(1) as f64);
    gpu_slots.set_sensitive(gpu_slots_enabled.is_active());
    let gpu_slots_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    gpu_slots_row.append(&gpu_slots_enabled);
    gpu_slots_row.append(&gpu_slots);
    content.append(&gpu_slots_row);
    let gpu_slots_for_toggle = gpu_slots.clone();
    gpu_slots_enabled.connect_toggled(move |toggle| {
        gpu_slots_for_toggle.set_sensitive(toggle.is_active());
    });

    let gpu_labels: Vec<_> = ManagedGpuPolicy::MANAGED_CHOICES
        .iter()
        .map(|policy| policy.label())
        .collect();
    let gpu_model = StringList::new(&gpu_labels);
    let gpu_policy = DropDown::new(Some(gpu_model), None::<&gtk4::Expression>);
    gpu_policy.set_widget_name(GPU_POLICY_WIDGET);
    gpu_policy.set_selected(
        ManagedGpuPolicy::MANAGED_CHOICES
            .iter()
            .position(|policy| *policy == loaded.settings.managed_gpu_policy)
            .unwrap_or(0) as u32,
    );
    content.append(&labeled_row("托管实例 GPU 策略", &gpu_policy));
    let gpu_note = Label::new(Some(loaded.settings.managed_gpu_policy.availability()));
    gpu_note.set_widget_name(GPU_POLICY_NOTE_WIDGET);
    gpu_note.add_css_class("dim-label");
    gpu_note.set_wrap(true);
    gpu_note.set_xalign(0.0);
    content.append(&gpu_note);
    let gpu_note_for_selection = gpu_note.clone();
    gpu_policy.connect_selected_notify(move |dropdown| {
        if let Some(policy) = ManagedGpuPolicy::MANAGED_CHOICES.get(dropdown.selected() as usize) {
            gpu_note_for_selection.set_text(policy.availability());
        }
    });

    content.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    content.append(&section_label("存储与诊断"));

    let cache = SpinButton::with_range(
        MIN_DOWNLOAD_CACHE_LIMIT_MB as f64,
        MAX_DOWNLOAD_CACHE_LIMIT_MB as f64,
        512.0,
    );
    cache.set_value(loaded.settings.download_cache_limit_mb as f64);
    content.append(&labeled_row("下载缓存上限（MiB）", &cache));

    let log_labels: Vec<_> = AppLogLevel::ALL.iter().map(|level| level.label()).collect();
    let log_model = StringList::new(&log_labels);
    let log_level = DropDown::new(Some(log_model), None::<&gtk4::Expression>);
    log_level.set_selected(
        AppLogLevel::ALL
            .iter()
            .position(|level| *level == loaded.settings.log_level)
            .unwrap_or(2) as u32,
    );
    content.append(&labeled_row("应用日志级别", &log_level));

    let restart_note = Label::new(Some(
        "托管 GPU 策略在没有运行或排队设备时保存后立即生效；否则与启动并发、内存/GPU 预算一样，在下次启动 liteavd 时生效。",
    ));
    restart_note.add_css_class("dim-label");
    restart_note.set_xalign(0.0);
    restart_note.set_wrap(true);
    content.append(&restart_note);

    let error = Label::new(None);
    error.add_css_class("error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    content.append(&error);

    let save = Button::with_label("保存");
    save.set_widget_name(SAVE_WIDGET);
    save.add_css_class("suggested-action");
    let original_sdk = loaded.settings.sdk_root.clone();
    let has_sdk_override = sdk_override.is_some();
    let window = win.downgrade();
    save.connect_clicked(move |_| {
        let sdk_root = if has_sdk_override {
            original_sdk.clone()
        } else {
            let path = entry.text().trim().to_string();
            if path.is_empty() {
                error.set_text("请输入 SDK 路径");
                return;
            }
            let sdk = std::path::Path::new(&path);
            if !sdk.join("emulator/emulator").is_file() {
                error.set_text(&format!("{path} 下未找到普通文件 emulator/emulator"));
                return;
            }
            Some(path)
        };
        let selected_log = AppLogLevel::ALL
            .get(log_level.selected() as usize)
            .copied()
            .unwrap_or_default();
        let updated = Settings {
            sdk_root,
            max_concurrent_starts: starts.value_as_int() as usize,
            memory_budget_mb: memory_enabled
                .is_active()
                .then(|| memory.value_as_int() as u64),
            host_gpu_slots: gpu_slots_enabled
                .is_active()
                .then(|| gpu_slots.value_as_int() as u32),
            download_cache_limit_mb: cache.value_as_int() as u64,
            log_level: selected_log,
            managed_gpu_policy: ManagedGpuPolicy::MANAGED_CHOICES
                .get(gpu_policy.selected() as usize)
                .copied()
                .unwrap_or(ManagedGpuPolicy::HeadlessSwangle),
            ..Settings::default()
        };
        match settings::save(&updated) {
            Ok(()) => {
                settings::configure_log_level(updated.log_level);
                if let Some(window) = window.upgrade() {
                    window.close();
                }
                on_saved(updated);
            }
            Err(save_error) => error.set_text(&format!("保存失败：{save_error:#}")),
        }
    });
    content.append(&save);
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::{SETTINGS_SCHEMA_VERSION, SettingsLoadStatus};

    fn find_named<T: IsA<gtk4::Widget> + Clone + 'static>(
        root: &gtk4::Widget,
        name: &str,
    ) -> Option<T> {
        if root.widget_name() == name {
            return root.clone().downcast::<T>().ok();
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Some(found) = find_named::<T>(&widget, name) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }

    #[test]
    #[ignore = "需要 DISPLAY；用 Xvfb 运行设置页 GUI 门禁"]
    fn environment_override_is_read_only_and_invalid_load_is_visible() {
        gtk4::init().expect("GTK init");
        let window = Window::new();
        let loaded = LoadedSettings {
            settings: Settings {
                schema_version: SETTINGS_SCHEMA_VERSION,
                sdk_root: Some("/stored/sdk".into()),
                ..Settings::default()
            },
            status: SettingsLoadStatus::Invalid {
                message: "synthetic corrupt file".into(),
            },
        };
        let content = build_content(
            &window,
            loaded,
            Some(PathBuf::from("/environment/sdk")),
            |_| {},
        );
        let root: gtk4::Widget = content.upcast();
        let entry = find_named::<Entry>(&root, SDK_ENTRY_WIDGET).unwrap();
        let warning = find_named::<Label>(&root, WARNING_WIDGET).unwrap();
        assert!(!entry.is_sensitive());
        assert_eq!(entry.text(), "/environment/sdk");
        assert!(warning.is_visible());
        assert!(warning.text().contains("原文件未被覆盖"));
    }

    #[test]
    #[ignore = "需要 DISPLAY；用 Xvfb 运行设置页 GUI 门禁"]
    fn managed_gpu_policy_selection_exposes_both_modes_without_fallback() {
        gtk4::init().expect("GTK init");
        let window = Window::new();
        let loaded = LoadedSettings {
            settings: Settings {
                managed_gpu_policy: ManagedGpuPolicy::DesktopHost,
                ..Settings::default()
            },
            status: SettingsLoadStatus::Current,
        };
        let content = build_content(&window, loaded, None, |_| {});
        let root: gtk4::Widget = content.upcast();
        let policy = find_named::<DropDown>(&root, GPU_POLICY_WIDGET).unwrap();
        let note = find_named::<Label>(&root, GPU_POLICY_NOTE_WIDGET).unwrap();
        assert_eq!(policy.model().unwrap().n_items(), 2);
        assert_eq!(policy.selected(), 1);
        assert!(note.text().contains("DISPLAY"));
        policy.set_selected(0);
        assert!(note.text().contains("无需 DISPLAY"));
    }
}
