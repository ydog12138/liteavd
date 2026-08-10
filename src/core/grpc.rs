//! gRPC 客户端（tonic）：连接模拟器广告文件提供的 grpc 端口。
//! vendored proto：emulator 37.1.11.0 (15917651) 自带定义。

pub mod android {
    pub mod emulation {
        #[allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
        pub mod control {
            include!(concat!(env!("OUT_DIR"), "/android.emulation.control.rs"));
        }
        #[allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
        pub mod control_v2 {
            include!(concat!(env!("OUT_DIR"), "/android.emulation.control.v2.rs"));
        }
        #[allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
        pub mod stats {
            include!(concat!(env!("OUT_DIR"), "/android.emulation.stats.rs"));
        }
        #[allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
        pub mod bluetooth {
            include!(concat!(env!("OUT_DIR"), "/android.emulation.bluetooth.rs"));
        }
        #[allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
        pub mod remote {
            include!(concat!(env!("OUT_DIR"), "/android.emulation.remote.rs"));
        }
    }
}

#[allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
pub mod emulator_snapshot {
    include!(concat!(env!("OUT_DIR"), "/emulator_snapshot.rs"));
}

use anyhow::Context;
use std::sync::Arc;
use std::time::Duration;
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::core::grpc_auth::GrpcJwtAuth;

pub use self::android::emulation::control::{
    AudioFormat, AudioPacket, EmulatorStatus, Image, ImageFormat, ImageTransport, KeyboardEvent,
    MicrophoneState, MouseEvent, SnapshotDetails, SnapshotPackage, Touch, TouchEvent,
    audio_format::{
        Channels as AudioChannels, DeliveryMode as AudioDeliveryMode,
        SampleFormat as AudioSampleFormat,
    },
    emulator_controller_client::EmulatorControllerClient,
    keyboard_event::{KeyCodeType, KeyEventType},
    snapshot_service_client::SnapshotServiceClient,
};

pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(10);
pub const INPUT_RPC_TIMEOUT: Duration = Duration::from_secs(2);
pub const AUDIO_STREAM_ESTABLISH_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_INPUT_TEXT_BYTES: usize = 1024;
pub const MAX_SNAPSHOT_ID_BYTES: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum AudioStreamConnectError {
    #[error("gRPC client 尚未连接：{0}")]
    Disconnected(String),
    #[error("streamAudio 建链/首包等待超过 {AUDIO_STREAM_ESTABLISH_TIMEOUT:?}")]
    Timeout,
    #[error("streamAudio 建链失败（{code:?}）：{message}")]
    Rpc { code: tonic::Code, message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum InputRpcError {
    #[error("输入文本超过 {MAX_INPUT_TEXT_BYTES}B：实际 {actual}B")]
    TextTooLarge { actual: usize },
    #[error("输入 key 不能为空或超过 64B")]
    InvalidKey,
    #[error("{operation} gRPC 失败（{code:?}）：{message}")]
    Rpc {
        operation: &'static str,
        code: tonic::Code,
        message: String,
    },
    #[error("gRPC client 尚未连接：{0}")]
    Disconnected(String),
}

#[derive(Clone)]
struct JwtInterceptor {
    auth: Arc<GrpcJwtAuth>,
    rpc_timeout: Option<Duration>,
}

impl Interceptor for JwtInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        let token = self
            .auth
            .bearer_token()
            .map_err(|error| tonic::Status::internal(format!("生成 gRPC JWT 失败：{error}")))?;
        let value = MetadataValue::try_from(format!("Bearer {token}"))
            .map_err(|_| tonic::Status::internal("gRPC JWT 无法编码为 metadata"))?;
        request.metadata_mut().insert("authorization", value);
        if let Some(timeout) = self.rpc_timeout {
            request.set_timeout(timeout);
        }
        Ok(request)
    }
}

type AuthenticatedChannel = InterceptedService<Channel, JwtInterceptor>;

/// 模拟器 gRPC 客户端：只接受对应 managed session 的 JWT 身份。
#[derive(Clone)]
pub struct GrpcClient {
    channel: Option<Channel>,
    endpoint: String,
    auth: Arc<GrpcJwtAuth>,
    rpc_timeout: Duration,
}

