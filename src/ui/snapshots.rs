//! focused exact session 的本地 snapshot 列表与 save/load/delete UI。

use std::sync::Arc;

use glib::SendWeakRef;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::core::grpc::{SnapshotDetails, validate_snapshot_id};
use crate::core::instance::DeviceRuntime;
use crate::core::operation::{
    OperationRunError, SnapshotMutation, list_route_snapshots, mutate_route_snapshot,
};
use crate::core::workspace::WorkspaceRoute;

pub const SNAPSHOT_WINDOW_WIDGET: &str = "liteavd-snapshot-window";
pub const SNAPSHOT_LIST_WIDGET: &str = "liteavd-snapshot-list";
pub const SNAPSHOT_SAVE_WIDGET: &str = "liteavd-snapshot-save";

#[derive(Clone)]
struct SnapshotPage {
    runtime: Arc<DeviceRuntime>,
    route: WorkspaceRoute,
    window: SendWeakRef<gtk4::Window>,
    list: SendWeakRef<gtk4::ListBox>,
    status: SendWeakRef<gtk4::Label>,
}

impl SnapshotPage {
    fn set_status(&self, message: &str) {
        if let Some(status) = self.status.upgrade() {
            status.set_text(message);
        }
    }

    fn refresh(&self) {
        self.set_status("正在读取 snapshots…");
        let page = self.clone();
        crate::ui::device_list::spawn_async(async move {
            let result = list_route_snapshots(page.runtime.clone(), page.route.clone()).await;
            glib::MainContext::default().invoke(move || page.render(result));
        });
    }

    fn render(&self, result: Result<Vec<SnapshotDetails>, OperationRunError>) {
        let (Some(window), Some(list), Some(status)) = (
            self.window.upgrade(),
            self.list.upgrade(),
            self.status.upgrade(),
        ) else {
            return;
        };
        let snapshots = match result {
            Ok(snapshots) => snapshots,
            Err(error) => {
                status.set_text(&format!("读取失败：{}", describe_error(&error)));
                return;
            }
        };
        while let Some(row) = list.row_at_index(0) {
            list.remove(&row);
        }
        for snapshot in &snapshots {
            list.append(&build_row(snapshot, &window, self.clone()));
        }
        status.set_text(&format!("{} 个 snapshot", snapshots.len()));
    }

    fn mutate(&self, snapshot_id: String, mutation: SnapshotMutation) {
        self.set_status(match mutation {
            SnapshotMutation::Save => "正在保存 snapshot…",
            SnapshotMutation::Load => "正在加载 snapshot…",
            SnapshotMutation::Delete => "正在删除 snapshot…",
        });
        let page = self.clone();
        crate::ui::device_list::spawn_async(async move {
            let result = mutate_route_snapshot(
                page.runtime.clone(),
                page.route.clone(),
                snapshot_id,
                mutation,
            )
            .await;
            glib::MainContext::default().invoke(move || match result {
                Ok(()) => page.refresh(),
                Err(error) => page.set_status(&format!("操作失败：{}", describe_error(&error))),
            });
        });
    }
}

pub fn open(parent: &adw::ApplicationWindow, runtime: Arc<DeviceRuntime>) {
    let Some(route) = runtime.workspace_snapshot().focused else {
        show_error(parent, "当前没有 focused session");
        return;
    };
    if runtime.grpc_client_for_route(&route).is_none() {
        show_error(
            parent,
            "focused session 没有受认证控制通道，不能管理 snapshot",
        );
        return;
    }
    let window = gtk4::Window::builder()
        .title(format!("Snapshots · {}", route.avd_name))
        .modal(true)
        .transient_for(parent)
        .default_width(680)
        .default_height(520)
        .build();
    window.set_widget_name(SNAPSHOT_WINDOW_WIDGET);
    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    outer.set_margin_top(10);
    outer.set_margin_bottom(10);
    outer.set_margin_start(10);
    outer.set_margin_end(10);

    let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    let name = gtk4::Entry::new();
    name.set_hexpand(true);
    name.set_placeholder_text(Some("snapshot id（字母数字、-、_、.）"));
    header.append(&name);
    let save = gtk4::Button::with_label("保存当前状态");
    save.set_widget_name(SNAPSHOT_SAVE_WIDGET);
    save.add_css_class("suggested-action");
    header.append(&save);
    let refresh = gtk4::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("刷新 snapshot 列表"));
    header.append(&refresh);
    let close = gtk4::Button::with_label("关闭");
    let window_for_close = window.downgrade();
    close.connect_clicked(move |_| {
        if let Some(window) = window_for_close.upgrade() {
            window.close();
        }
    });
    header.append(&close);
    outer.append(&header);

    let status = gtk4::Label::new(None);
    status.set_xalign(0.0);
    status.add_css_class("dim-label");
    outer.append(&status);
    let list = gtk4::ListBox::new();
    list.set_widget_name(SNAPSHOT_LIST_WIDGET);
    list.set_selection_mode(gtk4::SelectionMode::None);
    let scroll = gtk4::ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .vexpand(true)
        .child(&list)
        .build();
    outer.append(&scroll);
    window.set_child(Some(&outer));

    let page = SnapshotPage {
        runtime,
        route,
        window: SendWeakRef::from(window.downgrade()),
        list: SendWeakRef::from(list.downgrade()),
        status: SendWeakRef::from(status.downgrade()),
    };
    {
        let page = page.clone();
        refresh.connect_clicked(move |_| page.refresh());
    }
    {
        let page = page.clone();
        let window = window.downgrade();
        save.connect_clicked(move |_| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let snapshot_id = name.text().trim().to_owned();
            if let Err(error) = validate_snapshot_id(&snapshot_id) {
                page.set_status(&error.to_string());
                return;
            }
            confirm_and_mutate(&window, page.clone(), snapshot_id, SnapshotMutation::Save);
        });
    }
    page.refresh();
    window.present();
}

