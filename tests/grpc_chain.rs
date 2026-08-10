//! 阶段 3 集成验证：gRPC 完整链路（boot 状态/截图/快照列表）。
//! 运行：AVDM_SDK_ROOT=/home/haoran/liteavd-sdk cargo test --test grpc_chain -- --ignored

use std::path::PathBuf;
use std::time::Duration;

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

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn grpc_full_chain() {
    let root = sdk_root();
    let name = format!("liteavd_grpc_{}", std::process::id());
    let spec = AvdSpec {
        name: name.clone(),
        device: avd::get_profile("pixel_2").unwrap(),
        image: sample_image(),
        ram_mb: 1536,
        data_partition_mb: 4096,
        sdcard: None,
        gpu: GpuMode::SwangleIndirect,
    };
    avd::create_avd(&spec).expect("创建 AVD 失败");

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
    let launched = emulator::launch(&params).await.expect("launch 失败");
    let inst = &launched.instance;
    eprintln!("启动成功 pid={} grpc_port={}", inst.pid, inst.grpc_port);

    let client = launched.grpc_client().clone();

    // boot 状态（gRPC 直查）
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    let mut booted = false;
    while std::time::Instant::now() < deadline {
        if let Ok(b) = client.is_booted().await
            && b
        {
            booted = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert!(booted, "gRPC booted 未变为 true（240s）");

    // 截图（PNG magic + 非空）
    let shot_path = std::env::temp_dir().join(format!("liteavd_shot_{}.png", std::process::id()));
    let bytes = client.write_screenshot(&shot_path).await.expect("截图失败");
    assert!(bytes > 1000, "截图过小：{bytes}");
    let head = std::fs::read(&shot_path).unwrap();
    assert_eq!(&head[..4], b"\x89PNG", "非 PNG 文件");
    eprintln!("截图 {bytes}B 已写入 {}", shot_path.display());
    std::fs::remove_file(&shot_path).unwrap();

    // 快照列表（可能为空，但不报错）
    let snaps = client.list_snapshots().await.expect("list_snapshots 失败");
    eprintln!("快照数量：{}", snaps.len());

    emulator::stop_launched(&launched).await.expect("停止失败");
    avd::delete_avd(&name).expect("清理失败");
}
