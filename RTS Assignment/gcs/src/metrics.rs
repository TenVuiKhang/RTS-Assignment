// src/metrics.rs

use tokio::sync::Mutex;
use tokio::time::{interval, Duration};
use std::sync::Arc;
use tracing::{info, warn};

// ── Rejection record ──────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct RejectionRecord {
    pub command_id:           u64,
    pub urgency:              String,
    pub payload_summary:      String,
    pub reason:               String,
    pub timestamp:            String,
    pub interlock_latency_ms: f64,
}

impl std::fmt::Display for RejectionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] cmd #{} ({}/{}) BLOCKED — {} | latency {:.3}ms",
            self.timestamp, self.command_id,
            self.urgency, self.payload_summary,
            self.reason, self.interlock_latency_ms,
        )
    }
}

// ── Metrics struct ────────────────────────────────────────────────────────────
#[derive(Debug, Default)]
pub struct GcsMetrics {
    // Telemetry reception
    pub packets_received:        u64,
    pub decode_errors:           u64,
    pub sequence_gaps:           u64,
    pub loss_of_contact_events:  u64,
    pub missed_decode_deadlines: u64,
    pub rerequests_sent:         u64,

    // Reception drift (actual vs expected arrival)
    pub min_reception_drift_ms:   f64,
    pub max_reception_drift_ms:   f64,
    pub total_reception_drift_ms: f64,
    pub drift_samples:            u64,

    // Decode latency
    pub min_decode_latency_ms:   f64,
    pub max_decode_latency_ms:   f64,
    pub total_decode_latency_ms: f64,
    pub decode_samples:          u64,

    // Reception latency (drift + decode combined)
    pub min_reception_latency_ms:   f64,
    pub max_reception_latency_ms:   f64,
    pub total_reception_latency_ms: f64,
    pub reception_latency_samples:  u64,

    // Command dispatch
    pub commands_scheduled:        u64,
    pub commands_sent:             u64,
    pub commands_failed:           u64,
    pub commands_blocked:          u64,
    pub missed_urgent_deadlines:   u64,
    pub min_dispatch_latency_ms:   f64,
    pub max_dispatch_latency_ms:   f64,
    pub total_dispatch_latency_ms: f64,
    pub dispatch_samples:          u64,

    // Uplink jitter
    pub min_uplink_jitter_ms:   f64,
    pub max_uplink_jitter_ms:   f64,
    pub total_uplink_jitter_ms: f64,
    pub uplink_jitter_samples:  u64,

    // ACKs
    pub acks_received:   u64,
    pub acks_missed:     u64,
    pub commands_nacked: u64,

    // Fault and interlock
    pub faults_received:                 u64,
    pub interlocks_engaged:              u64,
    pub interlocks_released:             u64,
    pub missed_fault_response_deadlines: u64,
    pub min_interlock_latency_ms:        f64,
    pub max_interlock_latency_ms:        f64,
    pub total_interlock_latency_ms:      f64,
    pub interlock_samples:               u64,
    // Fault recovery time (fault detected → interlock released)
    pub min_fault_recovery_ms:   f64,
    pub max_fault_recovery_ms:   f64,
    pub total_fault_recovery_ms: f64,
    pub fault_recovery_samples:  u64,

    // Scheduler drift
    pub min_scheduler_drift_ms:   f64,
    pub max_scheduler_drift_ms:   f64,
    pub total_scheduler_drift_ms: f64,
    pub scheduler_drift_samples:  u64,
    pub scheduler_drops:          u64,

    // Rejection log (last 100)
    pub rejection_log: Vec<RejectionRecord>,
}

impl GcsMetrics {
    pub fn new() -> Self {
        // Initialise min fields to f64::MAX so first real sample always wins
        Self {
            min_reception_drift_ms:   f64::MAX,
            min_decode_latency_ms:    f64::MAX,
            min_reception_latency_ms: f64::MAX,
            min_dispatch_latency_ms:  f64::MAX,
            min_uplink_jitter_ms:     f64::MAX,
            min_interlock_latency_ms: f64::MAX,
            min_fault_recovery_ms:    f64::MAX,
            min_scheduler_drift_ms:   f64::MAX,
            ..Default::default()
        }
    }

