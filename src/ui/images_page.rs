//! 镜像管理页：本机已装组件 + 在线仓库浏览/下载/卸载。

use std::future::Future;
use std::path::{Path, PathBuf};

use gtk4::prelude::*;
use gtk4::{Button, Label, ListBox, ListBoxRow, ProgressBar, ScrolledWindow, Window};
use libadwaita::prelude::*;

use crate::core::install::{self, ComponentKind};
use crate::core::package_service::{
    InstallRequest, PackageError, PackageEvent, PackageLicense, PackageOperation, PackageService,
};
use crate::core::repo::{Archive, HostPlatform, Repo};

/// 提交到 UI 共用的长存 Tokio executor。
fn spawn_async<F>(f: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    crate::ui::background::spawn(f);
}

/// 回主线程执行 UI 操作。
/// 注意：thread_local 队列方案在跨线程场景失效（任务堆积在 worker 线程的 TLS，
/// 主线程 drain 自己的空队列）——审计 #3 已核验并修复为直接 invoke。
/// 闭包只捕获 Send 数据（SendWeakRef/String/Result/mpsc::Sender）。
fn post_ui(f: impl FnOnce() + Send + 'static) {
    glib::MainContext::default().invoke(f);
}

/// 单行数据。
#[derive(Clone)]
struct RowSpec {
    kind: ComponentKind,
    title: String,
    subtitle: String,
    size: u64,
    archive: Option<Archive>,
    /// 完整下载 URL（仓库层算好，调用方不再拼接相对路径）。
    download_url: String,
    /// 许可 ID 与可选全文；仓库引用了缺失文本的 ID 时必须中止。
    licenses: Vec<PackageLicense>,
    installed: bool,
}

fn installed_rows(sdk: &Path) -> Vec<RowSpec> {
    let mut rows = Vec::new();
    for kind in [ComponentKind::Emulator, ComponentKind::PlatformTools] {
        let dir = install::component_dir(sdk, &kind);
        if dir.exists() {
            rows.push(RowSpec {
                title: kind.display_name().to_string(),
                kind,
                subtitle: "已安装".into(),
                size: 0,
                archive: None,
                download_url: String::new(),
                licenses: Vec::new(),
                installed: true,
            });
        }
    }
    for image in crate::core::avd::scan_installed_images(sdk) {
        rows.push(RowSpec {
            kind: ComponentKind::SystemImage {
                api: image.api.clone(),
                tag: image.tag.clone(),
                abi: image.abi.clone(),
            },
            title: format!("{}/{} ({})", image.api, image.tag, image.abi),
            subtitle: "已安装".into(),
            size: 0,
            archive: None,
            download_url: String::new(),
            licenses: Vec::new(),
            installed: true,
        });
    }
    rows
}

fn online_rows(sdk: &Path, repo: &Repo, sys_repo: &Repo) -> Vec<RowSpec> {
    let platform = HostPlatform::current();
    let mut rows = Vec::new();
    for path in ["emulator", "platform-tools"] {
        let Some(pkg) = repo.package(path) else {
            continue;
        };
        let Some(archive) = pkg.best_archive(platform) else {
            continue;
        };
        let kind = if path == "emulator" {
            ComponentKind::Emulator
        } else {
            ComponentKind::PlatformTools
        };
        rows.push(RowSpec {
            installed: install::component_dir(sdk, &kind).exists(),
            title: pkg.display_name.clone(),
            subtitle: format!("rev {}", pkg.revision),
            size: archive.size,
            archive: Some(archive.clone()),
            download_url: archive.absolute_url(&repo.base_url),
            licenses: pkg
                .license_ids
                .iter()
                .map(|id| PackageLicense::new(id.clone(), repo.licenses.get(id).cloned()))
                .collect(),
            kind,
        });
    }
    for img in sys_repo.all_system_images() {
        let kind = ComponentKind::SystemImage {
            api: img.api.clone(),
            tag: img.tag.clone(),
            abi: img.abi.clone(),
        };
        rows.push(RowSpec {
            installed: install::component_dir(sdk, &kind).exists(),
            title: format!("Android {} {} {}", img.api, img.tag, img.abi),
            subtitle: img.display_name.clone(),
            size: img.archive.size,
            archive: Some(img.archive.clone()),
            download_url: img.download_url(),
            licenses: img
                .license_ids
                .iter()
                .map(|id| PackageLicense::new(id.clone(), sys_repo.licenses.get(id).cloned()))
                .collect(),
            kind,
        });
    }
    rows
}

