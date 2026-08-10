//! 阶段 2.3 集成验证：headless 启动模拟器 → sys.boot_completed=1。
//! 已实测结论：-gpu host 无头无 DISPLAY 渲染器不可用；swiftshader_indirect 中途 SIGSEGV；
//! 无头默认 = swangle_indirect（Google issue 390743125 建议）。
//! 运行：AVDM_SDK_ROOT=/home/haoran/liteavd-sdk cargo test --test headless_boot -- --ignored

use std::path::PathBuf;
use std::time::Duration;

use liteavd::core::adb;
use liteavd::core::avd::{self, AvdSpec, GpuMode, ManagedGpuPolicy};
use liteavd::core::emulator::{self, LaunchParams, ManagedAudioPolicy};
use liteavd::core::grpc_auth::GrpcLaunchConfig;
use liteavd::core::repo::Archive;

fn sdk_root() -> PathBuf {
    PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT"))
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

fn ensure_avd(name: &str) {
    let spec = AvdSpec {
        name: name.into(),
        device: avd::get_profile("pixel_2").unwrap(),
        image: sample_image(),
        ram_mb: 1536,
        data_partition_mb: 4096,
        sdcard: None,
        gpu: GpuMode::SwangleIndirect,
    };
    if avd::list_avds().iter().any(|a| a.name == name) {
        return;
    }
    avd::create_avd(&spec).expect("创建 AVD 失败");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn headless_boots_to_completion() {
    let root = sdk_root();
    let name = format!("liteavd_boot_{}", std::process::id());
    ensure_avd(&name);

    let params = LaunchParams {
        sdk_root: root.clone(),
        avd_name: name.clone(),
        port: 5556,
        grpc: GrpcLaunchConfig::new(8556).expect("创建 gRPC JWT 身份失败"),
        gpu_policy: ManagedGpuPolicy::HeadlessSwangle,
        audio_policy: ManagedAudioPolicy::Disabled,
        no_window: true,
        share_vid: false,
    };
    let launched = emulator::launch(&params)
        .await
        .expect("launch 失败（无头默认 swangle_indirect）");
    let inst = &launched.instance;
    eprintln!(
        "启动成功 pid={} console_port={} adb_port={} grpc_port={} allowlist={}",
        inst.pid,
        inst.console_port,
        inst.adb_port,
        inst.grpc_port,
        inst.grpc_allowlist.as_deref().unwrap_or("<无>"),
    );
    assert_eq!(inst.avd_name, name);
    assert_eq!(inst.grpc_port, 8556);

    let serial = format!("emulator-{}", inst.console_port);
    let secs = adb::wait_for_boot(&root, &serial, Duration::from_secs(240))
        .await
        .expect("boot 超时");
    eprintln!("boot_completed=1 用时 {secs:.0}s");

    emulator::stop_launched(&launched).await.expect("停止失败");
    avd::delete_avd(&name).expect("清理失败");
}
