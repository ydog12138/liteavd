//! 模拟器子进程管理 + 广告文件（pid_*.ini）发现/收养。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Read as _;
use std::net::TcpListener;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};

use crate::core::avd::{self, ManagedGpuPolicy};
use crate::core::grpc::GrpcClient;
use crate::core::grpc_auth::{GrpcJwtAuth, GrpcLaunchConfig};
use crate::core::microphone::{MicrophoneEndpointDescriptor, PulseMicrophoneEndpoint};
use crate::core::process_log::LaunchLog;
use crate::core::scheduler::{FIRST_CONSOLE_PORT, LAST_CONSOLE_PORT};
use crate::core::stream::{CaptureHandle, CaptureStats, CaptureSubscription};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedAudioPolicy {
    /// Disable the Emulator audio engine entirely.
    Disabled,
    /// Provision an exact, liteavd-owned Pulse FIFO source. Optional mode keeps
    /// device startup working on hosts without a compatible Pulse server.
    VirtualMicrophone { required: bool },
}

/// 启动参数。
#[derive(Debug, Clone)]
pub struct LaunchParams {
    pub sdk_root: PathBuf,
    pub avd_name: String,
    pub port: u16,
    pub grpc: GrpcLaunchConfig,
    pub gpu_policy: ManagedGpuPolicy,
    pub audio_policy: ManagedAudioPolicy,
    pub no_window: bool,
    pub share_vid: bool,
}

/// 运行中的实例（广告文件解析结果）。
#[derive(Debug, Clone)]
pub struct RunningInstance {
    pub pid: u32,
    pub ini_path: PathBuf,
    pub avd_name: String,
    pub console_port: u16,
    pub adb_port: u16,
    pub grpc_port: u16,
    pub grpc_allowlist: Option<String>,
    pub grpc_jwks: Option<PathBuf>,
    pub grpc_jwk_active: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedProcess {
    launcher_pid: u32,
    log_path: PathBuf,
    sdk_root: PathBuf,
}

impl ManagedProcess {
    pub(crate) fn launcher_pid(&self) -> u32 {
        self.launcher_pid
    }

    pub(crate) fn log_path(&self) -> &Path {
        &self.log_path
    }

    pub(crate) fn sdk_root(&self) -> &Path {
        &self.sdk_root
    }
}

#[derive(Debug)]
pub(crate) struct SessionResources {
    pub(crate) microphone: Option<PulseMicrophoneEndpoint>,
    pub(crate) grpc_auth: Arc<GrpcJwtAuth>,
    pub(crate) grpc_client: Option<GrpcClient>,
    pub(crate) process: Option<ManagedProcess>,
    pub(crate) capture: Option<CaptureHandle>,
}

/// 由 liteavd 启动、但尚未提交到 registry 的实例。
///
/// 若 future 被取消或提交失败，drop 会向已验证的 engine/launcher 发送终止信号；
/// `InstanceRegistry::complete_start` 消费该值后解除 guard，由 session 接管资源。
#[derive(Debug)]
pub struct LaunchedInstance {
    pub instance: RunningInstance,
    resources: Option<SessionResources>,
    cleanup_armed: bool,
}

impl LaunchedInstance {
    fn authenticated(instance: RunningInstance, resources: SessionResources) -> Self {
        Self {
            instance,
            resources: Some(resources),
            cleanup_armed: true,
        }
    }

    pub fn grpc_auth(&self) -> &Arc<GrpcJwtAuth> {
        &self
            .resources
            .as_ref()
            .expect("production launch must retain session resources")
            .grpc_auth
    }

    pub fn grpc_client(&self) -> &GrpcClient {
        self.resources
            .as_ref()
            .and_then(|resources| resources.grpc_client.as_ref())
            .expect("production launch must retain authenticated gRPC client")
    }

    pub fn log_path(&self) -> &Path {
        self.resources
            .as_ref()
            .and_then(|resources| resources.process.as_ref())
            .expect("production launch must retain managed process metadata")
            .log_path()
    }

    pub fn capture_subscription(&self) -> Option<CaptureSubscription> {
        self.resources
            .as_ref()
            .and_then(|resources| resources.capture.as_ref())
            .map(CaptureHandle::subscribe)
    }

    pub fn capture_stats(&self) -> Option<CaptureStats> {
        self.resources
            .as_ref()
            .and_then(|resources| resources.capture.as_ref())
            .map(CaptureHandle::stats)
    }

    pub fn microphone_endpoint(&self) -> Option<MicrophoneEndpointDescriptor> {
        self.resources
            .as_ref()
            .and_then(|resources| resources.microphone.as_ref())
            .map(PulseMicrophoneEndpoint::descriptor)
    }

    pub(crate) fn into_parts(mut self) -> (RunningInstance, Option<SessionResources>) {
        self.cleanup_armed = false;
        (self.instance.clone(), self.resources.take())
    }

