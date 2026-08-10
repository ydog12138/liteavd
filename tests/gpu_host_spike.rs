//! 阶段 5.5.1 spike 续：无头 GPU 渲染验证。
//! 尝试 EGL_PLATFORM=surfaceless + `-gpu host` 是否能在无 DISPLAY 下用上 RADV（真 GPU）。
//! 运行：AVDM_SDK_ROOT=/home/haoran/liteavd-sdk cargo test --test gpu_host_spike -- --ignored --nocapture

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use liteavd::core::adb;
use liteavd::core::avd::{self, AvdSpec, GpuMode, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::stream::share_vid_path;

const CONSOLE_PORT: u16 = 5582;
const GRPC_PORT: u16 = 8582;

fn kill_and_reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn sdk_root() -> PathBuf {
    PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT（已装 SDK 目录）"))
}

fn installed_image(root: &Path) -> liteavd::core::repo::SystemImage {
    let imgs = root.join("system-images");
    for api in std::fs::read_dir(&imgs).unwrap().flatten() {
        for tag in std::fs::read_dir(api.path()).unwrap().flatten() {
            for abi in std::fs::read_dir(tag.path()).unwrap().flatten() {
                if abi.path().join("system.img").is_file() {
                    return liteavd::core::repo::SystemImage {
                        api: api.file_name().to_string_lossy().into_owned(),
                        tag: tag.file_name().to_string_lossy().into_owned(),
                        abi: abi.file_name().to_string_lossy().into_owned(),
                        display_name: String::new(),
                        license_ids: vec![],
                        archive: liteavd::core::repo::Archive {
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

fn render_nodes(pid: u32) -> Vec<PathBuf> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .filter(|target| target.starts_with("/dev/dri/"))
        .collect()
}

fn run_scenario(gpu: &str, start_xvfb: bool, qt_hide_window: bool) {
    let root = sdk_root();
    let name = format!(
        "liteavd_gpu_{}_{}",
        std::process::id(),
        gpu.replace(['-', ' '], "_")
    );

    let image = {
        let imgs = root.join("system-images");
        let mut found = None;
        for api in std::fs::read_dir(&imgs).unwrap().flatten() {
            for tag in std::fs::read_dir(api.path()).unwrap().flatten() {
                for abi in std::fs::read_dir(tag.path()).unwrap().flatten() {
                    if abi.path().is_dir() {
                        found = Some(liteavd::core::repo::SystemImage {
                            api: api.file_name().to_string_lossy().into_owned(),
                            tag: tag.file_name().to_string_lossy().into_owned(),
                            abi: abi.file_name().to_string_lossy().into_owned(),
                            display_name: String::new(),
                            license_ids: vec![],
                            archive: liteavd::core::repo::Archive {
                                url: String::new(),
                                size: 0,
                                checksum: None,
                                host_os: None,
                                host_arch: None,
                            },
                        });
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        found.expect("SDK 中未找到已安装系统镜像")
    };

    avd::create_avd(&AvdSpec {
        name: name.clone(),
        device: avd::get_profile("pixel_2").unwrap(),
        image,
        ram_mb: 1536,
        data_partition_mb: 4096,
        sdcard: None,
        gpu: GpuMode::Host,
    })
    .expect("创建 AVD 失败");

    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = avd::delete_avd(&self.0);
        }
    }
    let _cleanup = Cleanup(name.clone());

    // 可选 Xvfb 虚拟 display
    let mut xvfb = None;
    let mut display_var: Option<String> = None;
    if start_xvfb {
        display_var = Some(":97".to_string());
        xvfb = Some(
            std::process::Command::new("Xvfb")
                .arg(":97")
                .arg("-screen")
                .arg("0")
                .arg("1280x1024x24")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("Xvfb 启动失败"),
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    let log = std::env::temp_dir().join(format!("liteavd-gpu-{gpu}-{}.log", std::process::id()));
    let mut cmd = std::process::Command::new(root.join("emulator/emulator"));
    cmd.arg("-avd")
        .arg(&name)
        .arg("-port")
        .arg(CONSOLE_PORT.to_string())
        .arg("-gpu")
        .arg(gpu);
    if qt_hide_window {
        cmd.arg("-qt-hide-window");
    } else {
        cmd.arg("-no-window");
    }
    cmd.arg("-no-audio")
        .arg("-no-boot-anim")
        .arg("-verbose")
        .env("ANDROID_EMU_ENABLE_GLES_DEQP", "0");
    if let Some(d) = &display_var {
        cmd.env("DISPLAY", d);
    }
    cmd.stdout(std::fs::File::create(&log).unwrap())
        .stderr(std::fs::File::create(&log).unwrap());
    let mut child = cmd.spawn().expect("启动模拟器失败");
    let pid = child.id();

    // 轮询广告文件确认渲染器已初始化
    let t0 = Instant::now();
    let mut engine_pid = None;
    let deadline = Instant::now() + std::time::Duration::from_secs(150);
    while Instant::now() < deadline {
        engine_pid = liteavd::core::emulator::find_running(CONSOLE_PORT).map(|i| i.pid);
        if engine_pid.is_some() {
            break;
        }
        if let Some(status) = child.try_wait().expect("wait 失败") {
            println!("== 模拟器提前退出 status={status}，日志见 {log:?}");
            let _ = child.wait();
            if let Some(mut xvfb) = xvfb {
                kill_and_reap(&mut xvfb);
            }
            return;
        }
        if Instant::now()
            .duration_since(t0)
            .as_secs()
            .is_multiple_of(20)
        {
            let dir = std::path::Path::new("/run/user/1000/avd/running");
            let entries: Vec<String> = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            println!(
                "== t={}s 广告目录：{:?}",
                Instant::now().duration_since(t0).as_secs(),
                entries
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let Some(engine_pid) = engine_pid else {
        kill_and_reap(&mut child);
        println!("== 150s 内未找到运行实例，日志见 {log:?}");
        if let Some(mut xvfb) = xvfb {
            kill_and_reap(&mut xvfb);
        }
        return;
    };
    println!("== 引擎 pid={engine_pid}");

    // 检查进程加载的渲染驱动：radv=真 GPU，swiftshader=CPU
    std::thread::sleep(std::time::Duration::from_secs(3));
    let maps = std::fs::read_to_string(format!("/proc/{engine_pid}/maps")).unwrap_or_default();
    let has_radv = maps.contains("radeonsi") || maps.contains("radv");
    let has_swiftshader = maps.contains("swiftshader") || maps.contains("libvulkan_swiftshader");
    let has_angle = maps.contains("libEGL") || maps.contains("libGLES");
    let has_llvmpipe = maps.contains("llvmpipe") || maps.contains("swrast");
    println!(
        "== 进程内存映射：radv/radeonsi={has_radv}，swiftshader={has_swiftshader}，llvmpipe={has_llvmpipe}，EGL/GLES={has_angle}"
    );

    // boot 判定
    let serial = format!("emulator-{CONSOLE_PORT}");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let booted = rt.block_on(liteavd::core::adb::wait_for_boot(
        &root,
        &serial,
        std::time::Duration::from_secs(180),
    ));
    drop(rt);
    println!(
        "== boot：{:?}",
        booted.as_ref().map(|_| "完成").unwrap_or("失败")
    );

    // 渲染器选择日志
    let log_text = std::fs::read_to_string(&log).unwrap_or_default();
    for line in log_text.lines() {
        if line.contains("renderer")
            || line.contains("Renderer")
            || line.contains("gfxstream")
            || line.contains("EGL")
        {
            println!("日志: {line}");
        }
    }

    // 停止
    unsafe {
        let _ = libc::kill(pid as i32, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_secs(3));
    let _ = child.try_wait();
    if let Some(running) = liteavd::core::emulator::find_running(CONSOLE_PORT) {
        unsafe {
            let _ = libc::kill(running.pid as i32, libc::SIGKILL);
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    kill_and_reap(&mut child);

    // 结论输出（不强制断言——spike 以观察为准）
    println!("== 结论：booted={}", booted.is_ok());
    if booted.is_ok() && has_radv {
        println!("== 🎯 GPU={gpu} 成功：真 GPU 渲染（RADV）");
    } else if booted.is_ok() {
        println!("== GPU={gpu} boot 成功，但渲染为 CPU（swiftshader={has_swiftshader}）");
    } else {
        println!("== GPU={gpu} 失败：渲染器未能初始化");
    }

    if let Some(mut xvfb) = xvfb {
        kill_and_reap(&mut xvfb);
    }
}

#[test]
#[ignore]
fn gpu_angle_indirect() {
    run_scenario("angle_indirect", false, false);
}

#[test]
#[ignore]
fn gpu_host_xvfb() {
    run_scenario("host", true, false);
}

#[test]
#[ignore]
fn gpu_host_xvfb_qt() {
    run_scenario("host", true, true);
}

/// 使用正式 JWT/广告/share-vid/虚拟麦克风启动路径验证
/// 桌面 XWayland host GPU。
///
/// 测试调用方必须把 `ANDROID_AVD_HOME` 指向独立测试目录；测试只创建唯一 AVD，
/// 并在结束前清理 engine、端口、认证材料、shm 与 AVD 文件。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要 XWayland DISPLAY、独立测试 AVD home、SDK/system image、KVM 和空闲端口"]
async fn gpu_host_production_xwayland() {
    let display = std::env::var("DISPLAY").expect("host GPU 测试需要 DISPLAY");
    let avd_home = PathBuf::from(
        std::env::var("ANDROID_AVD_HOME").expect("必须设置独立测试 ANDROID_AVD_HOME"),
    );
    assert!(avd_home.is_dir(), "测试 AVD home 不存在：{avd_home:?}");
    assert!(
        std::fs::read_dir(&avd_home)
            .expect("读取独立测试 AVD home 失败")
            .next()
            .is_none(),
        "测试 AVD home 必须是空目录：{avd_home:?}"
    );
    let root = sdk_root();
    let name = format!("liteavd_gpu_product_{}", std::process::id());
    let share_path = share_vid_path(CONSOLE_PORT);
    if share_path.exists() {
        std::fs::remove_file(&share_path).expect("删除陈旧 share-vid 失败");
    }

    avd::create_avd(&AvdSpec {
        name: name.clone(),
        device: avd::get_profile("pixel_2").unwrap(),
        image: installed_image(&root),
        ram_mb: 1536,
        data_partition_mb: 4096,
        sdcard: None,
        gpu: GpuMode::Host,
    })
    .expect("创建 host-GPU 测试 AVD 失败");

    struct Cleanup {
        name: String,
        sdk_root: PathBuf,
        shm_path: PathBuf,
        avd_home: PathBuf,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(instance) = emulator::list_running_for_sdk(&self.sdk_root)
                .into_iter()
                .find(|instance| instance.avd_name == self.name)
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
            let _ = avd::delete_avd(&self.name);
            let _ = std::fs::remove_file(&self.shm_path);
            // The root is caller-owned but explicitly isolated by the test.
            // Remove lock/staging/partial files too, then leave the directory
            // itself available to the caller.
            if let Ok(entries) = std::fs::read_dir(&self.avd_home) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let _ = std::fs::remove_dir_all(path);
                    } else {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
        }
    }
    let _cleanup = Cleanup {
        name: name.clone(),
        sdk_root: root.clone(),
        shm_path: share_path,
        avd_home: avd_home.clone(),
    };

    let params = LaunchParams {
        sdk_root: root.clone(),
        avd_name: name.clone(),
        port: CONSOLE_PORT,
        grpc: GrpcLaunchConfig::new(GRPC_PORT).expect("创建 gRPC JWT 身份失败"),
        gpu_policy: ManagedGpuPolicy::DesktopHost,
        audio_policy: ManagedAudioPolicy::VirtualMicrophone { required: true },
        no_window: true,
        share_vid: true,
    };
    let launched = emulator::launch(&params).await.unwrap_or_else(|error| {
        panic!("XWayland {display} production host launch 失败：{error:#}")
    });
    let microphone = launched
        .microphone_endpoint()
        .expect("required virtual microphone 必须保留私有 endpoint");
    assert!(microphone.fifo_path.exists());
    assert!(
        !launched
            .grpc_client()
            .microphone_state()
            .await
            .expect("查询 host-GPU microphone 初始状态失败"),
        "host-GPU session 的 microphone 必须默认关闭"
    );
    let engine_pid = launched.instance.pid;
    let render_nodes = render_nodes(engine_pid);
    let maps = std::fs::read_to_string(format!("/proc/{engine_pid}/maps")).unwrap_or_default();
    let software_renderer = maps.contains("libvulkan_swiftshader")
        || maps.contains("libvk_swiftshader")
        || maps.contains("llvmpipe")
        || maps.contains("swrast");
    eprintln!(
        "host production launch: display={display}, engine={engine_pid}, render_nodes={render_nodes:?}, software_renderer={software_renderer}, log={}",
        launched.log_path().display()
    );
    assert!(
        !render_nodes.is_empty(),
        "host 模式 engine 没有打开任何 /dev/dri render node"
    );
    assert!(!software_renderer, "host 模式加载了已知软件 renderer");

    adb::wait_for_boot(
        &root,
        &format!("emulator-{CONSOLE_PORT}"),
        Duration::from_secs(180),
    )
    .await
    .expect("host GPU AVD boot 超时");
    let screenshot = launched
        .grpc_client()
        .screenshot(0, 0)
        .await
        .expect("host GPU 认证 screenshot 失败");
    assert!(screenshot.image.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert!(launched.capture_subscription().is_some());

    emulator::stop_launched(&launched)
        .await
        .expect("停止 host GPU managed 实例失败");
    drop(launched);
    let cleanup_deadline = Instant::now() + Duration::from_secs(3);
    while microphone.fifo_path.exists() && Instant::now() < cleanup_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!microphone.fifo_path.exists());
    assert!(!share_vid_path(CONSOLE_PORT).exists());
    assert!(emulator::find_running(CONSOLE_PORT).is_none());
}
