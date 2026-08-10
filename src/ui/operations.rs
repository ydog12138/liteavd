//! 跨设备操作工具栏与确认/结果投影。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use glib::SendWeakRef;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::core::instance::DeviceRuntime;
use crate::core::operation::{
    ApkInstallRequest, OperationCancellation, OperationExecutionError, OperationKind,
    OperationPlan, OperationProgress, OperationProgressSink, OperationProgressStage,
    OperationReport, OperationResult, OperationSuccess, PushFilesRequest, execute_install_apks,
    execute_push_files, execute_screenshots, execute_stop,
};
use crate::core::workspace::OperationScope;

pub const SCOPE_WIDGET: &str = "liteavd-operation-scope";
pub const SCREENSHOT_WIDGET: &str = "liteavd-operation-screenshot";
pub const INSTALL_WIDGET: &str = "liteavd-operation-install";
pub const PUSH_WIDGET: &str = "liteavd-operation-push";
pub const SNAPSHOT_WIDGET: &str = "liteavd-operation-snapshot";
pub const LOG_WIDGET: &str = "liteavd-operation-log";
pub const STOP_WIDGET: &str = "liteavd-operation-stop";

pub fn build_controls(
    parent: &adw::ApplicationWindow,
    runtime: Arc<DeviceRuntime>,
    on_changed: Arc<dyn Fn() + Send + Sync>,
) -> gtk4::Box {
    let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let scope = gtk4::DropDown::from_strings(&["焦点设备", "已选设备", "全部运行"]);
    scope.set_widget_name(SCOPE_WIDGET);
    scope.set_selected(0);
    scope.set_tooltip_text(Some("操作目标范围"));
    controls.append(&scope);

    let screenshot = gtk4::Button::from_icon_name("camera-photo-symbolic");
    screenshot.set_widget_name(SCREENSHOT_WIDGET);
    screenshot.set_tooltip_text(Some("截图到目录"));
    let parent_for_screenshot = parent.downgrade();
    let runtime_for_screenshot = runtime.clone();
    let scope_for_screenshot = scope.clone();
    let changed_for_screenshot = on_changed.clone();
    screenshot.connect_clicked(move |_| {
        let Some(parent) = parent_for_screenshot.upgrade() else {
            return;
        };
        choose_screenshot_directory(
            parent,
            runtime_for_screenshot.clone(),
            selected_scope(&scope_for_screenshot),
            changed_for_screenshot.clone(),
        );
    });
    controls.append(&screenshot);

    let install = gtk4::Button::from_icon_name("document-open-symbolic");
    install.set_widget_name(INSTALL_WIDGET);
    install.set_tooltip_text(Some("向目标设备安装 APK"));
    let parent_for_install = parent.downgrade();
    let runtime_for_install = runtime.clone();
    let scope_for_install = scope.clone();
    let changed_for_install = on_changed.clone();
    install.connect_clicked(move |_| {
        let Some(parent) = parent_for_install.upgrade() else {
            return;
        };
        choose_apk(
            parent,
            runtime_for_install.clone(),
            selected_scope(&scope_for_install),
            changed_for_install.clone(),
        );
    });
    let drop_target = gtk4::DropTarget::new(
        gtk4::gdk::FileList::static_type(),
        gtk4::gdk::DragAction::COPY,
    );
    let parent_for_drop = parent.downgrade();
    let runtime_for_drop = runtime.clone();
    let scope_for_drop = scope.clone();
    let changed_for_drop = on_changed.clone();
    drop_target.connect_drop(move |_target, value, _x, _y| {
        let Some(parent) = parent_for_drop.upgrade() else {
            return false;
        };
        let Ok(files) = value.get::<gtk4::gdk::FileList>() else {
            return false;
        };
        let Ok(apks) = local_paths(files.files()) else {
            show_error(&parent, "APK 必须是本地文件系统中的普通文件");
            return false;
        };
        if apks.is_empty() || !apks.iter().all(|path| has_apk_extension(path)) {
            show_error(
                &parent,
                "一次拖放必须全部是 .apk 文件；不支持 .aab、.apks 或 XAPK",
            );
            return false;
        }
        confirm_and_install_apks(
            parent,
            runtime_for_drop.clone(),
            selected_scope(&scope_for_drop),
            changed_for_drop.clone(),
            apks,
        );
        true
    });
    install.add_controller(drop_target);
    controls.append(&install);

    let push = gtk4::Button::from_icon_name("folder-download-symbolic");
    push.set_widget_name(PUSH_WIDGET);
    push.set_tooltip_text(Some("向目标设备的 Download/liteavd 推送文件"));
    let parent_for_push = parent.downgrade();
    let runtime_for_push = runtime.clone();
    let scope_for_push = scope.clone();
    let changed_for_push = on_changed.clone();
    push.connect_clicked(move |_| {
        let Some(parent) = parent_for_push.upgrade() else {
            return;
        };
        choose_push_files(
            parent,
            runtime_for_push.clone(),
            selected_scope(&scope_for_push),
            changed_for_push.clone(),
        );
    });
    let push_drop_target = gtk4::DropTarget::new(
        gtk4::gdk::FileList::static_type(),
        gtk4::gdk::DragAction::COPY,
    );
    let parent_for_push_drop = parent.downgrade();
    let runtime_for_push_drop = runtime.clone();
    let scope_for_push_drop = scope.clone();
    let changed_for_push_drop = on_changed.clone();
    push_drop_target.connect_drop(move |_target, value, _x, _y| {
        let Some(parent) = parent_for_push_drop.upgrade() else {
            return false;
        };
        let Ok(files) = value.get::<gtk4::gdk::FileList>() else {
            return false;
        };
        let Ok(paths) = local_paths(files.files()) else {
            show_error(&parent, "只能推送本地文件系统中的普通文件");
            return false;
        };
        if paths.is_empty() {
            show_error(&parent, "至少拖入一个文件");
            return false;
        }
        confirm_and_push_files(
            parent,
            runtime_for_push_drop.clone(),
            selected_scope(&scope_for_push_drop),
            changed_for_push_drop.clone(),
            paths,
        );
        true
    });
    push.add_controller(push_drop_target);
    controls.append(&push);

    let snapshots = gtk4::Button::from_icon_name("document-save-symbolic");
    snapshots.set_widget_name(SNAPSHOT_WIDGET);
    snapshots.set_tooltip_text(Some("管理 focused session 的 snapshots"));
    let parent_for_snapshots = parent.downgrade();
    let runtime_for_snapshots = runtime.clone();
    snapshots.connect_clicked(move |_| {
        if let Some(parent) = parent_for_snapshots.upgrade() {
            crate::ui::snapshots::open(&parent, runtime_for_snapshots.clone());
        }
    });
    controls.append(&snapshots);

    let logs = gtk4::Button::from_icon_name("text-x-generic-symbolic");
    logs.set_widget_name(LOG_WIDGET);
    logs.set_tooltip_text(Some("查看 focused managed session 日志"));
    let parent_for_logs = parent.downgrade();
    let runtime_for_logs = runtime.clone();
    logs.connect_clicked(move |_| {
        if let Some(parent) = parent_for_logs.upgrade() {
            crate::ui::session_log::open(&parent, runtime_for_logs.clone());
        }
    });
    controls.append(&logs);

    let stop = gtk4::Button::from_icon_name("media-playback-stop-symbolic");
    stop.set_widget_name(STOP_WIDGET);
    stop.add_css_class("destructive-action");
    stop.set_tooltip_text(Some("停止目标设备"));
    let parent_for_stop = parent.downgrade();
    let runtime_for_stop = runtime;
    let scope_for_stop = scope;
    stop.connect_clicked(move |_| {
        let Some(parent) = parent_for_stop.upgrade() else {
            return;
        };
        confirm_and_stop(
            parent,
            runtime_for_stop.clone(),
            selected_scope(&scope_for_stop),
            on_changed.clone(),
        );
    });
    controls.append(&stop);
    controls
}

