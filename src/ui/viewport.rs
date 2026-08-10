//! GTK 主线程上的 share-vid 视口。
//!
//! capture 线程只发布 `Arc<Frame>`；本模块用 4ms GTK 主上下文泵消费最新帧，并让
//! `GdkMemoryTexture` 通过 `glib::Bytes` 持有同一 Arc，避免第二次像素复制。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use glib::ControlFlow;
use gtk4::gdk;
use gtk4::prelude::*;
use thiserror::Error;

use crate::core::instance::InputRouteGuard;
use crate::core::stream::{BYTES_PER_PIXEL, CaptureSubscription, Frame};
use crate::core::telemetry::LatencyProbe;
use crate::core::{grpc::GrpcClient, stream::FrameMeta};

pub const VIEWPORT_WIDGET: &str = "liteavd-viewport";
pub const PICTURE_WIDGET: &str = "liteavd-viewport-picture";
pub const ASPECT_WIDGET: &str = "liteavd-viewport-aspect";
pub const STATUS_WIDGET: &str = "liteavd-viewport-status";

const DEFAULT_RATIO: f32 = 9.0 / 16.0;
const VIEWPORT_HEIGHT: i32 = 420;
const FRAME_PUMP_INTERVAL: Duration = Duration::from_millis(4);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ViewportError {
    #[error("视口帧尺寸不能为零：{width}x{height}")]
    ZeroDimensions { width: u32, height: u32 },
    #[error("视口帧尺寸无法传给 GDK：{width}x{height}")]
    DimensionsOutOfRange { width: u32, height: u32 },
    #[error("视口 stride 不符：期望 {expected}B，实际 {actual}B")]
    InvalidStride { expected: usize, actual: u32 },
    #[error("视口像素长度不符：期望 {expected}B，实际 {actual}B")]
    InvalidPixelLength { expected: usize, actual: usize },
}

/// 已转换的 GDK 帧。texture 通过 `FrameBytes` 保持原始 `Arc<Frame>` 存活。
#[derive(Debug)]
pub struct FrameTexture {
    pub texture: gdk::MemoryTexture,
    pub aspect_ratio: f32,
    pub meta: FrameMeta,
    pub observed_at: std::time::Instant,
    pub copied_at: std::time::Instant,
}

#[derive(Debug)]
pub struct InteractiveViewport {
    pub root: gtk4::Box,
    pub telemetry: LatencyProbe,
}

#[derive(Debug)]
struct FrameBytes(Arc<Frame>);

impl AsRef<[u8]> for FrameBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0.pixels
    }
}

/// 在 GTK 主线程把一帧转换为无额外像素复制的 BGRA texture。
pub fn frame_texture(frame: Arc<Frame>) -> Result<FrameTexture, ViewportError> {
    let width = frame.meta.width;
    let height = frame.meta.height;
    if width == 0 || height == 0 {
        return Err(ViewportError::ZeroDimensions { width, height });
    }
    let width_i32 =
        i32::try_from(width).map_err(|_| ViewportError::DimensionsOutOfRange { width, height })?;
    let height_i32 =
        i32::try_from(height).map_err(|_| ViewportError::DimensionsOutOfRange { width, height })?;
    let expected_stride = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(BYTES_PER_PIXEL))
        .ok_or(ViewportError::DimensionsOutOfRange { width, height })?;
    if usize::try_from(frame.meta.stride).ok() != Some(expected_stride) {
        return Err(ViewportError::InvalidStride {
            expected: expected_stride,
            actual: frame.meta.stride,
        });
    }
    let expected_len = expected_stride
        .checked_mul(height as usize)
        .ok_or(ViewportError::DimensionsOutOfRange { width, height })?;
    if frame.pixels.len() != expected_len {
        return Err(ViewportError::InvalidPixelLength {
            expected: expected_len,
            actual: frame.pixels.len(),
        });
    }

    let meta = frame.meta;
    let observed_at = frame.observed_at;
    let copied_at = frame.copied_at;
    let bytes = glib::Bytes::from_owned(FrameBytes(frame));
    let texture = gdk::MemoryTexture::new(
        width_i32,
        height_i32,
        gdk::MemoryFormat::B8g8r8a8,
        &bytes,
        expected_stride,
    );
    Ok(FrameTexture {
        texture,
        aspect_ratio: width as f32 / height as f32,
        meta,
        observed_at,
        copied_at,
    })
}

/// 构建一个只消费给定订阅的 viewport。移除返回的 root 会释放订阅并停止帧泵。
pub fn build(subscription: CaptureSubscription) -> gtk4::Box {
    build_inner(subscription, None, None, None)
}

/// 构建绑定到同一 managed session gRPC client 的可交互 viewport。
pub fn build_interactive(subscription: CaptureSubscription, client: GrpcClient) -> gtk4::Box {
    build_interactive_measured(subscription, client).root
}

/// 构建可交互 viewport，并返回其有界延迟观测器。
pub fn build_interactive_measured(
    subscription: CaptureSubscription,
    client: GrpcClient,
) -> InteractiveViewport {
    let telemetry = LatencyProbe::default();
    let root = build_inner(subscription, Some(client), Some(telemetry.clone()), None);
    InteractiveViewport { root, telemetry }
}

