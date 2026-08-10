//! 多实例调度与资源 reservation。
//!
//! console port 由独立 RAII 分配器保护。启动任务进入 FIFO；`LaunchPermit` 限制
//! 同时启动数并预留内存/GPU，boot 成功后转换为由 session 持有的
//! `ResourceReservation`。取消、失败或 session drop 都会自动唤醒队首。

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use thiserror::Error;

pub const FIRST_CONSOLE_PORT: u16 = 5554;
pub const LAST_CONSOLE_PORT: u16 = 5586;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum PortReservationError {
    #[error("模拟器 console 端口 5554-5586 已全部占用")]
    Exhausted,
}

#[derive(Debug, Default)]
struct PortAllocatorState {
    reserved: BTreeSet<u16>,
}

/// 可跨线程共享的 console port 分配器。
///
/// 调用者把外部已运行实例的端口作为 `occupied` 传入；分配器同时记录本进程中
/// 尚未出现在广告文件里的启动任务，因此两个并发启动不会选中同一端口。
#[derive(Debug, Clone, Default)]
pub struct PortAllocator {
    state: Arc<Mutex<PortAllocatorState>>,
}

impl PortAllocator {
    pub fn reserve<I>(&self, occupied: I) -> Result<PortReservation, PortReservationError>
    where
        I: IntoIterator<Item = u16>,
    {
        let occupied: BTreeSet<_> = occupied.into_iter().collect();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(port) = (FIRST_CONSOLE_PORT..=LAST_CONSOLE_PORT)
            .step_by(2)
            .find(|port| !occupied.contains(port) && !state.reserved.contains(port))
        else {
            return Err(PortReservationError::Exhausted);
        };

        state.reserved.insert(port);
        Ok(PortReservation {
            port,
            state: Some(self.state.clone()),
        })
    }
}

/// RAII console reservation。启动失败、取消或 session drop 时自动释放。
#[derive(Debug)]
pub struct PortReservation {
    port: u16,
    state: Option<Arc<Mutex<PortAllocatorState>>>,
}

impl PortReservation {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reserved
            .remove(&self.port);
    }
}

impl Drop for PortReservation {
    fn drop(&mut self) {
        self.release_inner();
    }
}

/// 一台 AVD 在调度层声明的资源需求。调度器不会修改这些值。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceDemand {
    pub memory_mb: u64,
    pub gpu_slots: u32,
}

impl ResourceDemand {
    pub const fn new(memory_mb: u64, gpu_slots: u32) -> Self {
        Self {
            memory_mb,
            gpu_slots,
        }
    }
}

