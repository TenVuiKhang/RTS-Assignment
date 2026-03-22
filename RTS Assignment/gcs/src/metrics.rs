// src/metrics.rs  –  GCS performance tracking

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};
use tracing::{info, warn};

#[derive(Debug, Default)]
pub struct GcsMetrics {
    // Telemetry reception
    pub packets_received: u64,
    pub decode_errors: u64,
    pub sequence_gaps: u64,
    pub loss_of_contact_events: u64,
    pub missed_decode_deadlines: u64,

    // Reception timing
    pub max_reception_drift_ms: f64,
    pub total_reception_drift_ms: f64,
    pub drift_samples: u64,

    // Decode latency
    pub max_decode_latency_ms: f64,
    pub total_decode_latency_ms: f64,

    // Command dispatch
    pub commands_scheduled: u64,
    pub commands_sent: u64,
    pub commands_failed: u64,
    pub commands_blocked: u64,
    pub missed_urgent_deadlines: u64,
    pub max_dispatch_latency_ms: f64,
    pub total_dispatch_latency_ms: f64,
    pub dispatch_samples: u64,

    // ACK tracking
    pub acks_received: u64,
    pub acks_missed: u64,
    pub commands_nacked: u64,

    // Fault / interlock
    pub faults_received: u64,
    pub interlocks_engaged: u64,
    pub interlocks_released: u64,
    pub missed_fault_response_deadlines: u64,
    pub max_interlock_latency_ms: f64,
    pub total_interlock_latency_ms: f64,
    pub interlock_samples: u64,

    // Scheduler drift (GCS side)
    pub max_scheduler_drift_ms: f64,
    pub total_scheduler_drift_ms: f64,
    pub scheduler_drift_samples: u64,
    pub scheduler_drops: u64,

    // Rejection log (last 100 entries — full struct from interlock.rs)
    pub rejection_log: Vec<crate::interlock::RejectionRecord>,
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

    pub fn record_dispatch_latency(&mut self, ms: f64) {
        if ms > self.max_dispatch_latency_ms { self.max_dispatch_latency_ms = ms; }
        self.total_dispatch_latency_ms += ms;
        self.dispatch_samples += 1;
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

    pub fn log_rejection(&mut self, record: crate::interlock::RejectionRecord) {
        if self.rejection_log.len() >= 100 { self.rejection_log.remove(0); }
        tracing::warn!("REJECTION LOG: {}", record);
        self.rejection_log.push(record);
    }

    pub fn avg_drift_ms(&self) -> f64 {
        if self.drift_samples == 0 { 0.0 }
        else { self.total_reception_drift_ms / self.drift_samples as f64 }
    }

    pub fn avg_dispatch_ms(&self) -> f64 {
        if self.dispatch_samples == 0 { 0.0 }
        else { self.total_dispatch_latency_ms / self.dispatch_samples as f64 }
    }

    pub fn summary(&self) -> String {
        format!(
            r#"
Telemetry:  {} received  |  {} decode errors  |  {} gaps  |  {} loss-of-contact
Decode:     {:.3}ms avg  |  {:.3}ms max  |  {} deadline misses
Drift:      {:.3}ms avg  |  {:.3}ms max
Commands:   {} scheduled  |  {} sent  |  {} blocked  |  {} failed
Dispatch:   {:.3}ms avg  |  {:.3}ms max  |  {} urgent deadline misses
ACKs:       {} received  |  {} missed  |  {} NACKed
Faults:     {} received  |  {} interlock releases  |  {} response deadline misses
"#,
            self.packets_received, self.decode_errors, self.sequence_gaps, self.loss_of_contact_events,
            self.total_decode_latency_ms / self.packets_received.max(1) as f64,
            self.max_decode_latency_ms, self.missed_decode_deadlines,
            self.avg_drift_ms(), self.max_reception_drift_ms,
            self.commands_scheduled, self.commands_sent, self.commands_blocked, self.commands_failed,
            self.avg_dispatch_ms(), self.max_dispatch_latency_ms, self.missed_urgent_deadlines,
            self.acks_received, self.acks_missed, self.commands_nacked,
            self.faults_received, self.interlocks_released, self.missed_fault_response_deadlines,
        )
    }
}

pub fn spawn_reporter(metrics: Arc<Mutex<GcsMetrics>>, interval_secs: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(interval_secs));
        loop {
            ticker.tick().await;
            let m = metrics.lock().await;
            info!("=================================================");
            info!("📊 GCS METRICS:\n{}", m.summary());
            if m.max_reception_drift_ms > 5.0 {
                warn!("⚠️  High reception drift: {:.3}ms", m.max_reception_drift_ms);
            }
        }
    })
}