/// 构建绑定到确切 session/generation 的交互 viewport；路由失效即停止输入 worker。
pub fn build_routed_interactive(
    subscription: CaptureSubscription,
    client: GrpcClient,
    route: InputRouteGuard,
) -> gtk4::Box {
    let telemetry = LatencyProbe::default();
    build_inner(subscription, Some(client), Some(telemetry), Some(route))
}

fn build_inner(
    subscription: CaptureSubscription,
    client: Option<GrpcClient>,
    telemetry: Option<LatencyProbe>,
    route: Option<InputRouteGuard>,
) -> gtk4::Box {
    let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    root.set_widget_name(VIEWPORT_WIDGET);
    root.add_css_class("card");
    root.set_hexpand(true);
    root.set_height_request(VIEWPORT_HEIGHT);

    let picture = gtk4::Picture::new();
    picture.set_widget_name(PICTURE_WIDGET);
    picture.set_content_fit(gtk4::ContentFit::Contain);
    picture.set_can_shrink(true);
    picture.set_hexpand(true);
    picture.set_vexpand(true);
    let root_weak = root.downgrade();
    picture.connect_has_focus_notify(move |picture| {
        if let Some(root) = root_weak.upgrade() {
            if picture.has_focus() {
                root.add_css_class("accent");
            } else {
                root.remove_css_class("accent");
            }
        }
    });

    let aspect = gtk4::AspectFrame::new(0.5, 0.5, DEFAULT_RATIO, false);
    aspect.set_widget_name(ASPECT_WIDGET);
    aspect.set_hexpand(true);
    aspect.set_vexpand(true);
    aspect.set_child(Some(&picture));

    let status = gtk4::Label::new(Some("等待视频帧…"));
    status.set_widget_name(STATUS_WIDGET);
    status.add_css_class("dim-label");

    let stack = gtk4::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.add_named(&status, Some("status"));
    stack.add_named(&aspect, Some("video"));
    stack.set_visible_child_name("status");
    root.append(&stack);

    let subscription = RefCell::new(subscription);
    let frame_meta: Rc<RefCell<Option<FrameMeta>>> = Rc::new(RefCell::new(None));
    let input_telemetry = telemetry.clone();
    let input_binding = client.and_then(|client| {
        match crate::ui::input::attach(
            &picture,
            client,
            frame_meta.clone(),
            input_telemetry.expect("interactive viewport has telemetry"),
            route,
        ) {
            Ok(binding) => Some(binding),
            Err(error) => {
                crate::core::settings::emit(
                    crate::core::settings::AppLogLevel::Warn,
                    format_args!("初始化 viewport 输入失败：{error}"),
                );
                status.set_tooltip_text(Some(&error.to_string()));
                None
            }
        }
    });
    let picture_weak = picture.downgrade();
    let aspect_weak = aspect.downgrade();
    let stack_weak = stack.downgrade();
    let status_weak = status.downgrade();
    let root_weak = root.downgrade();
    let _viewport_source = glib::timeout_add_local(FRAME_PUMP_INTERVAL, move || {
        let _keep_input_alive = &input_binding;
        let Some(root) = root_weak.upgrade() else {
            return ControlFlow::Break;
        };
        if !root.is_mapped() {
            return ControlFlow::Continue;
        }
        if let Some(telemetry) = &telemetry {
            telemetry.record_ui_pump(std::time::Instant::now());
        }

        let frame = subscription.borrow_mut().take_latest();
        if let Some(frame) = frame {
            match frame_texture(frame) {
                Ok(rendered) => {
                    let Some(picture) = picture_weak.upgrade() else {
                        return ControlFlow::Break;
                    };
                    let Some(aspect) = aspect_weak.upgrade() else {
                        return ControlFlow::Break;
                    };
                    let Some(stack) = stack_weak.upgrade() else {
                        return ControlFlow::Break;
                    };
                    aspect.set_ratio(rendered.aspect_ratio);
                    picture.set_paintable(Some(&rendered.texture));
                    *frame_meta.borrow_mut() = Some(rendered.meta);
                    stack.set_visible_child_name("video");
                    if let Some(telemetry) = &telemetry {
                        telemetry.record_frame_commit(
                            rendered.meta.frame_counter,
                            rendered.observed_at,
                            rendered.copied_at,
                            std::time::Instant::now(),
                        );
                    }
                }
                Err(error) => {
                    *frame_meta.borrow_mut() = None;
                    let Some(status) = status_weak.upgrade() else {
                        return ControlFlow::Break;
                    };
                    let Some(stack) = stack_weak.upgrade() else {
                        return ControlFlow::Break;
                    };
                    status.set_label(&format!("视频帧无效：{error}"));
                    stack.set_visible_child_name("status");
                }
            }
            return ControlFlow::Continue;
        }

        if subscription.borrow().is_closed() {
            *frame_meta.borrow_mut() = None;
            if let (Some(status), Some(stack)) = (status_weak.upgrade(), stack_weak.upgrade()) {
                status.set_label("视频捕获已结束");
                stack.set_visible_child_name("status");
            }
            ControlFlow::Break
        } else {
            ControlFlow::Continue
        }
    });

    root
}