/// `None` 表示暂不限制该资源；默认只限制并发启动数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub max_concurrent_starts: usize,
    pub memory_budget_mb: Option<u64>,
    pub gpu_slots: Option<u32>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_starts: 1,
            memory_budget_mb: None,
            gpu_slots: None,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SchedulerError {
    #[error("启动并发上限必须大于 0")]
    InvalidStartLimit,
    #[error("设备 {0} 已在调度器中")]
    Duplicate(String),
    #[error("设备 {key} 请求 {requested}MiB 内存，超过调度预算 {limit}MiB")]
    MemoryDemandExceedsBudget {
        key: String,
        requested: u64,
        limit: u64,
    },
    #[error("设备 {key} 请求 {requested} 个 GPU slot，超过调度预算 {limit}")]
    GpuDemandExceedsBudget {
        key: String,
        requested: u32,
        limit: u32,
    },
    #[error("设备 {0} 的排队任务已取消")]
    Canceled(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueReason {
    Ready,
    EarlierTasks { ahead: usize },
    StartLimit { limit: usize },
    Memory { requested: u64, available: u64 },
    Gpu { requested: u32, available: u32 },
}

impl QueueReason {
    pub fn message(&self) -> String {
        match self {
            Self::Ready => "等待调度线程接管".into(),
            Self::EarlierTasks { ahead } => format!("前方有 {ahead} 个启动任务"),
            Self::StartLimit { limit } => format!("等待启动并发名额（上限 {limit}）"),
            Self::Memory {
                requested,
                available,
            } => format!("等待内存预算（需要 {requested}MiB，可用 {available}MiB）"),
            Self::Gpu {
                requested,
                available,
            } => format!("等待 GPU 预算（需要 {requested}，可用 {available}）"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStatus {
    pub position: usize,
    pub reason: QueueReason,
}

impl QueueStatus {
    pub fn message(&self) -> String {
        format!("队列第 {} 位：{}", self.position, self.reason.message())
    }
}

#[derive(Debug, Clone)]
struct QueueEntry {
    id: u64,
    key: String,
    demand: ResourceDemand,
}

#[derive(Debug, Clone)]
struct ActiveAllocation {
    key: String,
    demand: ResourceDemand,
    starting: bool,
}

#[derive(Debug, Default)]
struct SchedulerState {
    next_id: u64,
    queue: VecDeque<QueueEntry>,
    active: HashMap<u64, ActiveAllocation>,
    external: HashMap<String, ResourceDemand>,
}

#[derive(Debug)]
struct SchedulerShared {
    config: SchedulerConfig,
    state: Mutex<SchedulerState>,
    changed: Condvar,
}

/// 可跨线程共享的 FIFO 启动调度器。
#[derive(Debug, Clone)]
pub struct LaunchScheduler {
    shared: Arc<SchedulerShared>,
}

impl Default for LaunchScheduler {
    fn default() -> Self {
        Self::new(SchedulerConfig::default()).expect("默认 scheduler 配置必须有效")
    }
}

impl LaunchScheduler {
    pub fn new(config: SchedulerConfig) -> Result<Self, SchedulerError> {
        if config.max_concurrent_starts == 0 {
            return Err(SchedulerError::InvalidStartLimit);
        }
        Ok(Self {
            shared: Arc::new(SchedulerShared {
                config,
                state: Mutex::new(SchedulerState::default()),
                changed: Condvar::new(),
            }),
        })
    }

    /// Runtime policy may be replaced only when no queued, managed, or
    /// reconciled external allocation can retain demand computed from it.
    pub fn is_idle(&self) -> bool {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.queue.is_empty() && state.active.is_empty() && state.external.is_empty()
    }

    pub fn enqueue(
        &self,
        key: impl Into<String>,
        demand: ResourceDemand,
    ) -> Result<StartTicket, SchedulerError> {
        let key = key.into();
        if let Some(limit) = self.shared.config.memory_budget_mb
            && demand.memory_mb > limit
        {
            return Err(SchedulerError::MemoryDemandExceedsBudget {
                key,
                requested: demand.memory_mb,
                limit,
            });
        }
        if let Some(limit) = self.shared.config.gpu_slots
            && demand.gpu_slots > limit
        {
            return Err(SchedulerError::GpuDemandExceedsBudget {
                key,
                requested: demand.gpu_slots,
                limit,
            });
        }

        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.queue.iter().any(|entry| entry.key == key)
            || state.active.values().any(|entry| entry.key == key)
            || state.external.contains_key(&key)
        {
            return Err(SchedulerError::Duplicate(key));
        }
        let id = next_request_id(&mut state);
        state.queue.push_back(QueueEntry {
            id,
            key: key.clone(),
            demand,
        });
        self.shared.changed.notify_all();
        Ok(StartTicket {
            id,
            key,
            shared: self.shared.clone(),
            queued: true,
        })
    }

    /// 取消仍在 FIFO 中的任务；已取得 launch permit 时返回 `false`。
    pub fn cancel(&self, key: &str) -> bool {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(index) = state.queue.iter().position(|entry| entry.key == key) else {
            return false;
        };
        state.queue.remove(index);
        self.shared.changed.notify_all();
        true
    }

    /// 用已收养的外部实例重建资源占用；managed allocation 会按 key 自动排除。
    pub fn reconcile_external<I>(&self, entries: I)
    where
        I: IntoIterator<Item = (String, ResourceDemand)>,
    {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let managed: BTreeSet<_> = state
            .active
            .values()
            .map(|allocation| allocation.key.as_str())
            .collect();
        state.external = entries
            .into_iter()
            .filter(|(key, _)| !managed.contains(key.as_str()))
            .collect();
        self.shared.changed.notify_all();
    }

    pub fn remove_external(&self, key: &str) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.external.remove(key).is_some() {
            self.shared.changed.notify_all();
        }
    }
}

/// 排队句柄；drop 自动取消尚未获准的请求。
#[derive(Debug)]
pub struct StartTicket {
    id: u64,
    key: String,
    shared: Arc<SchedulerShared>,
    queued: bool,
}

impl StartTicket {
    pub fn status(&self) -> Result<QueueStatus, SchedulerError> {
        let state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        queue_status(&self.shared, &state, self.id)
            .ok_or_else(|| SchedulerError::Canceled(self.key.clone()))
    }

    /// 非阻塞尝试。`Ok(None)` 表示仍按 FIFO 等待。
    pub fn try_acquire(&mut self) -> Result<Option<LaunchPermit>, SchedulerError> {
        let shared = self.shared.clone();
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(status) = queue_status(&shared, &state, self.id) else {
            self.queued = false;
            return Err(SchedulerError::Canceled(self.key.clone()));
        };
        if status.reason != QueueReason::Ready {
            return Ok(None);
        }
        Ok(Some(admit(self, &mut state)))
    }

    /// 阻塞等待 FIFO 与资源条件满足。调用方应在专用 worker 上执行。
    pub fn wait(self) -> Result<LaunchPermit, SchedulerError> {
        self.wait_with_status(|_| {})
    }

    /// 等待期间在原因变化时回报最新队列位置/阻断资源。
    pub fn wait_with_status(
        mut self,
        mut on_status: impl FnMut(QueueStatus),
    ) -> Result<LaunchPermit, SchedulerError> {
        let shared = self.shared.clone();
        let mut last_status = None;
        loop {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            let Some(status) = queue_status(&shared, &state, self.id) else {
                self.queued = false;
                return Err(SchedulerError::Canceled(self.key.clone()));
            };
            if status.reason == QueueReason::Ready {
                return Ok(admit(&mut self, &mut state));
            }
            if last_status.as_ref() != Some(&status) {
                last_status = Some(status.clone());
                drop(state);
                on_status(status);
                // 回调期间资源可能变化；重新检查后再进入 condvar，避免丢失唤醒。
                continue;
            }
            state = shared
                .changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
            drop(state);
        }
    }
}

impl Drop for StartTicket {
    fn drop(&mut self) {
        if self.queued {
            cancel_id(&self.shared, self.id);
        }
    }
}

/// 启动阶段资源许可。失败/drop 释放全部资源；成功后转为 session reservation。
#[derive(Debug)]
pub struct LaunchPermit {
    id: u64,
    shared: Arc<SchedulerShared>,
    active: bool,
}

impl LaunchPermit {
    pub fn into_reservation(mut self) -> ResourceReservation {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let allocation = state
            .active
            .get_mut(&self.id)
            .expect("launch permit 必须指向 active allocation");
        allocation.starting = false;
        self.active = false;
        self.shared.changed.notify_all();
        ResourceReservation {
            id: self.id,
            shared: self.shared.clone(),
            active: true,
        }
    }
}

impl Drop for LaunchPermit {
    fn drop(&mut self) {
        if self.active {
            release_active(&self.shared, self.id);
        }
    }
}

/// 由 managed session 持有的内存/GPU reservation。
#[derive(Debug)]
pub struct ResourceReservation {
    id: u64,
    shared: Arc<SchedulerShared>,
    active: bool,
}

impl Drop for ResourceReservation {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            release_active(&self.shared, self.id);
        }
    }
}

fn next_request_id(state: &mut SchedulerState) -> u64 {
    loop {
        state.next_id = state.next_id.wrapping_add(1).max(1);
        let id = state.next_id;
        if !state.active.contains_key(&id) && state.queue.iter().all(|entry| entry.id != id) {
            return id;
        }
    }
}

fn allocated(state: &SchedulerState) -> ResourceDemand {
    state
        .active
        .values()
        .map(|entry| entry.demand)
        .chain(state.external.values().copied())
        .fold(ResourceDemand::default(), |mut total, demand| {
            total.memory_mb = total.memory_mb.saturating_add(demand.memory_mb);
            total.gpu_slots = total.gpu_slots.saturating_add(demand.gpu_slots);
            total
        })
}

fn queue_status(shared: &SchedulerShared, state: &SchedulerState, id: u64) -> Option<QueueStatus> {
    let index = state.queue.iter().position(|entry| entry.id == id)?;
    let entry = &state.queue[index];
    let reason = if index > 0 {
        QueueReason::EarlierTasks { ahead: index }
    } else if state.active.values().filter(|entry| entry.starting).count()
        >= shared.config.max_concurrent_starts
    {
        QueueReason::StartLimit {
            limit: shared.config.max_concurrent_starts,
        }
    } else {
        let used = allocated(state);
        if let Some(limit) = shared.config.memory_budget_mb {
            let available = limit.saturating_sub(used.memory_mb);
            if entry.demand.memory_mb > available {
                return Some(QueueStatus {
                    position: 1,
                    reason: QueueReason::Memory {
                        requested: entry.demand.memory_mb,
                        available,
                    },
                });
            }
        }
        if let Some(limit) = shared.config.gpu_slots {
            let available = limit.saturating_sub(used.gpu_slots);
            if entry.demand.gpu_slots > available {
                return Some(QueueStatus {
                    position: 1,
                    reason: QueueReason::Gpu {
                        requested: entry.demand.gpu_slots,
                        available,
                    },
                });
            }
        }
        QueueReason::Ready
    };
    Some(QueueStatus {
        position: index + 1,
        reason,
    })
}

fn admit(ticket: &mut StartTicket, state: &mut SchedulerState) -> LaunchPermit {
    let entry = state
        .queue
        .pop_front()
        .expect("只有 FIFO 队首可以取得 launch permit");
    debug_assert_eq!(entry.id, ticket.id);
    state.active.insert(
        entry.id,
        ActiveAllocation {
            key: entry.key,
            demand: entry.demand,
            starting: true,
        },
    );
    ticket.queued = false;
    ticket.shared.changed.notify_all();
    LaunchPermit {
        id: ticket.id,
        shared: ticket.shared.clone(),
        active: true,
    }
}

fn cancel_id(shared: &SchedulerShared, id: u64) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(index) = state.queue.iter().position(|entry| entry.id == id) {
        state.queue.remove(index);
        shared.changed.notify_all();
    }
}

fn release_active(shared: &SchedulerShared, id: u64) {
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.active.remove(&id).is_some() {
        shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    fn constrained_scheduler() -> LaunchScheduler {
        LaunchScheduler::new(SchedulerConfig {
            max_concurrent_starts: 2,
            memory_budget_mb: Some(4096),
            gpu_slots: Some(1),
        })
        .unwrap()
    }

    #[test]
    fn concurrent_port_reservations_are_distinct() {
        let allocator = PortAllocator::default();
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();

        for _ in 0..2 {
            let allocator = allocator.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                allocator.reserve([]).unwrap()
            }));
        }

        barrier.wait();
        let first = workers.remove(0).join().unwrap();
        let second = workers.remove(0).join().unwrap();
        assert_ne!(first.port(), second.port());
    }

    #[test]
    fn external_port_occupancy_is_respected() {
        let allocator = PortAllocator::default();
        let reservation = allocator.reserve([5554, 5556]).unwrap();
        assert_eq!(reservation.port(), 5558);
    }

    #[test]
    fn port_exhaustion_and_release_are_deterministic() {
        let allocator = PortAllocator::default();
        let mut reservations = Vec::new();
        for expected in (FIRST_CONSOLE_PORT..=LAST_CONSOLE_PORT).step_by(2) {
            let reservation = allocator.reserve([]).unwrap();
            assert_eq!(reservation.port(), expected);
            reservations.push(reservation);
        }

        assert_eq!(
            allocator.reserve([]).unwrap_err(),
            PortReservationError::Exhausted
        );
        reservations.remove(0).release();
        assert_eq!(allocator.reserve([]).unwrap().port(), FIRST_CONSOLE_PORT);
    }

    #[test]
    fn fifo_start_limit_advances_after_boot_completion() {
        let scheduler = constrained_scheduler();
        let mut first = scheduler
            .enqueue("first", ResourceDemand::new(512, 0))
            .unwrap();
        let mut second = scheduler
            .enqueue("second", ResourceDemand::new(512, 0))
            .unwrap();
        let mut third = scheduler
            .enqueue("third", ResourceDemand::new(512, 0))
            .unwrap();

        let first = first.try_acquire().unwrap().unwrap();
        let second = second.try_acquire().unwrap().unwrap();
        assert!(third.try_acquire().unwrap().is_none());
        assert_eq!(
            third.status().unwrap().reason,
            QueueReason::StartLimit { limit: 2 }
        );

        let first_session = first.into_reservation();
        let third = third.try_acquire().unwrap().unwrap();
        drop(second);
        drop(third);
        drop(first_session);
    }

    #[test]
    fn memory_and_gpu_reservations_live_until_session_drop() {
        let scheduler = constrained_scheduler();
        let mut first = scheduler
            .enqueue("first", ResourceDemand::new(3072, 1))
            .unwrap();
        let first = first.try_acquire().unwrap().unwrap().into_reservation();

        let mut memory_waiter = scheduler
            .enqueue("memory", ResourceDemand::new(2048, 0))
            .unwrap();
        assert!(memory_waiter.try_acquire().unwrap().is_none());
        assert_eq!(
            memory_waiter.status().unwrap().reason,
            QueueReason::Memory {
                requested: 2048,
                available: 1024,
            }
        );
        drop(first);
        let memory = memory_waiter.try_acquire().unwrap().unwrap();
        drop(memory);

        let mut gpu = scheduler.enqueue("gpu", ResourceDemand::new(0, 1)).unwrap();
        let gpu_session = gpu.try_acquire().unwrap().unwrap().into_reservation();
        let mut gpu_waiter = scheduler
            .enqueue("gpu-waiter", ResourceDemand::new(0, 1))
            .unwrap();
        assert!(gpu_waiter.try_acquire().unwrap().is_none());
        assert_eq!(
            gpu_waiter.status().unwrap().reason,
            QueueReason::Gpu {
                requested: 1,
                available: 0,
            }
        );
        drop(gpu_session);
        assert!(gpu_waiter.try_acquire().unwrap().is_some());
    }

    #[test]
    fn cancel_and_failure_advance_fifo_without_leaks() {
        let scheduler = LaunchScheduler::default();
        let mut first = scheduler
            .enqueue("first", ResourceDemand::default())
            .unwrap();
        let second = scheduler
            .enqueue("second", ResourceDemand::default())
            .unwrap();
        let mut third = scheduler
            .enqueue("third", ResourceDemand::default())
            .unwrap();
        let first = first.try_acquire().unwrap().unwrap();
        assert!(scheduler.cancel("second"));
        assert_eq!(
            second.status().unwrap_err(),
            SchedulerError::Canceled("second".into())
        );
        assert!(third.try_acquire().unwrap().is_none());
        drop(first);
        assert!(third.try_acquire().unwrap().is_some());
    }

    #[test]
    fn blocking_wait_reports_reason_and_wakes_after_release() {
        let scheduler = LaunchScheduler::default();
        let mut first = scheduler
            .enqueue("first", ResourceDemand::default())
            .unwrap();
        let first = first.try_acquire().unwrap().unwrap();
        let second = scheduler
            .enqueue("second", ResourceDemand::default())
            .unwrap();
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            second
                .wait_with_status(|status| status_tx.send(status).unwrap())
                .unwrap()
        });
        assert_eq!(
            status_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
                .reason,
            QueueReason::StartLimit { limit: 1 }
        );
        drop(first);
        drop(waiter.join().unwrap());
    }

    #[test]
    fn fifo_does_not_bypass_large_head_request() {
        let scheduler = constrained_scheduler();
        let mut running = scheduler
            .enqueue("running", ResourceDemand::new(3072, 0))
            .unwrap();
        let running = running.try_acquire().unwrap().unwrap().into_reservation();
        let large = scheduler
            .enqueue("large", ResourceDemand::new(2048, 0))
            .unwrap();
        let small = scheduler
            .enqueue("small", ResourceDemand::new(128, 0))
            .unwrap();
        assert!(matches!(
            large.status().unwrap().reason,
            QueueReason::Memory { .. }
        ));
        assert_eq!(
            small.status().unwrap().reason,
            QueueReason::EarlierTasks { ahead: 1 }
        );
        drop(running);
    }

    #[test]
    fn external_reconciliation_blocks_then_releases_budget() {
        let scheduler = constrained_scheduler();
        scheduler.reconcile_external([("external".into(), ResourceDemand::new(4096, 0))]);
        let mut queued = scheduler
            .enqueue("queued", ResourceDemand::new(512, 0))
            .unwrap();
        assert!(queued.try_acquire().unwrap().is_none());
        scheduler.reconcile_external([]);
        assert!(queued.try_acquire().unwrap().is_some());
    }

    #[test]
    fn impossible_requests_fail_instead_of_waiting_forever() {
        let scheduler = constrained_scheduler();
        assert!(matches!(
            scheduler.enqueue("memory", ResourceDemand::new(4097, 0)),
            Err(SchedulerError::MemoryDemandExceedsBudget { .. })
        ));
        assert!(matches!(
            scheduler.enqueue("gpu", ResourceDemand::new(0, 2)),
            Err(SchedulerError::GpuDemandExceedsBudget { .. })
        ));
    }
}