impl std::fmt::Debug for GrpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrpcClient")
            .field("rpc_timeout", &self.rpc_timeout)
            .finish_non_exhaustive()
    }
}

impl GrpcClient {
    /// 连接受 JWT 保护、只监听 loopback 的模拟器控制面。
    pub async fn connect(grpc_port: u16, auth: Arc<GrpcJwtAuth>) -> anyhow::Result<Self> {
        let endpoint = format!("http://127.0.0.1:{grpc_port}");
        let channel = Channel::from_shared(endpoint.clone())
            .context("gRPC 地址非法")?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await
            .context("gRPC 连接失败")?;
        Ok(Self {
            channel: Some(channel),
            endpoint,
            auth,
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
        })
    }

    /// 为应用重启后恢复的 session 建立轻量句柄。真正 RPC 所在 worker 会调用
    /// `reconnect`，因此这里不把 transport task 绑定到 GTK 主线程。
    pub(crate) fn reconnect_config(grpc_port: u16, auth: Arc<GrpcJwtAuth>) -> anyhow::Result<Self> {
        let endpoint = format!("http://127.0.0.1:{grpc_port}");
        Channel::from_shared(endpoint.clone()).context("gRPC 地址非法")?;
        Ok(Self {
            channel: None,
            endpoint,
            auth,
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
        })
    }

    #[cfg(test)]
    pub(crate) fn test_client(auth: Arc<GrpcJwtAuth>) -> Self {
        Self {
            channel: None,
            endpoint: "http://127.0.0.1:1".to_owned(),
            auth,
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
        }
    }

    /// tonic channel 的 transport task 归创建它的 Tokio runtime；跨 runtime
    /// worker 必须在自己的 runtime 内重连，而不是直接复用旧 channel。
    pub async fn reconnect(&self) -> anyhow::Result<Self> {
        let channel = Channel::from_shared(self.endpoint.clone())
            .context("gRPC 地址非法")?
            .connect_timeout(Duration::from_secs(5))
            .connect()
            .await
            .context("gRPC 重连失败")?;
        Ok(Self {
            channel: Some(channel),
            endpoint: self.endpoint.clone(),
            auth: self.auth.clone(),
            rpc_timeout: self.rpc_timeout,
        })
    }

    fn interceptor(&self) -> JwtInterceptor {
        JwtInterceptor {
            auth: self.auth.clone(),
            rpc_timeout: Some(self.rpc_timeout),
        }
    }

    fn controller(&self) -> anyhow::Result<EmulatorControllerClient<AuthenticatedChannel>> {
        self.controller_with_timeout(self.rpc_timeout)
    }

    fn controller_with_timeout(
        &self,
        rpc_timeout: Duration,
    ) -> anyhow::Result<EmulatorControllerClient<AuthenticatedChannel>> {
        Ok(EmulatorControllerClient::with_interceptor(
            self.channel.clone().context("gRPC client 尚未连接")?,
            JwtInterceptor {
                auth: self.auth.clone(),
                rpc_timeout: Some(rpc_timeout),
            },
        ))
    }

    fn streaming_controller(
        &self,
    ) -> anyhow::Result<EmulatorControllerClient<AuthenticatedChannel>> {
        Ok(EmulatorControllerClient::with_interceptor(
            self.channel.clone().context("gRPC client 尚未连接")?,
            JwtInterceptor {
                auth: self.auth.clone(),
                // 长流不能携带会在播放中途到期的 grpc-timeout；建链由外层
                // tokio timeout 单独限制。
                rpc_timeout: None,
            },
        ))
    }

    fn snapshots(&self) -> anyhow::Result<SnapshotServiceClient<AuthenticatedChannel>> {
        Ok(SnapshotServiceClient::with_interceptor(
            self.channel.clone().context("gRPC client 尚未连接")?,
            self.interceptor(),
        ))
    }

    /// 查询状态（含 booted）。
    pub async fn status(&self) -> anyhow::Result<EmulatorStatus> {
        let mut c = self.controller()?;
        c.get_status(tonic::Request::new(()))
            .await
            .map(|r| r.into_inner())
            .context("getStatus 失败")
    }

    /// boot 判定（对应 adb getprop sys.boot_completed，gRPC 直接可用）。
    pub async fn is_booted(&self) -> anyhow::Result<bool> {
        Ok(self.status().await?.booted)
    }

