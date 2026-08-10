//! 多设备工作区的 GTK 无关焦点/选择状态。
//!
//! 路由身份包含 session id 与 generation；同名 AVD 重启后旧焦点、选择和输入目标
//! 都会失效，不能自动转移到新进程。

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceRoute {
    pub avd_name: String,
    pub session_id: u64,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationScope {
    Focused,
    Selected,
    AllRunning,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    #[error("工作区中不存在 session {0:?}")]
    UnknownRoute(WorkspaceRoute),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub routes: Vec<WorkspaceRoute>,
    pub focused: Option<WorkspaceRoute>,
    pub selected: Vec<WorkspaceRoute>,
}

/// 可跨应用进程保存的工作区意图。session id/generation 是进程内临时身份，
/// 因此只保存 AVD 名称，并在启动时显式绑定到本次扫描得到的新 route。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceIntent {
    pub focused_avd: Option<String>,
    pub selected_avds: Vec<String>,
}

#[derive(Debug, Default)]
pub struct WorkspaceState {
    routes: BTreeMap<String, WorkspaceRoute>,
    focused: Option<WorkspaceRoute>,
    selected: BTreeSet<WorkspaceRoute>,
    pending_restore: Option<WorkspaceIntent>,
}

impl WorkspaceState {
    pub fn reconcile<I>(&mut self, routes: I)
    where
        I: IntoIterator<Item = WorkspaceRoute>,
    {
        self.routes = routes
            .into_iter()
            .map(|route| (route.avd_name.clone(), route))
            .collect();
        if self
            .focused
            .as_ref()
            .is_some_and(|route| !self.contains(route))
        {
            self.focused = None;
        }
        self.selected.retain(|route| {
            self.routes
                .get(&route.avd_name)
                .is_some_and(|current| current == route)
        });
        self.apply_pending_restore();
    }

    pub fn focus(&mut self, route: &WorkspaceRoute) -> Result<(), WorkspaceError> {
        if !self.contains(route) {
            return Err(WorkspaceError::UnknownRoute(route.clone()));
        }
        self.pending_restore = None;
        self.focused = Some(route.clone());
        Ok(())
    }

    pub fn toggle_selected(&mut self, route: &WorkspaceRoute) -> Result<bool, WorkspaceError> {
        if !self.contains(route) {
            return Err(WorkspaceError::UnknownRoute(route.clone()));
        }
        self.pending_restore = None;
        if self.selected.remove(route) {
            Ok(false)
        } else {
            self.selected.insert(route.clone());
            Ok(true)
        }
    }

    pub fn clear_selection(&mut self) {
        self.pending_restore = None;
        self.selected.clear();
    }

    pub fn restore_intent(&mut self, intent: &WorkspaceIntent) {
        self.focused = None;
        self.selected.clear();
        self.pending_restore = Some(intent.clone());
        self.apply_pending_restore();
    }

    pub fn intent(&self) -> WorkspaceIntent {
        let pending = self.pending_restore.as_ref();
        let mut selected_avds: BTreeSet<_> = self
            .selected
            .iter()
            .map(|route| route.avd_name.clone())
            .collect();
        if let Some(pending) = pending {
            selected_avds.extend(pending.selected_avds.iter().cloned());
        }
        WorkspaceIntent {
            focused_avd: self
                .focused
                .as_ref()
                .map(|route| route.avd_name.clone())
                .or_else(|| pending.and_then(|intent| intent.focused_avd.clone())),
            selected_avds: selected_avds.into_iter().collect(),
        }
    }

    fn apply_pending_restore(&mut self) {
        let Some(mut pending) = self.pending_restore.take() else {
            return;
        };
        if let Some(name) = pending.focused_avd.as_deref()
            && let Some(route) = self.routes.get(name)
        {
            self.focused = Some(route.clone());
            pending.focused_avd = None;
        }
        pending.selected_avds.retain(|name| {
            if let Some(route) = self.routes.get(name) {
                self.selected.insert(route.clone());
                false
            } else {
                true
            }
        });
        if pending.focused_avd.is_some() || !pending.selected_avds.is_empty() {
            self.pending_restore = Some(pending);
        }
    }

    pub fn contains(&self, route: &WorkspaceRoute) -> bool {
        self.routes
            .get(&route.avd_name)
            .is_some_and(|current| current == route)
    }

    pub fn targets(&self, scope: OperationScope) -> Vec<WorkspaceRoute> {
        match scope {
            OperationScope::Focused => self.focused.iter().cloned().collect(),
            OperationScope::Selected => self.selected.iter().cloned().collect(),
            OperationScope::AllRunning => self.routes.values().cloned().collect(),
        }
    }

