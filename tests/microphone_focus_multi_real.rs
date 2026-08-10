//! WP-3.7：三 managed AVD 的虚拟麦克风 exact-focus、故障取消与资源门禁。
//!
//! 快速门禁：
//! `AVDM_SDK_ROOT=/path/to/test-sdk cargo test --test microphone_focus_multi_real -- --ignored --nocapture --test-threads=1`
//! 正式 30 分钟门禁增加 `LITEAVD_MICROPHONE_SOAK_SECONDS=1800`；稳定的
//! host-GPU 组合再加 `LITEAVD_TEST_GPU_POLICY=desktop-host`。

use std::ffi::{OsStr, OsString};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use liteavd::core::avd::{self, AvdSpec, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc::GrpcClient;
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::instance::DeviceRuntime;
use liteavd::core::microphone::{
    MicrophoneCoordinator, MicrophoneEndpointDescriptor, MicrophonePumpExit, MicrophoneSource,
};
use liteavd::core::operation::{OperationKind, OperationResult, OperationSuccess, execute_stop};
use liteavd::core::repo::{Archive, SystemImage};
use liteavd::core::workspace::{OperationScope, WorkspaceRoute};

const SAMPLE_RATE: usize = 48_000;
const FOCUS_INTERVAL: Duration = Duration::from_secs(30);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(60);
const MAX_RSS_GROWTH_BYTES: u64 = 32 * 1024 * 1024;
const MAX_THREAD_GROWTH: usize = 4;
const MAX_FD_GROWTH: usize = 12;

type MicrophoneRun = tokio::task::JoinHandle<
    Result<MicrophonePumpExit, liteavd::core::microphone::MicrophoneRunError>,
>;
type ActiveRun = (tokio::sync::watch::Sender<bool>, MicrophoneRun);

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: this ignored integration binary contains one test and restores the value.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `EnvGuard::set`.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

struct Cleanup {
    avd_names: Vec<String>,
    sdk_root: PathBuf,
    avd_home: PathBuf,
    log_paths: Vec<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for instance in emulator::list_running_for_sdk(&self.sdk_root)
            .into_iter()
            .filter(|instance| self.avd_names.contains(&instance.avd_name))
        {
            if emulator::verify_emulator_pid(instance.pid, &self.sdk_root) {
                // SAFETY: identity is verified against this isolated SDK and unique AVD set.
                unsafe { libc::kill(instance.pid as i32, libc::SIGTERM) };
            }
            for _ in 0..100 {
                if !Path::new(&format!("/proc/{}", instance.pid)).exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if emulator::verify_emulator_pid(instance.pid, &self.sdk_root) {
                // SAFETY: same verified test process, escalation only during cleanup.
                unsafe { libc::kill(instance.pid as i32, libc::SIGKILL) };
            }
        }
        for name in &self.avd_names {
            let _ = avd::delete_avd(name);
        }
        for path in &self.log_paths {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(path.with_extension("log.previous"));
        }
        let _ = std::fs::remove_dir_all(&self.avd_home);
    }
}

#[derive(Debug, Clone, Copy)]
struct ProcessResources {
    rss_bytes: u64,
    threads: usize,
    file_descriptors: usize,
}

fn process_resources() -> ProcessResources {
    let rss_bytes = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                let kib = line.strip_prefix("VmRSS:")?.split_whitespace().next()?;
                kib.parse::<u64>()
                    .ok()
                    .and_then(|kib| kib.checked_mul(1024))
            })
        })
        .unwrap_or(0);
    ProcessResources {
        rss_bytes,
        threads: directory_entry_count("/proc/self/task"),
        file_descriptors: directory_entry_count("/proc/self/fd"),
    }
}

fn directory_entry_count(path: &str) -> usize {
    std::fs::read_dir(path)
        .map(|entries| entries.flatten().count())
        .unwrap_or(0)
}

fn percentile_millis(samples: &[Duration], percentile: usize) -> u128 {
    let mut millis: Vec<_> = samples.iter().map(Duration::as_millis).collect();
    millis.sort_unstable();
    let rank = (millis.len() * percentile).div_ceil(100).max(1);
    millis[rank - 1]
}

