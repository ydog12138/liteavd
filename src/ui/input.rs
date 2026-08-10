//! GTK viewport 事件 → 有界/合并输入 worker。

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use thiserror::Error;

use crate::core::grpc::{
    GrpcClient, KeyEventType, KeyboardEvent, MouseEvent, TouchEvent, keyboard_key_event,
    keyboard_text_event, touch_event,
};
use crate::core::input::{GuestPoint, TouchTracker, ViewportTransform, navigation_key};
use crate::core::instance::InputRouteGuard;
use crate::core::stream::FrameMeta;
use crate::core::telemetry::{InputToken, LatencyProbe};
use crate::core::workspace::WorkspaceRoute;

const MAX_RELIABLE_QUEUE: usize = 128;
const IDLE_WAIT: Duration = Duration::from_millis(1);

#[derive(Debug, Error)]
pub(crate) enum InputAttachError {
    #[error("创建输入 worker 失败：{0}")]
    Thread(#[source] std::io::Error),
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
enum DispatchError {
    #[error("输入队列已满")]
    QueueFull,
    #[error("输入 worker 已关闭")]
    Closed,
    #[error("输入路由对应的模拟器 session 已失效")]
    StaleRoute,
}

#[derive(Debug, Clone)]
enum InputEvent {
    Key(KeyboardEvent),
    Touch(TouchEvent),
    Mouse(MouseEvent),
}

impl InputEvent {
    async fn execute(&self, client: &GrpcClient) -> Result<(), crate::core::grpc::InputRpcError> {
        match self {
            Self::Key(event) => client.send_key_event(event.clone()).await,
            Self::Touch(event) => client.send_touch(event.clone()).await,
            Self::Mouse(event) => client.send_mouse(*event).await,
        }
    }

    /// 鼠标和触摸 RPC 描述的是完整绝对状态；transport 断开时重放不会累积位移。
    /// 键盘事件可能已经被服务端执行，不能在结果未知时自动重复。
    fn is_retry_safe(&self) -> bool {
        matches!(self, Self::Touch(_) | Self::Mouse(_))
    }
}

#[derive(Debug)]
struct InputJob {
    event: InputEvent,
    token: InputToken,
    route: Option<WorkspaceRoute>,
}

#[derive(Debug)]
struct ReliableJob {
    job: InputJob,
    counted: bool,
}

#[derive(Debug, Default)]
struct DispatchStats {
    reliable_sent: AtomicU64,
    motion_sent: AtomicU64,
    motion_replaced: AtomicU64,
    errors: AtomicU64,
}

#[derive(Debug)]
struct Shared {
    stop: AtomicBool,
    queued_reliable: AtomicUsize,
    motion: Mutex<Option<InputJob>>,
    worker_thread: Mutex<Option<std::thread::Thread>>,
    stats: DispatchStats,
    telemetry: LatencyProbe,
    route: Option<InputRouteGuard>,
}

impl Shared {
    fn new(telemetry: LatencyProbe, route: Option<InputRouteGuard>) -> Self {
        Self {
            stop: AtomicBool::new(false),
            queued_reliable: AtomicUsize::new(0),
            motion: Mutex::new(None),
            worker_thread: Mutex::new(None),
            stats: DispatchStats::default(),
            telemetry,
            route,
        }
    }

    fn route_is_current(&self) -> bool {
        self.route.as_ref().is_none_or(InputRouteGuard::is_current)
    }

    fn job_route_is_current(&self, job: &InputJob) -> bool {
        let bound_route = self.route.as_ref().map(InputRouteGuard::route);
        bound_route == job.route.as_ref() && self.route_is_current()
    }

    fn report_control_disconnected(&self) {
        if let Some(route) = &self.route {
            route.report_control_disconnected();
        }
    }

    fn report_control_connected(&self) {
        if let Some(route) = &self.route {
            route.report_control_connected();
        }
    }
}

#[derive(Debug, Clone)]
struct InputDispatcher {
    reliable: std::sync::mpsc::Sender<ReliableJob>,
    shared: Arc<Shared>,
}

impl InputDispatcher {
    fn reliable(&self, event: InputEvent) -> Result<(), DispatchError> {
        self.validate_route()?;
        let counted = self
            .shared
            .queued_reliable
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                (queued < MAX_RELIABLE_QUEUE).then_some(queued + 1)
            })
            .is_ok();
        if !counted {
            return Err(DispatchError::QueueFull);
        }
        self.send_reliable(ReliableJob {
            job: self.track(event),
            counted,
        })
    }

