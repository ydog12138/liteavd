//! Focused session 的宿主音频输出与应用级控制。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use gtk4::prelude::*;

use crate::core::audio::{
    AudioBuffer, AudioBufferStats, AudioPumpExit, AudioSink, AudioSinkError, AudioStreamError,
    OUTPUT_CHANNELS, OUTPUT_SAMPLE_RATE, ValidatedAudioPacket, run_route_audio,
};
use crate::core::instance::DeviceRuntime;
use crate::core::settings::{AppLogLevel, emit};
use crate::core::workspace::WorkspaceRoute;

pub const CONTROLS_WIDGET: &str = "liteavd-audio-controls";
pub const ENABLE_WIDGET: &str = "liteavd-audio-enable";
pub const MUTE_WIDGET: &str = "liteavd-audio-mute";
pub const VOLUME_WIDGET: &str = "liteavd-audio-volume";
const RETRY_DELAY: Duration = Duration::from_millis(750);
const OUTPUT_CALLBACK_FRAMES: u32 = OUTPUT_SAMPLE_RATE as u32 / 100;

struct FinishedAudio {
    result: Result<AudioPumpExit, AudioStreamError>,
    at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioStatus {
    Disabled,
    WaitingForFocus,
    Unavailable {
        avd_name: String,
        reason: String,
    },
    Connecting {
        avd_name: String,
    },
    Playing {
        avd_name: String,
        stats: AudioBufferStats,
    },
    Error {
        avd_name: String,
        message: String,
    },
}

struct ActiveAudio {
    route: WorkspaceRoute,
    control_stream_revision: u64,
    cancel: tokio::sync::watch::Sender<bool>,
    baseline: AudioBufferStats,
    result: Arc<Mutex<Option<FinishedAudio>>>,
}

impl ActiveAudio {
    fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

struct AudioOutputPool {
    buffer: Arc<AudioBuffer>,
    output_device_id: Option<String>,
    output: Mutex<Option<CpalAudioSink>>,
    creating: AtomicBool,
    open: AtomicBool,
    changed: tokio::sync::Notify,
}

impl AudioOutputPool {
    fn new(buffer: Arc<AudioBuffer>, output_device_id: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            buffer,
            output_device_id,
            output: Mutex::new(None),
            creating: AtomicBool::new(false),
            open: AtomicBool::new(true),
            changed: tokio::sync::Notify::new(),
        })
    }

    fn open(&self) {
        self.open.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    fn close(&self) {
        self.open.store(false, Ordering::Release);
        AudioBuffer::clear(&self.buffer);
        let output = self
            .output
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(output) = output {
            // CPAL Pulse stream drop joins its workers; keep that off the GTK thread.
            crate::ui::background::spawn(async move { drop(output) });
        }
        self.changed.notify_waiters();
    }

    async fn acquire(
        &self,
        cancel: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<Option<CpalAudioSink>, String> {
        loop {
            if *cancel.borrow() || !self.open.load(Ordering::Acquire) {
                return Ok(None);
            }
            let notified = self.changed.notified();
            if let Some(output) = self
                .output
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                return Ok(Some(output));
            }
            if self
                .creating
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let output =
                    CpalAudioSink::new(self.buffer.clone(), self.output_device_id.as_deref());
                self.creating.store(false, Ordering::Release);
                self.changed.notify_waiters();
                if *cancel.borrow() || !self.open.load(Ordering::Acquire) {
                    return Ok(None);
                }
                return output.map(Some);
            }
            tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        return Ok(None);
                    }
                }
                () = notified => {}
            }
        }
    }

    fn release(&self, output: CpalAudioSink) {
        if self.open.load(Ordering::Acquire) {
            *self
                .output
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(output);
        }
        self.changed.notify_one();
    }
}

struct ControllerState {
    enabled: bool,
    muted: bool,
    volume: f32,
    active: Option<ActiveAudio>,
    unavailable: Option<(String, String)>,
}

/// 不含 GTK 对象，可由主线程控制并由共用后台 runtime 执行长流。
pub struct AudioController {
    runtime: Arc<DeviceRuntime>,
    buffer: Arc<AudioBuffer>,
    output: Arc<AudioOutputPool>,
    state: Mutex<ControllerState>,
}

impl AudioController {
    pub fn new(runtime: Arc<DeviceRuntime>) -> Arc<Self> {
        Self::new_with_output_device_id(runtime, None)
    }

