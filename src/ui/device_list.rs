//! 设备卡片列表：AVD 扫描投影 + registry 命令 + 行内状态更新。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use glib::SendWeakRef;
use gtk4::prelude::*;
use gtk4::{Button, CheckButton, Label, ListBox, ListBoxRow};

use crate::core::avd::{self, ManagedGpuPolicy};
use crate::core::device_state::RecoveryReason;
use crate::core::emulator::{self, LaunchParams, ManagedAudioPolicy, RunningInstance};
use crate::core::grpc_auth::GrpcLaunchConfig;
use crate::core::instance::{DeviceRuntime, StartCommand};
use crate::core::scheduler::{ResourceDemand, SchedulerError};
use crate::core::settings::{AppLogLevel, emit};

pub use crate::core::device_state::DevicePhase as DeviceStatus;

const STATUS_WIDGET: &str = "liteavd-device-status";
const START_WIDGET: &str = "liteavd-device-start";
const STOP_WIDGET: &str = "liteavd-device-stop";
const SELECT_WIDGET: &str = "liteavd-device-select";
const VIEWPORT_HOLDER_WIDGET: &str = "liteavd-device-viewport-holder";
const UNKNOWN_AVD_MEMORY_MB: u64 = 2048;

fn status_label(status: &DeviceStatus) -> String {
    match status {
        DeviceStatus::Stopped => "已停止".into(),
        DeviceStatus::Queued(reason) => format!("等待资源：{reason}"),
        DeviceStatus::Starting => "启动中…".into(),
        DeviceStatus::Booting => "系统启动中…".into(),
        DeviceStatus::Running => "运行中".into(),
        DeviceStatus::Recovering(RecoveryReason::AdvertisementMissing) => {
            "恢复中：模拟器广告暂时缺失".into()
        }
        DeviceStatus::Recovering(RecoveryReason::ControlDisconnected) => {
            "恢复中：控制连接已断开".into()
        }
        DeviceStatus::Stopping => "停止中…".into(),
        DeviceStatus::Error(error) => format!("错误：{error}"),
    }
}

fn can_start(status: &DeviceStatus, has_session: bool) -> bool {
    !has_session && status.allows_start()
}

fn can_stop(status: &DeviceStatus, has_session: bool) -> bool {
    (!has_session && matches!(status, DeviceStatus::Queued(_)))
        || (has_session && (status.allows_stop() || matches!(status, DeviceStatus::Error(_))))
}

fn stop_label(status: &DeviceStatus, has_session: bool) -> &'static str {
    if !has_session && matches!(status, DeviceStatus::Queued(_)) {
        "取消"
    } else {
        "停止"
    }
}

/// 设备行的只读投影。
#[derive(Debug, Clone)]
pub struct DeviceData {
    pub name: String,
    pub path: PathBuf,
    pub status: DeviceStatus,
    pub inst: Option<RunningInstance>,
    pub resources: ResourceDemand,
}

#[derive(Clone)]
struct RowTargets {
    status: SendWeakRef<Label>,
    start: SendWeakRef<Button>,
    stop: SendWeakRef<Button>,
    viewport_holder: SendWeakRef<gtk4::Box>,
    controls: SendWeakRef<gtk4::FlowBox>,
}

/// 扫描广告文件并由 registry 合并，再投影全部 AVD。
pub fn list_devices(sdk_root: &Path, runtime: &DeviceRuntime) -> Vec<DeviceData> {
    let managed_policy = runtime.managed_gpu_policy();
    let avds = avd::list_avds();
    let adopted_demands = avds
        .iter()
        .map(|avd| (avd.name.clone(), adopted_resource_demand(&avd.config)))
        .collect();
    runtime.reconcile_running_for_sdk_with_demands(
        emulator::list_running_for_sdk(sdk_root),
        adopted_demands,
        sdk_root,
    );
    avds.into_iter()
        .map(|avd| {
            let projection = runtime.projection(&avd.name);
            let resources = managed_resource_demand_for_policy(&avd.config, managed_policy);
            DeviceData {
                name: avd.name,
                path: avd.path,
                status: projection.state.phase,
                inst: projection.session.map(|session| session.instance),
                resources,
            }
        })
        .collect()
}

