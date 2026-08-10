//! 与 GTK 无关的 viewport 坐标映射和单触点状态机。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportTransform {
    guest_width: u32,
    guest_height: u32,
    content_x: f64,
    content_y: f64,
    content_width: f64,
    content_height: f64,
}

impl ViewportTransform {
    pub fn new(
        viewport_width: f64,
        viewport_height: f64,
        guest_width: u32,
        guest_height: u32,
    ) -> Option<Self> {
        if !viewport_width.is_finite()
            || !viewport_height.is_finite()
            || viewport_width <= 0.0
            || viewport_height <= 0.0
            || guest_width == 0
            || guest_height == 0
        {
            return None;
        }
        let scale = (viewport_width / f64::from(guest_width))
            .min(viewport_height / f64::from(guest_height));
        let content_width = f64::from(guest_width) * scale;
        let content_height = f64::from(guest_height) * scale;
        Some(Self {
            guest_width,
            guest_height,
            content_x: (viewport_width - content_width) / 2.0,
            content_y: (viewport_height - content_height) / 2.0,
            content_width,
            content_height,
        })
    }

    /// 映射内容区内的点；letterbox 外返回 `None`。
    pub fn map(&self, x: f64, y: f64) -> Option<GuestPoint> {
        if !x.is_finite()
            || !y.is_finite()
            || x < self.content_x
            || y < self.content_y
            || x > self.content_x + self.content_width
            || y > self.content_y + self.content_height
        {
            return None;
        }
        Some(self.map_content_point(x, y))
    }

    /// 将任意有限点夹到内容边缘后映射；用于已开始触摸的 move/release。
    pub fn map_clamped(&self, x: f64, y: f64) -> Option<GuestPoint> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        Some(self.map_content_point(
            x.clamp(self.content_x, self.content_x + self.content_width),
            y.clamp(self.content_y, self.content_y + self.content_height),
        ))
    }

    fn map_content_point(&self, x: f64, y: f64) -> GuestPoint {
        let guest_x = ((x - self.content_x) / self.content_width * f64::from(self.guest_width))
            .floor()
            .clamp(0.0, f64::from(self.guest_width - 1));
        let guest_y = ((y - self.content_y) / self.content_height * f64::from(self.guest_height))
            .floor()
            .clamp(0.0, f64::from(self.guest_height - 1));
        GuestPoint {
            x: guest_x as i32,
            y: guest_y as i32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchSample {
    pub point: GuestPoint,
    pub identifier: i32,
    pub pressure: i32,
}

/// 产品快捷控制对应的 Emulator W3C/Android 按键。
///
/// 使用枚举限制 UI 可发送的硬件操作，避免把任意字符串绕过输入边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKey {
    Back,
    Home,
    AppSwitch,
    Power,
    VolumeDown,
    VolumeMute,
    VolumeUp,
}

impl DeviceKey {
    pub const fn grpc_key(self) -> &'static str {
        match self {
            Self::Back => "GoBack",
            Self::Home => "GoHome",
            Self::AppSwitch => "AppSwitch",
            Self::Power => "Power",
            Self::VolumeDown => "AudioVolumeDown",
            Self::VolumeMute => "AudioVolumeMute",
            Self::VolumeUp => "AudioVolumeUp",
        }
    }
}

/// 单触点 lifecycle。只有 press 可从内容区外被忽略；活动触点始终可靠 release。
#[derive(Debug, Default)]
pub struct TouchTracker {
    active: Option<GuestPoint>,
}

impl TouchTracker {
    pub const PRIMARY_IDENTIFIER: i32 = 0;
    pub const PRESSED_PRESSURE: i32 = 1;

    pub fn press(&mut self, transform: &ViewportTransform, x: f64, y: f64) -> Option<TouchSample> {
        if self.active.is_some() {
            return None;
        }
        let point = transform.map(x, y)?;
        self.active = Some(point);
        Some(sample(point, Self::PRESSED_PRESSURE))
    }

    pub fn move_to(
        &mut self,
        transform: &ViewportTransform,
        x: f64,
        y: f64,
    ) -> Option<TouchSample> {
        self.active?;
        let point = transform.map_clamped(x, y)?;
        if self.active == Some(point) {
            return None;
        }
        self.active = Some(point);
        Some(sample(point, Self::PRESSED_PRESSURE))
    }

    pub fn release(
        &mut self,
        transform: &ViewportTransform,
        x: f64,
        y: f64,
    ) -> Option<TouchSample> {
        let last = self.active.take()?;
        let point = transform.map_clamped(x, y).unwrap_or(last);
        Some(sample(point, 0))
    }

