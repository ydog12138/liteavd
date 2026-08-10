//! WP-3.5 focused audio controls GUI smoke.
//!
//! `DISPLAY=:98 cargo test --test gui_audio_smoke -- --ignored --nocapture`

use std::sync::Arc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use liteavd::core::instance::DeviceRuntime;
use liteavd::ui::audio::{
    AudioController, CONTROLS_WIDGET, ENABLE_WIDGET, MUTE_WIDGET, VOLUME_WIDGET, build_controls,
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
fn focused_audio_controls_are_visible_and_update_controller() {
    gtk4::init().expect("GTK init");
    let window = adw::ApplicationWindow::builder()
        .title("audio controls smoke")
        .build();
    let controller = AudioController::new(Arc::new(DeviceRuntime::default()));
    let controls = build_controls(controller.clone());
    window.set_content(Some(&controls));
    window.present();

    let root = controls.upcast_ref::<gtk4::Widget>();
    assert_eq!(root.widget_name(), CONTROLS_WIDGET);
    let enable: gtk4::ToggleButton = find_named(root, ENABLE_WIDGET).downcast().unwrap();
    let mute: gtk4::ToggleButton = find_named(root, MUTE_WIDGET).downcast().unwrap();
    let volume: gtk4::Scale = find_named(root, VOLUME_WIDGET).downcast().unwrap();
    assert!(enable.is_visible() && enable.is_active());
    assert!(mute.is_visible() && !mute.is_active());
    assert!(volume.is_visible());

    mute.set_active(true);
    volume.set_value(0.35);
    assert!(controller.muted());
    assert!((controller.volume() - 0.35).abs() < 0.001);

    enable.set_active(false);
    assert!(!controller.enabled());
    window.close();
}