fn fmt_size(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
    } else {
        format!("{bytes} B")
    }
}

#[derive(Clone)]
struct PageRefresh {
    sdk: PathBuf,
    window: glib::SendWeakRef<Window>,
    installed_list: glib::SendWeakRef<ListBox>,
    online_list: glib::SendWeakRef<ListBox>,
    online_specs: std::sync::Arc<std::sync::Mutex<Vec<RowSpec>>>,
    external_changed: std::sync::Arc<dyn Fn() + Send + Sync>,
}

impl PageRefresh {
    fn schedule_render(&self) {
        let refresh = self.clone();
        post_ui(move || refresh.render_now());
    }

    fn component_changed(&self) {
        (self.external_changed)();
        self.schedule_render();
    }

    fn replace_online(&self, specs: Vec<RowSpec>) {
        *self
            .online_specs
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = specs;
        self.schedule_render();
    }

    fn render_now(&self) {
        let (Some(window), Some(installed_list), Some(online_list)) = (
            self.window.upgrade(),
            self.installed_list.upgrade(),
            self.online_list.upgrade(),
        ) else {
            return;
        };
        while let Some(row) = installed_list.row_at_index(0) {
            installed_list.remove(&row);
        }
        for spec in installed_rows(&self.sdk) {
            installed_list.append(&build_row(&spec, &window, self.clone()));
        }

        while let Some(row) = online_list.row_at_index(0) {
            online_list.remove(&row);
        }
        let mut online = self
            .online_specs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        for spec in &mut online {
            spec.installed = install::component_dir(&self.sdk, &spec.kind).exists();
            online_list.append(&build_row(spec, &window, self.clone()));
        }
    }
}

