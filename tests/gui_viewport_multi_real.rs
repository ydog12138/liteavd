//! WP-1.5：三个 managed Emulator / interactive viewport 长期资源回归。
//!
//! 快速烟测：
//! `DISPLAY=:97 AVDM_SDK_ROOT=/path/to/sdk cargo test --test gui_viewport_multi_real -- --ignored --nocapture`
//! 正式门禁增加 `LITEAVD_MULTI_VIEWPORT_SOAK_SECONDS=1800`。
//! 故障隔离门禁再加 `LITEAVD_MULTI_FAULT=1`，在中点停止随机一台 engine。

use std::ffi::{OsStr, OsString};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gtk4::prelude::*;
use liteavd::core::avd::{self, AvdSpec, GpuMode, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc::KeyEventType;
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::repo::{Archive, SystemImage};
use liteavd::core::stream::share_vid_path;
use liteavd::core::telemetry::LatencyProbe;
use liteavd::ui::viewport::{PICTURE_WIDGET, build_interactive_measured};

const SLOTS: [(u16, u16); 3] = [(5576, 8576), (5578, 8578), (5580, 8580)];
const DEFAULT_SOAK_SECONDS: u64 = 30;
const STIMULUS_INTERVAL: Duration = Duration::from_secs(15);
const MAX_RSS_GROWTH_BYTES: u64 = 384 * 1024 * 1024;
const MAX_THREAD_GROWTH: usize = 2;
const MAX_FD_GROWTH: usize = 16;
const MAX_UI_PUMP_STALL_MICROS: u64 = 250_000;

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: this integration-test binary contains one test and restores the value.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `EnvGuard::set`; no sibling test observes this temporary value.
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
    shm_paths: Vec<PathBuf>,
    auth_dirs: Arc<Mutex<Vec<PathBuf>>>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for instance in emulator::list_running_for_sdk(&self.sdk_root)
            .into_iter()
            .filter(|instance| self.avd_names.contains(&instance.avd_name))
        {
            unsafe {
                libc::kill(instance.pid as i32, libc::SIGTERM);
            }
            for _ in 0..50 {
                if !Path::new(&format!("/proc/{}", instance.pid)).exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if emulator::verify_emulator_pid(instance.pid, &self.sdk_root) {
                unsafe {
                    libc::kill(instance.pid as i32, libc::SIGKILL);
                }
            }
        }
        for name in &self.avd_names {
            let _ = avd::delete_avd(name);
        }
        for path in &self.shm_paths {
            let _ = std::fs::remove_file(path);
        }
        for path in self
            .auth_dirs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
        {
            let _ = std::fs::remove_dir_all(path);
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

fn sdk_root() -> PathBuf {
    let root = PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT"));
    assert!(
        root.join("emulator/emulator").is_file(),
        "SDK 缺少 emulator"
    );
    root
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
    panic!("SDK 中未找到已安装系统镜像");
}

fn find_picture(root: &gtk4::Widget) -> gtk4::Picture {
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if widget.widget_name() == PICTURE_WIDGET {
            return widget.downcast().expect("viewport picture 类型不符");
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            stack.push(next.clone());
            child = next.next_sibling();
        }
    }
    panic!("viewport 缺少 picture");
}

fn iterate_main_context() {
    let context = glib::MainContext::default();
    while context.pending() {
        context.iteration(false);
    }
}

#[test]
#[ignore = "需要 Xvfb/Wayland、测试 SDK/system image、KVM 和六个空闲端口"]
fn three_managed_viewports_remain_bounded() {
    gtk4::init().expect("gtk4 初始化失败");
    let sdk_root = sdk_root();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 UNIX epoch")
        .as_nanos();
    let avd_home = std::env::temp_dir().join(format!(
        "liteavd-gui-multi-real-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&avd_home).expect("创建临时 AVD home 失败");
    let _avd_home_env = EnvGuard::set("ANDROID_AVD_HOME", &avd_home);
    let _emulator_ld = std::env::var_os("LITEAVD_EMULATOR_LD_LIBRARY_PATH")
        .map(|value| EnvGuard::set("LD_LIBRARY_PATH", value));

    for (console, grpc) in SLOTS {
        for port in [console, console + 1, grpc] {
            drop(
                TcpListener::bind(("127.0.0.1", port))
                    .unwrap_or_else(|error| panic!("测试端口 {port} 已占用：{error}")),
            );
        }
    }
    let avd_names: Vec<_> = (0..SLOTS.len())
        .map(|index| format!("liteavd_multi_{}_{}", std::process::id(), index + 1))
        .collect();
    let shm_paths: Vec<_> = SLOTS
        .iter()
        .map(|(console, _)| share_vid_path(*console))
        .collect();
    for path in &shm_paths {
        if path.exists() {
            std::fs::remove_file(path).expect("删除陈旧 share-vid shm 失败");
        }
    }
    let cleanup_auth_dirs = Arc::new(Mutex::new(Vec::new()));
    let _cleanup = Cleanup {
        avd_names: avd_names.clone(),
        sdk_root: sdk_root.clone(),
        avd_home,
        shm_paths: shm_paths.clone(),
        auth_dirs: cleanup_auth_dirs.clone(),
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
        .expect("创建多设备测试 AVD 失败");
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("创建 tokio runtime 失败");
    let mut launched = Vec::new();
    for ((console, grpc), avd_name) in SLOTS.into_iter().zip(&avd_names) {
        launched.push(
            runtime
                .block_on(emulator::launch(&LaunchParams {
                    sdk_root: sdk_root.clone(),
                    avd_name: avd_name.clone(),
                    port: console,
                    grpc: GrpcLaunchConfig::new(grpc).expect("创建 gRPC JWT 身份失败"),
                    gpu_policy: ManagedGpuPolicy::HeadlessSwangle,
                    audio_policy: ManagedAudioPolicy::Disabled,
                    no_window: true,
                    share_vid: true,
                }))
                .unwrap_or_else(|error| panic!("production launch {avd_name} 失败：{error:#}")),
        );
    }
    let auth_dirs: Vec<_> = launched
        .iter()
        .map(|instance| {
            instance
                .grpc_auth()
                .allowlist_path()
                .parent()
                .expect("allowlist 缺少 session 目录")
                .to_path_buf()
        })
        .collect();
    *cleanup_auth_dirs
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = auth_dirs.clone();

    let container = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    let mut pictures = Vec::new();
    let mut telemetry = Vec::<LatencyProbe>::new();
    let mut clients = Vec::new();
    for instance in &launched {
        let client = instance.grpc_client().clone();
        let interactive = build_interactive_measured(
            instance
                .capture_subscription()
                .expect("managed session 缺少 capture"),
            client.clone(),
        );
        pictures.push(find_picture(interactive.root.upcast_ref()));
        telemetry.push(interactive.telemetry);
        clients.push(client);
        container.append(&interactive.root);
    }
    let window = gtk4::Window::new();
    window.set_title(Some("liteavd three viewport soak"));
    window.set_default_size(1200, 760);
    window.set_child(Some(&container));
    window.present();

    let frame_deadline = Instant::now() + Duration::from_secs(60);
    while pictures.iter().any(|picture| picture.paintable().is_none()) {
        iterate_main_context();
        assert!(
            Instant::now() < frame_deadline,
            "60 秒内三个 viewport 未全部出帧"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    for (console, _) in SLOTS {
        runtime
            .block_on(liteavd::core::adb::wait_for_boot(
                &sdk_root,
                &format!("emulator-{console}"),
                Duration::from_secs(240),
            ))
            .unwrap_or_else(|error| panic!("等待 emulator-{console} boot 失败：{error:#}"));
    }

    let warmup_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < warmup_deadline {
        iterate_main_context();
        std::thread::sleep(Duration::from_millis(5));
    }
    for probe in &telemetry {
        probe.reset_measurement();
    }
    let resources_before = process_resources();
    let mut resources_max = resources_before;
    let published_before: Vec<_> = launched
        .iter()
        .map(|instance| instance.capture_stats().unwrap().frames_published)
        .collect();
    let soak_seconds = std::env::var("LITEAVD_MULTI_VIEWPORT_SOAK_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SOAK_SECONDS);
    let soak_started = Instant::now();
    let soak_deadline = soak_started + Duration::from_secs(soak_seconds);
    let fault_index = std::env::var_os("LITEAVD_MULTI_FAULT")
        .is_some()
        .then_some((nonce as usize) % launched.len());
    let fault_at = soak_started + Duration::from_secs((soak_seconds / 2).max(1));
    let mut fault_injected = false;
    let mut fault_completed = false;
    let (fault_result_tx, fault_result_rx) = mpsc::channel::<Result<(), String>>();
    let mut fault_worker = None;
    let mut survivor_published_at_fault: Option<Vec<u64>> = None;
    let mut next_resource_sample = Instant::now();
    let mut next_stimulus = Instant::now();
    let mut next_progress = soak_started + Duration::from_secs(300);
    while Instant::now() < soak_deadline {
        iterate_main_context();
        let now = Instant::now();
        if let Some(fault_index) = fault_index
            && !fault_injected
            && now >= fault_at
        {
            survivor_published_at_fault = Some(
                launched
                    .iter()
                    .map(|instance| instance.capture_stats().unwrap().frames_published)
                    .collect(),
            );
            let failed_pid = launched[fault_index].instance.pid;
            let failure_sdk = sdk_root.clone();
            let result_tx = fault_result_tx.clone();
            fault_worker = Some(std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string());
                let result = match runtime {
                    Ok(runtime) => runtime
                        .block_on(emulator::stop(failed_pid, &failure_sdk))
                        .map_err(|error| format!("{error:#}")),
                    Err(error) => Err(error),
                };
                let _ = result_tx.send(result);
            }));
            fault_injected = true;
            eprintln!(
                "multi viewport fault injected at device {}",
                fault_index + 1
            );
        }
        if fault_injected && !fault_completed {
            match fault_result_rx.try_recv() {
                Ok(Ok(())) => fault_completed = true,
                Ok(Err(error)) => panic!("故障注入 stop worker 失败：{error}"),
                Err(mpsc::TryRecvError::Disconnected) => {
                    panic!("故障注入 stop worker 未返回结果")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if now >= next_stimulus {
            for (index, client) in clients.iter().enumerate() {
                if fault_injected && Some(index) == fault_index {
                    continue;
                }
                runtime
                    .block_on(client.send_key("Power", KeyEventType::Keypress))
                    .expect("三设备 Power 刺激失败");
            }
            next_stimulus = now + STIMULUS_INTERVAL;
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
                "multi viewport soak progress: {}s/{soak_seconds}s, current={:?}, max={resources_max:?}",
                soak_started.elapsed().as_secs(),
                process_resources(),
            );
            next_progress += Duration::from_secs(300);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let resources_after = process_resources();
    let capture_stats: Vec<_> = launched
        .iter()
        .map(|instance| instance.capture_stats().unwrap())
        .collect();
    for (index, stats) in capture_stats.iter().enumerate() {
        assert!(
            stats.frames_published > published_before[index],
            "设备 {} soak 期间没有新帧：{stats:?}",
            index + 1
        );
        assert_eq!(stats.unstable_frames, 0, "设备 {} 出现不一致帧", index + 1);
        assert!(
            stats.last_error.is_none(),
            "设备 {} capture 错误：{stats:?}",
            index + 1
        );
    }
    if let Some(fault_index) = fault_index {
        assert!(fault_injected, "请求了故障注入但测试结束前未执行");
        if !fault_completed {
            fault_result_rx
                .recv_timeout(Duration::from_secs(15))
                .expect("故障注入 stop worker 超时")
                .unwrap_or_else(|error| panic!("故障注入 stop worker 失败：{error}"));
        }
        if let Some(worker) = fault_worker.take() {
            worker.join().expect("故障注入 stop worker panic");
        }
        assert!(
            !emulator::verify_emulator_pid(launched[fault_index].instance.pid, &sdk_root),
            "故障注入后 engine 仍存活"
        );
        let at_fault = survivor_published_at_fault.expect("缺少故障时 capture 基线");
        for (index, stats) in capture_stats.iter().enumerate() {
            if index != fault_index {
                assert!(
                    stats.frames_published > at_fault[index],
                    "设备 {} 在另一设备崩溃后没有继续出帧：{stats:?}",
                    index + 1
                );
            }
        }
    }
    assert!(
        resources_max
            .rss_bytes
            .saturating_sub(resources_before.rss_bytes)
            <= MAX_RSS_GROWTH_BYTES,
        "三 viewport RSS 增长超过 384 MiB：before={resources_before:?}, max={resources_max:?}"
    );
    assert!(
        resources_max.threads <= resources_before.threads + MAX_THREAD_GROWTH,
        "三 viewport 线程数无界增长：before={resources_before:?}, max={resources_max:?}"
    );
    assert!(
        resources_max.file_descriptors <= resources_before.file_descriptors + MAX_FD_GROWTH,
        "三 viewport fd 无界增长：before={resources_before:?}, max={resources_max:?}"
    );
    let tick_reports: Vec<_> = telemetry.iter().map(LatencyProbe::report).collect();
    for (index, report) in tick_reports.iter().enumerate() {
        assert!(
            report.frames_committed > 0,
            "设备 {} GTK 未提交新帧",
            index + 1
        );
        assert!(
            report.max_ui_pump_gap_micros < MAX_UI_PUMP_STALL_MICROS,
            "设备 {} GTK 主线程泵停顿超过 250ms：{report:?}",
            index + 1
        );
    }
    eprintln!(
        "multi viewport soak={soak_seconds}s, resources_before={resources_before:?}, resources_max={resources_max:?}, resources_after={resources_after:?}, capture={capture_stats:?}, viewport={tick_reports:?}"
    );

    window.set_child(None::<&gtk4::Widget>);
    window.close();
    drop(pictures);
    drop(telemetry);
    drop(clients);
    drop(container);
    iterate_main_context();
    for instance in launched.iter().rev() {
        runtime
            .block_on(emulator::stop_launched(instance))
            .expect("停止 multi managed 模拟器失败");
    }
    let log_paths: Vec<_> = launched
        .iter()
        .map(|instance| instance.log_path().to_path_buf())
        .collect();
    drop(launched);
    let auth_cleanup_deadline = Instant::now() + Duration::from_secs(2);
    while auth_dirs.iter().any(|path| path.exists()) && Instant::now() < auth_cleanup_deadline {
        iterate_main_context();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        auth_dirs.iter().all(|path| !path.exists()),
        "multi gRPC session auth 目录未全部清理：{auth_dirs:?}"
    );
    for name in &avd_names {
        avd::delete_avd(name).expect("删除 multi 测试 AVD 失败");
    }
    for path in shm_paths.into_iter().chain(log_paths) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("log.previous"));
    }
}