    #[cfg(test)]
    pub(crate) fn test_instance(instance: RunningInstance) -> Self {
        Self {
            instance,
            resources: None,
            cleanup_armed: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_managed(
        instance: RunningInstance,
        auth: Arc<GrpcJwtAuth>,
        launcher_pid: u32,
        sdk_root: PathBuf,
        log_path: PathBuf,
    ) -> Self {
        let client = GrpcClient::test_client(auth.clone());
        Self {
            instance,
            resources: Some(SessionResources {
                microphone: None,
                grpc_auth: auth,
                grpc_client: Some(client),
                process: Some(ManagedProcess {
                    launcher_pid,
                    log_path,
                    sdk_root,
                }),
                capture: None,
            }),
            cleanup_armed: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn test_attach_capture(&mut self, capture: CaptureHandle) {
        self.resources
            .as_mut()
            .expect("test managed launch must retain resources")
            .capture = Some(capture);
    }
}

impl Drop for LaunchedInstance {
    fn drop(&mut self) {
        if !self.cleanup_armed {
            return;
        }
        let Some(process) = self
            .resources
            .as_ref()
            .and_then(|resources| resources.process.as_ref())
        else {
            return;
        };
        schedule_managed_cleanup(
            self.instance.pid,
            process.launcher_pid(),
            process.sdk_root().to_path_buf(),
            Some(self.instance.console_port),
        );
    }
}

pub fn advertisement_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| PathBuf::from(d).join("avd/running"))
        .unwrap_or_else(|| {
            PathBuf::from("/run/user")
                .join(uid_hex().as_str())
                .join("avd/running")
        })
}

fn uid_hex() -> String {
    format!("{}", unsafe { libc::getuid() })
}

/// 广告文件路径：`<runtime>/avd/running/pid_<pid>.ini`
pub fn ad_file_path(pid: u32) -> PathBuf {
    advertisement_dir().join(format!("pid_{pid}.ini"))
}

/// 解析广告文件 ini（实测字段：port.serial/port.adb/grpc.port/grpc.allowlist/avd.name...）。
fn parse_ad_file(path: &PathBuf) -> anyhow::Result<RunningInstance> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取广告文件失败：{}", path.display()))?;
    let map: HashMap<String, String> = text
        .lines()
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let pid = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("pid_").and_then(|n| n.strip_suffix(".ini")))
        .and_then(|n| n.parse().ok())
        .context("广告文件名无法解析 pid")?;
    Ok(RunningInstance {
        pid,
        ini_path: path.clone(),
        avd_name: map.get("avd.name").cloned().unwrap_or_default(),
        console_port: map
            .get("port.serial")
            .and_then(|v| v.parse().ok())
            .context("广告文件缺 port.serial")?,
        adb_port: map
            .get("port.adb")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        grpc_port: map
            .get("grpc.port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        grpc_allowlist: map.get("grpc.allowlist").cloned(),
        grpc_jwks: map.get("grpc.jwks").map(PathBuf::from),
        grpc_jwk_active: map.get("grpc.jwk_active").map(PathBuf::from),
    })
}

fn process_alive(pid: u32) -> bool {
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    if !proc_dir.exists() {
        return false;
    }
    std::fs::read_to_string(proc_dir.join("stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(") ")
                .map(|(_, tail)| tail.starts_with('Z'))
        })
        .is_none_or(|zombie| !zombie)
}

fn process_looks_like_emulator(pid: u32) -> bool {
    let executable_matches = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .is_some_and(|name| name == "emulator" || name.contains("qemu-system"));
    let command_matches = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .is_some_and(|cmdline| {
            let text = String::from_utf8_lossy(&cmdline);
            text.contains("qemu-system") || text.contains("-avd")
        });
    executable_matches || command_matches
}

/// 审计 #10：验证 pid 确属本 SDK 的模拟器进程，防 PID 复用误杀。
/// 同时要求 `/proc/<pid>/exe` 规范化后位于 SDK 内，且 exe/cmdline 具有
/// `emulator` / `qemu-system` / `-avd` 特征。任一证据不可读时拒绝终止。
pub fn verify_emulator_pid(pid: u32, sdk_root: &Path) -> bool {
    if !process_alive(pid) || !process_looks_like_emulator(pid) {
        return false;
    }
    let exe_link = PathBuf::from(format!("/proc/{pid}/exe"));
    if let Ok(target) = std::fs::read_link(&exe_link) {
        let canon = target.canonicalize().unwrap_or(target);
        let sdk_canon = sdk_root
            .canonicalize()
            .unwrap_or_else(|_| sdk_root.to_path_buf());
        if canon.starts_with(&sdk_canon) {
            return true;
        }
        return false;
    }
    // exe 读取失败时无法证明进程位于已配置 SDK；拒绝终止。
    false
}

fn signal_verified(pid: u32, sdk_root: &Path, signal: i32) {
    if verify_emulator_pid(pid, sdk_root) {
        unsafe {
            libc::kill(pid as i32, signal);
        }
    }
}

fn schedule_managed_cleanup(
    engine_pid: u32,
    launcher_pid: u32,
    sdk_root: PathBuf,
    console_port: Option<u16>,
) {
    signal_verified(engine_pid, &sdk_root, libc::SIGTERM);
    signal_verified(launcher_pid, &sdk_root, libc::SIGTERM);
    let _ = std::thread::Builder::new()
        .name(format!("liteavd-cleanup-{engine_pid}"))
        .spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline
                && (process_alive(engine_pid) || process_alive(launcher_pid))
            {
                std::thread::sleep(Duration::from_millis(100));
            }
            signal_verified(engine_pid, &sdk_root, libc::SIGKILL);
            signal_verified(launcher_pid, &sdk_root, libc::SIGKILL);
            let kill_deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < kill_deadline && process_alive(engine_pid) {
                std::thread::sleep(Duration::from_millis(100));
            }
            if !process_alive(engine_pid) {
                cleanup_ad_file(engine_pid);
                if let Some(console_port) = console_port {
                    let _ =
                        remove_stale_share_vid(&crate::core::stream::share_vid_path(console_port));
                }
            }
        });
}

#[derive(Debug)]
struct PendingLaunch {
    child: Option<Child>,
    engine: Option<(u32, u16)>,
    sdk_root: PathBuf,
}

impl PendingLaunch {
    fn new(child: Child, sdk_root: PathBuf) -> Self {
        Self {
            child: Some(child),
            engine: None,
            sdk_root,
        }
    }