    // ── Record helpers ────────────────────────────────────────────────────

    pub fn record_reception_drift(&mut self, ms: f64) {
        if ms < self.min_reception_drift_ms { self.min_reception_drift_ms = ms; }
        if ms > self.max_reception_drift_ms { self.max_reception_drift_ms = ms; }
        self.total_reception_drift_ms += ms;
        self.drift_samples += 1;
    }

    pub fn record_decode_latency(&mut self, ms: f64) {
        if ms < self.min_decode_latency_ms { self.min_decode_latency_ms = ms; }
        if ms > self.max_decode_latency_ms { self.max_decode_latency_ms = ms; }
        self.total_decode_latency_ms += ms;
        self.decode_samples += 1;
    }

    pub fn record_reception_latency(&mut self, ms: f64) {
        if ms < self.min_reception_latency_ms { self.min_reception_latency_ms = ms; }
        if ms > self.max_reception_latency_ms { self.max_reception_latency_ms = ms; }
        self.total_reception_latency_ms += ms;
        self.reception_latency_samples += 1;
    }

    pub fn record_dispatch_latency(&mut self, ms: f64) {
        if ms < self.min_dispatch_latency_ms { self.min_dispatch_latency_ms = ms; }
        if ms > self.max_dispatch_latency_ms { self.max_dispatch_latency_ms = ms; }
        self.total_dispatch_latency_ms += ms;
        self.dispatch_samples += 1;
    }

    pub fn record_uplink_jitter(&mut self, ms: f64) {
        if ms < self.min_uplink_jitter_ms { self.min_uplink_jitter_ms = ms; }
        if ms > self.max_uplink_jitter_ms { self.max_uplink_jitter_ms = ms; }
        self.total_uplink_jitter_ms += ms;
        self.uplink_jitter_samples += 1;
    }

    pub fn record_interlock_latency(&mut self, ms: f64) {
        if ms < self.min_interlock_latency_ms { self.min_interlock_latency_ms = ms; }
        if ms > self.max_interlock_latency_ms { self.max_interlock_latency_ms = ms; }
        self.total_interlock_latency_ms += ms;
        self.interlock_samples += 1;
    }

    pub fn record_fault_recovery(&mut self, ms: f64) {
        if ms < self.min_fault_recovery_ms { self.min_fault_recovery_ms = ms; }
        if ms > self.max_fault_recovery_ms { self.max_fault_recovery_ms = ms; }
        self.total_fault_recovery_ms += ms;
        self.fault_recovery_samples += 1;
    }

    pub fn record_scheduler_drift(&mut self, ms: f64) {
        if ms < self.min_scheduler_drift_ms { self.min_scheduler_drift_ms = ms; }
        if ms > self.max_scheduler_drift_ms { self.max_scheduler_drift_ms = ms; }
        self.total_scheduler_drift_ms += ms;
        self.scheduler_drift_samples += 1;
    }

    pub fn log_rejection(&mut self, record: RejectionRecord) {
        if self.rejection_log.len() >= 100 { self.rejection_log.remove(0); }
        warn!("REJECTION: {}", record);
        self.rejection_log.push(record);
    }