fn selected_scope(dropdown: &gtk4::DropDown) -> OperationScope {
    scope_for_index(dropdown.selected())
}

fn scope_for_index(index: u32) -> OperationScope {
    match index {
        1 => OperationScope::Selected,
        2 => OperationScope::AllRunning,
        _ => OperationScope::Focused,
    }
}

fn scope_label(scope: OperationScope) -> &'static str {
    match scope {
        OperationScope::Focused => "焦点设备",
        OperationScope::Selected => "已选设备",
        OperationScope::AllRunning => "全部运行设备",
    }
}

fn choose_screenshot_directory(
    parent: adw::ApplicationWindow,
    runtime: Arc<DeviceRuntime>,
    scope: OperationScope,
    on_changed: Arc<dyn Fn() + Send + Sync>,
) {
    let plan = match runtime.plan_operation(OperationKind::Screenshot, scope) {
        Ok(plan) => plan,
        Err(error) => {
            show_error(&parent, &error.to_string());
            return;
        }
    };
    choose_screenshot_directory_for_plan(parent, runtime, plan, on_changed);
}

/// 卡片截图先把该 AVD 设为焦点并立即固化 exact route，再打开目录选择器。
/// 用户选择目录期间即使焦点变化，授权仍只会命中原 session 或安全失败。
pub(crate) fn choose_device_screenshot(
    parent: adw::ApplicationWindow,
    runtime: Arc<DeviceRuntime>,
    avd_name: &str,
) {
    if let Err(error) = runtime.focus_session(avd_name) {
        show_error(&parent, &error.to_string());
        return;
    }
    let plan = match runtime.plan_operation(OperationKind::Screenshot, OperationScope::Focused) {
        Ok(plan) => plan,
        Err(error) => {
            show_error(&parent, &error.to_string());
            return;
        }
    };
    choose_screenshot_directory_for_plan(parent, runtime, plan, Arc::new(|| {}));
}