    fn child_id(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    fn child_exited(&mut self) -> std::io::Result<bool> {
        self.child
            .as_mut()
            .expect("pending launch must own child")
            .try_wait()
            .map(|status| status.is_some())
    }

    fn track_engine(&mut self, instance: &RunningInstance) {
        self.engine = Some((instance.pid, instance.console_port));
    }

    fn engine_stopped(&mut self) {
        self.engine = None;
    }

    fn detach_reaper(&mut self) -> anyhow::Result<u32> {
        let launcher_pid = self.child_id().expect("pending launch must own child");
        std::thread::Builder::new()
            .name(format!("liteavd-launcher-{launcher_pid}"))
            .spawn(move || {
                loop {
                    let result =
                        unsafe { libc::waitpid(launcher_pid as i32, std::ptr::null_mut(), 0) };
                    if result >= 0
                        || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                    {
                        break;
                    }
                }
            })
            .context("创建模拟器 launcher reaper 线程失败")?;
        self.engine = None;
        drop(self.child.take());
        Ok(launcher_pid)
    }
}

impl Drop for PendingLaunch {
    fn drop(&mut self) {
        if let Some((engine_pid, console_port)) = self.engine {
            let launcher_pid = self.child_id().unwrap_or(engine_pid);
            schedule_managed_cleanup(
                engine_pid,
                launcher_pid,
                self.sdk_root.clone(),
                Some(console_port),
            );
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 扫描广告文件目录，返回全部运行中实例（按 /proc 过滤 stale）。
pub fn list_running() -> Vec<RunningInstance> {
    let dir = advertisement_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("pid_"))
        })
        .filter_map(|p| parse_ad_file(&p).ok())
        .filter(|i| process_alive(i.pid))
        .collect()
}

/// 扫描并验证广告 PID 确属当前 SDK 的 emulator/qemu 进程。
pub fn list_running_for_sdk(sdk_root: &Path) -> Vec<RunningInstance> {
    list_running()
        .into_iter()
        .filter(|instance| verify_emulator_pid(instance.pid, sdk_root))
        .collect()
}

/// 按 console 端口查找运行实例。
pub fn find_running(console_port: u16) -> Option<RunningInstance> {
    list_running()
        .into_iter()
        .find(|i| i.console_port == console_port)
}

fn validate_launch_slot(
    avd_name: &str,
    console_port: u16,
    grpc_port: u16,
    running: &[RunningInstance],
) -> anyhow::Result<()> {
    if !(FIRST_CONSOLE_PORT..=LAST_CONSOLE_PORT).contains(&console_port)
        || !console_port.is_multiple_of(2)
    {
        bail!(
            "console 端口必须是 {FIRST_CONSOLE_PORT}..={LAST_CONSOLE_PORT} 范围内的偶数：{console_port}"
        );
    }
    if running.iter().any(|instance| instance.avd_name == avd_name) {
        bail!("AVD {avd_name} 已有运行实例");
    }
    if running
        .iter()
        .any(|instance| instance.console_port == console_port)
    {
        bail!("console 端口 {console_port} 已被运行实例占用");
    }
    if grpc_port == 0 {
        bail!("gRPC 端口不能为 0");
    }
    if running
        .iter()
        .any(|instance| instance.grpc_port == grpc_port)
    {
        bail!("gRPC 端口 {grpc_port} 已被运行实例占用");
    }
    Ok(())
}

/// Evidence collected from the verified emulator engine for a desktop host
/// launch.  An opened DRM device is stronger evidence than merely seeing a
/// Vulkan or EGL library in the process map, so it is required alongside the
/// software renderer check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererEvidence {
    pub render_nodes: Vec<PathBuf>,
    pub software_renderers: Vec<String>,
}

/// Pure acceptance rule for renderer evidence.  Filesystem/process probing is
/// kept outside this function so success, software-renderer rejection, and
/// missing-device behavior are deterministic unit-test cases.
pub fn accept_renderer_evidence(evidence: &RendererEvidence) -> anyhow::Result<()> {
    if !evidence.software_renderers.is_empty() {
        bail!(
            "桌面 host GPU 检测到软件 renderer：{}",
            evidence.software_renderers.join(", ")
        );
    }
    if evidence.render_nodes.is_empty() {
        bail!("桌面 host GPU 未检测到 engine 打开的 /dev/dri/* 硬件设备");
    }
    Ok(())
}

const SOFTWARE_RENDERER_MARKERS: [&str; 8] = [
    "swiftshader",
    "llvmpipe",
    "swrast",
    "softpipe",
    "lavapipe",
    "kms_swrast",
    "libegl_software",
    "software rasterizer",
];

fn software_renderer_markers(maps: &str) -> Vec<String> {
    let maps = maps.to_ascii_lowercase();
    SOFTWARE_RENDERER_MARKERS
        .iter()
        .filter(|marker| maps.contains(**marker))
        .map(|marker| (*marker).to_owned())
        .collect()
}

fn renderer_evidence(pid: u32) -> anyhow::Result<RendererEvidence> {
    let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
    let mut render_nodes = Vec::new();
    for entry in std::fs::read_dir(&fd_dir)
        .with_context(|| format!("读取已验证 engine 的 fd 目录失败：{}", fd_dir.display()))?
        .flatten()
    {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let target_text = target.to_string_lossy();
        if target_text.starts_with("/dev/dri/") {
            render_nodes.push(target);
        }
    }
    render_nodes.sort();
    render_nodes.dedup();
    let maps_path = format!("/proc/{pid}/maps");
    let maps = std::fs::read_to_string(&maps_path)
        .with_context(|| format!("读取已验证 engine renderer maps 失败：{maps_path}"))?;
    Ok(RendererEvidence {
        render_nodes,
        software_renderers: software_renderer_markers(&maps),
    })
}

/// Validate host GPU evidence only after the engine PID has passed the SDK
/// identity check.  This function deliberately has no software fallback.
pub fn validate_desktop_host_renderer(
    pid: u32,
    sdk_root: &Path,
) -> anyhow::Result<RendererEvidence> {
    if !verify_emulator_pid(pid, sdk_root) {
        bail!("桌面 host GPU 的 engine 身份校验失败，拒绝使用未验证进程");
    }
    let evidence = renderer_evidence(pid)?;
    accept_renderer_evidence(&evidence)?;
    Ok(evidence)
}

/// Desktop host uses the caller's existing XWayland display.  Do this before
/// opening ports or spawning the emulator so a missing display cannot leave a
/// partially-started process behind.
pub fn validate_desktop_host_display() -> anyhow::Result<()> {
    if !display_value_is_nonempty(std::env::var_os("DISPLAY").as_deref()) {
        bail!(
            "桌面 host GPU 需要非空的继承 DISPLAY（通常是 Wayland 会话提供的 XWayland display）；未启动模拟器"
        );
    }
    Ok(())
}

fn display_value_is_nonempty(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.to_string_lossy().trim().is_empty())
}

const MAX_EMULATOR_LOCK_BYTES: u64 = 32;

/// Recover the Emulator's snapshot-operation marker after an interrupted
/// Quick Boot save.  The file is removed only when its recorded PID is gone
/// and neither BSD nor POSIX advisory locking reports a live owner.
fn recover_stale_snapshot_lock(avd_name: &str) -> anyhow::Result<bool> {
    let avd = avd::list_avds()
        .into_iter()
        .find(|avd| avd.name == avd_name)
        .with_context(|| format!("找不到待启动 AVD：{avd_name}"))?;
    recover_stale_snapshot_lock_in(&avd.path)
}