fn configured_memory_mb(config: &std::collections::HashMap<String, String>) -> u64 {
    config
        .get("hw.ramSize")
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(UNKNOWN_AVD_MEMORY_MB)
}

#[cfg(test)]
fn managed_resource_demand(config: &std::collections::HashMap<String, String>) -> ResourceDemand {
    managed_resource_demand_for_policy(config, ManagedGpuPolicy::HeadlessSwangle)
}

fn managed_resource_demand_for_policy(
    config: &std::collections::HashMap<String, String>,
    policy: ManagedGpuPolicy,
) -> ResourceDemand {
    ResourceDemand::new(configured_memory_mb(config), policy.gpu_slots())
}

fn adopted_resource_demand(config: &std::collections::HashMap<String, String>) -> ResourceDemand {
    let gpu_slots = u32::from(config.get("hw.gpu.mode").is_some_and(|mode| mode == "host"));
    ResourceDemand::new(configured_memory_mb(config), gpu_slots)
}

fn publish_start_failure(
    runtime: &DeviceRuntime,
    command: &StartCommand,
    error: impl std::fmt::Display,
    on_status: &mut impl FnMut(DeviceStatus),
) {
    let phase = DeviceStatus::Error(error.to_string());
    if runtime.fail_start(command, error.to_string()) {
        on_status(phase);
    }
}

/// 启动设备。所有状态和 session 资源先写入 registry，再投影给 UI。
pub async fn start_device(
    data: &DeviceData,
    sdk_root: PathBuf,
    runtime: Arc<DeviceRuntime>,
    mut on_status: impl FnMut(DeviceStatus) + Send + 'static,
) {
    let (command, ticket, queue_status) = match runtime.schedule_start(&data.name, data.resources) {
        Ok(scheduled) => scheduled,
        Err(error) => {
            on_status(DeviceStatus::Error(error.to_string()));
            return;
        }
    };
    on_status(DeviceStatus::Queued(queue_status.message()));

    let permit = match ticket.wait_with_status(|status| {
        let reason = status.message();
        if runtime
            .update_queue_status(&command, reason.clone())
            .is_ok()
        {
            on_status(DeviceStatus::Queued(reason));
        }
    }) {
        Ok(permit) => permit,
        Err(SchedulerError::Canceled(_)) => {
            on_status(runtime.projection(&data.name).state.phase);
            return;
        }
        Err(error) => {
            publish_start_failure(&runtime, &command, error, &mut on_status);
            return;
        }
    };
    if let Err(error) = runtime.mark_starting(&command) {
        on_status(runtime.projection(&data.name).state.phase);
        drop(permit);
        emit(
            AppLogLevel::Warn,
            format_args!("排队任务获准后已过期：{error}"),
        );
        return;
    }
    on_status(DeviceStatus::Starting);

    let occupied = emulator::list_running()
        .into_iter()
        .map(|instance| instance.console_port);
    let reservation = match runtime.reserve_port(occupied) {
        Ok(reservation) => reservation,
        Err(error) => {
            publish_start_failure(&runtime, &command, error, &mut on_status);
            return;
        }
    };
    let port = reservation.port();
    if let Err(error) = runtime.attach_start_port(&command, port) {
        publish_start_failure(&runtime, &command, error, &mut on_status);
        return;
    }

    let grpc = match GrpcLaunchConfig::new(port + 3000) {
        Ok(grpc) => grpc,
        Err(error) => {
            publish_start_failure(&runtime, &command, error, &mut on_status);
            return;
        }
    };
    let params = LaunchParams {
        sdk_root,
        avd_name: data.name.clone(),
        port,
        grpc,
        gpu_policy: runtime.managed_gpu_policy(),
        audio_policy: ManagedAudioPolicy::VirtualMicrophone { required: false },
        no_window: true,
        share_vid: true,
    };
    let launched = match emulator::launch(&params).await {
        Ok(launched) => launched,
        Err(error) => {
            publish_start_failure(&runtime, &command, format!("{error:#}"), &mut on_status);
            return;
        }
    };
    let launched_instance = launched.instance.clone();
    let console_port = launched.instance.console_port;
    let log_path = launched.log_path().to_path_buf();

    if let Err(error) = runtime.mark_booting(&command) {
        let _ = emulator::stop_launched(&launched).await;
        publish_start_failure(
            &runtime,
            &command,
            format!("{error}; 日志：{}", log_path.display()),
            &mut on_status,
        );
        return;
    }
    on_status(DeviceStatus::Booting);

    let serial = format!("emulator-{console_port}");
    if let Err(error) = crate::core::adb::wait_for_boot(
        &params.sdk_root,
        &serial,
        std::time::Duration::from_secs(240),
    )
    .await
    {
        let cleanup = emulator::stop_launched(&launched).await;
        let message = match cleanup {
            Ok(()) => format!("{error:#}; 日志：{}", log_path.display()),
            Err(cleanup_error) => {
                format!(
                    "{error:#}; 启动失败后的清理也失败：{cleanup_error:#}; 日志：{}",
                    log_path.display()
                )
            }
        };
        publish_start_failure(&runtime, &command, message, &mut on_status);
        return;
    }

    match runtime.complete_scheduled_start(&command, launched, reservation, permit) {
        Ok(_) => on_status(DeviceStatus::Running),
        Err(error) => {
            let cleanup = emulator::stop_instance(&launched_instance, &params.sdk_root).await;
            let message = match cleanup {
                Ok(()) => format!("{error}; 日志：{}", log_path.display()),
                Err(cleanup_error) => {
                    format!(
                        "{error}; session 提交失败后的清理也失败：{cleanup_error:#}; 日志：{}",
                        log_path.display()
                    )
                }
            };
            publish_start_failure(&runtime, &command, message, &mut on_status);
        }
    }
}

