//! 集成验证：自研代码零 Java 安装 emulator + platform-tools + 系统镜像。
//!
//! 运行（需网络，大文件下载，可中断续传）：
//! ```bash
//! AVDM_SDK_ROOT=/path/to/sdk cargo test --test install_chain -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use liteavd::core::download::{DownloadError, Downloader};
use liteavd::core::install::{ComponentKind, install_component};
use liteavd::core::repo::{Checksum, HostPlatform, Repo, SYSTEM_IMAGE_CHANNELS};

const REPO_BASE: &str = "https://dl.google.com/android/repository";

fn sdk_root() -> PathBuf {
    std::env::var("AVDM_SDK_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join("liteavd-sdk"))
                .unwrap()
        })
}

fn cache_dir(sdk: &Path) -> PathBuf {
    let d = sdk.join(".cache");
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> String {
    client
        .get(url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("拉取 {url} 失败：{e}"))
        .error_for_status()
        .unwrap()
        .text()
        .await
        .unwrap()
}

async fn download_component(
    dl: &Downloader,
    sdk: &Path,
    label: &str,
    url: &str,
    checksum: Option<&Checksum>,
) -> PathBuf {
    let dest = cache_dir(sdk).join(format!("{label}.zip"));
    if dest.exists() {
        println!("[{label}] 已缓存，跳过下载");
        return dest;
    }
    println!("[{label}] 下载 {url}");
    let progress = |done: u64, total: u64| {
        if total > 0 && done.is_multiple_of((total / 20).max(1)) {
            println!(
                "[{label}] {done}/{total} ({:.0}%)",
                done as f64 * 100.0 / total as f64
            );
        }
    };
    match dl.download(url, &dest, checksum, progress).await {
        Ok(()) => println!("[{label}] 下载完成（SHA-256 校验通过）"),
        Err(DownloadError::ChecksumMismatch { .. }) => panic!("[{label}] SHA-256 校验失败"),
        Err(e) => panic!("[{label}] 下载失败：{e}"),
    }
    dest
}

fn run_bin(path: &Path, args: &[&str]) -> String {
    let out = Command::new(path)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("运行 {} 失败：{e}", path.display()));
    assert!(
        out.status.success(),
        "{} {} 退出码非零：{}",
        path.display(),
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
#[ignore = "需要网络与大文件下载"]
async fn install_chain() {
    let sdk = sdk_root();
    std::fs::create_dir_all(&sdk).unwrap();
    println!("SDK 根目录：{}", sdk.display());
    let java = Command::new("java").arg("-version").output().is_ok();
    println!(
        "Java: {}",
        if java {
            "存在（非依赖项）"
        } else {
            "未安装 ✓"
        }
    );

    let dl = Downloader::new().unwrap();
    let client = reqwest::Client::builder()
        .user_agent("liteavd")
        .build()
        .unwrap();

    // 1. emulator + platform-tools：解析 → 下载 → 安装 → 运行验证
    let repo_xml = fetch_text(&client, &format!("{REPO_BASE}/repository2-3.xml")).await;
    let repo = Repo::parse(&repo_xml).expect("repository2-3.xml 解析失败");
    let platform = HostPlatform::current();
    for (path, kind) in [
        ("emulator", ComponentKind::Emulator),
        ("platform-tools", ComponentKind::PlatformTools),
    ] {
        let pkg = repo
            .package(path)
            .unwrap_or_else(|| panic!("缺少包 {path}"));
        let archive = pkg
            .best_archive(platform)
            .unwrap_or_else(|| panic!("{path} 无本平台 archive"));
        println!(
            "[{path}] {} ({} bytes, 渠道 {:?})",
            archive.url, archive.size, pkg.channel
        );
        let zip = download_component(
            &dl,
            &sdk,
            path,
            &format!("{REPO_BASE}/{}", archive.url),
            archive.checksum.as_ref(),
        )
        .await;
        let dest = install_component(&zip, &sdk, &kind).expect("安装失败");
        println!("[{path}] 已安装到 {}", dest.display());
    }

    // 2. 零 Java 运行验证
    let emu = sdk.join("emulator/emulator");
    let adb = sdk.join("platform-tools/adb");
    assert!(emu.exists(), "emulator 二进制缺失");
    assert!(adb.exists(), "adb 缺失");
    let v = run_bin(&emu, &["-version"]);
    println!("[emulator] {v}");
    let v = run_bin(&adb, &["version"]);
    println!("[adb] {v}");

    // 3. 系统镜像：google_apis android-35 x86_64
    let (tag, url) = SYSTEM_IMAGE_CHANNELS[0];
    let sys_xml = fetch_text(&client, url).await;
    let sys_repo = Repo::parse(&sys_xml).expect("sys-img XML 解析失败");
    let img = sys_repo
        .system_images(tag)
        .into_iter()
        .find(|i| i.api == "android-35" && i.abi == "x86_64")
        .expect("未找到 android-35 google_apis x86_64 镜像");
    println!("[sysimg] {} ({} bytes)", img.archive.url, img.archive.size);
    let zip = download_component(
        &dl,
        &sdk,
        "sysimg-35",
        &img.download_url(),
        img.archive.checksum.as_ref(),
    )
    .await;
    let kind = ComponentKind::SystemImage {
        api: img.api.clone(),
        tag: img.tag.clone(),
        abi: img.abi.clone(),
    };
    let dest = install_component(&zip, &sdk, &kind).expect("镜像安装失败");
    assert!(dest.join("system.img").exists(), "system.img 缺失");
    println!("[sysimg] 已安装到 {}", dest.display());
    println!("=== 零 Java 安装链全部通过 ===");
}
