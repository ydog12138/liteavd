//! 当前 focused managed session 的有界日志查看、过滤与导出。

use std::path::PathBuf;
use std::sync::Arc;

use glib::SendWeakRef;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::core::instance::DeviceRuntime;
use crate::core::process_log::{self, SessionLogFilter};

pub const LOG_WINDOW_WIDGET: &str = "liteavd-session-log-window";
pub const LOG_FILTER_WIDGET: &str = "liteavd-session-log-filter";
pub const LOG_EXPORT_WIDGET: &str = "liteavd-session-log-export";
const LOG_TEXT_WIDGET: &str = "liteavd-session-log-text";

pub fn open(parent: &adw::ApplicationWindow, runtime: Arc<DeviceRuntime>) {
    let Some(route) = runtime.workspace_snapshot().focused else {
        show_error(parent, "当前没有 focused session");
        return;
    };
    let Some(session) = runtime.session_for_route(&route) else {
        show_error(parent, "focused session 已变化");
        return;
    };
    let Some(log_path) = session.log_path else {
        show_error(
            parent,
            "该 session 没有 liteavd 托管日志（外部 adopted session 不提供日志）",
        );
        return;
    };
    open_path(parent, &route.avd_name, log_path);
}

fn open_path(parent: &adw::ApplicationWindow, avd_name: &str, log_path: PathBuf) -> gtk4::Window {
    let window = gtk4::Window::builder()
        .title(format!("Session 日志 · {avd_name}"))
        .modal(true)
        .transient_for(parent)
        .default_width(820)
        .default_height(620)
        .build();
    window.set_widget_name(LOG_WINDOW_WIDGET);
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    outer.set_margin_top(10);
    outer.set_margin_bottom(10);
    outer.set_margin_start(10);
    outer.set_margin_end(10);

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let filter = gtk4::DropDown::from_strings(&["全部", "stdout", "stderr"]);
    filter.set_widget_name(LOG_FILTER_WIDGET);
    header.append(&filter);
    let refresh = gtk4::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("重新读取日志"));
    header.append(&refresh);
    let export = gtk4::Button::with_label("导出…");
    export.set_widget_name(LOG_EXPORT_WIDGET);
    header.append(&export);
    let status = gtk4::Label::new(None);
    status.set_xalign(0.0);
    status.set_hexpand(true);
    status.add_css_class("dim-label");
    header.append(&status);
    let close = gtk4::Button::with_label("关闭");
    let window_for_close = window.downgrade();
    close.connect_clicked(move |_| {
        if let Some(window) = window_for_close.upgrade() {
            window.close();
        }
    });
    header.append(&close);
    outer.append(&header);

    let text = gtk4::TextView::new();
    text.set_widget_name(LOG_TEXT_WIDGET);
    text.set_editable(false);
    text.set_cursor_visible(false);
    text.set_monospace(true);
    text.set_wrap_mode(gtk4::WrapMode::None);
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Automatic)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .child(&text)
        .build();
    outer.append(&scroll);
    window.set_child(Some(&outer));

    let load = Arc::new({
        let path = log_path.clone();
        let text = SendWeakRef::from(text.downgrade());
        let status = SendWeakRef::from(status.downgrade());
        move |selected: u32| {
            let path = path.clone();
            let text = text.clone();
            let status = status.clone();
            if let Some(status) = status.upgrade() {
                status.set_text("读取中…");
            }
            crate::ui::background::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    process_log::read_session_log(&path, filter_for_index(selected))
                })
                .await
                .map_err(|error| anyhow::anyhow!("日志读取 worker 失败：{error}"))
                .and_then(|result| result);
                glib::MainContext::default().invoke(move || {
                    let (Some(text), Some(status)) = (text.upgrade(), status.upgrade()) else {
                        return;
                    };
                    match result {
                        Ok(document) => {
                            text.buffer().set_text(&document.text);
                            status.set_text(&format!(
                                "源日志 {} B{}",
                                document.source_bytes,
                                if document.used_previous {
                                    "（含轮转文件）"
                                } else {
                                    ""
                                }
                            ));
                        }
                        Err(error) => status.set_text(&format!("读取失败：{error:#}")),
                    }
                });
            });
        }
    });
    load(filter.selected());
    {
        let load = load.clone();
        filter.connect_selected_notify(move |filter| load(filter.selected()));
    }
    {
        let load = load.clone();
        let filter = filter.clone();
        refresh.connect_clicked(move |_| load(filter.selected()));
    }
    {
        let parent = window.downgrade();
        let path = log_path;
        let filter = filter.clone();
        let status = SendWeakRef::from(status.downgrade());
        export.connect_clicked(move |_| {
            let Some(parent) = parent.upgrade() else {
                return;
            };
            let path = path.clone();
            let status = status.clone();
            let selected = filter.selected();
            glib::spawn_future_local(async move {
                let dialog = gtk4::FileDialog::builder()
                    .title("导出 session 日志")
                    .initial_name("liteavd-session.log")
                    .build();
                let Ok(file) = dialog.save_future(Some(&parent)).await else {
                    return;
                };
                let Some(destination) = file.path() else {
                    if let Some(status) = status.upgrade() {
                        status.set_text("导出只支持本地文件系统路径");
                    }
                    return;
                };
                export_log(path, destination, selected, status);
            });
        });
    }
    window.present();
    window
}