/// 停止 registry 中的设备 session。失败时 session 与 reservation 保持可重试。
pub async fn stop_device(
    data: &DeviceData,
    sdk_root: PathBuf,
    runtime: Arc<DeviceRuntime>,
    mut on_status: impl FnMut(DeviceStatus) + Send + 'static,
) {
    if runtime.cancel_queued_start(&data.name) {
        on_status(DeviceStatus::Stopped);
        return;
    }
    let command = match runtime.begin_stop(&data.name) {
        Ok(command) => command,
        Err(_) => {
            on_status(runtime.projection(&data.name).state.phase);
            return;
        }
    };
    on_status(DeviceStatus::Stopping);

    let stop_result = match (command.launcher_pid(), command.sdk_root()) {
        (Some(launcher_pid), Some(session_sdk)) => {
            emulator::stop_managed(command.instance(), launcher_pid, session_sdk).await
        }
        _ => emulator::stop_instance(command.instance(), &sdk_root).await,
    };
    match stop_result {
        Ok(()) => match runtime.complete_stop(&command) {
            Ok(()) => on_status(DeviceStatus::Stopped),
            Err(error) => on_status(DeviceStatus::Error(error.to_string())),
        },
        Err(error) => {
            let message = format!("{error:#}");
            match runtime.fail_stop(&command, message.clone()) {
                Ok(()) => on_status(DeviceStatus::Error(message)),
                Err(stale) => on_status(DeviceStatus::Error(stale.to_string())),
            }
        }
    }
}

/// 构建单行卡片。状态变化直接更新该行，不再触发整张列表销毁重建。
pub fn build_row(data: &DeviceData, sdk_root: PathBuf, runtime: Arc<DeviceRuntime>) -> ListBoxRow {
    let microphone = crate::ui::microphone::MicrophoneController::new(runtime.clone());
    build_row_with_microphone(data, sdk_root, runtime, microphone)
}

fn build_row_with_microphone(
    data: &DeviceData,
    sdk_root: PathBuf,
    runtime: Arc<DeviceRuntime>,
    microphone: Arc<crate::ui::microphone::MicrophoneController>,
) -> ListBoxRow {
    let row = ListBoxRow::new();
    row.set_widget_name(&row_widget_name(&data.name));
    row.set_child(Some(&build_card(data, sdk_root, runtime, microphone)));
    row
}

