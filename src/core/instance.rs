//! 模拟器 session 生命周期与应用级运行状态。
//!
//! `InstanceRegistry` 是设备状态、运行实例和资源所有权的唯一写入边界。
//! UI 只读取 `DeviceProjection`，异步任务用 generation ticket 提交结果。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use thiserror::Error;

use crate::core::avd::ManagedGpuPolicy;
use crate::core::device_state::{DevicePhase, DeviceState, DeviceStateStore, RecoveryReason};
use crate::core::emulator::{self, LaunchedInstance, RunningInstance, SessionResources};
use crate::core::grpc::GrpcClient;
use crate::core::grpc_auth::GrpcJwtAuth;
use crate::core::microphone::{MicrophoneEndpointDescriptor, PulseMicrophoneEndpoint};
use crate::core::operation::{
    AuthorizedOperation, OperationCoordinator, OperationKind, OperationPlan, OperationPlanError,
    OperationReport, OperationRunError, OperationSuccess,
};
use crate::core::scheduler::{
    LaunchPermit, LaunchScheduler, PortAllocator, PortReservation, PortReservationError,
    QueueStatus, ResourceDemand, ResourceReservation, SchedulerConfig, SchedulerError, StartTicket,
};
use crate::core::settings::{AppLogLevel, emit};
use crate::core::stream::{CaptureHandle, CaptureSubscription};
use crate::core::workspace::{
    OperationScope, WorkspaceError, WorkspaceIntent, WorkspaceRoute, WorkspaceSnapshot,
    WorkspaceState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(u64);

impl SessionId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOrigin {
    Managed,
    Recovered,
    Adopted,
}

/// 运行 session 的只读投影，可安全发送给 UI。
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub generation: u64,
    pub origin: SessionOrigin,
    pub instance: RunningInstance,
    pub log_path: Option<PathBuf>,
}

/// 持有运行实例及其资源，drop 时自动归还 managed session 的端口 reservation。
#[derive(Debug)]
pub struct EmulatorSession {
    id: SessionId,
    generation: u64,
    origin: SessionOrigin,
    instance: RunningInstance,
    resources: Option<SessionResources>,
    _console_reservation: Option<PortReservation>,
    _resource_reservation: Option<ResourceReservation>,
}

impl EmulatorSession {
    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            id: self.id,
            generation: self.generation,
            origin: self.origin,
            instance: self.instance.clone(),
            log_path: self
                .resources
                .as_ref()
                .and_then(|resources| resources.process.as_ref())
                .map(|process| process.log_path().to_path_buf()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceProjection {
    pub state: DeviceState,
    pub session: Option<SessionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartCommand {
    avd_name: String,
    generation: u64,
}

impl StartCommand {
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// 输入 worker 持有的弱路由；每个事件发送前复验 session id + generation。
#[derive(Clone)]
pub struct InputRouteGuard {
    runtime: Weak<DeviceRuntime>,
    route: WorkspaceRoute,
}

impl std::fmt::Debug for InputRouteGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputRouteGuard")
            .field("route", &self.route)
            .finish_non_exhaustive()
    }
}

impl InputRouteGuard {
    pub fn route(&self) -> &WorkspaceRoute {
        &self.route
    }

    pub fn is_current(&self) -> bool {
        self.runtime.upgrade().is_some_and(|runtime| {
            runtime
                .registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .route_is_current(&self.route)
        })
    }

    pub fn focus(&self) -> bool {
        let Some(runtime) = self.runtime.upgrade() else {
            return false;
        };
        if !runtime
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .route_is_current(&self.route)
        {
            return false;
        }
        runtime
            .workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .focus(&self.route)
            .is_ok()
    }

    pub fn report_control_disconnected(&self) {
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.report_control_disconnected(&self.route);
        }
    }

    pub fn report_control_connected(&self) {
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.report_control_connected(&self.route);
        }
    }
}

#[derive(Debug, Clone)]
pub struct StopCommand {
    avd_name: String,
    generation: u64,
    session_id: SessionId,
    instance: RunningInstance,
    launcher_pid: Option<u32>,
    sdk_root: Option<PathBuf>,
    log_path: Option<PathBuf>,
}

impl StopCommand {
    pub fn instance(&self) -> &RunningInstance {
        &self.instance
    }

    pub fn launcher_pid(&self) -> Option<u32> {
        self.launcher_pid
    }

    pub fn sdk_root(&self) -> Option<&Path> {
        self.sdk_root.as_deref()
    }

    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RegistryError {
    #[error("设备 {0} 已有活动中的会话或命令")]
    AlreadyActive(String),
    #[error("设备 {0} 没有可停止的运行会话")]
    NoRunningSession(String),
    #[error("设备 {0} 的异步结果已过期")]
    StaleGeneration(String),
    #[error("启动实例与 reservation 不匹配：{0}")]
    InstanceMismatch(String),
    #[error("设备 {0} 没有可取消的排队任务")]
    NotQueued(String),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StartScheduleError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
}

#[derive(Debug, Default)]
pub struct InstanceRegistry {
    states: DeviceStateStore,
    sessions: HashMap<SessionId, EmulatorSession>,
    by_avd: HashMap<String, SessionId>,
    by_port: HashMap<u16, SessionId>,
    next_session_id: u64,
}

impl InstanceRegistry {
    pub fn begin_start(&mut self, avd_name: &str) -> Result<StartCommand, RegistryError> {
        if self.by_avd.contains_key(avd_name) {
            return Err(RegistryError::AlreadyActive(avd_name.to_owned()));
        }
        let generation = self
            .states
            .begin_start(avd_name)
            .map_err(|_| RegistryError::AlreadyActive(avd_name.to_owned()))?;
        Ok(StartCommand {
            avd_name: avd_name.to_owned(),
            generation,
        })
    }

    pub fn attach_start_port(
        &mut self,
        command: &StartCommand,
        port: u16,
    ) -> Result<(), RegistryError> {
        if self
            .states
            .attach_port(&command.avd_name, command.generation, port)
        {
            Ok(())
        } else {
            Err(RegistryError::StaleGeneration(command.avd_name.clone()))
        }
    }

    pub fn mark_queued(
        &mut self,
        command: &StartCommand,
        reason: String,
    ) -> Result<(), RegistryError> {
        if self.states.update(
            &command.avd_name,
            command.generation,
            DevicePhase::Queued(reason),
        ) {
            Ok(())
        } else {
            Err(RegistryError::StaleGeneration(command.avd_name.clone()))
        }
    }

    pub fn mark_starting(&mut self, command: &StartCommand) -> Result<(), RegistryError> {
        if self
            .states
            .update(&command.avd_name, command.generation, DevicePhase::Starting)
        {
            Ok(())
        } else {
            Err(RegistryError::StaleGeneration(command.avd_name.clone()))
        }
    }

    pub fn cancel_queued(&mut self, avd_name: &str) -> Result<(), RegistryError> {
        if self.states.cancel_queued(avd_name) {
            Ok(())
        } else {
            Err(RegistryError::NotQueued(avd_name.to_owned()))
        }
    }

    pub fn mark_booting(&mut self, command: &StartCommand) -> Result<(), RegistryError> {
        if self
            .states
            .update(&command.avd_name, command.generation, DevicePhase::Booting)
        {
            Ok(())
        } else {
            Err(RegistryError::StaleGeneration(command.avd_name.clone()))
        }
    }

