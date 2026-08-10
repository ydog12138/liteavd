//! 输入到 GTK texture commit 的有界、同进程单调时钟观测。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_PENDING_INPUTS: usize = 2_048;
const MAX_SAMPLES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputToken {
    id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencySample {
    pub queue_delay: Duration,
    pub rpc: Duration,
    pub rpc_to_frame: Duration,
    pub frame_copy: Duration,
    pub gtk_commit: Duration,
    pub dispatch_to_commit: Duration,
    pub end_to_end: Duration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Percentiles {
    pub p50_micros: u64,
    pub p95_micros: u64,
    pub p99_micros: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LatencyReport {
    pub sample_count: usize,
    pub frames_committed: u64,
    pub pending_inputs: usize,
    pub input_failures: u64,
    pub canceled_inputs: u64,
    pub dropped_pending_inputs: u64,
    pub queue_delay: Percentiles,
    pub rpc: Percentiles,
    pub rpc_to_frame: Percentiles,
    pub frame_copy: Percentiles,
    pub gtk_commit: Percentiles,
    pub dispatch_to_commit: Percentiles,
    pub end_to_end: Percentiles,
    pub ui_pump_gap: Percentiles,
    pub max_ui_pump_gap_micros: u64,
}

#[derive(Debug)]
struct PendingInput {
    token: InputToken,
    queued_at: Instant,
    sent_at: Option<Instant>,
    rpc_completed_at: Option<Instant>,
    baseline_counter: Option<u32>,
}

#[derive(Debug, Default)]
struct ProbeState {
    next_id: u64,
    last_committed_counter: Option<u32>,
    pending: VecDeque<PendingInput>,
    samples: VecDeque<LatencySample>,
    ui_pump_gaps: VecDeque<Duration>,
    last_ui_pump_at: Option<Instant>,
    max_ui_pump_gap: Duration,
    frames_committed: u64,
    input_failures: u64,
    canceled_inputs: u64,
    dropped_pending_inputs: u64,
}

#[derive(Debug, Clone, Default)]
pub struct LatencyProbe {
    state: Arc<Mutex<ProbeState>>,
}

impl LatencyProbe {
    pub fn begin_input(&self, queued_at: Instant) -> InputToken {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.next_id = state.next_id.wrapping_add(1).max(1);
        let token = InputToken { id: state.next_id };
        if state.pending.len() == MAX_PENDING_INPUTS {
            state.pending.pop_front();
            state.dropped_pending_inputs = state.dropped_pending_inputs.saturating_add(1);
        }
        let baseline_counter = state.last_committed_counter;
        state.pending.push_back(PendingInput {
            token,
            queued_at,
            sent_at: None,
            rpc_completed_at: None,
            baseline_counter,
        });
        token
    }

    pub fn mark_rpc_started(&self, token: InputToken, sent_at: Instant) {
        if let Some(mut pending) = self.pending_mut(token) {
            pending.sent_at = Some(sent_at);
        }
    }

    pub fn mark_rpc_completed(&self, token: InputToken, completed_at: Instant, success: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(index) = state
            .pending
            .iter()
            .position(|pending| pending.token == token)
        else {
            return;
        };
        if success {
            state.pending[index].rpc_completed_at = Some(completed_at);
        } else {
            state.pending.remove(index);
            state.input_failures = state.input_failures.saturating_add(1);
        }
    }

    pub fn cancel_input(&self, token: InputToken) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(index) = state
            .pending
            .iter()
            .position(|pending| pending.token == token)
        {
            state.pending.remove(index);
            state.canceled_inputs = state.canceled_inputs.saturating_add(1);
        }
    }

    pub fn record_frame_commit(
        &self,
        frame_counter: u32,
        observed_at: Instant,
        copied_at: Instant,
        committed_at: Instant,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.frames_committed = state.frames_committed.saturating_add(1);
        state.last_committed_counter = Some(frame_counter);
        let mut remaining = VecDeque::with_capacity(state.pending.len());
        while let Some(pending) = state.pending.pop_front() {
            let eligible = pending.baseline_counter != Some(frame_counter)
                && pending
                    .rpc_completed_at
                    .is_some_and(|completed| completed <= observed_at);
            if !eligible {
                remaining.push_back(pending);
                continue;
            }
            let Some(sent_at) = pending.sent_at else {
                continue;
            };
            let Some(completed_at) = pending.rpc_completed_at else {
                continue;
            };
            let Some(sample) = latency_sample(
                pending.queued_at,
                sent_at,
                completed_at,
                observed_at,
                copied_at,
                committed_at,
            ) else {
                continue;
            };
            if state.samples.len() == MAX_SAMPLES {
                state.samples.pop_front();
            }
            state.samples.push_back(sample);
        }
        state.pending = remaining;
    }

    pub fn record_ui_pump(&self, pump_at: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(previous) = state.last_ui_pump_at
            && let Some(gap) = pump_at.checked_duration_since(previous)
        {
            if state.ui_pump_gaps.len() == MAX_SAMPLES {
                state.ui_pump_gaps.pop_front();
            }
            state.ui_pump_gaps.push_back(gap);
            state.max_ui_pump_gap = state.max_ui_pump_gap.max(gap);
        }
        state.last_ui_pump_at = Some(pump_at);
    }

    pub fn report(&self) -> LatencyReport {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        LatencyReport {
            sample_count: state.samples.len(),
            frames_committed: state.frames_committed,
            pending_inputs: state.pending.len(),
            input_failures: state.input_failures,
            canceled_inputs: state.canceled_inputs,
            dropped_pending_inputs: state.dropped_pending_inputs,
            queue_delay: percentiles(state.samples.iter().map(|sample| sample.queue_delay)),
            rpc: percentiles(state.samples.iter().map(|sample| sample.rpc)),
            rpc_to_frame: percentiles(state.samples.iter().map(|sample| sample.rpc_to_frame)),
            frame_copy: percentiles(state.samples.iter().map(|sample| sample.frame_copy)),
            gtk_commit: percentiles(state.samples.iter().map(|sample| sample.gtk_commit)),
            dispatch_to_commit: percentiles(
                state.samples.iter().map(|sample| sample.dispatch_to_commit),
            ),
            end_to_end: percentiles(state.samples.iter().map(|sample| sample.end_to_end)),
            ui_pump_gap: percentiles(state.ui_pump_gaps.iter().copied()),
            max_ui_pump_gap_micros: duration_micros(state.max_ui_pump_gap),
        }
    }

    /// 保留当前 frame counter 基线，清空过渡期输入与统计，开始独立测量窗口。
    pub fn reset_measurement(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.pending.clear();
        state.samples.clear();
        state.ui_pump_gaps.clear();
        state.last_ui_pump_at = None;
        state.max_ui_pump_gap = Duration::ZERO;
        state.frames_committed = 0;
        state.input_failures = 0;
        state.canceled_inputs = 0;
        state.dropped_pending_inputs = 0;
    }

    fn pending_mut(&self, token: InputToken) -> Option<PendingGuard<'_>> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let index = state
            .pending
            .iter()
            .position(|pending| pending.token == token)?;
        Some(PendingGuard { state, index })
    }
}

struct PendingGuard<'a> {
    state: std::sync::MutexGuard<'a, ProbeState>,
    index: usize,
}