/// 渲染一个列表行；操作按钮回调用后台线程，进度经 invoke 回到主线程。
fn build_row(spec: &RowSpec, win: &Window, page_refresh: PageRefresh) -> ListBoxRow {
    let sdk = page_refresh.sdk.clone();
    let row = ListBoxRow::new();
    let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    box_.set_margin_top(6);
    box_.set_margin_bottom(6);
    box_.set_margin_start(10);
    box_.set_margin_end(10);

    let text = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    let title = Label::new(Some(&spec.title));
    title.set_xalign(0.0);
    title.set_wrap(true);
    text.append(&title);
    let mut sub = spec.subtitle.clone();
    if spec.size > 0 {
        sub.push_str(&format!(" · {}", fmt_size(spec.size)));
    }
    let sub = Label::new(Some(&sub));
    sub.set_xalign(0.0);
    sub.add_css_class("dim-label");
    text.append(&sub);
    box_.append(&text);

    let progress = ProgressBar::new();
    progress.set_show_text(false);
    progress.set_visible(false);

    let action = Button::new();
    if spec.installed {
        action.set_label("卸载");
        action.add_css_class("destructive-action");
        let sdk = sdk.clone();
        let kind = spec.kind.clone();
        let refresh = page_refresh.clone();
        let action_for_response = action.clone();
        let win_for_dialog = win.clone();
        action.connect_clicked(move |_| {
            let dlg = libadwaita::AlertDialog::builder()
                .heading("卸载组件")
                .body(format!(
                    "确定删除 {} 吗？此操作不可撤销。",
                    install::component_dir(&sdk, &kind).display()
                ))
                .build();
            dlg.add_response("cancel", "取消");
            dlg.add_response("uninstall", "卸载");
            dlg.set_default_response(Some("cancel"));
            dlg.set_close_response("cancel");
            let sdk2 = sdk.clone();
            let kind2 = kind.clone();
            let refresh2 = refresh.clone();
            let action2 = action_for_response.clone();
            dlg.connect_response(None, move |_dlg, resp| {
                if resp == "uninstall" {
                    let sdk = sdk2.clone();
                    let kind = kind2.clone();
                    let refresh = refresh2.clone();
                    action2.set_sensitive(false);
                    action2.set_label("卸载中…");
                    let action = glib::SendWeakRef::from(action2.clone().downgrade());
                    spawn_async(async move {
                        let result = match PackageService::new() {
                            Ok(service) => {
                                service
                                    .execute(
                                        PackageOperation::Uninstall {
                                            sdk_root: sdk,
                                            kind,
                                        },
                                        |_| {},
                                    )
                                    .await
                            }
                            Err(error) => Err(error),
                        };
                        match result {
                            Ok(_) => refresh.component_changed(),
                            Err(error) => post_ui(move || {
                                if let Some(action) = action.upgrade() {
                                    action.set_label("重试卸载");
                                    action.set_sensitive(true);
                                    action.set_tooltip_text(Some(&error.to_string()));
                                }
                            }),
                        }
                    });
                }
            });
            dlg.present(Some(&win_for_dialog));
        });
    } else if let Some(archive) = spec.archive.clone() {
        action.set_label("下载");
        action.add_css_class("suggested-action");
        let sdk = sdk.clone();
        let kind = spec.kind.clone();
        let refresh = page_refresh.clone();
        let prog = progress.clone();
        let act = action.clone();
        let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let win_for_license = glib::SendWeakRef::from(win.clone().downgrade());
        let action_w = glib::SendWeakRef::from(action.clone().downgrade());
        let act_w = glib::SendWeakRef::from(act.clone().downgrade());
        let prog_w = glib::SendWeakRef::from(prog.clone().downgrade());
        let licenses = spec.licenses.clone();
        let download_url = spec.download_url.clone();
        let action_lbl = action.clone();
        action.connect_clicked(move |_| {
            act.set_sensitive(false);
            action_lbl.set_label("下载中…");
            prog.set_fraction(0.0);
            prog.set_visible(true);
            // 审计 #12：进度定时器只在下载期间存在，完成后 Break 释放
            let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let done_timer = done.clone();
            let prog_reader = progress.clone();
            let prog_timer = prog.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
                if done_timer.load(std::sync::atomic::Ordering::Relaxed) {
                    return glib::ControlFlow::Break;
                }
                let frac = prog_reader.load(std::sync::atomic::Ordering::Relaxed) as f64 / 10000.0;
                prog_timer.set_fraction(frac);
                glib::ControlFlow::Continue
            });
            let archive = archive.clone();
            let progress = progress.clone();
            let sdk = sdk.clone();
            let kind = kind.clone();
            let refresh = refresh.clone();
            let licenses = licenses.clone();
            let download_url = download_url.clone();
            let win_for_license = win_for_license.clone();
            let action_w = action_w.clone();
            let act_w = act_w.clone();
            let prog_w = prog_w.clone();
            spawn_async(async move {
                let result: Result<_, PackageError> = async {
                    let service = PackageService::new()?;
                    let missing = service.missing_licenses(&sdk, &licenses)?;
                    if !missing.is_empty() {
                        let (tx, rx) = std::sync::mpsc::channel();
                        let display = missing
                            .iter()
                            .map(|license| license.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n——\n\n");
                        let win = win_for_license.clone();
                        post_ui(move || {
                            if let Some(window) = win.upgrade() {
                                show_license_dialog(&window, &display, tx);
                            }
                        });
                        match rx.recv() {
                            Ok(LicenseDecision::Accept) => {
                                service.accept_licenses(&sdk, &missing)?
                            }
                            Ok(LicenseDecision::Decline) => {
                                return Err(PackageError::LicenseDeclined);
                            }
                            Ok(LicenseDecision::Closed) | Err(_) => {
                                return Err(PackageError::LicenseDialogClosed);
                            }
                        }
                    }
                    service
                        .execute(
                            PackageOperation::Install(InstallRequest {
                                sdk_root: sdk,
                                kind,
                                archive,
                                url: download_url,
                                licenses,
                            }),
                            |event| {
                                if let PackageEvent::Downloading { downloaded, total } = event
                                    && total > 0
                                {
                                    progress.store(
                                        downloaded.saturating_mul(10_000) / total,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                }
                            },
                        )
                        .await
                }
                .await;
                done.store(true, std::sync::atomic::Ordering::Relaxed);
                if result.is_ok() {
                    refresh.component_changed();
                }
                let action_l = action_w.clone();
                let act_l = act_w.clone();
                let prog_l = prog_w.clone();
                post_ui(move || {
                    let a = action_l.upgrade();
                    if let Err(error) = result {
                        if let Some(a) = a {
                            if matches!(
                                &error,
                                PackageError::LicenseDeclined | PackageError::LicenseDialogClosed
                            ) {
                                a.set_label("下载");
                            } else {
                                a.set_label("重试");
                            }
                            a.set_tooltip_text(Some(&error.to_string()));
                        }
                        if let Some(a) = act_l.upgrade() {
                            a.set_sensitive(true);
                        }
                        if let Some(p) = prog_l.upgrade() {
                            p.set_visible(false);
                        }
                    } else {
                        if let Some(a) = a {
                            a.set_label("已安装");
                            a.set_sensitive(false);
                        }
                        if let Some(p) = prog_l.upgrade() {
                            p.set_visible(false);
                        }
                    }
                });
            });
        });
    } else {
        action.set_label("—");
        action.set_sensitive(false);
    }

    let right = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    right.set_valign(gtk4::Align::Center);
    right.append(&progress);
    right.append(&action);
    box_.append(&right);
    row.set_child(Some(&box_));
    row
}

