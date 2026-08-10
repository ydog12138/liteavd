//! AVD 创建/删除/列举：复刻 avdmanager 的文件行为（ini + config.ini）。

use std::collections::HashMap;
use std::ffi::CString;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::core::repo::SystemImage;

static AVD_OPERATION_SEQ: AtomicU32 = AtomicU32::new(0);

pub const PROFILE_CATALOG_SCHEMA_VERSION: u32 = 1;
pub const AVD_CREATION_SCHEMA_VERSION: u32 = 1;
pub const MIN_RAM_MB: u32 = 512;
pub const MAX_RAM_MB: u32 = 8192;
pub const RAM_STEP_MB: u32 = 256;
pub const MIN_DATA_PARTITION_MB: u64 = 1024;
pub const MAX_DATA_PARTITION_MB: u64 = 32768;
pub const DATA_PARTITION_STEP_MB: u64 = 512;
pub const FALLBACK_RAM_MB: u32 = 2048;

/// 内置设备 profile（公开规格）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceProfile {
    pub id: String,
    pub manufacturer: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub density: u32,
    pub default_ram_mb: u32,
    pub has_main_keys: bool,
}

/// 可导入/导出的版本化设备 profile catalog。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceProfileCatalog {
    pub schema_version: u32,
    pub profiles: Vec<DeviceProfile>,
}

impl DeviceProfileCatalog {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != PROFILE_CATALOG_SCHEMA_VERSION {
            bail!(
                "不支持的设备 profile schema {}（当前为 {}）",
                self.schema_version,
                PROFILE_CATALOG_SCHEMA_VERSION
            );
        }
        let mut ids = std::collections::HashSet::new();
        for profile in &self.profiles {
            if !valid_name(&profile.id) {
                bail!("设备 profile id 非法：{:?}", profile.id);
            }
            if !ids.insert(profile.id.as_str()) {
                bail!("设备 profile id 重复：{}", profile.id);
            }
            if profile.name.trim().is_empty()
                || profile.manufacturer.trim().is_empty()
                || profile.width == 0
                || profile.height == 0
                || profile.density == 0
                || !(MIN_RAM_MB..=MAX_RAM_MB).contains(&profile.default_ram_mb)
            {
                bail!("设备 profile 数据非法：{}", profile.id);
            }
        }
        Ok(())
    }
}

/// 内置常用型号表（与 Android Studio 默认设备一致）。
pub fn builtin_profile_catalog() -> DeviceProfileCatalog {
    DeviceProfileCatalog {
        schema_version: PROFILE_CATALOG_SCHEMA_VERSION,
        profiles: vec![
            DeviceProfile {
                id: "pixel_2".into(),
                manufacturer: "Google".into(),
                name: "Pixel 2".into(),
                width: 1080,
                height: 1920,
                density: 420,
                default_ram_mb: 1536,
                has_main_keys: false,
            },
            DeviceProfile {
                id: "pixel_3a".into(),
                manufacturer: "Google".into(),
                name: "Pixel 3a".into(),
                width: 1080,
                height: 2220,
                density: 440,
                default_ram_mb: 1536,
                has_main_keys: false,
            },
            DeviceProfile {
                id: "pixel_4".into(),
                manufacturer: "Google".into(),
                name: "Pixel 4".into(),
                width: 1080,
                height: 2280,
                density: 440,
                default_ram_mb: 1536,
                has_main_keys: false,
            },
            DeviceProfile {
                id: "pixel_5".into(),
                manufacturer: "Google".into(),
                name: "Pixel 5".into(),
                width: 1080,
                height: 2340,
                density: 440,
                default_ram_mb: 2048,
                has_main_keys: false,
            },
            DeviceProfile {
                id: "pixel_6".into(),
                manufacturer: "Google".into(),
                name: "Pixel 6".into(),
                width: 1080,
                height: 2400,
                density: 420,
                default_ram_mb: 2048,
                has_main_keys: false,
            },
            DeviceProfile {
                id: "pixel_7".into(),
                manufacturer: "Google".into(),
                name: "Pixel 7".into(),
                width: 1080,
                height: 2400,
                density: 420,
                default_ram_mb: 2048,
                has_main_keys: false,
            },
            DeviceProfile {
                id: "pixel_8".into(),
                manufacturer: "Google".into(),
                name: "Pixel 8".into(),
                width: 1080,
                height: 2400,
                density: 420,
                default_ram_mb: 2048,
                has_main_keys: false,
            },
            DeviceProfile {
                id: "pixel_9".into(),
                manufacturer: "Google".into(),
                name: "Pixel 9".into(),
                width: 1080,
                height: 2424,
                density: 420,
                default_ram_mb: 2048,
                has_main_keys: false,
            },
            DeviceProfile {
                id: "generic".into(),
                manufacturer: "Generic".into(),
                name: "Generic".into(),
                width: 1080,
                height: 1920,
                density: 420,
                default_ram_mb: 1536,
                has_main_keys: true,
            },
        ],
    }
}