    pub fn cancel(&mut self) -> Option<TouchSample> {
        self.active.take().map(|point| sample(point, 0))
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

fn sample(point: GuestPoint, pressure: i32) -> TouchSample {
    TouchSample {
        point,
        identifier: TouchTracker::PRIMARY_IDENTIFIER,
        pressure,
    }
}

/// GDK key name → emulator 支持的 W3C/Android 导航 key。
pub fn navigation_key(gdk_name: &str) -> Option<&'static str> {
    match gdk_name {
        "Return" | "KP_Enter" | "ISO_Enter" => Some("Enter"),
        "BackSpace" => Some("Backspace"),
        "Delete" | "KP_Delete" => Some("Delete"),
        "Tab" | "ISO_Left_Tab" => Some("Tab"),
        "Left" | "KP_Left" => Some("ArrowLeft"),
        "Right" | "KP_Right" => Some("ArrowRight"),
        "Up" | "KP_Up" => Some("ArrowUp"),
        "Down" | "KP_Down" => Some("ArrowDown"),
        "Page_Up" | "KP_Page_Up" => Some("PageUp"),
        "Page_Down" | "KP_Page_Down" => Some("PageDown"),
        "Escape" | "XF86Back" => Some("GoBack"),
        "Home" | "KP_Home" | "XF86HomePage" => Some("GoHome"),
        "Menu" => Some("AppSwitch"),
        "XF86PowerOff" => Some("Power"),
        "XF86AudioLowerVolume" => Some("AudioVolumeDown"),
        "XF86AudioMute" => Some("AudioVolumeMute"),
        "XF86AudioRaiseVolume" => Some("AudioVolumeUp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portrait_mapping_rejects_letterbox_and_maps_edges() {
        let transform = ViewportTransform::new(1000.0, 1000.0, 100, 200).unwrap();
        assert_eq!(
            transform.map(500.0, 500.0),
            Some(GuestPoint { x: 50, y: 100 })
        );
        assert_eq!(transform.map(249.0, 500.0), None);
        assert_eq!(transform.map(250.0, 0.0), Some(GuestPoint { x: 0, y: 0 }));
        assert_eq!(
            transform.map(750.0, 1000.0),
            Some(GuestPoint { x: 99, y: 199 })
        );
        assert_eq!(
            transform.map_clamped(-500.0, 2000.0),
            Some(GuestPoint { x: 0, y: 199 })
        );
    }

    #[test]
    fn landscape_mapping_handles_vertical_letterbox() {
        let transform = ViewportTransform::new(400.0, 400.0, 200, 100).unwrap();
        assert_eq!(
            transform.map(200.0, 100.0),
            Some(GuestPoint { x: 100, y: 0 })
        );
        assert_eq!(transform.map(200.0, 99.0), None);
        assert_eq!(
            transform.map(400.0, 300.0),
            Some(GuestPoint { x: 199, y: 99 })
        );
    }

    #[test]
    fn touch_tracker_always_releases_and_clamps_active_drag() {
        let transform = ViewportTransform::new(100.0, 100.0, 100, 100).unwrap();
        let mut tracker = TouchTracker::default();
        assert!(tracker.press(&transform, -1.0, 50.0).is_none());
        let down = tracker.press(&transform, 10.0, 20.0).unwrap();
        assert_eq!(down.pressure, 1);
        assert!(tracker.is_active());
        let moved = tracker.move_to(&transform, 500.0, -10.0).unwrap();
        assert_eq!(moved.point, GuestPoint { x: 99, y: 0 });
        let up = tracker.release(&transform, f64::NAN, f64::NAN).unwrap();
        assert_eq!(up.point, moved.point);
        assert_eq!(up.pressure, 0);
        assert!(!tracker.is_active());
        assert!(tracker.cancel().is_none());
    }

    #[test]
    fn cancel_uses_last_known_point_and_navigation_map_is_explicit() {
        let transform = ViewportTransform::new(10.0, 10.0, 10, 10).unwrap();
        let mut tracker = TouchTracker::default();
        tracker.press(&transform, 3.0, 4.0).unwrap();
        let cancel = tracker.cancel().unwrap();
        assert_eq!(cancel.point, GuestPoint { x: 3, y: 4 });
        assert_eq!(cancel.pressure, 0);
        assert_eq!(navigation_key("Escape"), Some("GoBack"));
        assert_eq!(navigation_key("Left"), Some("ArrowLeft"));
        assert_eq!(
            navigation_key("XF86AudioLowerVolume"),
            Some("AudioVolumeDown")
        );
        assert_eq!(navigation_key("XF86AudioMute"), Some("AudioVolumeMute"));
        assert_eq!(
            navigation_key("XF86AudioRaiseVolume"),
            Some("AudioVolumeUp")
        );
        assert_eq!(navigation_key("a"), None);
    }

    #[test]
    fn device_keys_map_to_bounded_emulator_keys() {
        assert_eq!(DeviceKey::Back.grpc_key(), "GoBack");
        assert_eq!(DeviceKey::Home.grpc_key(), "GoHome");
        assert_eq!(DeviceKey::AppSwitch.grpc_key(), "AppSwitch");
        assert_eq!(DeviceKey::Power.grpc_key(), "Power");
        assert_eq!(DeviceKey::VolumeDown.grpc_key(), "AudioVolumeDown");
        assert_eq!(DeviceKey::VolumeMute.grpc_key(), "AudioVolumeMute");
        assert_eq!(DeviceKey::VolumeUp.grpc_key(), "AudioVolumeUp");
    }
}
