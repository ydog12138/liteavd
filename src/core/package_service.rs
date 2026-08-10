//! 托管 SDK 组件操作：稳定下载缓存、许可门禁、安装/卸载和类型化进度。

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use directories::BaseDirs;
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::core::download::{DownloadError, Downloader};
use crate::core::install::{self, ComponentKind};
use crate::core::repo::{Archive, Checksum};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageLicense {
    pub id: String,
    pub text: Option<String>,
}

impl PackageLicense {
    pub fn new(id: impl Into<String>, text: Option<String>) -> Self {
        Self {
            id: id.into(),
            text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredLicense {
    pub id: String,
    pub text: String,
    pub normalized_text_sha1: String,
}

#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub sdk_root: PathBuf,
    pub kind: ComponentKind,
    pub archive: Archive,
    pub url: String,
    pub licenses: Vec<PackageLicense>,
}

#[derive(Debug, Clone)]
pub enum PackageOperation {
    Install(InstallRequest),
    Uninstall {
        sdk_root: PathBuf,
        kind: ComponentKind,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageEvent {
    CheckingLicenses,
    CheckingCache { path: PathBuf },
    Downloading { downloaded: u64, total: u64 },
    Installing,
    Uninstalling,
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageOutcome {
    Installed {
        component_path: PathBuf,
        cache_path: PathBuf,
        cache_reused: bool,
    },
    Uninstalled,
}

#[derive(thiserror::Error, Debug)]
pub enum PackageError {
    #[error("无法确定用户缓存目录")]
    CacheRootUnavailable,
    #[error("创建 HTTP 客户端失败：{source}")]
    Client {
        #[source]
        source: anyhow::Error,
    },
    #[error("许可 {id} 缺少可展示的文本")]
    MissingLicenseText { id: String },
    #[error("许可 {id} 的当前文本尚未接受（{normalized_text_sha1}）")]
    LicenseNotAccepted {
        id: String,
        normalized_text_sha1: String,
    },
    #[error("用户拒绝了许可协议")]
    LicenseDeclined,
    #[error("许可对话框在确认前被关闭")]
    LicenseDialogClosed,
    #[error("许可操作失败：{source}")]
    License {
        #[source]
        source: anyhow::Error,
    },
    #[error("缓存 key 无效：{reason}")]
    InvalidCacheKey { reason: String },
    #[error("缓存 I/O 失败：{source}")]
    CacheIo {
        #[source]
        source: std::io::Error,
    },
    #[error("缓存正被另一个下载使用：{path}")]
    CacheBusy { path: PathBuf },
    #[error("下载缓存上限不足：上限 {limit_bytes}B，当前不可回收/所需共 {required_bytes}B")]
    CacheLimitExceeded {
        limit_bytes: u64,
        required_bytes: u64,
    },
    #[error("下载失败：{0}")]
    Download(#[from] DownloadError),
    #[error("下载缓存校验失败：{reason}")]
    CacheValidation { reason: String },
    #[error("安装失败：{source}")]
    Install {
        #[source]
        source: anyhow::Error,
    },
    #[error("卸载失败：{source}")]
    Uninstall {
        #[source]
        source: anyhow::Error,
    },
    #[error("后台组件任务失败：{0}")]
    Join(String),
}

pub struct PackageService {
    cache_root: PathBuf,
    cache_limit_bytes: u64,
    downloader: Downloader,
}

impl PackageService {
    pub fn new() -> Result<Self, PackageError> {
        let cache_root = BaseDirs::new()
            .map(|dirs| dirs.cache_dir().join("liteavd/downloads"))
            .ok_or(PackageError::CacheRootUnavailable)?;
        let settings = crate::core::settings::load().settings;
        Self::with_cache_root_and_limit(
            cache_root,
            settings.download_cache_limit_mb.saturating_mul(1024 * 1024),
        )
    }

    pub fn with_cache_root(cache_root: PathBuf) -> Result<Self, PackageError> {
        Self::with_cache_root_and_limit(
            cache_root,
            crate::core::settings::DEFAULT_DOWNLOAD_CACHE_LIMIT_MB * 1024 * 1024,
        )
    }

    pub fn with_cache_root_and_limit(
        cache_root: PathBuf,
        cache_limit_bytes: u64,
    ) -> Result<Self, PackageError> {
        let downloader = Downloader::new().map_err(|source| PackageError::Client { source })?;
        Ok(Self {
            cache_root,
            cache_limit_bytes,
            downloader,
        })
    }

    pub fn cache_root(&self) -> &Path {
        &self.cache_root
    }

    pub fn missing_licenses(
        &self,
        sdk_root: &Path,
        licenses: &[PackageLicense],
    ) -> Result<Vec<RequiredLicense>, PackageError> {
        let mut missing = Vec::new();
        for license in licenses {
            let text = license
                .text
                .as_deref()
                .filter(|text| !install::normalize_license_text(text).is_empty())
                .ok_or_else(|| PackageError::MissingLicenseText {
                    id: license.id.clone(),
                })?;
            let hash = install::license_hash(text);
            let accepted = install::is_license_accepted(sdk_root, &license.id, text)
                .map_err(|source| PackageError::License { source })?;
            if !accepted {
                missing.push(RequiredLicense {
                    id: license.id.clone(),
                    text: text.to_string(),
                    normalized_text_sha1: hash,
                });
            }
        }
        Ok(missing)
    }

    pub fn accept_licenses(
        &self,
        sdk_root: &Path,
        licenses: &[RequiredLicense],
    ) -> Result<(), PackageError> {
        for license in licenses {
            install::accept_license(sdk_root, &license.id, &license.text)
                .map_err(|source| PackageError::License { source })?;
        }
        Ok(())
    }

    pub fn cache_path(&self, archive: &Archive, url: &str) -> Result<PathBuf, PackageError> {
        Ok(self
            .cache_root
            .join(cache_key(archive, url)?)
            .join("archive.zip"))
    }

    pub async fn execute(
        &self,
        operation: PackageOperation,
        progress: impl Fn(PackageEvent),
    ) -> Result<PackageOutcome, PackageError> {
        match operation {
            PackageOperation::Install(request) => self.install(request, progress).await,
            PackageOperation::Uninstall { sdk_root, kind } => {
                progress(PackageEvent::Uninstalling);
                tokio::task::spawn_blocking(move || install::uninstall_component(&sdk_root, &kind))
                    .await
                    .map_err(|error| PackageError::Join(error.to_string()))?
                    .map_err(|source| PackageError::Uninstall { source })?;
                progress(PackageEvent::Finished);
                Ok(PackageOutcome::Uninstalled)
            }
        }
    }

    async fn install(
        &self,
        request: InstallRequest,
        progress: impl Fn(PackageEvent),
    ) -> Result<PackageOutcome, PackageError> {
        progress(PackageEvent::CheckingLicenses);
        if let Some(license) = self
            .missing_licenses(&request.sdk_root, &request.licenses)?
            .into_iter()
            .next()
        {
            return Err(PackageError::LicenseNotAccepted {
                id: license.id,
                normalized_text_sha1: license.normalized_text_sha1,
            });
        }

        let cache_path = self.cache_path(&request.archive, &request.url)?;
        progress(PackageEvent::CheckingCache {
            path: cache_path.clone(),
        });
        ensure_private_directory(&self.cache_root)?;
        ensure_private_parent(&cache_path)?;
        // 配额检查与下载共用根 lease，避免不同 cache key 并发判断后共同突破上限。
        let _quota_lock = CacheLock::acquire(&self.cache_root.join("quota"))?;
        let _cache_lock = CacheLock::acquire(&cache_path)?;
        self.ensure_cache_capacity(&cache_path, request.archive.size)?;

        let cache_reused = validate_cached_archive(&cache_path, &request.archive)?;
        if !cache_reused {
            if request.archive.size == 0 {
                return Err(PackageError::CacheValidation {
                    reason: "仓库未提供归档大小，无法在有限缓存配额内安全下载".into(),
                });
            }
            if cache_path.exists() {
                std::fs::remove_file(&cache_path)
                    .map_err(|source| PackageError::CacheIo { source })?;
            }
            self.downloader
                .download(
                    &request.url,
                    &cache_path,
                    request.archive.checksum.as_ref(),
                    |downloaded, server_total| {
                        progress(PackageEvent::Downloading {
                            downloaded,
                            total: if server_total > 0 {
                                server_total
                            } else {
                                request.archive.size
                            },
                        });
                    },
                )
                .await?;
            if !validate_cached_archive(&cache_path, &request.archive)? {
                return Err(PackageError::CacheValidation {
                    reason: format!("{} 与仓库元数据不匹配", cache_path.display()),
                });
            }
        }
        let actual_size = std::fs::metadata(&cache_path)
            .map_err(|source| PackageError::CacheIo { source })?
            .len();
        self.ensure_cache_capacity(&cache_path, actual_size)?;
        drop(_cache_lock);
        drop(_quota_lock);

        progress(PackageEvent::Installing);
        let zip_path = cache_path.clone();
        let sdk_root = request.sdk_root;
        let kind = request.kind;
        let component_path = tokio::task::spawn_blocking(move || {
            install::install_component(&zip_path, &sdk_root, &kind)
        })
        .await
        .map_err(|error| PackageError::Join(error.to_string()))?
        .map_err(|source| PackageError::Install { source })?;
        progress(PackageEvent::Finished);
        Ok(PackageOutcome::Installed {
            component_path,
            cache_path,
            cache_reused,
        })
    }

    /// 保留当前 cache key，并按最旧优先清理其他、且没有活跃 lease 的条目。
    fn ensure_cache_capacity(
        &self,
        current_archive: &Path,
        required_size: u64,
    ) -> Result<(), PackageError> {
        if required_size > self.cache_limit_bytes {
            return Err(PackageError::CacheLimitExceeded {
                limit_bytes: self.cache_limit_bytes,
                required_bytes: required_size,
            });
        }
        let current_entry =
            current_archive
                .parent()
                .ok_or_else(|| PackageError::CacheValidation {
                    reason: "当前缓存文件没有条目目录".into(),
                })?;
        let mut used = 0u64;
        let mut candidates = Vec::new();
        let entries = match std::fs::read_dir(&self.cache_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => return Err(PackageError::CacheIo { source }),
        };
        for entry in entries {
            let entry = entry.map_err(|source| PackageError::CacheIo { source })?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|source| PackageError::CacheIo { source })?;
            if !metadata.file_type().is_dir() || path == current_entry {
                continue;
            }
            let size = cache_entry_payload_size(&path)?;
            used = used.saturating_add(size);
            candidates.push(CachePruneCandidate {
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                path,
                size,
            });
        }
        if used.saturating_add(required_size) <= self.cache_limit_bytes {
            return Ok(());
        }
        candidates.sort_by(|left, right| {
            left.modified
                .cmp(&right.modified)
                .then_with(|| left.path.cmp(&right.path))
        });
        for candidate in candidates {
            let archive = candidate.path.join("archive.zip");
            let lease = match CacheLock::acquire(&archive) {
                Ok(lease) => lease,
                Err(PackageError::CacheBusy { .. }) => continue,
                Err(error) => return Err(error),
            };
            let tombstone = self.cache_root.join(format!(
                ".prune-{}-{}",
                std::process::id(),
                candidate
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("cache")
            ));
            if tombstone.exists() {
                drop(lease);
                continue;
            }
            match std::fs::rename(&candidate.path, &tombstone) {
                Ok(()) => {
                    drop(lease);
                    std::fs::remove_dir_all(&tombstone)
                        .map_err(|source| PackageError::CacheIo { source })?;
                    used = used.saturating_sub(candidate.size);
                    if used.saturating_add(required_size) <= self.cache_limit_bytes {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => drop(lease),
                Err(source) => return Err(PackageError::CacheIo { source }),
            }
        }
        Err(PackageError::CacheLimitExceeded {
            limit_bytes: self.cache_limit_bytes,
            required_bytes: used.saturating_add(required_size),
        })
    }
}

#[derive(Debug)]
struct CachePruneCandidate {
    modified: SystemTime,
    path: PathBuf,
    size: u64,
}

fn cache_entry_payload_size(path: &Path) -> Result<u64, PackageError> {
    let mut size = 0u64;
    for name in ["archive.zip", "archive.zip.part"] {
        let candidate = path.join(name);
        match std::fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata.file_type().is_file() => {
                size = size.saturating_add(metadata.len());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(PackageError::CacheIo { source }),
        }
    }
    Ok(size)
}

fn cache_key(archive: &Archive, url: &str) -> Result<String, PackageError> {
    if let Some(checksum) = &archive.checksum {
        let (algorithm, expected_len) = match checksum {
            Checksum::Sha1(_) => ("sha1", 40),
            Checksum::Sha256(_) => ("sha256", 64),
        };
        let hex = checksum.hex().to_ascii_lowercase();
        if hex.len() != expected_len || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(PackageError::InvalidCacheKey {
                reason: format!("{algorithm} checksum 格式无效"),
            });
        }
        return Ok(format!("{algorithm}-{hex}"));
    }
    let mut hasher = Sha256::new();
    hasher.update(url.as_bytes());
    Ok(format!("url-sha256-{:x}", hasher.finalize()))
}

fn ensure_private_parent(path: &Path) -> Result<(), PackageError> {
    let parent = path.parent().ok_or_else(|| PackageError::CacheValidation {
        reason: "缓存路径没有父目录".to_string(),
    })?;
    std::fs::create_dir_all(parent).map_err(|source| PackageError::CacheIo { source })?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .map_err(|source| PackageError::CacheIo { source })
}

fn ensure_private_directory(path: &Path) -> Result<(), PackageError> {
    std::fs::create_dir_all(path).map_err(|source| PackageError::CacheIo { source })?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|source| PackageError::CacheIo { source })
}

fn validate_cached_archive(path: &Path, archive: &Archive) -> Result<bool, PackageError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => return Err(PackageError::CacheIo { source }),
    };
    if !metadata.file_type().is_file() {
        return Err(PackageError::CacheValidation {
            reason: format!("{} 不是普通文件", path.display()),
        });
    }
    if archive.size > 0 && metadata.len() != archive.size {
        return Ok(false);
    }
    let Some(expected) = archive.checksum.as_ref() else {
        return Ok(archive.size > 0);
    };
    let file = File::open(path).map_err(|source| PackageError::CacheIo { source })?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut buffer = [0u8; 64 * 1024];
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|source| PackageError::CacheIo { source })?;
        if read == 0 {
            break;
        }
        match expected {
            Checksum::Sha1(_) => sha1.update(&buffer[..read]),
            Checksum::Sha256(_) => sha256.update(&buffer[..read]),
        }
    }
    let actual = match expected {
        Checksum::Sha1(_) => format!("{:x}", sha1.finalize()),
        Checksum::Sha256(_) => format!("{:x}", sha256.finalize()),
    };
    Ok(expected.hex().eq_ignore_ascii_case(&actual))
}

struct CacheLock {
    file: File,
}

impl CacheLock {
    fn acquire(cache_path: &Path) -> Result<Self, PackageError> {
        let lock_path = cache_path.with_extension("zip.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|source| PackageError::CacheIo { source })?;
        // SAFETY: CacheLock 持有 fd 直到下载/校验阶段结束。
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let source = std::io::Error::last_os_error();
            if source.kind() == std::io::ErrorKind::WouldBlock {
                return Err(PackageError::CacheBusy {
                    path: cache_path.to_path_buf(),
                });
            }
            return Err(PackageError::CacheIo { source });
        }
        Ok(Self { file })
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        // SAFETY: file fd 在 drop 结束前仍有效。
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir() -> PathBuf {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "liteavd-package-service-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn platform_tools_zip(path: &Path) -> Vec<u8> {
        let file = File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(
            "platform-tools/adb",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"fake adb").unwrap();
        zip.finish().unwrap();
        std::fs::read(path).unwrap()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    struct HttpServer {
        url: String,
        requests: Arc<AtomicU32>,
    }

    impl HttpServer {
        fn spawn(bytes: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/archive.zip", listener.local_addr().unwrap());
            let bytes = Arc::new(bytes);
            let requests = Arc::new(AtomicU32::new(0));
            let worker_requests = requests.clone();
            std::thread::spawn(move || {
                for _ in 0..20_000 {
                    match listener.accept() {
                        Ok((mut socket, _)) => {
                            worker_requests.fetch_add(1, Ordering::Relaxed);
                            let mut request = [0u8; 4096];
                            let read = std::io::Read::read(&mut socket, &mut request).unwrap();
                            let request = String::from_utf8_lossy(&request[..read]);
                            let range = request.lines().find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("range: bytes=")
                                    .and_then(|value| value.split('-').next())
                                    .and_then(|value| value.parse::<usize>().ok())
                            });
                            if let Some(start) = range.filter(|start| *start < bytes.len()) {
                                let body = &bytes[start..];
                                write!(
                                    socket,
                                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                                    body.len(),
                                    start,
                                    bytes.len() - 1,
                                    bytes.len()
                                )
                                .unwrap();
                                socket.write_all(body).unwrap();
                            } else {
                                write!(
                                    socket,
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    bytes.len()
                                )
                                .unwrap();
                                socket.write_all(&bytes).unwrap();
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self { url, requests }
        }
    }

    fn request(root: &Path, server: &HttpServer, bytes: &[u8]) -> InstallRequest {
        InstallRequest {
            sdk_root: root.join("sdk"),
            kind: ComponentKind::PlatformTools,
            archive: Archive {
                url: server.url.clone(),
                size: bytes.len() as u64,
                checksum: Some(Checksum::Sha256(sha256_hex(bytes))),
                host_os: None,
                host_arch: None,
            },
            url: server.url.clone(),
            licenses: vec![],
        }
    }

    #[test]
    fn license_plan_tracks_text_hash_and_missing_text() {
        let root = temp_dir();
        let service = PackageService::with_cache_root(root.join("cache")).unwrap();
        let sdk = root.join("sdk");
        let old = PackageLicense::new("license", Some("old text".into()));
        let new = PackageLicense::new("license", Some("new text".into()));
        let missing = service
            .missing_licenses(&sdk, std::slice::from_ref(&old))
            .unwrap();
        assert_eq!(missing.len(), 1);
        service.accept_licenses(&sdk, &missing).unwrap();
        assert!(service.missing_licenses(&sdk, &[old]).unwrap().is_empty());
        assert_eq!(service.missing_licenses(&sdk, &[new]).unwrap().len(), 1);
        assert!(matches!(
            service.missing_licenses(&sdk, &[PackageLicense::new("lost", None)]),
            Err(PackageError::MissingLicenseText { .. })
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cache_limit_prunes_oldest_unlocked_entry_and_preserves_current() {
        let root = temp_dir();
        let cache = root.join("cache");
        for name in ["a", "b", "current"] {
            std::fs::create_dir_all(cache.join(name)).unwrap();
        }
        std::fs::write(cache.join("a/archive.zip"), vec![1u8; 300]).unwrap();
        std::fs::write(cache.join("b/archive.zip.part"), vec![2u8; 300]).unwrap();
        let current = cache.join("current/archive.zip");
        let service = PackageService::with_cache_root_and_limit(cache.clone(), 1000).unwrap();
        service.ensure_cache_capacity(&current, 600).unwrap();

        assert!(!cache.join("a").exists());
        assert!(cache.join("b/archive.zip.part").is_file());
        assert!(cache.join("current").is_dir());
        assert!(std::fs::read_dir(&cache).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".prune-")
        }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cache_limit_never_removes_active_lease() {
        let root = temp_dir();
        let cache = root.join("cache");
        let active = cache.join("active/archive.zip");
        let current = cache.join("current/archive.zip");
        std::fs::create_dir_all(active.parent().unwrap()).unwrap();
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&active, vec![1u8; 400]).unwrap();
        let lease = CacheLock::acquire(&active).unwrap();
        let service = PackageService::with_cache_root_and_limit(cache, 500).unwrap();
        assert!(matches!(
            service.ensure_cache_capacity(&current, 200),
            Err(PackageError::CacheLimitExceeded {
                limit_bytes: 500,
                required_bytes: 600,
            })
        ));
        assert!(active.is_file());
        drop(lease);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn archive_larger_than_cache_limit_fails_before_download() {
        let root = temp_dir();
        let cache = root.join("cache");
        let current = cache.join("current/archive.zip");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        let service = PackageService::with_cache_root_and_limit(cache, 500).unwrap();
        assert!(matches!(
            service.ensure_cache_capacity(&current, 501),
            Err(PackageError::CacheLimitExceeded {
                limit_bytes: 500,
                required_bytes: 501,
            })
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unknown_archive_size_is_rejected_before_network_io() {
        let root = temp_dir();
        let bytes = platform_tools_zip(&root.join("source.zip"));
        let server = HttpServer::spawn(bytes.clone());
        let service = PackageService::with_cache_root(root.join("cache")).unwrap();
        let mut install = request(&root, &server, &bytes);
        install.archive.size = 0;
        let result = service
            .execute(PackageOperation::Install(install), |_| {})
            .await;
        assert!(matches!(
            result,
            Err(PackageError::CacheValidation { ref reason })
                if reason.contains("归档大小")
        ));
        assert_eq!(server.requests.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn installs_from_stable_cache_and_reuses_verified_archive() {
        let root = temp_dir();
        let bytes = platform_tools_zip(&root.join("source.zip"));
        let server = HttpServer::spawn(bytes.clone());
        let first = PackageService::with_cache_root(root.join("cache")).unwrap();
        let outcome = first
            .execute(
                PackageOperation::Install(request(&root, &server, &bytes)),
                |_| {},
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            PackageOutcome::Installed {
                cache_reused: false,
                ..
            }
        ));
        assert_eq!(server.requests.load(Ordering::Relaxed), 1);

        let second = PackageService::with_cache_root(root.join("cache")).unwrap();
        let outcome = second
            .execute(
                PackageOperation::Install(request(&root, &server, &bytes)),
                |_| {},
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            PackageOutcome::Installed {
                cache_reused: true,
                ..
            }
        ));
        assert_eq!(server.requests.load(Ordering::Relaxed), 1);
        assert!(root.join("sdk/platform-tools/adb").is_file());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn new_service_resumes_stable_part_and_can_uninstall() {
        let root = temp_dir();
        let bytes = platform_tools_zip(&root.join("source.zip"));
        let server = HttpServer::spawn(bytes.clone());
        let service = PackageService::with_cache_root(root.join("cache")).unwrap();
        let request = request(&root, &server, &bytes);
        let cache_path = service.cache_path(&request.archive, &request.url).unwrap();
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(
            cache_path.with_extension("zip.part"),
            &bytes[..bytes.len() / 2],
        )
        .unwrap();
        drop(service);

        let rebuilt = PackageService::with_cache_root(root.join("cache")).unwrap();
        rebuilt
            .execute(PackageOperation::Install(request), |_| {})
            .await
            .unwrap();
        assert_eq!(server.requests.load(Ordering::Relaxed), 1);
        assert_eq!(std::fs::read(&cache_path).unwrap(), bytes);
        rebuilt
            .execute(
                PackageOperation::Uninstall {
                    sdk_root: root.join("sdk"),
                    kind: ComponentKind::PlatformTools,
                },
                |_| {},
            )
            .await
            .unwrap();
        assert!(!root.join("sdk/platform-tools").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn corrupt_complete_cache_is_replaced_before_install() {
        let root = temp_dir();
        let bytes = platform_tools_zip(&root.join("source.zip"));
        let server = HttpServer::spawn(bytes.clone());
        let service = PackageService::with_cache_root(root.join("cache")).unwrap();
        let request = request(&root, &server, &bytes);
        let cache_path = service.cache_path(&request.archive, &request.url).unwrap();
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(&cache_path, vec![0u8; bytes.len()]).unwrap();

        let outcome = service
            .execute(PackageOperation::Install(request), |_| {})
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PackageOutcome::Installed {
                cache_reused: false,
                ..
            }
        ));
        assert_eq!(server.requests.load(Ordering::Relaxed), 1);
        assert_eq!(std::fs::read(cache_path).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(root);
    }
}