fn installed_image(root: &Path) -> SystemImage {
    for api in std::fs::read_dir(root.join("system-images"))
        .expect("SDK 缺少 system-images")
        .flatten()
    {
        for tag in std::fs::read_dir(api.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            for abi in std::fs::read_dir(tag.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                if abi.path().join("system.img").is_file() {
                    return SystemImage {
                        api: api.file_name().to_string_lossy().into_owned(),
                        tag: tag.file_name().to_string_lossy().into_owned(),
                        abi: abi.file_name().to_string_lossy().into_owned(),
                        display_name: String::new(),
                        license_ids: vec![],
                        archive: Archive {
                            url: String::new(),
                            size: 0,
                            checksum: None,
                            host_os: None,
                            host_arch: None,
                        },
                    };
                }
            }
        }
    }
    panic!("SDK 中未找到系统镜像");
}

fn write_tone_wav(path: &Path, frequency: f64, seconds: usize) {
    let sample_count = SAMPLE_RATE * seconds;
    let data_bytes = u32::try_from(sample_count * 2).expect("WAV fixture 超过 RIFF 上限");
    let file = std::fs::File::create(path).expect("创建 WAV fixture 失败");
    let mut wav = BufWriter::new(file);
    wav.write_all(b"RIFF").unwrap();
    wav.write_all(&(36 + data_bytes).to_le_bytes()).unwrap();
    wav.write_all(b"WAVEfmt ").unwrap();
    wav.write_all(&16_u32.to_le_bytes()).unwrap();
    wav.write_all(&1_u16.to_le_bytes()).unwrap();
    wav.write_all(&1_u16.to_le_bytes()).unwrap();
    wav.write_all(&(SAMPLE_RATE as u32).to_le_bytes()).unwrap();
    wav.write_all(&((SAMPLE_RATE * 2) as u32).to_le_bytes())
        .unwrap();
    wav.write_all(&2_u16.to_le_bytes()).unwrap();
    wav.write_all(&16_u16.to_le_bytes()).unwrap();
    wav.write_all(b"data").unwrap();
    wav.write_all(&data_bytes.to_le_bytes()).unwrap();
    for index in 0..sample_count {
        let phase = std::f64::consts::TAU * frequency * index as f64 / SAMPLE_RATE as f64;
        wav.write_all(&((phase.sin() * 12_000.0) as i16).to_le_bytes())
            .unwrap();
    }
    wav.flush().unwrap();
}

fn spawn_wav(
    coordinator: Arc<MicrophoneCoordinator>,
    runtime: Arc<DeviceRuntime>,
    route: WorkspaceRoute,
    path: PathBuf,
) -> ActiveRun {
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    let run = tokio::spawn(async move {
        coordinator
            .run(
                runtime,
                route,
                MicrophoneSource::Wav {
                    path,
                    paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                },
                receiver,
            )
            .await
    });
    (cancel, run)
}

