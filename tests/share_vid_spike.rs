//! WP-1.2：生产 `share-vid` capture 链路实机验证。
//!
//! 运行：
//! `AVDM_SDK_ROOT=/path/to/sdk cargo test --test share_vid_spike -- --ignored --nocapture`
//!
//! 非 FHS 环境可额外设置 `LITEAVD_EMULATOR_LD_LIBRARY_PATH`；它只会在测试
//! 进程启动后传给模拟器，不影响 Cargo/GTK 的动态链接。

use std::ffi::{OsStr, OsString};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use liteavd::core::avd::{self, AvdSpec, GpuMode, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::repo::{Archive, SystemImage};
use liteavd::core::stream::{BYTES_PER_PIXEL, SHARE_VID_HEADER_LEN, share_vid_path};

const CONSOLE_PORT: u16 = 5580;
const GRPC_PORT: u16 = 8580;

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

fn assert_ports_available() {
    for port in [CONSOLE_PORT, CONSOLE_PORT + 1, GRPC_PORT] {
        drop(
            TcpListener::bind(("127.0.0.1", port))
                .unwrap_or_else(|error| panic!("测试端口 {port} 已占用：{error}")),
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "需要已安装的 SDK/system image、KVM 和空闲端口"]
async fn production_share_vid_capture() {
    let sdk_root = sdk_root();
    let _emulator_ld = std::env::var_os("LITEAVD_EMULATOR_LD_LIBRARY_PATH")
        .map(|value| EnvGuard::set("LD_LIBRARY_PATH", value));
    assert_ports_available();
    assert!(
        !emulator::list_running_for_sdk(&sdk_root)
            .iter()
            .any(|instance| instance.console_port == CONSOLE_PORT),
        "console 端口已有模拟器实例"
    );

    let shm_path = share_vid_path(CONSOLE_PORT);
    if shm_path.exists() {
        std::fs::remove_file(&shm_path).expect("删除无进程持有的陈旧 share-vid shm 失败");
    }
    let avd_name = format!("liteavd_shm_{}", std::process::id());
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
        shm_path: shm_path.clone(),
    };

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
    let launched = emulator::launch(&params)
        .await
        .expect("生产 launch/share-vid capture 启动失败");
    let log_path = launched.log_path().to_path_buf();
    let mut subscription = launched
        .capture_subscription()
        .expect("share_vid=true 未创建 capture subscription");

    let first = subscription
        .wait_timeout(Duration::from_secs(90))
        .expect("90 秒内未捕获到 share-vid 帧");
    assert_eq!(
        (first.meta.width, first.meta.height),
        (1080, 1920),
        "pixel_2 分辨率"
    );
    assert_eq!(first.meta.fps, 60);
    assert_eq!(first.meta.stride as usize, 1080 * BYTES_PER_PIXEL);
    assert_eq!(
        first.pixels.len(),
        first.meta.stride as usize * first.meta.height as usize
    );
    assert_eq!(
        std::fs::metadata(&shm_path)
            .expect("读取 shm metadata 失败")
            .len(),
        (SHARE_VID_HEADER_LEN + first.pixels.len()) as u64
    );

    liteavd::core::adb::wait_for_boot(
        &sdk_root,
        &format!("emulator-{CONSOLE_PORT}"),
        Duration::from_secs(180),
    )
    .await
    .expect("等待 boot 完成失败");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut counters = vec![first.meta.frame_counter];
    while counters.len() < 12 && Instant::now() < deadline {
        if let Some(frame) = subscription.wait_timeout(Duration::from_secs(2))
            && counters.last() != Some(&frame.meta.frame_counter)
        {
            counters.push(frame.meta.frame_counter);
        }
    }
    assert!(counters.len() >= 6, "20 秒内帧更新过少：{counters:?}");
    assert!(
        counters.windows(2).all(|pair| pair[1] > pair[0]),
        "帧计数未单调递增：{counters:?}"
    );

    let stats = launched.capture_stats().expect("capture stats 缺失");
    assert!(stats.frames_published >= counters.len() as u64);
    assert!(stats.last_copy_micros > 0);
    eprintln!("share-vid counters={counters:?}, stats={stats:?}");

    emulator::stop_launched(&launched)
        .await
        .expect("停止 managed 模拟器失败");
    drop(launched);
    assert!(
        subscription.is_closed(),
        "session drop 后 capture 线程未关闭"
    );
    avd::delete_avd(&avd_name).expect("删除测试 AVD 失败");
    let _ = std::fs::remove_file(&shm_path);
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_file(log_path.with_extension("log.previous"));
}
