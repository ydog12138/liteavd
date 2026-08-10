//! 模拟器 gRPC 的每 session ES256/JWT 身份。

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::core::emulator::RunningInstance;

pub const GRPC_JWT_ISSUER: &str = "liteavd";
const RECOVERY_VERSION: u32 = 1;
const RECOVERY_RECORD: &str = "recovery.json";
const RECOVERY_PRIVATE_KEY: &str = "identity.pk8";
const RECOVERY_LEASE: &str = "recovery.lock";
const MAX_RECOVERY_RECORD_BYTES: u64 = 16 * 1024;
const MAX_PRIVATE_KEY_BYTES: u64 = 4 * 1024;

/// 当前产品会调用、或在单设备纵切中即将调用的最小 gRPC 方法集合。
pub const GRPC_ALLOWED_METHODS: &[&str] = &[
    "/android.emulation.control.EmulatorController/getStatus",
    "/android.emulation.control.EmulatorController/getScreenshot",
    "/android.emulation.control.EmulatorController/streamAudio",
    "/android.emulation.control.EmulatorController/getMicrophoneState",
    "/android.emulation.control.EmulatorController/setMicrophoneState",
    "/android.emulation.control.EmulatorController/sendKey",
    "/android.emulation.control.EmulatorController/sendTouch",
    "/android.emulation.control.EmulatorController/sendMouse",
    "/android.emulation.control.SnapshotService/ListSnapshots",
    "/android.emulation.control.SnapshotService/SaveSnapshot",
    "/android.emulation.control.SnapshotService/LoadSnapshot",
    "/android.emulation.control.SnapshotService/DeleteSnapshot",
];

/// 显式、受 JWT 保护的 gRPC 启动配置。没有“0 表示默认”的隐式分支。
#[derive(Clone)]
pub struct GrpcLaunchConfig {
    port: u16,
    auth: Arc<GrpcJwtAuth>,
}

impl GrpcLaunchConfig {
    pub fn new(port: u16) -> anyhow::Result<Self> {
        if port == 0 {
            bail!("gRPC 端口不能为 0");
        }
        Ok(Self {
            port,
            auth: Arc::new(GrpcJwtAuth::new()?),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn auth(&self) -> &Arc<GrpcJwtAuth> {
        &self.auth
    }
}

impl fmt::Debug for GrpcLaunchConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GrpcLaunchConfig")
            .field("port", &self.port)
            .field("key_id", &self.auth.key_id)
            .finish()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryRecord {
    version: u32,
    engine_pid: u32,
    avd_name: String,
    console_port: u16,
    grpc_port: u16,
    key_id: String,
}

/// 运行期间私钥驻留内存。只有 session 已完成身份绑定且模拟器继续运行时，
/// 才在用户私有 runtime 目录留下 mode 0600 的恢复副本供下一进程接管。
pub struct GrpcJwtAuth {
    key_pair: EcdsaKeyPair,
    private_key_pkcs8: Vec<u8>,
    key_id: String,
    auth_dir: PathBuf,
    allowlist_path: PathBuf,
    installed_jwks: Mutex<Vec<PathBuf>>,
    recovery_lease: Mutex<Option<File>>,
    recoverable: AtomicBool,
    preserve_for_recovery: AtomicBool,
}

impl fmt::Debug for GrpcJwtAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GrpcJwtAuth")
            .field("key_id", &self.key_id)
            .field("auth_dir", &self.auth_dir)
            .finish_non_exhaustive()
    }
}

impl GrpcJwtAuth {
    pub fn new() -> anyhow::Result<Self> {
        Self::new_in(&runtime_root().join("liteavd/grpc-auth"))
    }