fn recover_stale_snapshot_lock_in(avd_dir: &Path) -> anyhow::Result<bool> {
    let avd_metadata = std::fs::symlink_metadata(avd_dir)
        .with_context(|| format!("检查 AVD 目录失败：{}", avd_dir.display()))?;
    if !avd_metadata.is_dir()
        || avd_metadata.file_type().is_symlink()
        || avd_metadata.uid() != unsafe { libc::getuid() }
    {
        bail!(
            "AVD 目录类型或所有者不安全，拒绝清理内部锁：{}",
            avd_dir.display()
        );
    }
    let lock_path = avd_dir.join("snapshot.lock.lock");
    let metadata = match std::fs::symlink_metadata(&lock_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("检查 Emulator snapshot 锁失败：{}", lock_path.display())
            });
        }
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.len() > MAX_EMULATOR_LOCK_BYTES
        || metadata.mode() & 0o077 != 0
    {
        bail!(
            "Emulator snapshot 锁类型、所有者或长度不安全，拒绝自动清理：{}",
            lock_path.display()
        );
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)
        .with_context(|| format!("打开 Emulator snapshot 锁失败：{}", lock_path.display()))?;
    let opened_metadata = file
        .metadata()
        .with_context(|| format!("复验 Emulator snapshot 锁失败：{}", lock_path.display()))?;
    if opened_metadata.dev() != metadata.dev() || opened_metadata.ino() != metadata.ino() {
        bail!("Emulator snapshot 锁在检查期间被替换，拒绝自动清理");
    }
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            bail!(
                "Emulator snapshot 操作仍持有锁，拒绝启动：{}",
                lock_path.display()
            );
        }
        return Err(error).context("检查 Emulator snapshot BSD 锁失败");
    }

    let mut posix_lock: libc::flock = unsafe { std::mem::zeroed() };
    posix_lock.l_type = libc::F_WRLCK as _;
    posix_lock.l_whence = libc::SEEK_SET as _;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &posix_lock) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            bail!(
                "Emulator snapshot 操作仍持有 POSIX 锁，拒绝启动：{}",
                lock_path.display()
            );
        }
        return Err(error).context("检查 Emulator snapshot POSIX 锁失败");
    }

    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .with_context(|| format!("读取 Emulator snapshot 锁失败：{}", lock_path.display()))?;
    let pid_text = contents
        .split(|byte| *byte == 0)
        .next()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .context("Emulator snapshot 锁不含可验证 PID，拒绝自动清理")?;
    let pid = pid_text
        .parse::<u32>()
        .context("Emulator snapshot 锁 PID 非法，拒绝自动清理")?;
    if process_alive(pid) {
        bail!("Emulator snapshot 锁记录的进程 {pid} 仍存活，拒绝启动");
    }

    std::fs::remove_file(&lock_path).with_context(|| {
        format!(
            "清理 stale Emulator snapshot 锁失败：{}",
            lock_path.display()
        )
    })?;
    File::open(avd_dir)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("同步 AVD 目录失败：{}", avd_dir.display()))?;
    Ok(true)
}