    pub fn complete_start(
        &mut self,
        command: &StartCommand,
        launched: LaunchedInstance,
        reservation: PortReservation,
    ) -> Result<SessionSnapshot, RegistryError> {
        self.complete_start_with_resources(command, launched, reservation, None)
    }

    fn complete_start_with_resources(
        &mut self,
        command: &StartCommand,
        launched: LaunchedInstance,
        reservation: PortReservation,
        resource_reservation: Option<ResourceReservation>,
    ) -> Result<SessionSnapshot, RegistryError> {
        let state = self
            .states
            .get(&command.avd_name)
            .filter(|state| state.generation == command.generation)
            .ok_or_else(|| RegistryError::StaleGeneration(command.avd_name.clone()))?;
        if launched.instance.avd_name != command.avd_name
            || launched.instance.console_port != reservation.port()
            || state.console_port != Some(reservation.port())
        {
            return Err(RegistryError::InstanceMismatch(command.avd_name.clone()));
        }
        if self.by_avd.contains_key(&command.avd_name)
            || self.by_port.contains_key(&launched.instance.console_port)
        {
            return Err(RegistryError::AlreadyActive(command.avd_name.clone()));
        }

        if !self
            .states
            .update(&command.avd_name, command.generation, DevicePhase::Running)
        {
            return Err(RegistryError::StaleGeneration(command.avd_name.clone()));
        }
        let (instance, resources) = launched.into_parts();
        let id = self.new_session_id();
        let session = EmulatorSession {
            id,
            generation: command.generation,
            origin: SessionOrigin::Managed,
            instance,
            resources,
            _console_reservation: Some(reservation),
            _resource_reservation: resource_reservation,
        };
        let snapshot = session.snapshot();
        self.by_avd.insert(command.avd_name.clone(), id);
        self.by_port
            .insert(snapshot.instance.console_port, session.id);
        self.sessions.insert(id, session);
        Ok(snapshot)
    }

    pub fn fail_start(&mut self, command: &StartCommand, error: String) -> bool {
        let changed = self.states.update(
            &command.avd_name,
            command.generation,
            DevicePhase::Error(error),
        );
        if changed {
            self.states
                .clear_port(&command.avd_name, command.generation);
        }
        changed
    }

    pub fn begin_stop(&mut self, avd_name: &str) -> Result<StopCommand, RegistryError> {
        let session_id = *self
            .by_avd
            .get(avd_name)
            .ok_or_else(|| RegistryError::NoRunningSession(avd_name.to_owned()))?;
        let instance = self
            .sessions
            .get(&session_id)
            .expect("by_avd 必须指向有效 session")
            .instance
            .clone();
        let process = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.resources.as_ref())
            .and_then(|resources| resources.process.as_ref());
        let generation = self.states.begin_stop(avd_name, instance.console_port);
        Ok(StopCommand {
            avd_name: avd_name.to_owned(),
            generation,
            session_id,
            instance,
            launcher_pid: process.map(|process| process.launcher_pid()),
            sdk_root: process.map(|process| process.sdk_root().to_path_buf()),
            log_path: process.map(|process| process.log_path().to_path_buf()),
        })
    }

    pub fn complete_stop(&mut self, command: &StopCommand) -> Result<(), RegistryError> {
        self.verify_stop_command(command)?;
        let session = self
            .sessions
            .remove(&command.session_id)
            .expect("已验证的 session 必须存在");
        self.by_avd.remove(&command.avd_name);
        self.by_port.remove(&session.instance.console_port);
        if self
            .states
            .update(&command.avd_name, command.generation, DevicePhase::Stopped)
        {
            Ok(())
        } else {
            Err(RegistryError::StaleGeneration(command.avd_name.clone()))
        }
    }

    pub fn fail_stop(&mut self, command: &StopCommand, error: String) -> Result<(), RegistryError> {
        self.verify_stop_command(command)?;
        if !self.states.update(
            &command.avd_name,
            command.generation,
            DevicePhase::Error(error),
        ) {
            return Err(RegistryError::StaleGeneration(command.avd_name.clone()));
        }
        self.states.attach_port(
            &command.avd_name,
            command.generation,
            command.instance.console_port,
        );
        Ok(())
    }

    /// 合并一次广告文件全量扫描：收养新实例，刷新已知实例，回收已消失会话。
    pub fn reconcile_running(&mut self, observed: Vec<RunningInstance>) {
        self.reconcile_running_with_probe(observed, |_| false, |_| None);
    }

    fn reconcile_running_with_probe(
        &mut self,
        mut observed: Vec<RunningInstance>,
        process_alive: impl Fn(&RunningInstance) -> bool,
        mut recover_resources: impl FnMut(&RunningInstance) -> Option<SessionResources>,
    ) {
        observed.sort_by_key(|instance| instance.console_port);
        let observed_identities: HashSet<_> = observed
            .iter()
            .map(|instance| {
                (
                    instance.console_port,
                    instance.pid,
                    instance.avd_name.clone(),
                )
            })
            .collect();

        // console port 可被后续进程复用，不能把旧 session 的 auth/process/capture
        // 资源转移给新 PID。先按完整身份回收，再处理本次 observed 的新收养。
        let missing: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(id, session)| {
                let identity = (
                    session.instance.console_port,
                    session.instance.pid,
                    session.instance.avd_name.clone(),
                );
                (!observed_identities.contains(&identity)).then_some(*id)
            })
            .collect();
        for id in missing {
            let Some(session) = self.sessions.get(&id) else {
                continue;
            };
            let phase = self
                .states
                .get(&session.instance.avd_name)
                .map(|state| state.phase.clone());
            if process_alive(&session.instance) {
                if matches!(
                    phase,
                    Some(DevicePhase::Running | DevicePhase::Recovering(_))
                ) {
                    self.states.update(
                        &session.instance.avd_name,
                        session.generation,
                        DevicePhase::Recovering(RecoveryReason::AdvertisementMissing),
                    );
                }
                continue;
            }

            if let Some(session) = self.sessions.remove(&id) {
                self.by_avd.remove(&session.instance.avd_name);
                self.by_port.remove(&session.instance.console_port);
                if matches!(phase, Some(DevicePhase::Stopping))
                    || session.origin == SessionOrigin::Adopted
                {
                    self.states.force_stopped(&session.instance.avd_name);
                } else {
                    self.states
                        .force_error(&session.instance.avd_name, "模拟器进程意外退出".to_owned());
                }
            }
        }

