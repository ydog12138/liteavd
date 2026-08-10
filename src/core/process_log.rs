//! 模拟器 stdout/stderr 的有界文件日志。

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;

const LOG_FILE_LIMIT: u64 = 512 * 1024;
const LOG_CHANNEL_CAPACITY: usize = 64;
static NEXT_LOG_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLogFilter {
    All,
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLogDocument {
    pub text: String,
    pub source_bytes: u64,
    pub used_previous: bool,
}

/// 读取当前日志及最多一个轮转文件；两者各自仍受 512 KiB writer 上限约束。
pub fn read_session_log(
    current: &Path,
    filter: SessionLogFilter,
) -> anyhow::Result<SessionLogDocument> {
    let previous = current.with_extension("log.previous");
    let mut bytes = Vec::new();
    let mut source_bytes = 0_u64;
    let mut used_previous = false;
    if previous.is_file() {
        let part = read_bounded_file(&previous)?;
        source_bytes += part.len() as u64;
        bytes.extend_from_slice(&part);
        bytes.push(b'\n');
        used_previous = true;
    }
    let part = read_bounded_file(current)
        .with_context(|| format!("读取 session 日志失败：{}", current.display()))?;
    source_bytes += part.len() as u64;
    bytes.extend_from_slice(&part);
    let text = String::from_utf8_lossy(&bytes);
    Ok(SessionLogDocument {
        text: filter_log(&text, filter),
        source_bytes,
        used_previous,
    })
}

/// 以 no-clobber 方式导出当前过滤结果；目标已存在时拒绝覆盖。
pub fn export_session_log(
    current: &Path,
    destination: &Path,
    filter: SessionLogFilter,
) -> anyhow::Result<u64> {
    let document = read_session_log(current, filter)?;
    let parent = destination.parent().context("日志导出路径缺少父目录")?;
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("日志导出文件名不是 UTF-8")?;
    let sequence = NEXT_LOG_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{file_name}.liteavd-export-{}-{sequence}.part",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("创建日志导出临时文件失败：{}", temporary.display()))?;
        file.write_all(document.text.as_bytes())?;
        file.sync_all()?;
        std::fs::hard_link(&temporary, destination).with_context(|| {
            format!(
                "发布日志导出失败（目标可能已存在）：{}",
                destination.display()
            )
        })?;
        File::open(parent)?.sync_all()?;
        Ok(document.text.len() as u64)
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

fn read_bounded_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(LOG_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LOG_FILE_LIMIT {
        anyhow::bail!("session 日志超过单文件上限：{}", path.display());
    }
    Ok(bytes)
}

fn filter_log(text: &str, filter: SessionLogFilter) -> String {
    if filter == SessionLogFilter::All {
        return text.to_owned();
    }
    let mut active = None;
    let mut output = String::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with("[stdout] ") {
            active = Some(SessionLogFilter::Stdout);
        } else if line.starts_with("[stderr] ") {
            active = Some(SessionLogFilter::Stderr);
        }
        if active == Some(filter) {
            output.push_str(line);
        }
    }
    output
}

#[derive(Debug, Clone)]
pub(crate) struct LaunchLog {
    path: PathBuf,
}

impl LaunchLog {
    pub(crate) fn create(avd_name: &str, console_port: u16) -> anyhow::Result<Self> {
        let dir = log_directory();
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(&dir)
            .with_context(|| format!("创建模拟器日志目录失败：{}", dir.display()))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("设置模拟器日志目录权限失败：{}", dir.display()))?;

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let sequence = NEXT_LOG_ID.fetch_add(1, Ordering::Relaxed);
        let name = sanitize_name(avd_name);
        let path = dir.join(format!(
            "{name}-{console_port}-{timestamp}-{}-{sequence}.log",
            std::process::id()
        ));
        create_private_file(&path)?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn capture(&self, stdout: ChildStdout, stderr: ChildStderr) -> anyhow::Result<()> {
        let (tx, rx) = sync_channel(LOG_CHANNEL_CAPACITY);
        let path = self.path.clone();
        std::thread::Builder::new()
            .name("liteavd-log-writer".into())
            .spawn(move || write_log_stream(path, rx))
            .context("创建模拟器日志写线程失败")?;
        spawn_reader("stdout", stdout, tx.clone())?;
        spawn_reader("stderr", stderr, tx)?;
        Ok(())
    }
}

#[derive(Debug)]
struct LogChunk {
    stream: &'static str,
    bytes: Vec<u8>,
}

fn spawn_reader(
    stream: &'static str,
    mut reader: impl Read + Send + 'static,
    tx: SyncSender<LogChunk>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name(format!("liteavd-log-{stream}"))
        .spawn(move || {
            let mut buffer = vec![0_u8; 8 * 1024];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                if tx
                    .send(LogChunk {
                        stream,
                        bytes: buffer[..read].to_vec(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .with_context(|| format!("创建模拟器 {stream} 日志线程失败"))?;
    Ok(())
}

fn write_log_stream(path: PathBuf, rx: Receiver<LogChunk>) {
    let previous = path.with_extension("log.previous");
    for chunk in rx {
        if append_bounded(&path, &previous, chunk).is_err() {
            break;
        }
    }
}

fn append_bounded(path: &Path, previous: &Path, chunk: LogChunk) -> anyhow::Result<()> {
    let prefix = format!("[{}] ", chunk.stream);
    let max_payload = LOG_FILE_LIMIT.saturating_sub(prefix.len() as u64) as usize;
    let bytes = if chunk.bytes.len() > max_payload {
        &chunk.bytes[chunk.bytes.len() - max_payload..]
    } else {
        &chunk.bytes
    };
    let current_len = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let separator_len = u64::from(current_len > 0);
    let entry_len = separator_len + prefix.len() as u64 + bytes.len() as u64;
    if current_len > 0 && current_len.saturating_add(entry_len) > LOG_FILE_LIMIT {
        let _ = std::fs::remove_file(previous);
        std::fs::rename(path, previous).with_context(|| {
            format!(
                "轮转模拟器日志失败：{} -> {}",
                path.display(),
                previous.display()
            )
        })?;
        create_private_file(path)?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("打开模拟器日志失败：{}", path.display()))?;
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
        file.write_all(b"\n")?;
    }
    file.write_all(prefix.as_bytes())?;
    file.write_all(bytes)?;
    Ok(())
}

fn create_private_file(path: &Path) -> anyhow::Result<()> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("创建模拟器日志失败：{}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn log_directory() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("liteavd-cache-{}", unsafe { libc::getuid() }))
        })
        .join("liteavd/logs")
}

fn sanitize_name(name: &str) -> String {
    let mut sanitized: String = name
        .chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        sanitized.push_str("avd");
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_current_and_previous_files_bounded() {
        let dir = std::env::temp_dir().join(format!("liteavd-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("emulator.log");
        let previous = path.with_extension("log.previous");
        create_private_file(&path).unwrap();

        for _ in 0..140 {
            append_bounded(
                &path,
                &previous,
                LogChunk {
                    stream: "stderr",
                    bytes: vec![b'x'; 8 * 1024],
                },
            )
            .unwrap();
        }

        assert!(std::fs::metadata(&path).unwrap().len() <= LOG_FILE_LIMIT);
        assert!(std::fs::metadata(&previous).unwrap().len() <= LOG_FILE_LIMIT);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sanitizes_untrusted_avd_name() {
        assert_eq!(sanitize_name("../pixel 9/测试"), ".._pixel_9___");
        assert_eq!(sanitize_name(""), "avd");
    }

    #[test]
    fn reads_filters_and_exports_rotated_logs_without_clobber() {
        let dir = std::env::temp_dir().join(format!(
            "liteavd-log-view-{}-{}",
            std::process::id(),
            NEXT_LOG_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let current = dir.join("emulator.log");
        let previous = current.with_extension("log.previous");
        std::fs::write(
            &previous,
            b"[stdout] old output\ncontinuation\n[stderr] old error\n",
        )
        .unwrap();
        std::fs::write(
            &current,
            b"[stdout] new output\n[stderr] new error\ncontinued error\n",
        )
        .unwrap();

        let stdout = read_session_log(&current, SessionLogFilter::Stdout).unwrap();
        assert!(stdout.used_previous);
        assert!(stdout.text.contains("old output"));
        assert!(stdout.text.contains("continuation"));
        assert!(stdout.text.contains("new output"));
        assert!(!stdout.text.contains("old error"));
        assert!(!stdout.text.contains("new error"));

        let destination = dir.join("export.log");
        let bytes = export_session_log(&current, &destination, SessionLogFilter::Stderr).unwrap();
        assert_eq!(bytes, std::fs::metadata(&destination).unwrap().len());
        let exported = std::fs::read_to_string(&destination).unwrap();
        assert!(exported.contains("old error"));
        assert!(exported.contains("continued error"));
        assert!(!exported.contains("new output"));
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        std::fs::write(&destination, "keep").unwrap();
        assert!(export_session_log(&current, &destination, SessionLogFilter::All).is_err());
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "keep");
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .contains("liteavd-export")
        }));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