/// 构建可放入 list 或响应式 workspace 的设备卡片主体。
pub(crate) fn build_card(
    data: &DeviceData,
    sdk_root: PathBuf,
    runtime: Arc<DeviceRuntime>,
    microphone: Arc<crate::ui::microphone::MicrophoneController>,
) -> gtk4::Box {
    let box_ = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    box_.add_css_class("card");
    box_.set_margin_top(8);
    box_.set_margin_bottom(8);
    box_.set_margin_start(12);
    box_.set_margin_end(12);

    let has_session = data.inst.is_some();
    let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let name = Label::new(Some(&data.name));
    name.add_css_class("title-4");
    top.append(&name);
    top.set_hexpand(true);

    let selected = CheckButton::with_label("选择");
    selected.set_widget_name(SELECT_WIDGET);
    selected.set_sensitive(has_session);
    selected.set_active(route_is_selected(&runtime, &data.name));
    let runtime_for_selection = runtime.clone();
    let avd_for_selection = data.name.clone();
    selected.connect_toggled(move |button| {
        let Some(route) = runtime_for_selection
            .input_route(&avd_for_selection)
            .map(|guard| guard.route().clone())
        else {
            button.set_active(false);
            return;
        };
        let is_selected = runtime_for_selection
            .workspace_snapshot()
            .selected
            .contains(&route);
        if button.is_active() != is_selected {
            let _ = runtime_for_selection.toggle_selected(&route);
        }
    });
    top.append(&selected);

    let status = Label::new(Some(&status_label(&data.status)));
    status.set_widget_name(STATUS_WIDGET);
    status.add_css_class("dim-label");
    top.append(&status);
    box_.append(&top);

    let detail = Label::new(Some(&data.path.display().to_string()));
    detail.add_css_class("caption");
    detail.add_css_class("dim-label");
    detail.set_xalign(0.0);
    box_.append(&detail);

    let viewport_holder = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    viewport_holder.set_widget_name(VIEWPORT_HOLDER_WIDGET);
    viewport_holder.set_hexpand(true);
    sync_viewport(
        &viewport_holder,
        data.inst.is_some(),
        runtime.capture_subscription(&data.name),
        runtime.grpc_client(&data.name),
        runtime.input_route(&data.name),
    );
    box_.append(&viewport_holder);

    let controls = crate::ui::device_controls::build(
        &data.name,
        has_session && runtime.grpc_client(&data.name).is_some(),
        runtime.clone(),
        microphone,
    );
    box_.append(&controls);

    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    buttons.set_halign(gtk4::Align::End);
    let start_btn = Button::with_label("启动");
    start_btn.set_widget_name(START_WIDGET);
    start_btn.add_css_class("suggested-action");
    start_btn.set_sensitive(can_start(&data.status, has_session));
    let stop_btn = Button::with_label(stop_label(&data.status, has_session));
    stop_btn.set_widget_name(STOP_WIDGET);
    stop_btn.add_css_class("destructive-action");
    stop_btn.set_sensitive(can_stop(&data.status, has_session));

    let data_for_start = data.clone();
    let sdk_for_start = sdk_root.clone();
    let runtime_for_start = runtime.clone();
    let targets = RowTargets {
        status: SendWeakRef::from(status.downgrade()),
        start: SendWeakRef::from(start_btn.downgrade()),
        stop: SendWeakRef::from(stop_btn.downgrade()),
        viewport_holder: SendWeakRef::from(viewport_holder.downgrade()),
        controls: SendWeakRef::from(controls.downgrade()),
    };
    start_btn.connect_clicked(move |_| {
        let data = data_for_start.clone();
        let sdk = sdk_for_start.clone();
        let runtime = runtime_for_start.clone();
        let runtime_for_status = runtime.clone();
        let avd_name = data.name.clone();
        let targets = targets.clone();
        spawn_async(async move {
            start_device(&data, sdk, runtime, move |phase| {
                let has_session = runtime_for_status.projection(&avd_name).session.is_some();
                let capture = runtime_for_status.capture_subscription(&avd_name);
                let grpc = runtime_for_status.grpc_client(&avd_name);
                let route = runtime_for_status.input_route(&avd_name);
                update_row(phase, has_session, targets.clone(), capture, grpc, route);
            })
            .await;
        });
    });
    buttons.append(&start_btn);

    let data_for_stop = data.clone();
    let sdk_for_stop = sdk_root;
    let runtime_for_stop = runtime;
    let targets = RowTargets {
        status: SendWeakRef::from(status.downgrade()),
        start: SendWeakRef::from(start_btn.downgrade()),
        stop: SendWeakRef::from(stop_btn.downgrade()),
        viewport_holder: SendWeakRef::from(viewport_holder.downgrade()),
        controls: SendWeakRef::from(controls.downgrade()),
    };
    stop_btn.connect_clicked(move |_| {
        let data = data_for_stop.clone();
        let sdk = sdk_for_stop.clone();
        let runtime = runtime_for_stop.clone();
        let runtime_for_status = runtime.clone();
        let avd_name = data.name.clone();
        let targets = targets.clone();
        spawn_async(async move {
            stop_device(&data, sdk, runtime, move |phase| {
                let has_session = runtime_for_status.projection(&avd_name).session.is_some();
                let capture = runtime_for_status.capture_subscription(&avd_name);
                let grpc = runtime_for_status.grpc_client(&avd_name);
                let route = runtime_for_status.input_route(&avd_name);
                update_row(phase, has_session, targets.clone(), capture, grpc, route);
            })
            .await;
        });
    });
    buttons.append(&stop_btn);
    box_.append(&buttons);

    box_
}

