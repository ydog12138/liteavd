//! zip 安装到 SDK 布局 + licenses 写入（事务化：备份 → 新装 → 验证 → 提交/回滚）。

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, bail};
use sha1::Sha1;
use sha2::Digest;
use zip::ZipArchive;

static INSTALL_SEQ: AtomicU32 = AtomicU32::new(0);

/// 每任务唯一后缀（审计 #5：原 PID 共享临时路径会被并发任务互相破坏）。
fn unique_suffix() -> String {
    let n = INSTALL_SEQ.fetch_add(1, Ordering::Relaxed);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{:x}-{n}", std::process::id(), ts)
}

/// 路径片段安全校验（审计 #7）：拒绝空/`.`/`..`、绝对路径、含 `/`、NUL。
pub fn validate_segment(seg: &str) -> anyhow::Result<()> {
    if seg.is_empty()
        || seg == "."
        || seg == ".."
        || seg.contains('/')
        || seg.contains('\\')
        || seg.contains('\0')
    {
        bail!("非法路径片段：{seg:?}");
    }
    Ok(())
}

/// SDK 组件类型，决定安装目录。
#[derive(Debug, Clone)]
pub enum ComponentKind {
    Emulator,
    PlatformTools,
    SystemImage {
        api: String,
        tag: String,
        abi: String,
    },
}

impl ComponentKind {
    pub fn display_name(&self) -> &str {
        match self {
            ComponentKind::Emulator => "emulator",
            ComponentKind::PlatformTools => "platform-tools",
            ComponentKind::SystemImage { .. } => "system image",
        }
    }

    fn identity(&self) -> String {
        match self {
            ComponentKind::Emulator => "emulator".to_string(),
            ComponentKind::PlatformTools => "platform-tools".to_string(),
            ComponentKind::SystemImage { api, tag, abi } => {
                format!("system-image:{api}:{tag}:{abi}")
            }
        }
    }
}

struct OperationLock {
    file: File,
}

impl OperationLock {
    fn acquire(sdk_root: &Path, identity: &str) -> anyhow::Result<Self> {
        std::fs::create_dir_all(sdk_root).context("创建 SDK 根目录失败")?;
        let lock_dir = sdk_root.join(".liteavd-locks");
        std::fs::create_dir_all(&lock_dir).context("创建组件锁目录失败")?;
        std::fs::set_permissions(&lock_dir, std::fs::Permissions::from_mode(0o700))?;
        let mut hasher = sha2::Sha256::new();
        hasher.update(identity.as_bytes());
        let path = lock_dir.join(format!("{:x}.lock", hasher.finalize()));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("打开操作锁 {} 失败", path.display()))?;
        // SAFETY: flock 只操作当前持有的 fd，OperationLock drop 前 fd 不会关闭。
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let source = std::io::Error::last_os_error();
            if source.kind() == std::io::ErrorKind::WouldBlock {
                bail!("操作正忙：{identity}");
            }
            return Err(source).context(format!("锁定操作 {identity} 失败"));
        }
        Ok(Self { file })
    }
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        // SAFETY: file fd 在 drop 结束前仍有效；解锁失败不能在 Drop 中 panic。
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn acquire_component_lock(sdk_root: &Path, kind: &ComponentKind) -> anyhow::Result<OperationLock> {
    validate_kind(kind)?;
    OperationLock::acquire(sdk_root, &format!("component:{}", kind.identity()))
}

/// 组件在 SDK 根目录下的安装位置。
pub fn component_dir(sdk_root: &Path, kind: &ComponentKind) -> PathBuf {
    match kind {
        ComponentKind::Emulator => sdk_root.join("emulator"),
        ComponentKind::PlatformTools => sdk_root.join("platform-tools"),
        ComponentKind::SystemImage { api, tag, abi } => {
            sdk_root.join("system-images").join(api).join(tag).join(abi)
        }
    }
}

/// 校验 kind 中的路径片段（审计 #7，防目录逃逸）。
pub fn validate_kind(kind: &ComponentKind) -> anyhow::Result<()> {
    if let ComponentKind::SystemImage { api, tag, abi } = kind {
        validate_segment(api)?;
        validate_segment(tag)?;
        validate_segment(abi)?;
    }
    Ok(())
}