/// 启动模拟器子进程并等待广告文件出现。
pub async fn launch(params: &LaunchParams) -> anyhow::Result<LaunchedInstance> {
    let gpu_policy = params.gpu_policy;
    if gpu_policy == ManagedGpuPolicy::DesktopHost {
        validate_desktop_host_display()?;
    }
    let grpc_port = params.grpc.port();
    validate_launch_slot(&params.avd_name, params.port, grpc_port, &list_running())?;
    if recover_stale_snapshot_lock(&params.avd_name)? {
        crate::core::settings::emit(
            crate::core::settings::AppLogLevel::Warn,
            format_args!(
                "已清理 AVD {} 的 stale Emulator snapshot 锁",
                params.avd_name
            ),
        );
    }
    let console_listener = TcpListener::bind(("127.0.0.1", params.port))
        .with_context(|| format!("console 端口 {} 实际不可用", params.port))?;
    let adb_listener = TcpListener::bind(("127.0.0.1", params.port + 1))
        .with_context(|| format!("adb 端口 {} 实际不可用", params.port + 1))?;
    let listener = TcpListener::bind(("127.0.0.1", grpc_port))
        .with_context(|| format!("gRPC 端口 {grpc_port} 不可用"))?;
    if params.share_vid {
        remove_stale_share_vid(&crate::core::stream::share_vid_path(params.port))?;
    }
    drop(console_listener);
    drop(adb_listener);
    drop(listener);
    let exe = params.sdk_root.join("emulator/emulator");
    if !exe.exists() {
        bail!("模拟器二进制不存在：{}", exe.display());
    }
    let microphone = match &params.audio_policy {
        ManagedAudioPolicy::Disabled => None,
        ManagedAudioPolicy::VirtualMicrophone { required } => {
            match PulseMicrophoneEndpoint::create(params.grpc.auth()) {
                Ok(endpoint) => Some(endpoint),
                Err(error) if *required => {
                    return Err(error.context("创建必需的虚拟麦克风端点失败"));
                }
                Err(error) => {
                    crate::core::settings::emit(
                        crate::core::settings::AppLogLevel::Warn,
                        format_args!(
                            "AVD {} 的虚拟麦克风不可用，继续以无音频输入模式启动：{error:#}",
                            params.avd_name
                        ),
                    );
                    None
                }
            }
        }
    };
    let log = LaunchLog::create(&params.avd_name, params.port)?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.env("ANDROID_AVD_HOME", crate::core::avd::avd_root());
    cmd.arg("-avd")
        .arg(&params.avd_name)
        .arg("-port")
        .arg(params.port.to_string())
        .arg("-grpc")
        .arg(grpc_port.to_string())
        .arg("-grpc-use-jwt")
        .arg("-grpc-allowlist")
        .arg(params.grpc.auth().allowlist_path())
        .arg("-gpu")
        .arg(gpu_policy.gpu_mode().as_str())
        .arg("-no-metrics")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(endpoint) = microphone.as_ref() {
        let descriptor = endpoint.descriptor();
        cmd.env("PULSE_SOURCE", &descriptor.pulse_source)
            .env("QEMU_AUDIO_DRV", "pa")
            .env("QEMU_PA_SOURCE", &descriptor.pulse_source)
            .env("QEMU_PA_SINK", &descriptor.pulse_sink);
        if let Some(server) = &descriptor.pulse_server {
            cmd.env("PULSE_SERVER", server)
                .env("QEMU_PA_SERVER", server);
        }
        cmd.arg("-audio").arg("pa").arg("-allow-host-audio");
    }
    if microphone.is_some() {
        // Linux headless Emulator binaries omit PulseAudio. Keep the ordinary
        // binary hidden. The bundled Qt has no Wayland platform plugin: use
        // offscreen for swangle and the already-validated XWayland display for
        // desktop host rendering, regardless of the parent app's Qt platform.
        cmd.arg("-qt-hide-window");
        cmd.env("QT_QPA_PLATFORM", microphone_qt_platform(gpu_policy));
        if params.no_window {
            cmd.arg("-no-boot-anim");
        }
    } else if params.no_window || gpu_policy == ManagedGpuPolicy::DesktopHost {
        cmd.arg("-no-window");
        cmd.arg("-no-audio");
        cmd.arg("-no-boot-anim");
    } else {
        cmd.arg("-qt-hide-window");
    }
    if params.share_vid {
        cmd.arg("-share-vid");
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("启动模拟器失败；日志：{}", log.path().display()))?;
    let mut pending = PendingLaunch::new(child, params.sdk_root.clone());
    let child = pending
        .child
        .as_mut()
        .expect("pending launch must own child");
    let stdout = child.stdout.take().expect("stdout configured as piped");
    let stderr = child.stderr.take().expect("stderr configured as piped");
    log.capture(stdout, stderr)
        .with_context(|| format!("初始化模拟器日志捕获失败；日志：{}", log.path().display()))?;

    // 轮询广告文件（最长 30s）。注意：广告文件按 qemu 引擎 pid 命名，
    // launcher 的 pid 不同，故按 avd.name 匹配而非 child.id()。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut inst = None;
    while tokio::time::Instant::now() < deadline {
        inst = list_running_for_sdk(&params.sdk_root)
            .into_iter()
            .find(|i| i.avd_name == params.avd_name && i.console_port == params.port);
        if inst.is_some() {
            break;
        }
        // 进程早退则快速失败
        if pending.child_exited().unwrap_or(true)
            && !list_running_for_sdk(&params.sdk_root)
                .iter()
                .any(|i| i.avd_name == params.avd_name && i.console_port == params.port)
        {
            bail!(
                "模拟器启动失败（launcher {} 已退出）；日志：{}",
                pending.child_id().unwrap_or_default(),
                log.path().display()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let Some(inst) = inst else {
        bail!(
            "等待广告文件超时（30s），模拟器可能启动失败；日志：{}",
            log.path().display()
        );
    };
    pending.track_engine(&inst);
    if inst.grpc_port != grpc_port {
        return Err(abort_after_advertisement(
            &mut pending,
            &inst,
            &params.sdk_root,
            anyhow!("广告文件 gRPC 端口与启动配置不一致"),
            log.path(),
        )
        .await);
    }
    let Some(jwks_dir) = inst.grpc_jwks.as_deref() else {
        return Err(abort_after_advertisement(
            &mut pending,
            &inst,
            &params.sdk_root,
            anyhow!("JWT 模式广告文件缺少 grpc.jwks"),
            log.path(),
        )
        .await);
    };
    let Some(active_jwk) = inst.grpc_jwk_active.as_deref() else {
        return Err(abort_after_advertisement(
            &mut pending,
            &inst,
            &params.sdk_root,
            anyhow!("JWT 模式广告文件缺少 grpc.jwk_active"),
            log.path(),
        )
        .await);
    };
    if let Err(error) = params
        .grpc
        .auth()
        .install_public_jwk(jwks_dir, active_jwk)
        .await
    {
        return Err(abort_after_advertisement(
            &mut pending,
            &inst,
            &params.sdk_root,
            error.context("安装模拟器 gRPC JWK 失败"),
            log.path(),
        )
        .await);
    }
    let auth = params.grpc.auth().clone();
    let client = match GrpcClient::connect(grpc_port, auth.clone()).await {
        Ok(client) => client,
        Err(error) => {
            return Err(abort_after_advertisement(
                &mut pending,
                &inst,
                &params.sdk_root,
                error.context("连接模拟器 gRPC 失败"),
                log.path(),
            )
            .await);
        }
    };
    if let Err(error) = client.status().await {
        return Err(abort_after_advertisement(
            &mut pending,
            &inst,
            &params.sdk_root,
            error.context("验证模拟器 gRPC JWT 失败"),
            log.path(),
        )
        .await);
    }
    if microphone.is_some() {
        if let Err(error) = client.set_microphone_enabled(false).await {
            return Err(abort_after_advertisement(
                &mut pending,
                &inst,
                &params.sdk_root,
                error.context("将虚拟麦克风初始化为关闭状态失败"),
                log.path(),
            )
            .await);
        }
        match client.microphone_state().await {
            Ok(false) => {}
            Ok(true) => {
                return Err(abort_after_advertisement(
                    &mut pending,
                    &inst,
                    &params.sdk_root,
                    anyhow!("虚拟麦克风关闭状态复验失败"),
                    log.path(),
                )
                .await);
            }
            Err(error) => {
                return Err(abort_after_advertisement(
                    &mut pending,
                    &inst,
                    &params.sdk_root,
                    error.context("复验虚拟麦克风关闭状态失败"),
                    log.path(),
                )
                .await);
            }
        }
    }
    if let Err(error) = auth.bind_recovery(&inst) {
        return Err(abort_after_advertisement(
            &mut pending,
            &inst,
            &params.sdk_root,
            error.context("提交模拟器 gRPC 恢复身份失败"),
            log.path(),
        )
        .await);
    }
    if gpu_policy == ManagedGpuPolicy::DesktopHost
        && let Err(error) = validate_desktop_host_renderer(inst.pid, &params.sdk_root)
    {
        return Err(abort_after_advertisement(
            &mut pending,
            &inst,
            &params.sdk_root,
            error.context("桌面 host GPU 硬件 renderer 校验失败"),
            log.path(),
        )
        .await);
    }
    let capture = if params.share_vid {
        match CaptureHandle::start(params.port) {
            Ok(capture) => Some(capture),
            Err(error) => {
                return Err(abort_after_advertisement(
                    &mut pending,
                    &inst,
                    &params.sdk_root,
                    error.into(),
                    log.path(),
                )
                .await);
            }
        }
    } else {
        None
    };
    let launcher_pid = match pending.detach_reaper() {
        Ok(pid) => pid,
        Err(error) => {
            return Err(abort_after_advertisement(
                &mut pending,
                &inst,
                &params.sdk_root,
                error,
                log.path(),
            )
            .await);
        }
    };
    Ok(LaunchedInstance::authenticated(
        inst,
        SessionResources {
            microphone,
            grpc_auth: auth,
            grpc_client: Some(client),
            process: Some(ManagedProcess {
                launcher_pid,
                log_path: log.path().to_path_buf(),
                sdk_root: params.sdk_root.clone(),
            }),
            capture,
        },
    ))
}

fn microphone_qt_platform(gpu_policy: ManagedGpuPolicy) -> &'static str {
    match gpu_policy {
        ManagedGpuPolicy::HeadlessSwangle => "offscreen",
        ManagedGpuPolicy::DesktopHost => "xcb",
    }
}

fn remove_stale_share_vid(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "清理已确认无端口占用的 share-vid 残留失败：{}",
                path.display()
            )
        }),
    }
}