/// 许可对话框的决策：同意则继续，拒绝/关闭则中止。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LicenseDecision {
    Accept,
    Decline,
    Closed,
}

/// license 对话框：展示协议全文，同意/拒绝均显式回传决策。
fn show_license_dialog(parent: &Window, texts: &str, tx: std::sync::mpsc::Sender<LicenseDecision>) {
    let win = Window::builder()
        .title("软件许可协议")
        .modal(true)
        .transient_for(parent)
        .default_width(560)
        .default_height(420)
        .build();
    let box_ = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    box_.set_margin_top(12);
    box_.set_margin_bottom(12);
    box_.set_margin_start(12);
    box_.set_margin_end(12);
    let hint = Label::new(Some("安装前需同意以下许可协议："));
    hint.set_xalign(0.0);
    box_.append(&hint);
    let scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .build();
    let text = Label::new(Some(texts));
    text.set_wrap(true);
    text.set_xalign(0.0);
    scroll.set_child(Some(&text));
    box_.append(&scroll);
    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    buttons.set_halign(gtk4::Align::End);
    let decline = Button::with_label("拒绝");
    let agree = Button::with_label("同意并下载");
    agree.add_css_class("suggested-action");
    let tx_decline = tx.clone();
    let win_d = win.downgrade();
    decline.connect_clicked(move |_| {
        let _ = tx_decline.send(LicenseDecision::Decline);
        if let Some(window) = win_d.upgrade() {
            window.close();
        }
    });
    let win2 = win.downgrade();
    let tx_agree = tx.clone();
    agree.connect_clicked(move |_| {
        let _ = tx_agree.send(LicenseDecision::Accept);
        if let Some(window) = win2.upgrade() {
            window.close();
        }
    });
    win.connect_close_request(move |_| {
        let _ = tx.send(LicenseDecision::Closed);
        glib::Propagation::Proceed
    });
    buttons.append(&decline);
    buttons.append(&agree);
    box_.append(&buttons);
    win.set_child(Some(&box_));
    win.present();
}

/// 打开镜像管理对话框。on_changed 在安装/卸载后触发（设备列表等刷新）。
pub fn open(parent: &impl IsA<Window>, on_changed: std::sync::Arc<dyn Fn() + Send + Sync>) {
    let sdk = crate::ui::main_window::sdk_root();
    open_for_sdk(parent, sdk, on_changed);
}

