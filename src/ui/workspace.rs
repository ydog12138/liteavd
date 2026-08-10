//! 响应式多设备工作区。
//!
//! FlowBox 保持 1–3 列同时可见；卡片刷新复用既有 viewport，避免广告事件
//! 打断其他 session 的视频和输入 worker。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gtk4::prelude::*;

use crate::core::instance::DeviceRuntime;
use crate::ui::device_list::{self, DeviceData};
use crate::ui::microphone::MicrophoneController;
use crate::ui::viewport::PICTURE_WIDGET;

pub const WORKSPACE_WIDGET: &str = "liteavd-workspace";
const CHILD_PREFIX: &str = "liteavd-workspace-device-";
const MIN_CARD_WIDTH: i32 = 300;
const MAX_COLUMNS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShortcutAction {
    Focus(i32),
    Activate(&'static str),
}

pub fn build(
    sdk_root: PathBuf,
    runtime: Arc<DeviceRuntime>,
    microphone: Arc<MicrophoneController>,
) -> gtk4::FlowBox {
    let devices = device_list::list_devices(&sdk_root, &runtime);
    build_from_devices(&devices, &sdk_root, runtime, microphone)
}

fn build_from_devices(
    devices: &[DeviceData],
    sdk_root: &Path,
    runtime: Arc<DeviceRuntime>,
    microphone: Arc<MicrophoneController>,
) -> gtk4::FlowBox {
    let workspace = gtk4::FlowBox::new();
    workspace.set_widget_name(WORKSPACE_WIDGET);
    workspace.set_selection_mode(gtk4::SelectionMode::Single);
    workspace.set_activate_on_single_click(true);
    workspace.set_min_children_per_line(1);
    workspace.set_max_children_per_line(MAX_COLUMNS);
    workspace.set_homogeneous(true);
    workspace.set_column_spacing(12);
    workspace.set_row_spacing(12);
    workspace.set_valign(gtk4::Align::Start);
    workspace.set_margin_top(12);
    workspace.set_margin_bottom(12);
    workspace.set_margin_start(12);
    workspace.set_margin_end(12);
    install_shortcuts(&workspace, runtime.clone());
    rebuild_contents(&workspace, devices, sdk_root, &runtime, &microphone);
    sync_focused_child(&workspace, &runtime);
    workspace
}

pub fn refresh(
    workspace: &gtk4::FlowBox,
    sdk_root: PathBuf,
    runtime: Arc<DeviceRuntime>,
    microphone: Arc<MicrophoneController>,
) {
    let devices = device_list::list_devices(&sdk_root, &runtime);
    let expected: Vec<_> = devices
        .iter()
        .map(|device| child_widget_name(&device.name))
        .collect();
    if child_names(workspace) != expected {
        rebuild_contents(workspace, &devices, &sdk_root, &runtime, &microphone);
    } else {
        for (index, device) in devices.iter().enumerate() {
            if let Some(child) = workspace.child_at_index(index as i32) {
                device_list::apply_projection(child.upcast_ref(), device, &runtime);
            }
        }
    }
    sync_focused_child(workspace, &runtime);
}

/// 只投影现有 registry，不扫描 SDK/广告目录；用于 worker 报告的控制面健康变化。
pub fn refresh_runtime_projection(workspace: &gtk4::FlowBox, runtime: Arc<DeviceRuntime>) {
    let mut index = 0;
    while let Some(child) = workspace.child_at_index(index) {
        if let Some(avd_name) = child_avd_name(&child) {
            device_list::apply_runtime_projection(child.upcast_ref(), &avd_name, &runtime);
        }
        index += 1;
    }
    sync_focused_child(workspace, &runtime);
}

fn rebuild_contents(
    workspace: &gtk4::FlowBox,
    devices: &[DeviceData],
    sdk_root: &Path,
    runtime: &Arc<DeviceRuntime>,
    microphone: &Arc<MicrophoneController>,
) {
    workspace.remove_all();
    for (index, device) in devices.iter().enumerate() {
        let card = device_list::build_card(
            device,
            sdk_root.to_path_buf(),
            runtime.clone(),
            microphone.clone(),
        );
        workspace.insert(&card, -1);
        let child = workspace
            .child_at_index(index as i32)
            .expect("inserted workspace card");
        child.set_widget_name(&child_widget_name(&device.name));
        child.set_size_request(MIN_CARD_WIDTH, -1);
        child.set_hexpand(true);

        let click = gtk4::GestureClick::new();
        click.set_button(1);
        click.set_propagation_phase(gtk4::PropagationPhase::Capture);
        let workspace_weak = workspace.downgrade();
        let child_weak = child.downgrade();
        let runtime = runtime.clone();
        click.connect_pressed(move |_, _, _, _| {
            let (Some(workspace), Some(child)) = (workspace_weak.upgrade(), child_weak.upgrade())
            else {
                return;
            };
            let _ = focus_child(&workspace, &child, &runtime);
        });
        card.add_controller(click);
    }
}

fn install_shortcuts(workspace: &gtk4::FlowBox, runtime: Arc<DeviceRuntime>) {
    let keys = gtk4::EventControllerKey::new();
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let workspace_weak = workspace.downgrade();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let key_name = key.name();
        let Some(action) = shortcut_action(key_name.as_deref(), key.to_unicode(), modifiers) else {
            return glib::Propagation::Proceed;
        };
        let Some(workspace) = workspace_weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let handled = match action {
            ShortcutAction::Focus(index) => focus_index(&workspace, index, &runtime),
            ShortcutAction::Activate(widget_name) => {
                activate_focused_control(&workspace, &runtime, widget_name)
            }
        };
        if handled {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    workspace.add_controller(keys);
}

fn shortcut_action(
    key_name: Option<&str>,
    unicode: Option<char>,
    modifiers: gtk4::gdk::ModifierType,
) -> Option<ShortcutAction> {
    use gtk4::gdk::ModifierType;

    let relevant = modifiers
        & (ModifierType::CONTROL_MASK
            | ModifierType::ALT_MASK
            | ModifierType::SHIFT_MASK
            | ModifierType::SUPER_MASK
            | ModifierType::META_MASK);
    if relevant == ModifierType::CONTROL_MASK
        && let Some(index) = unicode
            .and_then(|value| value.to_digit(10))
            .filter(|value| (1..=9).contains(value))
            .map(|value| value as i32 - 1)
    {
        return Some(ShortcutAction::Focus(index));
    }
    if relevant == (ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK)
        && unicode.is_some_and(|value| value.eq_ignore_ascii_case(&'s'))
    {
        return Some(ShortcutAction::Activate(
            crate::ui::device_controls::SCREENSHOT_WIDGET,
        ));
    }
    if relevant == ModifierType::ALT_MASK {
        return match (key_name, unicode.map(|value| value.to_ascii_lowercase())) {
            (Some("Left"), _) => Some(ShortcutAction::Activate(
                crate::ui::device_controls::BACK_WIDGET,
            )),
            (Some("Home"), _) => Some(ShortcutAction::Activate(
                crate::ui::device_controls::HOME_WIDGET,
            )),
            (_, Some('o')) => Some(ShortcutAction::Activate(
                crate::ui::device_controls::OVERVIEW_WIDGET,
            )),
            (_, Some('p')) => Some(ShortcutAction::Activate(
                crate::ui::device_controls::POWER_WIDGET,
            )),
            _ => None,
        };
    }
    if relevant == ModifierType::CONTROL_MASK {
        return match (key_name, unicode.map(|value| value.to_ascii_lowercase())) {
            (Some("Down"), _) => Some(ShortcutAction::Activate(
                crate::ui::device_controls::VOLUME_DOWN_WIDGET,
            )),
            (Some("Up"), _) => Some(ShortcutAction::Activate(
                crate::ui::device_controls::VOLUME_UP_WIDGET,
            )),
            (_, Some('m')) => Some(ShortcutAction::Activate(
                crate::ui::device_controls::VOLUME_MUTE_WIDGET,
            )),
            _ => None,
        };
    }
    None
}

fn activate_focused_control(
    workspace: &gtk4::FlowBox,
    runtime: &DeviceRuntime,
    widget_name: &str,
) -> bool {
    let Some(focused) = runtime.workspace_snapshot().focused else {
        return false;
    };
    let mut index = 0;
    while let Some(child) = workspace.child_at_index(index) {
        if child_avd_name(&child).as_deref() == Some(focused.avd_name.as_str())
            && let Some(button) = find_named_widget(child.upcast_ref(), widget_name)
                .and_then(|widget| widget.downcast::<gtk4::Button>().ok())
            && button.is_sensitive()
        {
            button.emit_clicked();
            return true;
        }
        index += 1;
    }
    false
}

fn focus_index(workspace: &gtk4::FlowBox, index: i32, runtime: &DeviceRuntime) -> bool {
    workspace
        .child_at_index(index)
        .is_some_and(|child| focus_child(workspace, &child, runtime))
}

fn focus_child(
    workspace: &gtk4::FlowBox,
    child: &gtk4::FlowBoxChild,
    runtime: &DeviceRuntime,
) -> bool {
    let Some(avd_name) = child_avd_name(child) else {
        return false;
    };
    if runtime.focus_session(&avd_name).is_err() {
        return false;
    }
    workspace.select_child(child);
    if let Some(picture) = find_named_widget(child.upcast_ref(), PICTURE_WIDGET) {
        picture.grab_focus();
    }
    true
}

fn sync_focused_child(workspace: &gtk4::FlowBox, runtime: &DeviceRuntime) {
    let focused = runtime.workspace_snapshot().focused;
    let Some(focused) = focused else {
        workspace.unselect_all();
        return;
    };
    let mut index = 0;
    while let Some(child) = workspace.child_at_index(index) {
        if child_avd_name(&child).as_deref() == Some(focused.avd_name.as_str()) {
            workspace.select_child(&child);
            return;
        }
        index += 1;
    }
    workspace.unselect_all();
}

fn child_names(workspace: &gtk4::FlowBox) -> Vec<String> {
    let mut names = Vec::new();
    let mut index = 0;
    while let Some(child) = workspace.child_at_index(index) {
        names.push(child.widget_name().to_string());
        index += 1;
    }
    names
}

fn child_widget_name(avd_name: &str) -> String {
    format!("{CHILD_PREFIX}{avd_name}")
}

fn child_avd_name(child: &gtk4::FlowBoxChild) -> Option<String> {
    child
        .widget_name()
        .strip_prefix(CHILD_PREFIX)
        .map(str::to_owned)
}

fn find_named_widget(root: &gtk4::Widget, name: &str) -> Option<gtk4::Widget> {
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if widget.widget_name() == name {
            return Some(widget);
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            stack.push(next.clone());
            child = next.next_sibling();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::core::device_state::DevicePhase;
    use crate::core::emulator::{LaunchedInstance, RunningInstance};
    use crate::core::grpc_auth::GrpcJwtAuth;
    use crate::core::scheduler::ResourceDemand;
    use crate::core::stream::{CaptureHandle, SHARE_VID_HEADER_LEN};

    fn running(name: &str, port: u16, pid: u32) -> RunningInstance {
        RunningInstance {
            pid,
            ini_path: PathBuf::from(format!("/tmp/{name}-{pid}.ini")),
            avd_name: name.to_owned(),
            console_port: port,
            adb_port: port + 1,
            grpc_port: port + 3000,
            grpc_allowlist: None,
            grpc_jwks: None,
            grpc_jwk_active: None,
        }
    }

    fn device(instance: RunningInstance) -> DeviceData {
        DeviceData {
            name: instance.avd_name.clone(),
            path: PathBuf::from(format!("/tmp/{}.avd", instance.avd_name)),
            status: DevicePhase::Running,
            inst: Some(instance),
            resources: ResourceDemand::default(),
        }
    }

    fn fixture_bytes(counter: u32, pixel: [u8; 4]) -> Vec<u8> {
        let (width, height) = (2_u32, 2_u32);
        let mut bytes = vec![0; SHARE_VID_HEADER_LEN + (width * height * 4) as usize];
        bytes[0..4].copy_from_slice(&width.to_le_bytes());
        bytes[4..8].copy_from_slice(&height.to_le_bytes());
        bytes[8..12].copy_from_slice(&60_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&counter.to_le_bytes());
        bytes[16..24].copy_from_slice(&u64::from(counter).to_le_bytes());
        for chunk in bytes[SHARE_VID_HEADER_LEN..].chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel);
        }
        bytes
    }

    fn install_managed(
        runtime: &DeviceRuntime,
        instance: &RunningInstance,
        pixel: [u8; 4],
    ) -> PathBuf {
        let fixture = std::env::temp_dir().join(format!(
            "liteavd-workspace-{}-{}",
            std::process::id(),
            instance.avd_name
        ));
        std::fs::write(&fixture, fixture_bytes(instance.pid, pixel)).unwrap();
        let capture = CaptureHandle::start_path(&fixture).unwrap();

        let command = runtime.begin_start(&instance.avd_name).unwrap();
        let reservation = runtime.reserve_port([]).unwrap();
        assert_eq!(reservation.port(), instance.console_port);
        runtime
            .attach_start_port(&command, reservation.port())
            .unwrap();
        runtime.mark_booting(&command).unwrap();
        let auth = Arc::new(GrpcJwtAuth::new().unwrap());
        let mut launched = LaunchedInstance::test_managed(
            instance.clone(),
            auth,
            instance.pid + 10_000,
            PathBuf::from("/tmp/liteavd-workspace-sdk"),
            PathBuf::from(format!("/tmp/{}.log", instance.avd_name)),
        );
        launched.test_attach_capture(capture);
        runtime
            .complete_start(&command, launched, reservation)
            .unwrap();
        fixture
    }

    fn drain_main_context() {
        let context = glib::MainContext::default();
        for _ in 0..50 {
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn child_position(workspace: &gtk4::FlowBox, index: i32) -> (f32, f32) {
        let child = workspace.child_at_index(index).unwrap();
        let bounds = child.compute_bounds(workspace).expect("child bounds");
        (bounds.x(), bounds.y())
    }

    fn picture(workspace: &gtk4::FlowBox, index: i32) -> gtk4::Picture {
        let child = workspace.child_at_index(index).unwrap();
        find_named_widget(child.upcast_ref(), PICTURE_WIDGET)
            .expect("synthetic viewport picture")
            .downcast()
            .expect("picture type")
    }

    #[test]
    fn focused_device_shortcuts_are_explicit_and_do_not_accept_extra_modifiers() {
        use gtk4::gdk::ModifierType;

        assert_eq!(
            shortcut_action(None, Some('2'), ModifierType::CONTROL_MASK),
            Some(ShortcutAction::Focus(1))
        );
        assert_eq!(
            shortcut_action(
                None,
                Some('s'),
                ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK
            ),
            Some(ShortcutAction::Activate(
                crate::ui::device_controls::SCREENSHOT_WIDGET
            ))
        );
        assert_eq!(
            shortcut_action(Some("Left"), None, ModifierType::ALT_MASK),
            Some(ShortcutAction::Activate(
                crate::ui::device_controls::BACK_WIDGET
            ))
        );
        assert_eq!(
            shortcut_action(Some("Up"), None, ModifierType::CONTROL_MASK),
            Some(ShortcutAction::Activate(
                crate::ui::device_controls::VOLUME_UP_WIDGET
            ))
        );
        assert_eq!(
            shortcut_action(
                Some("Up"),
                None,
                ModifierType::CONTROL_MASK | ModifierType::ALT_MASK
            ),
            None
        );
    }

    #[test]
    #[ignore = "requires GTK display; run under Xvfb"]
    fn three_synthetic_cards_resize_focus_and_remove_independently() {
        gtk4::init().expect("GTK init");
        let tokio_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Tokio runtime");
        let _tokio_guard = tokio_runtime.enter();
        let first = running("phone", 5554, 1001);
        let second = running("tablet", 5556, 1002);
        let third = running("foldable", 5558, 1003);
        let runtime = Arc::new(DeviceRuntime::default());
        let microphone = MicrophoneController::new(runtime.clone());
        let fixtures = [
            install_managed(&runtime, &first, [0, 0, 255, 255]),
            install_managed(&runtime, &second, [0, 255, 0, 255]),
            install_managed(&runtime, &third, [255, 0, 0, 255]),
        ];
        let mut devices = vec![
            device(first.clone()),
            device(second.clone()),
            device(third.clone()),
        ];
        let workspace = build_from_devices(
            &devices,
            Path::new("/tmp"),
            runtime.clone(),
            microphone.clone(),
        );
        let scroll = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .child(&workspace)
            .build();
        let window = gtk4::Window::builder()
            .default_width(1180)
            .default_height(720)
            .child(&scroll)
            .build();
        window.present();
        drain_main_context();

        assert_eq!(child_names(&workspace).len(), 3);
        assert!((0..3).all(|index| picture(&workspace, index).paintable().is_some()));
        for index in 0..3 {
            let child = workspace.child_at_index(index).unwrap();
            let controls: gtk4::FlowBox = find_named_widget(
                child.upcast_ref(),
                crate::ui::device_controls::CONTROLS_WIDGET,
            )
            .expect("managed card quick controls")
            .downcast()
            .expect("quick controls type");
            assert!(controls.is_sensitive());
            let microphone = find_named_widget(
                child.upcast_ref(),
                crate::ui::device_controls::MICROPHONE_WIDGET,
            )
            .expect("reserved microphone toggle");
            assert!(!microphone.is_sensitive());
        }
        assert_eq!(workspace.max_children_per_line(), 3);
        let wide_positions: Vec<_> = (0..3)
            .map(|index| child_position(&workspace, index))
            .collect();
        assert!(
            wide_positions
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0 && pair[0].1 == pair[1].1),
            "wide positions: {wide_positions:?}; workspace={}x{} mapped={} child={}x{}",
            workspace.width(),
            workspace.height(),
            workspace.is_mapped(),
            workspace.child_at_index(0).unwrap().width(),
            workspace.child_at_index(0).unwrap().height(),
        );

        assert!(focus_index(&workspace, 1, &runtime));
        assert_eq!(
            runtime
                .workspace_snapshot()
                .focused
                .as_ref()
                .map(|route| route.avd_name.as_str()),
            Some("tablet")
        );
        assert_eq!(
            workspace
                .selected_children()
                .first()
                .and_then(child_avd_name)
                .as_deref(),
            Some("tablet")
        );

        window.close();
        drain_main_context();
        let workspace =
            build_from_devices(&devices, Path::new("/tmp"), runtime.clone(), microphone);
        workspace.allocate(420, 1200, -1, None);
        let narrow_positions: Vec<_> = (0..3)
            .map(|index| child_position(&workspace, index))
            .collect();
        assert!(
            narrow_positions
                .windows(2)
                .all(|pair| pair[0].0 == pair[1].0 && pair[0].1 < pair[1].1),
            "narrow positions: {narrow_positions:?}"
        );

        let first_picture = picture(&workspace, 0).downgrade();
        let third_picture = picture(&workspace, 2).downgrade();
        let first_route = runtime.input_route("phone").unwrap();
        let tablet_route = runtime.input_route("tablet").unwrap();
        let third_route = runtime.input_route("foldable").unwrap();
        let stop = runtime.begin_stop("tablet").unwrap();
        runtime.complete_stop(&stop).unwrap();
        devices[1].status = DevicePhase::Stopped;
        devices[1].inst = None;
        let middle = workspace.child_at_index(1).unwrap();
        device_list::apply_projection(middle.upcast_ref(), &devices[1], &runtime);
        sync_focused_child(&workspace, &runtime);
        assert!(find_named_widget(middle.upcast_ref(), PICTURE_WIDGET).is_none());
        assert!(first_picture.upgrade().is_some());
        assert!(third_picture.upgrade().is_some());
        assert!(first_route.is_current());
        assert!(!tablet_route.is_current());
        assert!(third_route.is_current());
        assert!(runtime.workspace_snapshot().focused.is_none());
        assert!(workspace.selected_children().is_empty());

        window.set_child(None::<&gtk4::Widget>);
        window.close();
        drain_main_context();
        drop(middle);
        drop(scroll);
        drop(workspace);
        drop(runtime);
        for fixture in fixtures {
            std::fs::remove_file(fixture).unwrap();
        }
    }
}
