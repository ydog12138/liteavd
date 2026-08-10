//! `-share-vid` 共享内存帧读取与 latest-frame capture。
//!
//! Emulator 37.1.11 的已验证布局为 24 字节 little-endian header，随后是
//! `width * height * 4` BGRA 像素。文件事件与 frame counter 都只作为提示；
//! 读取必须重新验证尺寸、映射长度和复制前后的 header 一致性。

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use memmap2::{Mmap, MmapOptions};
use thiserror::Error;

pub const SHARE_VID_HEADER_LEN: usize = 24;
pub const BYTES_PER_PIXEL: usize = 4;
pub const MAX_FRAME_DIMENSION: u32 = 8192;
pub const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

const DEFAULT_ATTACH_RETRY: Duration = Duration::from_millis(100);
const DEFAULT_FRAME_WAIT: Duration = Duration::from_millis(1);
const CONSISTENCY_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frame_counter: u32,
    pub timestamp_ns: u64,
    pub stride: u32,
}

#[derive(Debug)]
pub struct Frame {
    pub meta: FrameMeta,
    pub pixels: Vec<u8>,
    pub observed_at: Instant,
    pub copied_at: Instant,
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("share-vid I/O 失败：{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("share-vid header 过短：需要 {SHARE_VID_HEADER_LEN}B，实际 {actual}B")]
    HeaderTooShort { actual: usize },
    #[error("share-vid 映射超过允许上限 {maximum}B：实际 {actual}B")]
    MappingTooLarge { actual: u64, maximum: usize },
    #[error("share-vid 尺寸不能为零：{width}x{height}")]
    ZeroDimensions { width: u32, height: u32 },
    #[error("share-vid 尺寸超过上限 {MAX_FRAME_DIMENSION}：{width}x{height}")]
    DimensionsTooLarge { width: u32, height: u32 },
    #[error("share-vid 尺寸算术溢出：{width}x{height}")]
    ArithmeticOverflow { width: u32, height: u32 },
    #[error("share-vid 像素区超过 {MAX_FRAME_BYTES}B 上限：{actual}B")]
    FrameTooLarge { actual: usize },
    #[error("share-vid 映射长度不符：header 需要 {expected}B，实际 {actual}B")]
    LengthMismatch { expected: usize, actual: usize },
    #[error("share-vid 连续 {attempts} 次复制期间 header 都发生变化")]
    UnstableFrame { attempts: usize },
    #[error("创建 share-vid capture 线程失败：{0}")]
    ThreadSpawn(#[source] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    width: u32,
    height: u32,
    fps: u32,
    frame_counter: u32,
    timestamp_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceIdentity {
    device: u64,
    inode: u64,
    len: u64,
}

/// 单线程同步 reader。调用者可直接采样，也可由 `CaptureHandle` 后台驱动。
#[derive(Debug)]
pub struct ShareVidReader {
    path: PathBuf,
    file: File,
    mapping: Mmap,
    identity: SourceIdentity,
    last_header: Option<Header>,
}

impl ShareVidReader {
    pub fn open(console_port: u16) -> Result<Self, CaptureError> {
        Self::open_path(share_vid_path(console_port))
    }

    pub fn open_path(path: impl Into<PathBuf>) -> Result<Self, CaptureError> {
        let path = path.into();
        let (file, mapping, identity) = map_path(&path)?;
        Ok(Self {
            path,
            file,
            mapping,
            identity,
            last_header: None,
        })
    }

    /// 返回 counter/header 变化后的最新完整帧；无变化时返回 `None`。
    pub fn read_latest(&mut self) -> Result<Option<Frame>, CaptureError> {
        self.read_latest_inner(|_| {})
    }

    fn read_latest_inner(
        &mut self,
        mut after_first_header: impl FnMut(usize),
    ) -> Result<Option<Frame>, CaptureError> {
        self.refresh_mapping()?;
        for attempt in 0..CONSISTENCY_ATTEMPTS {
            let before = read_header(&self.mapping)?;
            if self.last_header == Some(before) {
                return Ok(None);
            }
            let (stride, pixel_len, required_len) = frame_layout(before.width, before.height)?;
            if self.mapping.len() != required_len {
                return Err(CaptureError::LengthMismatch {
                    expected: required_len,
                    actual: self.mapping.len(),
                });
            }

            let observed_at = Instant::now();
            after_first_header(attempt);
            let mut pixels = vec![0_u8; pixel_len];
            // SAFETY: required_len was checked against the immutable mapping length and the
            // destination owns pixel_len bytes. The emulator is an external writer; the second
            // header read detects a concurrent frame/dimension update and discards that copy.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.mapping.as_ptr().add(SHARE_VID_HEADER_LEN),
                    pixels.as_mut_ptr(),
                    pixel_len,
                );
            }
            let after = read_header(&self.mapping)?;
            if before == after {
                let copied_at = Instant::now();
                self.last_header = Some(after);
                return Ok(Some(Frame {
                    meta: FrameMeta {
                        width: after.width,
                        height: after.height,
                        fps: after.fps,
                        frame_counter: after.frame_counter,
                        timestamp_ns: after.timestamp_ns,
                        stride,
                    },
                    pixels,
                    observed_at,
                    copied_at,
                }));
            }
            std::hint::spin_loop();
        }
        Err(CaptureError::UnstableFrame {
            attempts: CONSISTENCY_ATTEMPTS,
        })
    }

    fn refresh_mapping(&mut self) -> Result<(), CaptureError> {
        let metadata = std::fs::metadata(&self.path).map_err(|source| CaptureError::Io {
            path: self.path.clone(),
            source,
        })?;
        let next = identity(&metadata);
        if next == self.identity {
            return Ok(());
        }
        let (file, mapping, identity) = map_path(&self.path)?;
        self.file = file;
        self.mapping = mapping;
        self.identity = identity;
        self.last_header = None;
        Ok(())
    }
}

pub fn share_vid_path(console_port: u16) -> PathBuf {
    PathBuf::from("/dev/shm").join(format!("videmulator{console_port}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureStats {
    pub frames_published: u64,
    pub frames_dropped: u64,
    pub attach_retries: u64,
    pub unstable_frames: u64,
    pub last_copy_micros: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Default)]
struct AtomicCaptureStats {
    frames_published: AtomicU64,
    frames_dropped: AtomicU64,
    attach_retries: AtomicU64,
    unstable_frames: AtomicU64,
    last_copy_micros: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl AtomicCaptureStats {
    fn snapshot(&self) -> CaptureStats {
        CaptureStats {
            frames_published: self.frames_published.load(Ordering::Relaxed),
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
            attach_retries: self.attach_retries.load(Ordering::Relaxed),
            unstable_frames: self.unstable_frames.load(Ordering::Relaxed),
            last_copy_micros: self.last_copy_micros.load(Ordering::Relaxed),
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        }
    }

    fn set_error(&self, error: Option<String>) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = error;
    }
}

#[derive(Debug, Default)]
struct LatestFrameState {
    frame: Option<Arc<Frame>>,
    sequence: u64,
    last_observed_sequence: u64,
    closed: bool,
}

#[derive(Debug, Default)]
struct LatestFrameSlot {
    state: Mutex<LatestFrameState>,
    changed: Condvar,
}

/// 一个只读取最新帧的订阅；同一订阅不会重复返回相同 sequence。
#[derive(Debug, Clone)]
pub struct CaptureSubscription {
    slot: Arc<LatestFrameSlot>,
    seen_sequence: u64,
}

impl CaptureSubscription {
    pub fn take_latest(&mut self) -> Option<Arc<Frame>> {
        let mut state = self
            .slot
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        take_changed_frame(&mut state, &mut self.seen_sequence)
    }

    pub fn wait_timeout(&mut self, timeout: Duration) -> Option<Arc<Frame>> {
        let state = self
            .slot
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (mut state, _) = self
            .slot
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.sequence == self.seen_sequence && !state.closed
            })
            .unwrap_or_else(|error| error.into_inner());
        take_changed_frame(&mut state, &mut self.seen_sequence)
    }

