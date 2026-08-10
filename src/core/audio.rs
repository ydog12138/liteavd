//! Guest 扬声器 PCM 的格式边界、有界缓冲与可取消 stream pump。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{Stream, StreamExt};
use thiserror::Error;
use tokio::sync::watch;

use crate::core::grpc::{AudioChannels, AudioPacket, AudioSampleFormat, AudioStreamConnectError};
use crate::core::instance::DeviceRuntime;
use crate::core::workspace::WorkspaceRoute;

pub const OUTPUT_SAMPLE_RATE: u64 = 48_000;
pub const OUTPUT_CHANNELS: usize = 2;
pub const BYTES_PER_SAMPLE: usize = 2;
pub const BYTES_PER_FRAME: usize = OUTPUT_CHANNELS * BYTES_PER_SAMPLE;
pub const DEFAULT_BUFFER_MS: usize = 120;
pub const DEFAULT_PREBUFFER_MS: usize = 60;
pub const MAX_AUDIO_PACKET_BYTES: usize = 64 * 1024;
const VOLUME_UNITY: u16 = 1_000;
const FAST_START_MS: usize = 5;
const DECLICK_MILLIS: u64 = 5;
const DECLICK_FRAMES: u16 = (OUTPUT_SAMPLE_RATE * DECLICK_MILLIS / 1_000) as u16;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AudioPacketError {
    #[error("AudioPacket 缺少 format")]
    MissingFormat,
    #[error("streamAudio 采样率必须为 {OUTPUT_SAMPLE_RATE}Hz，实际 {0}Hz")]
    SampleRate(u64),
    #[error("streamAudio 必须为 stereo，实际 channels={0}")]
    Channels(i32),
    #[error("streamAudio 必须为 S16LE，实际 format={0}")]
    SampleFormat(i32),
    #[error("streamAudio packet 为空")]
    Empty,
    #[error("streamAudio packet 超过 {MAX_AUDIO_PACKET_BYTES}B：实际 {0}B")]
    TooLarge(usize),
    #[error("streamAudio packet 未按 {BYTES_PER_FRAME}B stereo frame 对齐：实际 {0}B")]
    Unaligned(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedAudioPacket<'a> {
    pub timestamp_micros: u64,
    pub s16le: &'a [u8],
}

pub fn validate_packet(packet: &AudioPacket) -> Result<ValidatedAudioPacket<'_>, AudioPacketError> {
    let format = packet
        .format
        .as_ref()
        .ok_or(AudioPacketError::MissingFormat)?;
    if format.sampling_rate != OUTPUT_SAMPLE_RATE {
        return Err(AudioPacketError::SampleRate(format.sampling_rate));
    }
    if format.channels != AudioChannels::Stereo as i32 {
        return Err(AudioPacketError::Channels(format.channels));
    }
    if format.format != AudioSampleFormat::AudFmtS16 as i32 {
        return Err(AudioPacketError::SampleFormat(format.format));
    }
    let len = packet.audio.len();
    if len == 0 {
        return Err(AudioPacketError::Empty);
    }
    if len > MAX_AUDIO_PACKET_BYTES {
        return Err(AudioPacketError::TooLarge(len));
    }
    if !len.is_multiple_of(BYTES_PER_FRAME) {
        return Err(AudioPacketError::Unaligned(len));
    }
    Ok(ValidatedAudioPacket {
        timestamp_micros: packet.timestamp,
        s16le: &packet.audio,
    })
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AudioBufferError {
    #[error("音频缓冲必须至少容纳一个 stereo frame")]
    EmptyCapacity,
    #[error("预缓冲不能超过总缓冲：prebuffer={prebuffer_samples}, capacity={capacity_samples}")]
    PrebufferTooLarge {
        prebuffer_samples: usize,
        capacity_samples: usize,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioBufferStats {
    pub samples_received: u64,
    pub samples_played: u64,
    pub samples_dropped: u64,
    pub underrun_samples: u64,
    pub contention_callbacks: u64,
    pub queued_samples: usize,
    pub primed: bool,
}

#[derive(Debug, Default)]
struct BufferCounters {
    samples_received: AtomicU64,
    samples_played: AtomicU64,
    samples_dropped: AtomicU64,
    underrun_samples: AtomicU64,
    contention_callbacks: AtomicU64,
}

/// 固定容量 stereo i16 环形语义缓冲。producer 可以短暂锁定；实时 callback
/// 只 `try_lock`，竞争时直接填静音，不阻塞音频线程。
#[derive(Debug)]
pub struct AudioBuffer {
    samples: Mutex<VecDeque<i16>>,
    capacity_samples: usize,
    prebuffer_samples: usize,
    fast_start_samples: usize,
    fast_start_pending: AtomicBool,
    primed: AtomicBool,
    muted: AtomicBool,
    volume_milli: AtomicU16,
    fade_in_frames: AtomicU16,
    fade_out_frames: AtomicU16,
    last_left: AtomicI32,
    last_right: AtomicI32,
    counters: BufferCounters,
}

impl Default for AudioBuffer {
    fn default() -> Self {
        Self::from_millis(DEFAULT_BUFFER_MS, DEFAULT_PREBUFFER_MS)
            .expect("default audio buffer configuration is valid")
    }
}

impl AudioBuffer {
    pub fn from_millis(capacity_ms: usize, prebuffer_ms: usize) -> Result<Self, AudioBufferError> {
        let samples_per_ms = OUTPUT_SAMPLE_RATE as usize * OUTPUT_CHANNELS / 1_000;
        let mut buffer = Self::new(
            capacity_ms.saturating_mul(samples_per_ms),
            prebuffer_ms.saturating_mul(samples_per_ms),
        )?;
        if prebuffer_ms > 0 {
            buffer.fast_start_samples =
                (FAST_START_MS * OUTPUT_SAMPLE_RATE as usize * OUTPUT_CHANNELS / 1_000)
                    .min(buffer.prebuffer_samples);
            buffer.fast_start_pending.store(true, Ordering::Release);
            buffer.primed.store(false, Ordering::Release);
        }
        buffer
            .fade_in_frames
            .store(DECLICK_FRAMES, Ordering::Release);
        Ok(buffer)
    }

    pub fn new(
        capacity_samples: usize,
        prebuffer_samples: usize,
    ) -> Result<Self, AudioBufferError> {
        let capacity_samples = capacity_samples / OUTPUT_CHANNELS * OUTPUT_CHANNELS;
        let prebuffer_samples = prebuffer_samples / OUTPUT_CHANNELS * OUTPUT_CHANNELS;
        if capacity_samples < OUTPUT_CHANNELS {
            return Err(AudioBufferError::EmptyCapacity);
        }
        if prebuffer_samples > capacity_samples {
            return Err(AudioBufferError::PrebufferTooLarge {
                prebuffer_samples,
                capacity_samples,
            });
        }
        Ok(Self {
            samples: Mutex::new(VecDeque::with_capacity(capacity_samples)),
            capacity_samples,
            prebuffer_samples,
            fast_start_samples: prebuffer_samples,
            fast_start_pending: AtomicBool::new(false),
            primed: AtomicBool::new(prebuffer_samples == 0),
            muted: AtomicBool::new(false),
            volume_milli: AtomicU16::new(VOLUME_UNITY),
            fade_in_frames: AtomicU16::new(0),
            fade_out_frames: AtomicU16::new(0),
            last_left: AtomicI32::new(0),
            last_right: AtomicI32::new(0),
            counters: BufferCounters::default(),
        })
    }

    pub fn push_s16le(&self, bytes: &[u8]) {
        debug_assert!(!bytes.is_empty() && bytes.len().is_multiple_of(BYTES_PER_FRAME));
        let incoming_samples = bytes.len() / BYTES_PER_SAMPLE;
        self.counters
            .samples_received
            .fetch_add(incoming_samples as u64, Ordering::Relaxed);
        let keep_samples = incoming_samples.min(self.capacity_samples);
        let skip_samples = incoming_samples - keep_samples;
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let overflow = samples
            .len()
            .saturating_add(keep_samples)
            .saturating_sub(self.capacity_samples);
        for _ in 0..overflow {
            samples.pop_front();
        }
        for pair in bytes[skip_samples * BYTES_PER_SAMPLE..].chunks_exact(BYTES_PER_SAMPLE) {
            samples.push_back(i16::from_le_bytes([pair[0], pair[1]]));
        }
        self.counters
            .samples_dropped
            .fetch_add((skip_samples + overflow) as u64, Ordering::Relaxed);
        let prebuffer_samples = if self.fast_start_pending.load(Ordering::Acquire) {
            self.fast_start_samples
        } else {
            self.prebuffer_samples
        };
        if samples.len() >= prebuffer_samples {
            self.primed.store(true, Ordering::Release);
            self.fast_start_pending.store(false, Ordering::Release);
        }
    }

    /// CPAL output callback：不阻塞、不分配；数据不足或 producer 正持锁时填静音。
    pub fn fill_i16(&self, output: &mut [i16]) {
        if self.fill_fade_out(output) {
            return;
        }
        if !self.primed.load(Ordering::Acquire) {
            output.fill(0);
            self.counters
                .underrun_samples
                .fetch_add(output.len() as u64, Ordering::Relaxed);
            return;
        }
        let Ok(mut samples) = self.samples.try_lock() else {
            output.fill(0);
            self.counters
                .underrun_samples
                .fetch_add(output.len() as u64, Ordering::Relaxed);
            self.counters
                .contention_callbacks
                .fetch_add(1, Ordering::Relaxed);
            return;
        };
        let muted = self.muted.load(Ordering::Relaxed);
        let volume = i32::from(self.volume_milli.load(Ordering::Relaxed));
        let mut fade_in = self.fade_in_frames.load(Ordering::Acquire);
        let mut played = 0_u64;
        let mut underrun = 0_u64;
        let mut last_left = 0_i32;
        let mut last_right = 0_i32;
        for (index, sample) in output.iter_mut().enumerate() {
            if let Some(value) = samples.pop_front() {
                let mut scaled = if muted {
                    0
                } else {
                    (i32::from(value) * volume / i32::from(VOLUME_UNITY))
                        .clamp(i32::from(i16::MIN), i32::from(i16::MAX))
                };
                if fade_in > 0 {
                    let progress = i32::from(DECLICK_FRAMES - fade_in + 1);
                    scaled = scaled * progress / i32::from(DECLICK_FRAMES);
                    if index % OUTPUT_CHANNELS == OUTPUT_CHANNELS - 1 {
                        fade_in -= 1;
                    }
                }
                *sample = scaled as i16;
                if index % OUTPUT_CHANNELS == 0 {
                    last_left = scaled;
                } else {
                    last_right = scaled;
                }
                played += 1;
            } else {
                *sample = 0;
                underrun += 1;
            }
        }
        if underrun > 0 {
            self.primed.store(false, Ordering::Release);
        }
        self.fade_in_frames.store(fade_in, Ordering::Release);
        if underrun == 0 && played > 0 {
            self.last_left.store(last_left, Ordering::Release);
            self.last_right.store(last_right, Ordering::Release);
        } else if underrun > 0 {
            self.last_left.store(0, Ordering::Release);
            self.last_right.store(0, Ordering::Release);
        }
        self.counters
            .samples_played
            .fetch_add(played, Ordering::Relaxed);
        self.counters
            .underrun_samples
            .fetch_add(underrun, Ordering::Relaxed);
    }

    pub fn clear(&self) {
        self.samples
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.primed
            .store(self.prebuffer_samples == 0, Ordering::Release);
        self.fast_start_pending
            .store(self.prebuffer_samples > 0, Ordering::Release);
        let has_tail = self.last_left.load(Ordering::Acquire) != 0
            || self.last_right.load(Ordering::Acquire) != 0;
        self.fade_out_frames
            .store(if has_tail { DECLICK_FRAMES } else { 0 }, Ordering::Release);
        self.fade_in_frames.store(DECLICK_FRAMES, Ordering::Release);
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }

    pub fn set_volume(&self, volume: f32) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.volume_milli.store(
            (volume * f32::from(VOLUME_UNITY)).round() as u16,
            Ordering::Release,
        );
    }

    pub fn volume(&self) -> f32 {
        f32::from(self.volume_milli.load(Ordering::Acquire)) / f32::from(VOLUME_UNITY)
    }

    pub fn stats(&self) -> AudioBufferStats {
        let queued_samples = self
            .samples
            .try_lock()
            .map(|samples| samples.len())
            .unwrap_or(0);
        AudioBufferStats {
            samples_received: self.counters.samples_received.load(Ordering::Relaxed),
            samples_played: self.counters.samples_played.load(Ordering::Relaxed),
            samples_dropped: self.counters.samples_dropped.load(Ordering::Relaxed),
            underrun_samples: self.counters.underrun_samples.load(Ordering::Relaxed),
            contention_callbacks: self.counters.contention_callbacks.load(Ordering::Relaxed),
            queued_samples,
            primed: self.primed.load(Ordering::Acquire),
        }
    }

    fn fill_fade_out(&self, output: &mut [i16]) -> bool {
        let mut remaining = self.fade_out_frames.load(Ordering::Acquire);
        if remaining == 0 {
            return false;
        }
        let left = self.last_left.load(Ordering::Relaxed);
        let right = self.last_right.load(Ordering::Relaxed);
        for frame in output.chunks_mut(OUTPUT_CHANNELS) {
            if remaining == 0 {
                frame.fill(0);
                continue;
            }
            let gain = i32::from(remaining);
            frame[0] = (left * gain / i32::from(DECLICK_FRAMES)) as i16;
            if let Some(sample) = frame.get_mut(1) {
                *sample = (right * gain / i32::from(DECLICK_FRAMES)) as i16;
            }
            remaining -= 1;
        }
        self.fade_out_frames.store(remaining, Ordering::Release);
        if remaining == 0 {
            self.last_left.store(0, Ordering::Release);
            self.last_right.store(0, Ordering::Release);
        }
        self.counters
            .underrun_samples
            .fetch_add(output.len() as u64, Ordering::Relaxed);
        true
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("音频 sink 失败：{0}")]
pub struct AudioSinkError(pub String);

pub trait AudioSink: Send {
    fn write(&mut self, packet: ValidatedAudioPacket<'_>) -> Result<(), AudioSinkError>;
    fn clear(&mut self);
}

impl<T: AudioSink + ?Sized> AudioSink for &mut T {
    fn write(&mut self, packet: ValidatedAudioPacket<'_>) -> Result<(), AudioSinkError> {
        (**self).write(packet)
    }

    fn clear(&mut self) {
        (**self).clear();
    }
}

impl AudioSink for Arc<AudioBuffer> {
    fn write(&mut self, packet: ValidatedAudioPacket<'_>) -> Result<(), AudioSinkError> {
        self.push_s16le(packet.s16le);
        Ok(())
    }

    fn clear(&mut self) {
        AudioBuffer::clear(self);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioPumpStats {
    pub packets: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioPumpExit {
    Canceled(AudioPumpStats),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AudioStreamError {
    #[error("音频 route 已失效")]
    StaleRoute,
    #[error("streamAudio 数据无效：{0}")]
    InvalidPacket(#[from] AudioPacketError),
    #[error("streamAudio RPC 失败（{code:?}）：{message}")]
    Rpc { code: tonic::Code, message: String },
    #[error("streamAudio 意外结束")]
    UnexpectedEnd,
    #[error(transparent)]
    Sink(#[from] AudioSinkError),
    #[error("streamAudio 连接失败：{0}")]
    Connect(String),
}

pub async fn pump_audio_stream<S, K>(
    mut source: S,
    mut sink: K,
    mut cancel: watch::Receiver<bool>,
    mut route_is_current: impl FnMut() -> bool,
) -> Result<AudioPumpExit, AudioStreamError>
where
    S: Stream<Item = Result<AudioPacket, tonic::Status>> + Unpin,
    K: AudioSink,
{
    let mut stats = AudioPumpStats::default();
    let result = loop {
        if *cancel.borrow() {
            break Ok(AudioPumpExit::Canceled(stats));
        }
        if !route_is_current() {
            break Err(AudioStreamError::StaleRoute);
        }
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    break Ok(AudioPumpExit::Canceled(stats));
                }
            }
            packet = source.next() => {
                let packet = match packet {
                    Some(Ok(packet)) => packet,
                    Some(Err(status)) => break Err(AudioStreamError::Rpc {
                        code: status.code(),
                        message: status.message().to_owned(),
                    }),
                    None => break Err(AudioStreamError::UnexpectedEnd),
                };
                if !route_is_current() {
                    break Err(AudioStreamError::StaleRoute);
                }
                let packet = match validate_packet(&packet) {
                    Ok(packet) => packet,
                    Err(error) => break Err(AudioStreamError::InvalidPacket(error)),
                };
                stats.packets += 1;
                stats.bytes += packet.s16le.len() as u64;
                if let Err(error) = sink.write(packet) {
                    break Err(AudioStreamError::Sink(error));
                }
            }
        }
    };
    sink.clear();
    result
}

pub async fn run_route_audio<K: AudioSink>(
    runtime: Arc<DeviceRuntime>,
    route: WorkspaceRoute,
    mut sink: K,
    mut cancel: watch::Receiver<bool>,
) -> Result<AudioPumpExit, AudioStreamError> {
    let result = async {
        let config = runtime
            .grpc_client_for_route(&route)
            .ok_or(AudioStreamError::StaleRoute)?;
        if *cancel.borrow() {
            return Ok(AudioPumpExit::Canceled(AudioPumpStats::default()));
        }
        let reconnect = config.reconnect();
        tokio::pin!(reconnect);
        let client = loop {
            tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        return Ok(AudioPumpExit::Canceled(AudioPumpStats::default()));
                    }
                }
                connected = &mut reconnect => break connected
                    .map_err(|error| AudioStreamError::Connect(format!("{error:#}")))?,
            }
        };
        if !runtime.route_is_current(&route) {
            return Err(AudioStreamError::StaleRoute);
        }
        let stream = loop {
            tokio::select! {
                changed = cancel.changed() => {
                    if changed.is_err() || *cancel.borrow() {
                        return Ok(AudioPumpExit::Canceled(AudioPumpStats::default()));
                    }
                }
                result = client.stream_audio_output() => match result {
                    Ok(stream) => break stream,
                    // A silent guest does not produce response headers/first packet. Retry the
                    // bounded request while this exact route remains focused.
                    Err(AudioStreamConnectError::Timeout) => {
                        if !runtime.route_is_current(&route) {
                            return Err(AudioStreamError::StaleRoute);
                        }
                    }
                    Err(AudioStreamConnectError::Rpc { code, message }) => {
                        return Err(AudioStreamError::Rpc { code, message });
                    }
                    Err(AudioStreamConnectError::Disconnected(error)) => {
                        return Err(AudioStreamError::Connect(error));
                    }
                }
            }
        };
        let check_runtime = runtime.clone();
        let check_route = route.clone();
        pump_audio_stream(stream, &mut sink, cancel, move || {
            check_runtime.route_is_current(&check_route)
        })
        .await
    }
    .await;
    sink.clear();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::grpc::AudioFormat;

    fn packet(samples: &[i16]) -> AudioPacket {
        AudioPacket {
            format: Some(AudioFormat {
                sampling_rate: OUTPUT_SAMPLE_RATE,
                channels: AudioChannels::Stereo as i32,
                format: AudioSampleFormat::AudFmtS16 as i32,
                ..Default::default()
            }),
            timestamp: 42,
            audio: samples
                .iter()
                .flat_map(|sample| sample.to_le_bytes())
                .collect(),
        }
    }

    #[test]
    fn packet_validation_rejects_format_size_and_alignment() {
        let valid = packet(&[1, -1, 2, -2]);
        assert_eq!(validate_packet(&valid).unwrap().timestamp_micros, 42);
        let mut invalid = valid.clone();
        invalid.format.as_mut().unwrap().sampling_rate = 44_100;
        assert_eq!(
            validate_packet(&invalid),
            Err(AudioPacketError::SampleRate(44_100))
        );
        let mut invalid = valid.clone();
        invalid.audio.clear();
        assert_eq!(validate_packet(&invalid), Err(AudioPacketError::Empty));
        let mut invalid = valid;
        invalid.audio.pop();
        assert_eq!(
            validate_packet(&invalid),
            Err(AudioPacketError::Unaligned(7))
        );
    }

    #[test]
    fn bounded_buffer_drops_oldest_and_rebuffers_after_underrun() {
        let buffer = AudioBuffer::new(8, 4).unwrap();
        buffer.push_s16le(&packet(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).audio);
        let mut output = [0_i16; 8];
        buffer.fill_i16(&mut output);
        assert_eq!(output, [3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(buffer.stats().samples_dropped, 2);
        let mut silence = [1_i16; 4];
        buffer.fill_i16(&mut silence);
        assert_eq!(silence, [0; 4]);
        assert!(!buffer.stats().primed);
    }

    #[test]
    fn callback_volume_mute_and_underflow_are_bounded() {
        let buffer = AudioBuffer::new(8, 0).unwrap();
        buffer.set_volume(0.5);
        buffer.push_s16le(&packet(&[1_000, -1_000, 2_000, -2_000]).audio);
        let mut output = [0_i16; 4];
        buffer.fill_i16(&mut output);
        assert_eq!(output, [500, -500, 1_000, -1_000]);
        buffer.set_muted(true);
        buffer.push_s16le(&packet(&[5, 6, 7, 8]).audio);
        buffer.fill_i16(&mut output);
        assert_eq!(output, [0; 4]);
        assert_eq!(buffer.volume(), 0.5);
        assert!(buffer.is_muted());
    }

    #[test]
    fn default_buffer_fast_starts_then_uses_full_rebuffer_after_underrun() {
        let buffer = AudioBuffer::default();
        let fast_start_samples =
            FAST_START_MS * OUTPUT_SAMPLE_RATE as usize * OUTPUT_CHANNELS / 1_000;
        let fast_start = vec![1_000_i16; fast_start_samples];
        buffer.push_s16le(&packet(&fast_start).audio);
        assert!(buffer.stats().primed);
        let mut output = vec![0_i16; fast_start_samples];
        buffer.fill_i16(&mut output);
        assert!(output.iter().any(|sample| *sample != 0));

        buffer.fill_i16(&mut output);
        assert!(!buffer.stats().primed);
        buffer.push_s16le(&packet(&fast_start).audio);
        assert!(
            !buffer.stats().primed,
            "欠载后的重新缓冲不能重复使用 fast-start 阈值"
        );
        let remaining = DEFAULT_PREBUFFER_MS * OUTPUT_SAMPLE_RATE as usize * OUTPUT_CHANNELS
            / 1_000
            - fast_start_samples;
        buffer.push_s16le(&packet(&vec![1_000_i16; remaining]).audio);
        assert!(buffer.stats().primed);

        let explicit = AudioBuffer::new(960, 600).unwrap();
        explicit.clear();
        explicit.push_s16le(&packet(&vec![1_000_i16; 480]).audio);
        assert!(
            !explicit.stats().primed,
            "通用 sample 构造器在 clear 后必须保留调用者的 prebuffer 语义"
        );
    }

    #[test]
    fn default_buffer_declicks_start_and_clear_without_allocating_a_tail() {
        let buffer = AudioBuffer::from_millis(20, 0).unwrap();
        let samples = vec![10_000_i16; usize::from(DECLICK_FRAMES) * OUTPUT_CHANNELS];
        buffer.push_s16le(&packet(&samples).audio);
        let mut ramp_in = vec![0_i16; samples.len()];
        buffer.fill_i16(&mut ramp_in);
        assert!(ramp_in[0].abs() < 100);
        assert!(ramp_in.last().unwrap().abs() >= 9_900);

        buffer.clear();
        let mut ramp_out = vec![0_i16; samples.len()];
        buffer.fill_i16(&mut ramp_out);
        assert!(ramp_out[0].abs() >= 9_900);
        assert!(ramp_out.last().unwrap().abs() < 100);
        let mut silence = [1_i16; 4];
        buffer.fill_i16(&mut silence);
        assert_eq!(silence, [0; 4]);
    }

    #[derive(Clone)]
    struct FakeSink {
        state: Arc<Mutex<(Vec<Vec<u8>>, usize)>>,
    }

    impl AudioSink for FakeSink {
        fn write(&mut self, packet: ValidatedAudioPacket<'_>) -> Result<(), AudioSinkError> {
            self.state.lock().unwrap().0.push(packet.s16le.to_vec());
            Ok(())
        }

        fn clear(&mut self) {
            self.state.lock().unwrap().1 += 1;
        }
    }

    #[tokio::test]
    async fn stream_pump_cancels_clears_and_rejects_stale_route() {
        let state = Arc::new(Mutex::new((Vec::new(), 0)));
        let sink = FakeSink {
            state: state.clone(),
        };
        let (cancel, receiver) = watch::channel(false);
        cancel.send(true).unwrap();
        let exit = pump_audio_stream(futures_util::stream::pending(), sink, receiver, || true)
            .await
            .unwrap();
        assert_eq!(exit, AudioPumpExit::Canceled(AudioPumpStats::default()));
        assert_eq!(state.lock().unwrap().1, 1);

        let sink = FakeSink {
            state: state.clone(),
        };
        let (_cancel, receiver) = watch::channel(false);
        assert_eq!(
            pump_audio_stream(
                futures_util::stream::iter([Ok(packet(&[1, 2]))]),
                sink,
                receiver,
                || false,
            )
            .await,
            Err(AudioStreamError::StaleRoute)
        );
        assert_eq!(state.lock().unwrap().1, 2);
    }

    #[tokio::test]
    async fn stream_pump_validates_before_writing() {
        let state = Arc::new(Mutex::new((Vec::new(), 0)));
        let sink = FakeSink {
            state: state.clone(),
        };
        let (_cancel, receiver) = watch::channel(false);
        let mut invalid = packet(&[1, 2]);
        invalid.audio.pop();
        assert_eq!(
            pump_audio_stream(
                futures_util::stream::iter([Ok(invalid)]),
                sink,
                receiver,
                || true,
            )
            .await,
            Err(AudioStreamError::InvalidPacket(
                AudioPacketError::Unaligned(3)
            ))
        );
        assert!(state.lock().unwrap().0.is_empty());
        assert_eq!(state.lock().unwrap().1, 1);
    }
}
