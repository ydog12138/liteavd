//! 应用设置：版本化 TOML、显式迁移状态与同目录原子发布。

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

use crate::core::scheduler::SchedulerConfig;

pub use crate::core::avd::ManagedGpuPolicy;

pub const SETTINGS_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_DOWNLOAD_CACHE_LIMIT_MB: u64 = 8192;
pub const MIN_DOWNLOAD_CACHE_LIMIT_MB: u64 = 512;
pub const MAX_DOWNLOAD_CACHE_LIMIT_MB: u64 = 65_536;
pub const MAX_CONCURRENT_STARTS: usize = 4;
const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppLogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

impl AppLogLevel {
    pub const ALL: [Self; 4] = [Self::Error, Self::Warn, Self::Info, Self::Debug];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Error => "仅错误",
            Self::Warn => "警告与错误",
            Self::Info => "信息（推荐）",
            Self::Debug => "调试",
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Error => 0,
            Self::Warn => 1,
            Self::Info => 2,
            Self::Debug => 3,
        }
    }
}

static ACTIVE_LOG_LEVEL: AtomicU8 = AtomicU8::new(AppLogLevel::Info.rank());

pub fn configure_log_level(level: AppLogLevel) {
    ACTIVE_LOG_LEVEL.store(level.rank(), Ordering::Relaxed);
}

pub fn log_enabled(level: AppLogLevel) -> bool {
    level.rank() <= ACTIVE_LOG_LEVEL.load(Ordering::Relaxed)
}

