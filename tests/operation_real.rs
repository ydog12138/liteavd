//! 单 managed AVD 的真实 screenshot / snapshot / APK / push / stop operation 链。
//!
//! `AVDM_SDK_ROOT=/path/to/test-sdk cargo test --test operation_real -- --ignored --nocapture`

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use liteavd::core::avd::{self, AvdSpec, GpuMode, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::input::DeviceKey;
use liteavd::core::instance::{DeviceRuntime, SessionOrigin};
use liteavd::core::operation::{
    ApkInstallRequest, OperationCancellation, OperationKind, OperationResult, OperationSuccess,
    PushFilesRequest, SnapshotMutation, execute_install_apk, execute_install_apks,
    execute_push_files, execute_screenshots, execute_stop, list_route_snapshots,
    mutate_route_snapshot, send_route_keypress,
};
use liteavd::core::recovery;
use liteavd::core::repo::{Archive, SystemImage};
use liteavd::core::workspace::OperationScope;
use sha2::{Digest, Sha256};

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
    output: PathBuf,
    console_port: Option<u16>,
    log_paths: Vec<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for instance in emulator::list_running_for_sdk(&self.sdk_root)
            .into_iter()
            .filter(|instance| instance.avd_name == self.avd_name)
        {
            if emulator::verify_emulator_pid(instance.pid, &self.sdk_root) {
                // SAFETY: identity was verified against the isolated SDK and unique AVD name.
                unsafe { libc::kill(instance.pid as i32, libc::SIGTERM) };
            }
            for _ in 0..50 {
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
        if let Some(console_port) = self.console_port {
            let _ = std::fs::remove_file(liteavd::core::stream::share_vid_path(console_port));
        }
        let _ = avd::delete_avd(&self.avd_name);
        for path in &self.log_paths {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(path.with_extension("log.previous"));
        }
        let _ = std::fs::remove_dir_all(&self.output);
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

fn sha256_file(path: &Path) -> String {
    let mut file = std::fs::File::open(path).expect("打开 hash fixture 失败");
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("读取 hash fixture 失败");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    format!("{:x}", hasher.finalize())
}

fn adb_success(
    tokio: &tokio::runtime::Runtime,
    sdk_root: &Path,
    serial: &str,
    args: Vec<OsString>,
) -> liteavd::core::adb::AdbCommandOutput {
    let output = tokio
        .block_on(liteavd::core::adb::run_cancellable(
            sdk_root,
            serial,
            args,
            Duration::from_secs(60),
            || false,
        ))
        .expect("真实 adb 命令未能执行");
    assert!(
        output.success(),
        "真实 adb 命令失败：{}",
        output.failure_summary()
    );
    output
}

#[test]
#[ignore = "需要隔离测试 SDK/system image、KVM 与空闲 console/gRPC 端口"]
fn managed_operation_chain_is_exact_and_cleans_up() {
    let sdk_root = PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT"));
    assert!(sdk_root.join("emulator/emulator").is_file());
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 UNIX epoch")
        .as_nanos();
    let avd_name = format!("liteavd_operation_{}_{}", std::process::id(), nonce);
    let avd_home = std::env::temp_dir().join(format!("{avd_name}-home"));
    let output = std::env::temp_dir().join(format!("{avd_name}-output"));
    std::fs::create_dir(&avd_home).expect("创建临时 AVD home 失败");
    let _avd_home = EnvGuard::set("ANDROID_AVD_HOME", &avd_home);
    let _emulator_ld = std::env::var_os("LITEAVD_EMULATOR_LD_LIBRARY_PATH")
        .map(|value| EnvGuard::set("LD_LIBRARY_PATH", value));
    let mut cleanup = Cleanup {
        avd_name: avd_name.clone(),
        sdk_root: sdk_root.clone(),
        avd_home,
        output: output.clone(),
        console_port: None,
        log_paths: Vec::new(),
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
    .expect("创建 operation 测试 AVD 失败");

    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("创建 Tokio runtime 失败");
    let runtime = Arc::new(DeviceRuntime::default());
    let command = runtime.begin_start(&avd_name).unwrap();
    let occupied = emulator::list_running_for_sdk(&sdk_root)
        .into_iter()
        .map(|instance| instance.console_port);
    let reservation = runtime.reserve_port(occupied).unwrap();
    let console_port = reservation.port();
    cleanup.console_port = Some(console_port);
    runtime.attach_start_port(&command, console_port).unwrap();
    let launched = tokio
        .block_on(emulator::launch(&LaunchParams {
            sdk_root: sdk_root.clone(),
            avd_name: avd_name.clone(),
            port: console_port,
            grpc: GrpcLaunchConfig::new(console_port + 3000).unwrap(),
            gpu_policy: ManagedGpuPolicy::HeadlessSwangle,
            audio_policy: ManagedAudioPolicy::Disabled,
            no_window: true,
            share_vid: true,
        }))
        .unwrap_or_else(|error| panic!("operation test launch 失败：{error:#}"));
    cleanup.log_paths.push(launched.log_path().to_path_buf());
    runtime.mark_booting(&command).unwrap();
    tokio
        .block_on(liteavd::core::adb::wait_for_boot(
            &sdk_root,
            &format!("emulator-{console_port}"),
            Duration::from_secs(240),
        ))
        .unwrap_or_else(|error| panic!("operation test boot 失败：{error:#}"));
    runtime
        .complete_start(&command, launched, reservation)
        .unwrap();
    let route = runtime.focus_session(&avd_name).unwrap();
    runtime.toggle_selected(&route).unwrap();
    let workspace_state = output.join("workspace.json");
    recovery::save_workspace(&workspace_state, &runtime.workspace_intent()).unwrap();

    // GUI 关闭只丢弃应用 runtime，不应终止 managed engine。新进程从广告事实、
    // 私有恢复身份和稳定 AVD 名称重建可交互 session。
    drop(runtime);
    let observed = emulator::list_running_for_sdk(&sdk_root);
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].avd_name, avd_name);
    let runtime = Arc::new(DeviceRuntime::default());
    runtime.reconcile_running_for_sdk_with_demands(observed, HashMap::new(), &sdk_root);
    runtime.restore_workspace_intent(&recovery::load_workspace(&workspace_state).unwrap());
    let recovered = runtime.projection(&avd_name).session.unwrap();
    assert_eq!(recovered.origin, SessionOrigin::Recovered);
    assert!(runtime.grpc_client(&avd_name).is_some());
    assert!(runtime.capture_subscription(&avd_name).is_some());
    assert_eq!(
        runtime.workspace_intent().focused_avd.as_deref(),
        Some(avd_name.as_str())
    );
    assert_eq!(
        runtime.workspace_intent().selected_avds,
        vec![avd_name.clone()]
    );
    let route = runtime
        .workspace_snapshot()
        .focused
        .expect("恢复后缺少 exact focused route");
    let serial = format!("emulator-{console_port}");
    tokio
        .block_on(send_route_keypress(
            runtime.clone(),
            route.clone(),
            DeviceKey::VolumeDown,
        ))
        .expect("真实设备音量减快捷键失败");
    tokio
        .block_on(send_route_keypress(
            runtime.clone(),
            route.clone(),
            DeviceKey::VolumeUp,
        ))
        .expect("真实设备音量加快捷键失败");

    let screenshot_plan = runtime
        .plan_operation(OperationKind::Screenshot, OperationScope::Focused)
        .unwrap();
    let screenshot = tokio
        .block_on(execute_screenshots(
            runtime.clone(),
            runtime.authorize_operation(screenshot_plan).unwrap(),
            output.clone(),
        ))
        .unwrap();
    let screenshot_path = match &screenshot.devices[0].result {
        OperationResult::Succeeded(OperationSuccess::Screenshot { path, bytes }) => {
            assert!(*bytes > 8);
            path.clone()
        }
        result => panic!("真实截图 operation 失败：{result:?}"),
    };
    assert!(
        std::fs::read(screenshot_path)
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );

    let snapshot_id = format!("liteavd-wp32-{}", std::process::id());
    tokio.block_on(async {
        mutate_route_snapshot(
            runtime.clone(),
            route.clone(),
            snapshot_id.clone(),
            SnapshotMutation::Save,
        )
        .await
        .expect("真实 saveSnapshot 失败");
        assert!(
            list_route_snapshots(runtime.clone(), route.clone())
                .await
                .expect("保存后 listSnapshots 失败")
                .iter()
                .any(|snapshot| snapshot.snapshot_id == snapshot_id)
        );
        let stream_revision = runtime.control_stream_revision();
        mutate_route_snapshot(
            runtime.clone(),
            route.clone(),
            snapshot_id.clone(),
            SnapshotMutation::Load,
        )
        .await
        .expect("真实 loadSnapshot 失败");
        assert_eq!(
            runtime.control_stream_revision(),
            stream_revision + 1,
            "snapshot load 应通知长存 session stream 重建"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            if list_route_snapshots(runtime.clone(), route.clone())
                .await
                .is_ok()
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "loadSnapshot 后控制面 60 秒内未恢复"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        mutate_route_snapshot(
            runtime.clone(),
            route.clone(),
            snapshot_id.clone(),
            SnapshotMutation::Delete,
        )
        .await
        .expect("真实 deleteSnapshot 失败");
        assert!(
            list_route_snapshots(runtime.clone(), route.clone())
                .await
                .expect("删除后 listSnapshots 失败")
                .iter()
                .all(|snapshot| snapshot.snapshot_id != snapshot_id)
        );
    });
    tokio
        .block_on(liteavd::core::adb::wait_for_boot(
            &sdk_root,
            &serial,
            Duration::from_secs(120),
        ))
        .unwrap_or_else(|error| panic!("snapshot load 后 adb 未恢复：{error:#}"));

    let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/apk");
    let normal_apk = fixture_root.join("liteavd-normal-v1.apk");
    let base_apk = fixture_root.join("liteavd-fixture-v1.apk");
    let french_split = fixture_root.join("liteavd-fixture-v1-fr.apk");
    assert_eq!(
        sha256_file(&normal_apk),
        "ee17a2440c604f55fac28994204a14de09dc03005efb30e4750e7d53a9dc4f11"
    );
    assert_eq!(
        sha256_file(&base_apk),
        "e40cd0c9fee6e1b20d4c27aa6bb6219a5916a964866c9464a2c37ea81f09a16c"
    );
    assert_eq!(
        sha256_file(&french_split),
        "66a8841b94db6aa2cd3f822aa58541befe70dc2807208d648ba22ec8a77c8b4b"
    );
    for attempt in 1..=2 {
        let install_plan = runtime
            .plan_operation(OperationKind::InstallApk, OperationScope::Focused)
            .unwrap();
        let install = tokio
            .block_on(execute_install_apks(
                runtime.clone(),
                runtime.authorize_operation(install_plan).unwrap(),
                sdk_root.clone(),
                ApkInstallRequest {
                    apks: vec![normal_apk.clone()],
                    options: liteavd::core::adb::ApkInstallOptions::default(),
                },
                OperationCancellation::default(),
                None,
            ))
            .unwrap();
        assert_eq!(
            install.devices[0].result,
            OperationResult::Succeeded(OperationSuccess::ApksInstalled {
                files: 1,
                exit_code: Some(0),
            }),
            "第 {attempt} 次普通 APK 安装失败"
        );
    }
    let normal_package_id = "io.github.ydog12138.liteavd.fixture.normal";
    let package_path = adb_success(
        &tokio,
        &sdk_root,
        &serial,
        vec![
            "shell".into(),
            "pm".into(),
            "path".into(),
            normal_package_id.into(),
        ],
    );
    assert!(package_path.stdout.summary().contains("base.apk"));
    adb_success(
        &tokio,
        &sdk_root,
        &serial,
        vec!["uninstall".into(), normal_package_id.into()],
    );

    let test_only_plan = runtime
        .plan_operation(OperationKind::InstallApk, OperationScope::Focused)
        .unwrap();
    let test_only_install = tokio
        .block_on(execute_install_apks(
            runtime.clone(),
            runtime.authorize_operation(test_only_plan).unwrap(),
            sdk_root.clone(),
            ApkInstallRequest {
                apks: vec![base_apk.clone()],
                options: liteavd::core::adb::ApkInstallOptions::default(),
            },
            OperationCancellation::default(),
            None,
        ))
        .unwrap();
    assert_eq!(
        test_only_install.devices[0].result,
        OperationResult::Succeeded(OperationSuccess::ApksInstalled {
            files: 1,
            exit_code: Some(0),
        })
    );
    let package_id = "io.github.ydog12138.liteavd.fixture";
    adb_success(
        &tokio,
        &sdk_root,
        &serial,
        vec!["uninstall".into(), package_id.into()],
    );

    let split_plan = runtime
        .plan_operation(OperationKind::InstallApk, OperationScope::Focused)
        .unwrap();
    let split_install = tokio
        .block_on(execute_install_apks(
            runtime.clone(),
            runtime.authorize_operation(split_plan).unwrap(),
            sdk_root.clone(),
            ApkInstallRequest {
                apks: vec![base_apk, french_split],
                options: liteavd::core::adb::ApkInstallOptions::default(),
            },
            OperationCancellation::default(),
            None,
        ))
        .unwrap();
    assert_eq!(
        split_install.devices[0].result,
        OperationResult::Succeeded(OperationSuccess::ApksInstalled {
            files: 2,
            exit_code: Some(0),
        })
    );
    let split_paths = adb_success(
        &tokio,
        &sdk_root,
        &serial,
        vec![
            "shell".into(),
            "pm".into(),
            "path".into(),
            package_id.into(),
        ],
    )
    .stdout
    .summary();
    assert!(
        split_paths.lines().count() >= 2,
        "split 未被安装：{split_paths}"
    );
    adb_success(
        &tokio,
        &sdk_root,
        &serial,
        vec!["uninstall".into(), package_id.into()],
    );

    let payload = output.join("push-payload.bin");
    let mut payload_file = std::fs::File::create(&payload).unwrap();
    let block = [0x5a_u8; 64 * 1024];
    for _ in 0..128 {
        payload_file.write_all(&block).unwrap();
    }
    payload_file.sync_all().unwrap();
    drop(payload_file);
    let payload_hash = sha256_file(&payload);
    let push_plan = runtime
        .plan_operation(OperationKind::PushFiles, OperationScope::Focused)
        .unwrap();
    let push = tokio
        .block_on(execute_push_files(
            runtime.clone(),
            runtime.authorize_operation(push_plan).unwrap(),
            sdk_root.clone(),
            PushFilesRequest {
                files: vec![payload],
            },
            OperationCancellation::default(),
            None,
        ))
        .unwrap();
    let remote_path = match &push.devices[0].result {
        OperationResult::Succeeded(OperationSuccess::FilesPushed {
            paths,
            bytes,
            exit_code,
        }) => {
            assert_eq!(*bytes, 8 * 1024 * 1024);
            assert_eq!(*exit_code, Some(0));
            assert_eq!(paths.len(), 1);
            paths[0].clone()
        }
        result => panic!("真实文件推送失败：{result:?}"),
    };
    let guest_hash = adb_success(
        &tokio,
        &sdk_root,
        &serial,
        vec![
            "shell".into(),
            "sha256sum".into(),
            remote_path.clone().into(),
        ],
    )
    .stdout
    .summary();
    assert!(guest_hash.starts_with(&payload_hash));
    adb_success(
        &tokio,
        &sdk_root,
        &serial,
        vec![
            "shell".into(),
            "test".into(),
            "!".into(),
            "-e".into(),
            format!("{remote_path}.part").into(),
        ],
    );
    adb_success(
        &tokio,
        &sdk_root,
        &serial,
        vec!["shell".into(), "rm".into(), "-f".into(), remote_path.into()],
    );

    let invalid_apk = output.join("invalid.apk");
    std::fs::write(&invalid_apk, b"not-an-apk").unwrap();
    let install_plan = runtime
        .plan_operation(OperationKind::InstallApk, OperationScope::Focused)
        .unwrap();
    let install = tokio
        .block_on(execute_install_apk(
            runtime.clone(),
            runtime.authorize_operation(install_plan).unwrap(),
            sdk_root.clone(),
            invalid_apk,
        ))
        .unwrap();
    assert!(matches!(
        install.devices[0].result,
        OperationResult::Failed(_)
    ));
    assert!(
        runtime
            .session_for_route(runtime.workspace_snapshot().focused.as_ref().unwrap())
            .is_some()
    );

    let stop_plan = runtime
        .plan_operation(OperationKind::Stop, OperationScope::Focused)
        .unwrap();
    let stop = tokio
        .block_on(execute_stop(
            runtime.clone(),
            runtime.authorize_operation(stop_plan).unwrap(),
            sdk_root,
        ))
        .unwrap();
    assert_eq!(
        stop.devices[0].result,
        OperationResult::Succeeded(OperationSuccess::Stopped)
    );
    assert!(runtime.workspace_snapshot().routes.is_empty());
}
