//! GUI 冒烟：设备卡片行构建 + 启停状态回传（审计 #10/#11 回归）。
//!
//! 需要图形环境：`DISPLAY=:97 cargo test --test gui_device_list_smoke -- --ignored`

use gtk4::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;

use liteavd::core::emulator::RunningInstance;
use liteavd::core::instance::DeviceRuntime;
use liteavd::core::scheduler::ResourceDemand;
use liteavd::ui::device_controls::{
    BACK_WIDGET, CONTROLS_WIDGET, HOME_WIDGET, MICROPHONE_FILE_WIDGET, MICROPHONE_STOP_WIDGET,
    MICROPHONE_WIDGET, OVERVIEW_WIDGET, POWER_WIDGET, SCREENSHOT_WIDGET, VOLUME_DOWN_WIDGET,
    VOLUME_MUTE_WIDGET, VOLUME_UP_WIDGET,
};
use liteavd::ui::device_list::{
    DeviceData, DeviceStatus, build_list, build_row, refresh_list, start_device, stop_device,
};

fn all_labels(widget: &gtk4::Widget) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<gtk4::Widget> = vec![widget.clone()];
    while let Some(w) = stack.pop() {
        if let Some(l) = w.downcast_ref::<gtk4::Label>() {
            out.push(l.text().to_string());
        }
        let mut child = w.first_child();
        while let Some(c) = child {
            stack.push(c.clone());
            child = c.next_sibling();
        }
    }
    out
}