async fn wait_for_exclusive_active(
    clients: &[GrpcClient],
    target: usize,
    timeout: Duration,
) -> Duration {
    let started = Instant::now();
    loop {
        let mut states = Vec::with_capacity(clients.len());
        for client in clients {
            states.push(
                client
                    .microphone_state()
                    .await
                    .expect("查询多设备 microphone state 失败"),
            );
        }
        assert!(
            states.iter().filter(|enabled| **enabled).count() <= 1,
            "检测到多个 guest 同时启用虚拟麦克风：{states:?}"
        );
        if states.get(target) == Some(&true) {
            return started.elapsed();
        }
        assert!(
            started.elapsed() < timeout,
            "目标设备 {target} 在 {timeout:?} 内未启用：{states:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn wait_for_all_disabled(clients: &[GrpcClient], timeout: Duration) {
    let started = Instant::now();
    loop {
        let mut states = Vec::with_capacity(clients.len());
        for client in clients {
            states.push(
                client
                    .microphone_state()
                    .await
                    .expect("查询多设备 microphone state 失败"),
            );
        }
        if states.iter().all(|enabled| !enabled) {
            return;
        }
        assert!(
            started.elapsed() < timeout,
            "等待全部 microphone 关闭超时：{states:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn expect_canceled(run: MicrophoneRun, context: &str) {
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), run)
            .await
            .unwrap_or_else(|_| panic!("{context} 后 pump 未在 3 秒内退出"))
            .unwrap_or_else(|error| panic!("{context} pump task panic：{error}"))
            .unwrap_or_else(|error| panic!("{context} coordinator 失败：{error}")),
        MicrophonePumpExit::Canceled
    );
}

fn endpoint_cleanup_complete(endpoints: &[MicrophoneEndpointDescriptor]) -> bool {
    let modules = std::process::Command::new("pactl")
        .args(["list", "short", "modules"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    endpoints.iter().all(|endpoint| {
        !endpoint.fifo_path.exists()
            && !modules.contains(&endpoint.pulse_source)
            && !modules.contains(&endpoint.pulse_sink)
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要隔离测试 SDK/system image、KVM、PulseAudio 与三组空闲端口"]
async fn three_devices_keep_microphone_exact_during_focus_faults_and_soak() {
    let sdk_root = PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT"));
    assert!(sdk_root.join("emulator/emulator").is_file());
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 UNIX epoch")
        .as_nanos();
    let avd_names: Vec<_> = (1..=3)
        .map(|index| {
            format!(
                "liteavd_microphone_multi_{}_{}_{}",
                std::process::id(),
                nonce,
                index
            )
        })
        .collect();
    let avd_home = std::env::temp_dir().join(format!("liteavd-microphone-multi-{nonce}"));
    std::fs::create_dir(&avd_home).expect("创建临时 AVD home 失败");
    let _avd_home = EnvGuard::set("ANDROID_AVD_HOME", &avd_home);
    let pulse_home = avd_home.join("pulse-home");
    let pulse_config = pulse_home.join(".config/pulse");
    std::fs::create_dir_all(&pulse_config).expect("创建隔离 Pulse home 失败");
    let cookie = pulse_config.join("cookie");
    std::fs::write(&cookie, [0_u8; 256]).expect("写入隔离 Pulse cookie 失败");
    std::fs::set_permissions(&cookie, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("设置隔离 Pulse cookie 权限失败");
    let _pulse_home = EnvGuard::set("HOME", &pulse_home);
    let _emulator_ld = std::env::var_os("LITEAVD_EMULATOR_LD_LIBRARY_PATH")
        .map(|value| EnvGuard::set("LD_LIBRARY_PATH", value));
    let gpu_policy = match std::env::var("LITEAVD_TEST_GPU_POLICY").as_deref() {
        Ok("desktop-host") => ManagedGpuPolicy::DesktopHost,
        Ok(value) => panic!("未知 LITEAVD_TEST_GPU_POLICY={value}"),
        Err(std::env::VarError::NotPresent) => ManagedGpuPolicy::HeadlessSwangle,
        Err(error) => panic!("读取 LITEAVD_TEST_GPU_POLICY 失败：{error}"),
    };
    let wav_paths: Vec<_> = [700.0, 1_000.0, 1_300.0]
        .into_iter()
        .enumerate()
        .map(|(index, frequency)| {
            let path = avd_home.join(format!("tone-{index}.wav"));
            write_tone_wav(&path, frequency, 40);
            path
        })
        .collect();
    let mut cleanup = Cleanup {
        avd_names: avd_names.clone(),
        sdk_root: sdk_root.clone(),
        avd_home,
        log_paths: Vec::new(),
    };

    let image = installed_image(&sdk_root);
    for name in &avd_names {
        avd::create_avd(&AvdSpec {
            name: name.clone(),
            device: avd::get_profile("pixel_2").expect("缺少 pixel_2 profile"),
            image: image.clone(),
            ram_mb: 1536,
            data_partition_mb: 4096,
            sdcard: None,
            gpu: gpu_policy.gpu_mode(),
        })
        .expect("创建多设备 microphone 测试 AVD 失败");
    }

    let runtime = Arc::new(DeviceRuntime::default());
    let mut endpoints = Vec::new();
    for avd_name in &avd_names {
        let command = runtime.begin_start(avd_name).unwrap();
        let occupied = emulator::list_running_for_sdk(&sdk_root)
            .into_iter()
            .map(|instance| instance.console_port);
        let reservation = runtime.reserve_port(occupied).unwrap();
        let console_port = reservation.port();
        runtime.attach_start_port(&command, console_port).unwrap();
        let launched = emulator::launch(&LaunchParams {
            sdk_root: sdk_root.clone(),
            avd_name: avd_name.clone(),
            port: console_port,
            grpc: GrpcLaunchConfig::new(console_port + 3000).unwrap(),
            gpu_policy,
            audio_policy: ManagedAudioPolicy::VirtualMicrophone { required: true },
            no_window: true,
            share_vid: false,
        })
        .await
        .unwrap_or_else(|error| panic!("launch {avd_name} 失败：{error:#}"));
        endpoints.push(
            launched
                .microphone_endpoint()
                .expect("required microphone endpoint"),
        );
        cleanup.log_paths.push(launched.log_path().to_path_buf());
        runtime.mark_booting(&command).unwrap();
        liteavd::core::adb::wait_for_boot(
            &sdk_root,
            &format!("emulator-{console_port}"),
            Duration::from_secs(240),
        )
        .await
        .unwrap_or_else(|error| panic!("等待 {avd_name} boot 失败：{error:#}"));
        runtime
            .complete_start(&command, launched, reservation)
            .unwrap();
    }

    let mut routes = Vec::new();
    let mut clients = Vec::new();
    for avd_name in &avd_names {
        let route = runtime.focus_session(avd_name).unwrap();
        clients.push(
            runtime
                .grpc_client_for_route(&route)
                .unwrap()
                .reconnect()
                .await
                .unwrap(),
        );
        routes.push(route);
    }
    wait_for_all_disabled(&clients, Duration::from_secs(3)).await;
    let coordinator = Arc::new(MicrophoneCoordinator::default());

    runtime.focus_session(&avd_names[0]).unwrap();
    let (_first_cancel, first_run) = spawn_wav(
        coordinator.clone(),
        runtime.clone(),
        routes[0].clone(),
        wav_paths[0].clone(),
    );
    wait_for_exclusive_active(&clients, 0, Duration::from_secs(5)).await;

    let handoff_started = Instant::now();
    runtime.focus_session(&avd_names[1]).unwrap();
    let (_second_cancel, second_run) = spawn_wav(
        coordinator.clone(),
        runtime.clone(),
        routes[1].clone(),
        wav_paths[1].clone(),
    );
    expect_canceled(first_run, "focus handoff").await;
    let handoff = wait_for_exclusive_active(&clients, 1, Duration::from_secs(5)).await;
    eprintln!(
        "microphone focus handoff={}ms (state-wait={}ms)",
        handoff_started.elapsed().as_millis(),
        handoff.as_millis()
    );

    assert!(runtime.request_control_stream_reset(&routes[1]));
    expect_canceled(second_run, "control revision reset").await;
    wait_for_all_disabled(&clients, Duration::from_secs(3)).await;

    let (explicit_cancel, explicit_run) = spawn_wav(
        coordinator.clone(),
        runtime.clone(),
        routes[1].clone(),
        wav_paths[1].clone(),
    );
    wait_for_exclusive_active(&clients, 1, Duration::from_secs(5)).await;
    explicit_cancel.send(true).unwrap();
    expect_canceled(explicit_run, "explicit cancel").await;
    wait_for_all_disabled(&clients, Duration::from_secs(3)).await;

    runtime.focus_session(&avd_names[2]).unwrap();
    let (_stop_cancel, stop_run) = spawn_wav(
        coordinator.clone(),
        runtime.clone(),
        routes[2].clone(),
        wav_paths[2].clone(),
    );
    wait_for_exclusive_active(&clients, 2, Duration::from_secs(5)).await;
    let failed_stop = runtime.begin_stop_route(&routes[2]).unwrap();
    expect_canceled(stop_run, "stop-in-flight").await;
    wait_for_all_disabled(&clients, Duration::from_secs(3)).await;
    runtime
        .fail_stop(&failed_stop, "fault injection keeps engine alive".into())
        .unwrap();

    // A survivor must still accept an exact source after another device's stop failed.
    routes[0] = runtime.focus_session(&avd_names[0]).unwrap();
    let (survivor_cancel, survivor_run) = spawn_wav(
        coordinator.clone(),
        runtime.clone(),
        routes[0].clone(),
        wav_paths[0].clone(),
    );
    wait_for_exclusive_active(&clients, 0, Duration::from_secs(5)).await;
    survivor_cancel.send(true).unwrap();
    expect_canceled(survivor_run, "post-fault survivor cancel").await;
    wait_for_all_disabled(&clients, Duration::from_secs(3)).await;

    let soak_seconds = std::env::var("LITEAVD_MICROPHONE_SOAK_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if soak_seconds > 0 {
        let resources_before = process_resources();
        let mut resources_max = resources_before;
        let soak_started = Instant::now();
        let soak_deadline = soak_started + Duration::from_secs(soak_seconds);
        let mut next_focus = Instant::now();
        let mut next_resource_sample = Instant::now();
        let mut next_progress = soak_started + PROGRESS_INTERVAL;
        let mut focus_index = avd_names.len() - 1;
        let mut active: Option<ActiveRun> = None;
        let mut handoffs = Vec::new();

        while Instant::now() < soak_deadline {
            let now = Instant::now();
            if now >= next_focus {
                focus_index = (focus_index + 1) % avd_names.len();
                routes[focus_index] = runtime.focus_session(&avd_names[focus_index]).unwrap();
                let replacement = spawn_wav(
                    coordinator.clone(),
                    runtime.clone(),
                    routes[focus_index].clone(),
                    wav_paths[focus_index].clone(),
                );
                if let Some((_cancel, previous)) = active.take() {
                    expect_canceled(previous, "soak focus handoff").await;
                }
                let elapsed =
                    wait_for_exclusive_active(&clients, focus_index, Duration::from_secs(5)).await;
                handoffs.push(elapsed);
                active = Some(replacement);
                next_focus = Instant::now() + FOCUS_INTERVAL;
            }

            if now >= next_resource_sample {
                let resources = process_resources();
                resources_max.rss_bytes = resources_max.rss_bytes.max(resources.rss_bytes);
                resources_max.threads = resources_max.threads.max(resources.threads);
                resources_max.file_descriptors = resources_max
                    .file_descriptors
                    .max(resources.file_descriptors);
                assert_eq!(
                    endpoints
                        .iter()
                        .filter(|endpoint| endpoint.fifo_path.exists())
                        .count(),
                    endpoints.len(),
                    "soak 期间 microphone endpoint 数量变化"
                );
                next_resource_sample = now + Duration::from_secs(1);
            }
            if now >= next_progress {
                eprintln!(
                    "microphone soak progress: {}s/{soak_seconds}s, handoffs={}, current={:?}, max={resources_max:?}",
                    soak_started.elapsed().as_secs(),
                    handoffs.len(),
                    process_resources(),
                );
                next_progress += PROGRESS_INTERVAL;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        if let Some((cancel, run)) = active {
            let _ = cancel.send(true);
            expect_canceled(run, "soak shutdown").await;
        }
        wait_for_all_disabled(&clients, Duration::from_secs(3)).await;
        let resources_after = process_resources();
        assert!(!handoffs.is_empty(), "microphone soak 没有执行焦点切换");
        assert!(
            resources_max
                .rss_bytes
                .saturating_sub(resources_before.rss_bytes)
                <= MAX_RSS_GROWTH_BYTES,
            "microphone soak RSS 增长超过 32MiB：before={resources_before:?}, max={resources_max:?}"
        );
        assert!(
            resources_max.threads <= resources_before.threads + MAX_THREAD_GROWTH,
            "microphone soak 线程数无界增长：before={resources_before:?}, max={resources_max:?}"
        );
        assert!(
            resources_max.file_descriptors <= resources_before.file_descriptors + MAX_FD_GROWTH,
            "microphone soak fd 数无界增长：before={resources_before:?}, max={resources_max:?}"
        );
        eprintln!(
            "microphone soak={soak_seconds}s, handoffs={}, handoff_p95={}ms, resources_before={resources_before:?}, resources_max={resources_max:?}, resources_after={resources_after:?}",
            handoffs.len(),
            percentile_millis(&handoffs, 95),
        );
    }

    let plan = runtime
        .plan_operation(OperationKind::Stop, OperationScope::AllRunning)
        .unwrap();
    let report = execute_stop(
        runtime.clone(),
        runtime.authorize_operation(plan).unwrap(),
        sdk_root.clone(),
    )
    .await
    .expect("三设备 exact stop operation 失败");
    assert_eq!(report.devices.len(), 3);
    assert!(
        report.devices.iter().all(|device| {
            device.result == OperationResult::Succeeded(OperationSuccess::Stopped)
        })
    );
    let cleanup_deadline = Instant::now() + Duration::from_secs(3);
    while !endpoint_cleanup_complete(&endpoints) {
        assert!(
            Instant::now() < cleanup_deadline,
            "三设备 Pulse endpoint/FIFO 清理超时：{endpoints:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for name in &avd_names {
        avd::delete_avd(name).expect("删除多设备 microphone 测试 AVD 失败");
    }
}