async fn abort_after_advertisement(
    pending: &mut PendingLaunch,
    instance: &RunningInstance,
    sdk_root: &Path,
    primary: anyhow::Error,
    log_path: &Path,
) -> anyhow::Error {
    match stop_instance(instance, sdk_root).await {
        Ok(()) => {
            pending.engine_stopped();
            anyhow!("{primary:#}; 日志：{}", log_path.display())
        }
        Err(cleanup) => anyhow!(
            "{primary:#}; 启动失败后的 engine 清理也失败：{cleanup:#}; 日志：{}",
            log_path.display()
        ),
    }
}

/// 启动并等待 boot 完成（launch + adb boot 判定）。
pub async fn launch_and_boot(
    params: &LaunchParams,
    boot_timeout: Duration,
) -> anyhow::Result<LaunchedInstance> {
    let launched = launch(params).await?;
    let serial = format!("emulator-{}", launched.instance.console_port);
    match crate::core::adb::wait_for_boot(&params.sdk_root, &serial, boot_timeout).await {
        Ok(_) => Ok(launched),
        Err(e) => {
            let log_path = launched.log_path().to_path_buf();
            match stop_launched(&launched).await {
                Ok(()) => Err(e).with_context(|| format!("日志：{}", log_path.display())),
                Err(cleanup) => Err(anyhow!(
                    "{e:#}; boot 失败后的清理也失败：{cleanup:#}; 日志：{}",
                    log_path.display()
                )),
            }
        }
    }
}

/// 停止尚未提交到 registry 的 managed launch。
pub async fn stop_launched(launched: &LaunchedInstance) -> anyhow::Result<()> {
    let process = launched
        .resources
        .as_ref()
        .and_then(|resources| resources.process.as_ref())
        .context("managed launch 缺少进程元数据")?;
    stop_managed(
        &launched.instance,
        process.launcher_pid(),
        process.sdk_root(),
    )
    .await
}

/// 停止已提交 session 的 engine 与 launcher；两者都必须通过进程身份校验。
pub async fn stop_managed(
    instance: &RunningInstance,
    launcher_pid: u32,
    sdk_root: &Path,
) -> anyhow::Result<()> {
    let engine = stop_instance(instance, sdk_root).await;
    let launcher = if launcher_pid == instance.pid {
        Ok(())
    } else {
        stop_process(launcher_pid, sdk_root, false).await
    };
    match (engine, launcher) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(engine), Ok(())) => Err(engine).context("停止模拟器 engine 失败"),
        (Ok(()), Err(launcher)) => Err(launcher).context("停止模拟器 launcher 失败"),
        (Err(engine), Err(launcher)) => Err(anyhow!(
            "停止 engine 失败：{engine:#}; 停止 launcher 也失败：{launcher:#}"
        )),
    }
}

/// 停止一个已解析的实例，并在确认 engine 退出后删除其命名 share-vid 文件。
pub async fn stop_instance(instance: &RunningInstance, sdk_root: &Path) -> anyhow::Result<()> {
    stop_process_with_timeouts(
        instance.pid,
        sdk_root,
        true,
        Some(&crate::core::stream::share_vid_path(instance.console_port)),
        Duration::from_secs(20),
        Duration::from_secs(5),
    )
    .await
}

/// 停止实例：SIGTERM，等 10s，再 SIGKILL。
/// 审计 #10：kill 前必须验证 pid 确属本 SDK 模拟器（防 PID 复用误杀）。
pub async fn stop(pid: u32, sdk_root: &Path) -> anyhow::Result<()> {
    stop_process(pid, sdk_root, true).await
}

async fn stop_process(pid: u32, sdk_root: &Path, remove_advertisement: bool) -> anyhow::Result<()> {
    stop_process_with_timeouts(
        pid,
        sdk_root,
        remove_advertisement,
        None,
        Duration::from_secs(10),
        Duration::from_secs(5),
    )
    .await
}

