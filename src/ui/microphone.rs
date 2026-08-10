//! 虚拟麦克风来源控制：宿主 Pulse 输入或 PCM WAV，始终只绑定一个 exact route。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::core::instance::DeviceRuntime;
use crate::core::microphone::{
    MICROPHONE_SAMPLE_RATE, MicrophoneBuffer, MicrophoneCoordinator, MicrophonePumpExit,
    MicrophoneSource,
};
use crate::core::workspace::WorkspaceRoute;

const HOST_INPUT_CALLBACK_FRAMES: u32 = MICROPHONE_SAMPLE_RATE / 50;
const HOST_INPUT_FIRST_CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    Host,
    Wav { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicrophoneStatus {
    Inactive,
    Active {
        avd_name: String,
        source: SourceKind,
    },
    Paused {
        avd_name: String,
        source: SourceKind,
    },
    Finished {
        avd_name: String,
        source: SourceKind,
    },
    Error {
        avd_name: String,
        source: SourceKind,
        message: String,
    },
}

struct FinishedSource {
    result: Result<MicrophonePumpExit, String>,
}

struct ActiveSource {
    route: WorkspaceRoute,
    source: SourceKind,
    cancel: tokio::sync::watch::Sender<bool>,
    finished: Arc<Mutex<Option<FinishedSource>>>,
    paused: Arc<AtomicBool>,
}

struct ControllerState {
    active: Option<ActiveSource>,
}

/// 不保存授权状态；关闭应用、切换来源或 route 失效都会取消当前 worker。
pub struct MicrophoneController {
    runtime: Arc<DeviceRuntime>,
    coordinator: Arc<MicrophoneCoordinator>,
    state: Mutex<ControllerState>,
}

impl std::fmt::Debug for MicrophoneController {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MicrophoneController")
            .finish_non_exhaustive()
    }
}