pub fn builtin_profiles() -> Vec<DeviceProfile> {
    builtin_profile_catalog().profiles
}

/// 读取版本化 JSON profile catalog；未知字段、未知版本和非法规格都会拒绝。
pub fn load_profile_catalog(path: &Path) -> anyhow::Result<DeviceProfileCatalog> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("读取设备 profile catalog 失败：{}", path.display()))?;
    if bytes.len() > 1024 * 1024 {
        bail!("设备 profile catalog 超过 1 MiB：{}", path.display());
    }
    let catalog: DeviceProfileCatalog = serde_json::from_slice(&bytes)
        .with_context(|| format!("解析设备 profile catalog 失败：{}", path.display()))?;
    catalog.validate()?;
    Ok(catalog)
}

pub fn get_profile(id: &str) -> Option<DeviceProfile> {
    builtin_profiles().into_iter().find(|p| p.id == id)
}

/// Raw emulator GPU mode used by AVD config files and command-line flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuMode {
    Auto,
    Host,
    SwiftshaderIndirect,
    SwangleIndirect,
}

impl GpuMode {
    pub const CREATION_CHOICES: [Self; 3] = [Self::Auto, Self::Host, Self::SwangleIndirect];

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Host => "host",
            Self::SwiftshaderIndirect => "swiftshader_indirect",
            Self::SwangleIndirect => "swangle_indirect",
        }
    }
}

/// Product-level managed GPU policy.  It intentionally cannot represent a
/// raw emulator mode, so launch and scheduler callers must choose one of the
/// two supported product behaviors explicitly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedGpuPolicy {
    #[default]
    HeadlessSwangle,
    DesktopHost,
}

impl ManagedGpuPolicy {
    pub const MANAGED_CHOICES: [Self; 2] = [Self::HeadlessSwangle, Self::DesktopHost];

    /// Resolve the product policy to the exact raw emulator flag.
    pub const fn gpu_mode(self) -> GpuMode {
        match self {
            Self::HeadlessSwangle => GpuMode::SwangleIndirect,
            Self::DesktopHost => GpuMode::Host,
        }
    }

    pub const fn gpu_slots(self) -> u32 {
        match self {
            Self::DesktopHost => 1,
            Self::HeadlessSwangle => 0,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::HeadlessSwangle => "无头兼容（swangle_indirect，无需 Xvfb）",
            Self::DesktopHost => "桌面硬件（继承 XWayland DISPLAY）",
        }
    }

    pub const fn availability(self) -> &'static str {
        match self {
            Self::HeadlessSwangle => "无需 DISPLAY；适用于无头和 Wayland 会话。",
            Self::DesktopHost => {
                "需要非空的继承 DISPLAY（通常来自桌面 XWayland），并且必须检测到硬件 renderer；缺失或软件 renderer 会拒绝启动。"
            }
        }
    }
}

/// 创建向导的版本化硬件默认值；具体 RAM 默认值来自所选 profile。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AvdCreationConfig {
    pub schema_version: u32,
    pub data_partition_mb: u64,
    pub sdcard: Option<String>,
    pub gpu: GpuMode,
}

impl Default for AvdCreationConfig {
    fn default() -> Self {
        Self {
            schema_version: AVD_CREATION_SCHEMA_VERSION,
            data_partition_mb: 6144,
            sdcard: None,
            gpu: GpuMode::Auto,
        }
    }
}