    /// 截图（PNG）。返回图片数据，可用 write_screenshot 落盘。
    pub async fn screenshot(&self, width: u32, height: u32) -> anyhow::Result<Image> {
        let mut c = self.controller()?;
        let fmt = ImageFormat {
            format: android::emulation::control::image_format::ImgFormat::Png as i32,
            width,
            height,
            ..Default::default()
        };
        c.get_screenshot(tonic::Request::new(fmt))
            .await
            .map(|r| r.into_inner())
            .context("getScreenshot 失败")
    }

    /// 截图并写入文件（PNG）。
    pub async fn write_screenshot(&self, path: &std::path::Path) -> anyhow::Result<u64> {
        let img = self.screenshot(0, 0).await?;
        let bytes = &img.image;
        if bytes.is_empty() {
            anyhow::bail!("截图数据为空");
        }
        std::fs::write(path, bytes).context("写截图文件失败")?;
        Ok(bytes.len() as u64)
    }

    /// 建立固定 48kHz/stereo/S16LE 的 guest 音频长流。
    ///
    /// 只限制建立阶段；返回后的 stream 由 session-bound coordinator 显式取消。
    pub async fn stream_audio_output(
        &self,
    ) -> Result<tonic::Streaming<AudioPacket>, AudioStreamConnectError> {
        let mut controller = self
            .streaming_controller()
            .map_err(|error| AudioStreamConnectError::Disconnected(error.to_string()))?;
        let request = output_audio_format();
        tokio::time::timeout(
            AUDIO_STREAM_ESTABLISH_TIMEOUT,
            controller.stream_audio(tonic::Request::new(request)),
        )
        .await
        .map_err(|_| AudioStreamConnectError::Timeout)?
        .map_err(|status| AudioStreamConnectError::Rpc {
            code: status.code(),
            message: status.message().to_owned(),
        })
        .map(tonic::Response::into_inner)
    }

    /// 查询 Emulator 是否正在消费配置给该进程的 host microphone source。
    pub async fn microphone_state(&self) -> anyhow::Result<bool> {
        let mut controller = self.controller()?;
        controller
            .get_microphone_state(tonic::Request::new(()))
            .await
            .map(|response| response.into_inner().real_audio_enabled)
            .context("getMicrophoneState 失败")
    }

    /// 显式开关该 managed session 的私有虚拟 microphone source。
    pub async fn set_microphone_enabled(&self, enabled: bool) -> anyhow::Result<()> {
        let mut controller = self.controller()?;
        controller
            .set_microphone_state(tonic::Request::new(MicrophoneState {
                real_audio_enabled: enabled,
            }))
            .await
            .map(|_| ())
            .context("setMicrophoneState 失败")
    }

    /// 快照列表（包含兼容与不兼容项，便于明确删除旧快照）。
    pub async fn list_snapshots(&self) -> anyhow::Result<Vec<SnapshotDetails>> {
        let mut c = self.snapshots()?;
        let req = tonic::Request::new(android::emulation::control::SnapshotFilter {
            status_filter: android::emulation::control::snapshot_filter::LoadStatus::All as i32,
        });
        let list = c
            .list_snapshots(req)
            .await
            .context("listSnapshots 失败")?
            .into_inner();
        Ok(list.snapshots)
    }

    /// 保存当前虚拟机状态到本地 AVD snapshot。
    pub async fn save_snapshot(&self, snapshot_id: &str) -> anyhow::Result<()> {
        let request = snapshot_request(snapshot_id)?;
        let mut client = self.snapshots()?;
        let response = client
            .save_snapshot(tonic::Request::new(request))
            .await
            .context("saveSnapshot 失败")?
            .into_inner();
        validate_snapshot_response("saveSnapshot", response)
    }

    /// 加载本地 AVD snapshot。调用可能使 guest 和控制通道短暂重置。
    pub async fn load_snapshot(&self, snapshot_id: &str) -> anyhow::Result<()> {
        let request = snapshot_request(snapshot_id)?;
        let mut client = self.snapshots()?;
        let response = client
            .load_snapshot(tonic::Request::new(request))
            .await
            .context("loadSnapshot 失败")?
            .into_inner();
        validate_snapshot_response("loadSnapshot", response)
    }