impl MicrophoneController {
    pub fn new(runtime: Arc<DeviceRuntime>) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            coordinator: Arc::new(MicrophoneCoordinator::default()),
            state: Mutex::new(ControllerState { active: None }),
        })
    }

    pub fn available(&self, avd_name: &str) -> bool {
        self.runtime.input_route(avd_name).is_some_and(|guard| {
            self.runtime
                .microphone_endpoint_for_route(guard.route())
                .is_some()
        })
    }

    pub fn start_host(self: &Arc<Self>, avd_name: &str) -> Result<(), String> {
        let route = self.route_for_start(avd_name)?;
        self.start(route, SourceKind::Host, None);
        Ok(())
    }

    pub fn start_wav(self: &Arc<Self>, avd_name: &str, path: PathBuf) -> Result<(), String> {
        if path.extension().and_then(|extension| extension.to_str()) != Some("wav") {
            return Err("首版文件注入只接受扩展名为 .wav 的 PCM WAV".into());
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("audio.wav")
            .to_owned();
        let route = self.route_for_start(avd_name)?;
        self.start(route, SourceKind::Wav { name }, Some(path));
        Ok(())
    }

    fn route_for_start(&self, avd_name: &str) -> Result<WorkspaceRoute, String> {
        let guard = self
            .runtime
            .input_route(avd_name)
            .ok_or_else(|| format!("{avd_name} 没有可控的运行 session"))?;
        guard.focus();
        let route = guard.route().clone();
        if self.runtime.microphone_endpoint_for_route(&route).is_none() {
            return Err(format!(
                "{avd_name} 没有虚拟麦克风端点；请确认 PipeWire/Pulse 与 pactl 可用后重启设备"
            ));
        }
        Ok(route)
    }

    fn start(
        self: &Arc<Self>,
        route: WorkspaceRoute,
        source_kind: SourceKind,
        wav_path: Option<PathBuf>,
    ) {
        let (cancel, receiver) = tokio::sync::watch::channel(false);
        let finished = Arc::new(Mutex::new(None));
        let paused = Arc::new(AtomicBool::new(false));
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(previous) = state.active.take() {
                let _ = previous.cancel.send(true);
            }
            state.active = Some(ActiveSource {
                route: route.clone(),
                source: source_kind.clone(),
                cancel: cancel.clone(),
                finished: finished.clone(),
                paused: paused.clone(),
            });
        }

        let runtime = self.runtime.clone();
        let coordinator = self.coordinator.clone();
        crate::ui::background::spawn(async move {
            let result = match wav_path {
                Some(path) => coordinator
                    .run(
                        runtime,
                        route,
                        MicrophoneSource::Wav { path, paused },
                        receiver,
                    )
                    .await
                    .map_err(|error| error.to_string()),
                None => run_host_input(coordinator, runtime, route, cancel, receiver).await,
            };
            *finished.lock().unwrap_or_else(|error| error.into_inner()) =
                Some(FinishedSource { result });
        });
    }

    pub fn stop_for(&self, avd_name: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.route.avd_name == avd_name)
            && let Some(active) = state.active.take()
        {
            let _ = active.cancel.send(true);
        }
    }

    /// `Some(true)` 表示切到暂停，`Some(false)` 表示继续；`None` 表示当前
    /// 卡片没有正在运行的 WAV 来源。
    pub fn toggle_wav_pause(&self, avd_name: &str) -> Option<bool> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let active = state.active.as_ref().filter(|active| {
            active.route.avd_name == avd_name
                && matches!(active.source, SourceKind::Wav { .. })
                && active
                    .finished
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_none()
        })?;
        let paused = !active.paused.load(Ordering::Acquire);
        active.paused.store(paused, Ordering::Release);
        Some(paused)
    }

    pub fn stop_all(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(active) = state.active.take() {
            let _ = active.cancel.send(true);
        }
    }

    pub fn status_for(&self, avd_name: &str) -> MicrophoneStatus {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(active) = state
            .active
            .as_ref()
            .filter(|active| active.route.avd_name == avd_name)
        else {
            return MicrophoneStatus::Inactive;
        };
        let finished = active
            .finished
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match finished.as_ref().map(|finished| &finished.result) {
            None if active.paused.load(Ordering::Acquire) => MicrophoneStatus::Paused {
                avd_name: avd_name.to_owned(),
                source: active.source.clone(),
            },
            None => MicrophoneStatus::Active {
                avd_name: avd_name.to_owned(),
                source: active.source.clone(),
            },
            Some(Ok(MicrophonePumpExit::EndOfFile)) => MicrophoneStatus::Finished {
                avd_name: avd_name.to_owned(),
                source: active.source.clone(),
            },
            Some(Ok(MicrophonePumpExit::Canceled)) => MicrophoneStatus::Inactive,
            Some(Err(message)) => MicrophoneStatus::Error {
                avd_name: avd_name.to_owned(),
                source: active.source.clone(),
                message: message.clone(),
            },
        }
    }
}

impl Drop for MicrophoneController {
    fn drop(&mut self) {
        self.stop_all();
    }
}

struct CpalMicrophoneInput {
    error: Arc<Mutex<Option<String>>>,
    _stream: cpal::Stream,
}

impl CpalMicrophoneInput {
    fn new(buffer: Arc<MicrophoneBuffer>) -> Result<Self, String> {
        crate::core::microphone::ensure_flatpak_pulse_cookie()?;
        let host = cpal::host_from_id(cpal::HostId::PulseAudio)
            .map_err(|error| format!("PulseAudio host 不可用：{error}"))?;
        let device = host
            .default_input_device()
            .ok_or_else(|| "PulseAudio 没有默认输入设备".to_owned())?;
        let error = Arc::new(Mutex::new(None));
        let callback_error = error.clone();
        let config = cpal::StreamConfig {
            channels: 1,
            sample_rate: MICROPHONE_SAMPLE_RATE,
            // Match the FIFO pump's 20 ms cadence. Pulse's default record fragment is
            // unbounded, which can defer the next callback beyond a short guest recording.
            buffer_size: cpal::BufferSize::Fixed(HOST_INPUT_CALLBACK_FRAMES),
        };
        let stream = device
            .build_input_stream(
                config,
                move |input: &[i16], _| buffer.push_mono_i16(input),
                move |stream_error| {
                    *callback_error
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                        Some(stream_error.to_string());
                },
                None,
            )
            .map_err(|error| format!("创建 PulseAudio 麦克风输入流失败：{error}"))?;
        stream
            .play()
            .map_err(|error| format!("启动 PulseAudio 麦克风输入流失败：{error}"))?;
        Ok(Self {
            error,
            _stream: stream,
        })
    }
}