fn build_row(
    snapshot: &SnapshotDetails,
    window: &gtk4::Window,
    page: SnapshotPage,
) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(8);
    content.set_margin_end(8);
    let labels = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    labels.set_hexpand(true);
    let title = gtk4::Label::new(Some(&snapshot.snapshot_id));
    title.set_xalign(0.0);
    labels.append(&title);
    let subtitle = gtk4::Label::new(Some(&format!(
        "{} · {} B",
        snapshot_status(snapshot.status),
        snapshot.size
    )));
    subtitle.set_xalign(0.0);
    subtitle.add_css_class("dim-label");
    labels.append(&subtitle);
    content.append(&labels);

    let load = gtk4::Button::with_label("加载");
    load.set_sensitive(snapshot.status != 1);
    let id_for_load = snapshot.snapshot_id.clone();
    let page_for_load = page.clone();
    let window_for_load = window.downgrade();
    load.connect_clicked(move |_| {
        let Some(window) = window_for_load.upgrade() else {
            return;
        };
        confirm_and_mutate(
            &window,
            page_for_load.clone(),
            id_for_load.clone(),
            SnapshotMutation::Load,
        );
    });
    content.append(&load);

    let delete = gtk4::Button::with_label("删除");
    delete.add_css_class("destructive-action");
    let id_for_delete = snapshot.snapshot_id.clone();
    let page_for_delete = page;
    let window_for_delete = window.downgrade();
    delete.connect_clicked(move |_| {
        let Some(window) = window_for_delete.upgrade() else {
            return;
        };
        confirm_and_mutate(
            &window,
            page_for_delete.clone(),
            id_for_delete.clone(),
            SnapshotMutation::Delete,
        );
    });
    content.append(&delete);
    row.set_child(Some(&content));
    row
}

fn confirm_and_mutate(
    window: &gtk4::Window,
    page: SnapshotPage,
    snapshot_id: String,
    mutation: SnapshotMutation,
) {
    let action = match mutation {
        SnapshotMutation::Save => "保存",
        SnapshotMutation::Load => "加载",
        SnapshotMutation::Delete => "删除",
    };
    let dialog = adw::AlertDialog::builder()
        .heading(format!("确认{action} snapshot"))
        .body(format!(
            "设备：{}（session {}）\nsnapshot：{}",
            page.route.avd_name, page.route.session_id, snapshot_id
        ))
        .build();
    dialog.add_response("cancel", "取消");
    dialog.add_response("confirm", action);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    if mutation != SnapshotMutation::Save {
        dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    }
    let window = window.clone();
    glib::spawn_future_local(async move {
        if dialog.choose_future(&window).await == "confirm" {
            page.mutate(snapshot_id, mutation);
        }
    });
}

fn snapshot_status(status: i32) -> &'static str {
    match status {
        0 => "兼容",
        1 => "不兼容",
        2 => "当前已加载",
        _ => "未知状态",
    }
}

fn describe_error(error: &OperationRunError) -> String {
    match error {
        OperationRunError::Failed(message) => message.clone(),
        OperationRunError::Canceled => "操作已取消".into(),
        OperationRunError::StaleRoute => "session 已变化".into(),
    }
}

fn show_error(parent: &adw::ApplicationWindow, message: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading("无法管理 snapshot")
        .body(message)
        .build();
    dialog.add_response("close", "关闭");
    dialog.set_close_response("close");
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_status_mapping_is_stable() {
        assert_eq!(snapshot_status(0), "兼容");
        assert_eq!(snapshot_status(1), "不兼容");
        assert_eq!(snapshot_status(2), "当前已加载");
        assert_eq!(snapshot_status(99), "未知状态");
    }

    fn find_button(root: &gtk4::Widget, label: &str) -> Option<gtk4::Button> {
        let mut stack = vec![root.clone()];
        while let Some(widget) = stack.pop() {
            if let Some(button) = widget.downcast_ref::<gtk4::Button>()
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

    #[test]
    #[ignore = "requires GTK display; run under Xvfb"]
    fn snapshot_result_renders_load_and_delete_actions() {
        gtk4::init().expect("GTK init");
        let window = gtk4::Window::new();
        let list = gtk4::ListBox::new();
        let status = gtk4::Label::new(None);
        let page = SnapshotPage {
            runtime: Arc::new(DeviceRuntime::default()),
            route: WorkspaceRoute {
                avd_name: "pixel".into(),
                session_id: 7,
                generation: 1,
            },
            window: SendWeakRef::from(window.downgrade()),
            list: SendWeakRef::from(list.downgrade()),
            status: SendWeakRef::from(status.downgrade()),
        };
        page.render(Ok(vec![SnapshotDetails {
            snapshot_id: "checkpoint".into(),
            status: 0,
            size: 4096,
            details: None,
        }]));

        let row = list.row_at_index(0).expect("snapshot row missing");
        assert!(find_button(row.upcast_ref(), "加载").is_some());
        assert!(find_button(row.upcast_ref(), "删除").is_some());
        assert_eq!(status.text(), "1 个 snapshot");
    }
}
