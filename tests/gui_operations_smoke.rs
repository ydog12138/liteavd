//! WP-2.3 跨设备操作工具栏 GUI smoke。
//!
//! `DISPLAY=:98 cargo test --test gui_operations_smoke -- --ignored --nocapture`

use std::sync::Arc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use liteavd::core::instance::DeviceRuntime;
use liteavd::ui::operations::{
    INSTALL_WIDGET, LOG_WIDGET, PUSH_WIDGET, SCOPE_WIDGET, SCREENSHOT_WIDGET, SNAPSHOT_WIDGET,
    STOP_WIDGET, build_controls,
};

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
#[ignore = "需要图形环境（Xvfb/Wayland）"]
fn operation_scope_and_actions_are_visible() {
    gtk4::init().expect("GTK init");
    let window = adw::ApplicationWindow::builder()
        .title("operation smoke")
        .build();
    let controls = build_controls(&window, Arc::new(DeviceRuntime::default()), Arc::new(|| {}));
    window.set_content(Some(&controls));
    window.present();

    let root = controls.upcast_ref::<gtk4::Widget>();
    let scope: gtk4::DropDown = find_named(root, SCOPE_WIDGET).downcast().unwrap();
    assert_eq!(scope.selected(), 0);
    scope.set_selected(1);
    assert_eq!(scope.selected(), 1);
    scope.set_selected(2);
    assert_eq!(scope.selected(), 2);

    for name in [
        SCREENSHOT_WIDGET,
        INSTALL_WIDGET,
        PUSH_WIDGET,
        SNAPSHOT_WIDGET,
        LOG_WIDGET,
        STOP_WIDGET,
    ] {
        let button: gtk4::Button = find_named(root, name).downcast().unwrap();
        assert!(button.is_visible() && button.is_sensitive(), "{name}");
    }
    let install: gtk4::Button = find_named(root, INSTALL_WIDGET).downcast().unwrap();
    let controllers = install.observe_controllers();
    assert!(
        (0..controllers.n_items()).any(|index| controllers
            .item(index)
            .is_some_and(|controller| controller.is::<gtk4::DropTarget>())),
        "APK 按钮应接受文件拖放"
    );
    let push: gtk4::Button = find_named(root, PUSH_WIDGET).downcast().unwrap();
    let controllers = push.observe_controllers();
    assert!(
        (0..controllers.n_items()).any(|index| controllers
            .item(index)
            .is_some_and(|controller| controller.is::<gtk4::DropTarget>())),
        "文件推送按钮应接受文件拖放"
    );
    let stop: gtk4::Button = find_named(root, STOP_WIDGET).downcast().unwrap();
    assert!(stop.has_css_class("destructive-action"));
    window.close();
}
