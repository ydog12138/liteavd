//! 宿主与 Flatpak 的持久目录策略。

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

pub const APPLICATION_ID: &str = "io.github.ydog12138.liteavd";

/// 当前进程是否运行在 Flatpak 沙箱内。
pub fn is_flatpak() -> bool {
    std::env::var_os("FLATPAK_ID").is_some()
}

/// 判断显式 SDK 环境覆盖在当前运行边界内是否可持久使用。
///
/// 裸机保持原有环境变量语义。Flatpak 内允许尚未创建的私有 data 子目录；
/// 私有目录之外只接管已经存在且含 emulator 的 SDK，避免把宿主环境中的
/// 不可见绝对路径误建到沙箱临时根文件系统。
pub fn sdk_override_allowed(root: &Path) -> bool {
    sdk_override_allowed_for(
        root,
        std::env::var_os("FLATPAK_ID").as_deref(),
        std::env::var_os("XDG_DATA_HOME").as_deref(),
    )
}

fn sdk_override_allowed_for(
    root: &Path,
    flatpak_id: Option<&OsStr>,
    xdg_data_home: Option<&OsStr>,
) -> bool {
    if flatpak_id.is_none() {
        return true;
    }
    let private_data_child = xdg_data_home.is_some_and(|data| {
        !root
            .components()
            .any(|component| component == Component::ParentDir)
            && root.starts_with(Path::new(data))
    });
    private_data_child || root.join("emulator/emulator").is_file()
}

/// 未显式配置时的 Android SDK 根目录。
///
/// 裸机保持 Android 的常见 `~/Android/Sdk`；Flatpak 使用应用私有的
/// `$XDG_DATA_HOME/liteavd/android-sdk`，避免请求整个 home 权限。
pub fn default_sdk_root() -> PathBuf {
    default_sdk_root_for(
        std::env::var_os("FLATPAK_ID").as_deref(),
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// 未显式设置 `ANDROID_AVD_HOME` 时的 AVD 根目录。
pub fn default_avd_root() -> PathBuf {
    default_avd_root_for(
        std::env::var_os("FLATPAK_ID").as_deref(),
        std::env::var_os("XDG_DATA_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
    .expect("HOME 与 XDG_DATA_HOME 均未设置")
}

fn default_sdk_root_for(
    flatpak_id: Option<&OsStr>,
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> PathBuf {
    if flatpak_id.is_some()
        && let Some(data_root) = data_root(xdg_data_home, home)
    {
        return data_root.join("liteavd/android-sdk");
    }
    home.map(PathBuf::from)
        .map(|root| root.join("Android/Sdk"))
        .unwrap_or_default()
}

fn default_avd_root_for(
    flatpak_id: Option<&OsStr>,
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    if flatpak_id.is_some()
        && let Some(data_root) = data_root(xdg_data_home, home)
    {
        return Some(data_root.join("liteavd/avd"));
    }
    home.map(PathBuf::from)
        .map(|root| root.join(".android/avd"))
}

fn data_root(xdg_data_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    xdg_data_home
        .map(PathBuf::from)
        .or_else(|| home.map(Path::new).map(|root| root.join(".local/share")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_defaults_remain_android_conventions() {
        let home = OsStr::new("/home/tester");
        assert_eq!(
            default_sdk_root_for(None, Some(OsStr::new("/host/data")), Some(home)),
            PathBuf::from("/home/tester/Android/Sdk")
        );
        assert_eq!(
            default_avd_root_for(None, Some(OsStr::new("/host/data")), Some(home)),
            Some(PathBuf::from("/home/tester/.android/avd"))
        );
    }

    #[test]
    fn flatpak_defaults_use_private_persistent_data() {
        let app_id = OsStr::new(APPLICATION_ID);
        let data = OsStr::new("/home/tester/.var/app/app-id/data");
        let home = OsStr::new("/home/tester");
        assert_eq!(
            default_sdk_root_for(Some(app_id), Some(data), Some(home)),
            PathBuf::from("/home/tester/.var/app/app-id/data/liteavd/android-sdk")
        );
        assert_eq!(
            default_avd_root_for(Some(app_id), Some(data), Some(home)),
            Some(PathBuf::from(
                "/home/tester/.var/app/app-id/data/liteavd/avd"
            ))
        );
    }

    #[test]
    fn flatpak_data_falls_back_below_home() {
        assert_eq!(
            default_sdk_root_for(
                Some(OsStr::new(APPLICATION_ID)),
                None,
                Some(OsStr::new("/home/tester"))
            ),
            PathBuf::from("/home/tester/.local/share/liteavd/android-sdk")
        );
    }

    #[test]
    fn flatpak_rejects_invisible_host_override_but_accepts_persistent_roots() {
        let app_id = OsStr::new(APPLICATION_ID);
        let data = OsStr::new("/home/tester/.var/app/app-id/data");
        assert!(!sdk_override_allowed_for(
            Path::new("/data/Projects/invisible-sdk"),
            Some(app_id),
            Some(data)
        ));
        assert!(sdk_override_allowed_for(
            Path::new("/home/tester/.var/app/app-id/data/liteavd/new-sdk"),
            Some(app_id),
            Some(data)
        ));
        assert!(!sdk_override_allowed_for(
            Path::new("/home/tester/.var/app/app-id/data/../ephemeral-sdk"),
            Some(app_id),
            Some(data)
        ));

        let external = std::env::temp_dir().join(format!(
            "liteavd-flatpak-sdk-override-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(external.join("emulator")).unwrap();
        std::fs::write(external.join("emulator/emulator"), b"fixture").unwrap();
        assert!(sdk_override_allowed_for(
            &external,
            Some(app_id),
            Some(data)
        ));
        std::fs::remove_dir_all(external).unwrap();

        assert!(sdk_override_allowed_for(
            Path::new("/any/host/path"),
            None,
            Some(data)
        ));
    }
}