#[test]
#[ignore = "需要图形环境（Xvfb/Wayland）"]
fn device_row_renders_and_status_callbacks_flow() {
    if gtk4::init().is_err() {
        panic!("gtk4 初始化失败（无图形环境？）");
    }
    let main_loop = glib::MainLoop::new(None, false);

    let data = DeviceData {
        name: "smoke-avd".into(),
        path: PathBuf::from("/tmp/smoke.avd"),
        status: DeviceStatus::Stopped,
        inst: None,
        resources: ResourceDemand::default(),
    };
    let runtime = Arc::new(DeviceRuntime::default());
    let row = build_row(&data, PathBuf::from("/nonexistent-sdk"), runtime.clone());
    let labels = all_labels(row.upcast_ref());
    assert!(
        labels.iter().any(|l| l.contains("smoke-avd")),
        "行应含设备名: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l.contains("已停止")),
        "行应显示已停止: {labels:?}"
    );

    let box_ = row.child().expect("行应有子控件");
    let mut quick_widgets = vec![box_.clone()];
    let mut quick_names = Vec::new();
    let mut quick_controls = None;
    let mut microphone = None;
    let mut microphone_file = None;
    while let Some(widget) = quick_widgets.pop() {
        quick_names.push(widget.widget_name().to_string());
        if widget.widget_name() == CONTROLS_WIDGET {
            quick_controls = Some(widget.clone());
        }
        if widget.widget_name() == MICROPHONE_WIDGET {
            microphone = Some(widget.clone());
        }
        if widget.widget_name() == MICROPHONE_FILE_WIDGET {
            microphone_file = widget.clone().downcast::<gtk4::Button>().ok();
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            quick_widgets.push(next.clone());
            child = next.next_sibling();
        }
    }
    for name in [
        CONTROLS_WIDGET,
        BACK_WIDGET,
        HOME_WIDGET,
        OVERVIEW_WIDGET,
        POWER_WIDGET,
        VOLUME_DOWN_WIDGET,
        VOLUME_MUTE_WIDGET,
        VOLUME_UP_WIDGET,
        MICROPHONE_WIDGET,
        MICROPHONE_FILE_WIDGET,
        MICROPHONE_STOP_WIDGET,
        SCREENSHOT_WIDGET,
    ] {
        assert!(
            quick_names.iter().any(|actual| actual == name),
            "缺少 {name}"
        );
    }
    assert!(
        quick_controls.is_some_and(|widget| !widget.is_sensitive()),
        "停止设备的快捷控制应整体禁用"
    );
    assert!(
        microphone.is_some_and(|widget| !widget.is_sensitive()),
        "停止设备的麦克风开关必须禁用"
    );
    let microphone_file = microphone_file.expect("应有 WAV 文件注入按钮");
    let controllers = microphone_file.observe_controllers();
    assert!(
        (0..controllers.n_items()).any(|index| controllers
            .item(index)
            .is_some_and(|controller| controller.is::<gtk4::DropTarget>())),
        "WAV 按钮应接受单文件拖放"
    );
    let mut stack: Vec<gtk4::Widget> = vec![box_];
    let mut buttons = Vec::new();
    while let Some(w) = stack.pop() {
        if let Some(b) = w.downcast_ref::<gtk4::Button>() {
            buttons.push(b.clone());
        }
        let mut child = w.first_child();
        while let Some(c) = child {
            stack.push(c.clone());
            child = c.next_sibling();
        }
    }
    let start = buttons
        .iter()
        .find(|b| b.label().as_deref() == Some("启动"))
        .expect("应有启动按钮");
    let stop = buttons
        .iter()
        .find(|b| b.label().as_deref() == Some("停止"))
        .expect("应有停止按钮");
    assert!(start.is_sensitive(), "Stopped 设备启动按钮应可点");
    assert!(!stop.is_sensitive(), "Stopped 设备停止按钮应禁用");

    let queued = DeviceData {
        status: DeviceStatus::Queued("等待启动并发名额（上限 1）".into()),
        ..data.clone()
    };
    let queued_row = build_row(&queued, PathBuf::from("/nonexistent-sdk"), runtime.clone());
    let queued_labels = all_labels(queued_row.upcast_ref());
    assert!(
        queued_labels.iter().any(|label| label.contains("上限 1")),
        "Queued 行应解释等待原因：{queued_labels:?}"
    );
    let mut queued_widgets = vec![queued_row.upcast::<gtk4::Widget>()];
    let mut cancel = None;
    while let Some(widget) = queued_widgets.pop() {
        if let Some(button) = widget.downcast_ref::<gtk4::Button>()
            && button.label().as_deref() == Some("取消")
        {
            cancel = Some(button.clone());
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            queued_widgets.push(next.clone());
            child = next.next_sibling();
        }
    }
    assert!(
        cancel.is_some_and(|button| button.is_sensitive()),
        "Queued 行应提供可用的取消按钮"
    );

    let running_instance = RunningInstance {
        pid: 424_242,
        ini_path: PathBuf::from("/tmp/pid_424242.ini"),
        avd_name: "selected-avd".into(),
        console_port: 5554,
        adb_port: 5555,
        grpc_port: 8554,
        grpc_allowlist: None,
        grpc_jwks: None,
        grpc_jwk_active: None,
    };
    let selection_runtime = Arc::new(DeviceRuntime::default());
    selection_runtime.reconcile_running(vec![running_instance.clone()]);
    let selection_data = DeviceData {
        name: "selected-avd".into(),
        path: PathBuf::from("/tmp/selected.avd"),
        status: DeviceStatus::Running,
        inst: Some(running_instance),
        resources: ResourceDemand::default(),
    };
    let selection_row = build_row(
        &selection_data,
        PathBuf::from("/nonexistent-sdk"),
        selection_runtime.clone(),
    );
    let mut selection_widgets = vec![selection_row.upcast::<gtk4::Widget>()];
    let mut selection = None;
    while let Some(widget) = selection_widgets.pop() {
        if let Some(button) = widget.downcast_ref::<gtk4::CheckButton>() {
            selection = Some(button.clone());
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            selection_widgets.push(next.clone());
            child = next.next_sibling();
        }
    }
    let selection = selection.expect("running 卡片应有选择控件");
    assert!(selection.is_sensitive());
    selection.set_active(true);
    assert_eq!(selection_runtime.workspace_snapshot().selected.len(), 1);
    selection.set_active(false);
    assert!(selection_runtime.workspace_snapshot().selected.is_empty());

    // 审计 #10/#11：stop 路径（无实例 → Stopped 回传），验证 worker 状态回调可达
    let status = std::sync::Arc::new(std::sync::Mutex::new(None));
    let status2 = status.clone();
    let loop2 = main_loop.clone();
    let d = data.clone();
    let runtime_for_stop = runtime.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            stop_device(
                &d,
                PathBuf::from("/nonexistent-sdk"),
                runtime_for_stop,
                move |st| {
                    *status2.lock().unwrap() = Some(st);
                    loop2.quit();
                },
            )
            .await;
        });
    });
    main_loop.run();
    assert_eq!(
        *status.lock().unwrap(),
        Some(DeviceStatus::Stopped),
        "inst=None 停止应回传 Stopped"
    );

    // 审计 #10：start_device 对不存在的 SDK 应回传 Error（而非卡死/崩溃）
    let status3 = std::sync::Arc::new(std::sync::Mutex::new(None));
    let status4 = status3.clone();
    let loop3 = glib::MainLoop::new(None, false);
    let l3 = loop3.clone();
    let d3 = data.clone();
    let runtime_for_start = runtime;
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            start_device(
                &d3,
                PathBuf::from("/nonexistent-sdk"),
                runtime_for_start,
                move |st| {
                    if !matches!(st, DeviceStatus::Queued(_) | DeviceStatus::Starting) {
                        *status4.lock().unwrap() = Some(st);
                        l3.quit();
                    }
                },
            )
            .await;
        });
    });
    loop3.run();
    let st = status3.lock().unwrap().take().expect("启动回调未到达");
    assert!(
        matches!(st, DeviceStatus::Error(_)),
        "SDK 不存在应回传 Error，实际 {st:?}"
    );

    // A-02/WP-1.1：广告 rescan 只刷新现有行，不销毁启动回调绑定的 widget。
    let old_avd_home = std::env::var_os("ANDROID_AVD_HOME");
    let avd_home = std::env::temp_dir().join(format!("liteavd-gui-refresh-{}", std::process::id()));
    std::fs::create_dir_all(avd_home.join("refresh-avd.avd")).unwrap();
    std::fs::write(
        avd_home.join("refresh-avd.avd/config.ini"),
        "AvdId=refresh-avd\ntarget=android-35\n",
    )
    .unwrap();
    std::fs::write(avd_home.join("refresh-avd.ini"), "target=android-35\n").unwrap();
    unsafe { std::env::set_var("ANDROID_AVD_HOME", &avd_home) };
    let refresh_runtime = Arc::new(DeviceRuntime::default());
    let command = refresh_runtime.begin_start("refresh-avd").unwrap();
    let list = build_list(PathBuf::from("/nonexistent-sdk"), refresh_runtime.clone());
    let original_row = list.row_at_index(0).unwrap();
    refresh_runtime.fail_start(&command, "refresh-error".into());
    refresh_list(&list, PathBuf::from("/nonexistent-sdk"), refresh_runtime);
    let refreshed_row = list.row_at_index(0).unwrap();
    assert_eq!(original_row, refreshed_row, "状态刷新不应重建 AVD 行");
    let refreshed_labels = all_labels(refreshed_row.upcast_ref());
    assert!(
        refreshed_labels
            .iter()
            .any(|label| label.contains("refresh-error")),
        "现有行应投影最新 Error：{refreshed_labels:?}"
    );
    let mut widgets = vec![refreshed_row.upcast::<gtk4::Widget>()];
    while let Some(widget) = widgets.pop() {
        if let Some(button) = widget.downcast_ref::<gtk4::Button>()
            && button.label().as_deref() == Some("启动")
        {
            assert!(button.is_sensitive(), "Error 设备应允许重新启动");
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            widgets.push(next.clone());
            child = next.next_sibling();
        }
    }
    std::fs::remove_dir_all(avd_home).unwrap();
    match old_avd_home {
        Some(path) => unsafe { std::env::set_var("ANDROID_AVD_HOME", path) },
        None => unsafe { std::env::remove_var("ANDROID_AVD_HOME") },
    }
}