    /// 构造绑定到指定 Pulse sink id 的控制器；主要供隔离集成门禁使用。
    pub fn new_for_output_device_id(
        runtime: Arc<DeviceRuntime>,
        output_device_id: impl Into<String>,
    ) -> Arc<Self> {
        Self::new_with_output_device_id(runtime, Some(output_device_id.into()))
    }

    fn new_with_output_device_id(
        runtime: Arc<DeviceRuntime>,
        output_device_id: Option<String>,
    ) -> Arc<Self> {
        let buffer = Arc::new(AudioBuffer::default());
        Arc::new(Self {
            runtime,
            output: AudioOutputPool::new(buffer.clone(), output_device_id),
            buffer,
            state: Mutex::new(ControllerState {
                enabled: true,
                muted: false,
                volume: 1.0,
                active: None,
                unavailable: None,
            }),
        })
    }

    /// GTK 主线程每 50ms 调用；焦点变化时先同步清空/取消旧 route，再启动新流。
    pub fn sync_focus(self: &Arc<Self>) {
        let focused = self.runtime.workspace_snapshot().focused;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.enabled {
            cancel_active(&mut state, &self.buffer);
            self.output.close();
            state.unavailable = None;
            return;
        }
        let Some(route) = focused else {
            cancel_active(&mut state, &self.buffer);
            self.output.close();
            state.unavailable = None;
            return;
        };
        if !self.runtime.route_is_current(&route) {
            cancel_active(&mut state, &self.buffer);
            self.output.close();
            state.unavailable = None;
            return;
        }
        let control_stream_revision = self.runtime.control_stream_revision();
        if let Some(active) = state.active.as_ref()
            && active.route == route
            && active.control_stream_revision == control_stream_revision
        {
            let should_retry = active
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .is_some_and(|finished| {
                    retryable(&finished.result) && finished.at.elapsed() >= RETRY_DELAY
                });
            if !should_retry {
                return;
            }
        }
        self.output.open();
        cancel_active(&mut state, &self.buffer);
        if self.runtime.grpc_client_for_route(&route).is_none() {
            self.output.close();
            state.unavailable = Some((
                route.avd_name,
                "adopted session 没有 liteavd JWT 私钥".into(),
            ));
            return;
        }

        self.buffer.set_volume(state.volume);
        self.buffer.set_muted(state.muted);
        let (cancel, receiver) = tokio::sync::watch::channel(false);
        let result = Arc::new(Mutex::new(None));
        let baseline = self.buffer.stats();
        state.unavailable = None;
        state.active = Some(ActiveAudio {
            route: route.clone(),
            control_stream_revision,
            cancel,
            baseline,
            result: result.clone(),
        });
        drop(state);

        let runtime = self.runtime.clone();
        let buffer = self.buffer.clone();
        let output = self.output.clone();
        crate::ui::background::spawn(async move {
            let mut receiver = receiver;
            let run = match output.acquire(&mut receiver).await {
                Ok(Some(sink)) => {
                    let run = run_route_audio(runtime, route, buffer, receiver).await;
                    output.release(sink);
                    run
                }
                Ok(None) => Ok(AudioPumpExit::Canceled(Default::default())),
                Err(error) => Err(AudioStreamError::Sink(AudioSinkError(error))),
            };
            *result.lock().unwrap_or_else(|error| error.into_inner()) = Some(FinishedAudio {
                result: run,
                at: Instant::now(),
            });
        });
    }

    pub fn set_enabled(self: &Arc<Self>, enabled: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.enabled == enabled {
            return;
        }
        state.enabled = enabled;
        state.unavailable = None;
        cancel_active(&mut state, &self.buffer);
        if enabled {
            self.output.open();
        } else {
            self.output.close();
        }
        drop(state);
        if enabled {
            self.sync_focus();
        }
    }

    pub fn enabled(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .enabled
    }

    pub fn set_muted(&self, muted: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.muted = muted;
        self.buffer.set_muted(muted);
    }

    pub fn muted(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .muted
    }