fn choose_screenshot_directory_for_plan(
    parent: adw::ApplicationWindow,
    runtime: Arc<DeviceRuntime>,
    plan: OperationPlan,
    on_changed: Arc<dyn Fn() + Send + Sync>,
) {
    glib::spawn_future_local(async move {
        let dialog = gtk4::FileDialog::builder()
            .title("选择截图输出目录")
            .build();
        let Ok(folder) = dialog.select_folder_future(Some(&parent)).await else {
            return;
        };
        let Some(output_dir) = folder.path() else {
            show_error(&parent, "截图目录必须是本地文件系统路径");
            return;
        };
        let authorized = match runtime.authorize_operation(plan) {
            Ok(authorized) => authorized,
            Err(error) => {
                show_error(&parent, &error.to_string());
                return;
            }
        };
        dispatch_report(
            &parent,
            on_changed,
            execute_screenshots(runtime, authorized, output_dir),
        );
    });
}

fn choose_apk(
    parent: adw::ApplicationWindow,
    runtime: Arc<DeviceRuntime>,
    scope: OperationScope,
    on_changed: Arc<dyn Fn() + Send + Sync>,
) {
    glib::spawn_future_local(async move {
        let dialog = gtk4::FileDialog::builder()
            .title("选择单 APK 或一组 split APK")
            .build();
        let Ok(files) = dialog.open_multiple_future(Some(&parent)).await else {
            return;
        };
        let selected = (0..files.n_items())
            .filter_map(|index| files.item(index))
            .filter_map(|item| item.downcast::<gtk4::gio::File>().ok())
            .collect::<Vec<_>>();
        let Ok(apks) = local_paths(selected) else {
            show_error(&parent, "APK 必须是本地文件系统中的普通文件");
            return;
        };
        if apks.is_empty() || !apks.iter().all(|path| has_apk_extension(path)) {
            show_error(
                &parent,
                "选择必须全部是 .apk 文件；不支持 .aab、.apks 或 XAPK",
            );
            return;
        }
        confirm_and_install_apks(parent, runtime, scope, on_changed, apks);
    });
}

fn confirm_and_install_apks(
    parent: adw::ApplicationWindow,
    runtime: Arc<DeviceRuntime>,
    scope: OperationScope,
    on_changed: Arc<dyn Fn() + Send + Sync>,
    apks: Vec<PathBuf>,
) {
    glib::spawn_future_local(async move {
        let plan = match runtime.plan_operation(OperationKind::InstallApk, scope) {
            Ok(plan) => plan,
            Err(error) => {
                show_error(&parent, &error.to_string());
                return;
            }
        };
        let Some(options) = confirm_apk_plan(&parent, &plan, &apks).await else {
            return;
        };
        let authorized = match runtime.authorize_operation(plan) {
            Ok(authorized) => authorized,
            Err(error) => {
                show_error(&parent, &error.to_string());
                return;
            }
        };
        let targets = authorized.plan().targets().to_vec();
        dispatch_controlled_report(
            &parent,
            on_changed,
            targets,
            move |cancellation, progress| {
                execute_install_apks(
                    runtime,
                    authorized,
                    crate::ui::main_window::sdk_root(),
                    ApkInstallRequest { apks, options },
                    cancellation,
                    progress,
                )
            },
        );
    });
}