impl AvdCreationConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != AVD_CREATION_SCHEMA_VERSION {
            bail!(
                "不支持的 AVD 创建配置 schema {}（当前为 {}）",
                self.schema_version,
                AVD_CREATION_SCHEMA_VERSION
            );
        }
        if !(MIN_DATA_PARTITION_MB..=MAX_DATA_PARTITION_MB).contains(&self.data_partition_mb) {
            bail!("AVD 数据分区超出支持范围：{} MiB", self.data_partition_mb);
        }
        if self
            .sdcard
            .as_deref()
            .is_some_and(|value| value.is_empty() || value.contains(['\n', '\r', '=', '\0']))
        {
            bail!("AVD SD card 配置非法");
        }
        Ok(())
    }
}

/// 无头模式默认 GPU（swiftshader_indirect 在本机 SIGSEGV，host 无头无 DISPLAY 渲染器不可用）。
pub fn headless_default_gpu() -> GpuMode {
    GpuMode::SwangleIndirect
}

/// AVD 规格（创建参数）。
#[derive(Debug, Clone)]
pub struct AvdSpec {
    pub name: String,
    pub device: DeviceProfile,
    pub image: SystemImage,
    pub ram_mb: u32,
    pub data_partition_mb: u64,
    pub sdcard: Option<String>,
    pub gpu: GpuMode,
}

/// 已存在的 AVD 摘要。
#[derive(Debug, Clone)]
pub struct AvdInfo {
    pub name: String,
    pub path: PathBuf,
    pub config: HashMap<String, String>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        && !name.starts_with('.')
}

/// `$ANDROID_AVD_HOME`，或当前运行环境的持久默认目录。
pub fn avd_root() -> PathBuf {
    if let Some(home) = std::env::var_os("ANDROID_AVD_HOME") {
        return PathBuf::from(home);
    }
    crate::core::paths::default_avd_root()
}

/// 设备 profile 的 MD5（avdmanager 写 hw.device.hash2 用）。
fn profile_hash(profile: &DeviceProfile) -> String {
    let s = format!(
        "{}:{}:{}:{}:{}:{}",
        profile.id,
        profile.manufacturer,
        profile.name,
        profile.width,
        profile.height,
        profile.density
    );
    format!("MD5:{}", md5_hex(&s))
}

fn md5_hex(data: &str) -> String {
    format!("{:x}", md5::compute(data.as_bytes()))
}

/// 生成 config.ini 内容（与 avdmanager 输出结构一致）。
pub fn config_ini(spec: &AvdSpec) -> String {
    let d = &spec.device;
    let image = &spec.image;
    let sysdir = format!("system-images/{}/{}/{}/", image.api, image.tag, image.abi);
    let mut s = String::new();
    let mut w = |k: &str, v: &str| {
        let _ = writeln!(s, "{k}={v}");
    };
    w("AvdId", &spec.name);
    w(
        "PlayStore.enabled",
        if image.tag == "google_apis_playstore" {
            "true"
        } else {
            "false"
        },
    );
    w("abi.type", &image.abi);
    w("avd.ini.displayname", &spec.name);
    w("avd.ini.encoding", "UTF-8");
    w(
        "disk.dataPartition.size",
        &format!("{}M", spec.data_partition_mb),
    );
    w("fastboot.forceColdBoot", "no");
    w("fastboot.forceFastBoot", "yes");
    w("hw.accelerometer", "yes");
    w("hw.audioInput", "yes");
    w("hw.battery", "yes");
    w("hw.camera.back", "virtualscene");
    w("hw.camera.front", "emulated");
    w("hw.cpu.arch", &image.abi);
    w("hw.cpu.ncore", "4");
    w("hw.dPad", "no");
    w("hw.device.hash2", &profile_hash(d));
    w("hw.device.manufacturer", &d.manufacturer);
    w("hw.device.name", &d.id);
    w("hw.gps", "yes");
    w("hw.gpu.enabled", "yes");
    w("hw.gpu.mode", spec.gpu.as_str());
    w("hw.initialOrientation", "Portrait");
    w("hw.keyboard", "yes");
    w("hw.lcd.density", &d.density.to_string());
    w("hw.lcd.height", &d.height.to_string());
    w("hw.lcd.width", &d.width.to_string());
    w("hw.mainKeys", if d.has_main_keys { "yes" } else { "no" });
    w("hw.ramSize", &spec.ram_mb.to_string());
    w(
        "hw.sdCard",
        if spec.sdcard.is_some() { "yes" } else { "no" },
    );
    w("hw.sensors.orientation", "yes");
    w("hw.sensors.proximity", "yes");
    w("hw.trackBall", "no");
    w("image.sysdir.1", &sysdir);
    w("runtime.network.latency", "none");
    w("runtime.network.speed", "full");
    if let Some(sdcard) = &spec.sdcard {
        w("sdcard.size", sdcard);
    }
    w("showDeviceFrame", "yes");
    w("skin.dynamic", "yes");
    w("skin.name", &format!("{}x{}", d.width, d.height));
    w("skin.path", "_no_skin");
    w("tag.display", &image_tag_display(&image.tag));
    w("tag.id", &image.tag);
    w("vm.heapSize", "256");
    s
}