    pub fn set_volume(&self, volume: f32) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.buffer.set_volume(state.volume);
    }

    pub fn volume(&self) -> f32 {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .volume
    }

    pub fn status(&self) -> AudioStatus {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.enabled {
            return AudioStatus::Disabled;
        }
        if let Some((avd_name, reason)) = &state.unavailable {
            return AudioStatus::Unavailable {
                avd_name: avd_name.clone(),
                reason: reason.clone(),
            };
        }
        let Some(active) = &state.active else {
            return AudioStatus::WaitingForFocus;
        };
        if let Some(finished) = active
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            return match &finished.result {
                Ok(AudioPumpExit::Canceled(_)) => AudioStatus::Connecting {
                    avd_name: active.route.avd_name.clone(),
                },
                Err(error) => AudioStatus::Error {
                    avd_name: active.route.avd_name.clone(),
                    message: audio_error_message(error),
                },
            };
        }
        let stats = stats_since(self.buffer.stats(), active.baseline);
        if stats.samples_received == 0 {
            AudioStatus::Connecting {
                avd_name: active.route.avd_name.clone(),
            }
        } else {
            AudioStatus::Playing {
                avd_name: active.route.avd_name.clone(),
                stats,
            }
        }
    }
}

impl Drop for AudioController {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        cancel_active(&mut state, &self.buffer);
        self.output.close();
    }
}

fn cancel_active(state: &mut ControllerState, buffer: &AudioBuffer) {
    buffer.clear();
    if let Some(active) = state.active.take() {
        active.cancel();
    }
}

fn stats_since(current: AudioBufferStats, baseline: AudioBufferStats) -> AudioBufferStats {
    AudioBufferStats {
        samples_received: current
            .samples_received
            .saturating_sub(baseline.samples_received),
        samples_played: current
            .samples_played
            .saturating_sub(baseline.samples_played),
        samples_dropped: current
            .samples_dropped
            .saturating_sub(baseline.samples_dropped),
        underrun_samples: current
            .underrun_samples
            .saturating_sub(baseline.underrun_samples),
        contention_callbacks: current
            .contention_callbacks
            .saturating_sub(baseline.contention_callbacks),
        queued_samples: current.queued_samples,
        primed: current.primed,
    }
}

fn audio_error_message(error: &AudioStreamError) -> String {
    match error {
        AudioStreamError::Rpc {
            code: tonic::Code::PermissionDenied,
            ..
        } => "该 session 的旧 JWT allowlist 不含 streamAudio；请重启此设备".into(),
        _ => error.to_string(),
    }
}

fn retryable(result: &Result<AudioPumpExit, AudioStreamError>) -> bool {
    match result {
        Ok(AudioPumpExit::Canceled(_)) => false,
        Err(AudioStreamError::Connect(_) | AudioStreamError::UnexpectedEnd) => true,
        Err(AudioStreamError::Rpc { code, .. }) => matches!(
            code,
            tonic::Code::Cancelled
                | tonic::Code::Unknown
                | tonic::Code::DeadlineExceeded
                | tonic::Code::ResourceExhausted
                | tonic::Code::Aborted
                | tonic::Code::Internal
                | tonic::Code::Unavailable
        ),
        Err(
            AudioStreamError::StaleRoute
            | AudioStreamError::InvalidPacket(_)
            | AudioStreamError::Sink(_),
        ) => false,
    }
}

struct CpalAudioSink {
    buffer: Arc<AudioBuffer>,
    stream_error: Arc<Mutex<Option<String>>>,
    _stream: cpal::Stream,
}

impl CpalAudioSink {
    fn new(buffer: Arc<AudioBuffer>, output_device_id: Option<&str>) -> Result<Self, String> {
        crate::core::microphone::ensure_flatpak_pulse_cookie()?;
        let host = cpal::host_from_id(cpal::HostId::PulseAudio)
            .map_err(|error| format!("PulseAudio host 不可用：{error}"))?;
        let device = if let Some(output_device_id) = output_device_id {
            let id = cpal::DeviceId::new(cpal::HostId::PulseAudio, output_device_id);
            host.device_by_id(&id)
                .ok_or_else(|| format!("PulseAudio 输出设备不存在：{output_device_id}"))?
        } else {
            host.default_output_device()
                .ok_or_else(|| "PulseAudio 没有默认输出设备".to_owned())?
        };
        let stream_error = Arc::new(Mutex::new(None));
        let callback_buffer = buffer.clone();
        let callback_error = stream_error.clone();
        let config = cpal::StreamConfig {
            channels: OUTPUT_CHANNELS as u16,
            sample_rate: OUTPUT_SAMPLE_RATE as u32,
            // Pulse may otherwise choose a roughly one-second target buffer, making
            // application mute and focus handoff audibly late. Keep the host callback
            // below the producer cadence and leave enough A/V budget for guest capture.
            buffer_size: cpal::BufferSize::Fixed(OUTPUT_CALLBACK_FRAMES),
        };
        let stream = device
            .build_output_stream(
                config,
                move |output: &mut [i16], _| callback_buffer.fill_i16(output),
                move |error| {
                    *callback_error
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
                },
                None,
            )
            .map_err(|error| format!("创建 PulseAudio 输出流失败：{error}"))?;
        stream
            .play()
            .map_err(|error| format!("启动 PulseAudio 输出流失败：{error}"))?;
        Ok(Self {
            buffer,
            stream_error,
            _stream: stream,
        })
    }
}

