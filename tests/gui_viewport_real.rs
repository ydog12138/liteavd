//! WP-1.3：真实 Emulator → production capture → GTK viewport 纵切。
//!
//! 运行（默认显示 30 秒）：
//! `DISPLAY=:97 AVDM_SDK_ROOT=/path/to/sdk cargo test --test gui_viewport_real -- --ignored --nocapture`
//!
//! `LITEAVD_VIEWPORT_SOAK_SECONDS=1800` 用于长期门禁；非 FHS 环境可另设
//! `LITEAVD_EMULATOR_LD_LIBRARY_PATH`，只向模拟器子进程补充动态库。

use std::ffi::{OsStr, OsString};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gtk4::prelude::*;
use liteavd::core::avd::{self, AvdSpec, GpuMode, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc::{KeyEventType, MouseEvent, touch_event};
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::input::{GuestPoint, TouchSample};
use liteavd::core::repo::{Archive, SystemImage};
use liteavd::core::stream::share_vid_path;
use liteavd::ui::viewport::{PICTURE_WIDGET, build_interactive_measured};

const CONSOLE_PORT: u16 = 5582;
const GRPC_PORT: u16 = 8582;
const DEFAULT_SOAK_SECONDS: u64 = 30;
const DEFAULT_LATENCY_SAMPLES: usize = 8;
const MAX_RSS_GROWTH_BYTES: u64 = 128 * 1024 * 1024;
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
        // SAFETY: see `EnvGuard::set`; no sibling test can observe the temporary value.
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
    shm_path: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(instance) = emulator::list_running_for_sdk(&self.sdk_root)
            .into_iter()
            .find(|instance| instance.avd_name == self.avd_name)
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
        let _ = avd::delete_avd(&self.avd_name);
        let _ = std::fs::remove_file(&self.shm_path);
        let _ = std::fs::remove_dir_all(&self.avd_home);
    }
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

