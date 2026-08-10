//! 独立于 GTK 的设备生命周期状态。
//!
//! generation 把异步任务结果绑定到发起它的命令；旧启动/停止任务晚到的回调
//! 不能覆盖较新的用户操作。广告扫描只合并外部事实，不抹掉本地过渡态与错误。

use std::collections::HashMap;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryReason {
    AdvertisementMissing,
    ControlDisconnected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicePhase {
    Stopped,
    Queued(String),
    Starting,
    Booting,
    Running,
    Recovering(RecoveryReason),
    Stopping,
    Error(String),
}

impl DevicePhase {
    pub fn allows_start(&self) -> bool {
        matches!(self, Self::Stopped | Self::Error(_))
    }

    pub fn allows_stop(&self) -> bool {
        matches!(self, Self::Running | Self::Recovering(_))
    }

    fn preserves_when_scan_is_absent(&self) -> bool {
        matches!(
            self,
            Self::Queued(_)
                | Self::Starting
                | Self::Booting
                | Self::Recovering(_)
                | Self::Stopping
                | Self::Error(_)
        )
    }

    fn preserves_when_scan_is_running(&self) -> bool {
        matches!(
            self,
            Self::Queued(_)
                | Self::Starting
                | Self::Booting
                | Self::Recovering(_)
                | Self::Stopping
                | Self::Error(_)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceState {
    pub generation: u64,
    pub phase: DevicePhase,
    pub console_port: Option<u16>,
}

impl DeviceState {
    fn stopped() -> Self {
        Self {
            generation: 0,
            phase: DevicePhase::Stopped,
            console_port: None,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StateTransitionError {
    #[error("设备已有活动中的会话或命令")]
    AlreadyActive,
}

#[derive(Debug, Default)]
pub struct DeviceStateStore {
    states: HashMap<String, DeviceState>,
    next_generation: u64,
}

impl DeviceStateStore {
    pub fn begin_start(&mut self, avd_name: &str) -> Result<u64, StateTransitionError> {
        if self
            .states
            .get(avd_name)
            .is_some_and(|state| !state.phase.allows_start())
        {
            return Err(StateTransitionError::AlreadyActive);
        }

        let generation = self.new_generation();
        self.states.insert(
            avd_name.to_owned(),
            DeviceState {
                generation,
                phase: DevicePhase::Starting,
                console_port: None,
            },
        );
        Ok(generation)
    }

    pub fn begin_stop(&mut self, avd_name: &str, console_port: u16) -> u64 {
        let generation = self.new_generation();
        self.states.insert(
            avd_name.to_owned(),
            DeviceState {
                generation,
                phase: DevicePhase::Stopping,
                console_port: Some(console_port),
            },
        );
        generation
    }

    pub fn attach_port(&mut self, avd_name: &str, generation: u64, port: u16) -> bool {
        let Some(state) = self.states.get_mut(avd_name) else {
            return false;
        };
        if state.generation != generation {
            return false;
        }
        state.console_port = Some(port);
        true
    }

    pub fn update(&mut self, avd_name: &str, generation: u64, phase: DevicePhase) -> bool {
        let Some(state) = self.states.get_mut(avd_name) else {
            return false;
        };
        if state.generation != generation {
            return false;
        }
        state.phase = phase;
        if matches!(state.phase, DevicePhase::Stopped) {
            state.console_port = None;
        }
        true
    }

    pub fn clear_port(&mut self, avd_name: &str, generation: u64) -> bool {
        let Some(state) = self.states.get_mut(avd_name) else {
            return false;
        };
        if state.generation != generation {
            return false;
        }
        state.console_port = None;
        true
    }

    pub fn force_running(&mut self, avd_name: &str, console_port: u16) -> DeviceState {
        let generation = self.new_generation();
        let state = DeviceState {
            generation,
            phase: DevicePhase::Running,
            console_port: Some(console_port),
        };
        self.states.insert(avd_name.to_owned(), state.clone());
        state
    }

    pub fn force_stopped(&mut self, avd_name: &str) -> DeviceState {
        let generation = self.new_generation();
        let state = DeviceState {
            generation,
            phase: DevicePhase::Stopped,
            console_port: None,
        };
        self.states.insert(avd_name.to_owned(), state.clone());
        state
    }

    pub fn force_error(&mut self, avd_name: &str, error: String) -> DeviceState {
        let generation = self.new_generation();
        let state = DeviceState {
            generation,
            phase: DevicePhase::Error(error),
            console_port: None,
        };
        self.states.insert(avd_name.to_owned(), state.clone());
        state
    }

    /// 只取消仍在队列中的 start，并用新 generation 使旧 worker 结果失效。
    pub fn cancel_queued(&mut self, avd_name: &str) -> bool {
        if !self
            .states
            .get(avd_name)
            .is_some_and(|state| matches!(state.phase, DevicePhase::Queued(_)))
        {
            return false;
        }
        self.force_stopped(avd_name);
        true
    }

    /// 把一次广告文件扫描合并到持久状态。
    ///
    /// `observed_port` 为 `Some` 表示看到了存活实例。扫描缺失不能抹掉本地命令的
    /// 过渡态或错误；扫描看到实例也不能把仍在 boot/stop 的命令提前标为完成。
    pub fn reconcile_scan(&mut self, avd_name: &str, observed_port: Option<u16>) -> DeviceState {
        if let Some(existing) = self.states.get(avd_name) {
            let preserve = match observed_port {
                Some(_) => existing.phase.preserves_when_scan_is_running(),
                None => existing.phase.preserves_when_scan_is_absent(),
            };
            if preserve {
                return existing.clone();
            }
        }

        let previous_generation = self
            .states
            .get(avd_name)
            .map(|state| state.generation)
            .unwrap_or(0);
        let next = match observed_port {
            Some(port) => DeviceState {
                generation: previous_generation,
                phase: DevicePhase::Running,
                console_port: Some(port),
            },
            None => DeviceState {
                generation: previous_generation,
                ..DeviceState::stopped()
            },
        };
        self.states.insert(avd_name.to_owned(), next.clone());
        next
    }

    pub fn get(&self, avd_name: &str) -> Option<&DeviceState> {
        self.states.get(avd_name)
    }

    fn new_generation(&mut self) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.next_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_generation_cannot_overwrite_new_command() {
        let mut store = DeviceStateStore::default();
        let first = store.begin_start("pixel").unwrap();
        assert!(store.update("pixel", first, DevicePhase::Error("failed".into())));

        let second = store.begin_start("pixel").unwrap();
        assert_ne!(first, second);
        assert!(!store.update("pixel", first, DevicePhase::Running));
        assert_eq!(store.get("pixel").unwrap().phase, DevicePhase::Starting);
    }

    #[test]
    fn scan_does_not_erase_transitions_or_errors() {
        let mut store = DeviceStateStore::default();
        let generation = store.begin_start("pixel").unwrap();
        store.attach_port("pixel", generation, 5554);

        assert_eq!(
            store.reconcile_scan("pixel", None).phase,
            DevicePhase::Starting
        );
        assert!(store.update("pixel", generation, DevicePhase::Booting));
        assert_eq!(
            store.reconcile_scan("pixel", Some(5554)).phase,
            DevicePhase::Booting
        );
        assert!(store.update("pixel", generation, DevicePhase::Error("boom".into())));
        assert_eq!(
            store.reconcile_scan("pixel", None).phase,
            DevicePhase::Error("boom".into())
        );
    }

    #[test]
    fn active_device_rejects_duplicate_start() {
        let mut store = DeviceStateStore::default();
        store.begin_start("pixel").unwrap();
        assert_eq!(
            store.begin_start("pixel").unwrap_err(),
            StateTransitionError::AlreadyActive
        );
    }

    #[test]
    fn canceling_queue_invalidates_old_generation() {
        let mut store = DeviceStateStore::default();
        let generation = store.begin_start("pixel").unwrap();
        assert!(store.update(
            "pixel",
            generation,
            DevicePhase::Queued("等待启动名额".into())
        ));
        assert!(store.cancel_queued("pixel"));
        let state = store.get("pixel").unwrap();
        assert_eq!(state.phase, DevicePhase::Stopped);
        assert_ne!(state.generation, generation);
        assert!(!store.update("pixel", generation, DevicePhase::Starting));
    }

    #[test]
    fn external_running_and_stopped_are_projected() {
        let mut store = DeviceStateStore::default();
        let running = store.reconcile_scan("external", Some(5560));
        assert_eq!(running.phase, DevicePhase::Running);
        assert_eq!(running.console_port, Some(5560));

        let stopped = store.reconcile_scan("external", None);
        assert_eq!(stopped.phase, DevicePhase::Stopped);
        assert_eq!(stopped.console_port, None);
    }
}