fn choose_push_files(
    parent: adw::ApplicationWindow,
    runtime: Arc<DeviceRuntime>,
    scope: OperationScope,
    on_changed: Arc<dyn Fn() + Send + Sync>,
) {
    glib::spawn_future_local(async move {
        let dialog = gtk4::FileDialog::builder()
            .title("选择要推送到 Download/liteavd 的文件")
            .build();
        let Ok(files) = dialog.open_multiple_future(Some(&parent)).await else {
            return;
        };
        let selected = (0..files.n_items())
            .filter_map(|index| files.item(index))
            .filter_map(|item| item.downcast::<gtk4::gio::File>().ok())
            .collect::<Vec<_>>();
        let Ok(paths) = local_paths(selected) else {
            show_error(&parent, "只能推送本地文件系统中的普通文件");
            return;
        };
        if paths.is_empty() {
            show_error(&parent, "至少选择一个文件");
            return;
        }
        confirm_and_push_files(parent, runtime, scope, on_changed, paths);
    });
}

fn confirm_and_push_files(
    parent: adw::ApplicationWindow,
    runtime: Arc<DeviceRuntime>,
    scope: OperationScope,
    on_changed: Arc<dyn Fn() + Send + Sync>,
    files: Vec<PathBuf>,
) {
    glib::spawn_future_local(async move {
        let plan = match runtime.plan_operation(OperationKind::PushFiles, scope) {
            Ok(plan) => plan,
            Err(error) => {
                show_error(&parent, &error.to_string());
                return;
            }
        };
        let detail = format!(
            "目标目录：/sdcard/Download/liteavd/\n默认不覆盖已有文件；先写 .part，再原子发布。\n\n{}",
            describe_files(&files)
        );
        if !confirm_plan(&parent, &plan, "推送文件", &detail).await {
            return;
        }
        let authorized = match runtime.authorize_operation(plan) {
            Ok(authorized) => authorized,
            Err(error) => {
                show_error(&parent, &error.to_string());
                return;
            }
        };
        let targets = authorized.plan().targets().to_vec();
        dispatch_controlled_report(
            &parent,
            on_changed,
            targets,
            move |cancellation, progress| {
                execute_push_files(
                    runtime,
                    authorized,
                    crate::ui::main_window::sdk_root(),
                    PushFilesRequest { files },
                    cancellation,
                    progress,
                )
            },
        );
    });
}

async fn confirm_apk_plan(
    parent: &adw::ApplicationWindow,
    plan: &OperationPlan,
    apks: &[PathBuf],
) -> Option<crate::core::adb::ApkInstallOptions> {
    let downgrade = gtk4::CheckButton::with_label("允许降级（-d）");
    let grant = gtk4::CheckButton::with_label("安装时授予运行时权限（-g）");
    let options = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    options.append(&downgrade);
    options.append(&grant);
    let targets = describe_targets(plan);
    let dialog = adw::AlertDialog::builder()
        .heading("确认安装 APK")
        .body(format!(
            "目标范围：{}\n{targets}\n\n命令固定包含 -r -t。\n{}",
            scope_label(plan.scope()),
            describe_files(apks)
        ))
        .extra_child(&options)
        .build();
    dialog.add_response("cancel", "取消");
    dialog.add_response("confirm", "安装 APK");
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    if dialog.choose_future(parent).await != "confirm" {
        return None;
    }
    Some(crate::core::adb::ApkInstallOptions {
        allow_downgrade: downgrade.is_active(),
        grant_runtime_permissions: grant.is_active(),
    })
}

fn confirm_and_stop(
    parent: adw::ApplicationWindow,
    runtime: Arc<DeviceRuntime>,
    scope: OperationScope,
    on_changed: Arc<dyn Fn() + Send + Sync>,
) {
    let plan = match runtime.plan_operation(OperationKind::Stop, scope) {
        Ok(plan) => plan,
        Err(error) => {
            show_error(&parent, &error.to_string());
            return;
        }
    };
    glib::spawn_future_local(async move {
        if !confirm_plan(&parent, &plan, "停止设备", "运行中的任务可能丢失未保存状态").await
        {
            return;
        }
        let authorized = match runtime.authorize_operation(plan) {
            Ok(authorized) => authorized,
            Err(error) => {
                show_error(&parent, &error.to_string());
                return;
            }
        };
        dispatch_report(
            &parent,
            on_changed,
            execute_stop(runtime, authorized, crate::ui::main_window::sdk_root()),
        );
    });
}

