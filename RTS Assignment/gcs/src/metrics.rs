// src/metrics.rs

use tokio::sync::Mutex;
use tokio::time::{interval, Duration};
use std::sync::Arc;
use tracing::{info, warn};

// ── Rejection record (owned here, no cross-module dependency) ─────────────────
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
    pub packets_received:         u64,
    pub decode_errors:            u64,
    pub sequence_gaps:            u64,
    pub loss_of_contact_events:   u64,
    pub missed_decode_deadlines:  u64,
    pub rerequests_sent:          u64,

    // Reception timing
    pub max_reception_drift_ms:   f64,
    pub total_reception_drift_ms: f64,
    pub drift_samples:            u64,
    pub max_decode_latency_ms:    f64,
    pub total_decode_latency_ms:  f64,
    pub max_reception_latency_ms: f64,

    // Command dispatch
    pub commands_scheduled:       u64,
    pub commands_sent:            u64,
    pub commands_failed:          u64,
    pub commands_blocked:         u64,
    pub missed_urgent_deadlines:  u64,
    pub max_dispatch_latency_ms:  f64,
    pub total_dispatch_latency_ms: f64,
    pub dispatch_samples:         u64,

    // Uplink jitter
    pub max_uplink_jitter_ms:     f64,
    pub total_uplink_jitter_ms:   f64,
    pub uplink_jitter_samples:    u64,

    // ACKs
    pub acks_received:            u64,
    pub acks_missed:              u64,
    pub commands_nacked:          u64,

    // Fault / interlock
    pub faults_received:                  u64,
    pub interlocks_engaged:               u64,
    pub interlocks_released:              u64,
    pub missed_fault_response_deadlines:  u64,
    pub max_interlock_latency_ms:         f64,
    pub total_interlock_latency_ms:       f64,
    pub interlock_samples:                u64,

    // Scheduler drift
    pub max_scheduler_drift_ms:    f64,
    pub total_scheduler_drift_ms:  f64,
    pub scheduler_drift_samples:   u64,
    pub scheduler_drops:           u64,

    // Rejection log (last 100)
    pub rejection_log: Vec<RejectionRecord>,
}

impl GcsMetrics {
    pub fn new() -> Self { Self::default() }

    pub fn record_reception_drift(&mut self, ms: f64) {
        if ms > self.max_reception_drift_ms { self.max_reception_drift_ms = ms; }
        self.total_reception_drift_ms += ms;
        self.drift_samples += 1;
    }

    pub fn record_decode_latency(&mut self, ms: f64) {
        if ms > self.max_decode_latency_ms { self.max_decode_latency_ms = ms; }
        self.total_decode_latency_ms += ms;
    }

    pub fn record_reception_latency(&mut self, ms: f64) {
        if ms > self.max_reception_latency_ms { self.max_reception_latency_ms = ms; }
    }

    pub fn record_dispatch_latency(&mut self, ms: f64) {
        if ms > self.max_dispatch_latency_ms { self.max_dispatch_latency_ms = ms; }
        self.total_dispatch_latency_ms += ms;
        self.dispatch_samples += 1;
    }

    pub fn record_uplink_jitter(&mut self, ms: f64) {
        if ms > self.max_uplink_jitter_ms { self.max_uplink_jitter_ms = ms; }
        self.total_uplink_jitter_ms += ms;
        self.uplink_jitter_samples += 1;
    }

    pub fn record_interlock_latency(&mut self, ms: f64) {
        if ms > self.max_interlock_latency_ms { self.max_interlock_latency_ms = ms; }
        self.total_interlock_latency_ms += ms;
        self.interlock_samples += 1;
    }

    pub fn record_scheduler_drift(&mut self, ms: f64) {
        if ms > self.max_scheduler_drift_ms { self.max_scheduler_drift_ms = ms; }
        self.total_scheduler_drift_ms += ms;
        self.scheduler_drift_samples += 1;
    }

    pub fn log_rejection(&mut self, record: RejectionRecord) {
        if self.rejection_log.len() >= 100 { self.rejection_log.remove(0); }
        warn!("REJECTION: {}", record);
        self.rejection_log.push(record);
    }

    fn avg(&self, total: f64, n: u64) -> f64 {
        if n == 0 { 0.0 } else { total / n as f64 }
    }

    pub fn summary(&self) -> String {
        format!(
"Telemetry : {} recv  {} errors  {} gaps  {} loss-of-contact
Decode    : {:.3}ms avg  {:.3}ms max  {} deadline misses
Drift     : {:.3}ms avg  {:.3}ms max
Commands  : {} scheduled  {} sent  {} blocked  {} failed
Dispatch  : {:.3}ms avg  {:.3}ms max  {} urgent misses
Jitter    : {:.3}ms avg  {:.3}ms max
ACKs      : {} recv  {} missed  {} nacked
Faults    : {} recv  {} interlocks  {} response misses
Rejections: {} total",
            self.packets_received, self.decode_errors,
            self.sequence_gaps, self.loss_of_contact_events,
            self.avg(self.total_decode_latency_ms, self.packets_received),
            self.max_decode_latency_ms, self.missed_decode_deadlines,
            self.avg(self.total_reception_drift_ms, self.drift_samples),
            self.max_reception_drift_ms,
            self.commands_scheduled, self.commands_sent,
            self.commands_blocked, self.commands_failed,
            self.avg(self.total_dispatch_latency_ms, self.dispatch_samples),
            self.max_dispatch_latency_ms, self.missed_urgent_deadlines,
            self.avg(self.total_uplink_jitter_ms, self.uplink_jitter_samples),
            self.max_uplink_jitter_ms,
            self.acks_received, self.acks_missed, self.commands_nacked,
            self.faults_received, self.interlocks_engaged,
            self.missed_fault_response_deadlines,
            self.rejection_log.len(),
        )
    }
}

pub fn spawn_reporter(
    metrics: Arc<Mutex<GcsMetrics>>,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(interval_secs));
        loop {
            ticker.tick().await;
            let m = metrics.lock().await;
            info!("=================================================");
            info!("📊 GCS METRICS:\n{}", m.summary());
            info!("=================================================");
            if m.max_reception_drift_ms > 5.0 {
                warn!("⚠️  High reception drift: {:.3}ms", m.max_reception_drift_ms);
            }
        }
    })
}