        for instance in observed {
            if let Some(session_id) = self.by_port.get(&instance.console_port).copied() {
                if let Some(session) = self.sessions.get_mut(&session_id)
                    && session.instance.pid == instance.pid
                    && session.instance.avd_name == instance.avd_name
                {
                    session.instance = instance;
                    if session.origin == SessionOrigin::Adopted
                        && let Some(resources) = recover_resources(&session.instance)
                    {
                        session.origin = SessionOrigin::Recovered;
                        session.resources = Some(resources);
                    }
                    if matches!(
                        self.states
                            .get(&session.instance.avd_name)
                            .map(|state| &state.phase),
                        Some(DevicePhase::Recovering(
                            RecoveryReason::AdvertisementMissing
                        ))
                    ) {
                        self.states.update(
                            &session.instance.avd_name,
                            session.generation,
                            DevicePhase::Running,
                        );
                    }
                }
                continue;
            }
            if self.by_avd.contains_key(&instance.avd_name) {
                // 同名外部重复实例不自动替换现有 session，避免 UI 绑定到错误设备。
                continue;
            }
            if self.states.get(&instance.avd_name).is_some_and(|state| {
                matches!(
                    state.phase,
                    DevicePhase::Queued(_) | DevicePhase::Starting | DevicePhase::Booting
                )
            }) {
                // 广告文件可能先于启动 future 的 complete_start 被刷新看到；本地 command
                // 对该 AVD 拥有优先权，不能把同一实例抢先收养为 external session。
                continue;
            }

            let state = self
                .states
                .force_running(&instance.avd_name, instance.console_port);
            let id = self.new_session_id();
            let avd_name = instance.avd_name.clone();
            let port = instance.console_port;
            let resources = recover_resources(&instance);
            let origin = if resources.is_some() {
                SessionOrigin::Recovered
            } else {
                SessionOrigin::Adopted
            };
            self.sessions.insert(
                id,
                EmulatorSession {
                    id,
                    generation: state.generation,
                    origin,
                    instance,
                    resources,
                    _console_reservation: None,
                    _resource_reservation: None,
                },
            );
            self.by_avd.insert(avd_name, id);
            self.by_port.insert(port, id);
        }
    }

    pub fn projection(&mut self, avd_name: &str) -> DeviceProjection {
        let session = self
            .by_avd
            .get(avd_name)
            .and_then(|id| self.sessions.get(id))
            .map(EmulatorSession::snapshot);
        let state = self
            .states
            .reconcile_scan(avd_name, session.as_ref().map(|s| s.instance.console_port));
        DeviceProjection { state, session }
    }

    fn occupied_ports(&self) -> Vec<u16> {
        self.by_port.keys().copied().collect()
    }

    fn adopted_avds(&self) -> Vec<String> {
        self.sessions
            .values()
            .filter(|session| session.origin != SessionOrigin::Managed)
            .map(|session| session.instance.avd_name.clone())
            .collect()
    }

    fn workspace_routes(&self) -> Vec<WorkspaceRoute> {
        self.sessions
            .values()
            .map(|session| WorkspaceRoute {
                avd_name: session.instance.avd_name.clone(),
                session_id: session.id.get(),
                generation: session.generation,
            })
            .collect()
    }

    fn route_for_avd(&self, avd_name: &str) -> Option<WorkspaceRoute> {
        self.by_avd
            .get(avd_name)
            .and_then(|id| self.sessions.get(id))
            .map(|session| WorkspaceRoute {
                avd_name: session.instance.avd_name.clone(),
                session_id: session.id.get(),
                generation: session.generation,
            })
    }

    fn route_is_current(&self, route: &WorkspaceRoute) -> bool {
        if self
            .states
            .get(&route.avd_name)
            .is_some_and(|state| matches!(state.phase, DevicePhase::Stopping))
        {
            return false;
        }
        self.route_for_avd(&route.avd_name).as_ref() == Some(route)
    }

    fn session_for_route(&self, route: &WorkspaceRoute) -> Option<&EmulatorSession> {
        self.by_avd
            .get(&route.avd_name)
            .and_then(|id| self.sessions.get(id))
            .filter(|session| {
                session.id.get() == route.session_id && session.generation == route.generation
            })
    }

    fn report_control_health(&mut self, route: &WorkspaceRoute, connected: bool) -> bool {
        let Some(session) = self.session_for_route(route) else {
            return false;
        };
        let generation = session.generation;
        let phase = self
            .states
            .get(&route.avd_name)
            .map(|state| state.phase.clone());
        let next = match (connected, phase) {
            (false, Some(DevicePhase::Running)) => {
                Some(DevicePhase::Recovering(RecoveryReason::ControlDisconnected))
            }
            (true, Some(DevicePhase::Recovering(RecoveryReason::ControlDisconnected))) => {
                Some(DevicePhase::Running)
            }
            _ => None,
        };
        next.is_some_and(|phase| self.states.update(&route.avd_name, generation, phase))
    }

    fn grpc_client(&self, avd_name: &str) -> Option<GrpcClient> {
        self.by_avd
            .get(avd_name)
            .and_then(|id| self.sessions.get(id))
            .and_then(|session| session.resources.as_ref())
            .and_then(|resources| resources.grpc_client.clone())
    }

    fn capture_subscription(&self, avd_name: &str) -> Option<CaptureSubscription> {
        self.by_avd
            .get(avd_name)
            .and_then(|id| self.sessions.get(id))
            .and_then(|session| session.resources.as_ref())
            .and_then(|resources| resources.capture.as_ref())
            .map(|capture| capture.subscribe())
    }

    fn verify_stop_command(&self, command: &StopCommand) -> Result<(), RegistryError> {
        let current = self
            .states
            .get(&command.avd_name)
            .filter(|state| state.generation == command.generation);
        let session_matches = self.by_avd.get(&command.avd_name) == Some(&command.session_id)
            && self.sessions.contains_key(&command.session_id);
        if current.is_none() || !session_matches {
            Err(RegistryError::StaleGeneration(command.avd_name.clone()))
        } else {
            Ok(())
        }
    }

    fn new_session_id(&mut self) -> SessionId {
        self.next_session_id = self.next_session_id.wrapping_add(1).max(1);
        SessionId(self.next_session_id)
    }
}

impl Drop for InstanceRegistry {
    fn drop(&mut self) {
        // GUI 退出不是 stop。仍在 registry 中的 liteavd session 保留恢复身份；
        // 显式 stop/crash 会先移除 session，因此继续执行正常密钥清理。
        for session in self.sessions.values() {
            if matches!(
                session.origin,
                SessionOrigin::Managed | SessionOrigin::Recovered
            ) && let Some(resources) = session.resources.as_ref()
            {
                if let Some(microphone) = resources.microphone.as_ref() {
                    microphone.preserve_recovery_on_drop();
                }
                resources.grpc_auth.preserve_recovery_on_drop();
            }
        }
    }
}

/// 应用级共享状态。GTK 窗口持有一个 `DeviceRuntime`，后台任务只通过其命令 API
/// 修改 registry，不直接持有 UI 对象。
#[derive(Debug)]
pub struct DeviceRuntime {
    registry: Mutex<InstanceRegistry>,
    ports: PortAllocator,
    scheduler: LaunchScheduler,
    workspace: Mutex<WorkspaceState>,
    operations: OperationCoordinator,
    managed_gpu_policy: RwLock<ManagedGpuPolicy>,
    projection_revision: AtomicU64,
    control_stream_revision: AtomicU64,
}

impl Default for DeviceRuntime {
    fn default() -> Self {
        Self::with_runtime_policy(
            SchedulerConfig::default(),
            ManagedGpuPolicy::HeadlessSwangle,
        )
        .expect("default runtime policy must be valid")
    }
}

impl DeviceRuntime {
    pub fn with_scheduler_config(config: SchedulerConfig) -> Result<Self, SchedulerError> {
        Self::with_runtime_policy(config, ManagedGpuPolicy::HeadlessSwangle)
    }