    /// 删除本地 AVD snapshot。
    pub async fn delete_snapshot(&self, snapshot_id: &str) -> anyhow::Result<()> {
        let request = snapshot_request(snapshot_id)?;
        let mut client = self.snapshots()?;
        let response = client
            .delete_snapshot(tonic::Request::new(request))
            .await
            .context("deleteSnapshot 失败")?
            .into_inner();
        validate_snapshot_response("deleteSnapshot", response)
    }

    pub async fn send_key_event(&self, event: KeyboardEvent) -> Result<(), InputRpcError> {
        let mut controller = self
            .controller_with_timeout(INPUT_RPC_TIMEOUT)
            .map_err(|error| InputRpcError::Disconnected(error.to_string()))?;
        controller
            .send_key(tonic::Request::new(event))
            .await
            .map(|_| ())
            .map_err(|status| input_status("sendKey", status))
    }

    pub async fn send_key(&self, key: &str, event_type: KeyEventType) -> Result<(), InputRpcError> {
        self.send_key_event(keyboard_key_event(key, event_type)?)
            .await
    }

    pub async fn send_text(&self, text: &str) -> Result<(), InputRpcError> {
        self.send_key_event(keyboard_text_event(text)?).await
    }

    pub async fn send_touch(&self, event: TouchEvent) -> Result<(), InputRpcError> {
        let mut controller = self
            .controller_with_timeout(INPUT_RPC_TIMEOUT)
            .map_err(|error| InputRpcError::Disconnected(error.to_string()))?;
        controller
            .send_touch(tonic::Request::new(event))
            .await
            .map(|_| ())
            .map_err(|status| input_status("sendTouch", status))
    }

    pub async fn send_mouse(&self, event: MouseEvent) -> Result<(), InputRpcError> {
        let mut controller = self
            .controller_with_timeout(INPUT_RPC_TIMEOUT)
            .map_err(|error| InputRpcError::Disconnected(error.to_string()))?;
        controller
            .send_mouse(tonic::Request::new(event))
            .await
            .map(|_| ())
            .map_err(|status| input_status("sendMouse", status))
    }
}

fn output_audio_format() -> AudioFormat {
    AudioFormat {
        sampling_rate: 48_000,
        channels: AudioChannels::Stereo as i32,
        format: AudioSampleFormat::AudFmtS16 as i32,
        // MODE_UNSPECIFIED may retain stale packets and explicitly allows the client to fall
        // behind. Focused playback is a real-time latest-sample path, so the emulator must
        // overwrite backlog instead of turning queue depth into audible A/V skew.
        mode: AudioDeliveryMode::ModeRealTime as i32,
    }
}

fn snapshot_request(snapshot_id: &str) -> anyhow::Result<SnapshotPackage> {
    validate_snapshot_id(snapshot_id)?;
    Ok(SnapshotPackage {
        snapshot_id: snapshot_id.to_owned(),
        ..Default::default()
    })
}

pub fn validate_snapshot_id(snapshot_id: &str) -> anyhow::Result<()> {
    let valid = !snapshot_id.is_empty()
        && snapshot_id.len() <= MAX_SNAPSHOT_ID_BYTES
        && !snapshot_id.starts_with('.')
        && snapshot_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        anyhow::bail!(
            "snapshot id 非法（1..={MAX_SNAPSHOT_ID_BYTES}B，仅允许 ASCII 字母数字、-、_、.，且不能以 . 开头）：{snapshot_id:?}"
        );
    }
    Ok(())
}

fn validate_snapshot_response(
    operation: &'static str,
    response: SnapshotPackage,
) -> anyhow::Result<()> {
    if response.success {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&response.err);
    let detail = detail.trim();
    if detail.is_empty() {
        anyhow::bail!("{operation} 被模拟器拒绝，未返回原因");
    }
    anyhow::bail!("{operation} 被模拟器拒绝：{detail}")
}

pub fn keyboard_key_event(
    key: &str,
    event_type: KeyEventType,
) -> Result<KeyboardEvent, InputRpcError> {
    if key.is_empty() || key.len() > 64 {
        return Err(InputRpcError::InvalidKey);
    }
    Ok(KeyboardEvent {
        code_type: KeyCodeType::Evdev as i32,
        event_type: event_type as i32,
        key: key.to_owned(),
        ..Default::default()
    })
}