fn adb_output(sdk_root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new(sdk_root.join("platform-tools/adb"))
        .arg("-s")
        .arg(format!("emulator-{CONSOLE_PORT}"))
        .args(args)
        .output()
        .expect("执行 adb 失败");
    assert!(
        output.status.success(),
        "adb {args:?} 失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("adb 输出不是 UTF-8")
}

fn touch(point: GuestPoint, pressure: i32) -> liteavd::core::grpc::TouchEvent {
    touch_event(TouchSample {
        point,
        identifier: 0,
        pressure,
    })
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

#[test]
#[ignore = "需要 Xvfb/Wayland、已安装 SDK/system image、KVM 和空闲端口"]
fn production_capture_renders_in_gtk_viewport() {
    gtk4::init().expect("gtk4 初始化失败");
    let sdk_root = sdk_root();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 UNIX epoch")
        .as_nanos();
    let avd_home = std::env::temp_dir().join(format!(
        "liteavd-gui-viewport-real-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&avd_home).expect("创建临时 AVD home 失败");
    let _avd_home_env = EnvGuard::set("ANDROID_AVD_HOME", &avd_home);
    let _emulator_ld = std::env::var_os("LITEAVD_EMULATOR_LD_LIBRARY_PATH")
        .map(|value| EnvGuard::set("LD_LIBRARY_PATH", value));

    for port in [CONSOLE_PORT, CONSOLE_PORT + 1, GRPC_PORT] {
        drop(
            TcpListener::bind(("127.0.0.1", port))
                .unwrap_or_else(|error| panic!("测试端口 {port} 已占用：{error}")),
        );
    }
    let shm_path = share_vid_path(CONSOLE_PORT);
    if shm_path.exists() {
        std::fs::remove_file(&shm_path).expect("删除陈旧 share-vid shm 失败");
    }
    let avd_name = format!("liteavd_gui_{}", std::process::id());
    avd::create_avd(&AvdSpec {
        name: avd_name.clone(),
        device: avd::get_profile("pixel_2").expect("缺少 pixel_2 profile"),
        image: installed_image(&sdk_root),
        ram_mb: 1536,
        data_partition_mb: 4096,
        sdcard: None,
        gpu: GpuMode::SwangleIndirect,
    })
    .expect("创建 AVD 失败");
    let _cleanup = Cleanup {
        avd_name: avd_name.clone(),
        sdk_root: sdk_root.clone(),
        avd_home,
        shm_path: shm_path.clone(),
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("创建 tokio runtime 失败");
    let params = LaunchParams {
        sdk_root: sdk_root.clone(),
        avd_name: avd_name.clone(),
        port: CONSOLE_PORT,
        grpc: GrpcLaunchConfig::new(GRPC_PORT).expect("创建 gRPC JWT 身份失败"),
        gpu_policy: ManagedGpuPolicy::HeadlessSwangle,
        audio_policy: ManagedAudioPolicy::Disabled,
        no_window: true,
        share_vid: true,
    };
    let auth_dir = params
        .grpc
        .auth()
        .allowlist_path()
        .parent()
        .expect("allowlist 缺少 session 目录")
        .to_path_buf();
    let launched = runtime
        .block_on(emulator::launch(&params))
        .expect("production launch 失败");
    let auth_weak = Arc::downgrade(launched.grpc_auth());
    let log_path = launched.log_path().to_path_buf();
    let mut capture_probe = launched
        .capture_subscription()
        .expect("production launch 缺少 capture probe");
    let client = launched.grpc_client().clone();
    let interactive = build_interactive_measured(
        launched
            .capture_subscription()
            .expect("production launch 缺少 viewport capture"),
        client.clone(),
    );
    let telemetry = interactive.telemetry;
    let root = interactive.root;
    let root_weak = root.downgrade();
    let picture = find_picture(root.upcast_ref());
    let window = gtk4::Window::new();
    window.set_title(Some("liteavd viewport real test"));
    window.set_default_size(480, 720);
    window.set_child(Some(&root));
    window.present();

    let frame_deadline = Instant::now() + Duration::from_secs(30);
    while picture.paintable().is_none() {
        iterate_main_context();
        assert!(Instant::now() < frame_deadline, "30 秒内 GTK 未显示真实帧");
        std::thread::sleep(Duration::from_millis(5));
    }
    let paintable = picture.paintable().unwrap();
    assert_eq!(
        (paintable.intrinsic_width(), paintable.intrinsic_height()),
        (1080, 1920)
    );

    runtime
        .block_on(liteavd::core::adb::wait_for_boot(
            &sdk_root,
            &format!("emulator-{CONSOLE_PORT}"),
            Duration::from_secs(180),
        ))
        .expect("等待 boot 完成失败");
    runtime
        .block_on(async {
            client.send_key("GoHome", KeyEventType::Keypress).await?;
            client
                .send_mouse(MouseEvent {
                    x: 540,
                    y: 960,
                    buttons: 0,
                    display: 0,
                })
                .await?;
            client.send_text("liteavd").await?;
            Ok::<(), liteavd::core::grpc::InputRpcError>(())
        })
        .expect("真实 key/mouse/text RPC 失败");
    std::thread::sleep(Duration::from_millis(750));

    let controllers = picture.observe_controllers();
    let drag = (0..controllers.n_items())
        .find_map(|index| {
            controllers
                .item(index)?
                .downcast::<gtk4::GestureDrag>()
                .ok()
        })
        .expect("interactive viewport 缺少 GestureDrag");
    let width = f64::from(picture.width());
    let height = f64::from(picture.height());
    drag.emit_by_name::<()>("drag-begin", &[&(width / 2.0), &(height * 0.8)]);
    drag.emit_by_name::<()>("drag-update", &[&0.0_f64, &(-height * 0.55)]);
    drag.emit_by_name::<()>("drag-end", &[&0.0_f64, &(-height * 0.55)]);
    let input_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < input_deadline {
        iterate_main_context();
        std::thread::sleep(Duration::from_millis(5));
    }
    let before_rotation = runtime
        .block_on(client.screenshot(0, 0))
        .expect("滑动后截图失败")
        .image;

    let published_before_rotation = launched.capture_stats().unwrap().frames_published;
    let frame_before_rotation = capture_probe
        .take_latest()
        .expect("旋转前没有可用 share-vid 帧");
    let screenshot_before_rotation = runtime
        .block_on(client.screenshot(0, 0))
        .expect("旋转前截图失败");
    let rotation_before = screenshot_before_rotation
        .format
        .as_ref()
        .and_then(|format| format.rotation)
        .expect("旋转前截图缺少 rotation")
        .rotation;
    let rotation_output = adb_output(&sdk_root, &["emu", "rotate"]);

    // console rotate 改变模拟器物理姿态；share-vid 暴露的显示 buffer 在
    // Emulator 37.1.11 上仍可保持 1080x1920，不能把宽高互换当成协议。
    let rotation_deadline = Instant::now() + Duration::from_secs(10);
    let rotated_screenshot = loop {
        let screenshot = runtime
            .block_on(client.screenshot(0, 0))
            .expect("轮询旋转后截图失败");
        if screenshot
            .format
            .as_ref()
            .and_then(|format| format.rotation)
            .is_some_and(|rotation| rotation.rotation != rotation_before)
        {
            break screenshot;
        }
        assert!(
            Instant::now() < rotation_deadline,
            "10 秒内 gRPC screenshot rotation 没有变化；adb emu 输出：{rotation_output:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    let frame_deadline = Instant::now() + Duration::from_secs(5);
    let rotated_frame = loop {
        iterate_main_context();
        if let Some(frame) = capture_probe.take_latest()
            && frame.meta.frame_counter > frame_before_rotation.meta.frame_counter
        {
            break frame;
        }
        assert!(
            Instant::now() < frame_deadline,
            "物理旋转后 5 秒内 share-vid 没有继续更新"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let texture_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        iterate_main_context();
        if picture.paintable().is_some_and(|paintable| {
            (paintable.intrinsic_width(), paintable.intrinsic_height())
                == (
                    rotated_frame.meta.width as i32,
                    rotated_frame.meta.height as i32,
                )
        }) {
            break;
        }
        assert!(
            Instant::now() < texture_deadline,
            "GTK texture 未跟随旋转后的实际 share-vid buffer"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        launched.capture_stats().unwrap().frames_published > published_before_rotation,
        "旋转后 capture 没有发布新帧"
    );
    let screenshot_format = rotated_screenshot.format.expect("旋转后截图缺少 format");
    assert!(screenshot_format.width > 0 && screenshot_format.height > 0);
    assert!(
        before_rotation != rotated_screenshot.image,
        "Android 物理旋转后截图没有变化"
    );
    let center = GuestPoint {
        x: i32::try_from(rotated_frame.meta.width / 2).expect("frame width 超过 i32"),
        y: i32::try_from(rotated_frame.meta.height / 2).expect("frame height 超过 i32"),
    };
    runtime
        .block_on(async {
            client.send_touch(touch(center, 1)).await?;
            client.send_touch(touch(center, 0)).await
        })
        .expect("旋转后按实际 share-vid 坐标触摸失败");
    eprintln!(
        "adb emu rotate={rotation_output:?}, share_vid={}x{}, screenshot={}x{} rotation={:?}",
        rotated_frame.meta.width,
        rotated_frame.meta.height,
        screenshot_format.width,
        screenshot_format.height,
        screenshot_format.rotation.map(|value| value.rotation),
    );

    let measurement_warmup = Instant::now() + Duration::from_secs(1);
    while Instant::now() < measurement_warmup {
        iterate_main_context();
        std::thread::sleep(Duration::from_millis(2));
    }
    telemetry.reset_measurement();
    let measurement_client = runtime
        .block_on(client.reconnect())
        .expect("延迟测量前刷新 gRPC 连接失败");

    let latency_samples = std::env::var("LITEAVD_LATENCY_SAMPLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_LATENCY_SAMPLES);
    let latency_deadline = Instant::now()
        + Duration::from_secs(u64::try_from(latency_samples).unwrap_or(u64::MAX).max(1));
    let mut open_app_switcher = true;
    let mut next_progress = 100;
    while telemetry.report().sample_count < latency_samples {
        let before = telemetry.report().sample_count;
        let key = if open_app_switcher {
            "AppSwitch"
        } else {
            "GoBack"
        };
        let token = telemetry.begin_input(Instant::now());
        telemetry.mark_rpc_started(token, Instant::now());
        let result = runtime.block_on(measurement_client.send_key(key, KeyEventType::Keypress));
        telemetry.mark_rpc_completed(token, Instant::now(), result.is_ok());
        result.unwrap_or_else(|error| panic!("延迟刺激 {key} gRPC 失败：{error}"));
        open_app_switcher = !open_app_switcher;
        let stimulus_deadline = Instant::now() + Duration::from_secs(3);
        while telemetry.report().sample_count == before {
            iterate_main_context();
            assert!(
                Instant::now() < stimulus_deadline && Instant::now() < latency_deadline,
                "延迟样本在单次刺激时限内未推进：{:?}",
                telemetry.report()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let collected = telemetry.report().sample_count;
        if latency_samples >= 500 && collected >= next_progress {
            eprintln!("latency progress: {collected}/{latency_samples}");
            next_progress += 100;
        }
    }
    let pending_deadline = Instant::now() + Duration::from_secs(3);
    while telemetry.report().pending_inputs > 0 {
        iterate_main_context();
        assert!(
            Instant::now() < pending_deadline,
            "延迟测量窗口结束后仍有 pending：{:?}",
            telemetry.report()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let latency = telemetry.report();
    assert_eq!(latency.input_failures, 0, "真实输入 RPC 出现失败");
    assert_eq!(latency.dropped_pending_inputs, 0, "延迟 pending 队列溢出");
    if latency_samples >= 500 {
        assert!(
            latency.end_to_end.p95_micros < 50_000,
            "输入到新 GTK texture 的 p95 未达到 50ms：{latency:?}"
        );
    }

    let soak_seconds = std::env::var("LITEAVD_VIEWPORT_SOAK_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SOAK_SECONDS);
    let warmup_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < warmup_deadline {
        iterate_main_context();
        std::thread::sleep(Duration::from_millis(5));
    }
    let resources_before = process_resources();
    let mut resources_max = resources_before;
    let published_before = launched.capture_stats().unwrap().frames_published;
    let soak_started = Instant::now();
    let soak_deadline = soak_started + Duration::from_secs(soak_seconds);
    let mut next_resource_sample = Instant::now();
    let mut next_stimulus = Instant::now();
    let mut next_progress = soak_started + Duration::from_secs(300);
    while Instant::now() < soak_deadline {
        iterate_main_context();
        if Instant::now() >= next_stimulus {
            runtime
                .block_on(measurement_client.send_key("Power", KeyEventType::Keypress))
                .expect("viewport soak Power 刺激失败");
            next_stimulus = Instant::now() + Duration::from_secs(15);
        }
        if Instant::now() >= next_resource_sample {
            let resources = process_resources();
            resources_max.rss_bytes = resources_max.rss_bytes.max(resources.rss_bytes);
            resources_max.threads = resources_max.threads.max(resources.threads);
            resources_max.file_descriptors = resources_max
                .file_descriptors
                .max(resources.file_descriptors);
            next_resource_sample = Instant::now() + Duration::from_secs(1);
        }
        if Instant::now() >= next_progress {
            eprintln!(
                "viewport soak progress: {}s/{soak_seconds}s, current={:?}, max={resources_max:?}",
                soak_started.elapsed().as_secs(),
                process_resources(),
            );
            next_progress += Duration::from_secs(300);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let stats = launched.capture_stats().unwrap();
    let resources_after = process_resources();
    assert!(
        stats.frames_published > published_before,
        "soak 期间 capture 没有发布新帧：{stats:?}"
    );
    assert!(
        resources_max
            .rss_bytes
            .saturating_sub(resources_before.rss_bytes)
            <= MAX_RSS_GROWTH_BYTES,
        "viewport RSS 增长超过 128 MiB：before={resources_before:?}, max={resources_max:?}"
    );
    assert!(
        resources_max.threads <= resources_before.threads + MAX_THREAD_GROWTH,
        "viewport 线程数无界增长：before={resources_before:?}, max={resources_max:?}"
    );
    assert!(
        resources_max.file_descriptors <= resources_before.file_descriptors + MAX_FD_GROWTH,
        "viewport fd 数无界增长：before={resources_before:?}, max={resources_max:?}"
    );
    let soak_telemetry = telemetry.report();
    assert!(
        soak_telemetry.max_ui_pump_gap_micros < MAX_UI_PUMP_STALL_MICROS,
        "viewport 主线程泵停顿超过 250ms：{soak_telemetry:?}"
    );
    eprintln!(
        "viewport soak={soak_seconds}s, resources_before={resources_before:?}, resources_max={resources_max:?}, resources_after={resources_after:?}, stats={stats:?}, ui_pump_gap={:?}",
        soak_telemetry.ui_pump_gap,
    );
    eprintln!("latency={latency:?}");

    // observe_controllers() 返回一个持续跟踪 widget/controller 的模型。
    // 测试探针必须在 viewport 拆除前显式释放，否则 JWT 清理时序会
    // 取决于 GTK 何时回收该模型，而不是产品 viewport 的生命周期。
    drop(drag);
    drop(controllers);
    drop(paintable);
    gtk4::prelude::GtkWindowExt::set_focus(&window, None::<&gtk4::Widget>);
    window.set_child(None::<&gtk4::Widget>);
    window.close();
    drop(window);
    drop(picture);
    drop(root);
    let viewport_cleanup_deadline = Instant::now() + Duration::from_secs(2);
    while root_weak.upgrade().is_some() && Instant::now() < viewport_cleanup_deadline {
        iterate_main_context();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        root_weak.upgrade().is_none(),
        "viewport root 仍被 GTK 对象持有"
    );
    runtime
        .block_on(emulator::stop_launched(&launched))
        .expect("停止 managed 模拟器失败");
    drop(launched);
    drop(measurement_client);
    drop(client);
    drop(params);
    let auth_cleanup_deadline = Instant::now() + Duration::from_secs(2);
    while auth_weak.strong_count() > 0 && Instant::now() < auth_cleanup_deadline {
        iterate_main_context();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        auth_weak.strong_count(),
        0,
        "gRPC session auth 仍被 Arc 持有"
    );
    assert!(
        !auth_dir.exists(),
        "gRPC session auth 目录未清理：{auth_dir:?}"
    );
    avd::delete_avd(&avd_name).expect("删除测试 AVD 失败");
    let _ = std::fs::remove_file(&shm_path);
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(log_path.with_extension("log.previous"));
}
