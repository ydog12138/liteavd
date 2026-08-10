//! 创建向导：设备型号 → 系统镜像 → 硬件配置 → 创建。

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use gtk4::prelude::*;
use gtk4::{
    Box as GtkBox, Button, DropDown, Entry, Label, ListBox, ListBoxRow, SpinButton, Stack, Window,
};

use crate::core::avd::{self, AvdCreationConfig, AvdSpec, DeviceProfile, GpuMode};
use crate::core::repo::SystemImage;

struct WizardState {
    profile: Option<DeviceProfile>,
    image: Option<SystemImage>,
}

type SharedState = Arc<Mutex<WizardState>>;
type SharedImages = Arc<Mutex<Vec<SystemImage>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageIdentity {
    api: String,
    tag: String,
    abi: String,
}

impl From<&SystemImage> for ImageIdentity {
    fn from(image: &SystemImage) -> Self {
        Self {
            api: image.api.clone(),
            tag: image.tag.clone(),
            abi: image.abi.clone(),
        }
    }
}

fn lock_state(state: &SharedState) -> std::sync::MutexGuard<'_, WizardState> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}

fn render_image_rows(
    image_list: &ListBox,
    empty_label: &Label,
    images: Vec<SystemImage>,
    model: &SharedImages,
    state: &SharedState,
) {
    let preferred = lock_state(state).image.as_ref().map(ImageIdentity::from);
    while let Some(row) = image_list.row_at_index(0) {
        image_list.remove(&row);
    }
    *model.lock().unwrap_or_else(|error| error.into_inner()) = images.clone();
    for image in &images {
        let row = ListBoxRow::new();
        let box_ = GtkBox::new(gtk4::Orientation::Vertical, 2);
        let name = Label::new(Some(&image.display_name));
        name.set_xalign(0.0);
        box_.append(&name);
        let id = Label::new(Some(&format!("{}/{}", image.tag, image.abi)));
        id.add_css_class("caption");
        id.add_css_class("dim-label");
        id.set_xalign(0.0);
        box_.append(&id);
        row.set_child(Some(&box_));
        image_list.append(&row);
    }
    empty_label.set_visible(images.is_empty());

    let selected = preferred
        .as_ref()
        .and_then(|identity| {
            images
                .iter()
                .position(|image| ImageIdentity::from(image) == *identity)
        })
        .or_else(|| (images.len() == 1).then_some(0));
    if let Some(index) = selected {
        image_list.select_row(image_list.row_at_index(index as i32).as_ref());
    } else {
        image_list.unselect_all();
        lock_state(state).image = None;
    }
}