    pub fn is_closed(&self) -> bool {
        self.slot
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed
    }
}

/// 后台 capture 句柄。drop 会取消 attach/read 并等待线程退出。
#[derive(Debug)]
pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    slot: Arc<LatestFrameSlot>,
    stats: Arc<AtomicCaptureStats>,
    worker: Option<JoinHandle<()>>,
}

impl CaptureHandle {
    pub fn start(console_port: u16) -> Result<Self, CaptureError> {
        Self::start_path(share_vid_path(console_port))
    }

    pub fn start_path(path: impl Into<PathBuf>) -> Result<Self, CaptureError> {
        let path = path.into();
        let stop = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(LatestFrameSlot::default());
        let stats = Arc::new(AtomicCaptureStats::default());
        let worker_stop = stop.clone();
        let worker_slot = slot.clone();
        let worker_stats = stats.clone();
        let worker = std::thread::Builder::new()
            .name("liteavd-share-vid".into())
            .spawn(move || capture_loop(path, worker_stop, worker_slot, worker_stats))
            .map_err(CaptureError::ThreadSpawn)?;
        Ok(Self {
            stop,
            slot,
            stats,
            worker: Some(worker),
        })
    }

    pub fn subscribe(&self) -> CaptureSubscription {
        CaptureSubscription {
            slot: self.slot.clone(),
            seen_sequence: 0,
        }
    }