fn image_tag_display(tag: &str) -> String {
    match tag {
        "google_apis" => "Google APIs".into(),
        "google_apis_playstore" => "Google Play".into(),
        "aosp_atd" => "AOSP ATD".into(),
        other => other.to_string(),
    }
}

struct AvdOperationLock {
    file: File,
}

impl AvdOperationLock {
    fn acquire(root: &Path, name: &str) -> anyhow::Result<Self> {
        let lock_dir = root.join(".liteavd-locks");
        std::fs::create_dir_all(&lock_dir).context("创建 AVD 锁目录失败")?;
        std::fs::set_permissions(&lock_dir, std::fs::Permissions::from_mode(0o700))?;
        let path = lock_dir.join(format!("{}.lock", md5_hex(name)));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("打开 AVD 操作锁失败：{}", path.display()))?;
        // SAFETY: flock 只操作当前持有的 fd，guard drop 前 fd 始终有效。
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let source = std::io::Error::last_os_error();
            if source.kind() == std::io::ErrorKind::WouldBlock {
                bail!("AVD 操作正忙：{name}");
            }
            return Err(source).context(format!("锁定 AVD 操作失败：{name}"));
        }
        Ok(Self { file })
    }
}

impl Drop for AvdOperationLock {
    fn drop(&mut self) {
        // SAFETY: fd 在 drop 结束前有效；释放失败不能在 Drop 中 panic。
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

struct CreationRollback {
    staging_dir: PathBuf,
    temporary_ini: PathBuf,
    published_dir: PathBuf,
    published_ini: PathBuf,
    committed: bool,
}

impl Drop for CreationRollback {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = std::fs::remove_file(&self.temporary_ini);
        let _ = std::fs::remove_dir_all(&self.staging_dir);
        let _ = std::fs::remove_file(&self.published_ini);
        let _ = std::fs::remove_dir_all(&self.published_dir);
    }
}

fn unique_suffix() -> String {
    let sequence = AVD_OPERATION_SEQ.fetch_add(1, Ordering::Relaxed);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{timestamp:x}-{sequence}", std::process::id())
}

fn write_new_synced(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("创建 {} 失败", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("写入 {} 失败", path.display()))?;
    file.sync_all()
        .with_context(|| format!("同步 {} 失败", path.display()))
}

fn sync_directory(path: &Path) -> anyhow::Result<()> {
    File::open(path)
        .with_context(|| format!("打开目录 {} 失败", path.display()))?
        .sync_all()
        .with_context(|| format!("同步目录 {} 失败", path.display()))
}

fn rename_noreplace(source: &Path, destination: &Path) -> anyhow::Result<()> {
    let source_c = CString::new(source.as_os_str().as_bytes()).context("源路径包含 NUL")?;
    let destination_c =
        CString::new(destination.as_os_str().as_bytes()).context("目标路径包含 NUL")?;
    // SAFETY: 两个 CString 在调用期间有效；RENAME_NOREPLACE 防止竞态覆盖已有 AVD。
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source_c.as_ptr(),
            libc::AT_FDCWD,
            destination_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "发布 AVD 路径失败：{} → {}",
                source.display(),
                destination.display()
            )
        })
    }
}