/// 打开创建向导（模态对话框）。
pub fn open(parent: &impl IsA<Window>, sdk_root: PathBuf, on_created: impl Fn() + 'static) {
    let win = Window::builder()
        .title("新建 AVD")
        .modal(true)
        .transient_for(parent)
        .default_width(540)
        .default_height(480)
        .build();
    win.set_widget_name("liteavd-create-wizard");

    let state: SharedState = Arc::new(Mutex::new(WizardState {
        profile: None,
        image: None,
    }));

    let stack = Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::SlideLeft);
    let creation_config = AvdCreationConfig::default();
    creation_config
        .validate()
        .expect("内置 AVD 创建配置必须有效");
    let ram_spin = SpinButton::with_range(
        avd::MIN_RAM_MB.into(),
        avd::MAX_RAM_MB.into(),
        avd::RAM_STEP_MB.into(),
    );
    ram_spin.set_value(avd::FALLBACK_RAM_MB.into());
    let disk_spin = SpinButton::with_range(
        avd::MIN_DATA_PARTITION_MB as f64,
        avd::MAX_DATA_PARTITION_MB as f64,
        avd::DATA_PARTITION_STEP_MB as f64,
    );
    disk_spin.set_value(creation_config.data_partition_mb as f64);
    let profiles = Rc::new(avd::builtin_profile_catalog().profiles);

    // 第 1 步：设备型号
    let step1 = GtkBox::new(gtk4::Orientation::Vertical, 8);
    let title1 = Label::new(Some("选择设备型号"));
    title1.add_css_class("title-2");
    step1.append(&title1);
    let profile_list = ListBox::new();
    for p in profiles.iter() {
        let row = ListBoxRow::new();
        let box_ = GtkBox::new(gtk4::Orientation::Vertical, 2);
        let name = Label::new(Some(&format!(
            "{}（{}×{} @{}dpi）",
            p.name, p.width, p.height, p.density
        )));
        name.set_xalign(0.0);
        box_.append(&name);
        let id = Label::new(Some(&p.id));
        id.add_css_class("caption");
        id.add_css_class("dim-label");
        id.set_xalign(0.0);
        box_.append(&id);
        row.set_child(Some(&box_));
        profile_list.append(&row);
    }
    {
        let state = state.clone();
        let profiles = profiles.clone();
        let ram_spin = ram_spin.clone();
        profile_list.connect_row_selected(move |_list, row| {
            if let Some(row) = row {
                let idx = row.index() as usize;
                let profile = profiles.get(idx).cloned();
                if let Some(profile) = &profile {
                    ram_spin.set_value(profile.default_ram_mb.into());
                }
                lock_state(&state).profile = profile;
            }
        });
    }
    step1.append(&profile_list);
    stack.add_titled(&step1, Some("step1"), "设备");

    // 第 2 步：系统镜像（本地已安装）
    let step2 = GtkBox::new(gtk4::Orientation::Vertical, 8);
    let title2 = Label::new(Some("选择系统镜像（本地已安装）"));
    title2.add_css_class("title-2");
    step2.append(&title2);
    let image_list = ListBox::new();
    image_list.set_widget_name("liteavd-create-image-list");
    let image_model: SharedImages = Arc::new(Mutex::new(Vec::new()));
    {
        let state = state.clone();
        let images = image_model.clone();
        image_list.connect_row_selected(move |_list, row| {
            if let Some(row) = row {
                let idx = row.index() as usize;
                lock_state(&state).image = images
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(idx)
                    .cloned();
            }
        });
    }
    let no_images = Label::new(Some(
        "本地没有完整的系统镜像（需要 system.img 与 source.properties）",
    ));
    no_images.add_css_class("error");
    render_image_rows(
        &image_list,
        &no_images,
        avd::scan_installed_images(&sdk_root),
        &image_model,
        &state,
    );
    step2.append(&no_images);
    step2.append(&image_list);
    let manage_images = Button::with_label("管理/安装镜像");
    manage_images.set_widget_name("liteavd-create-manage-images");
    {
        let win = win.downgrade();
        let sdk = sdk_root.clone();
        let image_list = glib::SendWeakRef::from(image_list.downgrade());
        let no_images = glib::SendWeakRef::from(no_images.downgrade());
        let image_model = image_model.clone();
        let state = state.clone();
        manage_images.connect_clicked(move |_| {
            let Some(win) = win.upgrade() else {
                return;
            };
            let sdk_for_refresh = sdk.clone();
            let image_list = image_list.clone();
            let no_images = no_images.clone();
            let image_model = image_model.clone();
            let state = state.clone();
            let refresh: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let images = avd::scan_installed_images(&sdk_for_refresh);
                let image_list = image_list.clone();
                let no_images = no_images.clone();
                let image_model = image_model.clone();
                let state = state.clone();
                glib::MainContext::default().invoke(move || {
                    if let (Some(image_list), Some(no_images)) =
                        (image_list.upgrade(), no_images.upgrade())
                    {
                        render_image_rows(&image_list, &no_images, images, &image_model, &state);
                    }
                });
            });
            crate::ui::images_page::open_for_sdk(&win, sdk.clone(), refresh);
        });
    }
    step2.append(&manage_images);
    stack.add_titled(&step2, Some("step2"), "镜像");

    // 第 3 步：硬件配置
    let step3 = GtkBox::new(gtk4::Orientation::Vertical, 8);
    let title3 = Label::new(Some("硬件配置"));
    title3.add_css_class("title-2");
    step3.append(&title3);

    let name_entry = Entry::new();
    name_entry.set_placeholder_text(Some("设备名称（如 Pixel_2_API_35）"));
    let box_name = GtkBox::new(gtk4::Orientation::Horizontal, 8);
    box_name.append(&Label::new(Some("名称")));
    box_name.append(&name_entry);
    step3.append(&box_name);

    let box_ram = GtkBox::new(gtk4::Orientation::Horizontal, 8);
    box_ram.append(&Label::new(Some("内存 (MB)")));
    box_ram.append(&ram_spin);
    step3.append(&box_ram);

    let box_disk = GtkBox::new(gtk4::Orientation::Horizontal, 8);
    box_disk.append(&Label::new(Some("数据分区 (MB)")));
    box_disk.append(&disk_spin);
    step3.append(&box_disk);

    let gpu_labels: Vec<_> = GpuMode::CREATION_CHOICES
        .iter()
        .map(GpuMode::as_str)
        .collect();
    let gpu_model = gtk4::StringList::new(&gpu_labels);
    let gpu_combo = DropDown::new(Some(gpu_model), None::<&gtk4::Expression>);
    gpu_combo.set_selected(
        GpuMode::CREATION_CHOICES
            .iter()
            .position(|mode| *mode == creation_config.gpu)
            .unwrap_or(0) as u32,
    );
    let box_gpu = GtkBox::new(gtk4::Orientation::Horizontal, 8);
    box_gpu.append(&Label::new(Some("GPU 模式")));
    box_gpu.append(&gpu_combo);
    step3.append(&box_gpu);

    let error = Label::new(None);
    error.add_css_class("error");
    step3.append(&error);
    stack.add_titled(&step3, Some("step3"), "配置");

    // 导航
    let page = Rc::new(Cell::new(0usize));
    let pages = 3usize;
    let back = Button::with_label("上一步");
    let next = Button::with_label("下一步");
    next.add_css_class("suggested-action");
    let nav = GtkBox::new(gtk4::Orientation::Horizontal, 8);
    nav.set_halign(gtk4::Align::End);
    nav.append(&back);
    nav.append(&next);

    let move_to: Rc<dyn Fn()> = Rc::new({
        let stack = stack.clone();
        let back = back.clone();
        let next = next.clone();
        let page = page.clone();
        move || {
            let p = page.get();
            stack.set_visible_child_name(&format!("step{}", p + 1));
            back.set_sensitive(p > 0);
            next.set_label(if p + 1 == pages {
                "创建"
            } else {
                "下一步"
            });
        }
    });
    move_to();

    {
        let page = page.clone();
        let move_to = move_to.clone();
        back.connect_clicked(move |_| {
            if page.get() > 0 {
                page.set(page.get() - 1);
                move_to();
            }
        });
    }
    {
        let page = page.clone();
        let move_to = move_to.clone();
        let state = state.clone();
        let win = win.clone();
        let on_created = Rc::new(on_created);
        next.connect_clicked(move |_| {
            if page.get() + 1 < pages {
                page.set(page.get() + 1);
                move_to();
                return;
            }
            let Some(profile) = lock_state(&state).profile.clone() else {
                error.set_text("请选择设备型号");
                return;
            };
            let Some(image) = lock_state(&state).image.clone() else {
                error.set_text("请选择系统镜像");
                return;
            };
            if let Err(problem) = avd::validate_installed_image(&sdk_root, &image) {
                error.set_text(&format!("镜像不可用：{problem:#}"));
                return;
            }
            let name = name_entry.text().to_string();
            if name.trim().is_empty() {
                error.set_text("请输入设备名称");
                return;
            }
            let spec = AvdSpec {
                name: name.trim().to_string(),
                device: profile,
                image,
                ram_mb: ram_spin.value() as u32,
                data_partition_mb: disk_spin.value() as u64,
                sdcard: creation_config.sdcard.clone(),
                gpu: GpuMode::CREATION_CHOICES
                    .get(gpu_combo.selected() as usize)
                    .copied()
                    .unwrap_or(GpuMode::Auto),
            };
            match avd::create_avd(&spec) {
                Ok(_) => {
                    win.close();
                    on_created();
                }
                Err(e) => error.set_text(&format!("创建失败：{e:#}")),
            }
        });
    }

    let content = GtkBox::new(gtk4::Orientation::Vertical, 8);
    content.append(&stack);
    content.append(&nav);
    win.set_child(Some(&content));

    win.present();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::repo::Archive;

    fn image(api: &str, tag: &str, abi: &str) -> SystemImage {
        SystemImage {
            api: api.into(),
            tag: tag.into(),
            abi: abi.into(),
            display_name: format!("{api} {tag} {abi}"),
            license_ids: Vec::new(),
            archive: Archive {
                url: String::new(),
                size: 0,
                checksum: None,
                host_os: None,
                host_arch: None,
            },
        }
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

    #[test]
    #[ignore = "requires GTK display; run under Xvfb"]
    fn image_refresh_restores_selection_by_identity() {
        gtk4::init().expect("GTK 初始化失败");
        let selected = image("android-35", "google_apis", "x86_64");
        let state = Arc::new(Mutex::new(WizardState {
            profile: None,
            image: Some(selected.clone()),
        }));
        let model = Arc::new(Mutex::new(Vec::new()));
        let list = ListBox::new();
        let empty = Label::new(None);

        render_image_rows(
            &list,
            &empty,
            vec![image("android-34", "default", "x86_64"), selected.clone()],
            &model,
            &state,
        );
        assert_eq!(list.selected_row().map(|row| row.index()), Some(1));

        render_image_rows(
            &list,
            &empty,
            vec![
                image("android-36", "google_apis", "x86_64"),
                selected.clone(),
                image("android-34", "default", "x86_64"),
            ],
            &model,
            &state,
        );
        assert_eq!(list.selected_row().map(|row| row.index()), Some(1));
        assert_eq!(
            lock_state(&state).image.as_ref().map(ImageIdentity::from),
            Some(ImageIdentity::from(&selected))
        );

        let sdk =
            std::env::temp_dir().join(format!("liteavd-create-wizard-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sdk);
        std::fs::create_dir_all(&sdk).unwrap();
        let parent = Window::new();
        open(&parent, sdk.clone(), || {});
        let wizard = Window::list_toplevels()
            .into_iter()
            .filter_map(|widget| widget.downcast::<Window>().ok())
            .find(|window| window.widget_name() == "liteavd-create-wizard")
            .expect("创建向导窗口不存在");
        assert!(find_button(wizard.upcast_ref(), "管理/安装镜像").is_some());
        wizard.close();
        let _ = std::fs::remove_dir_all(sdk);
    }
}
