//! 每个 managed session 独占的 PulseAudio 虚拟麦克风端点。
//!
//! Emulator 只看到这里创建的私有 FIFO source，而不会直接打开宿主默认麦克风。
//! PCM producer 必须写入 48 kHz、mono、S16LE，并在更高层负责实时节拍和取消。

use std::collections::{HashSet, VecDeque};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{
    FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::core::grpc_auth::GrpcJwtAuth;
use crate::core::instance::DeviceRuntime;
use crate::core::paths::APPLICATION_ID;
use crate::core::workspace::WorkspaceRoute;

pub const MICROPHONE_SAMPLE_RATE: u32 = 48_000;
pub const MICROPHONE_CHANNELS: u16 = 1;
const METADATA_VERSION: u32 = 1;
const METADATA_FILE: &str = "microphone.json";
const FIFO_FILE: &str = "microphone.pcm";
const FLATPAK_FIFO_PREFIX: &str = "liteavd-microphone-";
const FLATPAK_FIFO_SUFFIX: &str = ".pcm";
const MAX_METADATA_BYTES: u64 = 4 * 1024;
pub(crate) const PULSE_COOKIE_BYTES: usize = 256;
const FRAMES_PER_PACKET: usize = MICROPHONE_SAMPLE_RATE as usize / 50;
const FIFO_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const BUFFER_CAPACITY_FRAMES: usize = MICROPHONE_SAMPLE_RATE as usize * 120 / 1_000;

#[derive(Debug, Clone)]
pub enum MicrophoneSource {
    Host(Arc<MicrophoneBuffer>),
    Wav {
        path: PathBuf,
        paused: Arc<AtomicBool>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophonePumpExit {
    Canceled,
    EndOfFile,
}

#[derive(Debug, thiserror::Error)]
pub enum MicrophoneRunError {
    #[error("session route 已失效")]
    StaleRoute,
    #[error("session 没有可恢复的 JWT 控制身份")]
    Uncontrolled,
    #[error("session 没有虚拟麦克风端点")]
    Unavailable,
    #[error("WAV 不受支持：{0}")]
    Wav(#[from] WavError),
    #[error("虚拟麦克风 FIFO I/O 失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("虚拟麦克风 gRPC 失败：{0}")]
    Rpc(#[from] anyhow::Error),
    #[error("虚拟麦克风 worker 失败：{0}")]
    Worker(String),
}

/// 全应用共享的单路门禁。新的来源只有在旧来源完成关闭 RPC 后才能启用。
#[derive(Debug, Default)]
pub struct MicrophoneCoordinator {
    gate: tokio::sync::Mutex<()>,
}

impl MicrophoneCoordinator {
    pub async fn run(
        &self,
        runtime: Arc<DeviceRuntime>,
        route: WorkspaceRoute,
        source: MicrophoneSource,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<MicrophonePumpExit, MicrophoneRunError> {
        let _turn = self.gate.lock().await;
        if *cancel.borrow() {
            return Ok(MicrophonePumpExit::Canceled);
        }
        let prepared = match source {
            MicrophoneSource::Host(buffer) => PreparedSource::Host(buffer),
            MicrophoneSource::Wav { path, paused } => PreparedSource::Wav {
                reader: WavPcmReader::open(&path)?,
                paused,
            },
        };
        if !route_is_focused(&runtime, &route) {
            return Err(MicrophoneRunError::StaleRoute);
        }
        let endpoint = runtime
            .microphone_endpoint_for_route(&route)
            .ok_or(MicrophoneRunError::Unavailable)?;
        let client = runtime
            .grpc_client_for_route(&route)
            .ok_or(MicrophoneRunError::Uncontrolled)?
            .reconnect()
            .await?;
        if *cancel.borrow() || !route_is_focused(&runtime, &route) {
            return Ok(MicrophonePumpExit::Canceled);
        }
        if let Err(error) = client.set_microphone_enabled(true).await {
            let _ = client.set_microphone_enabled(false).await;
            return Err(error.into());
        }
        let enabled = client.microphone_state().await;
        if !matches!(enabled, Ok(true)) {
            let _ = client.set_microphone_enabled(false).await;
            if let Err(error) = enabled {
                return Err(error.into());
            }
            return Err(MicrophoneRunError::Rpc(anyhow::anyhow!("开启状态复验失败")));
        }

        let control_revision = runtime.control_stream_revision();
        let pump_runtime = runtime.clone();
        let pump_route = route.clone();
        let pump = tokio::task::spawn_blocking(move || {
            pump_fifo(
                &endpoint.fifo_path,
                prepared,
                &pump_runtime,
                &pump_route,
                control_revision,
                &mut cancel,
            )
        })
        .await;

        let disable = client.set_microphone_enabled(false).await;
        if let Err(error) = disable {
            return Err(MicrophoneRunError::Rpc(error.context("关闭虚拟麦克风失败")));
        }
        if client.microphone_state().await? {
            return Err(MicrophoneRunError::Rpc(anyhow::anyhow!("关闭状态复验失败")));
        }
        pump.map_err(|error| MicrophoneRunError::Worker(error.to_string()))?
    }
}

enum PreparedSource {
    Host(Arc<MicrophoneBuffer>),
    Wav {
        reader: WavPcmReader,
        paused: Arc<AtomicBool>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MicrophoneBufferStats {
    pub frames_received: u64,
    pub frames_dropped: u64,
    pub callback_contention: u64,
    pub underrun_frames: u64,
    pub queued_frames: usize,
}

/// CPAL input callback 使用的固定容量 mono ring。callback 只 try_lock，竞争时丢弃
/// 当前输入，不等待 FIFO 或网络。
#[derive(Debug)]
pub struct MicrophoneBuffer {
    samples: Mutex<VecDeque<i16>>,
    capacity: usize,
    frames_received: AtomicU64,
    frames_dropped: AtomicU64,
    callback_contention: AtomicU64,
    underrun_frames: AtomicU64,
}

impl Default for MicrophoneBuffer {
    fn default() -> Self {
        Self::new(BUFFER_CAPACITY_FRAMES)
    }
}

impl MicrophoneBuffer {
    pub fn new(capacity_frames: usize) -> Self {
        let capacity = capacity_frames.max(FRAMES_PER_PACKET);
        Self {
            samples: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            frames_received: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            callback_contention: AtomicU64::new(0),
            underrun_frames: AtomicU64::new(0),
        }
    }

    pub fn push_mono_i16(&self, input: &[i16]) {
        self.frames_received
            .fetch_add(input.len() as u64, Ordering::Relaxed);
        let Ok(mut samples) = self.samples.try_lock() else {
            self.frames_dropped
                .fetch_add(input.len() as u64, Ordering::Relaxed);
            self.callback_contention.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let kept = input.len().min(self.capacity);
        let skipped = input.len() - kept;
        let overflow = samples
            .len()
            .saturating_add(kept)
            .saturating_sub(self.capacity);
        for _ in 0..overflow {
            samples.pop_front();
        }
        samples.extend(&input[skipped..]);
        self.frames_dropped
            .fetch_add((skipped + overflow) as u64, Ordering::Relaxed);
    }

    fn fill_packet(&self, output: &mut [i16]) {
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut copied = 0;
        for sample in output.iter_mut() {
            if let Some(value) = samples.pop_front() {
                *sample = value;
                copied += 1;
            } else {
                *sample = 0;
            }
        }
        self.underrun_frames
            .fetch_add((output.len() - copied) as u64, Ordering::Relaxed);
    }

    pub fn stats(&self) -> MicrophoneBufferStats {
        MicrophoneBufferStats {
            frames_received: self.frames_received.load(Ordering::Relaxed),
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
            callback_contention: self.callback_contention.load(Ordering::Relaxed),
            underrun_frames: self.underrun_frames.load(Ordering::Relaxed),
            queued_frames: self
                .samples
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error("读取 WAV 失败：{0}")]
    Io(#[from] std::io::Error),
    #[error("不是 RIFF/WAVE 文件")]
    Container,
    #[error("WAV 缺少 fmt 或 data chunk")]
    MissingChunk,
    #[error("只支持 PCM WAV，实际 format={0}")]
    Encoding(u16),
    #[error("只支持 mono/stereo WAV，实际 channels={0}")]
    Channels(u16),
    #[error("只支持 1..={MICROPHONE_SAMPLE_RATE}Hz WAV，实际 rate={0}")]
    SampleRate(u32),
    #[error("只支持 U8/S16 WAV，实际 bits={0}")]
    Bits(u16),
    #[error("WAV fmt 与 frame 对齐信息不一致")]
    InvalidFormat,
    #[error("WAV data 未按完整 frame 对齐")]
    TruncatedFrame,
}

#[derive(Debug, Clone, Copy)]
struct WavFormat {
    channels: u16,
    sample_rate: u32,
    bits: u16,
    block_align: u16,
}

pub struct WavPcmReader {
    reader: BufReader<File>,
    format: WavFormat,
    remaining: u64,
    phase: u32,
    pending_sample: i16,
    pending_repeats: u32,
}

impl std::fmt::Debug for WavPcmReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WavPcmReader")
            .field("format", &self.format)
            .field("remaining", &self.remaining)
            .finish_non_exhaustive()
    }
}

impl WavPcmReader {
    pub fn open(path: &Path) -> Result<Self, WavError> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut header = [0_u8; 12];
        reader.read_exact(&mut header)?;
        if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
            return Err(WavError::Container);
        }
        let mut format = None;
        let mut data = None;
        while reader.stream_position()?.saturating_add(8) <= file_len {
            let mut chunk = [0_u8; 8];
            reader.read_exact(&mut chunk)?;
            let size = u64::from(u32::from_le_bytes(chunk[4..8].try_into().unwrap()));
            let payload = reader.stream_position()?;
            let end = payload
                .checked_add(size)
                .filter(|end| *end <= file_len)
                .ok_or(WavError::MissingChunk)?;
            match &chunk[..4] {
                b"fmt " => {
                    if size < 16 {
                        return Err(WavError::InvalidFormat);
                    }
                    let mut bytes = [0_u8; 16];
                    reader.read_exact(&mut bytes)?;
                    let encoding = u16::from_le_bytes([bytes[0], bytes[1]]);
                    if encoding != 1 {
                        return Err(WavError::Encoding(encoding));
                    }
                    let channels = u16::from_le_bytes([bytes[2], bytes[3]]);
                    if !matches!(channels, 1 | 2) {
                        return Err(WavError::Channels(channels));
                    }
                    let sample_rate = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
                    if sample_rate == 0 || sample_rate > MICROPHONE_SAMPLE_RATE {
                        return Err(WavError::SampleRate(sample_rate));
                    }
                    let block_align = u16::from_le_bytes([bytes[12], bytes[13]]);
                    let bits = u16::from_le_bytes([bytes[14], bytes[15]]);
                    if !matches!(bits, 8 | 16) {
                        return Err(WavError::Bits(bits));
                    }
                    if block_align != channels * (bits / 8) {
                        return Err(WavError::InvalidFormat);
                    }
                    format = Some(WavFormat {
                        channels,
                        sample_rate,
                        bits,
                        block_align,
                    });
                }
                b"data" => {
                    let Some(wav_format) = format else {
                        return Err(WavError::MissingChunk);
                    };
                    if !size.is_multiple_of(u64::from(wav_format.block_align)) {
                        return Err(WavError::TruncatedFrame);
                    }
                    data = Some((payload, size));
                    break;
                }
                _ => {}
            }
            reader.seek(SeekFrom::Start(end.saturating_add(size & 1)))?;
        }
        let format = format.ok_or(WavError::MissingChunk)?;
        let (offset, remaining) = data.ok_or(WavError::MissingChunk)?;
        reader.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            reader,
            format,
            remaining,
            phase: 0,
            pending_sample: 0,
            pending_repeats: 0,
        })
    }

    fn read_input_frame(&mut self) -> Result<Option<i16>, WavError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let mut frame = [0_u8; 4];
        let bytes = usize::from(self.format.block_align);
        self.reader.read_exact(&mut frame[..bytes])?;
        self.remaining -= bytes as u64;
        let decode = |offset: usize| match self.format.bits {
            8 => (i16::from(frame[offset]) - 128) << 8,
            16 => i16::from_le_bytes([frame[offset], frame[offset + 1]]),
            _ => unreachable!("validated WAV bits"),
        };
        let left = decode(0);
        if self.format.channels == 1 {
            Ok(Some(left))
        } else {
            let right = decode(usize::from(self.format.bits / 8));
            Ok(Some(((i32::from(left) + i32::from(right)) / 2) as i16))
        }
    }

    pub fn next_packet(&mut self) -> Result<Option<Vec<u8>>, WavError> {
        let mut output = Vec::with_capacity(FRAMES_PER_PACKET * 2);
        while output.len() < FRAMES_PER_PACKET * 2 {
            if self.pending_repeats == 0 {
                let Some(sample) = self.read_input_frame()? else {
                    break;
                };
                self.pending_sample = sample;
                self.phase += MICROPHONE_SAMPLE_RATE;
                self.pending_repeats = self.phase / self.format.sample_rate;
                self.phase %= self.format.sample_rate;
            }
            output.extend_from_slice(&self.pending_sample.to_le_bytes());
            self.pending_repeats -= 1;
        }
        if output.is_empty() {
            Ok(None)
        } else {
            Ok(Some(output))
        }
    }
}

fn pump_fifo(
    fifo_path: &Path,
    mut source: PreparedSource,
    runtime: &DeviceRuntime,
    route: &WorkspaceRoute,
    control_revision: u64,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<MicrophonePumpExit, MicrophoneRunError> {
    let Some(mut fifo) = open_fifo(fifo_path, runtime, route, control_revision, cancel)? else {
        return Ok(MicrophonePumpExit::Canceled);
    };
    let packet_duration = Duration::from_millis(20);
    let mut deadline = Instant::now();
    loop {
        if stopped(runtime, route, control_revision, cancel) {
            return Ok(MicrophonePumpExit::Canceled);
        }
        let bytes = match &mut source {
            PreparedSource::Host(buffer) => {
                let mut samples = [0_i16; FRAMES_PER_PACKET];
                buffer.fill_packet(&mut samples);
                samples
                    .into_iter()
                    .flat_map(i16::to_le_bytes)
                    .collect::<Vec<_>>()
            }
            PreparedSource::Wav { reader, paused } => {
                if paused.load(Ordering::Acquire) {
                    vec![0; FRAMES_PER_PACKET * 2]
                } else {
                    match reader.next_packet()? {
                        Some(bytes) => bytes,
                        None => return Ok(MicrophonePumpExit::EndOfFile),
                    }
                }
            }
        };
        write_fifo(&mut fifo, &bytes, runtime, route, control_revision, cancel)?;
        deadline += packet_duration;
        while Instant::now() < deadline {
            if stopped(runtime, route, control_revision, cancel) {
                return Ok(MicrophonePumpExit::Canceled);
            }
            std::thread::sleep((deadline - Instant::now()).min(Duration::from_millis(5)));
        }
        if Instant::now().saturating_duration_since(deadline) > Duration::from_millis(100) {
            deadline = Instant::now();
        }
    }
}

fn open_fifo(
    path: &Path,
    runtime: &DeviceRuntime,
    route: &WorkspaceRoute,
    control_revision: u64,
    cancel: &tokio::sync::watch::Receiver<bool>,
) -> Result<Option<File>, MicrophoneRunError> {
    let deadline = Instant::now() + FIFO_OPEN_TIMEOUT;
    loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => return Ok(Some(file)),
            Err(error)
                if error.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline =>
            {
                if stopped(runtime, route, control_revision, cancel) {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn write_fifo(
    fifo: &mut File,
    mut bytes: &[u8],
    runtime: &DeviceRuntime,
    route: &WorkspaceRoute,
    control_revision: u64,
    cancel: &tokio::sync::watch::Receiver<bool>,
) -> Result<(), MicrophoneRunError> {
    while !bytes.is_empty() {
        match fifo.write(bytes) {
            Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into()),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stopped(runtime, route, control_revision, cancel) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn stopped(
    runtime: &DeviceRuntime,
    route: &WorkspaceRoute,
    control_revision: u64,
    cancel: &tokio::sync::watch::Receiver<bool>,
) -> bool {
    *cancel.borrow()
        || !route_is_focused(runtime, route)
        || runtime.control_stream_revision() != control_revision
}

fn route_is_focused(runtime: &DeviceRuntime, route: &WorkspaceRoute) -> bool {
    runtime.route_is_current(route) && runtime.workspace_snapshot().focused.as_ref() == Some(route)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrophoneEndpointDescriptor {
    pub fifo_path: PathBuf,
    pub pulse_source: String,
    pub pulse_sink: String,
    pub pulse_server: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndpointMetadata {
    version: u32,
    pulse_source: String,
    source_module: u32,
    pulse_sink: String,
    sink_module: u32,
}

/// Pulse server 级模块由 session resource 持有。正常停止时卸载；仅应用退出并
/// 保留恢复身份时才让模块与 FIFO 继续存活。
pub(crate) struct PulseMicrophoneEndpoint {
    descriptor: MicrophoneEndpointDescriptor,
    source_module: u32,
    sink_module: u32,
    metadata_path: PathBuf,
    preserve_for_recovery: AtomicBool,
}

impl std::fmt::Debug for PulseMicrophoneEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PulseMicrophoneEndpoint")
            .field("descriptor", &self.descriptor)
            .field("source_module", &self.source_module)
            .field("sink_module", &self.sink_module)
            .finish_non_exhaustive()
    }
}

impl PulseMicrophoneEndpoint {
    pub(crate) fn create(auth: &GrpcJwtAuth) -> anyhow::Result<Self> {
        ensure_flatpak_pulse_cookie().map_err(anyhow::Error::msg)?;
        let pulse_source = format!("liteavd_mic_{}", auth.key_id());
        let pulse_sink = format!("liteavd_sink_{}", auth.key_id());
        let fifo_path = microphone_fifo_path(auth)?;
        cleanup_flatpak_orphan_fifos(auth.session_runtime_dir(), &fifo_path);
        create_private_fifo(&fifo_path)?;

        let sink_module = match load_module(
            "module-null-sink",
            &[
                format!("sink_name={pulse_sink}"),
                "rate=48000".to_owned(),
                "channels=2".to_owned(),
            ],
        ) {
            Ok(module) => module,
            Err(error) => {
                let _ = std::fs::remove_file(&fifo_path);
                return Err(error.context("创建虚拟麦克风静默输出 sink 失败"));
            }
        };
        let source_module = match load_module(
            "module-pipe-source",
            &[
                format!("source_name={pulse_source}"),
                format!("file={}", fifo_path.display()),
                "format=s16le".to_owned(),
                "rate=48000".to_owned(),
                "channels=1".to_owned(),
            ],
        ) {
            Ok(module) => module,
            Err(error) => {
                unload_module(sink_module);
                let _ = std::fs::remove_file(&fifo_path);
                return Err(error.context("创建虚拟麦克风 FIFO source 失败"));
            }
        };

        let metadata_path = auth.session_runtime_dir().join(METADATA_FILE);
        let metadata = EndpointMetadata {
            version: METADATA_VERSION,
            pulse_source: pulse_source.clone(),
            source_module,
            pulse_sink: pulse_sink.clone(),
            sink_module,
        };
        if let Err(error) = write_metadata(&metadata_path, &metadata) {
            unload_module(source_module);
            unload_module(sink_module);
            let _ = std::fs::remove_file(&fifo_path);
            return Err(error);
        }

        Ok(Self {
            descriptor: MicrophoneEndpointDescriptor {
                fifo_path,
                pulse_source,
                pulse_sink,
                pulse_server: pulse_server(),
            },
            source_module,
            sink_module,
            metadata_path,
            preserve_for_recovery: AtomicBool::new(false),
        })
    }

    pub(crate) fn recover(auth: &GrpcJwtAuth) -> anyhow::Result<Option<Self>> {
        let metadata_path = auth.session_runtime_dir().join(METADATA_FILE);
        let metadata = match read_metadata(&metadata_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let expected_source = format!("liteavd_mic_{}", auth.key_id());
        let expected_sink = format!("liteavd_sink_{}", auth.key_id());
        if metadata.version != METADATA_VERSION
            || metadata.pulse_source != expected_source
            || metadata.pulse_sink != expected_sink
        {
            bail!("虚拟麦克风恢复元数据与 JWT session 不匹配");
        }
        let fifo_path = microphone_fifo_path(auth)?;
        validate_private_fifo(&fifo_path)?;
        let modules = list_modules()?;
        if !module_matches(
            &modules,
            metadata.source_module,
            "module-pipe-source",
            &format!("source_name={expected_source}"),
        ) || !module_matches(
            &modules,
            metadata.sink_module,
            "module-null-sink",
            &format!("sink_name={expected_sink}"),
        ) {
            bail!("虚拟麦克风 Pulse 模块已经消失或身份不匹配");
        }
        Ok(Some(Self {
            descriptor: MicrophoneEndpointDescriptor {
                fifo_path,
                pulse_source: expected_source,
                pulse_sink: expected_sink,
                pulse_server: pulse_server(),
            },
            source_module: metadata.source_module,
            sink_module: metadata.sink_module,
            metadata_path,
            preserve_for_recovery: AtomicBool::new(false),
        }))
    }

    pub(crate) fn descriptor(&self) -> MicrophoneEndpointDescriptor {
        self.descriptor.clone()
    }

    pub(crate) fn preserve_recovery_on_drop(&self) {
        self.preserve_for_recovery.store(true, Ordering::Release);
    }
}

pub(crate) fn ensure_flatpak_pulse_cookie() -> Result<(), String> {
    if !crate::core::paths::is_flatpak() {
        return Ok(());
    }
    let home = std::env::var_os("HOME").ok_or_else(|| "Flatpak 内缺少 HOME".to_owned())?;
    ensure_pulse_cookie_at(&Path::new(&home).join(".config/pulse"))
}

pub(crate) fn ensure_pulse_cookie_at(directory: &Path) -> Result<(), String> {
    let cookie = directory.join("cookie");
    if cookie.exists() {
        return validate_private_pulse_cookie(&cookie);
    }
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("创建私有 PulseAudio 配置目录失败：{error}"))?;
    let directory_metadata = std::fs::symlink_metadata(directory)
        .map_err(|error| format!("读取私有 PulseAudio 配置目录失败：{error}"))?;
    if !directory_metadata.file_type().is_dir()
        || directory_metadata.uid() != unsafe { libc::getuid() }
    {
        return Err("私有 PulseAudio 配置路径不是当前用户的普通目录".into());
    }
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("收紧私有 PulseAudio 配置目录权限失败：{error}"))?;

    static NEXT_COOKIE: AtomicU64 = AtomicU64::new(0);
    let temporary = directory.join(format!(
        ".liteavd-cookie-{}-{}.tmp",
        std::process::id(),
        NEXT_COOKIE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&temporary)
        .map_err(|error| format!("创建临时 PulseAudio cookie 失败：{error}"))?;
    let publish = (|| -> std::io::Result<()> {
        file.write_all(&[0_u8; PULSE_COOKIE_BYTES])?;
        file.sync_all()?;
        match std::fs::hard_link(&temporary, &cookie) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error),
        }
    })();
    drop(file);
    let cleanup = std::fs::remove_file(&temporary);
    publish.map_err(|error| format!("发布私有 PulseAudio cookie 失败：{error}"))?;
    cleanup.map_err(|error| format!("清理临时 PulseAudio cookie 失败：{error}"))?;
    validate_private_pulse_cookie(&cookie)
}

fn validate_private_pulse_cookie(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("读取私有 PulseAudio cookie 失败：{error}"))?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.len() != PULSE_COOKIE_BYTES as u64
    {
        return Err(format!(
            "既有私有 PulseAudio cookie 不是当前用户的 {PULSE_COOKIE_BYTES}B 普通文件"
        ));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("既有私有 PulseAudio cookie 权限过宽".into());
    }
    Ok(())
}

impl Drop for PulseMicrophoneEndpoint {
    fn drop(&mut self) {
        if self.preserve_for_recovery.load(Ordering::Acquire) {
            return;
        }
        let source_module = self.source_module;
        let sink_module = self.sink_module;
        let fifo_path = self.descriptor.fifo_path.clone();
        let metadata_path = self.metadata_path.clone();
        let fallback_fifo = fifo_path.clone();
        let fallback_metadata = metadata_path.clone();
        if std::thread::Builder::new()
            .name("liteavd-microphone-cleanup".into())
            .spawn(move || cleanup_endpoint(source_module, sink_module, &fifo_path, &metadata_path))
            .is_err()
        {
            cleanup_endpoint(
                source_module,
                sink_module,
                &fallback_fifo,
                &fallback_metadata,
            );
        }
    }
}

fn cleanup_endpoint(source_module: u32, sink_module: u32, fifo: &Path, metadata: &Path) {
    unload_module(source_module);
    unload_module(sink_module);
    let _ = std::fs::remove_file(metadata);
    let _ = std::fs::remove_file(fifo);
}

/// `GrpcJwtAuth` 只会在确认 owner/engine PID 均已死亡后调用。仍需复验 metadata
/// 中的 module id、类型和 source/sink identity，避免仅按可伪造名称卸载。
pub(crate) fn cleanup_stale_auth_dir(auth_dir: &Path) {
    let metadata_path = auth_dir.join(METADATA_FILE);
    let Ok(metadata) = read_metadata(&metadata_path) else {
        return;
    };
    let Some(key_id) = metadata.pulse_source.strip_prefix("liteavd_mic_") else {
        return;
    };
    if metadata.version != METADATA_VERSION
        || metadata.pulse_sink != format!("liteavd_sink_{key_id}")
        || !valid_key_id(key_id)
    {
        return;
    }
    if let Ok(modules) = list_modules() {
        if module_matches(
            &modules,
            metadata.source_module,
            "module-pipe-source",
            &format!("source_name={}", metadata.pulse_source),
        ) {
            unload_module(metadata.source_module);
        }
        if module_matches(
            &modules,
            metadata.sink_module,
            "module-null-sink",
            &format!("sink_name={}", metadata.pulse_sink),
        ) {
            unload_module(metadata.sink_module);
        }
    }
    if let Ok(fifo_path) = microphone_fifo_path_for(auth_dir, key_id)
        && validate_private_fifo(&fifo_path).is_ok()
    {
        let _ = std::fs::remove_file(fifo_path);
    }
}

fn microphone_fifo_path(auth: &GrpcJwtAuth) -> anyhow::Result<PathBuf> {
    microphone_fifo_path_for(auth.session_runtime_dir(), auth.key_id())
}

fn microphone_fifo_path_for(auth_dir: &Path, key_id: &str) -> anyhow::Result<PathBuf> {
    microphone_fifo_path_for_context(
        auth_dir,
        key_id,
        std::env::var_os("FLATPAK_ID").as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
    )
}

fn microphone_fifo_path_for_context(
    auth_dir: &Path,
    key_id: &str,
    flatpak_id: Option<&std::ffi::OsStr>,
    runtime: Option<&std::ffi::OsStr>,
) -> anyhow::Result<PathBuf> {
    if !valid_key_id(key_id) {
        bail!("虚拟麦克风 session key id 非法");
    }
    let Some(flatpak_id) = flatpak_id else {
        return Ok(auth_dir.join(FIFO_FILE));
    };
    if flatpak_id != std::ffi::OsStr::new(APPLICATION_ID) {
        bail!("Flatpak application id 与固定产品 id 不匹配");
    }
    let runtime = runtime
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("Flatpak 内缺少 XDG_RUNTIME_DIR"))?;
    let shared = runtime.join("app").join(APPLICATION_ID);
    let metadata = std::fs::symlink_metadata(&shared).with_context(|| {
        format!(
            "读取 Flatpak 宿主共享 runtime 目录失败：{}",
            shared.display()
        )
    })?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!(
            "Flatpak 宿主共享 runtime 目录类型、所有者或权限不安全：{}",
            shared.display()
        );
    }
    let owner_pid = auth_owner_pid(auth_dir, key_id)?;
    Ok(shared.join(format!(
        "{FLATPAK_FIFO_PREFIX}{owner_pid}-{key_id}{FLATPAK_FIFO_SUFFIX}"
    )))
}

fn valid_key_id(key_id: &str) -> bool {
    !key_id.is_empty()
        && key_id.len() <= 64
        && key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn auth_owner_pid(auth_dir: &Path, key_id: &str) -> anyhow::Result<u32> {
    let directory = auth_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("虚拟麦克风 auth 目录名非法"))?;
    directory
        .strip_suffix(&format!("-{key_id}"))
        .and_then(|pid| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0)
        .ok_or_else(|| anyhow::anyhow!("虚拟麦克风 auth 目录与 session 不匹配"))
}

fn cleanup_flatpak_orphan_fifos(auth_dir: &Path, fifo_path: &Path) {
    if !crate::core::paths::is_flatpak() {
        return;
    }
    let Some(auth_parent) = auth_dir.parent() else {
        return;
    };
    let Some(shared) = fifo_path.parent() else {
        return;
    };
    let mut protected = HashSet::new();
    if let Ok(entries) = std::fs::read_dir(auth_parent) {
        for entry in entries.flatten() {
            let metadata_path = entry.path().join(METADATA_FILE);
            let Ok(metadata) = read_metadata(&metadata_path) else {
                continue;
            };
            let Some(key_id) = metadata.pulse_source.strip_prefix("liteavd_mic_") else {
                continue;
            };
            if metadata.version != METADATA_VERSION
                || metadata.pulse_sink != format!("liteavd_sink_{key_id}")
                || !valid_key_id(key_id)
            {
                continue;
            }
            if let Ok(path) = microphone_fifo_path_for(&entry.path(), key_id)
                && let Some(name) = path.file_name()
            {
                protected.insert(name.to_os_string());
            }
        }
    }
    let Ok(entries) = std::fs::read_dir(shared) else {
        return;
    };
    for entry in entries.flatten() {
        if protected.contains(&entry.file_name()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some((owner_pid, _)) = flatpak_fifo_identity(&name) else {
            continue;
        };
        if Path::new("/proc").join(owner_pid.to_string()).exists() {
            continue;
        }
        let path = entry.path();
        if validate_private_fifo(&path).is_ok() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn flatpak_fifo_identity(name: &str) -> Option<(u32, &str)> {
    let identity = name
        .strip_prefix(FLATPAK_FIFO_PREFIX)?
        .strip_suffix(FLATPAK_FIFO_SUFFIX)?;
    let (pid, key_id) = identity.split_once('-')?;
    let pid = pid.parse::<u32>().ok().filter(|pid| *pid > 0)?;
    valid_key_id(key_id).then_some((pid, key_id))
}

fn create_private_fifo(path: &Path) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        bail!("虚拟麦克风 FIFO 路径已存在：{}", path.display());
    }
    let path_c = CString::new(path.as_os_str().as_bytes()).context("FIFO 路径包含 NUL")?;
    // SAFETY: `path_c` is NUL terminated and points inside a validated private directory.
    if unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("创建虚拟麦克风 FIFO 失败：{}", path.display()));
    }
    validate_private_fifo(path)
}

fn validate_private_fifo(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("检查虚拟麦克风 FIFO 失败：{}", path.display()))?;
    if !metadata.file_type().is_fifo()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        bail!(
            "虚拟麦克风 FIFO 类型、所有者或权限不安全：{}",
            path.display()
        );
    }
    Ok(())
}

fn load_module(name: &str, arguments: &[String]) -> anyhow::Result<u32> {
    let output = Command::new("pactl")
        .arg("load-module")
        .arg(name)
        .args(arguments)
        .output()
        .with_context(|| format!("执行 pactl load-module {name} 失败"))?;
    if !output.status.success() {
        bail!(
            "pactl load-module {name} 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_module_id(&output.stdout)
}

fn parse_module_id(output: &[u8]) -> anyhow::Result<u32> {
    let text = std::str::from_utf8(output).context("pactl module id 不是 UTF-8")?;
    let trimmed = text.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("pactl 返回非法 module id：{trimmed:?}");
    }
    trimmed.parse().context("pactl module id 超出范围")
}

fn unload_module(module: u32) {
    let _ = Command::new("pactl")
        .args(["unload-module", &module.to_string()])
        .output();
}

fn list_modules() -> anyhow::Result<String> {
    let output = Command::new("pactl")
        .args(["list", "short", "modules"])
        .output()
        .context("执行 pactl list modules 失败")?;
    if !output.status.success() {
        bail!(
            "pactl list modules 失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("pactl modules 输出不是 UTF-8")
}

fn module_matches(modules: &str, id: u32, module_name: &str, identity: &str) -> bool {
    modules.lines().any(|line| {
        let mut fields = line.split('\t');
        fields.next() == Some(id.to_string().as_str())
            && fields.next() == Some(module_name)
            && fields.next().is_some_and(|arguments| {
                arguments
                    .split_ascii_whitespace()
                    .any(|argument| argument == identity)
            })
    })
}

fn write_metadata(path: &Path, metadata: &EndpointMetadata) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(metadata)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("创建虚拟麦克风恢复元数据失败：{}", path.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    File::open(path.parent().expect("metadata has parent"))?.sync_all()?;
    Ok(())
}

fn read_metadata(path: &Path) -> std::io::Result<EndpointMetadata> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > MAX_METADATA_BYTES
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "虚拟麦克风恢复元数据类型、所有者、权限或长度不安全",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_METADATA_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "虚拟麦克风恢复元数据过大",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn pulse_server() -> Option<String> {
    std::env::var("PULSE_SERVER").ok().or_else(|| {
        std::env::var_os("XDG_RUNTIME_DIR").map(|runtime| {
            PathBuf::from(runtime)
                .join("pulse/native")
                .to_string_lossy()
                .into_owned()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::core::emulator::RunningInstance;
    use crate::core::grpc_auth::GrpcJwtAuth;

    fn wav_file(
        label: &str,
        format: u16,
        channels: u16,
        rate: u32,
        bits: u16,
        data: &[u8],
    ) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "liteavd-microphone-{label}-{}-{}.wav",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let block_align = channels * (bits / 8);
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36_u32 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&format.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(data);
        std::fs::write(&path, wav).unwrap();
        path
    }

    #[test]
    fn parses_only_unsigned_decimal_module_ids() {
        assert_eq!(parse_module_id(b"42\n").unwrap(), 42);
        for invalid in [b"".as_slice(), b"-1", b"4 2", b"42 trailing", b"x"] {
            assert!(parse_module_id(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn module_identity_match_is_exact() {
        let modules = "42\tmodule-pipe-source\tsource_name=liteavd_mic_exact rate=48000\t\n\
                       43\tmodule-null-sink\tsink_name=liteavd_sink_exact\t\n";
        assert!(module_matches(
            modules,
            42,
            "module-pipe-source",
            "source_name=liteavd_mic_exact"
        ));
        assert!(!module_matches(
            modules,
            42,
            "module-pipe-source",
            "source_name=liteavd_mic"
        ));
        assert!(!module_matches(
            modules,
            43,
            "module-pipe-source",
            "sink_name=liteavd_sink_exact"
        ));
    }

    #[test]
    fn flatpak_fifo_uses_the_host_visible_private_app_runtime_directory() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "liteavd-flatpak-microphone-path-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let shared = root.join("app").join(APPLICATION_ID);
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
        let app_id = std::ffi::OsStr::new(APPLICATION_ID);
        let key_id = "session_Abc-123";
        let auth = root.join(format!("4242-{key_id}"));

        assert_eq!(
            microphone_fifo_path_for_context(&auth, key_id, None, None).unwrap(),
            auth.join(FIFO_FILE)
        );
        assert_eq!(
            microphone_fifo_path_for_context(&auth, key_id, Some(app_id), Some(root.as_os_str()))
                .unwrap(),
            shared.join(format!(
                "{FLATPAK_FIFO_PREFIX}4242-{key_id}{FLATPAK_FIFO_SUFFIX}"
            ))
        );
        assert_eq!(
            flatpak_fifo_identity(&format!(
                "{FLATPAK_FIFO_PREFIX}4242-{key_id}{FLATPAK_FIFO_SUFFIX}"
            )),
            Some((4242, key_id))
        );
        assert!(
            microphone_fifo_path_for_context(
                &auth,
                "../escape",
                Some(app_id),
                Some(root.as_os_str())
            )
            .is_err()
        );
        assert!(
            microphone_fifo_path_for_context(
                &auth,
                key_id,
                Some(std::ffi::OsStr::new("wrong.app")),
                Some(root.as_os_str())
            )
            .is_err()
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wav_u8_mono_is_streamed_and_resampled_to_48khz_s16() {
        let path = wav_file("u8", 1, 1, 24_000, 8, &[0, 255]);
        let mut reader = WavPcmReader::open(&path).unwrap();
        let packet = reader.next_packet().unwrap().unwrap();
        let samples: Vec<_> = packet
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect();
        assert_eq!(samples, [-32_768, -32_768, 32_512, 32_512]);
        assert!(reader.next_packet().unwrap().is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn wav_stereo_s16_is_downmixed_without_full_file_buffering() {
        let mut data = Vec::new();
        data.extend_from_slice(&10_000_i16.to_le_bytes());
        data.extend_from_slice(&(-2_000_i16).to_le_bytes());
        let path = wav_file("stereo", 1, 2, 48_000, 16, &data);
        let mut reader = WavPcmReader::open(&path).unwrap();
        assert_eq!(reader.remaining, 4);
        assert_eq!(
            reader.next_packet().unwrap().unwrap(),
            4_000_i16.to_le_bytes()
        );
        assert_eq!(reader.remaining, 0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn wav_rejects_non_pcm_high_rate_and_partial_frames() {
        let float = wav_file("float", 3, 1, 48_000, 16, &[0, 0]);
        assert!(matches!(
            WavPcmReader::open(&float),
            Err(WavError::Encoding(3))
        ));
        std::fs::remove_file(float).unwrap();

        let high = wav_file("high", 1, 1, 96_000, 16, &[0, 0]);
        assert!(matches!(
            WavPcmReader::open(&high),
            Err(WavError::SampleRate(96_000))
        ));
        std::fs::remove_file(high).unwrap();

        let partial = wav_file("partial", 1, 2, 48_000, 16, &[0, 0]);
        assert!(matches!(
            WavPcmReader::open(&partial),
            Err(WavError::TruncatedFrame)
        ));
        std::fs::remove_file(partial).unwrap();
    }

    #[test]
    fn host_callback_buffer_is_bounded_and_reports_drops_and_underruns() {
        let buffer = MicrophoneBuffer::new(FRAMES_PER_PACKET);
        buffer.push_mono_i16(&vec![7; FRAMES_PER_PACKET + 10]);
        let mut output = vec![0; FRAMES_PER_PACKET + 20];
        buffer.fill_packet(&mut output);
        assert!(
            output[..FRAMES_PER_PACKET]
                .iter()
                .all(|sample| *sample == 7)
        );
        assert!(
            output[FRAMES_PER_PACKET..]
                .iter()
                .all(|sample| *sample == 0)
        );
        let stats = buffer.stats();
        assert_eq!(stats.queued_frames, 0);
        assert_eq!(stats.frames_dropped, 10);
        assert_eq!(stats.underrun_frames, 20);
    }

    #[test]
    fn exact_route_must_also_remain_focused() {
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![
            RunningInstance {
                pid: 10_001,
                ini_path: "/tmp/phone.ini".into(),
                avd_name: "phone".into(),
                console_port: 5554,
                adb_port: 5555,
                grpc_port: 8554,
                grpc_allowlist: None,
                grpc_jwks: None,
                grpc_jwk_active: None,
            },
            RunningInstance {
                pid: 10_002,
                ini_path: "/tmp/tablet.ini".into(),
                avd_name: "tablet".into(),
                console_port: 5556,
                adb_port: 5557,
                grpc_port: 8556,
                grpc_allowlist: None,
                grpc_jwks: None,
                grpc_jwk_active: None,
            },
        ]);
        let phone = runtime.focus_session("phone").unwrap();
        assert!(route_is_focused(&runtime, &phone));
        runtime.focus_session("tablet").unwrap();
        assert!(!route_is_focused(&runtime, &phone));
        assert!(runtime.route_is_current(&phone));
    }

    #[test]
    #[ignore = "需要正在运行且支持 module-pipe-source/module-null-sink 的 Pulse 服务"]
    fn pulse_endpoint_recovery_preserves_identity_then_exact_drop_unloads_modules() {
        let auth = GrpcJwtAuth::new().unwrap();
        let endpoint = PulseMicrophoneEndpoint::create(&auth).unwrap();
        let descriptor = endpoint.descriptor();
        endpoint.preserve_recovery_on_drop();
        drop(endpoint);
        assert!(descriptor.fifo_path.exists());

        let recovered = PulseMicrophoneEndpoint::recover(&auth)
            .unwrap()
            .expect("recover preserved endpoint");
        assert_eq!(recovered.descriptor(), descriptor);
        drop(recovered);

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let modules = list_modules().unwrap();
            if !modules.contains(&descriptor.pulse_source)
                && !modules.contains(&descriptor.pulse_sink)
                && !descriptor.fifo_path.exists()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Pulse endpoint cleanup timed out"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
