//! WP-3.5：三 managed AVD 的 focused-only 音频切换短门禁。
//!
//! 快速门禁：
//! `AVDM_SDK_ROOT=/path/to/test-sdk cargo test --test audio_focus_multi_real -- --ignored --nocapture --test-threads=1`
//! 正式资源门禁增加 `LITEAVD_AUDIO_SOAK_SECONDS=1800`；它会使用确定性 tone
//! fixture 并把 CPAL 显式绑定到临时 null sink。desktop-host 复验再加
//! `LITEAVD_TEST_GPU_POLICY=desktop-host`。
//! 人工无重叠听辨可增加 `LITEAVD_AUDIO_MANUAL_FOCUS_SECONDS=6`；三台临时
//! guest 会分别使用 fixture `AudioTrack` 生成 440/660/880Hz，并在每次切换后停留。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use liteavd::core::audio::validate_packet;
use liteavd::core::avd::{self, AvdSpec, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::instance::DeviceRuntime;
use liteavd::core::operation::{OperationKind, OperationResult, OperationSuccess, execute_stop};
use liteavd::core::repo::{Archive, SystemImage};
use liteavd::core::workspace::OperationScope;
use liteavd::ui::audio::{AudioController, AudioStatus};

const SOAK_FOCUS_INTERVAL: Duration = Duration::from_secs(30);
const SOAK_PROGRESS_INTERVAL: Duration = Duration::from_secs(60);
const MAX_RSS_GROWTH_BYTES: u64 = 64 * 1024 * 1024;
const MAX_THREAD_GROWTH: usize = 4;
const MAX_FD_GROWTH: usize = 16;
const BUFFER_CAPACITY_SAMPLES: usize = 48_000 * 2 * 120 / 1_000;
const MANUAL_TONE_FREQUENCIES: [u32; 3] = [440, 660, 880];
const AUDIO_FIXTURE_PACKAGE: &str = "io.github.ydog12138.liteavd.audiofixture";

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: this ignored integration binary has one test and restores the value.
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
            let _ =
                std::fs::remove_file(liteavd::core::stream::share_vid_path(instance.console_port));
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

fn adb_shell(sdk_root: &Path, serial: &str, args: &[&str]) -> std::process::Output {
    Command::new(sdk_root.join("platform-tools/adb"))
        .arg("-s")
        .arg(serial)
        .arg("shell")
        .args(args)
        .output()
        .expect("执行测试 adb shell 失败")
}

fn schedule_timer(sdk_root: &Path, console_port: u16) {
    let serial = format!("emulator-{console_port}");
    let mut failures = Vec::new();
    for attempt in 1..=5 {
        let output = adb_shell(
            sdk_root,
            &serial,
            &[
                "am",
                "start",
                "-a",
                "android.intent.action.SET_TIMER",
                "--ei",
                "android.intent.extra.alarm.LENGTH",
                "2",
                "--ez",
                "android.intent.extra.alarm.SKIP_UI",
                "true",
            ],
        );
        if output.status.success() {
            return;
        }
        failures.push(format!(
            "attempt={attempt}, status={}, stdout={:?}, stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ));
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!(
        "guest 定时器音源启动失败（{serial}）：{}",
        failures.join("; ")
    );
}

fn stop_timer(sdk_root: &Path, console_port: u16) {
    for package in ["com.google.android.deskclock", "com.android.deskclock"] {
        let _ = adb_shell(
            sdk_root,
            &format!("emulator-{console_port}"),
            &["am", "force-stop", package],
        );
    }
}

struct DeterministicToneApps {
    sdk_root: PathBuf,
    consoles: Vec<u16>,
}

impl Drop for DeterministicToneApps {
    fn drop(&mut self) {
        for console in &self.consoles {
            let serial = format!("emulator-{console}");
            let _ = adb_shell(
                &self.sdk_root,
                &serial,
                &["am", "force-stop", AUDIO_FIXTURE_PACKAGE],
            );
            let _ = Command::new(self.sdk_root.join("platform-tools/adb"))
                .args(["-s", &serial, "uninstall", AUDIO_FIXTURE_PACKAGE])
                .output();
        }
    }
}

fn start_deterministic_tones(sdk_root: &Path, consoles: &[u16]) -> DeterministicToneApps {
    let adb = sdk_root.join("platform-tools/adb");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/audio/liteavd-audio-v1.apk");
    assert!(
        fixture.is_file(),
        "缺少 audio fixture：{}",
        fixture.display()
    );
    for (console, frequency_hz) in consoles.iter().zip(MANUAL_TONE_FREQUENCIES) {
        let serial = format!("emulator-{console}");
        let install = Command::new(&adb)
            .args(["-s", &serial, "install", "-r", "-t"])
            .arg(&fixture)
            .output()
            .expect("安装 audio fixture 失败");
        assert!(
            install.status.success(),
            "安装 {serial} audio fixture 失败：stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&install.stdout),
            String::from_utf8_lossy(&install.stderr)
        );
        let volume = adb_shell(
            sdk_root,
            &serial,
            &[
                "cmd",
                "media_session",
                "volume",
                "--stream",
                "3",
                "--set",
                "15",
            ],
        );
        assert!(
            volume.status.success(),
            "设置 {serial} MUSIC 音量失败：stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&volume.stdout),
            String::from_utf8_lossy(&volume.stderr)
        );
        let frequency = frequency_hz.to_string();
        let start = adb_shell(
            sdk_root,
            &serial,
            &[
                "am",
                "start",
                "-n",
                "io.github.ydog12138.liteavd.audiofixture/.MainActivity",
                "--ei",
                "frequency",
                &frequency,
            ],
        );
        assert!(
            start.status.success(),
            "启动 {serial} {frequency_hz}Hz fixture 失败：{}",
            String::from_utf8_lossy(&start.stderr)
        );
        let started = Instant::now();
        loop {
            let ready = adb_shell(
                sdk_root,
                &serial,
                &["run-as", AUDIO_FIXTURE_PACKAGE, "ls", "files/ready"],
            );
            if ready.status.success() {
                break;
            }
            let failure = adb_shell(
                sdk_root,
                &serial,
                &["run-as", AUDIO_FIXTURE_PACKAGE, "cat", "files/failure.txt"],
            );
            assert!(
                !failure.status.success(),
                "{serial} audio fixture 失败：{}",
                String::from_utf8_lossy(&failure.stdout)
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{serial} audio fixture 5 秒内未 ready"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("deterministic tone prepared: {serial} -> {frequency_hz}Hz");
    }
    DeterministicToneApps {
        sdk_root: sdk_root.to_path_buf(),
        consoles: consoles.to_vec(),
    }
}

struct PulseNullSink {
    module_id: u32,
    name: String,
    index: u32,
}

impl PulseNullSink {
    fn create(nonce: u128) -> Self {
        let name = format!("liteavd_audio_soak_{}_{}", std::process::id(), nonce);
        let output = Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                &format!("sink_name={name}"),
                &format!("sink_properties=device.description={name}"),
            ])
            .output()
            .expect("启动隔离 Pulse null sink 失败");
        assert!(
            output.status.success(),
            "启动隔离 Pulse null sink 失败：stdout={:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let module_id = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .expect("pactl 没有返回有效 module id");
        let sinks = Command::new("pactl")
            .args(["list", "short", "sinks"])
            .output()
            .expect("列出 Pulse sinks 失败");
        assert!(sinks.status.success(), "列出 Pulse sinks 失败");
        let index = String::from_utf8_lossy(&sinks.stdout)
            .lines()
            .find_map(|line| {
                let mut fields = line.split('\t');
                let index = fields.next()?;
                (fields.next()? == name).then(|| index.parse::<u32>().ok())?
            })
            .unwrap_or_else(|| panic!("未找到刚创建的 Pulse sink：{name}"));
        Self {
            module_id,
            name,
            index,
        }
    }

    fn wait_until_routed(&self, timeout: Duration) {
        let started = Instant::now();
        loop {
            if self.has_routed_input() {
                return;
            }
            assert!(
                started.elapsed() < timeout,
                "CPAL 在 {timeout:?} 内没有路由到隔离 sink {}",
                self.name
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn has_routed_input(&self) -> bool {
        let output = Command::new("pactl")
            .args(["list", "short", "sink-inputs"])
            .output()
            .expect("列出 Pulse sink inputs 失败");
        assert!(output.status.success(), "列出 Pulse sink inputs 失败");
        String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            line.split('\t')
                .nth(1)
                .and_then(|field| field.parse::<u32>().ok())
                == Some(self.index)
        })
    }
}

impl Drop for PulseNullSink {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.has_routed_input() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = Command::new("pactl")
            .args(["unload-module", &self.module_id.to_string()])
            .output();
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

fn wait_until_playing(
    controller: &Arc<AudioController>,
    avd_name: &str,
    timeout: Duration,
) -> Duration {
    let started = Instant::now();
    loop {
        controller.sync_focus();
        match controller.status() {
            AudioStatus::Playing {
                avd_name: active,
                stats,
            } if active == avd_name && stats.samples_played > 0 => return started.elapsed(),
            AudioStatus::Error {
                avd_name: active,
                message,
            } => panic!("{active} 音频输出失败：{message}"),
            _ => {}
        }
        assert!(
            started.elapsed() < timeout,
            "{avd_name} 在 {timeout:?} 内未恢复播放：{:?}",
            controller.status()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

async fn wait_for_active_guest_audio(runtime: &Arc<DeviceRuntime>, avd_name: &str) {
    let route = runtime
        .workspace_snapshot()
        .routes
        .into_iter()
        .find(|route| route.avd_name == avd_name)
        .expect("预热目标缺少 exact route");
    let client = runtime
        .grpc_client_for_route(&route)
        .expect("预热目标缺少认证 gRPC client")
        .reconnect()
        .await
        .expect("预热音频 gRPC 重连失败");
    let mut stream = client
        .stream_audio_output()
        .await
        .expect("预热 streamAudio 建链失败");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut bytes = 0_usize;
    let mut nonzero = 0_usize;
    while bytes < 48_000 * 2 * 2 * 60 / 1_000 || nonzero == 0 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "{avd_name} 5 秒内没有连续活跃 PCM");
        let packet = tokio::time::timeout(remaining, stream.message())
            .await
            .expect("预热 active PCM 超时")
            .expect("预热 streamAudio 接收失败")
            .expect("预热 streamAudio 意外结束");
        let packet = validate_packet(&packet).expect("预热 AudioPacket 格式无效");
        bytes += packet.s16le.len();
        nonzero += packet.s16le.iter().filter(|byte| **byte != 0).count();
    }
    // Emulator 37.1.11 can briefly starve a new stream while retiring this independent
    // probe. Let its HTTP/2 cancellation settle before timing the product's focus handoff;
    // production has no competing probe stream.
    drop(stream);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(runtime.route_is_current(&route));
}

#[test]
#[ignore = "需要隔离测试 SDK/system image、KVM、PulseAudio 与三组空闲端口"]
fn three_devices_follow_exact_focus_without_xvfb() {
    let sdk_root = PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT"));
    assert!(sdk_root.join("emulator/emulator").is_file());
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 UNIX epoch")
        .as_nanos();
    let avd_names: Vec<_> = (1..=3)
        .map(|index| {
            format!(
                "liteavd_audio_multi_{}_{}_{}",
                std::process::id(),
                nonce,
                index
            )
        })
        .collect();
    let avd_home = std::env::temp_dir().join(format!("liteavd-audio-multi-{nonce}"));
    std::fs::create_dir(&avd_home).expect("创建临时 AVD home 失败");
    let _avd_home = EnvGuard::set("ANDROID_AVD_HOME", &avd_home);
    let _emulator_ld = std::env::var_os("LITEAVD_EMULATOR_LD_LIBRARY_PATH")
        .map(|value| EnvGuard::set("LD_LIBRARY_PATH", value));
    let gpu_policy = match std::env::var("LITEAVD_TEST_GPU_POLICY").as_deref() {
        Ok("desktop-host") => ManagedGpuPolicy::DesktopHost,
        Ok(value) => panic!("未知 LITEAVD_TEST_GPU_POLICY={value}"),
        Err(std::env::VarError::NotPresent) => ManagedGpuPolicy::HeadlessSwangle,
        Err(error) => panic!("读取 LITEAVD_TEST_GPU_POLICY 失败：{error}"),
    };
    let manual_focus_seconds = std::env::var("LITEAVD_AUDIO_MANUAL_FOCUS_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let soak_seconds = std::env::var("LITEAVD_AUDIO_SOAK_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
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
        .expect("创建多设备 audio 测试 AVD 失败");
    }

    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("创建 Tokio runtime 失败");
    let runtime = Arc::new(DeviceRuntime::default());
    let mut consoles = Vec::new();
    for avd_name in &avd_names {
        let command = runtime.begin_start(avd_name).unwrap();
        let occupied = emulator::list_running_for_sdk(&sdk_root)
            .into_iter()
            .map(|instance| instance.console_port);
        let reservation = runtime.reserve_port(occupied).unwrap();
        let console_port = reservation.port();
        runtime.attach_start_port(&command, console_port).unwrap();
        let launched = tokio
            .block_on(emulator::launch(&LaunchParams {
                sdk_root: sdk_root.clone(),
                avd_name: avd_name.clone(),
                port: console_port,
                grpc: GrpcLaunchConfig::new(console_port + 3000).unwrap(),
                gpu_policy,
                audio_policy: ManagedAudioPolicy::Disabled,
                no_window: true,
                share_vid: false,
            }))
            .unwrap_or_else(|error| panic!("launch {avd_name} 失败：{error:#}"));
        cleanup.log_paths.push(launched.log_path().to_path_buf());
        runtime.mark_booting(&command).unwrap();
        tokio
            .block_on(liteavd::core::adb::wait_for_boot(
                &sdk_root,
                &format!("emulator-{console_port}"),
                Duration::from_secs(240),
            ))
            .unwrap_or_else(|error| panic!("等待 {avd_name} boot 失败：{error:#}"));
        runtime
            .complete_start(&command, launched, reservation)
            .unwrap();
        consoles.push(console_port);
    }

    let deterministic_tones = manual_focus_seconds > 0 || soak_seconds > 0;
    let tone_apps = if deterministic_tones {
        Some(start_deterministic_tones(&sdk_root, &consoles))
    } else {
        for console in &consoles {
            schedule_timer(&sdk_root, *console);
        }
        None
    };
    runtime.focus_session(&avd_names[0]).unwrap();
    let output_sink = (soak_seconds > 0).then(|| PulseNullSink::create(nonce));
    let controller = if let Some(output_sink) = &output_sink {
        AudioController::new_for_output_device_id(runtime.clone(), output_sink.name.clone())
    } else {
        AudioController::new(runtime.clone())
    };
    let first = wait_until_playing(&controller, &avd_names[0], Duration::from_secs(5));
    eprintln!("initial audio ready {}ms", first.as_millis());
    if let Some(output_sink) = &output_sink {
        output_sink.wait_until_routed(Duration::from_secs(2));
        eprintln!("CPAL routed to isolated sink {}", output_sink.name);
    }
    if manual_focus_seconds > 0 {
        eprintln!(
            "manual focus: {} ({}Hz) for {manual_focus_seconds}s",
            avd_names[0], MANUAL_TONE_FREQUENCIES[0]
        );
        std::thread::sleep(Duration::from_secs(manual_focus_seconds));
    }

    for (index, avd_name) in avd_names[1..].iter().enumerate() {
        tokio.block_on(wait_for_active_guest_audio(&runtime, avd_name));
        runtime.focus_session(avd_name).unwrap();
        let elapsed = wait_until_playing(&controller, avd_name, Duration::from_secs(5));
        eprintln!("focus handoff {avd_name} {}ms", elapsed.as_millis());
        assert!(elapsed <= Duration::from_millis(250));
        if manual_focus_seconds > 0 {
            let frequency = MANUAL_TONE_FREQUENCIES[index + 1];
            eprintln!("manual focus: {avd_name} ({frequency}Hz) for {manual_focus_seconds}s");
            std::thread::sleep(Duration::from_secs(manual_focus_seconds));
        }
    }

    // Stopping makes the exact route invalid before the engine exits; the next sync must
    // silence it immediately, then another live route can take over.
    let stopping_route = runtime.workspace_snapshot().focused.unwrap();
    let stopping = runtime.begin_stop_route(&stopping_route).unwrap();
    controller.sync_focus();
    assert!(matches!(controller.status(), AudioStatus::WaitingForFocus));
    runtime
        .fail_stop(&stopping, "fault injection keeps engine alive".into())
        .unwrap();
    runtime.focus_session(&avd_names[0]).unwrap();
    let recovered = wait_until_playing(&controller, &avd_names[0], Duration::from_millis(250));
    eprintln!("post-fault focus recovery {}ms", recovered.as_millis());
    assert!(recovered <= Duration::from_millis(250));
    if manual_focus_seconds > 0 {
        eprintln!(
            "manual focus after fault: {} ({}Hz) for {manual_focus_seconds}s",
            avd_names[0], MANUAL_TONE_FREQUENCIES[0]
        );
        std::thread::sleep(Duration::from_secs(manual_focus_seconds));
    }

    if soak_seconds > 0 {
        let resources_before = process_resources();
        let mut resources_max = resources_before;
        let soak_started = Instant::now();
        let soak_deadline = soak_started + Duration::from_secs(soak_seconds);
        let mut next_focus = Instant::now();
        let mut next_resource_sample = Instant::now();
        let mut next_progress = soak_started + SOAK_PROGRESS_INTERVAL;
        let mut focus_index = 0_usize;
        let mut handoffs = Vec::new();
        let mut samples_received = 0_u64;
        let mut samples_played = 0_u64;
        let mut samples_dropped = 0_u64;
        let mut contention_callbacks = 0_u64;

        while Instant::now() < soak_deadline {
            let now = Instant::now();
            if now >= next_focus {
                focus_index = (focus_index + 1) % avd_names.len();
                tokio.block_on(wait_for_active_guest_audio(
                    &runtime,
                    &avd_names[focus_index],
                ));
                runtime.focus_session(&avd_names[focus_index]).unwrap();
                let elapsed = wait_until_playing(
                    &controller,
                    &avd_names[focus_index],
                    Duration::from_millis(250),
                );
                assert!(elapsed <= Duration::from_millis(250));
                if let AudioStatus::Playing { stats, .. } = controller.status() {
                    assert!(
                        stats.queued_samples <= BUFFER_CAPACITY_SAMPLES,
                        "音频缓冲超过固定 120ms 容量：{stats:?}"
                    );
                    samples_received = samples_received.saturating_add(stats.samples_received);
                    samples_played = samples_played.saturating_add(stats.samples_played);
                    samples_dropped = samples_dropped.saturating_add(stats.samples_dropped);
                    contention_callbacks =
                        contention_callbacks.saturating_add(stats.contention_callbacks);
                }
                handoffs.push(elapsed);
                next_focus = Instant::now() + SOAK_FOCUS_INTERVAL;
            } else {
                controller.sync_focus();
            }

            if now >= next_resource_sample {
                let resources = process_resources();
                resources_max.rss_bytes = resources_max.rss_bytes.max(resources.rss_bytes);
                resources_max.threads = resources_max.threads.max(resources.threads);
                resources_max.file_descriptors = resources_max
                    .file_descriptors
                    .max(resources.file_descriptors);
                next_resource_sample = now + Duration::from_secs(1);
            }
            if now >= next_progress {
                eprintln!(
                    "audio soak progress: {}s/{soak_seconds}s, handoffs={}, current={:?}, max={resources_max:?}",
                    soak_started.elapsed().as_secs(),
                    handoffs.len(),
                    process_resources(),
                );
                next_progress += SOAK_PROGRESS_INTERVAL;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        let resources_after = process_resources();
        assert!(!handoffs.is_empty(), "audio soak 没有执行焦点切换");
        assert!(
            resources_max
                .rss_bytes
                .saturating_sub(resources_before.rss_bytes)
                <= MAX_RSS_GROWTH_BYTES,
            "audio soak RSS 增长超过 64MiB：before={resources_before:?}, max={resources_max:?}"
        );
        assert!(
            resources_max.threads <= resources_before.threads + MAX_THREAD_GROWTH,
            "audio soak 线程数无界增长：before={resources_before:?}, max={resources_max:?}"
        );
        assert!(
            resources_max.file_descriptors <= resources_before.file_descriptors + MAX_FD_GROWTH,
            "audio soak fd 数无界增长：before={resources_before:?}, max={resources_max:?}"
        );
        eprintln!(
            "audio soak={soak_seconds}s, handoffs={}, handoff_p95={}ms, resources_before={resources_before:?}, resources_max={resources_max:?}, resources_after={resources_after:?}, received={samples_received}, played={samples_played}, dropped={samples_dropped}, contention={contention_callbacks}",
            handoffs.len(),
            percentile_millis(&handoffs, 95),
        );
    }

    controller.set_enabled(false);
    drop(controller);
    drop(output_sink);
    drop(tone_apps);
    for console in &consoles {
        stop_timer(&sdk_root, *console);
    }
    let plan = runtime
        .plan_operation(OperationKind::Stop, OperationScope::AllRunning)
        .unwrap();
    let report = tokio
        .block_on(execute_stop(
            runtime.clone(),
            runtime.authorize_operation(plan).unwrap(),
            sdk_root.clone(),
        ))
        .expect("三设备 exact stop operation 失败");
    assert_eq!(report.devices.len(), 3);
    assert!(
        report.devices.iter().all(|device| {
            device.result == OperationResult::Succeeded(OperationSuccess::Stopped)
        })
    );
    for name in &avd_names {
        avd::delete_avd(name).expect("删除多设备 audio 测试 AVD 失败");
    }
}