/// 解压 zip 到 `dest`；拒绝路径穿越（`..`、绝对路径）。
pub fn extract_zip(zip_path: &Path, dest: &Path) -> anyhow::Result<()> {
    let file = File::open(zip_path).context("打开 zip 失败")?;
    let mut archive = ZipArchive::new(file).context("zip 解析失败")?;
    std::fs::create_dir_all(dest).context("创建目标目录失败")?;
    let mut count = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("读取 zip 条目失败")?;
        let name = entry.name().to_string();
        let safe = sanitize_entry(&name)?;
        if safe.is_none() {
            continue;
        }
        let path = dest.join(safe.unwrap());
        if entry.is_dir() {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&path).with_context(|| format!("写入 {path:?} 失败"))?;
        std::io::copy(&mut entry, &mut out)?;
        drop(out);
        // zip 内可能带 unix 权限（可执行位），解压后应用
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o7777))?;
        }
        count += 1;
    }
    if count == 0 {
        bail!("zip 内没有可安装的文件");
    }
    Ok(())
}

/// 返回安全相对路径；路径穿越/绝对路径返回 None。
fn sanitize_entry(name: &str) -> anyhow::Result<Option<PathBuf>> {
    let path = Path::new(name);
    if path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Ok(None);
    }
    if name.is_empty() || name.ends_with('/') {
        return Ok(None);
    }
    Ok(Some(path.to_path_buf()))
}

/// 组件结构校验：新安装就位后验证关键文件存在（审计 #6）。
pub fn verify_component(sdk_root: &Path, kind: &ComponentKind) -> anyhow::Result<()> {
    let dir = component_dir(sdk_root, kind);
    let (ok, what) = match kind {
        ComponentKind::Emulator => (
            dir.join("qemu/linux-x86_64/qemu-system-x86_64").exists(),
            "qemu/linux-x86_64/qemu-system-x86_64",
        ),
        ComponentKind::PlatformTools => (dir.join("adb").exists(), "adb"),
        ComponentKind::SystemImage { .. } => {
            let img = dir.join("system.img").exists();
            let src = dir.join("source.properties").exists();
            (img && src, "system.img + source.properties")
        }
    };
    if ok {
        Ok(())
    } else {
        bail!("组件结构校验失败：{} 缺少 {what}", dir.display())
    }
}