    fn new_in(parent: &Path) -> anyhow::Result<Self> {
        let allowlist = allowlist_json()?;
        let rng = SystemRandom::new();
        let mut random_id = [0_u8; 16];
        rng.fill(&mut random_id)
            .map_err(|_| anyhow!("生成 gRPC key id 失败"))?;
        let key_id = URL_SAFE_NO_PAD.encode(random_id);
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|_| anyhow!("生成 ES256 私钥失败"))?;
        let key_pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                .map_err(|_| anyhow!("加载 ES256 私钥失败"))?;
        let private_key_pkcs8 = pkcs8.as_ref().to_vec();

        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 gRPC auth 根目录失败：{}", parent.display()))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        cleanup_stale_session_dirs(parent);
        let auth_dir = parent.join(format!("{}-{key_id}", std::process::id()));
        std::fs::create_dir(&auth_dir)
            .with_context(|| format!("创建 gRPC session 目录失败：{}", auth_dir.display()))?;
        std::fs::set_permissions(&auth_dir, std::fs::Permissions::from_mode(0o700))?;
        let allowlist_path = auth_dir.join("allowlist.json");
        if let Err(error) = write_new_private(&allowlist_path, &allowlist) {
            let _ = std::fs::remove_dir_all(&auth_dir);
            return Err(error);
        }

        Ok(Self {
            key_pair,
            private_key_pkcs8,
            key_id,
            auth_dir,
            allowlist_path,
            installed_jwks: Mutex::new(Vec::new()),
            recovery_lease: Mutex::new(None),
            recoverable: AtomicBool::new(false),
            preserve_for_recovery: AtomicBool::new(false),
        })
    }

    pub fn allowlist_path(&self) -> &Path {
        &self.allowlist_path
    }

    pub(crate) fn session_runtime_dir(&self) -> &Path {
        &self.auth_dir
    }

    pub(crate) fn key_id(&self) -> &str {
        &self.key_id
    }

    /// 在模拟器身份已经验证后提交可恢复凭据。record 最后写入，只有完整记录
    /// 才会被下一 liteavd 进程识别。
    pub(crate) fn bind_recovery(&self, instance: &RunningInstance) -> anyhow::Result<()> {
        if self.recoverable.load(Ordering::Acquire) {
            return Ok(());
        }
        let lease = open_recovery_lease(&self.auth_dir)?;
        let private_path = self.auth_dir.join(RECOVERY_PRIVATE_KEY);
        write_new_private(&private_path, &self.private_key_pkcs8)?;
        let record_path = self.auth_dir.join(RECOVERY_RECORD);
        let record = serde_json::to_vec_pretty(&RecoveryRecord {
            version: RECOVERY_VERSION,
            engine_pid: instance.pid,
            avd_name: instance.avd_name.clone(),
            console_port: instance.console_port,
            grpc_port: instance.grpc_port,
            key_id: self.key_id.clone(),
        })?;
        if let Err(error) = write_new_private(&record_path, &record) {
            let _ = std::fs::remove_file(&private_path);
            return Err(error);
        }
        *self
            .recovery_lease
            .lock()
            .expect("recovery lease mutex poisoned") = Some(lease);
        self.recoverable.store(true, Ordering::Release);
        Ok(())
    }

    /// 恢复与广告文件完整身份一致且未被另一个 liteavd 进程持有的凭据。
    pub(crate) fn recover(instance: &RunningInstance) -> anyhow::Result<Option<Arc<Self>>> {
        Self::recover_in(&runtime_root().join("liteavd/grpc-auth"), instance)
    }

