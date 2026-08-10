//! WP-3.5：guest 同步音画事件经 production share-vid 与 CPAL/Pulse 输出后的偏差门禁。
//!
//! `AVDM_SDK_ROOT=/path/to/test-sdk cargo test --test audio_video_latency_real -- --ignored --nocapture --test-threads=1`
//! DesktopHost 复验增加 `LITEAVD_TEST_GPU_POLICY=desktop-host`。测试需要 KVM、
//! PipeWire/PulseAudio、`pactl`、`parec` 和一组空闲 console/gRPC 端口；输出只进入
//! session 唯一的临时 null sink，不会播放到宿主扬声器。

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use liteavd::core::avd::{self, AvdSpec, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::instance::DeviceRuntime;
use liteavd::core::operation::{OperationKind, OperationResult, OperationSuccess, execute_stop};
use liteavd::core::repo::{Archive, SystemImage};
use liteavd::core::stream::Frame;
use liteavd::core::workspace::OperationScope;
use liteavd::ui::audio::{AudioController, AudioStatus};

const AUDIO_FIXTURE_PACKAGE: &str = "io.github.ydog12138.liteavd.audiofixture";
const EVENT_PERIOD_MS: u32 = 1_200;
const REQUIRED_EVENTS: usize = 20;
const MAX_P95_SKEW: Duration = Duration::from_millis(180);
const MONITOR_FRAMES: usize = 960;

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
    avd_name: String,
    sdk_root: PathBuf,
    avd_home: PathBuf,
    console_port: Option<u16>,
    log_path: Option<PathBuf>,
    auth_dir: Option<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for instance in emulator::list_running_for_sdk(&self.sdk_root)
            .into_iter()
            .filter(|instance| instance.avd_name == self.avd_name)
        {
            if emulator::verify_emulator_pid(instance.pid, &self.sdk_root) {
                // SAFETY: identity is verified against this isolated SDK and unique test AVD.
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
        if let Some(port) = self.console_port {
            let _ = std::fs::remove_file(liteavd::core::stream::share_vid_path(port));
        }
        if let Some(path) = &self.log_path {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(path.with_extension("log.previous"));
        }
        if let Some(path) = &self.auth_dir {
            let _ = std::fs::remove_dir_all(path);
        }
        let _ = avd::delete_avd(&self.avd_name);
        let _ = std::fs::remove_dir_all(&self.avd_home);
    }
}

struct PulseNullSink {
    module_id: u32,
    name: String,
    index: u32,
}

impl PulseNullSink {
    fn create(nonce: u128) -> Self {
        let name = format!("liteavd_av_latency_{}_{}", std::process::id(), nonce);
        let output = Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                &format!("sink_name={name}"),
                &format!("sink_properties=device.description={name}"),
            ])
            .output()
            .expect("启动音画门禁 Pulse null sink 失败");
        assert!(
            output.status.success(),
            "启动音画门禁 Pulse null sink 失败：stdout={:?}, stderr={:?}",
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

    fn wait_until_routed(&self, timeout: Duration) {
        let started = Instant::now();
        while !self.has_routed_input() {
            assert!(
                started.elapsed() < timeout,
                "CPAL 在 {timeout:?} 内没有路由到隔离 sink {}",
                self.name
            );
            std::thread::sleep(Duration::from_millis(20));
        }
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
struct StateEvent {
    high: bool,
    at: Instant,
}

struct PulseMonitor {
    child: Child,
    events: mpsc::Receiver<StateEvent>,
    worker: Option<JoinHandle<()>>,
    stats: Arc<MonitorStats>,
}

#[derive(Default)]
struct MonitorStats {
    chunks: AtomicU64,
    classified: AtomicU64,
    max_abs_sample: AtomicU64,
}

impl PulseMonitor {
    fn start(sink_name: &str) -> Self {
        let mut child = Command::new("parec")
            .args([
                "--raw",
                &format!("--device={sink_name}.monitor"),
                "--format=s16le",
                "--rate=48000",
                "--channels=2",
                "--latency-msec=5",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("启动 Pulse sink monitor 失败");
        let mut stdout = child.stdout.take().expect("parec stdout 未捕获");
        let (sender, events) = mpsc::channel();
        let stats = Arc::new(MonitorStats::default());
        let worker_stats = stats.clone();
        let worker = std::thread::Builder::new()
            .name("liteavd-av-monitor".into())
            .spawn(move || {
                let mut bytes = vec![0_u8; MONITOR_FRAMES * 4];
                let mut stable = None;
                let mut candidate = None;
                let mut candidate_at = Instant::now();
                let mut candidate_count = 0_u8;
                while stdout.read_exact(&mut bytes).is_ok() {
                    worker_stats.chunks.fetch_add(1, Ordering::Relaxed);
                    let max_abs = bytes
                        .chunks_exact(2)
                        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs())
                        .max()
                        .unwrap_or(0);
                    worker_stats
                        .max_abs_sample
                        .fetch_max(u64::from(max_abs), Ordering::Relaxed);
                    let Some(high) = classify_tone(&bytes) else {
                        candidate = None;
                        candidate_count = 0;
                        continue;
                    };
                    worker_stats.classified.fetch_add(1, Ordering::Relaxed);
                    if stable == Some(high) {
                        candidate = None;
                        candidate_count = 0;
                        continue;
                    }
                    if candidate == Some(high) {
                        candidate_count += 1;
                    } else {
                        candidate = Some(high);
                        candidate_at = Instant::now();
                        candidate_count = 1;
                    }
                    if candidate_count >= 2 {
                        stable = Some(high);
                        if sender
                            .send(StateEvent {
                                high,
                                at: candidate_at,
                            })
                            .is_err()
                        {
                            return;
                        }
                        candidate = None;
                        candidate_count = 0;
                    }
                }
            })
            .expect("启动 Pulse monitor reader 失败");
        Self {
            child,
            events,
            worker: Some(worker),
            stats,
        }
    }

    fn stats(&self) -> (u64, u64, u64) {
        (
            self.stats.chunks.load(Ordering::Relaxed),
            self.stats.classified.load(Ordering::Relaxed),
            self.stats.max_abs_sample.load(Ordering::Relaxed),
        )
    }
}

impl Drop for PulseMonitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn classify_tone(bytes: &[u8]) -> Option<bool> {
    let samples: Vec<f64> = bytes
        .chunks_exact(4)
        .map(|frame| f64::from(i16::from_le_bytes([frame[0], frame[1]])))
        .collect();
    let mean_square =
        samples.iter().map(|sample| sample * sample).sum::<f64>() / samples.len().max(1) as f64;
    if mean_square < 16.0 {
        return None;
    }
    let low = tone_power(&samples, 440.0);
    let high = tone_power(&samples, 880.0);
    if high > low * 3.0 {
        Some(true)
    } else if low > high * 3.0 {
        Some(false)
    } else {
        None
    }
}

fn tone_power(samples: &[f64], frequency: f64) -> f64 {
    let omega = std::f64::consts::TAU * frequency / 48_000.0;
    let (mut real, mut imaginary) = (0.0, 0.0);
    for (index, sample) in samples.iter().enumerate() {
        let phase = omega * index as f64;
        real += sample * phase.cos();
        imaginary -= sample * phase.sin();
    }
    real * real + imaginary * imaginary
}

#[test]
fn pulse_monitor_classifier_distinguishes_low_amplitude_fixture_tones() {
    for (frequency, expected_high) in [(440.0, false), (880.0, true)] {
        let mut pcm = Vec::with_capacity(MONITOR_FRAMES * 4);
        for frame in 0..MONITOR_FRAMES {
            let phase = std::f64::consts::TAU * frequency * frame as f64 / 48_000.0;
            let sample = (phase.sin() * 100.0).round() as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        assert_eq!(classify_tone(&pcm), Some(expected_high));
    }
}

fn frame_state(frame: &Frame) -> Option<bool> {
    let points = [
        (frame.meta.width / 4, frame.meta.height / 2),
        (frame.meta.width * 3 / 4, frame.meta.height / 2),
    ];
    let mut red = 0_u32;
    let mut blue = 0_u32;
    for (x, y) in points {
        let offset = usize::try_from(y).ok()? * usize::try_from(frame.meta.stride).ok()?
            + usize::try_from(x).ok()? * 4;
        blue += u32::from(*frame.pixels.get(offset)?);
        red += u32::from(*frame.pixels.get(offset + 2)?);
    }
    if blue > red.saturating_mul(2) && blue > 180 {
        Some(true)
    } else if red > blue.saturating_mul(2) && red > 180 {
        Some(false)
    } else {
        None
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

fn percentile_millis(samples: &[Duration], percentile: usize) -> u128 {
    let mut millis: Vec<_> = samples.iter().map(Duration::as_millis).collect();
    millis.sort_unstable();
    let rank = (millis.len() * percentile).div_ceil(100).max(1);
    millis[rank - 1]
}

#[test]
#[ignore = "需要隔离测试 SDK/system image、KVM、PulseAudio、pactl/parec 与空闲端口"]
fn production_audio_video_event_skew_stays_below_budget() {
    let sdk_root = PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT"));
    assert!(sdk_root.join("emulator/emulator").is_file());
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 UNIX epoch")
        .as_nanos();
    let avd_name = format!("liteavd_av_latency_{}_{}", std::process::id(), nonce);
    let avd_home = std::env::temp_dir().join(format!("{avd_name}-home"));
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
    let mut cleanup = Cleanup {
        avd_name: avd_name.clone(),
        sdk_root: sdk_root.clone(),
        avd_home,
        console_port: None,
        log_path: None,
        auth_dir: None,
    };

    avd::create_avd(&AvdSpec {
        name: avd_name.clone(),
        device: avd::get_profile("pixel_2").expect("缺少 pixel_2 profile"),
        image: installed_image(&sdk_root),
        ram_mb: 1536,
        data_partition_mb: 4096,
        sdcard: None,
        gpu: gpu_policy.gpu_mode(),
    })
    .expect("创建音画延迟测试 AVD 失败");

    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("创建 Tokio runtime 失败");
    let runtime = Arc::new(DeviceRuntime::default());
    let command = runtime.begin_start(&avd_name).unwrap();
    let occupied = emulator::list_running_for_sdk(&sdk_root)
        .into_iter()
        .map(|instance| instance.console_port);
    let reservation = runtime
        .reserve_port(occupied)
        .expect("音画延迟测试没有空闲 console port");
    let console_port = reservation.port();
    cleanup.console_port = Some(console_port);
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
            share_vid: true,
        }))
        .unwrap_or_else(|error| panic!("音画延迟测试 launch 失败：{error:#}"));
    cleanup.log_path = Some(launched.log_path().to_path_buf());
    cleanup.auth_dir = Some(
        launched
            .grpc_auth()
            .allowlist_path()
            .parent()
            .expect("allowlist 缺少 session 目录")
            .to_path_buf(),
    );
    let mut capture = launched
        .capture_subscription()
        .expect("音画延迟测试缺少 production capture");
    runtime.mark_booting(&command).unwrap();
    tokio
        .block_on(liteavd::core::adb::wait_for_boot(
            &sdk_root,
            &format!("emulator-{console_port}"),
            Duration::from_secs(240),
        ))
        .unwrap_or_else(|error| panic!("音画延迟测试 boot 失败：{error:#}"));
    runtime
        .complete_start(&command, launched, reservation)
        .unwrap();
    runtime.focus_session(&avd_name).unwrap();

    let serial = format!("emulator-{console_port}");
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/audio/liteavd-audio-v1.apk");
    let install = Command::new(sdk_root.join("platform-tools/adb"))
        .args(["-s", &serial, "install", "-r", "-t"])
        .arg(&fixture)
        .output()
        .expect("安装音画 fixture 失败");
    assert!(
        install.status.success(),
        "安装音画 fixture 失败：stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&install.stdout),
        String::from_utf8_lossy(&install.stderr)
    );
    let volume = adb_shell(
        &sdk_root,
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
    assert!(volume.status.success(), "设置 fixture MUSIC 音量失败");

    let output_sink = PulseNullSink::create(nonce);
    let monitor = PulseMonitor::start(&output_sink.name);
    let controller =
        AudioController::new_for_output_device_id(runtime.clone(), output_sink.name.clone());
    controller.sync_focus();

    let period = EVENT_PERIOD_MS.to_string();
    let start = adb_shell(
        &sdk_root,
        &serial,
        &[
            "am",
            "start",
            "-n",
            "io.github.ydog12138.liteavd.audiofixture/.MainActivity",
            "--ei",
            "frequency",
            "440",
            "--ei",
            "period_ms",
            &period,
        ],
    );
    assert!(
        start.status.success(),
        "启动同步音画 fixture 失败：{}",
        String::from_utf8_lossy(&start.stderr)
    );

    let ready_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ready = adb_shell(
            &sdk_root,
            &serial,
            &["run-as", AUDIO_FIXTURE_PACKAGE, "ls", "files/ready"],
        );
        if ready.status.success() {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "audio fixture 5 秒内未 ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let initial_deadline = Instant::now() + Duration::from_secs(3);
    let mut audio_state = None;
    let mut video_state = None;
    while audio_state != Some(false) || video_state != Some(false) {
        controller.sync_focus();
        while let Ok(event) = monitor.events.try_recv() {
            audio_state = Some(event.high);
        }
        if let Some(frame) = capture.wait_timeout(Duration::from_millis(20))
            && let Some(state) = frame_state(&frame)
        {
            video_state = Some(state);
        }
        if matches!(controller.status(), AudioStatus::Error { .. }) {
            panic!("产品音频控制器失败：{:?}", controller.status());
        }
        assert!(
            Instant::now() < initial_deadline,
            "3 秒内未同时观察到初始 440Hz/红色状态：audio={audio_state:?}, video={video_state:?}, monitor={:?}, controller={:?}",
            monitor.stats(),
            controller.status()
        );
    }
    output_sink.wait_until_routed(Duration::from_secs(2));

    let event_deadline = Instant::now()
        + Duration::from_millis(u64::from(EVENT_PERIOD_MS) * (REQUIRED_EVENTS as u64 + 5));
    let mut audio_events = Vec::new();
    let mut video_events = Vec::new();
    while audio_events.len() < REQUIRED_EVENTS || video_events.len() < REQUIRED_EVENTS {
        controller.sync_focus();
        while let Ok(event) = monitor.events.try_recv() {
            if Some(event.high) != audio_state {
                audio_state = Some(event.high);
                audio_events.push(event);
            }
        }
        if let Some(frame) = capture.wait_timeout(Duration::from_millis(20))
            && let Some(high) = frame_state(&frame)
            && Some(high) != video_state
        {
            video_state = Some(high);
            video_events.push(StateEvent {
                high,
                at: frame.observed_at,
            });
        }
        assert!(
            Instant::now() < event_deadline,
            "同步音画事件不足：audio={}, video={}, controller={:?}",
            audio_events.len(),
            video_events.len(),
            controller.status()
        );
    }

    let mut skews = Vec::with_capacity(REQUIRED_EVENTS);
    let mut signed_skews_ms = Vec::with_capacity(REQUIRED_EVENTS);
    for index in 0..REQUIRED_EVENTS {
        let audio = audio_events[index];
        let video = video_events[index];
        let expected_high = index.is_multiple_of(2);
        assert_eq!(audio.high, expected_high, "audio event {index} 顺序错误");
        assert_eq!(video.high, expected_high, "video event {index} 顺序错误");
        skews.push(if audio.at >= video.at {
            let skew = audio.at.duration_since(video.at);
            signed_skews_ms.push(skew.as_millis() as i128);
            skew
        } else {
            let skew = video.at.duration_since(audio.at);
            signed_skews_ms.push(-(skew.as_millis() as i128));
            skew
        });
    }
    let p95_ms = percentile_millis(&skews, 95);
    let max_ms = skews.iter().max().expect("缺少音画 skew").as_millis();
    eprintln!(
        "audio/video events={}, skew_p95={}ms, skew_max={}ms, signed_ms={:?}",
        skews.len(),
        p95_ms,
        max_ms,
        signed_skews_ms
    );
    assert!(
        p95_ms <= MAX_P95_SKEW.as_millis(),
        "音画事件 skew p95 {p95_ms}ms 超过 {}ms",
        MAX_P95_SKEW.as_millis()
    );

    controller.set_enabled(false);
    drop(controller);
    drop(monitor);
    drop(output_sink);
    let _ = adb_shell(
        &sdk_root,
        &serial,
        &["am", "force-stop", AUDIO_FIXTURE_PACKAGE],
    );
    let _ = Command::new(sdk_root.join("platform-tools/adb"))
        .args(["-s", &serial, "uninstall", AUDIO_FIXTURE_PACKAGE])
        .output();
    let plan = runtime
        .plan_operation(OperationKind::Stop, OperationScope::Focused)
        .unwrap();
    let report = tokio
        .block_on(execute_stop(
            runtime.clone(),
            runtime.authorize_operation(plan).unwrap(),
            sdk_root.clone(),
        ))
        .expect("音画延迟 exact stop 失败");
    assert_eq!(
        report.devices[0].result,
        OperationResult::Succeeded(OperationSuccess::Stopped)
    );
    avd::delete_avd(&avd_name).expect("删除音画延迟测试 AVD 失败");
    cleanup.console_port = None;
}