fn update_row(
    phase: DeviceStatus,
    has_session: bool,
    targets: RowTargets,
    capture: Option<crate::core::stream::CaptureSubscription>,
    grpc: Option<crate::core::grpc::GrpcClient>,
    route: Option<crate::core::instance::InputRouteGuard>,
) {
    let label = status_label(&phase);
    let can_control = has_session && grpc.is_some();
    post_ui(move || {
        if let Some(widget) = targets.status.upgrade() {
            widget.set_label(&label);
        }
        if let Some(button) = targets.start.upgrade() {
            button.set_sensitive(can_start(&phase, has_session));
        }
        if let Some(button) = targets.stop.upgrade() {
            button.set_label(stop_label(&phase, has_session));
            button.set_sensitive(can_stop(&phase, has_session));
        }
        if let Some(holder) = targets.viewport_holder.upgrade() {
            sync_viewport(&holder, has_session, capture, grpc, route);
        }
        if let Some(controls) = targets.controls.upgrade() {
            controls.set_sensitive(can_control);
        }
    });
}

/// 调度到 GTK 主线程执行（任意线程可调）。
fn post_ui(f: impl FnOnce() + Send + 'static) {
    glib::MainContext::default().invoke(f);
}

/// 提交到 UI 共用的长存 Tokio executor。
pub fn spawn_async<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    crate::ui::background::spawn(future);
}

/// 构建主列表。
pub fn build_list(sdk_root: PathBuf, runtime: Arc<DeviceRuntime>) -> ListBox {
    let list = ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::None);
    let devices = list_devices(&sdk_root, &runtime);
    rebuild_list_contents(&list, &devices, &sdk_root, &runtime);
    list
}

/// 将 registry 的最新投影刷新到现有行。只有 AVD 集合/顺序变化时才重建，
/// 因而广告文件事件不会打断正在运行的行内启动/停止回调。
pub fn refresh_list(list: &ListBox, sdk_root: PathBuf, runtime: Arc<DeviceRuntime>) {
    let devices = list_devices(&sdk_root, &runtime);
    let mut expected: Vec<_> = devices
        .iter()
        .map(|device| row_widget_name(&device.name))
        .collect();
    if expected.is_empty() {
        expected.push("liteavd-empty-row".into());
    }
    let actual = list_row_names(list);
    if actual != expected {
        rebuild_list_contents(list, &devices, &sdk_root, &runtime);
        return;
    }

    for (index, device) in devices.iter().enumerate() {
        if let Some(row) = list.row_at_index(index as i32) {
            apply_projection(row.upcast_ref(), device, &runtime);
        }
    }
}