fn validate_spec(spec: &AvdSpec) -> anyhow::Result<()> {
    if !valid_name(&spec.name) {
        bail!("AVD 名称非法（仅允许字母数字、_、-、.）：{}", spec.name);
    }
    for (label, segment) in [
        ("API", spec.image.api.as_str()),
        ("tag", spec.image.tag.as_str()),
        ("ABI", spec.image.abi.as_str()),
    ] {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains(['/', '\\', '\0'])
        {
            bail!("系统镜像 {label} 路径片段非法：{segment:?}");
        }
    }
    if spec.ram_mb == 0 || spec.data_partition_mb == 0 {
        bail!("AVD RAM 与数据分区必须大于 0");
    }
    Ok(())
}

/// 创建 AVD：先在同一文件系统写入并同步 staging，再以 no-replace rename 发布。
pub fn create_avd(spec: &AvdSpec) -> anyhow::Result<AvdInfo> {
    create_avd_in_root(spec, &avd_root())
}

fn create_avd_in_root(spec: &AvdSpec, root: &Path) -> anyhow::Result<AvdInfo> {
    create_avd_in_root_with_publish_hook(spec, root, || Ok(()))
}

fn create_avd_in_root_with_publish_hook(
    spec: &AvdSpec,
    root: &Path,
    before_ini_publish: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<AvdInfo> {
    validate_spec(spec)?;
    let name = &spec.name;
    std::fs::create_dir_all(root).context("创建 AVD 目录失败")?;
    let _lock = AvdOperationLock::acquire(root, name)?;

    let avd_dir = root.join(format!("{name}.avd"));
    let final_ini = root.join(format!("{name}.ini"));
    if avd_dir.exists() || final_ini.exists() {
        bail!("AVD 已存在：{name}");
    }

    let suffix = unique_suffix();
    let identity = md5_hex(name);
    let staging_dir = root.join(format!(".liteavd-create-{identity}-{suffix}.avd.tmp"));
    let temporary_ini = root.join(format!(".liteavd-create-{identity}-{suffix}.ini.tmp"));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging_dir)
        .context("创建 AVD staging 目录失败")?;
    let mut rollback = CreationRollback {
        staging_dir: staging_dir.clone(),
        temporary_ini: temporary_ini.clone(),
        published_dir: avd_dir.clone(),
        published_ini: final_ini.clone(),
        committed: false,
    };

    let ini = format!(
        "avd.ini.encoding=UTF-8\npath={}\npath.rel=avd/{name}.avd\ntarget=android-{}\n",
        avd_dir.display(),
        spec.image.api_number()
    );
    let config_text = config_ini(spec);
    write_new_synced(&staging_dir.join("config.ini"), config_text.as_bytes())?;
    sync_directory(&staging_dir)?;
    write_new_synced(&temporary_ini, ini.as_bytes())?;
    sync_directory(root)?;

    rename_noreplace(&staging_dir, &avd_dir)?;
    before_ini_publish()?;
    rename_noreplace(&temporary_ini, &final_ini)?;
    sync_directory(root)?;

    rollback.committed = true;
    let config = parse_ini(&config_text);
    Ok(AvdInfo {
        name: name.clone(),
        path: avd_dir,
        config,
    })
}

/// 列举全部 AVD（读 `<avd_root>/*.ini` 的 path 与 config.ini）。
pub fn list_avds() -> Vec<AvdInfo> {
    let root = avd_root();
    list_avds_in_root(&root)
}

fn list_avds_in_root(root: &Path) -> Vec<AvdInfo> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".ini") {
            continue;
        }
        let Some(avd_name) = name.strip_suffix(".ini") else {
            continue;
        };
        let avd_dir = path
            .parent()
            .map(|p| p.join(format!("{avd_name}.avd")))
            .unwrap_or_default();
        let Ok(config_text) = std::fs::read_to_string(avd_dir.join("config.ini")) else {
            continue;
        };
        let config = parse_ini(&config_text);
        out.push(AvdInfo {
            name: avd_name.to_string(),
            path: avd_dir,
            config,
        });
    }
    out
}