    pub fn rejection_report(&self) -> String {
        let div = "+------------------------------------------------------------------------------+";
        if self.rejection_log.is_empty() {
            return format!("{div}\n| COMMAND REJECTION LOG — no rejections recorded{:>30} |\n{div}", "");
        }

        let mut lines = format!(
            "{div}\n| COMMAND REJECTION LOG — {} rejection(s){:>37} |\n{div}\n",
            self.rejection_log.len(), ""
        );

        for (i, r) in self.rejection_log.iter().enumerate() {
            lines.push_str(&format!(
                "| #{:<4} cmd {:>6}  [{:<9}]  {:<30}  latency {:.3}ms{:>2} |\n| {:<12}  Reason: {:<57} |\n",
                i + 1,
                r.command_id,
                r.urgency,
                r.payload_summary,
                r.interlock_latency_ms, "",
                r.timestamp,
                r.reason,
            ));
            if i < self.rejection_log.len() - 1 {
                lines.push_str(&format!("| {:-<76} |\n", ""));
            }
        }

        lines.push_str(div);
        lines
    }

    // ── Safe min display (returns 0.0 if no samples yet) ─────────────────
    fn safe_min(val: f64) -> f64 {
        if val == f64::MAX { 0.0 } else { val }
    }

    fn avg(total: f64, n: u64) -> f64 {
        if n == 0 { 0.0 } else { total / n as f64 }
    }

    fn ack_rate(&self) -> f64 {
        let total = self.acks_received + self.acks_missed;
        if total == 0 { 100.0 } else { self.acks_received as f64 / total as f64 * 100.0 }
    }

    // ── Formatted summary (mirrors OCS periodic metrics style) ───────────
    pub fn summary(&self) -> String {
        let div  = "+------------------------------------------------------------------------------+";
        let hdiv = "+---------------------------  GCS PERIODIC METRICS  --------------------------+";

        let reception_drift_avg = Self::avg(self.total_reception_drift_ms, self.drift_samples);
        let decode_avg          = Self::avg(self.total_decode_latency_ms,   self.decode_samples);
        let latency_avg         = Self::avg(self.total_reception_latency_ms, self.reception_latency_samples);
        let dispatch_avg        = Self::avg(self.total_dispatch_latency_ms,  self.dispatch_samples);
        let jitter_avg          = Self::avg(self.total_uplink_jitter_ms,     self.uplink_jitter_samples);
        let sched_drift_avg     = Self::avg(self.total_scheduler_drift_ms,   self.scheduler_drift_samples);
        let interlock_avg       = Self::avg(self.total_interlock_latency_ms, self.interlock_samples);
        let recovery_avg        = Self::avg(self.total_fault_recovery_ms,    self.fault_recovery_samples);

        let loss_pct = if self.packets_received + self.decode_errors == 0 { 0.0 }
            else { self.decode_errors as f64 / (self.packets_received + self.decode_errors) as f64 * 100.0 };

        let cmd_block_pct = if self.commands_scheduled == 0 { 0.0 }
            else { self.commands_blocked as f64 / self.commands_scheduled as f64 * 100.0 };

        format!(
"{div}\n\
| {hdiv:^76} |\n\
{div}\n\
| {:<12}  Total: {:<6}  Errors: {:<5}  Gaps: {:<5}  Loss: {:.2}%{:>5} |\n\
| {:<12}  Re-requests: {:<5}  Loss-of-contact events: {:<4}{:>13} |\n\
{div}\n\
| {:<12}  Avg Drift: {:.3}ms    Min: {:.3}ms    Max: {:.3}ms{:>4} |\n\
| {:<12}  Avg Decode: {:.3}ms   Min: {:.3}ms    Max: {:.3}ms{:>4} |\n\
| {:<12}  Avg Latency: {:.3}ms  Min: {:.3}ms    Max: {:.3}ms{:>4} |\n\
{div}\n\
| {:<12}  Scheduled: {:<6}  Sent: {:<6}  Blocked: {:<5} ({:.1}%){:>2} |\n\
| {:<12}  Avg Dispatch: {:.3}ms  Min: {:.3}ms  Max: {:.3}ms{:>5} |\n\
| {:<12}  Avg Jitter: {:.3}ms    Min: {:.3}ms  Max: {:.3}ms{:>5} |\n\
| {:<12}  ACKs: {:<6}  Missed: {:<5}  NACKed: {:<5}  Rate: {:.1}%{:>1} |\n\
{div}\n\
| {:<12}  Missed Decode: {:<4}  Missed Urgent Dispatch: {:<4}{:>14} |\n\
| {:<12}  Sched Drops: {:<5}  Avg Sched Drift: {:.3}ms  Max: {:.3}ms{:>3} |\n\
{div}\n\
| {:<12}  Faults Recv: {:<4}  Interlocks: {:<4}  Released: {:<4}{:>10} |\n\
| {:<12}  Avg Interlock Latency: {:.3}ms    Max: {:.3}ms{:>14} |\n\
| {:<12}  Avg Recovery: {:.3}ms    Max: {:.3}ms  Samples: {:<5}{:>4} |\n\
| {:<12}  Missed Fault Response Deadlines: {:<5}{:>19} |\n\
| {:<12}  Command Rejections: {:<5}{:>35} |\n\
{div}",
            // Telemetry row 1
            "TELEMETRY", self.packets_received, self.decode_errors, self.sequence_gaps, loss_pct, "",
            // Telemetry row 2
            "", self.rerequests_sent, self.loss_of_contact_events, "",
            // Drift
            "DRIFT", reception_drift_avg, Self::safe_min(self.min_reception_drift_ms), self.max_reception_drift_ms, "",
            // Decode
            "DECODE", decode_avg, Self::safe_min(self.min_decode_latency_ms), self.max_decode_latency_ms, "",
            // Latency
            "LATENCY", latency_avg, Self::safe_min(self.min_reception_latency_ms), self.max_reception_latency_ms, "",
            // Commands row 1
            "COMMANDS", self.commands_scheduled, self.commands_sent, self.commands_blocked, cmd_block_pct, "",
            // Commands row 2
            "", dispatch_avg, Self::safe_min(self.min_dispatch_latency_ms), self.max_dispatch_latency_ms, "",
            // Jitter
            "UPLINK", jitter_avg, Self::safe_min(self.min_uplink_jitter_ms), self.max_uplink_jitter_ms, "",
            // ACKs
            "ACK", self.acks_received, self.acks_missed, self.commands_nacked, self.ack_rate(), "",
            // Deadlines
            "DEADLINES", self.missed_decode_deadlines, self.missed_urgent_deadlines, "",
            // Scheduler
            "SCHEDULER", self.scheduler_drops, sched_drift_avg, self.max_scheduler_drift_ms, "",
            // Faults row 1
            "FAULTS", self.faults_received, self.interlocks_engaged, self.interlocks_released, "",
            // Faults row 2
            "", interlock_avg, self.max_interlock_latency_ms, "",
            // Recovery
            "RECOVERY", recovery_avg, self.max_fault_recovery_ms, self.fault_recovery_samples, "",
            // Missed fault deadlines
            "", self.missed_fault_response_deadlines, "",
            // Rejections
            "INTERLOCK", self.rejection_log.len(), "",
        )
    }
}