    fn recover_in(parent: &Path, instance: &RunningInstance) -> anyhow::Result<Option<Arc<Self>>> {
        let Ok(entries) = std::fs::read_dir(parent) else {
            return Ok(None);
        };
        let mut matches = Vec::new();
        for entry in entries.flatten() {
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if !metadata.is_dir() || entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
                continue;
            }
            let record_path = entry.path().join(RECOVERY_RECORD);
            let Ok(record) = read_recovery_record(&record_path) else {
                continue;
            };
            if recovery_matches(&record, instance) {
                matches.push((entry.path(), record));
            }
        }
        if matches.len() > 1 {
            bail!("同一模拟器存在多份 gRPC 恢复身份，拒绝猜测接管");
        }
        let Some((auth_dir, record)) = matches.pop() else {
            return Ok(None);
        };
        validate_private_dir(&auth_dir)?;
        let lease = open_recovery_lease(&auth_dir)?;
        let private_key_pkcs8 =
            read_private_file(&auth_dir.join(RECOVERY_PRIVATE_KEY), MAX_PRIVATE_KEY_BYTES)?;
        let rng = SystemRandom::new();
        let key_pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &private_key_pkcs8, &rng)
                .map_err(|_| anyhow!("恢复 ES256 私钥失败"))?;
        let jwks_dir = instance
            .grpc_jwks
            .as_deref()
            .context("恢复实例缺少 grpc.jwks")?;
        let active_jwk = instance
            .grpc_jwk_active
            .as_deref()
            .context("恢复实例缺少 grpc.jwk_active")?;
        let jwk_path = jwks_dir.join(format!("liteavd-{}.jwk", record.key_id));
        if !jwk_path.is_file()
            || !std::fs::read_to_string(active_jwk)
                .is_ok_and(|contents| contents.contains(&record.key_id))
        {
            bail!("模拟器没有激活待恢复的 liteavd JWK");
        }
        let allowlist_path = auth_dir.join("allowlist.json");
        if !allowlist_path.is_file() {
            bail!("待恢复身份缺少 allowlist");
        }
        Ok(Some(Arc::new(Self {
            key_pair,
            private_key_pkcs8,
            key_id: record.key_id,
            auth_dir,
            allowlist_path,
            installed_jwks: Mutex::new(vec![jwk_path]),
            recovery_lease: Mutex::new(Some(lease)),
            recoverable: AtomicBool::new(true),
            preserve_for_recovery: AtomicBool::new(false),
        })))
    }

    pub(crate) fn preserve_recovery_on_drop(&self) {
        if self.recoverable.load(Ordering::Acquire) {
            self.preserve_for_recovery.store(true, Ordering::Release);
        }
    }

    /// 将公钥交给模拟器，并等待 `grpc.jwk_active` 确认已加载。
    pub async fn install_public_jwk(
        &self,
        jwks_dir: &Path,
        active_jwk: &Path,
    ) -> anyhow::Result<()> {
        if !jwks_dir.is_dir() {
            bail!("模拟器 JWK 目录不存在：{}", jwks_dir.display());
        }
        let jwk_path = jwks_dir.join(format!("liteavd-{}.jwk", self.key_id));
        // Emulator 37.1.11 的目录 watcher 不响应 rename 到 `.jwk`；它会对
        // create_new 后尚为空的文件重试，并处理紧随其后的写入事件。
        write_new_private(&jwk_path, &self.public_jwk_json()?)?;
        self.installed_jwks
            .lock()
            .expect("installed_jwks mutex poisoned")
            .push(jwk_path.clone());

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if std::fs::read_to_string(active_jwk)
                .is_ok_and(|contents| contents.contains(&self.key_id))
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        bail!(
            "模拟器未在 5 秒内激活 liteavd JWK：{}",
            active_jwk.display()
        )
    }

    pub(crate) fn bearer_token(&self) -> anyhow::Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("系统时间早于 UNIX epoch")?
            .as_secs();
        let header = json!({"alg": "ES256", "kid": self.key_id});
        let claims = json!({
            "iss": GRPC_JWT_ISSUER,
            "aud": GRPC_ALLOWED_METHODS,
            "iat": now.saturating_sub(1),
            "exp": now + 60,
        });
        let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
        let claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
        let signing_input = format!("{header}.{claims}");
        let signature = self
            .key_pair
            .sign(&SystemRandom::new(), signing_input.as_bytes())
            .map_err(|_| anyhow!("签名 gRPC JWT 失败"))?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }

    fn public_jwk_json(&self) -> anyhow::Result<Vec<u8>> {
        let public_key = self.key_pair.public_key().as_ref();
        if public_key.len() != 65 || public_key[0] != 0x04 {
            bail!("ES256 公钥不是预期的未压缩 P-256 格式");
        }
        Ok(serde_json::to_vec_pretty(&json!({
            "keys": [{
                "kty": "EC",
                "crv": "P-256",
                "alg": "ES256",
                "use": "sig",
                "kid": self.key_id,
                "x": URL_SAFE_NO_PAD.encode(&public_key[1..33]),
                "y": URL_SAFE_NO_PAD.encode(&public_key[33..65]),
            }],
        }))?)
    }
}