async fn confirm_plan(
    parent: &adw::ApplicationWindow,
    plan: &OperationPlan,
    action: &str,
    detail: &str,
) -> bool {
    let targets = describe_targets(plan);
    let dialog = adw::AlertDialog::builder()
        .heading(format!("确认{action}"))
        .body(format!(
            "目标范围：{}\n{targets}\n\n{detail}",
            scope_label(plan.scope())
        ))
        .build();
    dialog.add_response("cancel", "取消");
    dialog.add_response("confirm", action);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    dialog.choose_future(parent).await == "confirm"
}

fn describe_targets(plan: &OperationPlan) -> String {
    plan.targets()
        .iter()
        .map(|target| format!("• {}（session {}）", target.avd_name, target.session_id))
        .collect::<Vec<_>>()
        .join("\n")
}

fn describe_files(files: &[PathBuf]) -> String {
    const DISPLAY_LIMIT: usize = 20;
    let mut lines = files
        .iter()
        .take(DISPLAY_LIMIT)
        .map(|path| format!("• {}", path.display()))
        .collect::<Vec<_>>();
    if files.len() > DISPLAY_LIMIT {
        lines.push(format!("… 另有 {} 个文件", files.len() - DISPLAY_LIMIT));
    }
    lines.join("\n")
}

fn local_paths(files: impl IntoIterator<Item = gtk4::gio::File>) -> Result<Vec<PathBuf>, ()> {
    files
        .into_iter()
        .map(|file| file.path().ok_or(()))
        .collect()
}

fn has_apk_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("apk"))
}

fn dispatch_report<F>(
    parent: &adw::ApplicationWindow,
    on_changed: Arc<dyn Fn() + Send + Sync>,
    future: F,
) where
    F: std::future::Future<Output = Result<OperationReport, OperationExecutionError>>
        + Send
        + 'static,
{
    let parent = SendWeakRef::from(parent.downgrade());
    crate::ui::device_list::spawn_async(async move {
        let result = future.await;
        on_changed();
        glib::MainContext::default().invoke(move || {
            let Some(parent) = parent.upgrade() else {
                return;
            };
            match result {
                Ok(report) => show_report(&parent, &report),
                Err(error) => show_error(&parent, &error.to_string()),
            }
        });
    });
}

fn dispatch_controlled_report<B, F>(
    parent: &adw::ApplicationWindow,
    on_changed: Arc<dyn Fn() + Send + Sync>,
    targets: Vec<crate::core::workspace::WorkspaceRoute>,
    build: B,
) where
    B: FnOnce(OperationCancellation, Option<OperationProgressSink>) -> F,
    F: std::future::Future<Output = Result<OperationReport, OperationExecutionError>>
        + Send
        + 'static,
{
    let states = Arc::new(Mutex::new(
        targets
            .iter()
            .cloned()
            .map(|route| (route, "等待中".to_owned()))
            .collect::<BTreeMap<_, _>>(),
    ));
    let dialog = adw::AlertDialog::builder()
        .heading("操作正在进行")
        .body(progress_body(&states))
        .build();
    let spinner = gtk4::Spinner::new();
    spinner.start();
    dialog.set_extra_child(Some(&spinner));
    dialog.add_response("cancel", "取消操作");
    dialog.set_close_response("cancel");
    let cancellation = OperationCancellation::default();
    let cancellation_for_response = cancellation.clone();
    dialog.connect_response(Some("cancel"), move |_, _| {
        cancellation_for_response.cancel();
    });

    let dialog_for_progress = SendWeakRef::from(dialog.downgrade());
    let states_for_progress = states.clone();
    let progress: OperationProgressSink = Arc::new(move |event| {
        let body = {
            let mut states = states_for_progress
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            states.insert(event.route.clone(), describe_progress(&event));
            progress_body_from_map(&states)
        };
        let dialog = dialog_for_progress.clone();
        glib::MainContext::default().invoke(move || {
            if let Some(dialog) = dialog.upgrade() {
                dialog.set_body(&body);
            }
        });
    });
    let future = build(cancellation, Some(progress));
    let parent_weak = SendWeakRef::from(parent.downgrade());
    let dialog_weak = SendWeakRef::from(dialog.downgrade());
    dialog.present(Some(parent));
    crate::ui::device_list::spawn_async(async move {
        let result = future.await;
        on_changed();
        glib::MainContext::default().invoke(move || {
            if let Some(dialog) = dialog_weak.upgrade() {
                dialog.force_close();
            }
            let Some(parent) = parent_weak.upgrade() else {
                return;
            };
            match result {
                Ok(report) => show_report(&parent, &report),
                Err(error) => show_error(&parent, &error.to_string()),
            }
        });
    });
}