    /// release/cancel 不能因为普通 key 队列满而丢失。
    fn critical(&self, event: InputEvent) -> Result<(), DispatchError> {
        self.validate_route()?;
        self.send_reliable(ReliableJob {
            job: self.track(event),
            counted: false,
        })
    }

    fn send_reliable(&self, job: ReliableJob) -> Result<(), DispatchError> {
        if !self.shared.route_is_current() {
            if job.counted {
                self.shared.queued_reliable.fetch_sub(1, Ordering::AcqRel);
            }
            self.shared.telemetry.cancel_input(job.job.token);
            return Err(DispatchError::StaleRoute);
        }
        if self.shared.stop.load(Ordering::Acquire) {
            if job.counted {
                self.shared.queued_reliable.fetch_sub(1, Ordering::AcqRel);
            }
            self.shared.telemetry.cancel_input(job.job.token);
            return Err(DispatchError::Closed);
        }
        let counted = job.counted;
        let token = job.job.token;
        if self.reliable.send(job).is_err() {
            if counted {
                self.shared.queued_reliable.fetch_sub(1, Ordering::AcqRel);
            }
            self.shared.telemetry.cancel_input(token);
            return Err(DispatchError::Closed);
        }
        unpark_worker(&self.shared);
        Ok(())
    }

    fn replace_motion(&self, event: InputEvent) -> Result<(), DispatchError> {
        self.validate_route()?;
        if self.shared.stop.load(Ordering::Acquire) {
            return Err(DispatchError::Closed);
        }
        let job = self.track(event);
        let mut motion = self
            .shared
            .motion
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !self.shared.route_is_current() {
            drop(motion);
            self.shared.telemetry.cancel_input(job.token);
            return Err(DispatchError::StaleRoute);
        }
        if self.shared.stop.load(Ordering::Acquire) {
            drop(motion);
            self.shared.telemetry.cancel_input(job.token);
            return Err(DispatchError::Closed);
        }
        let replaced = motion.replace(job);
        drop(motion);
        if let Some(replaced) = replaced {
            self.shared.telemetry.cancel_input(replaced.token);
            self.shared
                .stats
                .motion_replaced
                .fetch_add(1, Ordering::Relaxed);
        }
        unpark_worker(&self.shared);
        Ok(())
    }

    fn clear_motion(&self) {
        let removed = self
            .shared
            .motion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(removed) = removed {
            self.shared.telemetry.cancel_input(removed.token);
        }
    }

    /// 在 release 前把最后一个合并事件提升为可靠 FIFO 事件。
    fn flush_motion_critical(&self) -> Result<(), DispatchError> {
        let motion = self
            .shared
            .motion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(job) = motion {
            if !self.shared.route_is_current() {
                self.shared.telemetry.cancel_input(job.token);
                return Err(DispatchError::StaleRoute);
            }
            self.send_reliable(ReliableJob {
                job,
                counted: false,
            })
        } else {
            Ok(())
        }
    }

    fn track(&self, event: InputEvent) -> InputJob {
        InputJob {
            event,
            token: self.shared.telemetry.begin_input(Instant::now()),
            route: self
                .shared
                .route
                .as_ref()
                .map(|guard| guard.route().clone()),
        }
    }

    fn validate_route(&self) -> Result<(), DispatchError> {
        if self.shared.route_is_current() {
            Ok(())
        } else {
            Err(DispatchError::StaleRoute)
        }
    }
}