    pub fn with_runtime_policy(
        config: SchedulerConfig,
        managed_gpu_policy: ManagedGpuPolicy,
    ) -> Result<Self, SchedulerError> {
        Ok(Self {
            registry: Mutex::new(InstanceRegistry::default()),
            ports: PortAllocator::default(),
            scheduler: LaunchScheduler::new(config)?,
            workspace: Mutex::new(WorkspaceState::default()),
            operations: OperationCoordinator::default(),
            managed_gpu_policy: RwLock::new(managed_gpu_policy),
            projection_revision: AtomicU64::new(0),
            control_stream_revision: AtomicU64::new(0),
        })
    }

    pub fn managed_gpu_policy(&self) -> ManagedGpuPolicy {
        *self
            .managed_gpu_policy
            .read()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Apply the policy to future starts only while the scheduler is idle.
    /// A queued start has already reserved demand from the previous policy.
    pub fn try_update_managed_gpu_policy(&self, policy: ManagedGpuPolicy) -> bool {
        if !self.scheduler.is_idle() {
            return false;
        }
        *self
            .managed_gpu_policy
            .write()
            .unwrap_or_else(|error| error.into_inner()) = policy;
        true
    }

    pub fn reserve_port<I>(&self, occupied: I) -> Result<PortReservation, PortReservationError>
    where
        I: IntoIterator<Item = u16>,
    {
        let mut occupied: HashSet<_> = occupied.into_iter().collect();
        occupied.extend(
            self.registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .occupied_ports(),
        );
        self.ports.reserve(occupied)
    }

    pub fn begin_start(&self, avd_name: &str) -> Result<StartCommand, RegistryError> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin_start(avd_name)
    }

    /// 把 start 原子投影为 Queued；ticket drop/cancel 会释放 FIFO 项。
    pub fn schedule_start(
        &self,
        avd_name: &str,
        demand: ResourceDemand,
    ) -> Result<(StartCommand, StartTicket, QueueStatus), StartScheduleError> {
        let command = self.begin_start(avd_name)?;
        let ticket = match self.scheduler.enqueue(avd_name, demand) {
            Ok(ticket) => ticket,
            Err(error) => {
                self.fail_start(&command, error.to_string());
                return Err(error.into());
            }
        };
        let status = ticket.status()?;
        if let Err(error) = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark_queued(&command, status.message())
        {
            drop(ticket);
            return Err(error.into());
        }
        Ok((command, ticket, status))
    }

    pub fn mark_starting(&self, command: &StartCommand) -> Result<(), RegistryError> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark_starting(command)
    }

    pub fn update_queue_status(
        &self,
        command: &StartCommand,
        reason: String,
    ) -> Result<(), RegistryError> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark_queued(command, reason)
    }

    /// 只取消尚未取得 launch permit 的任务；返回 `false` 表示已进入启动阶段。
    pub fn cancel_queued_start(&self, avd_name: &str) -> bool {
        if !self.scheduler.cancel(avd_name) {
            return false;
        }
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancel_queued(avd_name)
            .is_ok()
    }

    pub fn attach_start_port(
        &self,
        command: &StartCommand,
        port: u16,
    ) -> Result<(), RegistryError> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .attach_start_port(command, port)
    }

    pub fn mark_booting(&self, command: &StartCommand) -> Result<(), RegistryError> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark_booting(command)
    }

    pub fn complete_start(
        &self,
        command: &StartCommand,
        launched: LaunchedInstance,
        reservation: PortReservation,
    ) -> Result<SessionSnapshot, RegistryError> {
        let result = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .complete_start(command, launched, reservation);
        if result.is_ok() {
            self.sync_workspace();
        }
        result
    }

    pub fn complete_scheduled_start(
        &self,
        command: &StartCommand,
        launched: LaunchedInstance,
        reservation: PortReservation,
        permit: LaunchPermit,
    ) -> Result<SessionSnapshot, RegistryError> {
        let resource_reservation = permit.into_reservation();
        let result = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .complete_start_with_resources(
                command,
                launched,
                reservation,
                Some(resource_reservation),
            );
        if result.is_ok() {
            self.sync_workspace();
        }
        result
    }

    pub fn fail_start(&self, command: &StartCommand, error: String) -> bool {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fail_start(command, error)
    }

    pub fn begin_stop(&self, avd_name: &str) -> Result<StopCommand, RegistryError> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin_stop(avd_name)
    }

    pub fn complete_stop(&self, command: &StopCommand) -> Result<(), RegistryError> {
        let result = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .complete_stop(command);
        if result.is_ok() {
            self.scheduler.remove_external(&command.avd_name);
            self.sync_workspace();
        }
        result
    }

    pub fn fail_stop(&self, command: &StopCommand, error: String) -> Result<(), RegistryError> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fail_stop(command, error)
    }

    pub fn reconcile_running(&self, observed: Vec<RunningInstance>) {
        self.reconcile_running_with_demands(observed, HashMap::new());
    }

    /// 合并实例事实，并以 adopted session 重建应用重启后的资源占用。
    pub fn reconcile_running_with_demands(
        &self,
        observed: Vec<RunningInstance>,
        demands: HashMap<String, ResourceDemand>,
    ) {
        self.reconcile_running_with_probe(observed, demands, |_| false, |_| None);
    }

    /// 广告文件可能先于 engine 消失；调用方提供当前 SDK 后，只有进程身份也失效
    /// 才回收 session。广告暂时缺失但 PID 仍属于该 SDK 时进入 Recovering。
    pub fn reconcile_running_for_sdk_with_demands(
        &self,
        observed: Vec<RunningInstance>,
        demands: HashMap<String, ResourceDemand>,
        sdk_root: &Path,
    ) {
        self.reconcile_running_with_probe(
            observed,
            demands,
            |instance| emulator::verify_emulator_pid(instance.pid, sdk_root),
            recover_session_resources,
        );
    }

    fn reconcile_running_with_probe(
        &self,
        observed: Vec<RunningInstance>,
        demands: HashMap<String, ResourceDemand>,
        process_alive: impl Fn(&RunningInstance) -> bool,
        recover_resources: impl FnMut(&RunningInstance) -> Option<SessionResources>,
    ) {
        let (adopted, routes) = {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            registry.reconcile_running_with_probe(observed, process_alive, recover_resources);
            (registry.adopted_avds(), registry.workspace_routes())
        };
        self.scheduler
            .reconcile_external(adopted.into_iter().map(|name| {
                (
                    name.clone(),
                    demands.get(&name).copied().unwrap_or_default(),
                )
            }));
        self.workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reconcile(routes);
    }

    pub fn projection(&self, avd_name: &str) -> DeviceProjection {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .projection(avd_name)
    }

    /// 返回 managed session 已认证的共享 gRPC client；adopted session 默认不可控。
    pub fn grpc_client(&self, avd_name: &str) -> Option<GrpcClient> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .grpc_client(avd_name)
    }

    /// 返回 managed session 的 latest-frame 订阅；未启用 share-vid 时为 `None`。
    pub fn capture_subscription(&self, avd_name: &str) -> Option<CaptureSubscription> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .capture_subscription(avd_name)
    }

    pub fn input_route(self: &Arc<Self>, avd_name: &str) -> Option<InputRouteGuard> {
        let route = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .route_for_avd(avd_name)?;
        Some(InputRouteGuard {
            runtime: Arc::downgrade(self),
            route,
        })
    }

    pub fn route_is_current(&self, route: &WorkspaceRoute) -> bool {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .route_is_current(route)
    }

    pub fn report_control_disconnected(&self, route: &WorkspaceRoute) -> bool {
        let changed = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .report_control_health(route, false);
        if changed {
            self.projection_revision.fetch_add(1, Ordering::Release);
            self.control_stream_revision.fetch_add(1, Ordering::AcqRel);
        }
        changed
    }

    pub fn report_control_connected(&self, route: &WorkspaceRoute) -> bool {
        let changed = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .report_control_health(route, true);
        if changed {
            self.projection_revision.fetch_add(1, Ordering::Release);
        }
        changed
    }

    pub fn projection_revision(&self) -> u64 {
        self.projection_revision.load(Ordering::Acquire)
    }

    /// 长存控制流需要在不改变 session identity 的操作（例如 snapshot load）后
    /// 主动重建。revision 只发信号，不替代每次 I/O 前的 exact-route 复验。
    pub fn control_stream_revision(&self) -> u64 {
        self.control_stream_revision.load(Ordering::Acquire)
    }

    pub fn request_control_stream_reset(&self, route: &WorkspaceRoute) -> bool {
        if !self.route_is_current(route) {
            return false;
        }
        self.control_stream_revision.fetch_add(1, Ordering::AcqRel);
        true
    }

    pub fn session_for_route(&self, route: &WorkspaceRoute) -> Option<SessionSnapshot> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_for_route(route)
            .map(EmulatorSession::snapshot)
    }

    pub fn grpc_client_for_route(&self, route: &WorkspaceRoute) -> Option<GrpcClient> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_for_route(route)
            .and_then(|session| session.resources.as_ref())
            .and_then(|resources| resources.grpc_client.clone())
    }

    pub fn microphone_endpoint_for_route(
        &self,
        route: &WorkspaceRoute,
    ) -> Option<MicrophoneEndpointDescriptor> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_for_route(route)
            .and_then(|session| session.resources.as_ref())
            .and_then(|resources| resources.microphone.as_ref())
            .map(PulseMicrophoneEndpoint::descriptor)
    }

    pub fn begin_stop_route(&self, route: &WorkspaceRoute) -> Result<StopCommand, RegistryError> {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if !registry.route_is_current(route) {
            return Err(RegistryError::StaleGeneration(route.avd_name.clone()));
        }
        registry.begin_stop(&route.avd_name)
    }

    pub fn focus_session(&self, avd_name: &str) -> Result<WorkspaceRoute, WorkspaceError> {
        let route = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .route_for_avd(avd_name)
            .ok_or_else(|| {
                WorkspaceError::UnknownRoute(WorkspaceRoute {
                    avd_name: avd_name.to_owned(),
                    session_id: 0,
                    generation: 0,
                })
            })?;
        self.workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .focus(&route)?;
        Ok(route)
    }

    pub fn toggle_selected(&self, route: &WorkspaceRoute) -> Result<bool, WorkspaceError> {
        self.workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .toggle_selected(route)
    }

    pub fn operation_targets(&self, scope: OperationScope) -> Vec<WorkspaceRoute> {
        self.workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .targets(scope)
    }

    pub fn plan_operation(
        &self,
        kind: OperationKind,
        scope: OperationScope,
    ) -> Result<OperationPlan, OperationPlanError> {
        self.operations
            .plan(kind, scope, self.operation_targets(scope))
    }

    pub fn authorize_operation(
        &self,
        plan: OperationPlan,
    ) -> Result<AuthorizedOperation, OperationPlanError> {
        self.operations
            .authorize(plan, self.workspace_snapshot().routes)
    }

    pub async fn execute_operation_with<F, Fut>(
        &self,
        authorized: AuthorizedOperation,
        run: F,
    ) -> OperationReport
    where
        F: FnMut(WorkspaceRoute) -> Fut,
        Fut: std::future::Future<Output = Result<OperationSuccess, OperationRunError>>,
    {
        self.operations
            .execute_with(authorized, |route| self.route_is_current(route), run)
            .await
    }

    pub fn workspace_snapshot(&self) -> WorkspaceSnapshot {
        self.workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }

    pub fn workspace_intent(&self) -> WorkspaceIntent {
        self.workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .intent()
    }

    pub fn restore_workspace_intent(&self, intent: &WorkspaceIntent) {
        self.workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .restore_intent(intent);
    }

    fn sync_workspace(&self) {
        let routes = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .workspace_routes();
        self.workspace
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reconcile(routes);
    }
}