impl std::ops::Deref for PendingGuard<'_> {
    type Target = PendingInput;

    fn deref(&self) -> &Self::Target {
        &self.state.pending[self.index]
    }
}

impl std::ops::DerefMut for PendingGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state.pending[self.index]
    }
}

fn latency_sample(
    queued_at: Instant,
    sent_at: Instant,
    completed_at: Instant,
    observed_at: Instant,
    copied_at: Instant,
    committed_at: Instant,
) -> Option<LatencySample> {
    Some(LatencySample {
        queue_delay: sent_at.checked_duration_since(queued_at)?,
        rpc: completed_at.checked_duration_since(sent_at)?,
        rpc_to_frame: observed_at.checked_duration_since(completed_at)?,
        frame_copy: copied_at.checked_duration_since(observed_at)?,
        gtk_commit: committed_at.checked_duration_since(copied_at)?,
        dispatch_to_commit: committed_at.checked_duration_since(queued_at)?,
        end_to_end: committed_at.checked_duration_since(sent_at)?,
    })
}

fn percentiles(values: impl Iterator<Item = Duration>) -> Percentiles {
    let mut micros: Vec<_> = values.map(duration_micros).collect();
    if micros.is_empty() {
        return Percentiles::default();
    }
    micros.sort_unstable();
    Percentiles {
        p50_micros: nearest_rank(&micros, 50),
        p95_micros: nearest_rank(&micros, 95),
        p99_micros: nearest_rank(&micros, 99),
    }
}

fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn nearest_rank(sorted: &[u64], percentile: usize) -> u64 {
    let rank = (percentile * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correlates_one_completed_input_with_the_next_new_committed_frame() {
        let probe = LatencyProbe::default();
        let base = Instant::now();
        probe.record_frame_commit(10, base, base, base);
        let token = probe.begin_input(base + Duration::from_millis(1));
        probe.mark_rpc_started(token, base + Duration::from_millis(2));
        probe.mark_rpc_completed(token, base + Duration::from_millis(4), true);

        probe.record_frame_commit(
            10,
            base + Duration::from_millis(5),
            base + Duration::from_millis(6),
            base + Duration::from_millis(7),
        );
        assert_eq!(probe.report().sample_count, 0);
        probe.record_frame_commit(
            11,
            base + Duration::from_millis(8),
            base + Duration::from_millis(10),
            base + Duration::from_millis(12),
        );

        let report = probe.report();
        assert_eq!(report.sample_count, 1);
        assert_eq!(report.queue_delay.p50_micros, 1_000);
        assert_eq!(report.rpc.p50_micros, 2_000);
        assert_eq!(report.rpc_to_frame.p50_micros, 4_000);
        assert_eq!(report.frame_copy.p50_micros, 2_000);
        assert_eq!(report.gtk_commit.p50_micros, 2_000);
        assert_eq!(report.dispatch_to_commit.p50_micros, 11_000);
        assert_eq!(report.end_to_end.p50_micros, 10_000);
    }

    #[test]
    fn failed_canceled_and_over_capacity_inputs_remain_bounded() {
        let probe = LatencyProbe::default();
        let now = Instant::now();
        let failed = probe.begin_input(now);
        probe.mark_rpc_completed(failed, now, false);
        let canceled = probe.begin_input(now);
        probe.cancel_input(canceled);
        for _ in 0..=MAX_PENDING_INPUTS {
            probe.begin_input(now);
        }
        let report = probe.report();
        assert_eq!(report.pending_inputs, MAX_PENDING_INPUTS);
        assert_eq!(report.input_failures, 1);
        assert_eq!(report.canceled_inputs, 1);
        assert_eq!(report.dropped_pending_inputs, 1);
    }

    #[test]
    fn one_new_frame_completes_all_prior_inputs() {
        let probe = LatencyProbe::default();
        let base = Instant::now();
        probe.record_frame_commit(1, base, base, base);
        for offset in 1..=3 {
            let token = probe.begin_input(base + Duration::from_millis(offset));
            probe.mark_rpc_started(token, base + Duration::from_millis(offset + 1));
            probe.mark_rpc_completed(token, base + Duration::from_millis(offset + 2), true);
        }
        probe.record_frame_commit(
            2,
            base + Duration::from_millis(10),
            base + Duration::from_millis(11),
            base + Duration::from_millis(12),
        );
        let report = probe.report();
        assert_eq!(report.sample_count, 3);
        assert_eq!(report.pending_inputs, 0);
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        let values = (1..=100).map(Duration::from_micros);
        assert_eq!(
            percentiles(values),
            Percentiles {
                p50_micros: 50,
                p95_micros: 95,
                p99_micros: 99,
            }
        );
    }

    #[test]
    fn reset_keeps_frame_baseline_but_clears_measurement_window() {
        let probe = LatencyProbe::default();
        let base = Instant::now();
        probe.record_frame_commit(7, base, base, base);
        let old = probe.begin_input(base);
        probe.mark_rpc_started(old, base);
        probe.reset_measurement();
        let fresh = probe.begin_input(base + Duration::from_millis(1));
        probe.mark_rpc_started(fresh, base + Duration::from_millis(2));
        probe.mark_rpc_completed(fresh, base + Duration::from_millis(3), true);
        probe.record_frame_commit(
            7,
            base + Duration::from_millis(4),
            base + Duration::from_millis(5),
            base + Duration::from_millis(6),
        );
        assert_eq!(probe.report().sample_count, 0);
        probe.record_frame_commit(
            8,
            base + Duration::from_millis(7),
            base + Duration::from_millis(8),
            base + Duration::from_millis(9),
        );
        let report = probe.report();
        assert_eq!(report.sample_count, 1);
        assert_eq!(report.pending_inputs, 0);
        assert_eq!(report.frames_committed, 2);
    }

    #[test]
    fn maximum_ui_pump_gap_survives_rolling_percentile_eviction_and_resets() {
        let probe = LatencyProbe::default();
        let base = Instant::now();
        probe.record_ui_pump(base);
        probe.record_ui_pump(base + Duration::from_millis(500));
        for offset in 1..=MAX_SAMPLES + 1 {
            probe.record_ui_pump(
                base + Duration::from_millis(500) + Duration::from_millis(offset as u64),
            );
        }
        let report = probe.report();
        assert_eq!(report.ui_pump_gap.p99_micros, 1_000);
        assert_eq!(report.max_ui_pump_gap_micros, 500_000);

        probe.reset_measurement();
        assert_eq!(probe.report().max_ui_pump_gap_micros, 0);
    }
}
