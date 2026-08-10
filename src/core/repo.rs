//! Google 软件仓库 XML 解析（repository2-3.xml）。

use std::collections::HashMap;

use anyhow::Context;
use roxmltree::Document;

pub const REPOSITORY_BASE: &str = "https://dl.google.com/android/repository/";
pub const REPOSITORY_URL: &str = "https://dl.google.com/android/repository/repository2-3.xml";
pub const SYSTEM_IMAGE_CHANNELS: &[(&str, &str)] = &[
    (
        "google_apis",
        "https://dl.google.com/android/repository/sys-img/google_apis/sys-img2-3.xml",
    ),
    (
        "google_apis_playstore",
        "https://dl.google.com/android/repository/sys-img/google_apis_playstore/sys-img2-3.xml",
    ),
    (
        "aosp_atd",
        "https://dl.google.com/android/repository/sys-img/aosp_atd/sys-img2-3.xml",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Stable,
    Beta,
    Dev,
    Canary,
}

impl Channel {
    /// 稳定度排序：Stable=0 最稳，用于同 path 多条目择优。
    pub fn rank(self) -> u8 {
        match self {
            Channel::Stable => 0,
            Channel::Beta => 1,
            Channel::Dev => 2,
            Channel::Canary => 3,
        }
    }
}

/// 包校验和（emulator 等为 sha1，镜像等为 sha256）。
#[derive(Debug, Clone)]
pub enum Checksum {
    Sha1(String),
    Sha256(String),
}

impl Checksum {
    pub fn hex(&self) -> &str {
        match self {
            Checksum::Sha1(h) | Checksum::Sha256(h) => h,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Archive {
    /// 相对仓库根的 URL（如 `emulator-linux_x64-15982021.zip`）。
    pub url: String,
    pub size: u64,
    pub checksum: Option<Checksum>,
    /// emulator 等包带 host-os/host-arch 属性；platform-tools 等不带。
    pub host_os: Option<String>,
    pub host_arch: Option<String>,
}

impl Archive {
    /// 完整下载 URL。已绝对化的 URL 原样返回，否则拼在 base 之后。
    pub fn absolute_url(&self, base: &str) -> String {
        if self.url.starts_with("http://") || self.url.starts_with("https://") {
            self.url.clone()
        } else {
            format!("{}/{}", base.trim_end_matches('/'), self.url)
        }
    }
}

#[derive(Debug, Clone)]
pub struct Package {
    pub path: String,
    pub display_name: String,
    pub channel: Channel,
    pub revision: String,
    pub license_ids: Vec<String>,
    pub archives: Vec<Archive>,
}

#[derive(Debug)]
pub struct Repo {
    /// license id -> 协议全文。
    pub licenses: HashMap<String, String>,
    pub packages: Vec<Package>,
    /// 本仓库 XML 所在基 URL（archive.url 相对此解析为完整下载 URL）。
    pub base_url: String,
}

impl Default for Repo {
    fn default() -> Self {
        Repo {
            licenses: HashMap::new(),
            packages: Vec::new(),
            base_url: REPOSITORY_BASE.to_string(),
        }
    }
}

impl Repo {
    pub fn parse(xml: &str) -> anyhow::Result<Repo> {
        let doc = Document::parse(xml).context("invalid repository XML")?;
        let root = doc.root_element();

        let mut repo = Repo::default();
        for node in root.descendants().filter(|n| n.is_element()) {
            match node.tag_name().name() {
                "license" => {
                    if let (Some(id), Some(text)) = (node.attribute("id"), node.text()) {
                        repo.licenses
                            .insert(id.to_string(), text.trim().to_string());
                    }
                }
                "remotePackage" => {
                    if let Some(pkg) = parse_package(node) {
                        repo.packages.push(pkg);
                    }
                }
                _ => {}
            }
        }
        Ok(repo)
    }

    /// 合并另一个仓库的包与 license（用于多 tag 系统镜像仓库聚合）。
    pub fn merge(&mut self, other: Repo) {
        self.packages.extend(other.packages);
        self.licenses.extend(other.licenses);
    }

    /// 同 path 多 channel 条目（如 emulator 的 dev/stable）取最稳定的。
    pub fn package(&self, path: &str) -> Option<&Package> {
        self.packages
            .iter()
            .filter(|p| p.path == path)
            .min_by_key(|p| p.channel.rank())
    }

    /// 抓取远端组件仓库 XML（emulator/platform-tools 等）并解析。
    pub async fn fetch_components() -> anyhow::Result<Repo> {
        let url = "https://dl.google.com/android/repository/repository2-3.xml";
        let text = crate::core::download::Downloader::new()?
            .fetch_text(url)
            .await?;
        let mut repo = Repo::parse(&text)?;
        repo.base_url = REPOSITORY_BASE.to_string();
        Ok(repo)
    }

    /// 抓取远端系统镜像仓库 XML（sys-img）。系统镜像按 tag 分目录，
    /// 根路径 `sys-img2-3.xml` 不存在（实测 404），必须逐 channel 抓取后合并。
    pub async fn fetch_sys_images() -> anyhow::Result<Repo> {
        let dl = crate::core::download::Downloader::new()?;
        let mut out = Repo {
            base_url: format!("{}/sys-img/", REPOSITORY_BASE.trim_end_matches('/')),
            ..Repo::default()
        };
        let mut errors = Vec::new();
        for (_tag, url) in SYSTEM_IMAGE_CHANNELS {
            match dl.fetch_text(url).await {
                Ok(text) => match Repo::parse(&text) {
                    Ok(repo) => out.merge(repo),
                    Err(e) => errors.push(format!("{url}: {e:#}")),
                },
                Err(e) => errors.push(format!("{url}: {e:#}")),
            }
        }
        if out.packages.is_empty() {
            return Err(anyhow::anyhow!(
                "系统镜像仓库全部抓取失败：{}",
                errors.join("; ")
            ));
        }
        Ok(out)
    }

    /// 解析系统镜像 XML（path 形如 `system-images;android-35;google_apis;x86_64`）。
    pub fn system_images(&self, tag: &str) -> Vec<SystemImage> {
        let mut out: Vec<SystemImage> = Vec::new();
        for pkg in &self.packages {
            let mut parts = pkg.path.split(';');
            if parts.next() != Some("system-images") {
                continue;
            }
            let (Some(api), Some(pkg_tag), Some(abi)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if pkg_tag != tag {
                continue;
            }
            if let Some(archive) = pkg.archives.first() {
                out.push(SystemImage {
                    api: api.to_string(),
                    tag: tag.to_string(),
                    abi: abi.to_string(),
                    display_name: pkg.display_name.clone(),
                    license_ids: pkg.license_ids.clone(),
                    archive: archive.clone(),
                });
            }
        }
        out
    }

    /// 全部系统镜像（api 数字降序，新版本在前）。
    pub fn all_system_images(&self) -> Vec<SystemImage> {
        let mut out: Vec<SystemImage> = Vec::new();
        for pkg in &self.packages {
            let mut parts = pkg.path.split(';');
            if parts.next() != Some("system-images") {
                continue;
            }
            let (Some(api), Some(tag), Some(abi)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if let Some(archive) = pkg.archives.first() {
                out.push(SystemImage {
                    api: api.to_string(),
                    tag: tag.to_string(),
                    abi: abi.to_string(),
                    display_name: pkg.display_name.clone(),
                    license_ids: pkg.license_ids.clone(),
                    archive: archive.clone(),
                });
            }
        }
        out.sort_by_key(|s| s.api_number().parse::<u32>().unwrap_or(0));
        out.reverse();
        out
    }
}

/// 系统镜像（路径 `system-images;android-35;google_apis;x86_64`）。
#[derive(Debug, Clone)]
pub struct SystemImage {
    /// `android-35`
    pub api: String,
    /// `google_apis` / `google_apis_playstore` / `aosp_atd`
    pub tag: String,
    /// `x86_64` / `x86` / `arm64-v8a` / `armeabi-v7a`
    pub abi: String,
    pub display_name: String,
    pub license_ids: Vec<String>,
    pub archive: Archive,
}

impl SystemImage {
    /// `android-35` → `35`。
    pub fn api_number(&self) -> &str {
        self.api.strip_prefix("android-").unwrap_or(&self.api)
    }

    /// 完整下载 URL（archive.url 相对 `sys-img/<tag>/`）。
    pub fn download_url(&self) -> String {
        format!(
            "https://dl.google.com/android/repository/sys-img/{}/{}",
            self.tag, self.archive.url
        )
    }
}

fn parse_package(node: roxmltree::Node) -> Option<Package> {
    let path = node.attribute("path")?.to_string();
    let mut display_name = String::new();
    let mut revision = String::new();
    let mut channel = Channel::Stable;
    let mut license_ids = Vec::new();
    let mut archives = Vec::new();

    for child in node.children().filter(|n| n.is_element()) {
        match child.tag_name().name() {
            "display-name" => display_name = child.text().unwrap_or("").trim().to_string(),
            "revision" => {
                if let Some(major) = child
                    .children()
                    .find(|n| n.is_element() && n.tag_name().name() == "major")
                    .and_then(|n| n.text())
                {
                    revision = major.trim().to_string();
                    if let Some(minor) = child
                        .children()
                        .find(|n| n.is_element() && n.tag_name().name() == "minor")
                        .and_then(|n| n.text())
                    {
                        revision.push('.');
                        revision.push_str(minor.trim());
                    }
                }
            }
            "channelRef" => {
                channel = match child.attribute("ref") {
                    Some("channel-1") => Channel::Beta,
                    Some("channel-2") => Channel::Dev,
                    Some("channel-3") => Channel::Canary,
                    _ => Channel::Stable,
                };
            }
            "uses-license" => {
                if let Some(ref_id) = child.attribute("ref") {
                    license_ids.push(ref_id.to_string());
                }
            }
            "archives" => {
                for a in child
                    .children()
                    .filter(|n| n.is_element() && n.tag_name().name() == "archive")
                {
                    if let Some(archive) = parse_archive(a) {
                        archives.push(archive);
                    }
                }
            }
            _ => {}
        }
    }

    Some(Package {
        path,
        display_name,
        channel,
        revision,
        license_ids,
        archives,
    })
}

fn parse_archive(node: roxmltree::Node) -> Option<Archive> {
    let mut url = None;
    let mut size = None;
    let mut checksum = None;
    for child in node.children().filter(|n| n.is_element()) {
        if child.tag_name().name() == "complete" {
            for g in child.children().filter(|n| n.is_element()) {
                match g.tag_name().name() {
                    "url" => url = g.text().map(str::trim),
                    "size" => size = g.text().and_then(|s| s.trim().parse().ok()),
                    "checksum" => {
                        let hex = g.text().map(str::trim).filter(|s| !s.is_empty());
                        checksum = match (g.attribute("type"), hex) {
                            (Some("sha1"), Some(h)) => Some(Checksum::Sha1(h.to_string())),
                            (Some("sha256"), Some(h)) => Some(Checksum::Sha256(h.to_string())),
                            (_, Some(h)) => Some(Checksum::Sha256(h.to_string())),
                            _ => None,
                        };
                    }
                    _ => {}
                }
            }
        }
    }
    Some(Archive {
        url: url?.to_string(),
        size: size.unwrap_or(0),
        checksum,
        host_os: node.attribute("host-os").map(str::to_string),
        host_arch: node.attribute("host-arch").map(str::to_string),
    })
}

/// 本机平台，用于从多平台 archive 中选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPlatform {
    LinuxX64,
    LinuxAarch64,
    DarwinX64,
    DarwinAarch64,
    WindowsX64,
}

impl HostPlatform {
    pub fn current() -> HostPlatform {
        if cfg!(target_os = "windows") {
            HostPlatform::WindowsX64
        } else if cfg!(target_os = "macos") {
            if cfg!(target_arch = "aarch64") {
                HostPlatform::DarwinAarch64
            } else {
                HostPlatform::DarwinX64
            }
        } else if cfg!(target_arch = "aarch64") {
            HostPlatform::LinuxAarch64
        } else {
            HostPlatform::LinuxX64
        }
    }

    fn os_arch(&self) -> (&'static str, &'static str) {
        match self {
            HostPlatform::LinuxX64 => ("linux", "x64"),
            HostPlatform::LinuxAarch64 => ("linux", "aarch64"),
            HostPlatform::DarwinX64 => ("macosx", "x64"),
            HostPlatform::DarwinAarch64 => ("macosx", "aarch64"),
            HostPlatform::WindowsX64 => ("windows", "x64"),
        }
    }

    /// 优先用 host-os/host-arch 属性；无属性的（如 platform-tools）回退 URL 模式匹配。
    pub fn matches_archive(&self, a: &Archive) -> bool {
        match (&a.host_os, &a.host_arch) {
            (Some(os), Some(arch)) => {
                let (self_os, self_arch) = self.os_arch();
                os == self_os && arch == self_arch
            }
            _ => {
                let url = &a.url;
                match self {
                    HostPlatform::LinuxX64 => url.contains("linux") && !url.contains("aarch64"),
                    HostPlatform::LinuxAarch64 => url.contains("linux") && url.contains("aarch64"),
                    HostPlatform::DarwinX64 => url.contains("darwin") && !url.contains("aarch64"),
                    HostPlatform::DarwinAarch64 => {
                        url.contains("darwin") && url.contains("aarch64")
                    }
                    HostPlatform::WindowsX64 => url.contains("win"),
                }
            }
        }
    }
}

impl Package {
    /// 本平台的最新 archive（XML 按新版本在前排列，取第一个匹配）。
    /// 系统镜像等跨平台包的 archive 无 host-os/host-arch 属性且 URL 无平台线索，
    /// 此时回退取第一个。
    pub fn best_archive(&self, platform: HostPlatform) -> Option<&Archive> {
        let attr_match = self.archives.iter().find(|a| {
            matches!((&a.host_os, &a.host_arch), (Some(_), Some(_))) && platform.matches_archive(a)
        });
        if attr_match.is_some() {
            return attr_match;
        }
        self.archives
            .iter()
            .find(|a| platform.matches_archive(a))
            .or_else(|| self.archives.first())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Repo {
        let xml = include_str!("../../tests/fixtures/repository2-3-sample.xml");
        Repo::parse(xml).expect("fixture 解析失败")
    }

    #[test]
    fn parses_emulator_package() {
        let repo = fixture();
        let pkg = repo.package("emulator").expect("emulator 包缺失");
        assert_eq!(pkg.display_name, "Android Emulator");
        assert_eq!(pkg.channel, Channel::Stable);
        assert!(pkg.revision.starts_with("37"));
        assert!(pkg.license_ids.contains(&"android-sdk-license".to_string()));
    }

    #[test]
    fn selects_linux_archive() {
        let repo = fixture();
        let pkg = repo.package("emulator").unwrap();
        let a = pkg
            .best_archive(HostPlatform::LinuxX64)
            .expect("无 linux 包");
        assert!(a.url.starts_with("emulator-linux_x64-"));
        assert!(a.size > 0);
        assert!(matches!(a.checksum, Some(Checksum::Sha1(_))));
    }

    #[test]
    fn platform_tools_linux_zip() {
        let repo = fixture();
        let pkg = repo.package("platform-tools").unwrap();
        let a = pkg.best_archive(HostPlatform::LinuxX64).unwrap();
        assert!(a.url.ends_with("-linux.zip"));
    }

    #[test]
    fn best_archive_falls_back_without_host_attrs() {
        let xml = r#"<sdk-repository>
  <remotePackage path="sys-img-extra">
    <display-name>Extra</display-name>
    <revision>1</revision>
    <archives>
      <archive>
        <complete>
          <size>11</size>
          <url>extra-r01.zip</url>
        </complete>
      </archive>
    </archives>
  </remotePackage>
</sdk-repository>"#;
        let repo = Repo::parse(xml).unwrap();
        let pkg = repo.package("sys-img-extra").unwrap();
        assert_eq!(
            pkg.archives.len(),
            1,
            "archives 应解析出 1 个，实际 {}",
            pkg.archives.len()
        );
        let a = pkg
            .best_archive(HostPlatform::LinuxX64)
            .expect("跨平台包应回退取第一个");
        assert_eq!(a.url, "extra-r01.zip");
    }

    #[test]
    fn parses_licenses() {
        let repo = fixture();
        let text = repo
            .licenses
            .get("android-sdk-license")
            .expect("license 缺失");
        assert!(text.contains("Android Software Development Kit License Agreement"));
    }

    #[test]
    fn unknown_package_is_none() {
        let repo = fixture();
        assert!(repo.package("does-not-exist").is_none());
    }

    #[test]
    fn absolute_url_prepends_base() {
        let a = Archive {
            url: "emulator-linux_x64-15982021.zip".into(),
            size: 0,
            checksum: None,
            host_os: None,
            host_arch: None,
        };
        assert_eq!(
            a.absolute_url("https://dl.google.com/android/repository/"),
            "https://dl.google.com/android/repository/emulator-linux_x64-15982021.zip"
        );
        assert_eq!(
            a.absolute_url("https://dl.google.com/android/repository"),
            "https://dl.google.com/android/repository/emulator-linux_x64-15982021.zip"
        );
    }

    #[test]
    fn absolute_url_passes_through_full_urls() {
        let a = Archive {
            url: "https://example.com/other.zip".into(),
            size: 0,
            checksum: None,
            host_os: None,
            host_arch: None,
        };
        assert_eq!(a.absolute_url("https://example.com/"), a.url);
    }

    #[test]
    fn merge_combines_packages_and_licenses() {
        let mut a = Repo::parse(r#"<sdk-repository><license id="l1"><![CDATA[t1]]></license><remotePackage path="p1"><display-name>P1</display-name><revision>1</revision><archives><archive><complete><size>1</size><url>a.zip</url></complete></archive></archives></remotePackage></sdk-repository>"#).unwrap();
        let b = Repo::parse(r#"<sdk-repository><license id="l2"><![CDATA[t2]]></license><remotePackage path="p2"><display-name>P2</display-name><revision>1</revision><archives><archive><complete><size>1</size><url>b.zip</url></complete></archive></archives></remotePackage></sdk-repository>"#).unwrap();
        a.merge(b);
        assert_eq!(a.packages.len(), 2);
        assert!(a.licenses.contains_key("l1"));
        assert!(a.licenses.contains_key("l2"));
    }

    fn sysimg_fixture() -> Repo {
        let xml = include_str!("../../tests/fixtures/sys-img-sample.xml");
        Repo::parse(xml).expect("fixture 解析失败")
    }

    #[test]
    fn parses_system_image() {
        let repo = sysimg_fixture();
        let images = repo.system_images("google_apis");
        let img = images
            .iter()
            .find(|i| i.api == "android-35" && i.abi == "x86_64")
            .expect("android-35 x86_64 缺失");
        assert_eq!(img.api_number(), "35");
        assert_eq!(img.archive.url, "x86_64-35_r09.zip");
        assert!(img.archive.size > 1_000_000_000);
        assert!(matches!(img.archive.checksum, Some(Checksum::Sha1(_))));
        assert!(img.license_ids.contains(&"android-sdk-license".to_string()));
        assert_eq!(
            img.download_url(),
            "https://dl.google.com/android/repository/sys-img/google_apis/x86_64-35_r09.zip"
        );
    }

    #[test]
    fn filters_system_image_by_tag() {
        let repo = sysimg_fixture();
        let others = repo.system_images("aosp_atd");
        assert!(others.is_empty());
    }

    #[test]
    fn lists_all_abis() {
        let repo = sysimg_fixture();
        let images = repo.system_images("google_apis");
        let abis: Vec<_> = images.iter().map(|i| i.abi.as_str()).collect();
        assert!(abis.contains(&"arm64-v8a"));
        assert!(abis.contains(&"x86_64"));
    }
}