#[derive(Debug)]
struct InputWorker {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl InputWorker {
    fn start(
        client: GrpcClient,
        telemetry: LatencyProbe,
        route: Option<InputRouteGuard>,
    ) -> Result<(Self, InputDispatcher), InputAttachError> {
        let shared = Arc::new(Shared::new(telemetry, route));
        let (reliable, receiver) = std::sync::mpsc::channel();
        let dispatcher = InputDispatcher {
            reliable,
            shared: shared.clone(),
        };
        let worker_shared = shared.clone();
        let join = std::thread::Builder::new()
            .name("liteavd-input".into())
            .spawn(move || input_loop(client, receiver, worker_shared))
            .map_err(InputAttachError::Thread)?;
        *shared
            .worker_thread
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(join.thread().clone());
        Ok((
            Self {
                shared,
                join: Some(join),
            },
            dispatcher,
        ))
    }
}

impl Drop for InputWorker {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        unpark_worker(&self.shared);
        if let Some(join) = self.join.take() {
            // GTK detach 不能等待最长 2 秒的在途 RPC；reaper 在线程外完成 join。
            let _ = std::thread::Builder::new()
                .name("liteavd-input-reaper".into())
                .spawn(move || {
                    let _ = join.join();
                });
        }
    }
}

fn input_loop(
    client_config: GrpcClient,
    reliable: std::sync::mpsc::Receiver<ReliableJob>,
    shared: Arc<Shared>,
) {
    if !shared.route_is_current() {
        close_failed_worker(&reliable, &shared);
        return;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            crate::core::settings::emit(
                crate::core::settings::AppLogLevel::Error,
                format_args!("创建 viewport 输入 runtime 失败：{error}"),
            );
            close_failed_worker(&reliable, &shared);
            return;
        }
    };
    let Some(mut client) = reconnect_until_available(&runtime, &client_config, &shared) else {
        close_failed_worker(&reliable, &shared);
        return;
    };

    'worker: loop {
        let job = match reliable.try_recv() {
            Ok(reliable) => {
                if reliable.counted {
                    shared.queued_reliable.fetch_sub(1, Ordering::AcqRel);
                }
                shared.stats.reliable_sent.fetch_add(1, Ordering::Relaxed);
                Some(reliable.job)
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                let motion = shared
                    .motion
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if motion.is_some() {
                    shared.stats.motion_sent.fetch_add(1, Ordering::Relaxed);
                    motion
                } else if shared.stop.load(Ordering::Acquire) {
                    break;
                } else {
                    std::thread::park_timeout(IDLE_WAIT);
                    None
                }
            }
        };

        if let Some(job) = job {
            if !shared.job_route_is_current(&job) {
                shared.telemetry.cancel_input(job.token);
                close_failed_worker(&reliable, &shared);
                break;
            }
            shared.telemetry.mark_rpc_started(job.token, Instant::now());
            let token = job.token;
            let mut result = runtime.block_on(job.event.execute(&client));
            if result.is_ok() {
                shared.report_control_connected();
            }
            if result.is_err() {
                if !shared.route_is_current() {
                    shared.telemetry.cancel_input(token);
                    close_failed_worker(&reliable, &shared);
                    break 'worker;
                }
                match runtime.block_on(client.reconnect()) {
                    Ok(reconnected) => {
                        client = reconnected;
                        shared.report_control_connected();
                        if job.event.is_retry_safe() {
                            if !shared.route_is_current() {
                                shared.telemetry.cancel_input(token);
                                close_failed_worker(&reliable, &shared);
                                break 'worker;
                            }
                            result = runtime.block_on(job.event.execute(&client));
                        }
                    }
                    Err(error) => {
                        shared.report_control_disconnected();
                        crate::core::settings::emit(
                            crate::core::settings::AppLogLevel::Warn,
                            format_args!("viewport 输入 gRPC 恢复连接失败：{error:#}"),
                        );
                    }
                }
            }
            shared
                .telemetry
                .mark_rpc_completed(token, Instant::now(), result.is_ok());
            if let Err(error) = result {
                let count = shared.stats.errors.fetch_add(1, Ordering::Relaxed) + 1;
                if count == 1 || count.is_multiple_of(64) {
                    crate::core::settings::emit(
                        crate::core::settings::AppLogLevel::Warn,
                        format_args!("viewport 输入 gRPC 失败（第 {count} 次）：{error}"),
                    );
                }
            }
        }
    }
}