pub fn emit(level: AppLogLevel, message: fmt::Arguments<'_>) {
    if log_enabled(level) {
        eprintln!("[{level}] {message}");
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub schema_version: u32,
    /// SDK 根目录（如 /home/user/liteavd-sdk）。
    pub sdk_root: Option<String>,
    pub max_concurrent_starts: usize,
    /// `None` 表示不限制；调度器不会擅自缩小 AVD RAM。
    pub memory_budget_mb: Option<u64>,
    /// 约束 managed desktop-host 与接管 host-GPU 实例；headless swangle 不占 host slot。
    pub host_gpu_slots: Option<u32>,
    pub download_cache_limit_mb: u64,
    pub log_level: AppLogLevel,
    #[serde(default)]
    pub managed_gpu_policy: ManagedGpuPolicy,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            sdk_root: None,
            max_concurrent_starts: 1,
            memory_budget_mb: None,
            host_gpu_slots: None,
            download_cache_limit_mb: DEFAULT_DOWNLOAD_CACHE_LIMIT_MB,
            log_level: AppLogLevel::Info,
            managed_gpu_policy: ManagedGpuPolicy::HeadlessSwangle,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != SETTINGS_SCHEMA_VERSION {
            bail!(
                "不支持的 settings schema {}（当前为 {}）",
                self.schema_version,
                SETTINGS_SCHEMA_VERSION
            );
        }
        if !(1..=MAX_CONCURRENT_STARTS).contains(&self.max_concurrent_starts) {
            bail!(
                "启动并发必须在 1..={MAX_CONCURRENT_STARTS} 之间：{}",
                self.max_concurrent_starts
            );
        }
        if let Some(memory) = self.memory_budget_mb
            && !(1024..=262_144).contains(&memory)
        {
            bail!("内存预算必须在 1024..=262144 MiB 之间，或设为不限制");
        }
        if let Some(slots) = self.host_gpu_slots
            && !(1..=16).contains(&slots)
        {
            bail!("host GPU slot 必须在 1..=16 之间，或设为不限制");
        }
        if !(MIN_DOWNLOAD_CACHE_LIMIT_MB..=MAX_DOWNLOAD_CACHE_LIMIT_MB)
            .contains(&self.download_cache_limit_mb)
        {
            bail!(
                "下载缓存上限必须在 {MIN_DOWNLOAD_CACHE_LIMIT_MB}..={MAX_DOWNLOAD_CACHE_LIMIT_MB} MiB 之间"
            );
        }
        if let Some(root) = self.sdk_root.as_deref()
            && (root.trim().is_empty() || root.contains('\0'))
        {
            bail!("SDK 路径不能为空且不能包含 NUL");
        }
        Ok(())
    }

    pub fn scheduler_config(&self) -> SchedulerConfig {
        SchedulerConfig {
            max_concurrent_starts: self.max_concurrent_starts,
            memory_budget_mb: self.memory_budget_mb,
            gpu_slots: self.host_gpu_slots,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsLoadStatus {
    Missing,
    Current,
    Migrated { from_schema: u32 },
    Invalid { message: String },
}

impl SettingsLoadStatus {
    pub fn warning(&self) -> Option<String> {
        match self {
            Self::Missing | Self::Current => None,
            Self::Migrated { from_schema } => Some(format!(
                "设置已从 schema {from_schema} 迁移到 {SETTINGS_SCHEMA_VERSION}；保存后将写入新格式"
            )),
            Self::Invalid { message } => Some(format!(
                "设置文件不可用，当前使用安全默认值；原文件未被覆盖：{message}"
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSettings {
    pub settings: Settings,
    pub status: SettingsLoadStatus,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySettingsV0 {
    sdk_root: Option<String>,
}

/// 设置文件路径：`$XDG_CONFIG_HOME/liteavd/settings.toml`，否则使用
/// `~/.config/liteavd/settings.toml`。
pub fn settings_path() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(dir).join("liteavd/settings.toml"));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".config/liteavd/settings.toml"))
        .context("无法确定设置目录：XDG_CONFIG_HOME 和 HOME 均未设置")
}

pub fn load() -> LoadedSettings {
    match settings_path() {
        Ok(path) => load_from(&path),
        Err(error) => invalid_load(error.to_string()),
    }
}

pub fn load_from(path: &Path) -> LoadedSettings {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return invalid_load(format!("{} 不是普通设置文件", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadedSettings {
                settings: Settings::default(),
                status: SettingsLoadStatus::Missing,
            };
        }
        Err(error) => {
            return invalid_load(format!("读取 {} 失败：{error}", path.display()));
        }
    };
    if metadata.len() > MAX_SETTINGS_BYTES {
        return invalid_load(format!(
            "{} 超过 {} 字节上限",
            path.display(),
            MAX_SETTINGS_BYTES
        ));
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => return invalid_load(format!("打开 {} 失败：{error}", path.display())),
    };
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    if let Err(error) = file.take(MAX_SETTINGS_BYTES + 1).read_to_end(&mut bytes) {
        return invalid_load(format!("读取 {} 失败：{error}", path.display()));
    }
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return invalid_load(format!(
            "{} 在读取期间增长并超过 {} 字节上限",
            path.display(),
            MAX_SETTINGS_BYTES
        ));
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => return invalid_load(format!("{} 不是 UTF-8：{error}", path.display())),
    };
    let value: toml::Value = match toml::from_str(&text) {
        Ok(value) => value,
        Err(error) => return invalid_load(format!("解析 {} 失败：{error}", path.display())),
    };
    let schema = value
        .get("schema_version")
        .and_then(toml::Value::as_integer);
    if schema.is_none() {
        let legacy: LegacySettingsV0 = match toml::from_str(&text) {
            Ok(settings) => settings,
            Err(error) => {
                return invalid_load(format!("旧版设置 {} 无法迁移：{error}", path.display()));
            }
        };
        let settings = Settings {
            sdk_root: legacy.sdk_root,
            ..Settings::default()
        };
        if let Err(error) = settings.validate() {
            return invalid_load(format!("旧版设置 {} 无效：{error:#}", path.display()));
        }
        return LoadedSettings {
            settings,
            status: SettingsLoadStatus::Migrated { from_schema: 0 },
        };
    }
    if schema != Some(i64::from(SETTINGS_SCHEMA_VERSION)) {
        return invalid_load(format!(
            "{} 使用不支持的 schema {}",
            path.display(),
            schema.unwrap_or_default()
        ));
    }
    let settings: Settings = match toml::from_str(&text) {
        Ok(settings) => settings,
        Err(error) => return invalid_load(format!("解析 {} 失败：{error}", path.display())),
    };
    if let Err(error) = settings.validate() {
        return invalid_load(format!("{} 校验失败：{error:#}", path.display()));
    }
    LoadedSettings {
        settings,
        status: SettingsLoadStatus::Current,
    }
}

fn invalid_load(message: String) -> LoadedSettings {
    LoadedSettings {
        settings: Settings::default(),
        status: SettingsLoadStatus::Invalid { message },
    }
}

pub fn save(settings: &Settings) -> anyhow::Result<()> {
    save_to(&settings_path()?, settings)
}

pub fn save_to(path: &Path, settings: &Settings) -> anyhow::Result<()> {
    settings.validate()?;
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        bail!("拒绝覆盖非普通设置文件：{}", path.display());
    }
    let dir = path.parent().context("设置文件路径没有父目录")?;
    std::fs::create_dir_all(dir).with_context(|| format!("创建设置目录失败：{}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("设置设置目录权限失败：{}", dir.display()))?;

    let text = toml::to_string_pretty(settings).context("序列化设置失败")?;
    let mut temp = AtomicTemp::create(path)?;
    temp.file
        .write_all(text.as_bytes())
        .with_context(|| format!("写入临时设置失败：{}", temp.path.display()))?;
    temp.file
        .sync_all()
        .with_context(|| format!("同步临时设置失败：{}", temp.path.display()))?;

    #[cfg(test)]
    if FAIL_BEFORE_RENAME.with(|failure| failure.replace(false)) {
        bail!("测试注入：settings rename 前失败");
    }

    std::fs::rename(&temp.path, path).with_context(|| {
        format!(
            "原子发布设置失败：{} -> {}",
            temp.path.display(),
            path.display()
        )
    })?;
    temp.published = true;
    File::open(dir)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("同步设置目录失败：{}", dir.display()))?;
    Ok(())
}

struct AtomicTemp {
    path: PathBuf,
    file: File,
    published: bool,
}

impl AtomicTemp {
    fn create(target: &Path) -> anyhow::Result<Self> {
        let file_name = target
            .file_name()
            .and_then(|name| name.to_str())
            .context("设置文件名不是有效 UTF-8")?;
        for sequence in 0..128u32 {
            let path = target.with_file_name(format!(
                ".{file_name}.tmp-{}-{sequence}",
                std::process::id()
            ));
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file,
                        published: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("创建同目录临时设置失败：{}", path.display()));
                }
            }
        }
        bail!("无法为 {} 分配临时设置文件", target.display())
    }
}

impl Drop for AtomicTemp {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl fmt::Display for AppLogLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_BEFORE_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt as _, symlink};
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path() -> PathBuf {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "liteavd-settings-test-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("settings.toml")
    }

    #[test]
    fn roundtrip_current_schema_is_private() {
        let path = temp_path();
        let settings = Settings {
            sdk_root: Some("/opt/sdk".into()),
            max_concurrent_starts: 2,
            memory_budget_mb: Some(8192),
            host_gpu_slots: Some(1),
            download_cache_limit_mb: 4096,
            log_level: AppLogLevel::Debug,
            ..Settings::default()
        };
        save_to(&path, &settings).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.settings, settings);
        assert_eq!(loaded.status, SettingsLoadStatus::Current);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn desktop_host_policy_is_serde_compatible_and_persistent() {
        let path = temp_path();
        let settings = Settings {
            managed_gpu_policy: ManagedGpuPolicy::DesktopHost,
            ..Settings::default()
        };
        save_to(&path, &settings).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("managed_gpu_policy = \"desktop_host\""));
        assert_eq!(
            load_from(&path).settings.managed_gpu_policy,
            ManagedGpuPolicy::DesktopHost
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn current_settings_without_policy_use_headless_default() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "schema_version = 1\nsdk_root = \"/sdk\"\nmax_concurrent_starts = 1\nmemory_budget_mb = 8192\nhost_gpu_slots = 1\ndownload_cache_limit_mb = 8192\nlog_level = \"info\"\n",
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.status, SettingsLoadStatus::Current);
        assert_eq!(
            loaded.settings.managed_gpu_policy,
            ManagedGpuPolicy::HeadlessSwangle
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn migrates_legacy_sdk_without_writing_source() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "sdk_root = '/legacy/sdk'\n").unwrap();
        let before = std::fs::read(&path).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.settings.sdk_root.as_deref(), Some("/legacy/sdk"));
        assert_eq!(
            loaded.status,
            SettingsLoadStatus::Migrated { from_schema: 0 }
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn corrupt_or_future_schema_reports_fallback_without_overwrite() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not = [valid").unwrap();
        let before = std::fs::read(&path).unwrap();
        let loaded = load_from(&path);
        assert!(matches!(loaded.status, SettingsLoadStatus::Invalid { .. }));
        assert_eq!(loaded.settings, Settings::default());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        std::fs::write(&path, "schema_version = 99\n").unwrap();
        let loaded = load_from(&path);
        assert!(matches!(loaded.status, SettingsLoadStatus::Invalid { .. }));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn oversized_settings_are_bounded_and_never_rewritten() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let oversized = vec![b'x'; (MAX_SETTINGS_BYTES + 1) as usize];
        std::fs::write(&path, &oversized).unwrap();
        let loaded = load_from(&path);
        assert!(matches!(
            loaded.status,
            SettingsLoadStatus::Invalid { ref message } if message.contains("超过")
        ));
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            MAX_SETTINGS_BYTES + 1
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn injected_failure_preserves_previous_file_and_cleans_temp() {
        let path = temp_path();
        save_to(&path, &Settings::default()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let changed = Settings {
            sdk_root: Some("/new/sdk".into()),
            ..Settings::default()
        };
        FAIL_BEFORE_RENAME.with(|failure| failure.set(true));
        assert!(save_to(&path, &changed).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec!["settings.toml"]);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn injected_failure_is_isolated_to_current_thread() {
        let injected_path = temp_path();
        let other_path = temp_path();
        FAIL_BEFORE_RENAME.with(|failure| failure.set(true));

        let other_path_for_thread = other_path.clone();
        std::thread::spawn(move || save_to(&other_path_for_thread, &Settings::default()).unwrap())
            .join()
            .unwrap();

        assert!(save_to(&injected_path, &Settings::default()).is_err());
        assert!(other_path.is_file());
        assert!(!injected_path.exists());
        std::fs::remove_dir_all(other_path.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(injected_path.parent().unwrap()).unwrap();
    }

    #[test]
    fn refuses_to_replace_symlink() {
        let path = temp_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let target = path.parent().unwrap().join("target");
        std::fs::write(&target, "unchanged").unwrap();
        symlink(&target, &path).unwrap();
        let error = save_to(&path, &Settings::default()).unwrap_err();
        assert!(error.to_string().contains("非普通设置文件"));
        assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn runtime_log_level_is_applied_without_reloading_settings() {
        configure_log_level(AppLogLevel::Warn);
        assert!(log_enabled(AppLogLevel::Error));
        assert!(log_enabled(AppLogLevel::Warn));
        assert!(!log_enabled(AppLogLevel::Info));
        assert!(!log_enabled(AppLogLevel::Debug));
        configure_log_level(AppLogLevel::Info);
    }
}
