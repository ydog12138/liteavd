//! WP-1.3：GdkMemoryTexture 像素与 GTK viewport 生命周期回归。
//!
//! 需要图形环境：
//! `DISPLAY=:97 cargo test --test gui_viewport_smoke -- --ignored --nocapture`

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk4::gdk;
use gtk4::prelude::*;
use liteavd::core::stream::{CaptureHandle, Frame, FrameMeta, SHARE_VID_HEADER_LEN};
use liteavd::ui::viewport::{
    ASPECT_WIDGET, PICTURE_WIDGET, STATUS_WIDGET, ViewportError, build, frame_texture,
};

fn frame(width: u32, height: u32, counter: u32, pixels: Vec<u8>) -> Arc<Frame> {
    Arc::new(Frame {
        meta: FrameMeta {
            width,
            height,
            fps: 60,
            frame_counter: counter,
            timestamp_ns: u64::from(counter) * 1_000,
            stride: width * 4,
        },
        pixels,
        observed_at: Instant::now(),
        copied_at: Instant::now(),
    })
}

fn fixture_bytes(width: u32, height: u32, counter: u32, pixel: [u8; 4]) -> Vec<u8> {
    let pixel_len = width as usize * height as usize * 4;
    let mut bytes = vec![0_u8; SHARE_VID_HEADER_LEN + pixel_len];
    bytes[0..4].copy_from_slice(&width.to_le_bytes());
    bytes[4..8].copy_from_slice(&height.to_le_bytes());
    bytes[8..12].copy_from_slice(&60_u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&counter.to_le_bytes());
    bytes[16..24].copy_from_slice(&(u64::from(counter) * 1_000).to_le_bytes());
    for chunk in bytes[SHARE_VID_HEADER_LEN..].chunks_exact_mut(4) {
        chunk.copy_from_slice(&pixel);
    }
    bytes
}

fn replace_fixture(path: &Path, bytes: &[u8]) {
    let temporary = path.with_extension("next");
    std::fs::write(&temporary, bytes).unwrap();
    std::fs::rename(temporary, path).unwrap();
}

fn find_named<T: IsA<gtk4::Widget> + glib::object::Cast>(root: &gtk4::Widget, name: &str) -> T {
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if widget.widget_name() == name {
            return widget
                .downcast::<T>()
                .unwrap_or_else(|_| panic!("{name} 类型不符"));
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            stack.push(next.clone());
            child = next.next_sibling();
        }
    }
    panic!("未找到 widget {name}");
}

fn drive_until(mut ready: impl FnMut() -> bool) {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready() {
        while context.pending() {
            context.iteration(false);
        }
        assert!(Instant::now() < deadline, "GTK 状态在 3 秒内未到达");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn drain_for(duration: Duration) {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        while context.pending() {
            context.iteration(false);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn bgra_texture_preserves_pixels_orientation_and_frame_lifetime() {
    let pixels = vec![
        0, 0, 255, 255, // top-left red
        0, 255, 0, 255, // top-right green
        255, 0, 0, 255, // bottom-left blue
        255, 255, 255, 255, // bottom-right white
    ];
    let source = frame(2, 2, 1, pixels.clone());
    let weak = Arc::downgrade(&source);
    let rendered = frame_texture(source.clone()).expect("frame 转 texture 失败");
    drop(source);
    assert!(weak.upgrade().is_some(), "texture 必须持有 Frame 内存");
    assert_eq!(rendered.aspect_ratio, 1.0);
    assert_eq!(
        (rendered.texture.width(), rendered.texture.height()),
        (2, 2)
    );

    let mut downloader = gdk::TextureDownloader::new(&rendered.texture);
    downloader.set_format(gdk::MemoryFormat::B8g8r8a8);
    let (downloaded, stride) = downloader.download_bytes();
    assert!(stride >= 8);
    assert_eq!(&downloaded.as_ref()[0..8], &pixels[0..8]);
    assert_eq!(&downloaded.as_ref()[stride..stride + 8], &pixels[8..16]);
    drop(downloaded);
    drop(downloader);
    drop(rendered);
    assert!(weak.upgrade().is_none(), "texture drop 应释放 Frame 内存");

    let invalid = frame(2, 2, 2, vec![0; 15]);
    assert!(matches!(
        frame_texture(invalid),
        Err(ViewportError::InvalidPixelLength { .. })
    ));
}

fn viewport_consumes_latest_only_when_mapped_and_detaches_cleanly() {
    let path = std::env::temp_dir().join(format!("liteavd-gui-viewport-{}", std::process::id()));
    replace_fixture(&path, &fixture_bytes(2, 2, 1, [0, 0, 255, 255]));
    let capture = CaptureHandle::start_path(&path).unwrap();
    let root = build(capture.subscribe());
    let root_weak = root.downgrade();
    let root_widget = root.clone().upcast::<gtk4::Widget>();
    let picture: gtk4::Picture = find_named(&root_widget, PICTURE_WIDGET);
    let aspect: gtk4::AspectFrame = find_named(&root_widget, ASPECT_WIDGET);
    let status: gtk4::Label = find_named(&root_widget, STATUS_WIDGET);

    let window = gtk4::Window::new();
    window.set_default_size(360, 360);
    window.set_child(Some(&root));
    window.present();
    drive_until(|| picture.paintable().is_some());
    assert!((aspect.ratio() - 1.0).abs() < f32::EPSILON);
    assert_eq!(picture.paintable().unwrap().intrinsic_width(), 2);

    window.set_visible(false);
    replace_fixture(&path, &fixture_bytes(3, 1, 2, [255, 0, 0, 255]));
    drain_for(Duration::from_millis(250));
    assert_eq!(
        picture.paintable().unwrap().intrinsic_width(),
        2,
        "unmapped viewport 不应上传 texture"
    );

    window.present();
    drive_until(|| {
        picture
            .paintable()
            .is_some_and(|paintable| paintable.intrinsic_width() == 3)
    });
    assert!((aspect.ratio() - 3.0).abs() < f32::EPSILON);

    drop(capture);
    drive_until(|| status.text() == "视频捕获已结束");

    window.set_child(None::<&gtk4::Widget>);
    drop(picture);
    drop(aspect);
    drop(status);
    drop(root_widget);
    drop(root);
    window.close();
    drain_for(Duration::from_millis(50));
    assert!(root_weak.upgrade().is_none(), "viewport 控件存在引用环");
    std::fs::remove_file(path).unwrap();
}

#[test]
#[ignore = "需要图形环境（Xvfb/Wayland）"]
fn viewport_smoke_suite() {
    gtk4::init().expect("gtk4 初始化失败");
    bgra_texture_preserves_pixels_orientation_and_frame_lifetime();
    viewport_consumes_latest_only_when_mapped_and_detaches_cleanly();
}
