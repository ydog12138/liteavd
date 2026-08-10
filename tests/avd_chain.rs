//! 阶段 2.1 集成验证：真实创建 AVD，`emulator -list-avds` 必须可见。
//! 运行：AVDM_SDK_ROOT=/home/haoran/liteavd-sdk cargo test --test avd_chain -- --ignored

use std::path::PathBuf;
use std::process::Command;

use liteavd::core::avd::{self, AvdSpec, GpuMode};

fn sdk_root() -> PathBuf {
    PathBuf::from(std::env::var("AVDM_SDK_ROOT").expect("需设置 AVDM_SDK_ROOT（已装 SDK 目录）"))
}

fn pick_system_image() -> liteavd::core::repo::SystemImage {
    let root = sdk_root();
    avd::scan_installed_images(&root)
        .into_iter()
        .next()
        .expect("SDK 中未找到结构完整的已安装系统镜像")
}

struct TestAvdHome {
    path: PathBuf,
    previous: Option<std::ffi::OsString>,
    avd_name: Option<String>,
}

impl TestAvdHome {
    fn new(name: String) -> Self {
        let path = std::env::temp_dir().join(format!("{name}-home"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("创建隔离 AVD home 失败");
        let previous = std::env::var_os("ANDROID_AVD_HOME");
        // SAFETY: 此 ignored 集成测试单独运行；guard 会在所有退出路径恢复环境。
        unsafe { std::env::set_var("ANDROID_AVD_HOME", &path) };
        Self {
            path,
            previous,
            avd_name: Some(name),
        }
    }

    fn delete(&mut self) {
        if let Some(name) = self.avd_name.take() {
            avd::delete_avd(&name).expect("清理失败");
        }
    }
}

impl Drop for TestAvdHome {
    fn drop(&mut self) {
        if let Some(name) = self.avd_name.take() {
            let _ = avd::delete_avd(&name);
        }
        match self.previous.take() {
            Some(previous) => {
                // SAFETY: 恢复本测试进入前的环境。
                unsafe { std::env::set_var("ANDROID_AVD_HOME", previous) };
            }
            None => {
                // SAFETY: 恢复本测试进入前的未设置状态。
                unsafe { std::env::remove_var("ANDROID_AVD_HOME") };
            }
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
#[ignore]
fn creates_avd_visible_to_emulator() {
    let root = sdk_root();
    let image = pick_system_image();
    let name = format!("liteavd_it_{}", std::process::id());
    let mut test_home = TestAvdHome::new(name.clone());
    let spec = AvdSpec {
        name: name.clone(),
        device: avd::get_profile("pixel_2").unwrap(),
        image,
        ram_mb: 1536,
        data_partition_mb: 4096,
        sdcard: None,
        gpu: GpuMode::Host,
    };
    avd::create_avd(&spec).expect("创建 AVD 失败");

    let list = Command::new(root.join("emulator/emulator"))
        .arg("-list-avds")
        .output()
        .expect("emulator -list-avds 失败");
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        stdout.lines().any(|l| l.trim() == name),
        "emulator -list-avds 未包含 {name}，输出：{stdout}"
    );

    let avds = avd::list_avds();
    let mine = avds
        .iter()
        .find(|a| a.name == name)
        .expect("list_avds 找不到");
    assert_eq!(
        mine.config.get("abi.type").map(String::as_str),
        Some("x86_64")
    );

    test_home.delete();
    let stdout2 = Command::new(root.join("emulator/emulator"))
        .arg("-list-avds")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&stdout2.stdout).contains(&name));
}