/// auth 根目录是当前用户 0700；只清理名称可解析且 PID 已不存在的真目录。
/// PID 复用时宁可保留，不猜测删除。
fn cleanup_stale_session_dirs(parent: &Path) {
    let current_pid = std::process::id();
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some((pid, _)) = name.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let recovery_path = entry.path().join(RECOVERY_RECORD);
        if recovery_path.exists() {
            match read_recovery_record(&recovery_path) {
                Ok(record) if Path::new(&format!("/proc/{}", record.engine_pid)).exists() => {
                    continue;
                }
                Err(_) => continue,
                Ok(_) => {}
            }
        }
        if pid == current_pid || Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        crate::core::microphone::cleanup_stale_auth_dir(&entry.path());
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

impl Drop for GrpcJwtAuth {
    fn drop(&mut self) {
        if self.preserve_for_recovery.load(Ordering::Acquire)
            && self.recoverable.load(Ordering::Acquire)
        {
            return;
        }
        if let Ok(paths) = self.installed_jwks.lock() {
            for path in paths.iter() {
                let _ = std::fs::remove_file(path);
            }
        }
        let _ = std::fs::remove_dir_all(&self.auth_dir);
    }
}

fn recovery_matches(record: &RecoveryRecord, instance: &RunningInstance) -> bool {
    record.version == RECOVERY_VERSION
        && record.engine_pid == instance.pid
        && record.avd_name == instance.avd_name
        && record.console_port == instance.console_port
        && record.grpc_port == instance.grpc_port
        && !record.key_id.is_empty()
        && record
            .key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_private_dir(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!("gRPC 恢复目录权限或所有者不安全：{}", path.display());
    }
    Ok(())
}

fn open_recovery_lease(auth_dir: &Path) -> anyhow::Result<File> {
    validate_private_dir(auth_dir)?;
    let path = auth_dir.join(RECOVERY_LEASE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("打开 gRPC 恢复 lease 失败：{}", path.display()))?;
    if file.metadata()?.mode() & 0o077 != 0 {
        bail!("gRPC 恢复 lease 权限过宽：{}", path.display());
    }
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        bail!("gRPC 恢复身份正由另一个 liteavd 进程持有");
    }
    Ok(file)
}

fn read_recovery_record(path: &Path) -> anyhow::Result<RecoveryRecord> {
    let bytes = read_private_file(path, MAX_RECOVERY_RECORD_BYTES)?;
    let record: RecoveryRecord = serde_json::from_slice(&bytes)?;
    Ok(record)
}

fn read_private_file(path: &Path, max_bytes: u64) -> anyhow::Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("打开私有恢复文件失败：{}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > max_bytes
    {
        bail!("恢复文件类型、权限或大小无效：{}", path.display());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        bail!("恢复文件超过 {max_bytes}B：{}", path.display());
    }
    Ok(bytes)
}

fn runtime_root() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/user").join(unsafe { libc::getuid() }.to_string()))
}

fn allowlist_json() -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec_pretty(&json!({
        "unprotected": [],
        "allowlist": [{
            "iss": GRPC_JWT_ISSUER,
            "protected": GRPC_ALLOWED_METHODS,
        }],
    }))?)
}