async fn run_host_input(
    coordinator: Arc<MicrophoneCoordinator>,
    runtime: Arc<DeviceRuntime>,
    route: WorkspaceRoute,
    cancel: tokio::sync::watch::Sender<bool>,
    receiver: tokio::sync::watch::Receiver<bool>,
) -> Result<MicrophonePumpExit, String> {
    let buffer = Arc::new(MicrophoneBuffer::default());
    let input = CpalMicrophoneInput::new(buffer.clone())?;
    let input_error = input.error.clone();
    let monitor_buffer = buffer.clone();
    let monitor_cancel = cancel.clone();
    let monitor = tokio::spawn(async move {
        let started = std::time::Instant::now();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if input_error
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_some()
            {
                let _ = monitor_cancel.send(true);
                break;
            }
            if started.elapsed() >= HOST_INPUT_FIRST_CALLBACK_TIMEOUT
                && monitor_buffer.stats().frames_received == 0
            {
                *input_error
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(format!(
                    "宿主麦克风在 {HOST_INPUT_FIRST_CALLBACK_TIMEOUT:?} 内未返回音频帧"
                ));
                let _ = monitor_cancel.send(true);
                break;
            }
        }
    });
    let run = coordinator
        .run(runtime, route, MicrophoneSource::Host(buffer), receiver)
        .await
        .map_err(|error| error.to_string());
    monitor.abort();
    let stream_error = input
        .error
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    drop(input);
    if let Some(error) = stream_error {
        Err(format!("PulseAudio 麦克风输入中断：{error}"))
    } else {
        run
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_or_adopted_devices_have_no_microphone_capability() {
        let runtime = Arc::new(DeviceRuntime::default());
        let controller = MicrophoneController::new(runtime);
        assert!(!controller.available("stopped"));
        assert!(controller.start_host("stopped").is_err());
        assert_eq!(controller.status_for("stopped"), MicrophoneStatus::Inactive);
    }

    #[test]
    fn wav_pause_toggle_is_route_scoped_and_reversible() {
        let runtime = Arc::new(DeviceRuntime::default());
        let controller = MicrophoneController::new(runtime);
        let (cancel, _receiver) = tokio::sync::watch::channel(false);
        controller.state.lock().unwrap().active = Some(ActiveSource {
            route: WorkspaceRoute {
                avd_name: "phone".into(),
                session_id: 7,
                generation: 3,
            },
            source: SourceKind::Wav {
                name: "tone.wav".into(),
            },
            cancel,
            finished: Arc::new(Mutex::new(None)),
            paused: Arc::new(AtomicBool::new(false)),
        });
        assert_eq!(controller.toggle_wav_pause("tablet"), None);
        assert_eq!(controller.toggle_wav_pause("phone"), Some(true));
        assert!(matches!(
            controller.status_for("phone"),
            MicrophoneStatus::Paused { .. }
        ));
        assert_eq!(controller.toggle_wav_pause("phone"), Some(false));
        assert!(matches!(
            controller.status_for("phone"),
            MicrophoneStatus::Active { .. }
        ));
    }

    #[test]
    #[ignore = "需要正在运行的 PulseAudio/PipeWire Pulse 兼容服务和默认输入设备"]
    fn pulse_input_stream_opens_without_blocking_callback_work() {
        let buffer = Arc::new(MicrophoneBuffer::default());
        let input =
            CpalMicrophoneInput::new(buffer.clone()).expect("create PulseAudio microphone input");
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(input.error.lock().unwrap().is_none());
        assert!(buffer.stats().frames_received > 0);
    }

    #[test]
    fn host_input_callback_period_matches_fifo_packet_cadence() {
        assert_eq!(HOST_INPUT_CALLBACK_FRAMES, 960);
    }
}
