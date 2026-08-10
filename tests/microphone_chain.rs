//! WP-3.7 gate：exact Pulse FIFO source 必须抵达 guest `AudioRecord`。
//!
//! `AVDM_SDK_ROOT=/path/to/test-sdk cargo test --test microphone_chain -- --ignored --nocapture --test-threads=1`

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use liteavd::core::adb::{ApkInstallOptions, install_apks_cancellable};
use liteavd::core::avd::{self, AvdSpec, GpuMode, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc::GrpcClient;
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::instance::{DeviceRuntime, SessionOrigin};
use liteavd::core::microphone::{MicrophoneCoordinator, MicrophonePumpExit, MicrophoneSource};
use liteavd::core::repo::{Archive, SystemImage};

const PACKAGE: &str = "io.github.ydog12138.liteavd.microphone";
const SAMPLE_RATE: usize = 48_000;
const TONE_HZ: f64 = 1_000.0;
const TONE_AMPLITUDE: f64 = 12_000.0;

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
    avd_name: String,
    sdk_root: PathBuf,
    avd_home: PathBuf,
    console_port: Option<u16>,
    log_path: Option<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        // The AVD and data partition are unique to this test, so deleting the
        // AVD is sufficient. A synchronous `adb uninstall` can hang forever
        // when `LaunchedInstance` has already terminated the guest during
        // panic unwinding.
        for instance in emulator::list_running_for_sdk(&self.sdk_root)
            .into_iter()
            .filter(|instance| instance.avd_name == self.avd_name)
        {
            if emulator::verify_emulator_pid(instance.pid, &self.sdk_root) {
                // SAFETY: identity is verified against the isolated SDK and unique AVD.
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
        if let Some(path) = &self.log_path {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(path.with_extension("log.previous"));
        }
        let _ = avd::delete_avd(&self.avd_name);
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

fn adb_output(sdk_root: &Path, serial: &str, args: &[&str]) -> std::process::Output {
    Command::new(sdk_root.join("platform-tools/adb"))
        .arg("-s")
        .arg(serial)
        .args(args)
        .output()
        .expect("执行测试 adb 失败")
}

fn private_file(sdk_root: &Path, serial: &str, name: &str) -> std::process::Output {
    adb_output(
        sdk_root,
        serial,
        &[
            "exec-out",
            "run-as",
            PACKAGE,
            "cat",
            &format!("files/{name}"),
        ],
    )
}

async fn wait_for_private_file(
    sdk_root: &Path,
    serial: &str,
    name: &str,
    timeout: Duration,
) -> std::process::Output {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let output = private_file(sdk_root, serial, name);
        if output.status.success() {
            return output;
        }
        let failure = private_file(sdk_root, serial, "failure.txt");
        assert!(
            !failure.status.success(),
            "guest 录音 fixture 失败：{}",
            String::from_utf8_lossy(&failure.stdout)
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "等待 guest 文件 {name} 超时：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn write_tone_wav(path: &Path, seconds: usize) {
    let sample_count = SAMPLE_RATE * seconds;
    let data_bytes = (sample_count * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_bytes as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE as u32).to_le_bytes());
    wav.extend_from_slice(&((SAMPLE_RATE * 2) as u32).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for sample_index in 0..sample_count {
        let phase = std::f64::consts::TAU * TONE_HZ * sample_index as f64 / SAMPLE_RATE as f64;
        wav.extend_from_slice(&((phase.sin() * TONE_AMPLITUDE) as i16).to_le_bytes());
    }
    std::fs::write(path, wav).expect("创建确定性 WAV fixture 失败");
}

fn wait_for_pulse_source(name: &str, state: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let output = Command::new("pactl")
            .args(["list", "short", "sources"])
            .output()
            .expect("查询 Pulse source 失败");
        if String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.contains(name) && line.ends_with(state))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn wait_for_microphone_state(client: &GrpcClient, expected: bool, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match client.microphone_state().await {
            Ok(actual) if actual == expected => return,
            Ok(_) | Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(actual) => panic!("等待 microphone state={expected} 超时，最终状态为 {actual}"),
            Err(error) => panic!("等待 microphone state={expected} 超时，最终 RPC 失败：{error:#}"),
        }
    }
}

fn spectral_amplitude(samples: &[i16], frequency: f64) -> f64 {
    let mut real = 0.0;
    let mut imaginary = 0.0;
    for (index, sample) in samples.iter().enumerate() {
        let phase = std::f64::consts::TAU * frequency * index as f64 / SAMPLE_RATE as f64;
        real += *sample as f64 * phase.cos();
        imaginary -= *sample as f64 * phase.sin();
    }
    2.0 * real.hypot(imaginary) / samples.len() as f64
}

fn assert_contains_tone(bytes: &[u8]) {
    assert_eq!(bytes.len() % 2, 0, "guest PCM 不是完整 S16 sample");
    let samples: Vec<i16> = bytes
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
        .collect();
    assert!(samples.len() >= SAMPLE_RATE, "guest PCM 少于一秒");

    let window = 4_800;
    let mut best: f64 = 0.0;
    let mut best_rms: f64 = 0.0;
    let mut best_off_tone: f64 = 0.0;
    for candidate in samples.windows(window).step_by(window / 2) {
        let rms = (candidate
            .iter()
            .map(|sample| (*sample as f64).powi(2))
            .sum::<f64>()
            / window as f64)
            .sqrt();
        let tone = spectral_amplitude(candidate, TONE_HZ);
        if tone > best {
            best = tone;
            best_rms = rms;
            best_off_tone =
                spectral_amplitude(candidate, 700.0).max(spectral_amplitude(candidate, 1_300.0));
        }
    }
    eprintln!(
        "guest PCM bytes={} best_1khz={best:.1} rms={best_rms:.1} off_tone={best_off_tone:.1}",
        bytes.len()
    );
    assert!(best_rms > 500.0, "guest 录音中没有足够能量");
    assert!(best > best_rms * 0.8, "guest 录音没有稳定 1kHz 分量");
    assert!(best > best_off_tone * 5.0, "1kHz 分量未显著高于旁带");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要隔离测试 SDK/system image、KVM 与空闲 console/gRPC 端口"]
async fn routed_pulse_microphone_reaches_guest() {
    let sdk_root = PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT"));
    assert!(sdk_root.join("emulator/emulator").is_file());
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/microphone/liteavd-microphone-v1.apk");
    assert!(fixture.is_file(), "缺少 microphone APK fixture");

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 UNIX epoch")
        .as_nanos();
    let avd_name = format!("liteavd_microphone_{}_{}", std::process::id(), nonce);
    let avd_home = std::env::temp_dir().join(format!("{avd_name}-home"));
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
    let _pulse_log = EnvGuard::set("PULSE_LOG", "4");
    let wav_path = avd_home.join("tone.wav");
    write_tone_wav(&wav_path, 5);
    let mut cleanup = Cleanup {
        avd_name: avd_name.clone(),
        sdk_root: sdk_root.clone(),
        avd_home,
        console_port: None,
        log_path: None,
    };

    avd::create_avd(&AvdSpec {
        name: avd_name.clone(),
        device: avd::get_profile("pixel_2").expect("缺少 pixel_2 profile"),
        image: installed_image(&sdk_root),
        ram_mb: 1536,
        data_partition_mb: 4096,
        sdcard: None,
        gpu: GpuMode::SwangleIndirect,
    })
    .expect("创建 microphone 测试 AVD 失败");

    let runtime = Arc::new(DeviceRuntime::default());
    let command = runtime
        .begin_start(&avd_name)
        .expect("创建测试 start command");
    let occupied = emulator::list_running_for_sdk(&sdk_root)
        .into_iter()
        .map(|instance| instance.console_port);
    let reservation = runtime.reserve_port(occupied).expect("预留 console port");
    let console_port = reservation.port();
    runtime
        .attach_start_port(&command, console_port)
        .expect("绑定测试 console port");
    cleanup.console_port = Some(console_port);
    let launched = emulator::launch(&LaunchParams {
        sdk_root: sdk_root.clone(),
        avd_name: avd_name.clone(),
        port: console_port,
        grpc: GrpcLaunchConfig::new(console_port + 3000).unwrap(),
        gpu_policy: ManagedGpuPolicy::HeadlessSwangle,
        audio_policy: ManagedAudioPolicy::VirtualMicrophone { required: true },
        no_window: true,
        share_vid: true,
    })
    .await
    .unwrap_or_else(|error| panic!("microphone test launch 失败：{error:#}"));
    let log_path = launched.log_path().to_path_buf();
    cleanup.log_path = Some(log_path.clone());
    let endpoint = launched
        .microphone_endpoint()
        .expect("required microphone launch must retain endpoint");
    assert!(
        !launched
            .grpc_client()
            .microphone_state()
            .await
            .expect("查询初始 microphone state 失败"),
        "managed microphone 必须默认关闭"
    );
    runtime.mark_booting(&command).expect("标记测试 booting");
    let serial = format!("emulator-{console_port}");
    liteavd::core::adb::wait_for_boot(&sdk_root, &serial, Duration::from_secs(240))
        .await
        .unwrap_or_else(|error| panic!("microphone test boot 失败：{error:#}"));
    runtime
        .complete_start(&command, launched, reservation)
        .expect("提交测试 managed session");
    let route = runtime
        .focus_session(&avd_name)
        .expect("focus microphone test session");

    let install = install_apks_cancellable(
        &sdk_root,
        &serial,
        std::slice::from_ref(&fixture),
        ApkInstallOptions {
            allow_downgrade: false,
            grant_runtime_permissions: true,
        },
        || false,
    )
    .await
    .expect("安装 microphone fixture 失败");
    assert!(
        install.success(),
        "安装 fixture 失败：{}",
        install.failure_summary()
    );
    let start = adb_output(
        &sdk_root,
        &serial,
        &[
            "shell",
            "am",
            "start",
            "-n",
            &format!("{PACKAGE}/.MainActivity"),
        ],
    );
    assert!(
        start.status.success(),
        "启动 microphone fixture 失败：{}",
        String::from_utf8_lossy(&start.stderr)
    );
    wait_for_private_file(&sdk_root, &serial, "ready", Duration::from_secs(10)).await;
    let coordinator = Arc::new(MicrophoneCoordinator::default());
    let (_cancel, receiver) = tokio::sync::watch::channel(false);
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let run = tokio::spawn({
        let coordinator = coordinator.clone();
        let runtime = runtime.clone();
        let route = route.clone();
        let wav_path = wav_path.clone();
        async move {
            coordinator
                .run(
                    runtime,
                    route,
                    MicrophoneSource::Wav {
                        path: wav_path,
                        paused,
                    },
                    receiver,
                )
                .await
        }
    });
    assert!(
        wait_for_pulse_source(&endpoint.pulse_source, "RUNNING", Duration::from_secs(5)),
        "Emulator 未连接 exact Pulse source；日志：\n{}",
        std::fs::read_to_string(&log_path).unwrap_or_else(|error| error.to_string())
    );
    for args in [
        ["set-source-volume", endpoint.pulse_source.as_str(), "100%"],
        ["set-source-mute", endpoint.pulse_source.as_str(), "0"],
    ] {
        let output = Command::new("pactl")
            .args(args)
            .output()
            .expect("修正私有 Pulse source 状态失败");
        assert!(
            output.status.success(),
            "修正私有 Pulse source 状态失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        run.await
            .expect("microphone coordinator task panic")
            .expect("WAV coordinator 失败"),
        MicrophonePumpExit::EndOfFile
    );
    wait_for_private_file(&sdk_root, &serial, "done", Duration::from_secs(10)).await;
    let capture = private_file(&sdk_root, &serial, "capture.pcm");
    assert!(
        capture.status.success(),
        "读取 guest PCM 失败：{}",
        String::from_utf8_lossy(&capture.stderr)
    );
    assert_contains_tone(&capture.stdout);
    let client = runtime
        .grpc_client_for_route(&route)
        .expect("runtime 保留 authenticated client")
        .reconnect()
        .await
        .expect("重连 authenticated client");
    assert!(!client.microphone_state().await.unwrap());

    // Snapshot load and control disconnect keep the same session identity but invalidate
    // long-lived control streams. The revision must cancel the FIFO pump and restore the
    // default-off microphone state before a replacement stream can begin.
    let (_reset_cancel, reset_receiver) = tokio::sync::watch::channel(false);
    let reset_run = tokio::spawn({
        let coordinator = coordinator.clone();
        let runtime = runtime.clone();
        let route = route.clone();
        let wav_path = wav_path.clone();
        async move {
            coordinator
                .run(
                    runtime,
                    route,
                    MicrophoneSource::Wav {
                        path: wav_path,
                        paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    },
                    reset_receiver,
                )
                .await
        }
    });
    wait_for_microphone_state(&client, true, Duration::from_secs(3)).await;
    let revision = runtime.control_stream_revision();
    assert!(runtime.request_control_stream_reset(&route));
    assert_eq!(runtime.control_stream_revision(), revision + 1);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), reset_run)
            .await
            .expect("control revision 后 microphone pump 未及时退出")
            .expect("control revision microphone task panic")
            .expect("control revision microphone coordinator 失败"),
        MicrophonePumpExit::Canceled
    );
    wait_for_microphone_state(&client, false, Duration::from_secs(3)).await;
    drop(client);
    let uninstall = adb_output(&sdk_root, &serial, &["uninstall", PACKAGE]);
    assert!(uninstall.status.success(), "卸载 fixture 失败");

    let observed = emulator::list_running_for_sdk(&sdk_root)
        .into_iter()
        .find(|instance| instance.avd_name == avd_name && instance.console_port == console_port)
        .expect("应用重启前广告实例消失");
    drop(runtime);
    let runtime = Arc::new(DeviceRuntime::default());
    runtime.reconcile_running_for_sdk_with_demands(vec![observed], HashMap::new(), &sdk_root);
    let recovered_route = runtime
        .focus_session(&avd_name)
        .expect("恢复后 focus exact session");
    assert_eq!(
        runtime
            .session_for_route(&recovered_route)
            .expect("恢复 session snapshot")
            .origin,
        SessionOrigin::Recovered
    );
    assert_eq!(
        runtime
            .microphone_endpoint_for_route(&recovered_route)
            .expect("恢复 microphone endpoint"),
        endpoint
    );
    let recovered_client = runtime
        .grpc_client_for_route(&recovered_route)
        .expect("恢复 authenticated client")
        .reconnect()
        .await
        .expect("恢复后重连 gRPC");
    assert!(!recovered_client.microphone_state().await.unwrap());

    // `begin_stop_route` invalidates the exact route before the engine exits. Exercise a
    // failed stop first: the pump must cancel and disable its source, while the restored
    // session remains controllable for the final exact stop.
    let (_stop_cancel, stop_receiver) = tokio::sync::watch::channel(false);
    let stop_run = tokio::spawn({
        let coordinator = coordinator.clone();
        let runtime = runtime.clone();
        let route = recovered_route.clone();
        let wav_path = wav_path.clone();
        async move {
            coordinator
                .run(
                    runtime,
                    route,
                    MicrophoneSource::Wav {
                        path: wav_path,
                        paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    },
                    stop_receiver,
                )
                .await
        }
    });
    wait_for_microphone_state(&recovered_client, true, Duration::from_secs(3)).await;
    let failed_stop = runtime
        .begin_stop_route(&recovered_route)
        .expect("开始 recovered stop fault injection");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), stop_run)
            .await
            .expect("stop-in-flight 后 microphone pump 未及时退出")
            .expect("stop-in-flight microphone task panic")
            .expect("stop-in-flight microphone coordinator 失败"),
        MicrophonePumpExit::Canceled
    );
    wait_for_microphone_state(&recovered_client, false, Duration::from_secs(3)).await;
    runtime
        .fail_stop(&failed_stop, "fault injection keeps engine alive".into())
        .expect("恢复 failed stop session");
    let recovered_route = runtime
        .focus_session(&avd_name)
        .expect("failed stop 后恢复 focus");
    assert!(runtime.route_is_current(&recovered_route));
    assert!(!recovered_client.microphone_state().await.unwrap());
    drop(recovered_client);

    let stop = runtime
        .begin_stop_route(&recovered_route)
        .expect("开始最终 recovered exact stop");
    emulator::stop_instance(stop.instance(), &sdk_root)
        .await
        .expect("停止 recovered session 失败");
    runtime
        .complete_stop(&stop)
        .expect("提交 recovered exact stop");
    avd::delete_avd(&avd_name).expect("清理 AVD 失败");
    cleanup.console_port = None;
}