/// 安装组件 zip（事务：解压到唯一临时目录 → 剥离单层包装 → 备份旧版 →
/// 新装就位 → 结构验证 → 提交删备份 / 失败回滚恢复旧版）。
pub fn install_component(
    zip_path: &Path,
    sdk_root: &Path,
    kind: &ComponentKind,
) -> anyhow::Result<PathBuf> {
    // 审计 #7：安装前校验路径片段
    validate_kind(kind)?;
    let _operation_lock = acquire_component_lock(sdk_root, kind)?;
    let target = component_dir(sdk_root, kind);
    let staging = sdk_root.join(format!(".tmp-install-{}", unique_suffix()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    extract_zip(zip_path, &staging)?;

    // 官方 zip 顶层可能带与组件同名目录（emulator/、platform-tools/），剥离单层包装。
    let inner = strip_single_wrapper(&staging);

    let backup = sdk_root.join(format!(".tmp-backup-{}", unique_suffix()));
    let had_old = target.exists();
    if had_old {
        std::fs::rename(&target, &backup).context("备份旧版本失败")?;
    }
    std::fs::create_dir_all(target.parent().context("无父目录")?)?;
    let commit = (|| -> anyhow::Result<()> {
        if inner != staging {
            std::fs::rename(&inner, &target).context("移动到目标位置失败")?;
            let _ = std::fs::remove_dir_all(&staging);
        } else {
            std::fs::rename(&staging, &target).context("移动到目标位置失败")?;
        }
        verify_component(sdk_root, kind)?;
        Ok(())
    })();
    match commit {
        Ok(()) => {
            if had_old {
                let _ = std::fs::remove_dir_all(&backup);
            }
            Ok(target)
        }
        Err(e) => {
            // 回滚：删除未通过验证的新装，恢复备份旧版
            let _ = std::fs::remove_dir_all(&target);
            if had_old {
                let _ = std::fs::rename(&backup, &target);
            }
            let _ = std::fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

/// 删除已安装组件；与安装共用跨进程互斥，且不删除正在执行的工具。
pub fn uninstall_component(sdk_root: &Path, kind: &ComponentKind) -> anyhow::Result<()> {
    validate_kind(kind)?;
    let _operation_lock = acquire_component_lock(sdk_root, kind)?;
    let target = component_dir(sdk_root, kind);
    if !target.exists() {
        bail!("组件未安装：{}", target.display());
    }
    if matches!(kind, ComponentKind::Emulator | ComponentKind::PlatformTools)
        && let Some(pid) = process_using_dir(&target)
    {
        bail!("组件正被进程 {pid} 使用，不能卸载：{}", target.display());
    }
    std::fs::remove_dir_all(&target).with_context(|| format!("卸载组件 {} 失败", target.display()))
}

fn process_using_dir(target: &Path) -> Option<u32> {
    let target = std::fs::canonicalize(target).ok()?;
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(exe) = std::fs::read_link(entry.path().join("exe")) else {
            continue;
        };
        if exe.starts_with(&target) {
            return Some(pid);
        }
    }
    None
}

/// 若 staging 顶层只有一个目录且无文件，返回该目录；否则原样返回。
fn strip_single_wrapper(staging: &Path) -> PathBuf {
    let mut only_dir: Option<PathBuf> = None;
    let Ok(rd) = std::fs::read_dir(staging) else {
        return staging.to_path_buf();
    };
    for entry in rd.flatten() {
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => return staging.to_path_buf(),
        };
        if ft.is_dir() {
            if only_dir.is_some() {
                return staging.to_path_buf();
            }
            only_dir = Some(entry.path());
        } else {
            return staging.to_path_buf();
        }
    }
    only_dir.unwrap_or_else(|| staging.to_path_buf())
}

/// 规范化许可文本：统一换行并忽略首尾空白。
pub fn normalize_license_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

/// license 哈希：展示文本规范化后的 SHA-1 十六进制。
pub fn license_hash(text: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(normalize_license_text(text).as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 读取 `$SDK/licenses/<id>` 中已接受的所有历史哈希。
pub fn accepted_license_hashes(
    sdk_root: &Path,
    id: &str,
) -> anyhow::Result<std::collections::HashSet<String>> {
    validate_segment(id)?;
    let path = sdk_root.join("licenses").join(id);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error).context("读取许可文件元数据失败"),
    };
    if !metadata.file_type().is_file() {
        bail!("许可记录不是普通文件：{}", path.display());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读取许可记录 {} 失败", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_ascii_lowercase)
        .collect())
}

pub fn is_license_accepted(sdk_root: &Path, id: &str, text: &str) -> anyhow::Result<bool> {
    Ok(accepted_license_hashes(sdk_root, id)?.contains(&license_hash(text)))
}

/// 把新 hash 追加到 `$SDK/licenses/<id>`，保留同 ID 的历史文本并去重。
pub fn accept_license(sdk_root: &Path, id: &str, text: &str) -> anyhow::Result<()> {
    validate_segment(id)?;
    if normalize_license_text(text).is_empty() {
        bail!("许可文本为空：{id}");
    }
    let _operation_lock = OperationLock::acquire(sdk_root, &format!("license:{id}"))?;
    let dir = sdk_root.join("licenses");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(id);
    let hash = license_hash(text);
    let existing = accepted_license_hashes(sdk_root, id)?;
    if existing.contains(&hash) {
        return Ok(());
    }
    let needs_newline = std::fs::read(&path)
        .map(|bytes| !bytes.is_empty() && !bytes.ends_with(b"\n"))
        .unwrap_or(false);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("打开许可记录 {} 失败", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    if needs_newline {
        writeln!(file)?;
    }
    writeln!(file, "{hash}")?;
    file.sync_all()?;
    Ok(())
}

/// 已接受的 license id 集合。
pub fn accepted_licenses(sdk_root: &Path) -> std::collections::HashSet<String> {
    let dir = sdk_root.join("licenses");
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, content) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(content).unwrap();
        }
        zip.finish().unwrap();
    }

    use std::sync::atomic::{AtomicU32, Ordering};

    static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("liteavd-install-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn extracts_zip_skipping_traversal() {
        let dir = temp_dir();
        let zip = dir.join("pkg.zip");
        make_zip(
            &zip,
            &[
                ("emulator/a.txt", b"hello"),
                ("emulator/sub/b.txt", b"world"),
                ("../evil.txt", b"evil"),
                ("/abs/evil2.txt", b"evil2"),
            ],
        );
        let dest = dir.join("out");
        extract_zip(&zip, &dest).unwrap();
        assert_eq!(
            std::fs::read(dest.join("emulator/a.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(
            std::fs::read(dest.join("emulator/sub/b.txt")).unwrap(),
            b"world"
        );
        assert!(!dir.join("evil.txt").exists());
        assert!(!Path::new("/abs/evil2.txt").exists());
    }

    #[test]
    fn installs_to_component_dirs() {
        let dir = temp_dir();
        let sdk = dir.join("sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        let zip = dir.join("emulator.zip");
        make_zip(
            &zip,
            &[
                ("emulator/qemu/linux-x86_64/qemu-system-x86_64", b"bin"),
                ("emulator/lib/x.txt", b"x"),
            ],
        );
        let out = install_component(&zip, &sdk, &ComponentKind::Emulator).unwrap();
        assert_eq!(out, sdk.join("emulator"));
        assert!(
            sdk.join("emulator/qemu/linux-x86_64/qemu-system-x86_64")
                .exists()
        );

        let syszip = dir.join("img.zip");
        make_zip(
            &syszip,
            &[("system.img", b"img"), ("source.properties", b"Pkg.Desc=1")],
        );
        let kind = ComponentKind::SystemImage {
            api: "android-35".into(),
            tag: "google_apis".into(),
            abi: "x86_64".into(),
        };
        install_component(&syszip, &sdk, &kind).unwrap();
        assert!(
            sdk.join("system-images/android-35/google_apis/x86_64/system.img")
                .exists()
        );
    }

    #[test]
    fn license_roundtrip() {
        let dir = temp_dir();
        let sdk = dir.join("sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        let text = "Terms and Conditions\n...";
        accept_license(&sdk, "android-sdk-license", text).unwrap();
        let file = std::fs::read_to_string(sdk.join("licenses/android-sdk-license")).unwrap();
        assert_eq!(file.trim(), license_hash(text));
        assert!(accepted_licenses(&sdk).contains("android-sdk-license"));
        assert!(!accepted_licenses(&sdk).contains("other"));
    }

    #[test]
    fn same_license_id_preserves_old_hash_and_deduplicates_new_hash() {
        let dir = temp_dir();
        let sdk = dir.join("sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        let old = "old terms\r\nline two\n";
        let new = "new terms\nline two";

        accept_license(&sdk, "android-sdk-license", old).unwrap();
        accept_license(&sdk, "android-sdk-license", new).unwrap();
        accept_license(&sdk, "android-sdk-license", new).unwrap();

        let lines: Vec<_> = std::fs::read_to_string(sdk.join("licenses/android-sdk-license"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines, vec![license_hash(old), license_hash(new)]);
        assert!(is_license_accepted(&sdk, "android-sdk-license", old).unwrap());
        assert!(is_license_accepted(&sdk, "android-sdk-license", new).unwrap());
        assert!(!is_license_accepted(&sdk, "android-sdk-license", "other").unwrap());
    }

    #[test]
    fn license_write_failure_is_reported() {
        let dir = temp_dir();
        let sdk = dir.join("sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        std::fs::write(sdk.join("licenses"), b"not a directory").unwrap();
        assert!(accept_license(&sdk, "android-sdk-license", "terms").is_err());
    }

    #[test]
    fn component_lock_rejects_a_second_writer() {
        let dir = temp_dir();
        let sdk = dir.join("sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        let zip = dir.join("emulator.zip");
        make_zip(
            &zip,
            &[("emulator/qemu/linux-x86_64/qemu-system-x86_64", b"bin")],
        );
        let kind = ComponentKind::Emulator;
        let held = acquire_component_lock(&sdk, &kind).unwrap();
        let error = install_component(&zip, &sdk, &kind).unwrap_err();
        assert!(error.to_string().contains("操作正忙"), "{error:#}");
        drop(held);
        install_component(&zip, &sdk, &kind).unwrap();

        let held = acquire_component_lock(&sdk, &kind).unwrap();
        let error = uninstall_component(&sdk, &kind).unwrap_err();
        assert!(error.to_string().contains("操作正忙"), "{error:#}");
        drop(held);
        uninstall_component(&sdk, &kind).unwrap();
    }

    #[test]
    fn uninstall_reports_missing_and_removes_installed_component() {
        let dir = temp_dir();
        let sdk = dir.join("sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        let kind = ComponentKind::SystemImage {
            api: "android-35".into(),
            tag: "google_apis".into(),
            abi: "x86_64".into(),
        };
        assert!(uninstall_component(&sdk, &kind).is_err());
        let target = component_dir(&sdk, &kind);
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("system.img"), b"image").unwrap();
        uninstall_component(&sdk, &kind).unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn uninstall_refuses_running_platform_tool() {
        let dir = temp_dir();
        let sdk = dir.join("sdk");
        let target = component_dir(&sdk, &ComponentKind::PlatformTools);
        std::fs::create_dir_all(&target).unwrap();
        let executable = target.join("adb");
        std::fs::copy("/bin/sleep", &executable).unwrap();
        let mut child = std::process::Command::new(&executable)
            .arg("30")
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        let error = uninstall_component(&sdk, &ComponentKind::PlatformTools).unwrap_err();
        assert!(error.to_string().contains("正被进程"), "{error:#}");
        assert!(target.exists());

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn reject_traversal_segments() {
        assert!(validate_segment("android-35").is_ok());
        assert!(validate_segment("google_apis").is_ok());
        assert!(validate_segment("").is_err());
        assert!(validate_segment("..").is_err());
        assert!(validate_segment(".").is_err());
        assert!(validate_segment("a/b").is_err());
        assert!(validate_segment("a\\b").is_err());
        assert!(validate_segment("a\0b").is_err());
        assert!(validate_segment("/abs").is_err());
    }

    #[test]
    fn rejects_unsafe_kind_and_license_id() {
        let dir = temp_dir();
        let sdk = dir.join("sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        let bad = ComponentKind::SystemImage {
            api: "../evil".into(),
            tag: "google_apis".into(),
            abi: "x86_64".into(),
        };
        assert!(validate_kind(&bad).is_err());
        assert!(accept_license(&sdk, "../evil", "text").is_err());
    }

    #[test]
    fn replaces_old_version_transactionally() {
        let dir = temp_dir();
        let sdk = dir.join("sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        let zip = dir.join("emulator.zip");
        make_zip(
            &zip,
            &[("emulator/qemu/linux-x86_64/qemu-system-x86_64", b"newbin")],
        );
        // 先装一个"旧版"
        let old = sdk.join("emulator");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("old-marker"), b"old").unwrap();
        install_component(&zip, &sdk, &ComponentKind::Emulator).unwrap();
        assert!(
            sdk.join("emulator/qemu/linux-x86_64/qemu-system-x86_64")
                .exists()
        );
        assert!(!sdk.join("emulator/old-marker").exists(), "旧版应被替换");
        // 无残留临时目录
        let leftovers: Vec<_> = std::fs::read_dir(&sdk)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with(".tmp-")
            })
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时目录：{leftovers:?}");
    }

    #[test]
    fn failed_install_restores_old_version() {
        let dir = temp_dir();
        let sdk = dir.join("sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        let old = sdk.join("emulator");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("old-marker"), b"old").unwrap();
        // zip 缺少关键文件（qemu 引擎）→ 结构校验失败 → 回滚
        let bad_zip = dir.join("bad.zip");
        make_zip(&bad_zip, &[("emulator/README.txt", b"incomplete")]);
        let err = install_component(&bad_zip, &sdk, &ComponentKind::Emulator).unwrap_err();
        assert!(
            err.to_string().contains("结构校验失败"),
            "应报结构校验失败：{err:#}"
        );
        assert!(sdk.join("emulator/old-marker").exists(), "回滚后旧版应保留");
        let leftovers: Vec<_> = std::fs::read_dir(&sdk)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with(".tmp-")
            })
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时目录：{leftovers:?}");
    }
}