impl AudioSink for CpalAudioSink {
    fn write(&mut self, packet: ValidatedAudioPacket<'_>) -> Result<(), AudioSinkError> {
        if let Some(error) = self
            .stream_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            return Err(AudioSinkError(error));
        }
        self.buffer.push_s16le(packet.s16le);
        Ok(())
    }

    fn clear(&mut self) {
        AudioBuffer::clear(&self.buffer);
    }
}

pub fn build_controls(controller: Arc<AudioController>) -> gtk4::Box {
    let controls = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
    controls.set_widget_name(CONTROLS_WIDGET);

    let enable = gtk4::ToggleButton::new();
    enable.set_widget_name(ENABLE_WIDGET);
    enable.set_icon_name("media-playback-stop-symbolic");
    enable.set_active(controller.enabled());
    enable.set_tooltip_text(Some("播放 focused 设备声音"));
    let controller_for_enable = controller.clone();
    enable.connect_toggled(move |button| {
        controller_for_enable.set_enabled(button.is_active());
    });
    controls.append(&enable);

    let mute = gtk4::ToggleButton::new();
    mute.set_widget_name(MUTE_WIDGET);
    mute.set_icon_name("audio-volume-muted-symbolic");
    mute.set_active(controller.muted());
    mute.set_tooltip_text(Some("静音 guest 音频输出"));
    let controller_for_mute = controller.clone();
    mute.connect_toggled(move |button| controller_for_mute.set_muted(button.is_active()));
    controls.append(&mute);

    let volume = gtk4::Scale::with_range(gtk4::Orientation::Horizontal, 0.0, 1.0, 0.05);
    volume.set_widget_name(VOLUME_WIDGET);
    volume.set_width_request(96);
    volume.set_draw_value(false);
    volume.set_value(f64::from(controller.volume()));
    volume.set_tooltip_text(Some("liteavd 播放音量"));
    let controller_for_volume = controller.clone();
    volume.connect_value_changed(move |scale| {
        controller_for_volume.set_volume(scale.value() as f32);
    });
    controls.append(&volume);

    let enable_weak = enable.downgrade();
    let mute_weak = mute.downgrade();
    glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
        let (Some(enable), Some(mute)) = (enable_weak.upgrade(), mute_weak.upgrade()) else {
            return glib::ControlFlow::Break;
        };
        let status = controller.status();
        let tooltip = status_tooltip(&status);
        enable.set_tooltip_text(Some(&tooltip));
        enable.set_icon_name(match status {
            AudioStatus::Disabled => "media-playback-start-symbolic",
            AudioStatus::Error { .. } | AudioStatus::Unavailable { .. } => {
                "dialog-warning-symbolic"
            }
            _ => "media-playback-stop-symbolic",
        });
        mute.set_sensitive(!matches!(status, AudioStatus::Disabled));
        glib::ControlFlow::Continue
    });

    controls
}

fn status_tooltip(status: &AudioStatus) -> String {
    match status {
        AudioStatus::Disabled => "focused 设备声音已关闭".into(),
        AudioStatus::WaitingForFocus => "等待 focused managed session".into(),
        AudioStatus::Unavailable { avd_name, reason } => {
            format!("{avd_name} 音频不可用：{reason}")
        }
        AudioStatus::Connecting { avd_name } => format!("正在等待 {avd_name} 的 guest 音频…"),
        AudioStatus::Playing { avd_name, stats } => format!(
            "正在播放 {avd_name}：{} queued samples，{} underrun",
            stats.queued_samples, stats.underrun_samples
        ),
        AudioStatus::Error { avd_name, message } => format!("{avd_name} 音频错误：{message}"),
    }
}