/// 为指定 SDK 打开镜像管理；创建向导用此入口保证安装目标与当前选择一致。
pub fn open_for_sdk(
    parent: &impl IsA<Window>,
    sdk: PathBuf,
    on_changed: std::sync::Arc<dyn Fn() + Send + Sync>,
) {
    let win = Window::builder()
        .title("镜像管理")
        .modal(true)
        .transient_for(parent)
        .default_width(760)
        .default_height(600)
        .build();

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    header.set_margin_top(10);
    header.set_margin_bottom(10);
    header.set_margin_start(12);
    header.set_margin_end(12);

    let path = Label::new(Some(&format!("SDK：{}", sdk.display())));
    path.set_xalign(0.0);
    path.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    header.append(&path);

    let spinner = gtk4::Spinner::new();
    spinner.set_visible(false);
    header.append(&spinner);

    let close_btn = Button::with_label("关闭");
    let win2 = win.clone();
    close_btn.connect_clicked(move |_| win2.close());
    let refresh = Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("重新拉取仓库"));
    header.append(&refresh);
    header.append(&close_btn);
    outer.append(&header);

    let scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .build();
    let body = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    body.set_margin_bottom(12);

    let installed_label = Label::new(Some("本机已装"));
    installed_label.add_css_class("headings");
    installed_label.set_xalign(0.0);
    installed_label.set_margin_top(8);
    installed_label.set_margin_start(12);
    installed_label.set_margin_bottom(2);
    body.append(&installed_label);
    let installed_list = ListBox::new();
    installed_list.set_selection_mode(gtk4::SelectionMode::None);
    body.append(&installed_list);

    let online_label = Label::new(Some("在线仓库（Google）"));
    online_label.add_css_class("headings");
    online_label.set_xalign(0.0);
    online_label.set_margin_top(12);
    online_label.set_margin_start(12);
    online_label.set_margin_bottom(2);
    body.append(&online_label);
    let online_list = ListBox::new();
    online_list.set_selection_mode(gtk4::SelectionMode::None);
    body.append(&online_list);
    let loading = Label::new(Some("正在拉取仓库…"));
    loading.add_css_class("dim-label");
    loading.set_margin_top(12);
    body.append(&loading);

    scroll.set_child(Some(&body));
    outer.append(&scroll);
    win.set_child(Some(&outer));

    let page_refresh = PageRefresh {
        sdk: sdk.clone(),
        window: glib::SendWeakRef::from(win.downgrade()),
        installed_list: glib::SendWeakRef::from(installed_list.downgrade()),
        online_list: glib::SendWeakRef::from(online_list.downgrade()),
        online_specs: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        external_changed: on_changed,
    };
    page_refresh.render_now();

    let spinner2 = spinner.clone();
    let loading2 = loading.clone();
    let sdk2 = sdk.clone();
    let page_refresh2 = page_refresh.clone();
    let load_online = move || {
        spinner2.set_visible(true);
        loading2.set_visible(true);
        loading2.set_text("正在拉取仓库…");
        let sdk = sdk2.clone();
        let page_refresh = page_refresh2.clone();
        let loading = glib::SendWeakRef::from(loading2.downgrade());
        let spinner = glib::SendWeakRef::from(spinner2.downgrade());
        spawn_async(async move {
            let repos = async {
                let comp = Repo::fetch_components().await?;
                let sys = Repo::fetch_sys_images().await?;
                anyhow::Ok((comp, sys))
            }
            .await;
            post_ui(move || {
                let (Some(spinner), Some(loading)) = (spinner.upgrade(), loading.upgrade()) else {
                    return;
                };
                spinner.set_visible(false);
                match repos {
                    Ok((comp, sys)) => {
                        loading.set_text("");
                        loading.set_visible(false);
                        let specs = online_rows(&sdk, &comp, &sys);
                        let empty = specs.is_empty();
                        page_refresh.replace_online(specs);
                        if empty {
                            loading.set_text("仓库为空或无法解析");
                            loading.set_visible(true);
                        }
                    }
                    Err(e) => {
                        loading.set_text(&format!("拉取仓库失败：{e:#}"));
                        loading.set_visible(true);
                    }
                }
            });
        });
    };
    load_online();
    let load_online2 = load_online.clone();
    refresh.connect_clicked(move |_| load_online2());

    win.present();
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use sha2::{Digest as _, Sha256};

    use super::*;
    use crate::core::repo::Checksum;

    static TEST_SEQUENCE: AtomicU32 = AtomicU32::new(0);

    fn test_dir() -> PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "liteavd-images-page-service-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn platform_tools_zip(path: &Path) -> Vec<u8> {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(
            "platform-tools/adb",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(format!("fake adb {}", std::process::id()).as_bytes())
            .unwrap();
        zip.finish().unwrap();
        std::fs::read(path).unwrap()
    }

    fn serve_once(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/platform-tools.zip",
            listener.local_addr().unwrap()
        );
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(&body).unwrap();
        });
        url
    }

    fn find_button(root: &gtk4::Widget, label: &str) -> Option<Button> {
        let mut stack = vec![root.clone()];
        while let Some(widget) = stack.pop() {
            if let Some(button) = widget.downcast_ref::<Button>()
                && button.label().as_deref() == Some(label)
            {
                return Some(button.clone());
            }
            let mut child = widget.first_child();
            while let Some(next) = child {
                stack.push(next.clone());
                child = next.next_sibling();
            }
        }
        None
    }

    fn iterate_main_context() {
        let context = glib::MainContext::default();
        while context.pending() {
            context.iteration(false);
        }
    }

    #[test]
    #[ignore = "requires GTK display; run under Xvfb"]
    fn package_service_success_rebuilds_installed_and_online_lists() {
        gtk4::init().expect("GTK 初始化失败");
        let root = test_dir();
        let sdk = root.join("sdk");
        let bytes = platform_tools_zip(&root.join("platform-tools.zip"));
        let url = serve_once(bytes.clone());
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let archive = Archive {
            url: url.clone(),
            size: bytes.len() as u64,
            checksum: Some(Checksum::Sha256(format!("{:x}", hasher.finalize()))),
            host_os: None,
            host_arch: None,
        };
        let cache_path = PackageService::new()
            .unwrap()
            .cache_path(&archive, &url)
            .unwrap();
        if let Some(cache_entry) = cache_path.parent() {
            let _ = std::fs::remove_dir_all(cache_entry);
        }

        let window = Window::new();
        let installed_list = ListBox::new();
        let online_list = ListBox::new();
        let changed = Arc::new(AtomicU32::new(0));
        let changed_for_callback = changed.clone();
        let spec = RowSpec {
            kind: ComponentKind::PlatformTools,
            title: "Platform Tools".into(),
            subtitle: "test".into(),
            size: archive.size,
            archive: Some(archive),
            download_url: url,
            licenses: vec![],
            installed: false,
        };
        let refresh = PageRefresh {
            sdk: sdk.clone(),
            window: glib::SendWeakRef::from(window.downgrade()),
            installed_list: glib::SendWeakRef::from(installed_list.downgrade()),
            online_list: glib::SendWeakRef::from(online_list.downgrade()),
            online_specs: Arc::new(std::sync::Mutex::new(vec![spec])),
            external_changed: Arc::new(move || {
                changed_for_callback.fetch_add(1, Ordering::Relaxed);
            }),
        };
        refresh.render_now();
        let row = online_list.row_at_index(0).expect("缺少在线组件行");
        find_button(row.upcast_ref(), "下载")
            .expect("缺少下载按钮")
            .emit_clicked();

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            iterate_main_context();
            if changed.load(Ordering::Relaxed) > 0
                && installed_list.row_at_index(0).is_some()
                && find_button(
                    online_list
                        .row_at_index(0)
                        .expect("在线行不应消失")
                        .upcast_ref(),
                    "卸载",
                )
                .is_some()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(changed.load(Ordering::Relaxed), 1);
        assert!(sdk.join("platform-tools/adb").is_file());
        assert!(installed_list.row_at_index(0).is_some());
        assert!(
            find_button(
                online_list
                    .row_at_index(0)
                    .expect("在线行不应消失")
                    .upcast_ref(),
                "卸载"
            )
            .is_some()
        );

        if let Some(cache_entry) = cache_path.parent() {
            let _ = std::fs::remove_dir_all(cache_entry);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "requires GTK display; run under Xvfb"]
    fn license_dialog_reports_decline_and_window_close() {
        gtk4::init().expect("GTK 初始化失败");
        let parent = Window::new();

        let (decline_tx, decline_rx) = std::sync::mpsc::channel();
        show_license_dialog(&parent, "terms", decline_tx);
        let decline_window = Window::list_toplevels()
            .into_iter()
            .filter_map(|widget| widget.downcast::<Window>().ok())
            .find(|window| window.title().as_deref() == Some("软件许可协议"))
            .expect("缺少许可对话框");
        find_button(decline_window.upcast_ref(), "拒绝")
            .expect("缺少拒绝按钮")
            .emit_clicked();
        iterate_main_context();
        assert_eq!(
            decline_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            LicenseDecision::Decline
        );

        let (close_tx, close_rx) = std::sync::mpsc::channel();
        show_license_dialog(&parent, "terms", close_tx);
        let close_window = Window::list_toplevels()
            .into_iter()
            .filter_map(|widget| widget.downcast::<Window>().ok())
            .find(|window| window.title().as_deref() == Some("软件许可协议"))
            .expect("缺少第二个许可对话框");
        close_window.close();
        iterate_main_context();
        assert_eq!(
            close_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            LicenseDecision::Closed
        );
    }
}
