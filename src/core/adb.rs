//! adb 最小封装：设备就绪与 boot_completed 判定、APK 安装。
//!
//! 5.6.2 审计 #9 修复：原 std::process::Command 同步阻塞且 wait-for-device
//! 无超时（会无限阻塞）；改为 tokio::process + 统一 deadline。

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, bail};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub const MAX_ADB_OUTPUT_BYTES: usize = 64 * 1024;
pub const DEFAULT_INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedOutput {
    bytes: Vec<u8>,
    total_bytes: u64,
}

impl BoundedOutput {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn truncated(&self) -> bool {
        self.total_bytes > self.bytes.len() as u64
    }

    pub fn summary(&self) -> String {
        let text = String::from_utf8_lossy(&self.bytes).trim().to_owned();
        if self.truncated() {
            format!("[输出已截断，总计 {}B]\n{text}", self.total_bytes)
        } else {
            text
        }
    }
}

#[derive(Debug)]
pub struct AdbCommandOutput {
    pub status: ExitStatus,
    pub stdout: BoundedOutput,
    pub stderr: BoundedOutput,
}

impl AdbCommandOutput {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    pub fn failure_summary(&self) -> String {
        if self.stderr.total_bytes() > 0 {
            self.stderr.summary()
        } else {
            self.stdout.summary()
        }
    }
}