    pub fn stats(&self) -> CaptureStats {
        self.stats.snapshot()
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

fn capture_loop(
    path: PathBuf,
    stop: Arc<AtomicBool>,
    slot: Arc<LatestFrameSlot>,
    stats: Arc<AtomicCaptureStats>,
) {
    let mut reader = None;
    while !stop.load(Ordering::Acquire) {
        if reader.is_none() {
            match ShareVidReader::open_path(path.clone()) {
                Ok(attached) => reader = Some(attached),
                Err(error) => {
                    stats.attach_retries.fetch_add(1, Ordering::Relaxed);
                    stats.set_error(Some(error.to_string()));
                    std::thread::park_timeout(DEFAULT_ATTACH_RETRY);
                    continue;
                }
            }
        }

        let started = Instant::now();
        match reader
            .as_mut()
            .expect("reader attached above")
            .read_latest()
        {
            Ok(Some(frame)) => {
                stats.last_copy_micros.store(
                    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
                    Ordering::Relaxed,
                );
                stats.set_error(None);
                publish_frame(&slot, &stats, Arc::new(frame));
            }
            Ok(None) => std::thread::park_timeout(DEFAULT_FRAME_WAIT),
            Err(error @ CaptureError::UnstableFrame { .. }) => {
                stats.unstable_frames.fetch_add(1, Ordering::Relaxed);
                stats.set_error(Some(error.to_string()));
                std::thread::park_timeout(DEFAULT_FRAME_WAIT);
            }
            Err(error) => {
                stats.attach_retries.fetch_add(1, Ordering::Relaxed);
                stats.set_error(Some(error.to_string()));
                reader = None;
                std::thread::park_timeout(DEFAULT_ATTACH_RETRY);
            }
        }
    }

    let mut state = slot.state.lock().unwrap_or_else(|error| error.into_inner());
    state.closed = true;
    slot.changed.notify_all();
}

fn publish_frame(slot: &LatestFrameSlot, stats: &AtomicCaptureStats, frame: Arc<Frame>) {
    let mut state = slot.state.lock().unwrap_or_else(|error| error.into_inner());
    if state.sequence > state.last_observed_sequence {
        stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
    }
    state.sequence = state.sequence.wrapping_add(1).max(1);
    state.frame = Some(frame);
    stats.frames_published.fetch_add(1, Ordering::Relaxed);
    slot.changed.notify_all();
}

fn take_changed_frame(state: &mut LatestFrameState, seen_sequence: &mut u64) -> Option<Arc<Frame>> {
    if state.sequence == *seen_sequence {
        return None;
    }
    *seen_sequence = state.sequence;
    state.last_observed_sequence = state.last_observed_sequence.max(state.sequence);
    state.frame.clone()
}

fn map_path(path: &Path) -> Result<(File, Mmap, SourceIdentity), CaptureError> {
    let file = File::open(path).map_err(|source| CaptureError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| CaptureError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let maximum = SHARE_VID_HEADER_LEN + MAX_FRAME_BYTES;
    if metadata.len() > maximum as u64 {
        return Err(CaptureError::MappingTooLarge {
            actual: metadata.len(),
            maximum,
        });
    }
    let len = usize::try_from(metadata.len()).map_err(|_| CaptureError::MappingTooLarge {
        actual: metadata.len(),
        maximum,
    })?;
    if len < SHARE_VID_HEADER_LEN {
        return Err(CaptureError::HeaderTooShort { actual: len });
    }
    // SAFETY: the file remains owned by ShareVidReader for at least as long as the mapping.
    // Header/length validation and copy consistency are performed before pixels are exposed.
    let mapping =
        unsafe { MmapOptions::new().len(len).map(&file) }.map_err(|source| CaptureError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok((file, mapping, identity(&metadata)))
}

fn identity(metadata: &std::fs::Metadata) -> SourceIdentity {
    SourceIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        len: metadata.len(),
    }
}

fn read_header(mapping: &Mmap) -> Result<Header, CaptureError> {
    if mapping.len() < SHARE_VID_HEADER_LEN {
        return Err(CaptureError::HeaderTooShort {
            actual: mapping.len(),
        });
    }
    let pointer = mapping.as_ptr();
    // SAFETY: mmap is page-aligned and offsets 0/4/8/12/16 have natural alignment. Volatile
    // reads are required because an external emulator process updates this shared header.
    let (width, height, fps, frame_counter, timestamp_ns) = unsafe {
        (
            u32::from_le(std::ptr::read_volatile(pointer.cast::<u32>())),
            u32::from_le(std::ptr::read_volatile(pointer.add(4).cast::<u32>())),
            u32::from_le(std::ptr::read_volatile(pointer.add(8).cast::<u32>())),
            u32::from_le(std::ptr::read_volatile(pointer.add(12).cast::<u32>())),
            u64::from_le(std::ptr::read_volatile(pointer.add(16).cast::<u64>())),
        )
    };
    Ok(Header {
        width,
        height,
        fps,
        frame_counter,
        timestamp_ns,
    })
}

fn frame_layout(width: u32, height: u32) -> Result<(u32, usize, usize), CaptureError> {
    if width == 0 || height == 0 {
        return Err(CaptureError::ZeroDimensions { width, height });
    }
    let stride = width
        .checked_mul(BYTES_PER_PIXEL as u32)
        .ok_or(CaptureError::ArithmeticOverflow { width, height })?;
    let pixel_len_u32 = stride
        .checked_mul(height)
        .ok_or(CaptureError::ArithmeticOverflow { width, height })?;
    if width > MAX_FRAME_DIMENSION || height > MAX_FRAME_DIMENSION {
        return Err(CaptureError::DimensionsTooLarge { width, height });
    }
    let pixel_len = usize::try_from(pixel_len_u32)
        .map_err(|_| CaptureError::ArithmeticOverflow { width, height })?;
    if pixel_len > MAX_FRAME_BYTES {
        return Err(CaptureError::FrameTooLarge { actual: pixel_len });
    }
    let required_len = SHARE_VID_HEADER_LEN
        .checked_add(pixel_len)
        .ok_or(CaptureError::ArithmeticOverflow { width, height })?;
    Ok((stride, pixel_len, required_len))
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    fn fixture_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "liteavd-share-vid-{label}-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn frame_bytes(width: u32, height: u32, counter: u32, pixel: u8) -> Vec<u8> {
        let (_, pixel_len, total) = frame_layout(width, height).unwrap();
        let mut bytes = vec![0_u8; total];
        bytes[0..4].copy_from_slice(&width.to_le_bytes());
        bytes[4..8].copy_from_slice(&height.to_le_bytes());
        bytes[8..12].copy_from_slice(&60_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&counter.to_le_bytes());
        bytes[16..24].copy_from_slice(&(u64::from(counter) * 1_000).to_le_bytes());
        bytes[SHARE_VID_HEADER_LEN..].fill(pixel);
        assert_eq!(bytes.len(), SHARE_VID_HEADER_LEN + pixel_len);
        bytes
    }

    fn replace_fixture(path: &Path, bytes: &[u8]) {
        let temporary = path.with_extension(format!(
            "tmp-{}",
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&temporary, bytes).unwrap();
        std::fs::rename(temporary, path).unwrap();
    }

    #[test]
    fn reads_valid_bgra_frame_and_detects_no_change() {
        let path = fixture_path("valid");
        replace_fixture(&path, &frame_bytes(4, 3, 7, 0x5a));
        let mut reader = ShareVidReader::open_path(&path).unwrap();
        let frame = reader.read_latest().unwrap().unwrap();
        assert_eq!((frame.meta.width, frame.meta.height), (4, 3));
        assert_eq!(frame.meta.stride, 16);
        assert_eq!(frame.meta.frame_counter, 7);
        assert_eq!(frame.pixels, vec![0x5a; 48]);
        assert!(reader.read_latest().unwrap().is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_short_length_mismatch_limits_and_overflow() {
        let short = fixture_path("short");
        std::fs::write(&short, [0_u8; 12]).unwrap();
        assert!(matches!(
            ShareVidReader::open_path(&short),
            Err(CaptureError::HeaderTooShort { actual: 12 })
        ));
        std::fs::remove_file(short).unwrap();

        let mismatch = fixture_path("mismatch");
        let mut bytes = frame_bytes(2, 2, 1, 0);
        bytes.pop();
        replace_fixture(&mismatch, &bytes);
        let mut reader = ShareVidReader::open_path(&mismatch).unwrap();
        assert!(matches!(
            reader.read_latest(),
            Err(CaptureError::LengthMismatch { .. })
        ));
        std::fs::remove_file(mismatch).unwrap();

        assert!(matches!(
            frame_layout(u32::MAX, 2),
            Err(CaptureError::ArithmeticOverflow { .. })
        ));
        assert!(matches!(
            frame_layout(MAX_FRAME_DIMENSION + 1, 1),
            Err(CaptureError::DimensionsTooLarge { .. })
        ));

        let oversized = fixture_path("oversized");
        let oversized_file = File::create(&oversized).unwrap();
        oversized_file
            .set_len((SHARE_VID_HEADER_LEN + MAX_FRAME_BYTES + 1) as u64)
            .unwrap();
        assert!(matches!(
            ShareVidReader::open_path(&oversized),
            Err(CaptureError::MappingTooLarge { .. })
        ));
        std::fs::remove_file(oversized).unwrap();
    }

    #[test]
    fn remaps_when_inode_or_dimensions_change() {
        let path = fixture_path("resize");
        replace_fixture(&path, &frame_bytes(2, 2, 1, 0x11));
        let mut reader = ShareVidReader::open_path(&path).unwrap();
        assert_eq!(reader.read_latest().unwrap().unwrap().pixels.len(), 16);

        replace_fixture(&path, &frame_bytes(3, 2, 2, 0x22));
        let resized = reader.read_latest().unwrap().unwrap();
        assert_eq!((resized.meta.width, resized.meta.height), (3, 2));
        assert_eq!(resized.pixels.len(), 24);
        assert!(resized.pixels.iter().all(|byte| *byte == 0x22));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn discards_copy_when_counter_changes_mid_frame() {
        let path = fixture_path("counter-race");
        replace_fixture(&path, &frame_bytes(8, 8, 1, 0x33));
        let mut reader = ShareVidReader::open_path(&path).unwrap();
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let result = reader.read_latest_inner(|attempt| {
            file.seek(SeekFrom::Start(12)).unwrap();
            file.write_all(&(attempt as u32 + 2).to_le_bytes()).unwrap();
            file.flush().unwrap();
        });
        assert!(matches!(result, Err(CaptureError::UnstableFrame { .. })));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn capture_attaches_late_reconnects_and_only_keeps_latest() {
        let path = fixture_path("capture");
        let handle = CaptureHandle::start_path(&path).unwrap();
        let mut subscription = handle.subscribe();
        std::thread::sleep(Duration::from_millis(130));
        assert!(handle.stats().attach_retries > 0);

        replace_fixture(&path, &frame_bytes(2, 2, 1, 0x10));
        let first = subscription
            .wait_timeout(Duration::from_secs(2))
            .expect("capture 未 attach");
        assert_eq!(first.meta.frame_counter, 1);

        std::fs::remove_file(&path).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        for counter in 2..=8 {
            replace_fixture(&path, &frame_bytes(3, 1, counter, counter as u8));
            std::thread::sleep(Duration::from_millis(15));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let latest = loop {
            if let Some(frame) = subscription.take_latest()
                && frame.meta.frame_counter == 8
            {
                break frame;
            }
            assert!(Instant::now() < deadline, "capture 未发布重连后的最新帧");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!((latest.meta.width, latest.meta.height), (3, 1));
        assert!(handle.stats().frames_dropped > 0);

        let drop_started = Instant::now();
        drop(handle);
        assert!(drop_started.elapsed() < Duration::from_secs(1));
        assert!(subscription.is_closed());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cancel_during_attach_is_prompt() {
        let path = fixture_path("cancel");
        let handle = CaptureHandle::start_path(path).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let started = Instant::now();
        drop(handle);
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
