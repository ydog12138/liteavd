//! WP-3.6：三 managed AVD 的真实 APK、文件推送、部分失败和取消门禁。
//!
//! `AVDM_SDK_ROOT=/path/to/test-sdk cargo test --test operation_multi_real -- --ignored --nocapture --test-threads=1`

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use liteavd::core::avd::{self, AvdSpec, GpuMode, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::instance::DeviceRuntime;
use liteavd::core::operation::{
    ApkInstallRequest, OperationCancellation, OperationKind, OperationResult, OperationSuccess,
    PushFilesRequest, execute_install_apks, execute_push_files, execute_stop,
};
use liteavd::core::repo::{Archive, SystemImage};
use liteavd::core::workspace::OperationScope;
use sha2::{Digest, Sha256};

const DEVICE_COUNT: usize = 3;
const LARGE_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;
const MAX_RSS_GROWTH_BYTES: u64 = 64 * 1024 * 1024;
const MAX_THREAD_GROWTH: usize = 4;
const MAX_FD_GROWTH: usize = 16;
const NORMAL_PACKAGE_ID: &str = "io.github.ydog12138.liteavd.fixture.normal";
const GUEST_PUSH_ROOT: &str = "/sdcard/Download/liteavd";

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
    output: PathBuf,
    log_paths: Vec<PathBuf>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for instance in emulator::list_running_for_sdk(&self.sdk_root)
            .into_iter()
            .filter(|instance| self.avd_names.contains(&instance.avd_name))
        {
            if emulator::verify_emulator_pid(instance.pid, &self.sdk_root) {
                // SAFETY: identity is verified against the isolated SDK and unique AVD set.
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
        let _ = std::fs::remove_dir_all(&self.output);
        let _ = std::fs::remove_dir_all(&self.avd_home);
    }
}

#[derive(Debug, Clone, Copy)]
struct ProcessResources {
    rss_bytes: u64,
    threads: usize,
    file_descriptors: usize,
}

struct ResourceSampler {
    stop: Arc<AtomicBool>,
    peak_rss: Arc<AtomicU64>,
    peak_threads: Arc<AtomicUsize>,
    peak_fds: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl ResourceSampler {
    fn start(initial: ProcessResources) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak_rss = Arc::new(AtomicU64::new(initial.rss_bytes));
        let peak_threads = Arc::new(AtomicUsize::new(initial.threads));
        let peak_fds = Arc::new(AtomicUsize::new(initial.file_descriptors));
        let stop_for_thread = stop.clone();
        let rss_for_thread = peak_rss.clone();
        let threads_for_thread = peak_threads.clone();
        let fds_for_thread = peak_fds.clone();
        let handle = std::thread::spawn(move || {
            while !stop_for_thread.load(Ordering::Acquire) {
                let resources = process_resources();
                rss_for_thread.fetch_max(resources.rss_bytes, Ordering::Relaxed);
                threads_for_thread.fetch_max(resources.threads, Ordering::Relaxed);
                fds_for_thread.fetch_max(resources.file_descriptors, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Self {
            stop,
            peak_rss,
            peak_threads,
            peak_fds,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> ProcessResources {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap();
        ProcessResources {
            rss_bytes: self.peak_rss.load(Ordering::Relaxed),
            threads: self.peak_threads.load(Ordering::Relaxed),
            file_descriptors: self.peak_fds.load(Ordering::Relaxed),
        }
    }
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
        .expect("执行真实 adb 命令失败")
}

fn adb_success(sdk_root: &Path, serial: &str, args: &[&str]) -> String {
    let output = adb_output(sdk_root, serial, args);
    assert!(
        output.status.success(),
        "adb {serial} {args:?} 失败：stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn make_large_payload(path: &Path) -> String {
    let mut file = std::fs::File::create(path).expect("创建大文件 fixture 失败");
    let mut hasher = Sha256::new();
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut block = vec![0_u8; 1024 * 1024];
    for _ in 0..(LARGE_PAYLOAD_BYTES / block.len() as u64) {
        for chunk in block.chunks_exact_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            chunk.copy_from_slice(&state.to_le_bytes());
        }
        file.write_all(&block).expect("写入大文件 fixture 失败");
        hasher.update(&block);
    }
    file.sync_all().expect("同步大文件 fixture 失败");
    format!("{:x}", hasher.finalize())
}

fn find_adb_push_pid(serial: &str, source: &Path) -> Option<u32> {
    let source = source.as_os_str().as_encoded_bytes();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let arguments = cmdline
            .split(|byte| *byte == 0)
            .filter(|argument| !argument.is_empty())
            .collect::<Vec<_>>();
        if arguments.contains(&b"push".as_slice())
            && arguments.contains(&serial.as_bytes())
            && arguments.contains(&source)
        {
            return Some(pid);
        }
    }
    None
}

fn remove_remote_artifacts(sdk_root: &Path, serials: &[String], path: &str) {
    let part = format!("{path}.part");
    for serial in serials {
        adb_success(sdk_root, serial, &["shell", "rm", "-f", path, &part]);
    }
}

#[test]
#[ignore = "需要隔离测试 SDK/system image、KVM、三组空闲端口和至少约 5GiB 临时空间"]
fn three_device_artifact_operations_are_bounded_and_isolated() {
    let sdk_root = PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT"));
    assert!(sdk_root.join("emulator/emulator").is_file());
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 UNIX epoch")
        .as_nanos();
    let avd_names = (1..=DEVICE_COUNT)
        .map(|index| {
            format!(
                "liteavd_operation_multi_{}_{}_{}",
                std::process::id(),
                nonce,
                index
            )
        })
        .collect::<Vec<_>>();
    let avd_home = std::env::temp_dir().join(format!("liteavd-operation-multi-{nonce}-avd"));
    let output = std::env::temp_dir().join(format!("liteavd-operation-multi-{nonce}-output"));
    std::fs::create_dir(&avd_home).expect("创建临时 AVD home 失败");
    std::fs::create_dir(&output).expect("创建临时输出目录失败");
    let _avd_home = EnvGuard::set("ANDROID_AVD_HOME", &avd_home);
    let _emulator_ld = std::env::var_os("LITEAVD_EMULATOR_LD_LIBRARY_PATH")
        .map(|value| EnvGuard::set("LD_LIBRARY_PATH", value));
    let mut cleanup = Cleanup {
        avd_names: avd_names.clone(),
        sdk_root: sdk_root.clone(),
        avd_home,
        output: output.clone(),
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
            gpu: GpuMode::SwangleIndirect,
        })
        .expect("创建三设备 operation 测试 AVD 失败");
    }

    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("创建 Tokio runtime 失败");
    let runtime = Arc::new(DeviceRuntime::default());
    let mut serials = Vec::new();
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
                gpu_policy: ManagedGpuPolicy::HeadlessSwangle,
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
        serials.push(format!("emulator-{console_port}"));
    }

    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/apk/liteavd-normal-v1.apk");
    let install_plan = runtime
        .plan_operation(OperationKind::InstallApk, OperationScope::AllRunning)
        .unwrap();
    let install = tokio
        .block_on(execute_install_apks(
            runtime.clone(),
            runtime.authorize_operation(install_plan).unwrap(),
            sdk_root.clone(),
            ApkInstallRequest {
                apks: vec![fixture],
                options: liteavd::core::adb::ApkInstallOptions::default(),
            },
            OperationCancellation::default(),
            None,
        ))
        .expect("三设备真实 APK operation 失败");
    assert_eq!(install.devices.len(), DEVICE_COUNT);
    assert!(install.devices.iter().all(|device| {
        device.result
            == OperationResult::Succeeded(OperationSuccess::ApksInstalled {
                files: 1,
                exit_code: Some(0),
            })
    }));
    for serial in &serials {
        assert!(
            adb_success(
                &sdk_root,
                serial,
                &["shell", "pm", "path", NORMAL_PACKAGE_ID]
            )
            .contains("base.apk")
        );
        adb_success(&sdk_root, serial, &["uninstall", NORMAL_PACKAGE_ID]);
    }

    let large_payload = output.join("large-payload.bin");
    let host_hash = make_large_payload(&large_payload);
    let resources_before = process_resources();
    let sampler = ResourceSampler::start(resources_before);
    let large_plan = runtime
        .plan_operation(OperationKind::PushFiles, OperationScope::AllRunning)
        .unwrap();
    let large_operation_id = large_plan.id().get();
    let large_report = tokio
        .block_on(execute_push_files(
            runtime.clone(),
            runtime.authorize_operation(large_plan).unwrap(),
            sdk_root.clone(),
            PushFilesRequest {
                files: vec![large_payload.clone()],
            },
            OperationCancellation::default(),
            None,
        ))
        .expect("三设备大文件 operation 失败");
    let large_remote = format!("{GUEST_PUSH_ROOT}/large-payload-op{large_operation_id}-1.bin");
    assert!(large_report.devices.iter().all(|device| {
        device.result
            == OperationResult::Succeeded(OperationSuccess::FilesPushed {
                paths: vec![large_remote.clone()],
                bytes: LARGE_PAYLOAD_BYTES,
                exit_code: Some(0),
            })
    }));
    for serial in &serials {
        let guest_hash = adb_success(&sdk_root, serial, &["shell", "sha256sum", &large_remote]);
        assert!(guest_hash.starts_with(&host_hash));
    }
    remove_remote_artifacts(&sdk_root, &serials, &large_remote);

    let partial_payload = output.join("partial.bin");
    std::fs::write(&partial_payload, b"partial failure fixture").unwrap();
    let partial_plan = runtime
        .plan_operation(OperationKind::PushFiles, OperationScope::AllRunning)
        .unwrap();
    let partial_remote = format!(
        "{GUEST_PUSH_ROOT}/partial-op{}-1.bin",
        partial_plan.id().get()
    );
    adb_success(
        &sdk_root,
        &serials[1],
        &["shell", "mkdir", "-p", GUEST_PUSH_ROOT],
    );
    adb_success(&sdk_root, &serials[1], &["shell", "touch", &partial_remote]);
    let partial_report = tokio
        .block_on(execute_push_files(
            runtime.clone(),
            runtime.authorize_operation(partial_plan).unwrap(),
            sdk_root.clone(),
            PushFilesRequest {
                files: vec![partial_payload],
            },
            OperationCancellation::default(),
            None,
        ))
        .expect("三设备部分失败 operation 未返回报告");
    assert!(matches!(
        partial_report.devices[0].result,
        OperationResult::Succeeded(OperationSuccess::FilesPushed { .. })
    ));
    assert!(matches!(
        &partial_report.devices[1].result,
        OperationResult::Failed(error) if error.contains("已存在，未覆盖")
    ));
    assert!(matches!(
        partial_report.devices[2].result,
        OperationResult::Succeeded(OperationSuccess::FilesPushed { .. })
    ));
    remove_remote_artifacts(&sdk_root, &serials, &partial_remote);

    let cancel_plan = runtime
        .plan_operation(OperationKind::PushFiles, OperationScope::AllRunning)
        .unwrap();
    let cancel_remote = format!(
        "{GUEST_PUSH_ROOT}/large-payload-op{}-1.bin",
        cancel_plan.id().get()
    );
    let cancellation = OperationCancellation::default();
    let cancellation_for_task = cancellation.clone();
    let second_serial = serials[1].clone();
    let source_for_task = large_payload.clone();
    let cancel_when_second_push_runs = async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(pid) = find_adb_push_pid(&second_serial, &source_for_task) {
                cancellation_for_task.cancel();
                return pid;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "120 秒内未观察到第二台设备的真实 adb push"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let execute_cancel = execute_push_files(
        runtime.clone(),
        runtime.authorize_operation(cancel_plan).unwrap(),
        sdk_root.clone(),
        PushFilesRequest {
            files: vec![large_payload],
        },
        cancellation,
        None,
    );
    let (cancel_report, canceled_adb_pid) =
        tokio.block_on(async { tokio::join!(execute_cancel, cancel_when_second_push_runs) });
    let cancel_report = cancel_report.expect("三设备取消 operation 未返回报告");
    assert!(matches!(
        cancel_report.devices[0].result,
        OperationResult::Succeeded(OperationSuccess::FilesPushed { .. })
    ));
    assert_eq!(cancel_report.devices[1].result, OperationResult::Canceled);
    assert_eq!(cancel_report.devices[2].result, OperationResult::Canceled);
    assert!(
        !Path::new(&format!("/proc/{canceled_adb_pid}")).exists(),
        "取消后 adb 子进程仍存活：{canceled_adb_pid}"
    );
    for serial in &serials {
        adb_success(
            &sdk_root,
            serial,
            &["shell", "test", "!", "-e", &format!("{cancel_remote}.part")],
        );
    }
    remove_remote_artifacts(&sdk_root, &serials, &cancel_remote);

    let resources_peak = sampler.finish();
    let resources_after = process_resources();
    assert!(
        resources_peak
            .rss_bytes
            .saturating_sub(resources_before.rss_bytes)
            <= MAX_RSS_GROWTH_BYTES,
        "大文件 operation RSS 增长超过 64MiB：before={resources_before:?}, peak={resources_peak:?}"
    );
    assert!(
        resources_peak.threads <= resources_before.threads + MAX_THREAD_GROWTH,
        "大文件 operation 线程数无界增长：before={resources_before:?}, peak={resources_peak:?}"
    );
    assert!(
        resources_peak.file_descriptors <= resources_before.file_descriptors + MAX_FD_GROWTH,
        "大文件 operation fd 数无界增长：before={resources_before:?}, peak={resources_peak:?}"
    );
    eprintln!(
        "operation resources: before={resources_before:?}, peak={resources_peak:?}, after={resources_after:?}, payload={LARGE_PAYLOAD_BYTES}B"
    );

    let stop_plan = runtime
        .plan_operation(OperationKind::Stop, OperationScope::AllRunning)
        .unwrap();
    let stop = tokio
        .block_on(execute_stop(
            runtime.clone(),
            runtime.authorize_operation(stop_plan).unwrap(),
            sdk_root.clone(),
        ))
        .expect("三设备 exact stop operation 失败");
    assert!(
        stop.devices.iter().all(|device| {
            device.result == OperationResult::Succeeded(OperationSuccess::Stopped)
        })
    );
    for name in &avd_names {
        avd::delete_avd(name).expect("删除三设备 operation 测试 AVD 失败");
    }
    assert!(
        emulator::list_running_for_sdk(&sdk_root)
            .into_iter()
            .all(|instance| !avd_names.contains(&instance.avd_name))
    );
    eprintln!("three-device artifact operation gate completed in isolated AVD home");
}