fn reconnect_until_available(
    runtime: &tokio::runtime::Runtime,
    client_config: &GrpcClient,
    shared: &Shared,
) -> Option<GrpcClient> {
    let mut delay = Duration::from_millis(100);
    let mut failures = 0_u64;
    loop {
        if shared.stop.load(Ordering::Acquire) || !shared.route_is_current() {
            return None;
        }
        match runtime.block_on(client_config.reconnect()) {
            Ok(client) => {
                shared.report_control_connected();
                return Some(client);
            }
            Err(error) => {
                shared.report_control_disconnected();
                failures += 1;
                if failures == 1 || failures.is_multiple_of(32) {
                    crate::core::settings::emit(
                        crate::core::settings::AppLogLevel::Warn,
                        format_args!(
                            "viewport 输入 gRPC 重连失败（第 {failures} 次），将继续恢复：{error:#}"
                        ),
                    );
                }
                std::thread::park_timeout(delay);
                delay = (delay * 2).min(Duration::from_secs(2));
            }
        }
    }
}

fn close_failed_worker(reliable: &std::sync::mpsc::Receiver<ReliableJob>, shared: &Shared) {
    shared.stop.store(true, Ordering::Release);
    while let Ok(job) = reliable.try_recv() {
        if job.counted {
            shared.queued_reliable.fetch_sub(1, Ordering::AcqRel);
        }
        shared.telemetry.cancel_input(job.job.token);
    }
    let motion = shared
        .motion
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some(motion) = motion {
        shared.telemetry.cancel_input(motion.token);
    }
}

fn unpark_worker(shared: &Shared) {
    if let Some(worker) = shared
        .worker_thread
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
    {
        worker.unpark();
    }
}

/// 控件拥有该值；drop 先发送 release/key-up，再让 worker drain 并退出。
#[derive(Debug)]
pub(crate) struct InputBinding {
    dispatcher: InputDispatcher,
    touch: Rc<RefCell<TouchTracker>>,
    pressed_keys: Rc<RefCell<HashSet<String>>>,
    right_pressed: Rc<Cell<bool>>,
    mouse_point: Rc<Cell<Option<GuestPoint>>>,
    _worker: InputWorker,
}

impl Drop for InputBinding {
    fn drop(&mut self) {
        self.dispatcher.clear_motion();
        if let Some(sample) = self.touch.borrow_mut().cancel() {
            let _ = self
                .dispatcher
                .critical(InputEvent::Touch(touch_event(sample)));
        }
        for key in self.pressed_keys.borrow_mut().drain() {
            if let Ok(event) = keyboard_key_event(&key, KeyEventType::Keyup) {
                let _ = self.dispatcher.critical(InputEvent::Key(event));
            }
        }
        release_mouse(&self.dispatcher, &self.right_pressed, &self.mouse_point);
    }
}

pub(crate) fn attach(
    picture: &gtk4::Picture,
    client: GrpcClient,
    frame_meta: Rc<RefCell<Option<FrameMeta>>>,
    telemetry: LatencyProbe,
    route: Option<InputRouteGuard>,
) -> Result<InputBinding, InputAttachError> {
    let (worker, dispatcher) = InputWorker::start(client, telemetry, route.clone())?;
    let touch = Rc::new(RefCell::new(TouchTracker::default()));
    let pressed_keys = Rc::new(RefCell::new(HashSet::new()));
    let right_pressed = Rc::new(Cell::new(false));
    let mouse_point = Rc::new(Cell::new(None));

    picture.set_focusable(true);
    picture.set_focus_on_click(true);

    if let Some(route) = route {
        let focus_route = gtk4::EventControllerFocus::new();
        focus_route.connect_enter(move |_| {
            route.focus();
        });
        picture.add_controller(focus_route);
    }

    attach_touch(picture, &dispatcher, &touch, &frame_meta);
    attach_mouse(
        picture,
        &dispatcher,
        &touch,
        &frame_meta,
        &right_pressed,
        &mouse_point,
    );
    attach_keyboard(
        picture,
        &dispatcher,
        &touch,
        &pressed_keys,
        &right_pressed,
        &mouse_point,
    );

    Ok(InputBinding {
        dispatcher,
        touch,
        pressed_keys,
        right_pressed,
        mouse_point,
        _worker: worker,
    })
}

fn transform(
    picture: &gtk4::Picture,
    meta: &RefCell<Option<FrameMeta>>,
) -> Option<ViewportTransform> {
    let meta = meta.borrow().as_ref().copied()?;
    ViewportTransform::new(
        f64::from(picture.width()),
        f64::from(picture.height()),
        meta.width,
        meta.height,
    )
}

