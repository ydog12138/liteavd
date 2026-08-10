//! 广告目录事件监听。
//!
//! 文件事件只表示“事实可能变化”；消费者收到提示后必须重新执行一次全量扫描，
//! 不能把单个 create/remove/rename 事件直接解释为实例生命周期。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::thread::{JoinHandle, Thread};

use anyhow::{Context, bail};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::core::emulator;

#[derive(Debug, Clone, Copy)]
enum MonitorSignal {
    Rescan,
}

/// 广告目录监控句柄。drop 会停止并等待监控线程退出。
pub struct AdvertisementMonitor {
    signal_tx: SyncSender<MonitorSignal>,
    worker: Option<JoinHandle<()>>,
    worker_thread: Thread,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for AdvertisementMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdvertisementMonitor")
            .finish_non_exhaustive()
    }
}

impl AdvertisementMonitor {
    /// 监视模拟器默认广告目录。启动成功后立即触发一次初始全量 rescan。
    pub fn start(on_rescan: impl Fn() + Send + 'static) -> anyhow::Result<Self> {
        Self::start_at(emulator::advertisement_dir(), on_rescan)
    }

    /// 显式要求一次全量 rescan；用于手工刷新及事件 overflow 后的恢复入口。
    pub fn request_rescan(&self) {
        let _ = self.signal_tx.try_send(MonitorSignal::Rescan);
    }

    fn start_at(target: PathBuf, on_rescan: impl Fn() + Send + 'static) -> anyhow::Result<Self> {
        let (signal_tx, signal_rx) = sync_channel(1);
        let watcher_tx = signal_tx.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let event_target = target.clone();
        let (ready_tx, ready_rx) = sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("liteavd-advertisements".into())
            .spawn(move || {
                let event_tx = watcher_tx.clone();
                let watcher_result =
                    notify::recommended_watcher(move |event: notify::Result<Event>| match event {
                        Ok(event) if event_is_relevant(&event, &event_target) => {
                            let _ = event_tx.try_send(MonitorSignal::Rescan);
                        }
                        Ok(_) => {}
                        Err(_) => {
                            // inotify overflow、watch invalidation 等错误都降级为全量 rescan。
                            let _ = event_tx.try_send(MonitorSignal::Rescan);
                        }
                    });
                let mut watcher = match watcher_result {
                    Ok(watcher) => watcher,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                let mut watched = match bind_deepest_existing(&mut watcher, &target, None) {
                    Ok(path) => path,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                on_rescan();

                while signal_rx.recv().is_ok() {
                    drain_signals(&signal_rx);
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    // emulator 可能直接写 ini；短暂合并同一批 create/modify 事件，
                    // 避免在文件尚未完整时把已知 session 误判为消失。
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    drain_signals(&signal_rx);
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Ok(next) =
                        bind_deepest_existing(&mut watcher, &target, Some(watched.as_path()))
                    {
                        watched = next;
                    }
                    on_rescan();
                }
            })
            .context("创建广告目录监控线程失败")?;
        let worker_thread = worker.thread().clone();

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                signal_tx,
                worker: Some(worker),
                worker_thread,
                stop,
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                bail!("启动广告目录监控失败：{error}")
            }
            Err(_) => {
                let _ = worker.join();
                bail!("广告目录监控线程在初始化期间退出")
            }
        }
    }
}

impl Drop for AdvertisementMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.signal_tx.try_send(MonitorSignal::Rescan);
        // 若未来通过循环引用在回调自身析构句柄，只分离 JoinHandle，避免 self-join；
        // stop flag 会让线程在回调返回后退出。
        if std::thread::current().id() == self.worker_thread.id() {
            self.worker.take();
            return;
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn drain_signals(rx: &Receiver<MonitorSignal>) {
    loop {
        match rx.try_recv() {
            Ok(MonitorSignal::Rescan) => {}
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
        }
    }
}

fn event_is_relevant(event: &Event, target: &Path) -> bool {
    event.paths.is_empty()
        || event
            .paths
            .iter()
            .any(|path| path.starts_with(target) || target.starts_with(path))
}

fn deepest_existing_directory(target: &Path) -> anyhow::Result<PathBuf> {
    target
        .ancestors()
        .find(|path| path.is_dir())
        .map(Path::to_path_buf)
        .with_context(|| format!("广告目录没有可监视的现存父目录：{}", target.display()))
}

fn bind_deepest_existing(
    watcher: &mut RecommendedWatcher,
    target: &Path,
    current: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let next = deepest_existing_directory(target)?;
    if current == Some(next.as_path()) {
        return Ok(next);
    }
    watcher
        .watch(&next, RecursiveMode::NonRecursive)
        .with_context(|| format!("监视广告目录失败：{}", next.display()))?;
    if let Some(current) = current {
        // 先成功绑定新目录再移除旧 watch；目录已删除时 unwatch 失败无害。
        let _ = watcher.unwatch(current);
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "liteavd-ad-watch-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn wait_for_count(count: &AtomicUsize, expected: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while count.load(Ordering::Acquire) < expected {
            assert!(
                std::time::Instant::now() < deadline,
                "5 秒内未收到广告目录事件"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn run_and_wait(count: &AtomicUsize, action: impl FnOnce()) {
        let before = count.load(Ordering::Acquire);
        action();
        wait_for_count(count, before + 1);
    }

    #[test]
    fn watches_late_directory_create_rename_remove_and_manual_rescan() {
        let root = temp_root("lifecycle");
        let target = root.join("avd/running");
        std::fs::create_dir_all(&root).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let callback_count = count.clone();
        let monitor = AdvertisementMonitor::start_at(target.clone(), move || {
            callback_count.fetch_add(1, Ordering::AcqRel);
        })
        .unwrap();
        wait_for_count(&count, 1); // initial full rescan

        run_and_wait(&count, || std::fs::create_dir_all(&target).unwrap());
        let first = target.join("pid_1.ini");
        let renamed = target.join("pid_2.ini");
        run_and_wait(&count, || {
            std::fs::write(&first, "avd.name=test\n").unwrap()
        });
        run_and_wait(&count, || std::fs::rename(&first, &renamed).unwrap());
        run_and_wait(&count, || std::fs::remove_file(&renamed).unwrap());

        run_and_wait(&count, || monitor.request_rescan());
        drop(monitor);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relevance_filter_ignores_siblings_but_accepts_ancestors() {
        let target = Path::new("/run/user/1000/avd/running");
        let sibling =
            Event::new(notify::EventKind::Any).add_path(PathBuf::from("/run/user/1000/unrelated"));
        let ancestor =
            Event::new(notify::EventKind::Any).add_path(PathBuf::from("/run/user/1000/avd"));
        assert!(!event_is_relevant(&sibling, target));
        assert!(event_is_relevant(&ancestor, target));
    }
}
