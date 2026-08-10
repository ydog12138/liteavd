//! WP-0.3：模拟器 gRPC 暴露与匿名访问矩阵。
//!
//! 该测试只使用临时 `ANDROID_AVD_HOME`，顺序验证默认参数与显式 `-grpc`。
//! 运行：
//! `AVDM_SDK_ROOT=/path/to/sdk cargo test --test grpc_auth_spike -- --ignored --nocapture`
//!
//! 非 FHS 环境可额外设置 `LITEAVD_EMULATOR_LD_LIBRARY_PATH`；它只会在测试进程
//! 已启动后传给模拟器子进程，不会影响 Cargo/GTK 的动态链接。

use std::ffi::{OsStr, OsString};
use std::net::{IpAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use liteavd::core::avd::{self, AvdSpec, GpuMode, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy, RunningInstance};
use liteavd::core::grpc::{EmulatorControllerClient, GrpcClient};
use liteavd::core::grpc_auth::{GrpcJwtAuth, GrpcLaunchConfig};
use liteavd::core::repo::Archive;

const DEFAULT_CONSOLE_PORT: u16 = 5570;
const EXPLICIT_CONSOLE_PORT: u16 = 5572;
const EXPLICIT_GRPC_PORT: u16 = 8571;

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: this integration-test binary has one test, so no other test thread can
        // observe the temporary process environment.
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see `EnvGuard::set`; the single test restores the original value.
        unsafe {
            if let Some(value) = &self.previous {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

struct TestWorkspace {
    avd_home: PathBuf,
    _avd_home_env: EnvGuard,
    _emulator_ld_env: Option<EnvGuard>,
}

impl TestWorkspace {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间早于 UNIX epoch")
            .as_nanos();
        let avd_home =
            std::env::temp_dir().join(format!("liteavd-grpc-auth-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&avd_home).expect("创建临时 AVD home 失败");
        let avd_home_env = EnvGuard::set("ANDROID_AVD_HOME", &avd_home);
        let emulator_ld_env = std::env::var_os("LITEAVD_EMULATOR_LD_LIBRARY_PATH")
            .map(|value| EnvGuard::set("LD_LIBRARY_PATH", value));
        Self {
            avd_home,
            _avd_home_env: avd_home_env,
            _emulator_ld_env: emulator_ld_env,
        }
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.avd_home) {
            eprintln!("清理临时 AVD home 失败：{error}");
        }
    }
}

struct RunningCleanup {
    instance: Option<RunningInstance>,
    child: Option<tokio::process::Child>,
    sdk_root: PathBuf,
    avd_name: Option<String>,
}

impl RunningCleanup {
    fn new(
        instance: RunningInstance,
        child: tokio::process::Child,
        sdk_root: PathBuf,
        avd_name: String,
    ) -> Self {
        Self {
            instance: Some(instance),
            child: Some(child),
            sdk_root,
            avd_name: Some(avd_name),
        }
    }

    fn managed(instance: RunningInstance, sdk_root: PathBuf, avd_name: String) -> Self {
        Self {
            instance: Some(instance),
            child: None,
            sdk_root,
            avd_name: Some(avd_name),
        }
    }

    fn instance(&self) -> &RunningInstance {
        self.instance.as_ref().expect("实例已清理")
    }

    async fn finish(mut self) {
        if let Some(instance) = self.instance.take() {
            if let Some(mut child) = self.child.take() {
                unsafe {
                    libc::kill(instance.pid as i32, libc::SIGTERM);
                }
                if tokio::time::timeout(Duration::from_secs(10), child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                }
                let _ = std::fs::remove_file(instance.ini_path);
            } else {
                emulator::stop(instance.pid, &self.sdk_root)
                    .await
                    .expect("停止测试模拟器失败");
            }
        }
        if let Some(name) = self.avd_name.take() {
            avd::delete_avd(&name).expect("删除测试 AVD 失败");
        }
    }
}

impl Drop for RunningCleanup {
    fn drop(&mut self) {
        if let Some(instance) = self.instance.take()
            && emulator::verify_emulator_pid(instance.pid, &self.sdk_root)
        {
            // Panic/early-return fallback. Normal cleanup uses async `emulator::stop`.
            unsafe {
                libc::kill(instance.pid as i32, libc::SIGTERM);
            }
            for _ in 0..50 {
                if !Path::new(&format!("/proc/{}", instance.pid)).exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if Path::new(&format!("/proc/{}", instance.pid)).exists() {
                unsafe {
                    libc::kill(instance.pid as i32, libc::SIGKILL);
                }
            }
        }
        if let Some(child) = self.child.as_mut()
            && let Some(pid) = child.id()
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
        if let Some(name) = self.avd_name.take() {
            let _ = avd::delete_avd(&name);
        }
    }
}

#[derive(Debug)]
struct CaseEvidence {
    requested_grpc_port: Option<u16>,
    advertised_grpc_port: u16,
    listeners: Vec<String>,
    allowlist: Option<String>,
    token_advertised: bool,
    anonymous_status_succeeded: bool,
}

fn sdk_root() -> PathBuf {
    let root = PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT"));
    assert!(
        root.join("emulator/emulator").is_file(),
        "SDK 缺少 emulator"
    );
    assert!(
        root.join("system-images/android-35/google_apis/x86_64/system.img")
            .is_file(),
        "测试 SDK 缺少 android-35/google_apis/x86_64"
    );
    root
}

fn sample_image() -> liteavd::core::repo::SystemImage {
    liteavd::core::repo::SystemImage {
        api: "android-35".into(),
        tag: "google_apis".into(),
        abi: "x86_64".into(),
        display_name: String::new(),
        license_ids: vec![],
        archive: Archive {
            url: String::new(),
            size: 0,
            checksum: None,
            host_os: None,
            host_arch: None,
        },
    }
}

fn create_avd(name: &str) {
    let spec = AvdSpec {
        name: name.into(),
        device: avd::get_profile("pixel_2").expect("缺少 pixel_2 profile"),
        image: sample_image(),
        ram_mb: 1536,
        data_partition_mb: 4096,
        sdcard: None,
        gpu: GpuMode::SwangleIndirect,
    };
    avd::create_avd(&spec).expect("创建隔离测试 AVD 失败");
}

fn assert_ports_available(ports: &[u16]) {
    for port in ports {
        let listener = TcpListener::bind(("127.0.0.1", *port))
            .unwrap_or_else(|error| panic!("测试端口 {port} 已占用：{error}"));
        drop(listener);
    }
}

fn listen_addrs(port: u16) -> Vec<String> {
    let filter = format!("sport = :{port}");
    let output = std::process::Command::new("ss")
        .args(["-H", "-ltn", &filter])
        .output()
        .expect("ss 不可用");
    assert!(output.status.success(), "ss 查询失败");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(3).map(str::to_owned))
        .collect()
}

fn is_loopback_listener(address: &str) -> bool {
    let Some((host, _port)) = address.rsplit_once(':') else {
        return false;
    };
    let host = host.trim_matches(['[', ']']);
    let Ok(ip) = host.parse::<IpAddr>() else {
        return false;
    };
    ip.is_loopback()
        || matches!(ip, IpAddr::V6(ipv6) if ipv6.to_ipv4_mapped().is_some_and(|ipv4| ipv4.is_loopback()))
}

async fn run_insecure_case(sdk_root: &Path) -> CaseEvidence {
    let console_port = EXPLICIT_CONSOLE_PORT;
    let avd_name = "liteavd-grpc-insecure";
    create_avd(avd_name);
    let mut command = tokio::process::Command::new(sdk_root.join("emulator/emulator"));
    command
        .arg("-avd")
        .arg(avd_name)
        .arg("-port")
        .arg(console_port.to_string())
        .arg("-gpu")
        .arg(GpuMode::SwangleIndirect.as_str())
        .arg("-no-window")
        .arg("-no-audio")
        .arg("-no-boot-anim")
        .arg("-grpc")
        .arg(EXPLICIT_GRPC_PORT.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command.kill_on_drop(true);
    let mut child = command.spawn().expect("启动模拟器失败");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let instance = loop {
        if let Some(instance) = emulator::list_running()
            .into_iter()
            .find(|instance| instance.avd_name == avd_name && instance.console_port == console_port)
        {
            break instance;
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill().await;
            panic!("等待广告文件超时");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let cleanup = RunningCleanup::new(instance, child, sdk_root.to_path_buf(), avd_name.into());
    let instance = cleanup.instance();

    assert_eq!(
        instance.grpc_port, EXPLICIT_GRPC_PORT,
        "显式 gRPC 端口未生效"
    );
    assert_ne!(instance.grpc_port, 0, "广告文件没有有效 gRPC 端口");
    let advertisement = std::fs::read_to_string(&instance.ini_path).expect("读取广告文件失败");
    eprintln!("gRPC advertisement:\n{advertisement}");
    let listeners = listen_addrs(instance.grpc_port);
    assert!(!listeners.is_empty(), "gRPC 端口没有 TCP listener");

    let anonymous_status_succeeded = anonymous_status(instance.grpc_port).await.is_ok();

    let evidence = CaseEvidence {
        requested_grpc_port: Some(EXPLICIT_GRPC_PORT),
        advertised_grpc_port: instance.grpc_port,
        listeners,
        allowlist: instance.grpc_allowlist.clone(),
        token_advertised: advertisement
            .lines()
            .any(|line| line.starts_with("grpc.token=")),
        anonymous_status_succeeded,
    };
    eprintln!("gRPC exposure evidence: {evidence:#?}");
    cleanup.finish().await;
    evidence
}

async fn assert_no_grpc_without_flag(sdk_root: &Path) {
    let avd_name = "liteavd-grpc-omitted";
    create_avd(avd_name);
    let mut command = tokio::process::Command::new(sdk_root.join("emulator/emulator"));
    command
        .arg("-avd")
        .arg(avd_name)
        .arg("-port")
        .arg(DEFAULT_CONSOLE_PORT.to_string())
        .arg("-gpu")
        .arg(GpuMode::SwangleIndirect.as_str())
        .arg("-no-window")
        .arg("-no-audio")
        .arg("-no-boot-anim")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command.kill_on_drop(true);
    let mut child = command.spawn().expect("启动省略 -grpc 的探测实例失败");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Some(instance) = emulator::list_running()
            .into_iter()
            .find(|instance| instance.avd_name == avd_name)
        {
            let cleanup =
                RunningCleanup::new(instance, child, sdk_root.to_path_buf(), avd_name.into());
            cleanup.finish().await;
            panic!("显式 -port 且省略 -grpc 时不应产生 gRPC 广告文件");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
    avd::delete_avd(avd_name).expect("删除省略 -grpc 的测试 AVD 失败");
}

async fn anonymous_status(port: u16) -> Result<(), tonic::Code> {
    let endpoint = format!("http://127.0.0.1:{port}");
    let channel = tonic::transport::Channel::from_shared(endpoint)
        .expect("gRPC 地址非法")
        .connect_timeout(Duration::from_secs(5))
        .connect()
        .await
        .expect("匿名 gRPC channel 连接失败");
    let mut client = EmulatorControllerClient::new(channel);
    let mut request = tonic::Request::new(());
    request.set_timeout(Duration::from_secs(5));
    client
        .get_status(request)
        .await
        .map(|_| ())
        .map_err(|status| status.code())
}

async fn run_protected_case(sdk_root: &Path) {
    let avd_name = "liteavd-grpc-protected";
    create_avd(avd_name);
    let params = LaunchParams {
        sdk_root: sdk_root.to_path_buf(),
        avd_name: avd_name.into(),
        port: EXPLICIT_CONSOLE_PORT,
        grpc: GrpcLaunchConfig::new(EXPLICIT_GRPC_PORT).expect("创建 gRPC JWT 身份失败"),
        gpu_policy: ManagedGpuPolicy::HeadlessSwangle,
        audio_policy: ManagedAudioPolicy::Disabled,
        no_window: true,
        share_vid: false,
    };
    let launched = emulator::launch(&params)
        .await
        .expect("受保护的 production launch 失败");
    let launch_log = launched.log_path().to_path_buf();
    assert!(launch_log.is_file(), "production launch 未创建日志");
    assert_eq!(
        std::fs::metadata(&launch_log)
            .expect("读取 production log metadata 失败")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let cleanup = RunningCleanup::managed(
        launched.instance.clone(),
        sdk_root.to_path_buf(),
        avd_name.into(),
    );
    let instance = cleanup.instance();
    let advertisement = std::fs::read_to_string(&instance.ini_path).expect("读取广告文件失败");
    eprintln!("protected gRPC advertisement:\n{advertisement}");
    let listeners = listen_addrs(instance.grpc_port);
    assert!(!listeners.is_empty());
    assert!(
        listeners
            .iter()
            .all(|address| is_loopback_listener(address))
    );
    assert!(instance.grpc_jwks.as_deref().is_some_and(Path::is_dir));
    assert!(
        instance
            .grpc_jwk_active
            .as_deref()
            .is_some_and(Path::is_file)
    );
    assert_eq!(
        instance.grpc_allowlist.as_deref(),
        Some(
            params
                .grpc
                .auth()
                .allowlist_path()
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(
        !advertisement
            .lines()
            .any(|line| line.starts_with("grpc.token="))
    );

    let anonymous = anonymous_status(instance.grpc_port).await.unwrap_err();
    assert!(matches!(
        anonymous,
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied
    ));

    let wrong_auth = Arc::new(GrpcJwtAuth::new().expect("创建错误测试身份失败"));
    let wrong_client = GrpcClient::connect(instance.grpc_port, wrong_auth)
        .await
        .expect("错误身份的 channel 连接失败");
    assert!(
        wrong_client.status().await.is_err(),
        "未注册 JWK 不应通过认证"
    );

    let client = GrpcClient::connect(instance.grpc_port, launched.grpc_auth().clone())
        .await
        .expect("认证 gRPC channel 连接失败");
    client.status().await.expect("有效 JWT getStatus 失败");
    let screenshot_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let screenshot = client
            .screenshot(0, 0)
            .await
            .expect("有效 JWT getScreenshot 失败");
        if screenshot.image.starts_with(b"\x89PNG") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < screenshot_deadline,
            "JWT 截图在 60 秒内仍未产生 PNG"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    client
        .list_snapshots()
        .await
        .expect("有效 JWT listSnapshots 失败");
    emulator::stop_launched(&launched)
        .await
        .expect("managed engine/launcher 停止失败");
    drop(launched);
    cleanup.finish().await;
    let _ = std::fs::remove_file(&launch_log);
    let _ = std::fs::remove_file(launch_log.with_extension("log.previous"));
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "需要独立测试 SDK、KVM、ss 和空闲端口；会顺序创建并启动三个临时 AVD"]
async fn grpc_exposure_matrix() {
    let _workspace = TestWorkspace::new();
    let sdk_root = sdk_root();
    assert_ports_available(&[
        DEFAULT_CONSOLE_PORT,
        DEFAULT_CONSOLE_PORT + 1,
        EXPLICIT_CONSOLE_PORT,
        EXPLICIT_CONSOLE_PORT + 1,
        EXPLICIT_GRPC_PORT,
    ]);

    assert_no_grpc_without_flag(&sdk_root).await;
    let insecure = run_insecure_case(&sdk_root).await;
    assert_eq!(insecure.requested_grpc_port, Some(EXPLICIT_GRPC_PORT));
    assert_eq!(insecure.advertised_grpc_port, EXPLICIT_GRPC_PORT);
    assert!(insecure.allowlist.is_some());
    assert!(!insecure.token_advertised);
    assert!(insecure.anonymous_status_succeeded);
    assert!(
        insecure
            .listeners
            .iter()
            .all(|address| !is_loopback_listener(address)),
        "显式无 JWT 的负向用例应证明其为 wildcard listener：{:?}",
        insecure.listeners
    );
    run_protected_case(&sdk_root).await;
}