fn export_log(
    source: PathBuf,
    destination: PathBuf,
    selected: u32,
    status: SendWeakRef<gtk4::Label>,
) {
    crate::ui::background::spawn(async move {
        let destination_for_export = destination.clone();
        let result = tokio::task::spawn_blocking(move || {
            process_log::export_session_log(
                &source,
                &destination_for_export,
                filter_for_index(selected),
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("日志导出 worker 失败：{error}"))
        .and_then(|result| result);
        glib::MainContext::default().invoke(move || {
            let Some(status) = status.upgrade() else {
                return;
            };
            match result {
                Ok(bytes) => {
                    status.set_text(&format!("已导出 {}（{bytes} B）", destination.display()))
                }
                Err(error) => status.set_text(&format!("导出失败：{error:#}")),
            }
        });
    });
}

fn filter_for_index(index: u32) -> SessionLogFilter {
    match index {
        1 => SessionLogFilter::Stdout,
        2 => SessionLogFilter::Stderr,
        _ => SessionLogFilter::All,
    }
}

fn show_error(parent: &adw::ApplicationWindow, message: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading("无法打开 session 日志")
        .body(message)
        .build();
    dialog.add_response("close", "关闭");
    dialog.set_close_response("close");
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn filter_mapping_is_explicit() {
        assert_eq!(filter_for_index(0), SessionLogFilter::All);
        assert_eq!(filter_for_index(1), SessionLogFilter::Stdout);
        assert_eq!(filter_for_index(2), SessionLogFilter::Stderr);
        assert_eq!(filter_for_index(99), SessionLogFilter::All);
    }

    fn find_named(root: &gtk4::Widget, name: &str) -> gtk4::Widget {
        let mut stack = vec![root.clone()];
        while let Some(widget) = stack.pop() {
            if widget.widget_name() == name {
                return widget;
            }
            let mut child = widget.first_child();
            while let Some(next) = child {
                stack.push(next.clone());
                child = next.next_sibling();
            }
        }
        panic!("missing widget {name}");
    }

    #[test]
    #[ignore = "requires GTK display; run under Xvfb"]
    fn viewer_loads_and_filters_log_off_main_thread() {
        gtk4::init().expect("GTK init");
        let path = std::env::temp_dir().join(format!(
            "liteavd-session-log-ui-{}-{}.log",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, b"[stdout] visible\n[stderr] hidden\n").unwrap();
        let parent = adw::ApplicationWindow::builder().title("parent").build();
        let window = open_path(&parent, "pixel", path.clone());
        let text: gtk4::TextView = find_named(window.upcast_ref(), LOG_TEXT_WIDGET)
            .downcast()
            .unwrap();
        let filter: gtk4::DropDown = find_named(window.upcast_ref(), LOG_FILTER_WIDGET)
            .downcast()
            .unwrap();

        filter.set_selected(1);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let context = glib::MainContext::default();
            while context.pending() {
                context.iteration(false);
            }
            let buffer = text.buffer();
            let contents = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
            if contents.contains("visible") && !contents.contains("hidden") {
                break;
            }
            assert!(Instant::now() < deadline, "日志过滤 UI 未完成");
            std::thread::sleep(Duration::from_millis(5));
        }
        window.close();
        std::fs::remove_file(path).unwrap();
    }
}