fn rebuild_list_contents(
    list: &ListBox,
    devices: &[DeviceData],
    sdk_root: &Path,
    runtime: &Arc<DeviceRuntime>,
) {
    while let Some(row) = list.row_at_index(0) {
        list.remove(&row);
    }
    let microphone = crate::ui::microphone::MicrophoneController::new(runtime.clone());
    for data in devices {
        list.append(&build_row_with_microphone(
            data,
            sdk_root.to_path_buf(),
            runtime.clone(),
            microphone.clone(),
        ));
    }
    if devices.is_empty() {
        let empty = Label::new(Some("没有 AVD。"));
        empty.add_css_class("dim-label");
        empty.set_margin_top(24);
        list.append(&empty);
        if let Some(row) = list.row_at_index(0) {
            row.set_widget_name("liteavd-empty-row");
        }
    }
}

pub(crate) fn apply_projection(
    root: &gtk4::Widget,
    data: &DeviceData,
    runtime: &Arc<DeviceRuntime>,
) {
    let phase = &data.status;
    let has_session = data.inst.is_some();
    if let Some(label) =
        find_named_widget(root, STATUS_WIDGET).and_then(|widget| widget.downcast::<Label>().ok())
    {
        label.set_label(&status_label(phase));
    }
    if let Some(button) =
        find_named_widget(root, START_WIDGET).and_then(|widget| widget.downcast::<Button>().ok())
    {
        button.set_sensitive(can_start(phase, has_session));
    }
    if let Some(button) =
        find_named_widget(root, STOP_WIDGET).and_then(|widget| widget.downcast::<Button>().ok())
    {
        button.set_sensitive(can_stop(phase, has_session));
    }
    if let Some(selected) = find_named_widget(root, SELECT_WIDGET)
        .and_then(|widget| widget.downcast::<CheckButton>().ok())
    {
        selected.set_sensitive(has_session);
        selected.set_active(has_session && route_is_selected(runtime, &data.name));
    }
    if let Some(controls) = find_named_widget(root, crate::ui::device_controls::CONTROLS_WIDGET)
        .and_then(|widget| widget.downcast::<gtk4::FlowBox>().ok())
    {
        controls.set_sensitive(has_session && runtime.grpc_client(&data.name).is_some());
    }
    if let Some(holder) = find_named_widget(root, VIEWPORT_HOLDER_WIDGET)
        .and_then(|widget| widget.downcast::<gtk4::Box>().ok())
    {
        sync_viewport(
            &holder,
            has_session,
            runtime.capture_subscription(&data.name),
            runtime.grpc_client(&data.name),
            runtime.input_route(&data.name),
        );
    }
}

pub(crate) fn apply_runtime_projection(
    root: &gtk4::Widget,
    avd_name: &str,
    runtime: &Arc<DeviceRuntime>,
) {
    let projection = runtime.projection(avd_name);
    apply_projection(
        root,
        &DeviceData {
            name: avd_name.to_owned(),
            path: PathBuf::new(),
            status: projection.state.phase,
            inst: projection.session.map(|session| session.instance),
            resources: ResourceDemand::default(),
        },
        runtime,
    );
}

fn route_is_selected(runtime: &Arc<DeviceRuntime>, avd_name: &str) -> bool {
    runtime.input_route(avd_name).is_some_and(|guard| {
        runtime
            .workspace_snapshot()
            .selected
            .contains(guard.route())
    })
}

fn sync_viewport(
    holder: &gtk4::Box,
    has_session: bool,
    capture: Option<crate::core::stream::CaptureSubscription>,
    grpc: Option<crate::core::grpc::GrpcClient>,
    route: Option<crate::core::instance::InputRouteGuard>,
) {
    if !has_session {
        while let Some(child) = holder.first_child() {
            holder.remove(&child);
        }
        return;
    }
    let route_class = route.as_ref().map(|route| {
        let route = route.route();
        format!("liteavd-route-{}-{}", route.session_id, route.generation)
    });
    let route_matches = match (holder.first_child(), route_class.as_deref()) {
        (Some(child), Some(class)) => child.has_css_class(class),
        (Some(_), None) => true,
        (None, _) => false,
    };
    if !route_matches {
        while let Some(child) = holder.first_child() {
            holder.remove(&child);
        }
    }
    if holder.first_child().is_none() {
        match (capture, grpc, route) {
            (Some(subscription), Some(client), Some(route)) => {
                let viewport =
                    crate::ui::viewport::build_routed_interactive(subscription, client, route);
                if let Some(class) = route_class {
                    viewport.add_css_class(&class);
                }
                holder.append(&viewport);
            }
            (Some(subscription), _, _) => {
                holder.append(&crate::ui::viewport::build(subscription));
            }
            (None, _, _) => {}
        }
    }
}