// ── Periodic metrics (printed during runtime) ────────────────────────────────
impl GcsMetrics {
    pub fn periodic_summary(&self) -> String {
        let div = "+------------------------------------------------------------------------------+";
        let reception_drift_avg = Self::avg(self.total_reception_drift_ms, self.drift_samples);
        let decode_avg          = Self::avg(self.total_decode_latency_ms,   self.decode_samples);
        let latency_avg         = Self::avg(self.total_reception_latency_ms, self.reception_latency_samples);
        let dispatch_avg        = Self::avg(self.total_dispatch_latency_ms,  self.dispatch_samples);
        let jitter_avg          = Self::avg(self.total_uplink_jitter_ms,     self.uplink_jitter_samples);

        format!(
"{div}\n\
|                        GCS  --  PERIODIC METRICS                             |\n\
{div}\n\
| {:<10}  Total: {:<6}  Errors: {:<4}  Gaps: {:<4}  Reqs: {:<4}  Loss: {:.2}%{:>3} |\n\
{div}\n\
| {:<10}  Avg Drift: {:.3}ms     Min: {:.3}ms     Max: {:.3}ms{:>8} |\n\
| {:<10}  Avg Decode: {:.3}ms    Min: {:.3}ms     Max: {:.3}ms{:>8} |\n\
| {:<10}  Avg Latency: {:.3}ms   Min: {:.3}ms     Max: {:.3}ms{:>8} |\n\
{div}\n\
| {:<10}  Scheduled: {:<5}  Sent: {:<5}  Blocked: {:<4}  Failed: {:<4}{:>8} |\n\
| {:<10}  Avg Dispatch: {:.3}ms  Min: {:.3}ms  Max: {:.3}ms{:>10} |\n\
| {:<10}  Avg Jitter: {:.3}ms    Min: {:.3}ms  Max: {:.3}ms{:>10} |\n\
| {:<10}  ACKs: {:<5}  Missed: {:<4}  NACKed: {:<4}  Rate: {:.1}%{:>8} |\n\
{div}\n\
| {:<10}  Missed Decode: {:<4}  Missed Urgent: {:<4}  Sched Drops: {:<4}{:>8} |\n\
{div}\n\
| {:<10}  Faults: {:<4}  Interlocks: {:<4}  Rejections: {:<4}{:>15} |\n\
{div}",
            // Telemetry
            "TELEMETRY",
            self.packets_received, self.decode_errors, self.sequence_gaps,
            self.rerequests_sent,
            if self.packets_received + self.decode_errors == 0 { 0.0 }
            else { self.decode_errors as f64 / (self.packets_received + self.decode_errors) as f64 * 100.0 },
            "",
            // Drift / decode / latency
            "DRIFT",   reception_drift_avg, Self::safe_min(self.min_reception_drift_ms), self.max_reception_drift_ms, "",
            "DECODE",  decode_avg,          Self::safe_min(self.min_decode_latency_ms),  self.max_decode_latency_ms,  "",
            "LATENCY", latency_avg,         Self::safe_min(self.min_reception_latency_ms), self.max_reception_latency_ms, "",
            // Commands
            "COMMANDS",
            self.commands_scheduled, self.commands_sent, self.commands_blocked, self.commands_failed, "",
            "", dispatch_avg, Self::safe_min(self.min_dispatch_latency_ms), self.max_dispatch_latency_ms, "",
            "UPLINK",  jitter_avg, Self::safe_min(self.min_uplink_jitter_ms), self.max_uplink_jitter_ms, "",
            "ACK", self.acks_received, self.acks_missed, self.commands_nacked, self.ack_rate(), "",
            // Deadlines
            "DEADLINES",
            self.missed_decode_deadlines, self.missed_urgent_deadlines, self.scheduler_drops, "",
            // Faults
            "FAULTS", self.faults_received, self.interlocks_engaged, self.rejection_log.len(), "",
        )
    }
}

// ── Reporter task — periodic metrics during runtime ───────────────────────────
pub fn spawn_reporter(
    metrics:       Arc<Mutex<GcsMetrics>>,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(interval_secs));
        loop {
            ticker.tick().await;
            let m = metrics.lock().await;

            // Print periodic snapshot
            info!("\n{}", m.periodic_summary());

            // Live warnings beneath the table
            if m.max_reception_drift_ms > 5.0 {
                warn!("[!] High reception drift: {:.3}ms", m.max_reception_drift_ms);
            }
            if m.missed_decode_deadlines > 0 {
                warn!("[!] Decode deadline misses: {}", m.missed_decode_deadlines);
            }
            if m.loss_of_contact_events > 0 {
                warn!("[!] Loss-of-contact events: {}", m.loss_of_contact_events);
            }
            if m.missed_fault_response_deadlines > 0 {
                warn!("[!] Fault response deadline misses: {}", m.missed_fault_response_deadlines);
            }
            if !m.rejection_log.is_empty() {
                warn!("[!] {} command(s) rejected so far", m.rejection_log.len());
            }
        }
    })
}