pub fn keyboard_text_event(text: &str) -> Result<KeyboardEvent, InputRpcError> {
    if text.len() > MAX_INPUT_TEXT_BYTES {
        return Err(InputRpcError::TextTooLarge { actual: text.len() });
    }
    Ok(KeyboardEvent {
        text: text.to_owned(),
        event_type: KeyEventType::Keypress as i32,
        ..Default::default()
    })
}

pub fn touch_event(sample: crate::core::input::TouchSample) -> TouchEvent {
    TouchEvent {
        touches: vec![Touch {
            x: sample.point.x,
            y: sample.point.y,
            identifier: sample.identifier,
            pressure: sample.pressure,
            ..Default::default()
        }],
        display: 0,
    }
}

fn input_status(operation: &'static str, status: tonic::Status) -> InputRpcError {
    InputRpcError::Rpc {
        operation,
        code: status.code(),
        message: status.message().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interceptor_adds_bearer_token_and_deadline() {
        let auth = Arc::new(GrpcJwtAuth::new().unwrap());
        let mut interceptor = JwtInterceptor {
            auth,
            rpc_timeout: Some(Duration::from_secs(7)),
        };
        let request = interceptor.call(tonic::Request::new(())).unwrap();
        let authorization = request
            .metadata()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(authorization.starts_with("Bearer ey"));
        assert!(request.metadata().contains_key("grpc-timeout"));

        let auth = Arc::new(GrpcJwtAuth::new().unwrap());
        let mut streaming = JwtInterceptor {
            auth,
            rpc_timeout: None,
        };
        let request = streaming.call(tonic::Request::new(())).unwrap();
        assert!(request.metadata().contains_key("authorization"));
        assert!(!request.metadata().contains_key("grpc-timeout"));
    }

    #[test]
    fn audio_output_request_uses_real_time_delivery_without_backlog() {
        let request = output_audio_format();
        assert_eq!(request.sampling_rate, 48_000);
        assert_eq!(request.channels, AudioChannels::Stereo as i32);
        assert_eq!(request.format, AudioSampleFormat::AudFmtS16 as i32);
        assert_eq!(request.mode, AudioDeliveryMode::ModeRealTime as i32);
    }

    #[test]
    fn builds_bounded_keyboard_and_touch_requests() {
        let key = keyboard_key_event("GoBack", KeyEventType::Keydown).unwrap();
        assert_eq!(key.key, "GoBack");
        assert_eq!(key.event_type, KeyEventType::Keydown as i32);
        assert!(matches!(
            keyboard_key_event("", KeyEventType::Keypress),
            Err(InputRpcError::InvalidKey)
        ));
        assert!(matches!(
            keyboard_text_event(&"x".repeat(MAX_INPUT_TEXT_BYTES + 1)),
            Err(InputRpcError::TextTooLarge { .. })
        ));

        let event = touch_event(crate::core::input::TouchSample {
            point: crate::core::input::GuestPoint { x: 12, y: 34 },
            identifier: 7,
            pressure: 0,
        });
        assert_eq!(event.touches.len(), 1);
        assert_eq!(event.touches[0].x, 12);
        assert_eq!(event.touches[0].y, 34);
        assert_eq!(event.touches[0].identifier, 7);
        assert_eq!(event.touches[0].pressure, 0);
    }

    #[test]
    fn snapshot_requests_validate_ids_and_surface_emulator_errors() {
        assert_eq!(
            snapshot_request("release_1.2-qa").unwrap().snapshot_id,
            "release_1.2-qa"
        );
        for invalid in ["", ".hidden", "with space", "../escape"] {
            assert!(snapshot_request(invalid).is_err(), "应拒绝 {invalid:?}");
        }
        assert!(snapshot_request(&"x".repeat(MAX_SNAPSHOT_ID_BYTES + 1)).is_err());
        validate_snapshot_response(
            "saveSnapshot",
            SnapshotPackage {
                success: true,
                ..Default::default()
            },
        )
        .unwrap();
        let error = validate_snapshot_response(
            "saveSnapshot",
            SnapshotPackage {
                success: false,
                err: b"disk full".to_vec(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("disk full"));
    }
}