fn attach_touch(
    picture: &gtk4::Picture,
    dispatcher: &InputDispatcher,
    tracker: &Rc<RefCell<TouchTracker>>,
    frame_meta: &Rc<RefCell<Option<FrameMeta>>>,
) {
    let gesture = gtk4::GestureDrag::new();
    gesture.set_button(1);
    let origin = Rc::new(Cell::new((0.0, 0.0)));

    let picture_weak = picture.downgrade();
    let dispatcher_down = dispatcher.clone();
    let tracker_down = tracker.clone();
    let meta_down = frame_meta.clone();
    let origin_down = origin.clone();
    gesture.connect_drag_begin(move |_, x, y| {
        let Some(picture) = picture_weak.upgrade() else {
            return;
        };
        picture.grab_focus();
        origin_down.set((x, y));
        let Some(transform) = transform(&picture, &meta_down) else {
            return;
        };
        if let Some(sample) = tracker_down.borrow_mut().press(&transform, x, y)
            && dispatcher_down
                .reliable(InputEvent::Touch(touch_event(sample)))
                .is_err()
        {
            tracker_down.borrow_mut().cancel();
        }
    });

    let picture_weak = picture.downgrade();
    let dispatcher_move = dispatcher.clone();
    let tracker_move = tracker.clone();
    let meta_move = frame_meta.clone();
    let origin_move = origin.clone();
    gesture.connect_drag_update(move |_, offset_x, offset_y| {
        let Some(picture) = picture_weak.upgrade() else {
            return;
        };
        let Some(transform) = transform(&picture, &meta_move) else {
            return;
        };
        let (start_x, start_y) = origin_move.get();
        if let Some(sample) =
            tracker_move
                .borrow_mut()
                .move_to(&transform, start_x + offset_x, start_y + offset_y)
        {
            let _ = dispatcher_move.replace_motion(InputEvent::Touch(touch_event(sample)));
        }
    });

    let picture_weak = picture.downgrade();
    let dispatcher_up = dispatcher.clone();
    let tracker_up = tracker.clone();
    let meta_up = frame_meta.clone();
    let origin_up = origin;
    gesture.connect_drag_end(move |_, offset_x, offset_y| {
        let Some(picture) = picture_weak.upgrade() else {
            return;
        };
        let (start_x, start_y) = origin_up.get();
        let sample = transform(&picture, &meta_up)
            .and_then(|transform| {
                tracker_up
                    .borrow_mut()
                    .release(&transform, start_x + offset_x, start_y + offset_y)
            })
            .or_else(|| tracker_up.borrow_mut().cancel());
        let _ = dispatcher_up.flush_motion_critical();
        if let Some(sample) = sample {
            let _ = dispatcher_up.critical(InputEvent::Touch(touch_event(sample)));
        }
    });

    let dispatcher_cancel = dispatcher.clone();
    let tracker_cancel = tracker.clone();
    gesture.connect_cancel(move |_, _| {
        dispatcher_cancel.clear_motion();
        if let Some(sample) = tracker_cancel.borrow_mut().cancel() {
            let _ = dispatcher_cancel.critical(InputEvent::Touch(touch_event(sample)));
        }
    });
    picture.add_controller(gesture);
}

