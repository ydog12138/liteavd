//! WP-3.5 gate：`-no-audio` + JWT `streamAudio` 必须返回真实 guest PCM。
//!
//! `AVDM_SDK_ROOT=/path/to/test-sdk cargo test --test audio_chain -- --ignored --nocapture --test-threads=1`

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use liteavd::core::avd::{self, AvdSpec, GpuMode, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc::{AudioChannels, AudioSampleFormat, KeyEventType};
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::instance::DeviceRuntime;
use liteavd::core::operation::{OperationKind, OperationResult, OperationSuccess, execute_stop};
use liteavd::core::repo::{Archive, SystemImage};
use liteavd::core::workspace::OperationScope;
use liteavd::ui::audio::{AudioController, AudioStatus};

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
        if let Some(port) = self.console_port {
            let _ = std::fs::remove_file(liteavd::core::stream::share_vid_path(port));
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

fn adb_shell(sdk_root: &Path, serial: &str, args: &[&str]) -> std::process::Output {
    Command::new(sdk_root.join("platform-tools/adb"))
        .arg("-s")
        .arg(serial)
        .arg("shell")
        .args(args)
        .output()
        .expect("执行测试 adb shell 失败")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要隔离测试 SDK/system image、KVM 与空闲 console/gRPC 端口"]
async fn no_audio_flag_still_exposes_authenticated_guest_pcm() {
    let sdk_root = PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT"));
    assert!(sdk_root.join("emulator/emulator").is_file());
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 UNIX epoch")
        .as_nanos();
    let avd_name = format!("liteavd_audio_{}_{}", std::process::id(), nonce);
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
    .expect("创建 audio 测试 AVD 失败");

    let occupied = emulator::list_running_for_sdk(&sdk_root)
        .into_iter()
        .map(|instance| instance.console_port);
    let runtime = Arc::new(DeviceRuntime::default());
    let command = runtime.begin_start(&avd_name).unwrap();
    let reservation = runtime
        .reserve_port(occupied)
        .expect("audio 测试没有空闲 console port");
    let console_port = reservation.port();
    cleanup.console_port = Some(console_port);
    runtime.attach_start_port(&command, console_port).unwrap();
    let launched = emulator::launch(&LaunchParams {
        sdk_root: sdk_root.clone(),
        avd_name: avd_name.clone(),
        port: console_port,
        grpc: GrpcLaunchConfig::new(console_port + 3000).unwrap(),
        gpu_policy,
        audio_policy: ManagedAudioPolicy::Disabled,
        no_window: true,
        share_vid: false,
    })
    .await
    .unwrap_or_else(|error| panic!("audio test launch 失败：{error:#}"));
    cleanup.log_path = Some(launched.log_path().to_path_buf());
    runtime.mark_booting(&command).unwrap();
    liteavd::core::adb::wait_for_boot(
        &sdk_root,
        &format!("emulator-{console_port}"),
        Duration::from_secs(240),
    )
    .await
    .unwrap_or_else(|error| panic!("audio test boot 失败：{error:#}"));

    let serial = format!("emulator-{console_port}");
    let timer = adb_shell(
        &sdk_root,
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
    eprintln!(
        "timer stimulus status={} stdout={} stderr={}",
        timer.status,
        String::from_utf8_lossy(&timer.stdout).trim(),
        String::from_utf8_lossy(&timer.stderr).trim()
    );
    assert!(timer.status.success(), "guest 定时器音源启动失败");

    // Emulator 直到首个音频 packet 可用才完成 server-streaming RPC 的 response
    // future，因此音源必须先排定；5 秒 timeout 同时约束响应头和首包等待。
    let client = launched.grpc_client().reconnect().await.unwrap();
    let mut stream = client
        .stream_audio_output()
        .await
        .expect("认证 streamAudio 建链/首包等待失败");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut packets = 0_u64;
    let mut bytes = 0_u64;
    let mut nonzero_bytes = 0_u64;
    while tokio::time::Instant::now() < deadline && nonzero_bytes == 0 {
        let packet = match tokio::time::timeout(Duration::from_millis(500), stream.message()).await
        {
            Ok(Ok(Some(packet))) => packet,
            Ok(Ok(None)) => panic!("streamAudio 在 guest 音源前结束"),
            Ok(Err(error)) => panic!("streamAudio 接收失败：{error}"),
            Err(_) => {
                let key = if packets.is_multiple_of(2) {
                    "AudioVolumeUp"
                } else {
                    "AudioVolumeDown"
                };
                client
                    .send_key(key, KeyEventType::Keypress)
                    .await
                    .unwrap_or_else(|error| panic!("音频刺激 {key} 失败：{error}"));
                continue;
            }
        };
        let format = packet.format.expect("AudioPacket 缺少 format");
        assert_eq!(format.sampling_rate, 48_000);
        assert_eq!(format.channels, AudioChannels::Stereo as i32);
        assert_eq!(format.format, AudioSampleFormat::AudFmtS16 as i32);
        assert_eq!(packet.audio.len() % 4, 0, "stereo S16 packet 未按帧对齐");
        packets += 1;
        bytes += packet.audio.len() as u64;
        nonzero_bytes += packet.audio.iter().filter(|sample| **sample != 0).count() as u64;
    }
    for package in ["com.google.android.deskclock", "com.android.deskclock"] {
        let _ = adb_shell(&sdk_root, &serial, &["am", "force-stop", package]);
    }
    assert!(packets > 0, "streamAudio 20 秒内没有返回 packet");
    assert!(
        nonzero_bytes > 0,
        "streamAudio 只返回静音：packets={packets} bytes={bytes}"
    );
    eprintln!("streamAudio packets={packets} bytes={bytes} nonzero_bytes={nonzero_bytes}");

    drop(stream);
    runtime
        .complete_start(&command, launched, reservation)
        .unwrap();
    runtime.focus_session(&avd_name).unwrap();

    // 继续走产品 coordinator → CPAL/PulseAudio callback，而不是只证明 raw RPC。
    let controller = AudioController::new(runtime.clone());
    controller.sync_focus();
    let timer = adb_shell(
        &sdk_root,
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
    assert!(timer.status.success(), "第二次 guest 定时器音源启动失败");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let played = loop {
        controller.sync_focus();
        match controller.status() {
            AudioStatus::Playing { stats, .. }
                if stats.samples_received > 0 && stats.samples_played > 0 =>
            {
                break stats;
            }
            AudioStatus::Error { message, .. } => panic!("产品音频输出失败：{message}"),
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "产品 CPAL sink 20 秒内没有消费 guest PCM：{:?}",
            controller.status()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    eprintln!("product audio buffer stats={played:?}");
    controller.set_enabled(false);
    assert_eq!(controller.status(), AudioStatus::Disabled);
    tokio::time::sleep(Duration::from_millis(100)).await;

    for package in ["com.google.android.deskclock", "com.android.deskclock"] {
        let _ = adb_shell(&sdk_root, &serial, &["am", "force-stop", package]);
    }
    let plan = runtime
        .plan_operation(OperationKind::Stop, OperationScope::Focused)
        .unwrap();
    let report = execute_stop(
        runtime.clone(),
        runtime.authorize_operation(plan).unwrap(),
        sdk_root.clone(),
    )
    .await
    .expect("产品 exact stop 执行失败");
    assert_eq!(
        report.devices[0].result,
        OperationResult::Succeeded(OperationSuccess::Stopped)
    );
    avd::delete_avd(&avd_name).expect("删除 audio 测试 AVD 失败");
    cleanup.console_port = None;
}