fn write_new_private(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("创建认证文件失败：{}", path.display()))?;
    let result = file
        .write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("写入认证文件失败：{}", path.display()));
    if result.is_err() {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    #[test]
    fn creates_private_runtime_identity_and_valid_es256_token() {
        let root =
            std::env::temp_dir().join(format!("liteavd-grpc-auth-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let auth = GrpcJwtAuth::new_in(&root).unwrap();
        let mode = std::fs::metadata(&auth.auth_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);

        let token = auth.bearer_token().unwrap();
        let parts: Vec<_> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(
            &ECDSA_P256_SHA256_FIXED,
            auth.key_pair.public_key().as_ref(),
        )
        .verify(signing_input.as_bytes(), &signature)
        .unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], GRPC_JWT_ISSUER);
        assert!(
            GRPC_ALLOWED_METHODS
                .contains(&"/android.emulation.control.EmulatorController/streamAudio")
        );
        assert!(
            GRPC_ALLOWED_METHODS
                .contains(&"/android.emulation.control.EmulatorController/getMicrophoneState")
        );
        assert!(
            GRPC_ALLOWED_METHODS
                .contains(&"/android.emulation.control.EmulatorController/setMicrophoneState")
        );
        assert!(
            !GRPC_ALLOWED_METHODS
                .contains(&"/android.emulation.control.EmulatorController/injectAudio")
        );
        assert_eq!(
            claims["aud"].as_array().unwrap().len(),
            GRPC_ALLOWED_METHODS.len()
        );
        let auth_dir = auth.auth_dir.clone();
        drop(auth);
        assert!(!auth_dir.exists());
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn rejects_zero_grpc_port() {
        assert!(GrpcLaunchConfig::new(0).is_err());
    }

    #[test]
    fn new_identity_cleans_only_dead_pid_session_directories() {
        let root = std::env::temp_dir().join(format!(
            "liteavd-grpc-auth-stale-unit-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(format!("{}-live", std::process::id()))).unwrap();
        std::fs::create_dir_all(root.join(format!("{}-stale", u32::MAX))).unwrap();
        std::fs::create_dir_all(root.join("not-a-session")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            root.join(format!("{}-stale", u32::MAX)),
            root.join("1-symlink"),
        )
        .unwrap();

        let auth = GrpcJwtAuth::new_in(&root).unwrap();
        assert!(root.join(format!("{}-live", std::process::id())).is_dir());
        assert!(!root.join(format!("{}-stale", u32::MAX)).exists());
        assert!(root.join("not-a-session").is_dir());
        assert!(root.join("1-symlink").is_symlink());

        drop(auth);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recoverable_identity_is_private_exclusive_and_cleans_after_stop() {
        let root = std::env::temp_dir().join(format!(
            "liteavd-grpc-auth-recovery-unit-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let parent = root.join("auth");
        let jwks = root.join("jwks");
        std::fs::create_dir_all(&jwks).unwrap();
        let active = root.join("active");
        let auth = Arc::new(GrpcJwtAuth::new_in(&parent).unwrap());
        std::fs::write(&active, format!("active: {}", auth.key_id)).unwrap();
        auth.install_public_jwk(&jwks, &active).await.unwrap();
        let instance = RunningInstance {
            pid: std::process::id(),
            ini_path: root.join("pid.ini"),
            avd_name: "recovered".into(),
            console_port: 5554,
            adb_port: 5555,
            grpc_port: 8554,
            grpc_allowlist: None,
            grpc_jwks: Some(jwks),
            grpc_jwk_active: Some(active),
        };
        auth.bind_recovery(&instance).unwrap();
        let auth_dir = auth.auth_dir.clone();
        assert_eq!(
            std::fs::metadata(auth_dir.join(RECOVERY_PRIVATE_KEY))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        assert!(GrpcJwtAuth::recover_in(&parent, &instance).is_err());

        auth.preserve_recovery_on_drop();
        drop(auth);
        assert!(auth_dir.is_dir());
        let recovered = GrpcJwtAuth::recover_in(&parent, &instance)
            .unwrap()
            .unwrap();
        assert!(recovered.bearer_token().is_ok());
        assert!(GrpcJwtAuth::recover_in(&parent, &instance).is_err());
        drop(recovered);
        assert!(!auth_dir.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