fn attach_mouse(
    picture: &gtk4::Picture,
    dispatcher: &InputDispatcher,
    tracker: &Rc<RefCell<TouchTracker>>,
    frame_meta: &Rc<RefCell<Option<FrameMeta>>>,
    right_pressed: &Rc<Cell<bool>>,
    mouse_point: &Rc<Cell<Option<GuestPoint>>>,
) {
    let motion = gtk4::EventControllerMotion::new();
    let picture_weak = picture.downgrade();
    let dispatcher_motion = dispatcher.clone();
    let tracker_motion = tracker.clone();
    let meta_motion = frame_meta.clone();
    let right_motion = right_pressed.clone();
    let point_motion = mouse_point.clone();
    motion.connect_motion(move |_, x, y| {
        if tracker_motion.borrow().is_active() {
            return;
        }
        let Some(picture) = picture_weak.upgrade() else {
            return;
        };
        let Some(point) = transform(&picture, &meta_motion).and_then(|value| value.map(x, y))
        else {
            return;
        };
        point_motion.set(Some(point));
        let _ = dispatcher_motion.replace_motion(InputEvent::Mouse(MouseEvent {
            x: point.x,
            y: point.y,
            buttons: if right_motion.get() { 2 } else { 0 },
            display: 0,
        }));
    });
    picture.add_controller(motion);

    let right = gtk4::GestureClick::new();
    right.set_button(3);
    let picture_weak = picture.downgrade();
    let dispatcher_down = dispatcher.clone();
    let meta_down = frame_meta.clone();
    let right_down = right_pressed.clone();
    let point_down = mouse_point.clone();
    right.connect_pressed(move |_, _, x, y| {
        let Some(picture) = picture_weak.upgrade() else {
            return;
        };
        picture.grab_focus();
        let Some(point) = transform(&picture, &meta_down).and_then(|value| value.map(x, y)) else {
            return;
        };
        point_down.set(Some(point));
        right_down.set(true);
        dispatcher_down.clear_motion();
        let _ = dispatcher_down.reliable(InputEvent::Mouse(MouseEvent {
            x: point.x,
            y: point.y,
            buttons: 2,
            display: 0,
        }));
    });

    let picture_weak = picture.downgrade();
    let dispatcher_up = dispatcher.clone();
    let meta_up = frame_meta.clone();
    let right_up = right_pressed.clone();
    let point_up = mouse_point.clone();
    right.connect_released(move |_, _, x, y| {
        let Some(picture) = picture_weak.upgrade() else {
            return;
        };
        right_up.set(false);
        let _ = dispatcher_up.flush_motion_critical();
        if let Some(point) = transform(&picture, &meta_up).and_then(|value| value.map_clamped(x, y))
        {
            point_up.set(Some(point));
            let _ = dispatcher_up.critical(InputEvent::Mouse(MouseEvent {
                x: point.x,
                y: point.y,
                buttons: 0,
                display: 0,
            }));
        }
    });
    picture.add_controller(right);
}

fn attach_keyboard(
    picture: &gtk4::Picture,
    dispatcher: &InputDispatcher,
    touch: &Rc<RefCell<TouchTracker>>,
    pressed_keys: &Rc<RefCell<HashSet<String>>>,
    right_pressed: &Rc<Cell<bool>>,
    mouse_point: &Rc<Cell<Option<GuestPoint>>>,
) {
    let input_method = gtk4::IMMulticontext::new();
    input_method.set_client_widget(Some(picture));
    let dispatcher_text = dispatcher.clone();
    input_method.connect_commit(move |_, text| {
        if !text.is_empty()
            && let Ok(event) = keyboard_text_event(text)
        {
            let _ = dispatcher_text.reliable(InputEvent::Key(event));
        }
    });

    let keys = gtk4::EventControllerKey::new();
    keys.set_im_context(Some(&input_method));
    let dispatcher_down = dispatcher.clone();
    let pressed_down = pressed_keys.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        let Some(name) = key.name() else {
            return glib::Propagation::Proceed;
        };
        let Some(mapped) = navigation_key(&name) else {
            return glib::Propagation::Proceed;
        };
        if pressed_down.borrow_mut().insert(mapped.to_owned())
            && let Ok(event) = keyboard_key_event(mapped, KeyEventType::Keydown)
        {
            let _ = dispatcher_down.reliable(InputEvent::Key(event));
        }
        glib::Propagation::Stop
    });

    let dispatcher_up = dispatcher.clone();
    let pressed_up = pressed_keys.clone();
    keys.connect_key_released(move |_, key, _, _| {
        let Some(name) = key.name() else {
            return;
        };
        let Some(mapped) = navigation_key(&name) else {
            return;
        };
        if pressed_up.borrow_mut().remove(mapped)
            && let Ok(event) = keyboard_key_event(mapped, KeyEventType::Keyup)
        {
            let _ = dispatcher_up.critical(InputEvent::Key(event));
        }
    });
    picture.add_controller(keys);

    let focus = gtk4::EventControllerFocus::new();
    let input_in = input_method.clone();
    focus.connect_enter(move |_| input_in.focus_in());
    let input_out = input_method;
    let dispatcher_leave = dispatcher.clone();
    let touch_leave = touch.clone();
    let pressed_leave = pressed_keys.clone();
    let right_leave = right_pressed.clone();
    let point_leave = mouse_point.clone();
    focus.connect_leave(move |_| {
        input_out.focus_out();
        input_out.reset();
        dispatcher_leave.clear_motion();
        if let Some(sample) = touch_leave.borrow_mut().cancel() {
            let _ = dispatcher_leave.critical(InputEvent::Touch(touch_event(sample)));
        }
        for key in pressed_leave.borrow_mut().drain() {
            if let Ok(event) = keyboard_key_event(&key, KeyEventType::Keyup) {
                let _ = dispatcher_leave.critical(InputEvent::Key(event));
            }
        }
        release_mouse(&dispatcher_leave, &right_leave, &point_leave);
    });
    picture.add_controller(focus);
}

