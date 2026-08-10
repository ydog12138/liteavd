//! 跨设备操作的目标快照、确认和逐设备结果模型。
//!
//! plan 固化 exact session route；authorize 在用户确认后再次校验目标。执行时每个
//! 目标独立返回结果，单设备失败不会阻止其他设备，也不会把结果归到同名替换 session。

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::core::grpc::KeyEventType;
use crate::core::grpc::SnapshotDetails;
use crate::core::input::DeviceKey;
use crate::core::instance::{DeviceRuntime, RegistryError};
use crate::core::workspace::{OperationScope, WorkspaceRoute};
use crate::core::{adb, emulator};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(u64);

impl OperationId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Screenshot,
    InstallApk,
    PushFiles,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotMutation {
    Save,
    Load,
    Delete,
}

impl OperationKind {
    pub fn requires_confirmation(self) -> bool {
        matches!(self, Self::InstallApk | Self::PushFiles | Self::Stop)
    }
}

#[derive(Debug, Clone, Default)]
pub struct OperationCancellation(Arc<AtomicBool>);

impl OperationCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationProgressStage {
    Starting,
    Transferring,
    Publishing,
    CleaningUp,
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationProgress {
    pub id: OperationId,
    pub route: WorkspaceRoute,
    pub stage: OperationProgressStage,
    pub completed_items: usize,
    pub total_items: usize,
}

pub type OperationProgressSink = Arc<dyn Fn(OperationProgress) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApkInstallRequest {
    pub apks: Vec<PathBuf>,
    pub options: adb::ApkInstallOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushFilesRequest {
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationPlan {
    id: OperationId,
    kind: OperationKind,
    scope: OperationScope,
    targets: Vec<WorkspaceRoute>,
}

impl OperationPlan {
    pub fn id(&self) -> OperationId {
        self.id
    }

    pub fn kind(&self) -> OperationKind {
        self.kind
    }

    pub fn scope(&self) -> OperationScope {
        self.scope
    }

    pub fn targets(&self) -> &[WorkspaceRoute] {
        &self.targets
    }

    pub fn requires_confirmation(&self) -> bool {
        self.kind.requires_confirmation()
    }
}

#[derive(Debug)]
pub struct AuthorizedOperation(OperationPlan);

impl AuthorizedOperation {
    pub fn plan(&self) -> &OperationPlan {
        &self.0
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum OperationPlanError {
    #[error("{scope:?} 没有可操作的运行 session")]
    EmptyTargets { scope: OperationScope },
    #[error("确认期间操作目标已变化：{0:?}")]
    TargetsChanged(Vec<WorkspaceRoute>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationSuccess {
    Screenshot {
        path: PathBuf,
        bytes: u64,
    },
    ApksInstalled {
        files: usize,
        exit_code: Option<i32>,
    },
    FilesPushed {
        paths: Vec<String>,
        bytes: u64,
        exit_code: Option<i32>,
    },
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationResult {
    Succeeded(OperationSuccess),
    Failed(String),
    Canceled,
    StaleRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationRunError {
    Failed(String),
    Canceled,
    StaleRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceOperationResult {
    pub route: WorkspaceRoute,
    pub result: OperationResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationReport {
    pub id: OperationId,
    pub kind: OperationKind,
    pub devices: Vec<DeviceOperationResult>,
}

#[derive(Debug, Default)]
pub struct OperationCoordinator {
    next_id: AtomicU64,
}

#[derive(Debug, Error)]
pub enum OperationExecutionError {
    #[error("operation kind 不符：期望 {expected:?}，实际 {actual:?}")]
    KindMismatch {
        expected: OperationKind,
        actual: OperationKind,
    },
    #[error("创建截图目录失败：{path}: {source}")]
    ScreenshotDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("APK 必须是存在的普通 .apk 文件：{0}")]
    InvalidApk(PathBuf),
    #[error("至少需要选择一个 APK")]
    EmptyApks,
    #[error("推送源必须是存在且非符号链接的普通文件：{0}")]
    InvalidPushFile(PathBuf),
    #[error("至少需要选择一个待推送文件")]
    EmptyPushFiles,
}

impl OperationCoordinator {
    pub fn plan(
        &self,
        kind: OperationKind,
        scope: OperationScope,
        mut targets: Vec<WorkspaceRoute>,
    ) -> Result<OperationPlan, OperationPlanError> {
        targets.sort();
        targets.dedup();
        if targets.is_empty() {
            return Err(OperationPlanError::EmptyTargets { scope });
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(OperationPlan {
            id: OperationId(id),
            kind,
            scope,
            targets,
        })
    }

    /// 用户确认后以当前 exact routes 复验，不允许确认对话框打开期间静默换目标。
    pub fn authorize(
        &self,
        plan: OperationPlan,
        current_routes: impl IntoIterator<Item = WorkspaceRoute>,
    ) -> Result<AuthorizedOperation, OperationPlanError> {
        let current: BTreeSet<_> = current_routes.into_iter().collect();
        let changed: Vec<_> = plan
            .targets
            .iter()
            .filter(|route| !current.contains(*route))
            .cloned()
            .collect();
        if changed.is_empty() {
            Ok(AuthorizedOperation(plan))
        } else {
            Err(OperationPlanError::TargetsChanged(changed))
        }
    }

    pub async fn execute_with<F, Fut, C>(
        &self,
        authorized: AuthorizedOperation,
        mut is_current: C,
        mut run: F,
    ) -> OperationReport
    where
        F: FnMut(WorkspaceRoute) -> Fut,
        Fut: Future<Output = Result<OperationSuccess, OperationRunError>>,
        C: FnMut(&WorkspaceRoute) -> bool,
    {
        let plan = authorized.0;
        let mut devices = Vec::with_capacity(plan.targets.len());
        for route in plan.targets {
            let result = if !is_current(&route) {
                OperationResult::StaleRoute
            } else {
                match run(route.clone()).await {
                    Ok(success) => OperationResult::Succeeded(success),
                    Err(OperationRunError::Failed(error)) => OperationResult::Failed(error),
                    Err(OperationRunError::Canceled) => OperationResult::Canceled,
                    Err(OperationRunError::StaleRoute) => OperationResult::StaleRoute,
                }
            };
            devices.push(DeviceOperationResult { route, result });
        }
        OperationReport {
            id: plan.id,
            kind: plan.kind,
            devices,
        }
    }
}

/// 对 exact route 列举 snapshot；adopted/替换 session 不会借用其他设备的控制面。
pub async fn list_route_snapshots(
    runtime: Arc<DeviceRuntime>,
    route: WorkspaceRoute,
) -> Result<Vec<SnapshotDetails>, OperationRunError> {
    let client_config = runtime
        .grpc_client_for_route(&route)
        .ok_or(OperationRunError::StaleRoute)?;
    let client = match client_config.reconnect().await {
        Ok(client) => client,
        Err(error) => {
            runtime.report_control_disconnected(&route);
            return Err(OperationRunError::Failed(format!("{error:#}")));
        }
    };
    if !runtime.route_is_current(&route) {
        return Err(OperationRunError::StaleRoute);
    }
    let snapshots = match client.list_snapshots().await {
        Ok(snapshots) => {
            runtime.report_control_connected(&route);
            snapshots
        }
        Err(error) => {
            runtime.report_control_disconnected(&route);
            return Err(OperationRunError::Failed(format!("{error:#}")));
        }
    };
    if !runtime.route_is_current(&route) {
        return Err(OperationRunError::StaleRoute);
    }
    Ok(snapshots)
}

/// 对 exact route 执行本地 snapshot 写操作。
pub async fn mutate_route_snapshot(
    runtime: Arc<DeviceRuntime>,
    route: WorkspaceRoute,
    snapshot_id: String,
    mutation: SnapshotMutation,
) -> Result<(), OperationRunError> {
    let client_config = runtime
        .grpc_client_for_route(&route)
        .ok_or(OperationRunError::StaleRoute)?;
    let client = match client_config.reconnect().await {
        Ok(client) => client,
        Err(error) => {
            runtime.report_control_disconnected(&route);
            return Err(OperationRunError::Failed(format!("{error:#}")));
        }
    };
    if !runtime.route_is_current(&route) {
        return Err(OperationRunError::StaleRoute);
    }
    let result = match mutation {
        SnapshotMutation::Save => client.save_snapshot(&snapshot_id).await,
        SnapshotMutation::Load => client.load_snapshot(&snapshot_id).await,
        SnapshotMutation::Delete => client.delete_snapshot(&snapshot_id).await,
    };
    match result {
        Ok(()) => runtime.report_control_connected(&route),
        Err(error) => {
            return Err(OperationRunError::Failed(format!("{error:#}")));
        }
    };
    if !runtime.route_is_current(&route) {
        return Err(OperationRunError::StaleRoute);
    }
    if mutation == SnapshotMutation::Load {
        runtime.request_control_stream_reset(&route);
    }
    Ok(())
}

/// 向一个 exact session 发送产品允许的单次硬件键。
///
/// 这是卡片快捷控制的 core 边界；连接前后都复验 route，adopted 或已替换
/// session 不会借用同名设备的新控制面。
pub async fn send_route_keypress(
    runtime: Arc<DeviceRuntime>,
    route: WorkspaceRoute,
    key: DeviceKey,
) -> Result<(), OperationRunError> {
    let client_config = runtime
        .grpc_client_for_route(&route)
        .ok_or(OperationRunError::StaleRoute)?;
    let client = match client_config.reconnect().await {
        Ok(client) => client,
        Err(error) => {
            runtime.report_control_disconnected(&route);
            return Err(OperationRunError::Failed(format!("{error:#}")));
        }
    };
    if !runtime.route_is_current(&route) {
        return Err(OperationRunError::StaleRoute);
    }
    match client
        .send_key(key.grpc_key(), KeyEventType::Keypress)
        .await
    {
        Ok(()) => {
            runtime.report_control_connected(&route);
        }
        Err(error) => {
            runtime.report_control_disconnected(&route);
            return Err(OperationRunError::Failed(format!("{error:#}")));
        }
    }
    if !runtime.route_is_current(&route) {
        return Err(OperationRunError::StaleRoute);
    }
    Ok(())
}

pub async fn execute_screenshots(
    runtime: Arc<DeviceRuntime>,
    authorized: AuthorizedOperation,
    output_dir: PathBuf,
) -> Result<OperationReport, OperationExecutionError> {
    require_kind(&authorized, OperationKind::Screenshot)?;
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|source| OperationExecutionError::ScreenshotDirectory {
            path: output_dir.clone(),
            source,
        })?;
    let operation_id = authorized.plan().id().get();
    let runner_runtime = runtime.clone();
    Ok(runtime
        .execute_operation_with(authorized, move |route| {
            let runtime = runner_runtime.clone();
            let output_dir = output_dir.clone();
            async move {
                let client_config = runtime
                    .grpc_client_for_route(&route)
                    .ok_or(OperationRunError::StaleRoute)?;
                let client = match client_config.reconnect().await {
                    Ok(client) => {
                        runtime.report_control_connected(&route);
                        client
                    }
                    Err(error) => {
                        runtime.report_control_disconnected(&route);
                        return Err(OperationRunError::Failed(format!("{error:#}")));
                    }
                };
                if !runtime.route_is_current(&route) {
                    return Err(OperationRunError::StaleRoute);
                }
                let image = match client.screenshot(0, 0).await {
                    Ok(image) => {
                        runtime.report_control_connected(&route);
                        image
                    }
                    Err(error) => {
                        runtime.report_control_disconnected(&route);
                        return Err(OperationRunError::Failed(format!("{error:#}")));
                    }
                };
                if !runtime.route_is_current(&route) {
                    return Err(OperationRunError::StaleRoute);
                }
                if !image.image.starts_with(b"\x89PNG\r\n\x1a\n") {
                    return Err(OperationRunError::Failed("模拟器返回的截图不是 PNG".into()));
                }
                let path = output_dir.join(format!(
                    "{}-op{operation_id}.png",
                    safe_artifact_name(&route.avd_name)
                ));
                publish_screenshot(&path, &image.image, || runtime.route_is_current(&route))
                    .await?;
                Ok(OperationSuccess::Screenshot {
                    path,
                    bytes: image.image.len() as u64,
                })
            }
        })
        .await)
}

pub async fn execute_install_apk(
    runtime: Arc<DeviceRuntime>,
    authorized: AuthorizedOperation,
    sdk_root: PathBuf,
    apk: PathBuf,
) -> Result<OperationReport, OperationExecutionError> {
    execute_install_apks(
        runtime,
        authorized,
        sdk_root,
        ApkInstallRequest {
            apks: vec![apk],
            options: adb::ApkInstallOptions::default(),
        },
        OperationCancellation::default(),
        None,
    )
    .await
}

pub async fn execute_install_apks(
    runtime: Arc<DeviceRuntime>,
    authorized: AuthorizedOperation,
    sdk_root: PathBuf,
    request: ApkInstallRequest,
    cancellation: OperationCancellation,
    progress: Option<OperationProgressSink>,
) -> Result<OperationReport, OperationExecutionError> {
    require_kind(&authorized, OperationKind::InstallApk)?;
    if request.apks.is_empty() {
        return Err(OperationExecutionError::EmptyApks);
    }
    for apk in &request.apks {
        if !is_apk_file(apk).await {
            return Err(OperationExecutionError::InvalidApk(apk.clone()));
        }
    }
    let operation_id = authorized.plan().id();
    let runner_runtime = runtime.clone();
    Ok(runtime
        .execute_operation_with(authorized, move |route| {
            let runtime = runner_runtime.clone();
            let sdk_root = sdk_root.clone();
            let request = request.clone();
            let cancellation = cancellation.clone();
            let progress = progress.clone();
            async move {
                report_progress(
                    &progress,
                    operation_id,
                    &route,
                    OperationProgressStage::Starting,
                    0,
                    request.apks.len(),
                );
                let session = runtime
                    .session_for_route(&route)
                    .ok_or(OperationRunError::StaleRoute)?;
                let serial = format!("emulator-{}", session.instance.console_port);
                let output = adb::install_apks_cancellable(
                    &sdk_root,
                    &serial,
                    &request.apks,
                    request.options,
                    || cancellation.is_canceled() || !runtime.route_is_current(&route),
                )
                .await
                .map_err(|error| {
                    classify_adb_failure(&runtime, &route, &cancellation, error.to_string())
                })?;
                ensure_operation_current(&runtime, &route, &cancellation)?;
                report_progress(
                    &progress,
                    operation_id,
                    &route,
                    OperationProgressStage::Finished,
                    request.apks.len(),
                    request.apks.len(),
                );
                Ok(OperationSuccess::ApksInstalled {
                    files: request.apks.len(),
                    exit_code: output.status.code(),
                })
            }
        })
        .await)
}

const GUEST_PUSH_ROOT: &str = "/sdcard/Download/liteavd";
const PUSH_TIMEOUT: Duration = Duration::from_secs(600);
const PUSH_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn execute_push_files(
    runtime: Arc<DeviceRuntime>,
    authorized: AuthorizedOperation,
    sdk_root: PathBuf,
    request: PushFilesRequest,
    cancellation: OperationCancellation,
    progress: Option<OperationProgressSink>,
) -> Result<OperationReport, OperationExecutionError> {
    require_kind(&authorized, OperationKind::PushFiles)?;
    if request.files.is_empty() {
        return Err(OperationExecutionError::EmptyPushFiles);
    }
    let mut total_bytes = 0_u64;
    for file in &request.files {
        let Ok(metadata) = tokio::fs::symlink_metadata(file).await else {
            return Err(OperationExecutionError::InvalidPushFile(file.clone()));
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(OperationExecutionError::InvalidPushFile(file.clone()));
        }
        total_bytes = total_bytes.saturating_add(metadata.len());
    }
    let operation_id = authorized.plan().id();
    let runner_runtime = runtime.clone();
    Ok(runtime
        .execute_operation_with(authorized, move |route| {
            let runtime = runner_runtime.clone();
            let sdk_root = sdk_root.clone();
            let request = request.clone();
            let cancellation = cancellation.clone();
            let progress = progress.clone();
            async move {
                push_files_to_route(
                    runtime,
                    route,
                    sdk_root,
                    request.files,
                    total_bytes,
                    operation_id,
                    cancellation,
                    progress,
                )
                .await
            }
        })
        .await)
}

#[allow(clippy::too_many_arguments)]
async fn push_files_to_route(
    runtime: Arc<DeviceRuntime>,
    route: WorkspaceRoute,
    sdk_root: PathBuf,
    files: Vec<PathBuf>,
    total_bytes: u64,
    operation_id: OperationId,
    cancellation: OperationCancellation,
    progress: Option<OperationProgressSink>,
) -> Result<OperationSuccess, OperationRunError> {
    report_progress(
        &progress,
        operation_id,
        &route,
        OperationProgressStage::Starting,
        0,
        files.len(),
    );
    let session = runtime
        .session_for_route(&route)
        .ok_or(OperationRunError::StaleRoute)?;
    let serial = format!("emulator-{}", session.instance.console_port);
    run_exact_adb(
        &runtime,
        &route,
        &cancellation,
        &sdk_root,
        &serial,
        [
            OsString::from("shell"),
            OsString::from("mkdir"),
            OsString::from("-p"),
            OsString::from(GUEST_PUSH_ROOT),
        ],
        PUSH_CONTROL_TIMEOUT,
    )
    .await?;

    let destinations = files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let final_path = format!(
                "{GUEST_PUSH_ROOT}/{}",
                guest_artifact_name(file, operation_id, index + 1)
            );
            let part_path = format!("{final_path}.part");
            (file.clone(), part_path, final_path)
        })
        .collect::<Vec<_>>();
    let mut staged = Vec::new();
    let mut published = Vec::new();
    let operation_result = async {
        for (_, _, final_path) in &destinations {
            run_exact_adb(
                &runtime,
                &route,
                &cancellation,
                &sdk_root,
                &serial,
                [
                    OsString::from("shell"),
                    OsString::from("test"),
                    OsString::from("!"),
                    OsString::from("-e"),
                    OsString::from(final_path),
                ],
                PUSH_CONTROL_TIMEOUT,
            )
            .await
            .map_err(|error| match error {
                OperationRunError::Failed(_) => {
                    OperationRunError::Failed(format!("远端目标已存在，未覆盖：{final_path}"))
                }
                other => other,
            })?;
        }
        for (index, (local, part_path, _)) in destinations.iter().enumerate() {
            report_progress(
                &progress,
                operation_id,
                &route,
                OperationProgressStage::Transferring,
                index,
                files.len(),
            );
            // `adb push` may have created a partial remote file before it exits or is
            // canceled, so register the unique staging path before starting it.
            staged.push(part_path.clone());
            run_exact_adb(
                &runtime,
                &route,
                &cancellation,
                &sdk_root,
                &serial,
                [
                    OsString::from("push"),
                    local.as_os_str().to_owned(),
                    OsString::from(part_path),
                ],
                PUSH_TIMEOUT,
            )
            .await?;
        }
        for (index, (_, part_path, final_path)) in destinations.iter().enumerate() {
            report_progress(
                &progress,
                operation_id,
                &route,
                OperationProgressStage::Publishing,
                index,
                files.len(),
            );
            run_exact_adb(
                &runtime,
                &route,
                &cancellation,
                &sdk_root,
                &serial,
                [
                    OsString::from("shell"),
                    OsString::from("mv"),
                    OsString::from("-n"),
                    OsString::from(part_path),
                    OsString::from(final_path),
                ],
                PUSH_CONTROL_TIMEOUT,
            )
            .await?;
            run_exact_adb(
                &runtime,
                &route,
                &cancellation,
                &sdk_root,
                &serial,
                [
                    OsString::from("shell"),
                    OsString::from("test"),
                    OsString::from("!"),
                    OsString::from("-e"),
                    OsString::from(part_path),
                ],
                PUSH_CONTROL_TIMEOUT,
            )
            .await
            .map_err(|error| match error {
                OperationRunError::Failed(_) => OperationRunError::Failed(format!(
                    "远端目标在发布时出现冲突，未覆盖：{final_path}"
                )),
                other => other,
            })?;
            run_exact_adb(
                &runtime,
                &route,
                &cancellation,
                &sdk_root,
                &serial,
                [
                    OsString::from("shell"),
                    OsString::from("test"),
                    OsString::from("-f"),
                    OsString::from(final_path),
                ],
                PUSH_CONTROL_TIMEOUT,
            )
            .await?;
            staged.retain(|path| path != part_path);
            published.push(final_path.clone());
        }
        ensure_operation_current(&runtime, &route, &cancellation)?;
        Ok(OperationSuccess::FilesPushed {
            paths: published.clone(),
            bytes: total_bytes,
            exit_code: Some(0),
        })
    }
    .await;

    if operation_result.is_err() && runtime.route_is_current(&route) {
        report_progress(
            &progress,
            operation_id,
            &route,
            OperationProgressStage::CleaningUp,
            0,
            files.len(),
        );
        cleanup_guest_files(&runtime, &route, &sdk_root, &serial, &staged, &published).await;
    }
    if operation_result.is_ok() {
        report_progress(
            &progress,
            operation_id,
            &route,
            OperationProgressStage::Finished,
            files.len(),
            files.len(),
        );
    }
    operation_result
}

async fn run_exact_adb<I, S>(
    runtime: &DeviceRuntime,
    route: &WorkspaceRoute,
    cancellation: &OperationCancellation,
    sdk_root: &Path,
    serial: &str,
    args: I,
    timeout: Duration,
) -> Result<adb::AdbCommandOutput, OperationRunError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    ensure_operation_current(runtime, route, cancellation)?;
    let output = adb::run_cancellable(sdk_root, serial, args, timeout, || {
        cancellation.is_canceled() || !runtime.route_is_current(route)
    })
    .await
    .map_err(|error| classify_adb_failure(runtime, route, cancellation, error.to_string()))?;
    ensure_operation_current(runtime, route, cancellation)?;
    if !output.success() {
        return Err(OperationRunError::Failed(format!(
            "adb 命令退出 {:?}：{}",
            output.status.code(),
            output.failure_summary()
        )));
    }
    Ok(output)
}

async fn cleanup_guest_files(
    runtime: &DeviceRuntime,
    route: &WorkspaceRoute,
    sdk_root: &Path,
    serial: &str,
    staged: &[String],
    published: &[String],
) {
    if staged.is_empty() && published.is_empty() {
        return;
    }
    let mut args = vec![
        OsString::from("shell"),
        OsString::from("rm"),
        OsString::from("-f"),
    ];
    args.extend(staged.iter().chain(published).map(OsString::from));
    let _ = adb::run_cancellable(sdk_root, serial, args, PUSH_CONTROL_TIMEOUT, || {
        !runtime.route_is_current(route)
    })
    .await;
}

fn ensure_operation_current(
    runtime: &DeviceRuntime,
    route: &WorkspaceRoute,
    cancellation: &OperationCancellation,
) -> Result<(), OperationRunError> {
    if !runtime.route_is_current(route) {
        Err(OperationRunError::StaleRoute)
    } else if cancellation.is_canceled() {
        Err(OperationRunError::Canceled)
    } else {
        Ok(())
    }
}

fn classify_adb_failure(
    runtime: &DeviceRuntime,
    route: &WorkspaceRoute,
    cancellation: &OperationCancellation,
    error: String,
) -> OperationRunError {
    if !runtime.route_is_current(route) {
        OperationRunError::StaleRoute
    } else if cancellation.is_canceled() {
        OperationRunError::Canceled
    } else {
        OperationRunError::Failed(error)
    }
}

fn report_progress(
    sink: &Option<OperationProgressSink>,
    id: OperationId,
    route: &WorkspaceRoute,
    stage: OperationProgressStage,
    completed_items: usize,
    total_items: usize,
) {
    if let Some(sink) = sink {
        sink(OperationProgress {
            id,
            route: route.clone(),
            stage,
            completed_items,
            total_items,
        });
    }
}

pub async fn execute_stop(
    runtime: Arc<DeviceRuntime>,
    authorized: AuthorizedOperation,
    fallback_sdk_root: PathBuf,
) -> Result<OperationReport, OperationExecutionError> {
    require_kind(&authorized, OperationKind::Stop)?;
    let runner_runtime = runtime.clone();
    Ok(runtime
        .execute_operation_with(authorized, move |route| {
            let runtime = runner_runtime.clone();
            let fallback_sdk_root = fallback_sdk_root.clone();
            async move {
                let command = match runtime.begin_stop_route(&route) {
                    Ok(command) => command,
                    Err(RegistryError::StaleGeneration(_))
                    | Err(RegistryError::NoRunningSession(_)) => {
                        return Err(OperationRunError::StaleRoute);
                    }
                    Err(error) => return Err(OperationRunError::Failed(error.to_string())),
                };
                let result = match (command.launcher_pid(), command.sdk_root()) {
                    (Some(launcher_pid), Some(session_sdk)) => {
                        emulator::stop_managed(command.instance(), launcher_pid, session_sdk).await
                    }
                    _ => emulator::stop_instance(command.instance(), &fallback_sdk_root).await,
                };
                match result {
                    Ok(()) => runtime
                        .complete_stop(&command)
                        .map(|()| OperationSuccess::Stopped)
                        .map_err(|error| OperationRunError::Failed(error.to_string())),
                    Err(error) => {
                        let message = format!("{error:#}");
                        match runtime.fail_stop(&command, message.clone()) {
                            Ok(()) => Err(OperationRunError::Failed(message)),
                            Err(stale) => Err(OperationRunError::Failed(format!(
                                "{message}; 状态提交失败：{stale}"
                            ))),
                        }
                    }
                }
            }
        })
        .await)
}

fn require_kind(
    authorized: &AuthorizedOperation,
    expected: OperationKind,
) -> Result<(), OperationExecutionError> {
    let actual = authorized.plan().kind();
    if actual == expected {
        Ok(())
    } else {
        Err(OperationExecutionError::KindMismatch { expected, actual })
    }
}

async fn is_apk_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("apk"))
        && tokio::fs::symlink_metadata(path)
            .await
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn guest_artifact_name(path: &Path, operation_id: OperationId, index: usize) -> String {
    let mut stem: String = path
        .file_stem()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default()
        .chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() {
        stem.push_str("file");
    }
    let extension: String = path
        .extension()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default()
        .chars()
        .take(16)
        .filter(|character| character.is_ascii_alphanumeric())
        .collect();
    let suffix = if extension.is_empty() {
        String::new()
    } else {
        format!(".{extension}")
    };
    format!("{stem}-op{}-{index}{suffix}", operation_id.get())
}

fn safe_artifact_name(name: &str) -> String {
    let mut safe: String = name
        .chars()
        .take(80)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        safe.push_str("device");
    }
    safe
}

async fn publish_screenshot(
    path: &Path,
    bytes: &[u8],
    still_current: impl FnOnce() -> bool,
) -> Result<(), OperationRunError> {
    let temporary = path.with_extension("png.part");
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await
        .map_err(|error| {
            OperationRunError::Failed(format!(
                "创建截图临时文件失败 {}：{error}",
                temporary.display()
            ))
        })?;
    let write_result = async {
        file.write_all(bytes).await?;
        file.flush().await?;
        drop(file);
        if !still_current() {
            return Ok(false);
        }
        tokio::fs::hard_link(&temporary, path).await?;
        Ok::<bool, std::io::Error>(true)
    }
    .await;
    let _ = tokio::fs::remove_file(&temporary).await;
    match write_result {
        Ok(true) => Ok(()),
        Ok(false) => Err(OperationRunError::StaleRoute),
        Err(error) => Err(OperationRunError::Failed(format!(
            "提交截图文件失败 {}：{error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::core::emulator::RunningInstance;
    use crate::core::instance::DeviceRuntime;

    fn route(name: &str, session_id: u64) -> WorkspaceRoute {
        WorkspaceRoute {
            avd_name: name.into(),
            session_id,
            generation: 1,
        }
    }

    fn running(name: &str, port: u16, pid: u32) -> RunningInstance {
        RunningInstance {
            pid,
            ini_path: PathBuf::from(format!("/tmp/{name}-{pid}.ini")),
            avd_name: name.into(),
            console_port: port,
            adb_port: port + 1,
            grpc_port: port + 3000,
            grpc_allowlist: None,
            grpc_jwks: None,
            grpc_jwk_active: None,
        }
    }

    #[test]
    fn plans_are_deterministic_nonempty_and_have_unique_ids() {
        let coordinator = OperationCoordinator::default();
        assert_eq!(
            coordinator
                .plan(OperationKind::Screenshot, OperationScope::Focused, vec![])
                .unwrap_err(),
            OperationPlanError::EmptyTargets {
                scope: OperationScope::Focused
            }
        );
        let first = coordinator
            .plan(
                OperationKind::Screenshot,
                OperationScope::Selected,
                vec![route("b", 2), route("a", 1), route("b", 2)],
            )
            .unwrap();
        let second = coordinator
            .plan(
                OperationKind::Stop,
                OperationScope::AllRunning,
                vec![route("c", 3)],
            )
            .unwrap();
        assert_eq!(first.targets, vec![route("a", 1), route("b", 2)]);
        assert!(first.id.get() < second.id.get());
        assert!(!first.requires_confirmation());
        assert!(second.requires_confirmation());
        assert!(OperationKind::InstallApk.requires_confirmation());
    }

    #[test]
    fn authorization_rejects_replaced_session_without_changing_plan_targets() {
        let coordinator = OperationCoordinator::default();
        let plan = coordinator
            .plan(
                OperationKind::Stop,
                OperationScope::Selected,
                vec![route("a", 1), route("b", 2)],
            )
            .unwrap();
        assert_eq!(
            coordinator
                .authorize(plan, [route("a", 9), route("b", 2)])
                .unwrap_err(),
            OperationPlanError::TargetsChanged(vec![route("a", 1)])
        );
    }

    #[tokio::test]
    async fn execution_preserves_target_order_and_reports_partial_failure_and_stale() {
        let coordinator = OperationCoordinator::default();
        let plan = coordinator
            .plan(
                OperationKind::InstallApk,
                OperationScope::AllRunning,
                vec![route("c", 3), route("a", 1), route("b", 2)],
            )
            .unwrap();
        let authorized = coordinator
            .authorize(plan, [route("a", 1), route("b", 2), route("c", 3)])
            .unwrap();
        let called = Arc::new(Mutex::new(Vec::new()));
        let called_in_runner = called.clone();
        let report = coordinator
            .execute_with(
                authorized,
                |target| target.avd_name != "b",
                move |target| {
                    let called = called_in_runner.clone();
                    async move {
                        called.lock().unwrap().push(target.avd_name.clone());
                        if target.avd_name == "c" {
                            Err(OperationRunError::Failed("install failed".into()))
                        } else {
                            Ok(OperationSuccess::ApksInstalled {
                                files: 1,
                                exit_code: Some(0),
                            })
                        }
                    }
                },
            )
            .await;
        assert_eq!(&*called.lock().unwrap(), &["a", "c"]);
        assert_eq!(
            report
                .devices
                .iter()
                .map(|device| (&device.route.avd_name, &device.result))
                .collect::<Vec<_>>(),
            vec![
                (
                    &"a".to_owned(),
                    &OperationResult::Succeeded(OperationSuccess::ApksInstalled {
                        files: 1,
                        exit_code: Some(0),
                    })
                ),
                (&"b".to_owned(), &OperationResult::StaleRoute),
                (
                    &"c".to_owned(),
                    &OperationResult::Failed("install failed".into())
                )
            ]
        );
    }

    #[tokio::test]
    async fn device_keypress_rejects_unknown_exact_route() {
        let runtime = Arc::new(DeviceRuntime::default());
        assert_eq!(
            send_route_keypress(runtime, route("missing", 7), DeviceKey::VolumeUp).await,
            Err(OperationRunError::StaleRoute)
        );
    }

    #[tokio::test]
    async fn apk_executor_uses_exact_serial_and_keeps_partial_failure() {
        let root =
            std::env::temp_dir().join(format!("liteavd-operation-apk-{}", std::process::id()));
        let tools = root.join("platform-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let adb = tools.join("adb");
        std::fs::write(
            &adb,
            "#!/bin/sh\nif [ \"$2\" = \"emulator-5556\" ]; then echo rejected >&2; exit 17; fi\necho Success\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&adb).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&adb, permissions).unwrap();
        let apk = root.join("fixture.apk");
        std::fs::write(&apk, b"fixture").unwrap();

        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![
            running("phone", 5554, 1001),
            running("tablet", 5556, 1002),
        ]);
        let plan = runtime
            .plan_operation(OperationKind::InstallApk, OperationScope::AllRunning)
            .unwrap();
        let authorized = runtime.authorize_operation(plan).unwrap();
        let report = execute_install_apk(runtime, authorized, root.clone(), apk)
            .await
            .unwrap();

        assert_eq!(report.devices.len(), 2);
        assert_eq!(
            report.devices[0].result,
            OperationResult::Succeeded(OperationSuccess::ApksInstalled {
                files: 1,
                exit_code: Some(0),
            })
        );
        assert!(matches!(
            &report.devices[1].result,
            OperationResult::Failed(error) if error.contains("rejected")
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn three_device_install_keeps_success_failure_and_user_cancel_distinct() {
        let root = std::env::temp_dir().join(format!(
            "liteavd-operation-three-install-{}",
            std::process::id()
        ));
        let tools = root.join("platform-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let pid_file = root.join("adb.pid");
        let adb = tools.join("adb");
        std::fs::write(
            &adb,
            format!(
                "#!/bin/sh\nif [ \"$2\" = emulator-5556 ]; then echo rejected >&2; exit 17; fi\nif [ \"$2\" = emulator-5558 ]; then echo $$ > '{}'; exec sleep 30; fi\necho Success\n",
                pid_file.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&adb).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&adb, permissions).unwrap();
        let apk = root.join("fixture.apk");
        std::fs::write(&apk, b"fixture").unwrap();
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![
            running("alpha", 5554, 1001),
            running("beta", 5556, 1002),
            running("gamma", 5558, 1003),
        ]);
        let plan = runtime
            .plan_operation(OperationKind::InstallApk, OperationScope::AllRunning)
            .unwrap();
        let authorized = runtime.authorize_operation(plan).unwrap();
        let cancellation = OperationCancellation::default();
        let cancellation_for_task = cancellation.clone();
        let pid_for_cancel = pid_file.clone();
        let cancel = async move {
            while !pid_for_cancel.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancellation_for_task.cancel();
        };
        let execute = execute_install_apks(
            runtime,
            authorized,
            root.clone(),
            ApkInstallRequest {
                apks: vec![apk],
                options: adb::ApkInstallOptions::default(),
            },
            cancellation,
            None,
        );
        let (report, ()) = tokio::join!(execute, cancel);
        let report = report.unwrap();
        assert_eq!(
            report.devices[0].result,
            OperationResult::Succeeded(OperationSuccess::ApksInstalled {
                files: 1,
                exit_code: Some(0),
            })
        );
        assert!(matches!(
            &report.devices[1].result,
            OperationResult::Failed(error) if error.contains("rejected")
        ));
        assert_eq!(report.devices[2].result, OperationResult::Canceled);
        let pid: u32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn split_apk_executor_preserves_explicit_flags_and_reports_progress() {
        let root =
            std::env::temp_dir().join(format!("liteavd-operation-split-{}", std::process::id()));
        let tools = root.join("platform-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let args_file = root.join("args");
        let adb = tools.join("adb");
        std::fs::write(
            &adb,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                args_file.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&adb).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&adb, permissions).unwrap();
        let base = root.join("base.apk");
        let split = root.join("split_config.en.apk");
        std::fs::write(&base, b"base").unwrap();
        std::fs::write(&split, b"split").unwrap();

        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![running("phone", 5554, 1001)]);
        let plan = runtime
            .plan_operation(OperationKind::InstallApk, OperationScope::AllRunning)
            .unwrap();
        let authorized = runtime.authorize_operation(plan).unwrap();
        let progress = Arc::new(Mutex::new(Vec::new()));
        let progress_for_sink = progress.clone();
        let report = execute_install_apks(
            runtime,
            authorized,
            root.clone(),
            ApkInstallRequest {
                apks: vec![base.clone(), split.clone()],
                options: adb::ApkInstallOptions {
                    allow_downgrade: true,
                    grant_runtime_permissions: true,
                },
            },
            OperationCancellation::default(),
            Some(Arc::new(move |event| {
                progress_for_sink.lock().unwrap().push(event);
            })),
        )
        .await
        .unwrap();

        assert_eq!(
            report.devices[0].result,
            OperationResult::Succeeded(OperationSuccess::ApksInstalled {
                files: 2,
                exit_code: Some(0),
            })
        );
        assert_eq!(
            std::fs::read_to_string(args_file)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            vec![
                "-s",
                "emulator-5554",
                "install-multiple",
                "-r",
                "-t",
                "-d",
                "-g",
                base.to_str().unwrap(),
                split.to_str().unwrap(),
            ]
        );
        assert_eq!(
            progress
                .lock()
                .unwrap()
                .iter()
                .map(|event| event.stage)
                .collect::<Vec<_>>(),
            vec![
                OperationProgressStage::Starting,
                OperationProgressStage::Finished,
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn apk_and_push_validation_reject_symlinks_and_empty_requests() {
        let root = std::env::temp_dir().join(format!(
            "liteavd-operation-local-validation-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.apk");
        let link = root.join("link.apk");
        std::fs::write(&source, b"fixture").unwrap();
        std::os::unix::fs::symlink(&source, &link).unwrap();
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![running("phone", 5554, 1001)]);

        let plan = runtime
            .plan_operation(OperationKind::InstallApk, OperationScope::AllRunning)
            .unwrap();
        let authorized = runtime.authorize_operation(plan).unwrap();
        assert!(matches!(
            execute_install_apks(
                runtime.clone(),
                authorized,
                root.clone(),
                ApkInstallRequest {
                    apks: vec![link.clone()],
                    options: adb::ApkInstallOptions::default(),
                },
                OperationCancellation::default(),
                None,
            )
            .await,
            Err(OperationExecutionError::InvalidApk(path)) if path == link
        ));

        let plan = runtime
            .plan_operation(OperationKind::PushFiles, OperationScope::AllRunning)
            .unwrap();
        let authorized = runtime.authorize_operation(plan).unwrap();
        assert!(matches!(
            execute_push_files(
                runtime,
                authorized,
                root.clone(),
                PushFilesRequest { files: Vec::new() },
                OperationCancellation::default(),
                None,
            )
            .await,
            Err(OperationExecutionError::EmptyPushFiles)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn push_executor_uses_unique_staging_no_clobber_and_deterministic_results() {
        let root =
            std::env::temp_dir().join(format!("liteavd-operation-push-{}", std::process::id()));
        let tools = root.join("platform-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let calls_file = root.join("calls");
        let adb = tools.join("adb");
        std::fs::write(
            &adb,
            format!(
                "#!/bin/sh\nprintf 'CALL\\n' >> '{}'\nprintf '%s\\n' \"$@\" >> '{}'\n",
                calls_file.display(),
                calls_file.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&adb).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&adb, permissions).unwrap();
        let first = root.join("first.txt");
        let second = root.join("odd name.bin");
        std::fs::write(&first, b"abc").unwrap();
        std::fs::write(&second, b"wxyz").unwrap();

        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![running("phone", 5554, 1001)]);
        let plan = runtime
            .plan_operation(OperationKind::PushFiles, OperationScope::AllRunning)
            .unwrap();
        let operation_id = plan.id();
        let authorized = runtime.authorize_operation(plan).unwrap();
        let report = execute_push_files(
            runtime,
            authorized,
            root.clone(),
            PushFilesRequest {
                files: vec![first.clone(), second.clone()],
            },
            OperationCancellation::default(),
            None,
        )
        .await
        .unwrap();
        let expected_paths = vec![
            format!("{GUEST_PUSH_ROOT}/first-op{}-1.txt", operation_id.get()),
            format!("{GUEST_PUSH_ROOT}/odd_name-op{}-2.bin", operation_id.get()),
        ];
        assert_eq!(
            report.devices[0].result,
            OperationResult::Succeeded(OperationSuccess::FilesPushed {
                paths: expected_paths.clone(),
                bytes: 7,
                exit_code: Some(0),
            })
        );
        let calls = std::fs::read_to_string(calls_file).unwrap();
        assert!(calls.contains("emulator-5554\npush\n"));
        for path in expected_paths {
            assert!(calls.contains(&format!("test\n!\n-e\n{path}")));
            assert!(calls.contains(&format!("mv\n-n\n{path}.part\n{path}")));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn canceled_push_reaps_adb_and_cleans_partial_staging_file() {
        let root = std::env::temp_dir().join(format!(
            "liteavd-operation-push-cancel-{}",
            std::process::id()
        ));
        let tools = root.join("platform-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let calls_file = root.join("calls");
        let pid_file = root.join("adb.pid");
        let adb = tools.join("adb");
        std::fs::write(
            &adb,
            format!(
                "#!/bin/sh\nprintf 'CALL\\n' >> '{calls}'\nprintf '%s\\n' \"$@\" >> '{calls}'\nif [ \"$3\" = push ]; then echo $$ > '{pid}'; exec sleep 30; fi\n",
                calls = calls_file.display(),
                pid = pid_file.display(),
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&adb).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&adb, permissions).unwrap();
        let file = root.join("large.bin");
        std::fs::write(&file, b"fixture").unwrap();

        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![running("phone", 5554, 1001)]);
        let plan = runtime
            .plan_operation(OperationKind::PushFiles, OperationScope::AllRunning)
            .unwrap();
        let operation_id = plan.id();
        let authorized = runtime.authorize_operation(plan).unwrap();
        let cancellation = OperationCancellation::default();
        let cancellation_for_task = cancellation.clone();
        let pid_for_task = pid_file.clone();
        let cancel = async move {
            while !pid_for_task.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancellation_for_task.cancel();
        };
        let execute = execute_push_files(
            runtime,
            authorized,
            root.clone(),
            PushFilesRequest { files: vec![file] },
            cancellation,
            None,
        );
        let (report, ()) = tokio::join!(execute, cancel);
        let report = report.unwrap();
        assert_eq!(report.devices[0].result, OperationResult::Canceled);
        let pid: u32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        let expected_part = format!(
            "{GUEST_PUSH_ROOT}/large-op{}-1.bin.part",
            operation_id.get()
        );
        let calls = std::fs::read_to_string(calls_file).unwrap();
        assert!(calls.contains(&format!("shell\nrm\n-f\n{expected_part}")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn push_refuses_existing_destination_before_transfer() {
        let root = std::env::temp_dir().join(format!(
            "liteavd-operation-push-existing-{}",
            std::process::id()
        ));
        let tools = root.join("platform-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let calls_file = root.join("calls");
        let adb = tools.join("adb");
        std::fs::write(
            &adb,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{calls}'\nif [ \"$3\" = shell ] && [ \"$4\" = test ] && [ \"$5\" = '!' ]; then echo exists >&2; exit 1; fi\n",
                calls = calls_file.display(),
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&adb).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&adb, permissions).unwrap();
        let file = root.join("fixture.bin");
        std::fs::write(&file, b"fixture").unwrap();
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![running("phone", 5554, 1001)]);
        let plan = runtime
            .plan_operation(OperationKind::PushFiles, OperationScope::AllRunning)
            .unwrap();
        let authorized = runtime.authorize_operation(plan).unwrap();
        let report = execute_push_files(
            runtime,
            authorized,
            root.clone(),
            PushFilesRequest { files: vec![file] },
            OperationCancellation::default(),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            &report.devices[0].result,
            OperationResult::Failed(error) if error.contains("已存在，未覆盖")
        ));
        assert!(
            !std::fs::read_to_string(calls_file)
                .unwrap()
                .lines()
                .any(|argument| argument == "push")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn route_loss_cancels_and_reaps_push_without_contacting_replacement() {
        let root = std::env::temp_dir().join(format!(
            "liteavd-operation-push-stale-{}",
            std::process::id()
        ));
        let tools = root.join("platform-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let calls_file = root.join("calls");
        let pid_file = root.join("adb.pid");
        let adb = tools.join("adb");
        std::fs::write(
            &adb,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{calls}'\nif [ \"$3\" = push ]; then echo $$ > '{pid}'; exec sleep 30; fi\n",
                calls = calls_file.display(),
                pid = pid_file.display(),
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&adb).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&adb, permissions).unwrap();
        let file = root.join("fixture.bin");
        std::fs::write(&file, b"fixture").unwrap();
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![running("phone", 5554, 1001)]);
        let plan = runtime
            .plan_operation(OperationKind::PushFiles, OperationScope::AllRunning)
            .unwrap();
        let authorized = runtime.authorize_operation(plan).unwrap();
        let runtime_for_replace = runtime.clone();
        let pid_for_replace = pid_file.clone();
        let replace = async move {
            while !pid_for_replace.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            runtime_for_replace.reconcile_running(vec![running("phone", 5554, 2002)]);
        };
        let execute = execute_push_files(
            runtime,
            authorized,
            root.clone(),
            PushFilesRequest { files: vec![file] },
            OperationCancellation::default(),
            None,
        );
        let (report, ()) = tokio::join!(execute, replace);
        let report = report.unwrap();
        assert_eq!(report.devices[0].result, OperationResult::StaleRoute);
        let pid: u32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert!(
            !std::fs::read_to_string(calls_file)
                .unwrap()
                .lines()
                .any(|argument| argument == "rm")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn artifact_names_never_create_paths() {
        assert_eq!(safe_artifact_name("../Pixel 2/测试"), "___Pixel_2___");
        assert_eq!(safe_artifact_name(""), "device");
        assert_eq!(
            guest_artifact_name(Path::new("../测试 apk"), OperationId(9), 2),
            "___apk-op9-2"
        );
        assert_eq!(
            guest_artifact_name(Path::new("report.final.PDF"), OperationId(4), 1),
            "report_final-op4-1.PDF"
        );
    }

    #[tokio::test]
    async fn screenshot_publish_is_no_clobber_and_cleans_stale_part() {
        let root =
            std::env::temp_dir().join(format!("liteavd-operation-shot-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("phone-op1.png");
        publish_screenshot(&path, b"png-one", || true)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"png-one");
        assert!(matches!(
            publish_screenshot(&path, b"replacement", || true).await,
            Err(OperationRunError::Failed(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"png-one");

        let stale = root.join("tablet-op1.png");
        assert_eq!(
            publish_screenshot(&stale, b"stale", || false).await,
            Err(OperationRunError::StaleRoute)
        );
        assert!(!stale.exists());
        assert!(!stale.with_extension("png.part").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