async fn stop_process_with_timeouts(
    pid: u32,
    sdk_root: &Path,
    remove_advertisement: bool,
    share_vid_path: Option<&Path>,
    term_timeout: Duration,
    kill_timeout: Duration,
) -> anyhow::Result<()> {
    if !process_alive(pid) {
        return cleanup_stopped_process(pid, remove_advertisement, share_vid_path);
    }
    if !verify_emulator_pid(pid, sdk_root) {
        bail!("拒绝终止：pid {pid} 不属于 SDK 模拟器进程（防 PID 复用误杀）");
    }
    unsafe {
        let r = libc::kill(pid as i32, libc::SIGTERM);
        if r != 0 && process_alive(pid) {
            bail!("kill(TERM) 失败 errno={}", std::io::Error::last_os_error());
        }
    }
    let deadline = tokio::time::Instant::now() + term_timeout;
    while tokio::time::Instant::now() < deadline {
        if !process_alive(pid) {
            return cleanup_stopped_process(pid, remove_advertisement, share_vid_path);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    unsafe {
        let result = libc::kill(pid as i32, libc::SIGKILL);
        if result != 0 && process_alive(pid) {
            bail!("kill(KILL) 失败 errno={}", std::io::Error::last_os_error());
        }
    }
    let kill_deadline = tokio::time::Instant::now() + kill_timeout;
    while tokio::time::Instant::now() < kill_deadline {
        if !process_alive(pid) {
            return cleanup_stopped_process(pid, remove_advertisement, share_vid_path);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    bail!("模拟器进程 {pid} 无法终止")
}

fn cleanup_stopped_process(
    pid: u32,
    remove_advertisement: bool,
    share_vid_path: Option<&Path>,
) -> anyhow::Result<()> {
    let share_vid_result = share_vid_path.map(remove_stale_share_vid).transpose();
    if remove_advertisement {
        cleanup_ad_file(pid);
    }
    share_vid_result.map(|_| ())
}

fn cleanup_ad_file(pid: u32) {
    let p = ad_file_path(pid);
    let _ = std::fs::remove_file(p);
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn snapshot_lock_fixture(suffix: &str, contents: &[u8]) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "liteavd-snapshot-lock-{}-{suffix}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let lock = root.join("snapshot.lock.lock");
        std::fs::write(&lock, contents).unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
        (root, lock)
    }

    #[test]
    fn managed_policy_maps_to_explicit_emulator_mode_and_evidence_rules() {
        assert_eq!(
            ManagedGpuPolicy::HeadlessSwangle.gpu_mode(),
            crate::core::avd::GpuMode::SwangleIndirect
        );
        assert_eq!(
            ManagedGpuPolicy::DesktopHost.gpu_mode(),
            crate::core::avd::GpuMode::Host
        );
        assert_eq!(ManagedGpuPolicy::HeadlessSwangle.gpu_slots(), 0);
        assert_eq!(ManagedGpuPolicy::DesktopHost.gpu_slots(), 1);
        assert_eq!(
            microphone_qt_platform(ManagedGpuPolicy::HeadlessSwangle),
            "offscreen"
        );
        assert_eq!(microphone_qt_platform(ManagedGpuPolicy::DesktopHost), "xcb");
        assert_eq!(
            software_renderer_markers("libvulkan_radeon.so\nlibEGL.so"),
            Vec::<String>::new()
        );
        assert_eq!(
            software_renderer_markers("libvulkan_swiftshader.so libLLVM-llvmpipe.so"),
            vec!["swiftshader".to_owned(), "llvmpipe".to_owned()]
        );
    }

    #[test]
    fn desktop_display_preflight_rejects_missing_and_blank_values() {
        assert!(!display_value_is_nonempty(None));
        assert!(!display_value_is_nonempty(Some(std::ffi::OsStr::new("  "))));
        assert!(display_value_is_nonempty(Some(std::ffi::OsStr::new(":0"))));
    }

    #[test]
    fn stale_snapshot_lock_recovery_requires_dead_pid_and_unlocked_regular_file() {
        let (stale_root, stale_lock) =
            snapshot_lock_fixture("stale", format!("{}\0", u32::MAX).as_bytes());
        assert!(recover_stale_snapshot_lock_in(&stale_root).unwrap());
        assert!(!stale_lock.exists());
        std::fs::remove_dir(stale_root).unwrap();

        let (live_root, live_lock) =
            snapshot_lock_fixture("live", format!("{}\0", std::process::id()).as_bytes());
        let live_error = recover_stale_snapshot_lock_in(&live_root).unwrap_err();
        assert!(live_error.to_string().contains("仍存活"));
        assert!(live_lock.exists());
        std::fs::remove_dir_all(live_root).unwrap();

        let (busy_root, busy_lock) =
            snapshot_lock_fixture("busy", format!("{}\0", u32::MAX).as_bytes());
        let busy_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&busy_lock)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(busy_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let busy_error = recover_stale_snapshot_lock_in(&busy_root).unwrap_err();
        assert!(busy_error.to_string().contains("仍持有锁"));
        drop(busy_file);
        std::fs::remove_dir_all(busy_root).unwrap();

        let symlink_root = std::env::temp_dir().join(format!(
            "liteavd-snapshot-lock-{}-symlink",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&symlink_root);
        std::fs::create_dir(&symlink_root).unwrap();
        let target = symlink_root.join("target");
        std::fs::write(&target, format!("{}\0", u32::MAX)).unwrap();
        std::os::unix::fs::symlink(&target, symlink_root.join("snapshot.lock.lock")).unwrap();
        let symlink_error = recover_stale_snapshot_lock_in(&symlink_root).unwrap_err();
        assert!(symlink_error.to_string().contains("不安全"));
        std::fs::remove_dir_all(symlink_root).unwrap();
    }

    #[test]
    fn renderer_evidence_requires_verified_engine_identity() {
        let error = validate_desktop_host_renderer(
            std::process::id(),
            Path::new("/nonexistent-liteavd-sdk"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("身份校验"));
    }

    #[test]
    fn renderer_evidence_acceptance_has_pure_success_and_rejection_paths() {
        let hardware = RendererEvidence {
            render_nodes: vec![PathBuf::from("/dev/dri/renderD128")],
            software_renderers: Vec::new(),
        };
        assert!(accept_renderer_evidence(&hardware).is_ok());

        let software = RendererEvidence {
            render_nodes: hardware.render_nodes.clone(),
            software_renderers: vec!["llvmpipe".into()],
        };
        let software_error = accept_renderer_evidence(&software).unwrap_err();
        assert!(software_error.to_string().contains("软件 renderer"));

        let missing_device = RendererEvidence {
            render_nodes: Vec::new(),
            software_renderers: Vec::new(),
        };
        let missing_error = accept_renderer_evidence(&missing_device).unwrap_err();
        assert!(missing_error.to_string().contains("硬件设备"));
    }

    #[test]
    fn parses_ad_file() {
        let dir = std::env::temp_dir().join(format!("liteavd-ad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pid_12345.ini");
        std::fs::write(
            &path,
            "avd.ini.encoding=UTF-8\npath=/home/x/.android/avd/dev.avd\navd.name=dev\nport.serial=5554\nport.adb=5555\ngrpc.port=8554\ngrpc.allowlist=/sdk/emulator/lib/emulator_access.json\n",
        )
        .unwrap();
        let inst = parse_ad_file(&path).unwrap();
        assert_eq!(inst.pid, 12345);
        assert_eq!(inst.console_port, 5554);
        assert_eq!(inst.adb_port, 5555);
        assert_eq!(inst.grpc_port, 8554);
        assert_eq!(
            inst.grpc_allowlist.as_deref(),
            Some("/sdk/emulator/lib/emulator_access.json")
        );
        assert_eq!(inst.avd_name, "dev");
    }

    #[test]
    fn ad_file_path_format() {
        let p = ad_file_path(777);
        assert!(p.to_string_lossy().ends_with("avd/running/pid_777.ini"));
    }

    // 审计 #10：PID 复用误杀防护验证
    #[test]
    fn rejects_unrelated_pid() {
        let sdk = PathBuf::from("/nonexistent-sdk");
        // 测试进程自己的 exe 不在任何 SDK 内 → 拒绝
        assert!(!verify_emulator_pid(std::process::id(), &sdk));
        // 不存在进程 → 拒绝
        assert!(!verify_emulator_pid(u32::MAX, &sdk));
    }

    #[test]
    fn rejects_pid_outside_sdk_even_if_alive() {
        // 用真实存活进程（自身），但 SDK 指向 /nonexistent → exe 匹配失败即拒
        assert!(!verify_emulator_pid(
            std::process::id(),
            &PathBuf::from("/proc/self/..")
        ));
    }

    #[test]
    fn rejects_reused_pid_even_when_unrelated_executable_is_inside_root() {
        let executable = std::fs::read_link("/proc/self/exe").unwrap();
        let root = executable.parent().unwrap();
        assert!(executable.starts_with(root));
        assert!(!verify_emulator_pid(std::process::id(), root));
    }

    #[test]
    fn dropping_pending_launch_kills_and_reaps_launcher() {
        let child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = child.id();
        let pending = PendingLaunch::new(child, PathBuf::from("/nonexistent-sdk"));
        assert!(process_alive(pid));
        drop(pending);
        assert!(!process_alive(pid));
    }

    #[tokio::test]
    async fn stopping_an_already_gone_process_is_idempotent() {
        assert!(stop(u32::MAX, Path::new("/nonexistent-sdk")).await.is_ok());
    }

    #[test]
    #[ignore = "只由 stop_escalates_to_sigkill 启动为辅助进程"]
    fn term_ignoring_helper_process() {
        let Some(ready) = std::env::var_os("LITEAVD_TERM_HELPER_READY") else {
            return;
        };
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        std::fs::write(ready, b"ready").unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    #[tokio::test]
    async fn stop_escalates_to_sigkill_for_verified_process() {
        let root =
            std::env::temp_dir().join(format!("liteavd-stop-escalation-{}", std::process::id()));
        let sdk_root = root.join("sdk");
        let emulator_dir = sdk_root.join("emulator");
        std::fs::create_dir_all(&emulator_dir).unwrap();
        let executable = emulator_dir.join("emulator");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let ready = root.join("ready");
        let stale_share_vid = root.join("stale-share-vid");
        std::fs::write(&stale_share_vid, b"stale frame").unwrap();
        let mut child = std::process::Command::new(&executable)
            .arg("--exact")
            .arg("core::emulator::tests::term_ignoring_helper_process")
            .arg("--ignored")
            .arg("--test-threads=1")
            .env("LITEAVD_TERM_HELPER_READY", &ready)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&root);
                panic!("辅助进程未就绪");
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let stop_result = stop_process_with_timeouts(
            child.id(),
            &sdk_root,
            false,
            Some(&stale_share_vid),
            Duration::from_millis(100),
            Duration::from_secs(1),
        )
        .await;
        if let Err(error) = stop_result {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&root);
            panic!("停止辅助进程失败：{error:#}");
        }
        let status = child.wait().unwrap();
        assert!(!status.success(), "忽略 SIGTERM 的进程必须由 SIGKILL 结束");
        assert!(
            !stale_share_vid.exists(),
            "确认进程退出后必须删除对应的 share-vid 残留"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn running(avd_name: &str, console_port: u16) -> RunningInstance {
        RunningInstance {
            pid: 123,
            ini_path: PathBuf::from("/tmp/pid_123.ini"),
            avd_name: avd_name.into(),
            console_port,
            adb_port: console_port + 1,
            grpc_port: 8554,
            grpc_allowlist: None,
            grpc_jwks: None,
            grpc_jwk_active: None,
        }
    }

    #[test]
    fn launch_slot_requires_recommended_even_port() {
        assert!(validate_launch_slot("pixel", 5553, 8553, &[]).is_err());
        assert!(validate_launch_slot("pixel", 5555, 8555, &[]).is_err());
        assert!(validate_launch_slot("pixel", 5588, 8588, &[]).is_err());
        assert!(validate_launch_slot("pixel", 5554, 8554, &[]).is_ok());
    }

    #[test]
    fn launch_slot_rejects_duplicate_avd_or_port() {
        let instances = [running("existing", 5554)];
        assert!(validate_launch_slot("existing", 5556, 8556, &instances).is_err());
        assert!(validate_launch_slot("other", 5554, 8556, &instances).is_err());
        assert!(validate_launch_slot("other", 5556, 8554, &instances).is_err());
        assert!(validate_launch_slot("other", 5556, 8556, &instances).is_ok());
    }

    #[test]
    fn stale_share_vid_is_removed_before_port_reuse() {
        let root =
            std::env::temp_dir().join(format!("liteavd-stale-share-vid-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("videmulator5554");
        std::fs::write(&path, b"stale frame").unwrap();
        remove_stale_share_vid(&path).unwrap();
        assert!(!path.exists());
        remove_stale_share_vid(&path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