fn recover_session_resources(instance: &RunningInstance) -> Option<SessionResources> {
    let auth = match GrpcJwtAuth::recover(instance) {
        Ok(Some(auth)) => auth,
        Ok(None) => return None,
        Err(error) => {
            emit(
                AppLogLevel::Warn,
                format_args!(
                    "无法恢复 AVD {} 的 gRPC 身份，将只读接管：{error:#}",
                    instance.avd_name
                ),
            );
            return None;
        }
    };
    let client = match GrpcClient::reconnect_config(instance.grpc_port, auth.clone()) {
        Ok(client) => client,
        Err(error) => {
            auth.preserve_recovery_on_drop();
            emit(
                AppLogLevel::Warn,
                format_args!(
                    "无法恢复 AVD {} 的 gRPC client，将只读接管：{error:#}",
                    instance.avd_name
                ),
            );
            return None;
        }
    };
    let microphone = match PulseMicrophoneEndpoint::recover(&auth) {
        Ok(microphone) => microphone,
        Err(error) => {
            emit(
                AppLogLevel::Warn,
                format_args!(
                    "无法恢复 AVD {} 的虚拟麦克风，控制与画面仍继续恢复：{error:#}",
                    instance.avd_name
                ),
            );
            None
        }
    };
    let capture = match CaptureHandle::start(instance.console_port) {
        Ok(capture) => capture,
        Err(error) => {
            auth.preserve_recovery_on_drop();
            emit(
                AppLogLevel::Warn,
                format_args!(
                    "无法恢复 AVD {} 的 share-vid capture，将只读接管：{error}",
                    instance.avd_name
                ),
            );
            return None;
        }
    };
    Some(SessionResources {
        microphone,
        grpc_auth: auth,
        grpc_client: Some(client),
        process: None,
        capture: Some(capture),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;
    use crate::core::grpc_auth::GrpcJwtAuth;
    use crate::core::scheduler::{QueueReason, SchedulerConfig, SchedulerError};
    use crate::core::stream::CaptureHandle;

    fn instance(avd_name: &str, port: u16) -> RunningInstance {
        instance_with_pid(avd_name, port, u32::from(port))
    }

    fn instance_with_pid(avd_name: &str, port: u16, pid: u32) -> RunningInstance {
        RunningInstance {
            pid,
            ini_path: PathBuf::from(format!("/tmp/pid_{port}.ini")),
            avd_name: avd_name.to_owned(),
            console_port: port,
            adb_port: port + 1,
            grpc_port: port + 3000,
            grpc_allowlist: None,
            grpc_jwks: None,
            grpc_jwk_active: None,
        }
    }

    fn launched(avd_name: &str, port: u16) -> LaunchedInstance {
        LaunchedInstance::test_instance(instance(avd_name, port))
    }

    #[test]
    fn runtime_carries_managed_gpu_policy_without_ambiguous_mode() {
        let runtime = DeviceRuntime::with_runtime_policy(
            SchedulerConfig::default(),
            ManagedGpuPolicy::DesktopHost,
        )
        .unwrap();
        assert_eq!(runtime.managed_gpu_policy(), ManagedGpuPolicy::DesktopHost);
        assert!(runtime.try_update_managed_gpu_policy(ManagedGpuPolicy::HeadlessSwangle));
        assert_eq!(
            runtime.managed_gpu_policy(),
            ManagedGpuPolicy::HeadlessSwangle
        );

        let (_command, _ticket, _status) = runtime
            .schedule_start("queued", ResourceDemand::new(1536, 0))
            .unwrap();
        assert!(!runtime.try_update_managed_gpu_policy(ManagedGpuPolicy::DesktopHost));
        assert_eq!(
            runtime.managed_gpu_policy(),
            ManagedGpuPolicy::HeadlessSwangle
        );
    }

    #[test]
    fn managed_session_owns_port_until_stop_completes() {
        let runtime = DeviceRuntime::default();
        let start = runtime.begin_start("pixel").unwrap();
        let reservation = runtime.reserve_port([]).unwrap();
        assert_eq!(reservation.port(), 5554);
        runtime
            .attach_start_port(&start, reservation.port())
            .unwrap();
        runtime.mark_booting(&start).unwrap();
        runtime
            .complete_start(&start, launched("pixel", 5554), reservation)
            .unwrap();

        let next = runtime.reserve_port([]).unwrap();
        assert_eq!(next.port(), 5556);
        drop(next);

        let stop = runtime.begin_stop("pixel").unwrap();
        runtime.complete_stop(&stop).unwrap();
        assert_eq!(runtime.reserve_port([]).unwrap().port(), 5554);
    }

    #[test]
    fn failed_stop_retains_session_and_reservation() {
        let runtime = DeviceRuntime::default();
        let start = runtime.begin_start("pixel").unwrap();
        let reservation = runtime.reserve_port([]).unwrap();
        runtime
            .attach_start_port(&start, reservation.port())
            .unwrap();
        runtime.mark_booting(&start).unwrap();
        runtime
            .complete_start(&start, launched("pixel", 5554), reservation)
            .unwrap();

        let stop = runtime.begin_stop("pixel").unwrap();
        runtime.fail_stop(&stop, "cannot stop".into()).unwrap();
        let projection = runtime.projection("pixel");
        assert!(matches!(projection.state.phase, DevicePhase::Error(_)));
        assert!(projection.session.is_some());
        assert_eq!(runtime.reserve_port([]).unwrap().port(), 5556);
    }

    #[test]
    fn scan_adopts_and_releases_external_session() {
        let runtime = DeviceRuntime::default();
        runtime.reconcile_running(vec![instance("external", 5554)]);
        let running = runtime.projection("external");
        assert_eq!(running.state.phase, DevicePhase::Running);
        assert_eq!(running.session.unwrap().origin, SessionOrigin::Adopted);
        assert_eq!(runtime.reserve_port([]).unwrap().port(), 5556);

        runtime.reconcile_running(Vec::new());
        let stopped = runtime.projection("external");
        assert_eq!(stopped.state.phase, DevicePhase::Stopped);
        assert!(stopped.session.is_none());
    }

    #[test]
    fn missing_advertisement_keeps_live_session_route_then_recovers() {
        let runtime = Arc::new(DeviceRuntime::default());
        let start = runtime.begin_start("pixel").unwrap();
        let reservation = runtime.reserve_port([]).unwrap();
        runtime
            .attach_start_port(&start, reservation.port())
            .unwrap();
        runtime.mark_booting(&start).unwrap();
        runtime
            .complete_start(&start, launched("pixel", reservation.port()), reservation)
            .unwrap();
        let route = runtime.input_route("pixel").unwrap().route().clone();

        runtime.reconcile_running_with_probe(Vec::new(), HashMap::new(), |_| true, |_| None);
        assert_eq!(
            runtime.projection("pixel").state.phase,
            DevicePhase::Recovering(RecoveryReason::AdvertisementMissing)
        );
        assert!(runtime.route_is_current(&route));
        assert_eq!(runtime.reserve_port([]).unwrap().port(), 5556);

        runtime.reconcile_running_with_probe(
            vec![instance("pixel", 5554)],
            HashMap::new(),
            |_| true,
            |_| None,
        );
        assert_eq!(
            runtime.projection("pixel").state.phase,
            DevicePhase::Running
        );
        assert!(runtime.route_is_current(&route));
    }

    #[test]
    fn managed_crash_is_error_but_interrupted_stop_completes_as_stopped() {
        let runtime = DeviceRuntime::default();
        let start = runtime.begin_start("crashed").unwrap();
        let reservation = runtime.reserve_port([]).unwrap();
        runtime
            .attach_start_port(&start, reservation.port())
            .unwrap();
        runtime.mark_booting(&start).unwrap();
        runtime
            .complete_start(&start, launched("crashed", reservation.port()), reservation)
            .unwrap();
        runtime.reconcile_running_with_probe(Vec::new(), HashMap::new(), |_| false, |_| None);
        let crashed = runtime.projection("crashed");
        assert_eq!(
            crashed.state.phase,
            DevicePhase::Error("模拟器进程意外退出".into())
        );
        assert!(crashed.session.is_none());
        assert_eq!(runtime.reserve_port([]).unwrap().port(), 5554);

        let start = runtime.begin_start("stopping").unwrap();
        let reservation = runtime.reserve_port([]).unwrap();
        runtime
            .attach_start_port(&start, reservation.port())
            .unwrap();
        runtime.mark_booting(&start).unwrap();
        runtime
            .complete_start(
                &start,
                launched("stopping", reservation.port()),
                reservation,
            )
            .unwrap();
        runtime.begin_stop("stopping").unwrap();
        runtime.reconcile_running_with_probe(Vec::new(), HashMap::new(), |_| false, |_| None);
        let stopped = runtime.projection("stopping");
        assert_eq!(stopped.state.phase, DevicePhase::Stopped);
        assert!(stopped.session.is_none());
    }

    #[test]
    fn adopted_session_survives_advertisement_gap_only_while_process_is_verified() {
        let runtime = DeviceRuntime::default();
        runtime.reconcile_running(vec![instance("external", 5554)]);
        runtime.reconcile_running_with_probe(Vec::new(), HashMap::new(), |_| true, |_| None);
        assert_eq!(
            runtime.projection("external").state.phase,
            DevicePhase::Recovering(RecoveryReason::AdvertisementMissing)
        );
        assert!(runtime.projection("external").session.is_some());

        runtime.reconcile_running_with_probe(Vec::new(), HashMap::new(), |_| false, |_| None);
        assert_eq!(
            runtime.projection("external").state.phase,
            DevicePhase::Stopped
        );
        assert!(runtime.projection("external").session.is_none());
    }

    #[test]
    fn recovered_session_regains_control_capture_and_exact_route_without_recovery_repeat() {
        let runtime = Arc::new(DeviceRuntime::default());
        let auth = Arc::new(GrpcJwtAuth::new().unwrap());
        let client = GrpcClient::test_client(auth.clone());
        let fixture =
            std::env::temp_dir().join(format!("liteavd-recovered-capture-{}", std::process::id()));
        let mut resources = Some(SessionResources {
            microphone: None,
            grpc_auth: auth,
            grpc_client: Some(client),
            process: None,
            capture: Some(CaptureHandle::start_path(&fixture).unwrap()),
        });
        let observed = instance_with_pid("recovered", 5554, 4242);
        runtime.reconcile_running_with_probe(
            vec![observed.clone()],
            HashMap::new(),
            |_| true,
            |_| None,
        );
        let route = runtime.input_route("recovered").unwrap().route().clone();
        assert_eq!(
            runtime.projection("recovered").session.unwrap().origin,
            SessionOrigin::Adopted
        );

        runtime.reconcile_running_with_probe(
            vec![observed.clone()],
            HashMap::new(),
            |_| true,
            |_| resources.take(),
        );
        let snapshot = runtime.projection("recovered").session.unwrap();
        assert_eq!(snapshot.origin, SessionOrigin::Recovered);
        assert!(runtime.grpc_client_for_route(&route).is_some());
        assert!(runtime.capture_subscription("recovered").is_some());

        let mut recovery_calls = 0;
        runtime.reconcile_running_with_probe(
            vec![observed],
            HashMap::new(),
            |_| true,
            |_| {
                recovery_calls += 1;
                None
            },
        );
        assert_eq!(recovery_calls, 0);
        assert!(runtime.route_is_current(&route));
        drop(runtime);
        let _ = std::fs::remove_file(fixture);
    }

    #[test]
    fn scan_never_transfers_session_resources_across_reused_port() {
        let runtime = DeviceRuntime::default();
        runtime.reconcile_running(vec![instance_with_pid("old", 5554, 1001)]);
        let old_session = runtime.projection("old").session.unwrap().id;

        runtime.reconcile_running(vec![instance_with_pid("new", 5554, 2002)]);
        assert_eq!(runtime.projection("old").state.phase, DevicePhase::Stopped);
        let replacement = runtime.projection("new").session.unwrap();
        assert_ne!(replacement.id, old_session);
        assert_eq!(replacement.instance.pid, 2002);
        assert_eq!(replacement.origin, SessionOrigin::Adopted);
    }

    #[test]
    fn input_route_and_focus_do_not_cross_session_replacement() {
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![instance_with_pid("pixel", 5554, 1001)]);
        let old_guard = runtime.input_route("pixel").unwrap();
        let old_route = runtime.focus_session("pixel").unwrap();
        assert!(old_guard.is_current());
        assert_eq!(
            runtime.workspace_snapshot().focused,
            Some(old_route.clone())
        );

        runtime.reconcile_running(vec![instance_with_pid("pixel", 5554, 2002)]);
        assert!(!old_guard.is_current());
        assert!(runtime.workspace_snapshot().focused.is_none());
        let replacement = runtime.input_route("pixel").unwrap();
        assert!(replacement.is_current());
        assert_ne!(replacement.route(), &old_route);
    }

    #[test]
    fn input_route_is_invalid_while_stop_is_in_flight_and_recovers_on_failure() {
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![instance_with_pid("pixel", 5554, 1001)]);
        let guard = runtime.input_route("pixel").unwrap();
        let route = runtime.focus_session("pixel").unwrap();
        assert!(guard.is_current());
        let revision = runtime.control_stream_revision();
        assert!(runtime.request_control_stream_reset(&route));
        assert_eq!(runtime.control_stream_revision(), revision + 1);

        let stop = runtime.begin_stop("pixel").unwrap();
        assert!(!guard.is_current());
        assert!(!runtime.request_control_stream_reset(&route));
        assert_eq!(runtime.control_stream_revision(), revision + 1);

        runtime.fail_stop(&stop, "still running".into()).unwrap();
        assert!(guard.is_current());
        assert!(runtime.request_control_stream_reset(&route));
    }

    #[test]
    fn control_disconnect_recovers_without_overwriting_advertisement_failure_or_stale_route() {
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![instance_with_pid("pixel", 5554, 1001)]);
        let route = runtime.input_route("pixel").unwrap().route().clone();
        let control_revision = runtime.control_stream_revision();
        assert!(runtime.report_control_disconnected(&route));
        assert_eq!(runtime.control_stream_revision(), control_revision + 1);
        assert_eq!(
            runtime.projection("pixel").state.phase,
            DevicePhase::Recovering(RecoveryReason::ControlDisconnected)
        );
        assert!(runtime.report_control_connected(&route));
        assert_eq!(
            runtime.projection("pixel").state.phase,
            DevicePhase::Running
        );

        runtime.reconcile_running_with_probe(Vec::new(), HashMap::new(), |_| true, |_| None);
        assert_eq!(
            runtime.projection("pixel").state.phase,
            DevicePhase::Recovering(RecoveryReason::AdvertisementMissing)
        );
        assert!(!runtime.report_control_connected(&route));
        assert_eq!(
            runtime.projection("pixel").state.phase,
            DevicePhase::Recovering(RecoveryReason::AdvertisementMissing)
        );

        runtime.reconcile_running(vec![instance_with_pid("pixel", 5554, 2002)]);
        assert!(!runtime.report_control_disconnected(&route));
    }

    #[test]
    fn operation_confirmation_and_stop_reject_replaced_session() {
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![
            instance_with_pid("phone", 5554, 1001),
            instance_with_pid("tablet", 5556, 1002),
        ]);
        let phone = runtime.input_route("phone").unwrap().route().clone();
        let tablet = runtime.input_route("tablet").unwrap().route().clone();
        runtime.toggle_selected(&tablet).unwrap();
        let plan = runtime
            .plan_operation(OperationKind::Stop, OperationScope::Selected)
            .unwrap();
        assert_eq!(plan.targets(), std::slice::from_ref(&tablet));

        runtime.reconcile_running(vec![
            instance_with_pid("phone", 5554, 1001),
            instance_with_pid("tablet", 5556, 2002),
        ]);

        assert!(matches!(
            runtime.authorize_operation(plan),
            Err(OperationPlanError::TargetsChanged(changed)) if changed == vec![tablet.clone()]
        ));
        assert_eq!(
            runtime.begin_stop_route(&tablet).unwrap_err(),
            RegistryError::StaleGeneration("tablet".into())
        );
        assert!(runtime.route_is_current(&phone));
        assert!(runtime.session_for_route(&tablet).is_none());
    }

    #[test]
    fn duplicate_start_and_stale_completion_are_rejected() {
        let mut registry = InstanceRegistry::default();
        let first = registry.begin_start("pixel").unwrap();
        assert_eq!(
            registry.begin_start("pixel").unwrap_err(),
            RegistryError::AlreadyActive("pixel".into())
        );
        assert!(registry.fail_start(&first, "failed".into()));

        let second = registry.begin_start("pixel").unwrap();
        assert_ne!(first.generation(), second.generation());
        assert_eq!(
            registry.mark_booting(&first).unwrap_err(),
            RegistryError::StaleGeneration("pixel".into())
        );
    }

    #[test]
    fn scan_cannot_steal_pending_managed_start() {
        let runtime = DeviceRuntime::default();
        let start = runtime.begin_start("pixel").unwrap();
        let reservation = runtime.reserve_port([]).unwrap();
        runtime
            .attach_start_port(&start, reservation.port())
            .unwrap();
        runtime.mark_booting(&start).unwrap();

        runtime.reconcile_running(vec![instance("pixel", 5554)]);
        let pending = runtime.projection("pixel");
        assert_eq!(pending.state.phase, DevicePhase::Booting);
        assert!(pending.session.is_none());

        runtime
            .complete_start(&start, launched("pixel", 5554), reservation)
            .unwrap();
        assert_eq!(
            runtime.projection("pixel").state.phase,
            DevicePhase::Running
        );
    }

    #[test]
    fn scheduled_resources_live_with_session_and_release_on_stop() {
        let runtime = DeviceRuntime::with_scheduler_config(SchedulerConfig {
            max_concurrent_starts: 1,
            memory_budget_mb: Some(2048),
            gpu_slots: Some(1),
        })
        .unwrap();
        let (first, mut first_ticket, _) = runtime
            .schedule_start("first", ResourceDemand::new(1536, 0))
            .unwrap();
        assert!(matches!(
            runtime.projection("first").state.phase,
            DevicePhase::Queued(_)
        ));
        let first_permit = first_ticket.try_acquire().unwrap().unwrap();
        runtime.mark_starting(&first).unwrap();
        let first_port = runtime.reserve_port([]).unwrap();
        runtime
            .attach_start_port(&first, first_port.port())
            .unwrap();
        runtime.mark_booting(&first).unwrap();
        runtime
            .complete_scheduled_start(
                &first,
                launched("first", first_port.port()),
                first_port,
                first_permit,
            )
            .unwrap();

        let (second, mut second_ticket, _) = runtime
            .schedule_start("second", ResourceDemand::new(1024, 0))
            .unwrap();
        assert!(second_ticket.try_acquire().unwrap().is_none());
        assert_eq!(
            second_ticket.status().unwrap().reason,
            QueueReason::Memory {
                requested: 1024,
                available: 512,
            }
        );

        let stop = runtime.begin_stop("first").unwrap();
        runtime.complete_stop(&stop).unwrap();
        let second_permit = second_ticket.try_acquire().unwrap().unwrap();
        runtime.mark_starting(&second).unwrap();
        drop(second_permit);
    }

    #[test]
    fn queued_start_can_be_canceled_without_stale_completion() {
        let runtime = DeviceRuntime::default();
        let (first, mut first_ticket, _) = runtime
            .schedule_start("first", ResourceDemand::default())
            .unwrap();
        let first_permit = first_ticket.try_acquire().unwrap().unwrap();
        runtime.mark_starting(&first).unwrap();

        let (second, second_ticket, _) = runtime
            .schedule_start("second", ResourceDemand::default())
            .unwrap();
        assert!(runtime.cancel_queued_start("second"));
        assert_eq!(
            second_ticket.wait().unwrap_err(),
            SchedulerError::Canceled("second".into())
        );
        assert_eq!(
            runtime.projection("second").state.phase,
            DevicePhase::Stopped
        );
        assert_eq!(
            runtime.mark_starting(&second).unwrap_err(),
            RegistryError::StaleGeneration("second".into())
        );
        drop(first_permit);
    }

    #[test]
    fn adopted_sessions_rebuild_and_release_scheduler_budget() {
        let runtime = DeviceRuntime::with_scheduler_config(SchedulerConfig {
            max_concurrent_starts: 1,
            memory_budget_mb: Some(2048),
            gpu_slots: None,
        })
        .unwrap();
        runtime.reconcile_running_with_demands(
            vec![instance("external", 5554)],
            HashMap::from([("external".into(), ResourceDemand::new(1536, 0))]),
        );
        let (managed, mut ticket, _) = runtime
            .schedule_start("managed", ResourceDemand::new(1024, 0))
            .unwrap();
        assert!(ticket.try_acquire().unwrap().is_none());
        assert!(matches!(
            ticket.status().unwrap().reason,
            QueueReason::Memory { .. }
        ));

        runtime.reconcile_running_with_demands(Vec::new(), HashMap::new());
        let permit = ticket.try_acquire().unwrap().unwrap();
        runtime.mark_starting(&managed).unwrap();
        drop(permit);
    }

    #[test]
    fn managed_process_disappearance_releases_resource_reservation() {
        let runtime = DeviceRuntime::with_scheduler_config(SchedulerConfig {
            max_concurrent_starts: 1,
            memory_budget_mb: Some(1024),
            gpu_slots: None,
        })
        .unwrap();
        let (first, mut first_ticket, _) = runtime
            .schedule_start("first", ResourceDemand::new(1024, 0))
            .unwrap();
        let first_permit = first_ticket.try_acquire().unwrap().unwrap();
        runtime.mark_starting(&first).unwrap();
        let port = runtime.reserve_port([]).unwrap();
        runtime.attach_start_port(&first, port.port()).unwrap();
        runtime.mark_booting(&first).unwrap();
        runtime
            .complete_scheduled_start(&first, launched("first", port.port()), port, first_permit)
            .unwrap();

        let (second, mut second_ticket, _) = runtime
            .schedule_start("second", ResourceDemand::new(1024, 0))
            .unwrap();
        assert!(second_ticket.try_acquire().unwrap().is_none());
        runtime.reconcile_running(Vec::new());
        assert_eq!(
            runtime.projection("first").state.phase,
            DevicePhase::Error("模拟器进程意外退出".into())
        );
        assert!(second_ticket.try_acquire().unwrap().is_some());
        runtime.fail_start(&second, "test cleanup".into());
    }

    #[tokio::test]
    async fn managed_session_retains_grpc_identity_until_stop() {
        let runtime = DeviceRuntime::default();
        let start = runtime.begin_start("pixel").unwrap();
        let reservation = runtime.reserve_port([]).unwrap();
        runtime
            .attach_start_port(&start, reservation.port())
            .unwrap();
        runtime.mark_booting(&start).unwrap();

        let auth = Arc::new(GrpcJwtAuth::new().unwrap());
        let weak_auth = Arc::downgrade(&auth);
        let sdk_root = PathBuf::from("/tmp/liteavd-test-sdk");
        let log_path = PathBuf::from("/tmp/liteavd-test.log");
        let mut launched = LaunchedInstance::test_managed(
            instance("pixel", 5554),
            auth,
            4242,
            sdk_root.clone(),
            log_path.clone(),
        );
        let capture = CaptureHandle::start_path(std::env::temp_dir().join(format!(
            "liteavd-session-capture-missing-{}",
            std::process::id()
        )))
        .unwrap();
        let capture_lifetime = capture.subscribe();
        launched.test_attach_capture(capture);
        runtime
            .complete_start(&start, launched, reservation)
            .unwrap();
        assert!(weak_auth.upgrade().is_some());
        assert!(runtime.grpc_client("pixel").is_some());
        assert!(runtime.capture_subscription("pixel").is_some());
        assert!(!capture_lifetime.is_closed());
        assert_eq!(
            runtime.projection("pixel").session.unwrap().log_path,
            Some(log_path.clone())
        );

        let stop = runtime.begin_stop("pixel").unwrap();
        assert_eq!(stop.launcher_pid(), Some(4242));
        assert_eq!(stop.sdk_root(), Some(sdk_root.as_path()));
        assert_eq!(stop.log_path(), Some(log_path.as_path()));
        runtime.complete_stop(&stop).unwrap();
        assert!(weak_auth.upgrade().is_none());
        assert!(runtime.grpc_client("pixel").is_none());
        assert!(runtime.capture_subscription("pixel").is_none());
        assert!(capture_lifetime.is_closed());
    }
}
