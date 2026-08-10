//! 应用重启所需的最小持久状态。
//!
//! 这里只保存稳定的 AVD 名称意图；session id、generation、PID 和端口必须在
//! 新进程中重新扫描并验证，绝不能从磁盘恢复为运行事实。

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::core::workspace::WorkspaceIntent;

const WORKSPACE_VERSION: u32 = 1;
const MAX_STATE_BYTES: u64 = 64 * 1024;
const MAX_AVD_NAMES: usize = 256;
const MAX_AVD_NAME_BYTES: usize = 255;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredWorkspace {
    version: u32,
    focused_avd: Option<String>,
    selected_avds: Vec<String>,
}

pub fn workspace_state_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("liteavd/workspace.json")
}

pub fn load_workspace(path: &Path) -> anyhow::Result<WorkspaceIntent> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspaceIntent::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("打开工作区状态失败：{}", path.display()));
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("读取工作区状态元数据失败：{}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_STATE_BYTES {
        bail!("工作区状态不是常规小文件：{}", path.display());
    }
    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut contents)
        .with_context(|| format!("读取工作区状态失败：{}", path.display()))?;
    if contents.len() as u64 > MAX_STATE_BYTES {
        bail!("工作区状态超过 {MAX_STATE_BYTES}B");
    }
    let stored: StoredWorkspace = serde_json::from_slice(&contents)
        .with_context(|| format!("解析工作区状态失败：{}", path.display()))?;
    if stored.version != WORKSPACE_VERSION {
        bail!("不支持的工作区状态版本：{}", stored.version);
    }
    normalize(WorkspaceIntent {
        focused_avd: stored.focused_avd,
        selected_avds: stored.selected_avds,
    })
}

pub fn save_workspace(path: &Path, intent: &WorkspaceIntent) -> anyhow::Result<()> {
    let intent = normalize(intent.clone())?;
    let contents = serde_json::to_vec_pretty(&StoredWorkspace {
        version: WORKSPACE_VERSION,
        focused_avd: intent.focused_avd,
        selected_avds: intent.selected_avds,
    })?;
    if contents.len() as u64 > MAX_STATE_BYTES {
        bail!("工作区状态超过 {MAX_STATE_BYTES}B");
    }
    let parent = path.parent().context("工作区状态路径缺少父目录")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("创建工作区状态目录失败：{}", parent.display()))?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        bail!("拒绝覆盖非常规工作区状态文件：{}", path.display());
    }

    let temp_id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("工作区状态文件名不是 UTF-8")?;
    let temp = parent.join(format!(".{file_name}.{}.{temp_id}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("创建工作区临时状态失败：{}", temp.display()))?;
    let result = (|| -> anyhow::Result<()> {
        file.write_all(&contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.with_context(|| format!("原子保存工作区状态失败：{}", path.display()))
}

fn normalize(mut intent: WorkspaceIntent) -> anyhow::Result<WorkspaceIntent> {
    if intent.selected_avds.len() > MAX_AVD_NAMES {
        bail!("工作区选择设备超过 {MAX_AVD_NAMES} 台");
    }
    if let Some(name) = intent.focused_avd.as_deref() {
        validate_name(name)?;
    }
    for name in &intent.selected_avds {
        validate_name(name)?;
    }
    intent.selected_avds.sort();
    intent.selected_avds.dedup();
    Ok(intent)
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > MAX_AVD_NAME_BYTES || name.contains('\0') {
        bail!("无效的持久化 AVD 名称");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, symlink};

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "liteavd-recovery-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn workspace_roundtrip_is_atomic_private_and_normalized() {
        let root = temp_root("roundtrip");
        let path = root.join("state/workspace.json");
        save_workspace(
            &path,
            &WorkspaceIntent {
                focused_avd: Some("phone".into()),
                selected_avds: vec!["tablet".into(), "phone".into(), "phone".into()],
            },
        )
        .unwrap();
        assert_eq!(
            load_workspace(&path).unwrap(),
            WorkspaceIntent {
                focused_avd: Some("phone".into()),
                selected_avds: vec!["phone".into(), "tablet".into()],
            }
        );
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_update_and_symlink_never_replace_existing_state() {
        let root = temp_root("reject");
        let path = root.join("state/workspace.json");
        save_workspace(&path, &WorkspaceIntent::default()).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            save_workspace(
                &path,
                &WorkspaceIntent {
                    focused_avd: Some("x".repeat(MAX_AVD_NAME_BYTES + 1)),
                    selected_avds: vec![],
                }
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let target = root.join("target");
        std::fs::write(&target, b"untouched").unwrap();
        let link = root.join("state/link.json");
        symlink(&target, &link).unwrap();
        assert!(save_workspace(&link, &WorkspaceIntent::default()).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn load_rejects_unknown_version_and_oversized_file() {
        let root = temp_root("load");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("workspace.json");
        std::fs::write(
            &path,
            br#"{"version":2,"focused_avd":null,"selected_avds":[]}"#,
        )
        .unwrap();
        assert!(load_workspace(&path).is_err());
        std::fs::write(&path, vec![b'x'; MAX_STATE_BYTES as usize + 1]).unwrap();
        assert!(load_workspace(&path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