fn find_named_widget(root: &gtk4::Widget, name: &str) -> Option<gtk4::Widget> {
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if widget.widget_name() == name {
            return Some(widget);
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            stack.push(next.clone());
            child = next.next_sibling();
        }
    }
    None
}

fn list_row_names(list: &ListBox) -> Vec<String> {
    let mut names = Vec::new();
    let mut index = 0;
    while let Some(row) = list.row_at_index(index) {
        names.push(row.widget_name().to_string());
        index += 1;
    }
    names
}

fn row_widget_name(avd_name: &str) -> String {
    format!("liteavd-device-row-{avd_name}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn resource_demand_is_conservative_for_missing_or_invalid_ram() {
        assert_eq!(
            managed_resource_demand(&HashMap::new()).memory_mb,
            UNKNOWN_AVD_MEMORY_MB
        );
        assert_eq!(
            managed_resource_demand(&HashMap::from([("hw.ramSize".into(), "invalid".into())]))
                .memory_mb,
            UNKNOWN_AVD_MEMORY_MB
        );
        assert_eq!(
            managed_resource_demand(&HashMap::from([("hw.ramSize".into(), "1536".into())]))
                .memory_mb,
            1536
        );
    }

    #[test]
    fn managed_gpu_policy_controls_scheduler_slot_demand() {
        let config = HashMap::from([("hw.ramSize".into(), "1536".into())]);
        assert_eq!(
            managed_resource_demand_for_policy(&config, ManagedGpuPolicy::HeadlessSwangle),
            ResourceDemand::new(1536, 0)
        );
        assert_eq!(
            managed_resource_demand_for_policy(&config, ManagedGpuPolicy::DesktopHost),
            ResourceDemand::new(1536, 1)
        );
    }

    #[test]
    fn recovery_states_remain_stoppable_and_explain_the_failed_channel() {
        let advertisement = DeviceStatus::Recovering(RecoveryReason::AdvertisementMissing);
        assert!(can_stop(&advertisement, true));
        assert!(status_label(&advertisement).contains("广告"));
        let control = DeviceStatus::Recovering(RecoveryReason::ControlDisconnected);
        assert!(can_stop(&control, true));
        assert!(status_label(&control).contains("控制连接"));
    }

    #[test]
    fn queued_ui_start_can_be_canceled_before_emulator_spawn() {
        let runtime = Arc::new(DeviceRuntime::default());
        let (blocking, mut blocking_ticket, _) = runtime
            .schedule_start("blocking", ResourceDemand::default())
            .unwrap();
        let blocking_permit = blocking_ticket.try_acquire().unwrap().unwrap();
        runtime.mark_starting(&blocking).unwrap();

        let data = DeviceData {
            name: "queued-ui-test".into(),
            path: PathBuf::from("/tmp/queued-ui-test.avd"),
            status: DeviceStatus::Stopped,
            inst: None,
            resources: ResourceDemand::default(),
        };
        let (start_status_tx, start_status_rx) = mpsc::channel();
        let start_runtime = runtime.clone();
        let start_data = data.clone();
        let start_worker = std::thread::spawn(move || {
            let tokio = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio.block_on(start_device(
                &start_data,
                PathBuf::from("/nonexistent-sdk"),
                start_runtime,
                move |status| start_status_tx.send(status).unwrap(),
            ));
        });
        assert!(matches!(
            start_status_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            DeviceStatus::Queued(_)
        ));

        let (stop_status_tx, stop_status_rx) = mpsc::channel();
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio.block_on(stop_device(
            &data,
            PathBuf::from("/nonexistent-sdk"),
            runtime.clone(),
            move |status| stop_status_tx.send(status).unwrap(),
        ));
        assert_eq!(
            stop_status_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            DeviceStatus::Stopped
        );
        start_worker.join().unwrap();
        assert_eq!(
            runtime.projection(&data.name).state.phase,
            DeviceStatus::Stopped
        );

        drop(blocking_permit);
        runtime.fail_start(&blocking, "test cleanup".into());
    }
}