fn release_mouse(
    dispatcher: &InputDispatcher,
    right_pressed: &Cell<bool>,
    mouse_point: &Cell<Option<GuestPoint>>,
) {
    if !right_pressed.replace(false) {
        return;
    }
    dispatcher.clear_motion();
    if let Some(point) = mouse_point.get() {
        let _ = dispatcher.critical(InputEvent::Mouse(MouseEvent {
            x: point.x,
            y: point.y,
            buttons: 0,
            display: 0,
        }));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::core::emulator::RunningInstance;
    use crate::core::instance::DeviceRuntime;

    fn dispatcher_without_worker() -> (InputDispatcher, std::sync::mpsc::Receiver<ReliableJob>) {
        let shared = Arc::new(Shared::new(LatencyProbe::default(), None));
        let (reliable, receiver) = std::sync::mpsc::channel();
        (InputDispatcher { reliable, shared }, receiver)
    }

    fn running(avd_name: &str, pid: u32) -> RunningInstance {
        RunningInstance {
            pid,
            ini_path: PathBuf::from(format!("/tmp/{avd_name}-{pid}.ini")),
            avd_name: avd_name.to_owned(),
            console_port: 5554,
            adb_port: 5555,
            grpc_port: 8554,
            grpc_allowlist: None,
            grpc_jwks: None,
            grpc_jwk_active: None,
        }
    }

    #[test]
    fn stale_session_route_rejects_new_input_without_leaking_telemetry() {
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![running("pixel", 1001)]);
        let route = runtime.input_route("pixel").unwrap();
        let shared = Arc::new(Shared::new(LatencyProbe::default(), Some(route)));
        let (reliable, receiver) = std::sync::mpsc::channel();
        let dispatcher = InputDispatcher { reliable, shared };

        runtime.reconcile_running(vec![running("pixel", 2002)]);

        assert_eq!(
            dispatcher.reliable(InputEvent::Key(KeyboardEvent::default())),
            Err(DispatchError::StaleRoute)
        );
        assert!(receiver.try_recv().is_err());
        assert_eq!(dispatcher.shared.queued_reliable.load(Ordering::Acquire), 0);
        assert_eq!(dispatcher.shared.telemetry.report().pending_inputs, 0);
    }

    #[test]
    fn queued_event_keeps_exact_route_and_expires_before_rpc() {
        let runtime = Arc::new(DeviceRuntime::default());
        runtime.reconcile_running(vec![running("pixel", 1001)]);
        let route = runtime.input_route("pixel").unwrap();
        let expected = route.route().clone();
        let shared = Arc::new(Shared::new(LatencyProbe::default(), Some(route)));
        let (reliable, receiver) = std::sync::mpsc::channel();
        let dispatcher = InputDispatcher { reliable, shared };
        dispatcher
            .reliable(InputEvent::Key(KeyboardEvent::default()))
            .unwrap();
        let queued = receiver.recv().unwrap();
        assert_eq!(queued.job.route.as_ref(), Some(&expected));

        runtime.reconcile_running(vec![running("pixel", 2002)]);

        assert!(!dispatcher.shared.job_route_is_current(&queued.job));
        dispatcher.shared.telemetry.cancel_input(queued.job.token);
        assert_eq!(dispatcher.shared.telemetry.report().pending_inputs, 0);
    }

    #[test]
    fn motion_is_capacity_one_and_release_is_reliable() {
        let (dispatcher, receiver) = dispatcher_without_worker();
        dispatcher
            .replace_motion(InputEvent::Mouse(MouseEvent {
                x: 1,
                ..Default::default()
            }))
            .unwrap();
        dispatcher
            .replace_motion(InputEvent::Mouse(MouseEvent {
                x: 2,
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(
            dispatcher
                .shared
                .stats
                .motion_replaced
                .load(Ordering::Relaxed),
            1
        );
        dispatcher.clear_motion();
        dispatcher
            .critical(InputEvent::Touch(TouchEvent::default()))
            .unwrap();
        assert!(!receiver.recv().unwrap().counted);
    }

    #[test]
    fn final_motion_is_promoted_before_release() {
        let (dispatcher, receiver) = dispatcher_without_worker();
        dispatcher
            .replace_motion(InputEvent::Mouse(MouseEvent {
                x: 42,
                ..Default::default()
            }))
            .unwrap();
        dispatcher.flush_motion_critical().unwrap();
        dispatcher
            .critical(InputEvent::Touch(TouchEvent::default()))
            .unwrap();

        let motion = receiver.recv().unwrap();
        assert!(!motion.counted);
        let InputEvent::Mouse(motion) = motion.job.event else {
            panic!("提升后的事件类型错误");
        };
        assert_eq!(motion.x, 42);
        assert!(matches!(
            receiver.recv().unwrap().job.event,
            InputEvent::Touch(_)
        ));
    }

    #[test]
    fn ordinary_reliable_queue_has_a_hard_limit() {
        let (dispatcher, _receiver) = dispatcher_without_worker();
        for _ in 0..MAX_RELIABLE_QUEUE {
            dispatcher
                .reliable(InputEvent::Key(KeyboardEvent::default()))
                .unwrap();
        }
        assert_eq!(
            dispatcher.reliable(InputEvent::Key(KeyboardEvent::default())),
            Err(DispatchError::QueueFull)
        );
        assert!(
            dispatcher
                .critical(InputEvent::Touch(TouchEvent::default()))
                .is_ok()
        );
    }

    #[test]
    fn right_button_release_is_critical_and_send_failure_restores_count() {
        let (dispatcher, receiver) = dispatcher_without_worker();
        let right_pressed = Cell::new(true);
        let point = Cell::new(Some(GuestPoint { x: 8, y: 9 }));
        release_mouse(&dispatcher, &right_pressed, &point);
        let release = receiver.recv().unwrap();
        assert!(!release.counted);
        let InputEvent::Mouse(release) = release.job.event else {
            panic!("右键 release 类型错误");
        };
        assert_eq!((release.x, release.y, release.buttons), (8, 9, 0));

        drop(receiver);
        assert_eq!(
            dispatcher.reliable(InputEvent::Key(KeyboardEvent::default())),
            Err(DispatchError::Closed)
        );
        assert_eq!(dispatcher.shared.queued_reliable.load(Ordering::Acquire), 0);
    }

    #[test]
    fn only_absolute_pointer_state_is_safe_to_retry() {
        assert!(InputEvent::Mouse(MouseEvent::default()).is_retry_safe());
        assert!(InputEvent::Touch(TouchEvent::default()).is_retry_safe());
        assert!(!InputEvent::Key(KeyboardEvent::default()).is_retry_safe());
    }

    #[test]
    fn failed_worker_start_closes_and_cancels_queued_jobs() {
        let (dispatcher, receiver) = dispatcher_without_worker();
        dispatcher
            .reliable(InputEvent::Key(KeyboardEvent::default()))
            .unwrap();
        dispatcher
            .replace_motion(InputEvent::Mouse(MouseEvent::default()))
            .unwrap();

        close_failed_worker(&receiver, &dispatcher.shared);

        assert!(dispatcher.shared.stop.load(Ordering::Acquire));
        assert_eq!(dispatcher.shared.queued_reliable.load(Ordering::Acquire), 0);
        let report = dispatcher.shared.telemetry.report();
        assert_eq!(report.pending_inputs, 0);
        assert_eq!(report.canceled_inputs, 2);
        assert_eq!(
            dispatcher.reliable(InputEvent::Key(KeyboardEvent::default())),
            Err(DispatchError::Closed)
        );
    }
}