/// 删除 AVD（ini + .avd 目录）。
pub fn delete_avd(name: &str) -> anyhow::Result<()> {
    let root = avd_root();
    delete_avd_in_root_if(name, &root, |candidate| {
        crate::core::emulator::list_running()
            .iter()
            .any(|instance| instance.avd_name == candidate)
    })
}

fn delete_avd_in_root_if(
    name: &str,
    root: &Path,
    is_running: impl FnOnce(&str) -> bool,
) -> anyhow::Result<()> {
    // 审计 #7：删除前复验名称（防目录逃逸）
    if !valid_name(name) {
        bail!("AVD 名称非法（仅允许字母数字、_、-、.）：{name}");
    }
    std::fs::create_dir_all(root).context("创建 AVD 目录失败")?;
    let _lock = AvdOperationLock::acquire(root, name)?;
    if is_running(name) {
        bail!("AVD 正在运行，必须先停止后才能删除：{name}");
    }
    let ini = root.join(format!("{name}.ini"));
    let dir = root.join(format!("{name}.avd"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    if ini.exists() {
        std::fs::remove_file(&ini)?;
    }
    Ok(())
}

/// 扫描 SDK 中已安装的系统镜像（system-images/<api>/<tag>/<abi>/）。
pub fn scan_installed_images(sdk_root: &Path) -> Vec<crate::core::repo::SystemImage> {
    let mut out = Vec::new();
    let base = sdk_root.join("system-images");
    let Ok(apis) = std::fs::read_dir(&base) else {
        return out;
    };
    for api in apis.flatten() {
        if !api.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let api = api.file_name().to_string_lossy().to_string();
        if !valid_path_segment(&api) {
            continue;
        }
        let Ok(tags) = std::fs::read_dir(base.join(&api)) else {
            continue;
        };
        for tag in tags.flatten() {
            if !tag.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let tag = tag.file_name().to_string_lossy().to_string();
            if !valid_path_segment(&tag) {
                continue;
            }
            let Ok(abis) = std::fs::read_dir(base.join(&api).join(&tag)) else {
                continue;
            };
            for abi in abis.flatten() {
                if !abi.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                let abi = abi.file_name().to_string_lossy().to_string();
                if !valid_path_segment(&abi) {
                    continue;
                }
                let image_dir = base.join(&api).join(&tag).join(&abi);
                if !image_dir.join("system.img").is_file()
                    || !image_dir.join("source.properties").is_file()
                {
                    continue;
                }
                out.push(crate::core::repo::SystemImage {
                    display_name: format!(
                        "Android {} · {}",
                        api_number(&api),
                        image_tag_display(&tag)
                    ),
                    api: api.clone(),
                    tag: tag.clone(),
                    abi: abi.clone(),
                    license_ids: vec![],
                    archive: crate::core::repo::Archive {
                        url: String::new(),
                        size: 0,
                        checksum: None,
                        host_os: None,
                        host_arch: None,
                    },
                });
            }
        }
    }
    out.sort_by(|left, right| {
        (&left.api, &left.tag, &left.abi).cmp(&(&right.api, &right.tag, &right.abi))
    });
    out
}

/// 复验向导所选镜像仍是当前 SDK 中完整安装的 system image。
pub fn validate_installed_image(sdk_root: &Path, image: &SystemImage) -> anyhow::Result<PathBuf> {
    for (label, segment) in [
        ("API", image.api.as_str()),
        ("tag", image.tag.as_str()),
        ("ABI", image.abi.as_str()),
    ] {
        if !valid_path_segment(segment) {
            bail!("系统镜像 {label} 路径片段非法：{segment:?}");
        }
    }
    let image_dir = sdk_root
        .join("system-images")
        .join(&image.api)
        .join(&image.tag)
        .join(&image.abi);
    if !image_dir.join("system.img").is_file() || !image_dir.join("source.properties").is_file() {
        bail!(
            "系统镜像不完整或已被移除（需要 system.img 与 source.properties）：{}",
            image_dir.display()
        );
    }
    Ok(image_dir)
}

fn valid_path_segment(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && segment != ".." && !segment.contains(['/', '\\', '\0'])
}

fn api_number(api: &str) -> String {
    api.strip_prefix("android-").unwrap_or(api).to_string()
}

/// 极简 ini 解析（`k=v` 行，忽略注释/空行）。
pub fn parse_ini(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_avd_home() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("liteavd-avd-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_image() -> SystemImage {
        SystemImage {
            api: "android-35".into(),
            tag: "google_apis".into(),
            abi: "x86_64".into(),
            display_name: "Google APIs Intel x86_64 Atom System Image".into(),
            license_ids: vec!["android-sdk-license".into()],
            archive: crate::core::repo::Archive {
                url: "x86_64-35_r09.zip".into(),
                size: 0,
                checksum: None,
                host_os: None,
                host_arch: None,
            },
        }
    }

    fn sample_spec(name: &str) -> AvdSpec {
        AvdSpec {
            name: name.into(),
            device: get_profile("pixel_2").unwrap(),
            image: sample_image(),
            ram_mb: 1536,
            data_partition_mb: 6144,
            sdcard: Some("512M".into()),
            gpu: GpuMode::Auto,
        }
    }

    #[test]
    fn creates_avd_with_ini_and_config() {
        let home = temp_avd_home();
        let info = create_avd_in_root(&sample_spec("Pixel_2_API_35"), &home).unwrap();
        assert!(home.join("Pixel_2_API_35.ini").exists());
        assert!(info.path.join("config.ini").exists());

        let cfg = &info.config;
        assert_eq!(cfg.get("AvdId").map(String::as_str), Some("Pixel_2_API_35"));
        assert_eq!(cfg.get("abi.type").map(String::as_str), Some("x86_64"));
        assert_eq!(
            cfg.get("image.sysdir.1").map(String::as_str),
            Some("system-images/android-35/google_apis/x86_64/")
        );
        assert_eq!(cfg.get("hw.lcd.width").map(String::as_str), Some("1080"));
        assert_eq!(cfg.get("hw.lcd.height").map(String::as_str), Some("1920"));
        assert_eq!(cfg.get("hw.lcd.density").map(String::as_str), Some("420"));
        assert_eq!(cfg.get("hw.ramSize").map(String::as_str), Some("1536"));
        assert_eq!(cfg.get("tag.id").map(String::as_str), Some("google_apis"));
        assert!(cfg.get("hw.device.hash2").unwrap().starts_with("MD5:"));

        // 清理（避免影响其他测试）
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn rejects_invalid_names() {
        assert!(!valid_name(""));
        assert!(!valid_name("my avd"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(".hidden"));
        assert!(valid_name("Pixel_9_Pro"));
    }

    #[test]
    fn profile_and_creation_config_are_versioned_and_validated() {
        let catalog = builtin_profile_catalog();
        catalog.validate().unwrap();
        assert_eq!(catalog.schema_version, PROFILE_CATALOG_SCHEMA_VERSION);
        let encoded = serde_json::to_vec(&catalog).unwrap();
        let path = temp_avd_home().join("profiles.json");
        std::fs::write(&path, encoded).unwrap();
        assert_eq!(load_profile_catalog(&path).unwrap(), catalog);

        let mut unsupported = catalog.clone();
        unsupported.schema_version += 1;
        std::fs::write(&path, serde_json::to_vec(&unsupported).unwrap()).unwrap();
        assert!(
            load_profile_catalog(&path)
                .unwrap_err()
                .to_string()
                .contains("schema")
        );

        let mut duplicate = catalog.clone();
        duplicate.profiles.push(duplicate.profiles[0].clone());
        assert!(
            duplicate
                .validate()
                .unwrap_err()
                .to_string()
                .contains("重复")
        );

        let mut config = AvdCreationConfig::default();
        config.validate().unwrap();
        assert_eq!(config.schema_version, AVD_CREATION_SCHEMA_VERSION);
        assert_eq!(config.data_partition_mb, 6144);
        config.schema_version += 1;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("schema")
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn lists_and_deletes() {
        let home = temp_avd_home();
        create_avd_in_root(&sample_spec("devA"), &home).unwrap();
        create_avd_in_root(&sample_spec("devB"), &home).unwrap();
        let avds = list_avds_in_root(&home);
        let names: Vec<_> = avds.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"devA") && names.contains(&"devB"));

        delete_avd_in_root_if("devA", &home, |_| false).unwrap();
        let avds = list_avds_in_root(&home);
        let names: Vec<_> = avds.iter().map(|a| a.name.as_str()).collect();
        assert!(!names.contains(&"devA"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn scans_installed_images() {
        let dir = std::env::temp_dir().join(format!("liteavd-scan-{}", std::process::id()));
        let root = dir.join("sdk");
        let valid = root.join("system-images/android-35/google_apis/x86_64");
        std::fs::create_dir_all(&valid).unwrap();
        std::fs::write(valid.join("system.img"), b"image").unwrap();
        std::fs::write(valid.join("source.properties"), b"Pkg.Revision=1").unwrap();
        std::fs::create_dir_all(
            root.join("system-images/android-36/google_apis_playstore/arm64-v8a"),
        )
        .unwrap();
        let imgs = scan_installed_images(&root);
        assert_eq!(imgs.len(), 1, "空 ABI 目录不能冒充已安装镜像");
        let first = imgs.iter().find(|i| i.api == "android-35").unwrap();
        assert_eq!(first.tag, "google_apis");
        assert_eq!(first.abi, "x86_64");
        assert!(first.display_name.contains("35"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_ini_prevents_creation_without_leaving_avd_directory() {
        let home = temp_avd_home();
        std::fs::write(home.join("reserved.ini"), "existing").unwrap();

        let error = create_avd_in_root(&sample_spec("reserved"), &home).unwrap_err();

        assert!(error.to_string().contains("已存在"));
        assert!(!home.join("reserved.avd").exists());
        assert_eq!(
            std::fs::read_to_string(home.join("reserved.ini")).unwrap(),
            "existing"
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn publish_failure_rolls_back_both_avd_artifacts() {
        let home = temp_avd_home();

        let error = create_avd_in_root_with_publish_hook(&sample_spec("rollback"), &home, || {
            anyhow::bail!("injected publish failure")
        })
        .unwrap_err();

        assert!(error.to_string().contains("injected publish failure"));
        assert!(!home.join("rollback.avd").exists());
        assert!(!home.join("rollback.ini").exists());
        assert!(
            std::fs::read_dir(&home)
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().contains("rollback"))
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn concurrent_creation_of_same_name_is_rejected_without_overwrite() {
        let home = temp_avd_home();
        let worker_home = home.clone();
        let (published_tx, published_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            create_avd_in_root_with_publish_hook(&sample_spec("concurrent"), &worker_home, || {
                published_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        });
        published_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();

        let error = create_avd_in_root(&sample_spec("concurrent"), &home).unwrap_err();
        assert!(error.to_string().contains("正忙"));
        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert!(home.join("concurrent.ini").is_file());
        assert!(home.join("concurrent.avd/config.ini").is_file());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn refuses_to_delete_running_avd_without_touching_files() {
        let home = temp_avd_home();
        create_avd_in_root(&sample_spec("busy"), &home).unwrap();

        let error = delete_avd_in_root_if("busy", &home, |_| true).unwrap_err();

        assert!(error.to_string().contains("正在运行"));
        assert!(home.join("busy.ini").exists());
        assert!(home.join("busy.avd/config.ini").exists());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn list_ignores_incomplete_ini_without_config() {
        let home = temp_avd_home();
        std::fs::write(
            home.join("broken.ini"),
            format!("path={}\n", home.join("broken.avd").display()),
        )
        .unwrap();

        assert!(
            list_avds_in_root(&home)
                .iter()
                .all(|avd| avd.name != "broken")
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn parses_ini() {
        let map = parse_ini("a=1\n# comment\n\nb = 2 \n");
        assert_eq!(map.get("a").map(String::as_str), Some("1"));
        assert_eq!(map.get("b").map(String::as_str), Some("2"));
    }
}