fn describe_progress(progress: &OperationProgress) -> String {
    let stage = match progress.stage {
        OperationProgressStage::Starting => "准备中",
        OperationProgressStage::Transferring => "正在传输",
        OperationProgressStage::Publishing => "正在发布",
        OperationProgressStage::CleaningUp => "正在清理",
        OperationProgressStage::Finished => "已完成",
    };
    if progress.total_items == 0 {
        stage.into()
    } else {
        format!(
            "{stage} {}/{}",
            progress.completed_items, progress.total_items
        )
    }
}

fn progress_body(
    states: &Mutex<BTreeMap<crate::core::workspace::WorkspaceRoute, String>>,
) -> String {
    let states = states.lock().unwrap_or_else(|error| error.into_inner());
    progress_body_from_map(&states)
}

fn progress_body_from_map(
    states: &BTreeMap<crate::core::workspace::WorkspaceRoute, String>,
) -> String {
    states
        .iter()
        .map(|(route, state)| {
            format!(
                "• {}（session {}）：{state}",
                route.avd_name, route.session_id
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn show_report(parent: &adw::ApplicationWindow, report: &OperationReport) {
    let succeeded = report
        .devices
        .iter()
        .filter(|device| matches!(device.result, OperationResult::Succeeded(_)))
        .count();
    let lines = report
        .devices
        .iter()
        .map(|device| {
            let result = match &device.result {
                OperationResult::Succeeded(OperationSuccess::Screenshot { path, bytes }) => {
                    format!("成功：{}（{bytes} B）", path.display())
                }
                OperationResult::Succeeded(OperationSuccess::ApksInstalled {
                    files,
                    exit_code,
                }) => format!("安装成功：{files} 个 APK（exit {exit_code:?}）"),
                OperationResult::Succeeded(OperationSuccess::FilesPushed {
                    paths,
                    bytes,
                    exit_code,
                }) => format!(
                    "推送成功：{} 个文件、{bytes} B（exit {exit_code:?}）\n{}",
                    paths.len(),
                    paths.join("\n")
                ),
                OperationResult::Succeeded(OperationSuccess::Stopped) => "已停止".into(),
                OperationResult::Failed(error) => format!("失败：{error}"),
                OperationResult::Canceled => "已取消".into(),
                OperationResult::StaleRoute => "跳过：session 已变化".into(),
            };
            format!("• {}：{result}", device.route.avd_name)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let dialog = adw::AlertDialog::builder()
        .heading(format!(
            "操作 #{}：{succeeded}/{} 成功",
            report.id.get(),
            report.devices.len()
        ))
        .body(lines)
        .build();
    dialog.add_response("close", "关闭");
    dialog.set_close_response("close");
    dialog.present(Some(parent));
}

pub(crate) fn show_error(parent: &adw::ApplicationWindow, message: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading("操作无法执行")
        .body(message)
        .build();
    dialog.add_response("close", "关闭");
    dialog.set_close_response("close");
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_mapping_is_explicit() {
        assert_eq!(scope_for_index(0), OperationScope::Focused);
        assert_eq!(scope_for_index(1), OperationScope::Selected);
        assert_eq!(scope_for_index(2), OperationScope::AllRunning);
        assert_eq!(scope_for_index(99), OperationScope::Focused);
        assert_eq!(scope_label(OperationScope::Selected), "已选设备");
        assert!(has_apk_extension(Path::new("base.APK")));
        assert!(!has_apk_extension(Path::new("bundle.apks")));
    }
}