#[derive(Debug, Error)]
pub enum AdbCommandError {
    #[error("adb 命令已取消")]
    Canceled,
    #[error("adb 命令超过 {0:?}")]
    Timeout(Duration),
    #[error("启动 adb 命令失败：{0}")]
    Spawn(#[source] std::io::Error),
    #[error("等待 adb 命令失败：{0}")]
    Wait(#[source] std::io::Error),
    #[error("读取 adb {stream} 失败：{message}")]
    Output {
        stream: &'static str,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApkInstallOptions {
    pub allow_downgrade: bool,
    pub grant_runtime_permissions: bool,
}

/// adb 可执行文件路径（SDK 内）。
pub fn adb_path(sdk_root: &Path) -> std::path::PathBuf {
    sdk_root.join("platform-tools/adb")
}

/// 轮询 `sys.boot_completed` 直到为 1（返回总共等待秒数）。
/// `timeout` 覆盖 wait-for-device 与 boot 轮询全流程，超时即报错。
pub async fn wait_for_boot(
    sdk_root: &Path,
    serial: &str,
    timeout: Duration,
) -> anyhow::Result<f64> {
    let adb = adb_path(sdk_root);
    let start = std::time::Instant::now();
    // 先 wait-for-device（设备进入 shell 可达）——带整体超时，不再无限阻塞
    let wait = tokio::time::timeout(
        timeout,
        Command::new(&adb)
            .arg("-s")
            .arg(serial)
            .arg("wait-for-device")
            .arg("shell")
            .arg("true")
            .status(),
    )
    .await
    .context("adb wait-for-device 超时")?
    .context("adb wait-for-device 失败")?;
    if !wait.success() {
        bail!("adb wait-for-device 返回非零");
    }
    loop {
        let out = tokio::time::timeout(
            timeout.saturating_sub(start.elapsed()),
            Command::new(&adb)
                .arg("-s")
                .arg(serial)
                .arg("shell")
                .arg("getprop sys.boot_completed")
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("等待 sys.boot_completed 超时（{}s）", timeout.as_secs()))?
        .ok();
        if let Some(out) = out {
            let text = String::from_utf8_lossy(&out.stdout);
            if text.trim() == "1" {
                return Ok(start.elapsed().as_secs_f64());
            }
        }
        if start.elapsed() > timeout {
            bail!("等待 sys.boot_completed 超时（{}s）", timeout.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// 运行一个 adb 子命令；stdout/stderr 各只保留最后 64KiB。
pub async fn run_cancellable<I, S, C>(
    sdk_root: &Path,
    serial: &str,
    args: I,
    timeout: Duration,
    mut canceled: C,
) -> Result<AdbCommandOutput, AdbCommandError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    C: FnMut() -> bool,
{
    let adb = adb_path(sdk_root);
    let args: Vec<OsString> = args
        .into_iter()
        .map(|argument| argument.as_ref().to_owned())
        .collect();
    let mut command = Command::new(&adb);
    command
        .arg("-s")
        .arg(serial)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut text_busy_retries = 0;
    let mut child = loop {
        if canceled() {
            return Err(AdbCommandError::Canceled);
        }
        match command.spawn() {
            Ok(child) => break child,
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && text_busy_retries < 10 => {
                text_busy_retries += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(AdbCommandError::Spawn(error)),
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let stdout_task = tokio::spawn(read_bounded_pipe(stdout));
    let stderr_task = tokio::spawn(read_bounded_pipe(stderr));
    let deadline = tokio::time::Instant::now() + timeout;

    let status = loop {
        if canceled() {
            terminate_and_reap(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(AdbCommandError::Canceled);
        }
        if tokio::time::Instant::now() >= deadline {
            terminate_and_reap(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(AdbCommandError::Timeout(timeout));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                terminate_and_reap(&mut child).await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(AdbCommandError::Wait(error));
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let stdout = join_output(stdout_task, "stdout").await?;
    let stderr = join_output(stderr_task, "stderr").await?;
    Ok(AdbCommandOutput {
        status,
        stdout,
        stderr,
    })
}

/// 安装 APK：`adb install -r -t <apk>`。默认 5 分钟超时。
pub async fn install_apk(sdk_root: &Path, serial: &str, apk: &Path) -> anyhow::Result<()> {
    install_apk_cancellable(sdk_root, serial, apk, || false).await
}

/// 可取消的 APK 安装。取消会 kill + wait adb 子进程，避免 session 换代后命令继续
/// 命中复用同一 console port 的新设备。
pub async fn install_apk_cancellable(
    sdk_root: &Path,
    serial: &str,
    apk: &Path,
    canceled: impl FnMut() -> bool,
) -> anyhow::Result<()> {
    let apk = apk.to_path_buf();
    install_apks_cancellable(
        sdk_root,
        serial,
        std::slice::from_ref(&apk),
        ApkInstallOptions::default(),
        canceled,
    )
    .await
    .map(|_| ())
}

pub async fn install_apks_cancellable(
    sdk_root: &Path,
    serial: &str,
    apks: &[PathBuf],
    options: ApkInstallOptions,
    canceled: impl FnMut() -> bool,
) -> anyhow::Result<AdbCommandOutput> {
    if apks.is_empty() {
        bail!("adb install 至少需要一个 APK");
    }
    let mut args = Vec::<OsString>::with_capacity(apks.len() + 5);
    args.push(if apks.len() == 1 {
        "install".into()
    } else {
        "install-multiple".into()
    });
    args.extend([OsString::from("-r"), OsString::from("-t")]);
    if options.allow_downgrade {
        args.push("-d".into());
    }
    if options.grant_runtime_permissions {
        args.push("-g".into());
    }
    args.extend(apks.iter().map(|path| path.as_os_str().to_owned()));
    let output = run_cancellable(sdk_root, serial, args, DEFAULT_INSTALL_TIMEOUT, canceled)
        .await
        .map_err(|error| anyhow::anyhow!("adb install 失败：{error}"))?;
    if !output.success() {
        bail!("adb install 失败：{}", output.failure_summary());
    }
    Ok(output)
}

async fn read_bounded_pipe(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
) -> std::io::Result<BoundedOutput> {
    let mut bytes = Vec::with_capacity(MAX_ADB_OUTPUT_BYTES);
    let mut total_bytes = 0_u64;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = pipe.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(read as u64);
        if read >= MAX_ADB_OUTPUT_BYTES {
            bytes.clear();
            bytes.extend_from_slice(&chunk[read - MAX_ADB_OUTPUT_BYTES..read]);
            continue;
        }
        let overflow = bytes
            .len()
            .saturating_add(read)
            .saturating_sub(MAX_ADB_OUTPUT_BYTES);
        if overflow > 0 {
            bytes.drain(..overflow);
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(BoundedOutput { bytes, total_bytes })
}

async fn join_output(
    task: tokio::task::JoinHandle<std::io::Result<BoundedOutput>>,
    stream: &'static str,
) -> Result<BoundedOutput, AdbCommandError> {
    match task.await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(AdbCommandError::Output {
            stream,
            message: error.to_string(),
        }),
        Err(error) => Err(AdbCommandError::Output {
            stream,
            message: error.to_string(),
        }),
    }
}

async fn terminate_and_reap(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn adb_path_resolves_under_sdk() {
        let p = adb_path(Path::new("/sdk"));
        assert_eq!(p, std::path::PathBuf::from("/sdk/platform-tools/adb"));
    }

    #[tokio::test]
    async fn wait_for_boot_fails_fast_when_adb_missing() {
        // adb 不存在 → 立即报错（不是超时挂死）
        let sdk = std::path::PathBuf::from("/nonexistent-sdk");
        let err = wait_for_boot(&sdk, "emulator-5554", Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("wait-for-device"),
            "错误应来自 wait-for-device：{err:#}"
        );
    }

    #[tokio::test]
    async fn canceled_install_kills_and_reaps_adb_process() {
        let root = std::env::temp_dir().join(format!("liteavd-adb-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tools = root.join("platform-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let pid_file = root.join("adb.pid");
        let script = tools.join("adb");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();
        let apk = root.join("test.apk");
        std::fs::write(&apk, b"fixture").unwrap();
        // 先确认脚本已进入，再触发取消；不能用固定轮询次数假定满载机器会在
        // 100ms 内调度到新进程。
        let pid_for_cancel = pid_file.clone();
        let error = install_apk_cancellable(&root, "emulator-5554", &apk, move || {
            pid_for_cancel.is_file()
        })
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("已取消"),
            "预期取消错误，实际为：{error:#}"
        );
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn install_error_output_is_bounded() {
        let root = std::env::temp_dir().join(format!("liteavd-adb-output-{}", std::process::id()));
        let tools = root.join("platform-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let script = tools.join("adb");
        std::fs::write(
            &script,
            "#!/bin/sh\ndd if=/dev/zero bs=131072 count=1 2>/dev/null | tr '\\0' x >&2\nexit 17\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();
        let apk = root.join("test.apk");
        std::fs::write(&apk, b"fixture").unwrap();

        let error = install_apk(&root, "emulator-5554", &apk).await.unwrap_err();
        assert!(
            error.to_string().len() <= 70 * 1024,
            "adb 错误输出未受 64KiB 级上限约束：{} bytes",
            error.to_string().len()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn runner_timeout_kills_and_reaps_child() {
        let root = std::env::temp_dir().join(format!("liteavd-adb-timeout-{}", std::process::id()));
        let tools = root.join("platform-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let pid_file = root.join("adb.pid");
        let script = tools.join("adb");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();

        let error = run_cancellable(
            &root,
            "emulator-5554",
            ["shell", "true"],
            Duration::from_millis(100),
            || false,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, AdbCommandError::Timeout(_)));
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn install_flags_and_split_arguments_are_explicit() {
        let root =
            std::env::temp_dir().join(format!("liteavd-adb-install-args-{}", std::process::id()));
        let tools = root.join("platform-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let args_file = root.join("args");
        let script = tools.join("adb");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                args_file.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();
        let base = root.join("base.apk");
        let split = root.join("split_config.apk");
        std::fs::write(&base, b"base").unwrap();
        std::fs::write(&split, b"split").unwrap();

        install_apks_cancellable(
            &root,
            "emulator-5554",
            std::slice::from_ref(&base),
            ApkInstallOptions {
                allow_downgrade: true,
                grant_runtime_permissions: true,
            },
            || false,
        )
        .await
        .unwrap();
        let single = std::fs::read_to_string(&args_file).unwrap();
        assert_eq!(
            single.lines().collect::<Vec<_>>(),
            vec![
                "-s",
                "emulator-5554",
                "install",
                "-r",
                "-t",
                "-d",
                "-g",
                base.to_str().unwrap(),
            ]
        );

        install_apks_cancellable(
            &root,
            "emulator-5554",
            &[base.clone(), split.clone()],
            ApkInstallOptions::default(),
            || false,
        )
        .await
        .unwrap();
        let multiple = std::fs::read_to_string(&args_file).unwrap();
        assert_eq!(
            multiple.lines().collect::<Vec<_>>(),
            vec![
                "-s",
                "emulator-5554",
                "install-multiple",
                "-r",
                "-t",
                base.to_str().unwrap(),
                split.to_str().unwrap(),
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