pub fn log_status(status: &AudioStatus) {
    if let AudioStatus::Error { avd_name, message } = status {
        emit(
            AppLogLevel::Warn,
            format_args!("{avd_name} focused 音频输出失败：{message}"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::core::emulator::RunningInstance;
    use crate::core::microphone::{PULSE_COOKIE_BYTES, ensure_pulse_cookie_at};

    fn test_directory(label: &str) -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "liteavd-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn status_tooltips_do_not_claim_unavailable_audio_is_playing() {
        let unavailable = status_tooltip(&AudioStatus::Unavailable {
            avd_name: "external".into(),
            reason: "no key".into(),
        });
        assert!(unavailable.contains("不可用"));
        let playing = status_tooltip(&AudioStatus::Playing {
            avd_name: "phone".into(),
            stats: AudioBufferStats {
                queued_samples: 32,
                ..Default::default()
            },
        });
        assert!(playing.contains("正在播放"));
    }

    #[test]
    fn output_callback_period_is_ten_milliseconds() {
        assert_eq!(OUTPUT_CALLBACK_FRAMES, 480);
    }

    #[test]
    fn only_transient_stream_failures_retry() {
        assert!(retryable(&Err(AudioStreamError::UnexpectedEnd)));
        assert!(retryable(&Err(AudioStreamError::Rpc {
            code: tonic::Code::Unavailable,
            message: "snapshot reconnect".into(),
        })));
        assert!(!retryable(&Err(AudioStreamError::Rpc {
            code: tonic::Code::PermissionDenied,
            message: "old allowlist".into(),
        })));
        assert!(!retryable(&Err(AudioStreamError::InvalidPacket(
            crate::core::audio::AudioPacketError::Empty,
        ))));
    }

    #[test]
    fn private_pulse_cookie_is_no_clobber_and_mode_0600() {
        let root = test_directory("pulse-cookie");
        let pulse = root.join("config/pulse");
        ensure_pulse_cookie_at(&pulse).unwrap();
        let cookie = pulse.join("cookie");
        let metadata = std::fs::symlink_metadata(&cookie).unwrap();
        assert_eq!(metadata.len(), PULSE_COOKIE_BYTES as u64);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::symlink_metadata(&pulse)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        std::fs::write(&cookie, [7_u8; PULSE_COOKIE_BYTES]).unwrap();
        ensure_pulse_cookie_at(&pulse).unwrap();
        assert_eq!(std::fs::read(&cookie).unwrap(), [7_u8; PULSE_COOKIE_BYTES]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn malformed_existing_pulse_cookie_is_rejected() {
        let root = test_directory("bad-pulse-cookie");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("cookie"), [0_u8; 8]).unwrap();
        let error = ensure_pulse_cookie_at(&root).unwrap_err();
        assert!(error.contains("256B"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn adopted_focus_is_explicitly_unavailable_without_starting_a_sink() {
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![RunningInstance {
            pid: 10_001,
            ini_path: "/tmp/adopted.ini".into(),
            avd_name: "adopted".into(),
            console_port: 5554,
            adb_port: 5555,
            grpc_port: 8554,
            grpc_allowlist: None,
            grpc_jwks: None,
            grpc_jwk_active: None,
        }]);
        runtime.focus_session("adopted").unwrap();
        let controller = AudioController::new(runtime);
        controller.sync_focus();
        assert!(matches!(
            controller.status(),
            AudioStatus::Unavailable { avd_name, .. } if avd_name == "adopted"
        ));
    }

    #[test]
    #[ignore = "需要正在运行的 PulseAudio/PipeWire Pulse 兼容服务和默认输出设备"]
    fn pulse_sink_consumes_prebuffered_pcm() {
        let buffer = Arc::new(AudioBuffer::default());
        let mut sink =
            CpalAudioSink::new(buffer.clone(), None).expect("create PulseAudio output sink");
        let samples = vec![0_i16; OUTPUT_SAMPLE_RATE as usize * OUTPUT_CHANNELS / 10];
        let pcm: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        sink.write(ValidatedAudioPacket {
            timestamp_micros: 0,
            s16le: &pcm,
        })
        .expect("queue PCM");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let stats = buffer.stats();
        assert_eq!(stats.samples_received, samples.len() as u64);
        assert!(
            stats.samples_played > 0,
            "PulseAudio callback did not consume PCM: {stats:?}"
        );
        sink.clear();
    }
}