    pub fn snapshot(&self) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            routes: self.routes.values().cloned().collect(),
            focused: self.focused.clone(),
            selected: self.selected.iter().cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(name: &str, session_id: u64, generation: u64) -> WorkspaceRoute {
        WorkspaceRoute {
            avd_name: name.into(),
            session_id,
            generation,
        }
    }

    #[test]
    fn stale_session_identity_loses_focus_and_selection() {
        let old = route("pixel", 1, 7);
        let replacement = route("pixel", 2, 8);
        let mut state = WorkspaceState::default();
        state.reconcile([old.clone()]);
        state.focus(&old).unwrap();
        state.toggle_selected(&old).unwrap();

        state.reconcile([replacement.clone()]);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.routes, vec![replacement]);
        assert!(snapshot.focused.is_none());
        assert!(snapshot.selected.is_empty());
        assert_eq!(
            state.focus(&old).unwrap_err(),
            WorkspaceError::UnknownRoute(old)
        );
    }

    #[test]
    fn operation_scopes_are_explicit_and_deterministic() {
        let first = route("a", 1, 1);
        let second = route("b", 2, 1);
        let third = route("c", 3, 1);
        let mut state = WorkspaceState::default();
        state.reconcile([third.clone(), first.clone(), second.clone()]);
        state.focus(&second).unwrap();
        state.toggle_selected(&third).unwrap();
        state.toggle_selected(&first).unwrap();

        assert_eq!(state.targets(OperationScope::Focused), vec![second]);
        assert_eq!(
            state.targets(OperationScope::Selected),
            vec![first.clone(), third.clone()]
        );
        assert_eq!(
            state.targets(OperationScope::AllRunning),
            vec![first, route("b", 2, 1), third]
        );
    }

    #[test]
    fn disappearing_devices_are_removed_without_changing_other_focus() {
        let first = route("a", 1, 1);
        let second = route("b", 2, 1);
        let mut state = WorkspaceState::default();
        state.reconcile([first.clone(), second.clone()]);
        state.focus(&first).unwrap();
        state.reconcile([first.clone()]);
        assert_eq!(state.snapshot().focused, Some(first));
        assert!(!state.contains(&second));
    }

    #[test]
    fn persisted_names_rebind_only_during_explicit_restore() {
        let old = route("pixel", 1, 4);
        let tablet = route("tablet", 2, 5);
        let mut state = WorkspaceState::default();
        state.reconcile([old.clone(), tablet.clone()]);
        state.focus(&old).unwrap();
        state.toggle_selected(&old).unwrap();
        state.toggle_selected(&tablet).unwrap();
        let intent = state.intent();

        let replacement = route("pixel", 8, 9);
        state.reconcile([replacement.clone(), tablet.clone()]);
        assert!(state.snapshot().focused.is_none());
        assert_eq!(state.snapshot().selected, vec![tablet.clone()]);

        state.restore_intent(&intent);
        assert_eq!(state.snapshot().focused, Some(replacement.clone()));
        assert_eq!(state.snapshot().selected, vec![replacement, tablet]);
    }

    #[test]
    fn unresolved_restore_survives_scans_but_user_action_cancels_it() {
        let phone = route("phone", 1, 1);
        let tablet = route("tablet", 2, 1);
        let mut state = WorkspaceState::default();
        let intent = WorkspaceIntent {
            focused_avd: Some("phone".into()),
            selected_avds: vec!["phone".into(), "tablet".into()],
        };
        state.restore_intent(&intent);
        assert_eq!(state.intent(), intent);

        state.reconcile([tablet.clone()]);
        assert!(state.snapshot().focused.is_none());
        assert_eq!(state.snapshot().selected, vec![tablet.clone()]);
        assert_eq!(state.intent(), intent);

        state.reconcile([phone.clone(), tablet.clone()]);
        assert_eq!(state.snapshot().focused, Some(phone.clone()));
        assert_eq!(state.snapshot().selected, vec![phone, tablet]);

        let other = route("other", 3, 1);
        state.restore_intent(&WorkspaceIntent {
            focused_avd: Some("missing".into()),
            selected_avds: vec!["missing".into()],
        });
        state.reconcile([other.clone()]);
        state.focus(&other).unwrap();
        assert_eq!(
            state.intent(),
            WorkspaceIntent {
                focused_avd: Some("other".into()),
                selected_avds: vec![],
            }
        );
    }
}
