// src/metrics.rs
// System metrics tracking and reporting

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{interval, Duration};
use tracing::{info, warn};

#[derive(Debug, Default)]
pub struct Metrics {
    // Sensor metrics
    pub total_readings: u64,
    pub dropped_readings: u64,
    
    // Jitter tracking (Lab 1)
    pub max_jitter_ms: f64,
    pub jitter_samples: Vec<f64>,
    
    // Deadline tracking (Lab 2)
    pub missed_deadlines: u64,
    pub max_latency_ms: f64,
    
    // Scheduling drift
    pub max_drift_ms: f64,
    pub drift_samples: Vec<f64>,
    
    // Buffer state
    pub buffer_fill_percentage: f32,
    pub max_buffer_fill: f32,
    
    // Fault tracking
    pub fault_count: u64,
    pub consecutive_thermal_misses: u8,
    
    // CPU utilization
    pub cpu_active_time_ms: u128,
    pub cpu_total_time_ms: u128,
    
    // Network metrics
    pub packets_sent: u64,
    pub packets_failed: u64,
    pub commands_received: u64,
    pub commands_processed: u64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            jitter_samples: Vec::with_capacity(100),
            drift_samples: Vec::with_capacity(100),
            ..Default::default()
        }
    }

    /// Record jitter measurement
    pub fn record_jitter(&mut self, jitter_ms: f64) {
        if jitter_ms > self.max_jitter_ms {
            self.max_jitter_ms = jitter_ms;
        }
        
        self.jitter_samples.push(jitter_ms);
        if self.jitter_samples.len() > 100 {
            self.jitter_samples.remove(0);
        }
    }

    /// Record scheduling drift
    pub fn record_drift(&mut self, drift_ms: f64) {
        if drift_ms > self.max_drift_ms {
            self.max_drift_ms = drift_ms;
        }
        
        self.drift_samples.push(drift_ms);
        if self.drift_samples.len() > 100 {
            self.drift_samples.remove(0);
        }
    }

    /// Record latency
    pub fn record_latency(&mut self, latency_ms: f64) {
        if latency_ms > self.max_latency_ms {
            self.max_latency_ms = latency_ms;
        }
    }

    /// Update buffer fill level
    pub fn update_buffer_fill(&mut self, fill_percentage: f32) {
        self.buffer_fill_percentage = fill_percentage;
        if fill_percentage > self.max_buffer_fill {
            self.max_buffer_fill = fill_percentage;
        }
    }

    /// Calculate CPU utilization percentage
    pub fn cpu_utilization(&self) -> f64 {
        if self.cpu_total_time_ms == 0 {
            0.0
        } else {
            (self.cpu_active_time_ms as f64 / self.cpu_total_time_ms as f64) * 100.0
        }
    }

    /// Calculate average jitter from samples
    pub fn avg_jitter(&self) -> f64 {
        if self.jitter_samples.is_empty() {
            0.0
        } else {
            self.jitter_samples.iter().sum::<f64>() / self.jitter_samples.len() as f64
        }
    }

    /// Calculate average drift from samples
    pub fn avg_drift(&self) -> f64 {
        if self.drift_samples.is_empty() {
            0.0
        } else {
            self.drift_samples.iter().sum::<f64>() / self.drift_samples.len() as f64
        }
    }

    /// Calculate packet success rate
    pub fn packet_success_rate(&self) -> f64 {
        let total = self.packets_sent + self.packets_failed;
        if total == 0 {
            100.0
        } else {
            (self.packets_sent as f64 / total as f64) * 100.0
        }
    }

    /// Generate metrics summary for logging
    pub fn summary(&self) -> String {
        format!(
            r#"
Readings: {} total, {} dropped ({:.1}% loss)
Deadlines: {} missed
Jitter: {:.3}ms avg, {:.3}ms max
Drift: {:.3}ms avg, {:.3}ms max
Latency: {:.3}ms max
Buffer: {:.1}% current, {:.1}% peak
Faults: {} total
CPU: {:.1}%
Network: {} sent, {} failed ({:.1}% success)
Commands: {} received, {} processed
"#,
            self.total_readings,
            self.dropped_readings,
            if self.total_readings > 0 {
                (self.dropped_readings as f64 / self.total_readings as f64) * 100.0
            } else {
                0.0
            },
            self.missed_deadlines,
            self.avg_jitter(),
            self.max_jitter_ms,
            self.avg_drift(),
            self.max_drift_ms,
            self.max_latency_ms,
            self.buffer_fill_percentage,
            self.max_buffer_fill,
            self.fault_count,
            self.cpu_utilization(),
            self.packets_sent,
            self.packets_failed,
            self.packet_success_rate(),
            self.commands_received,
            self.commands_processed
        )
    }

    /// Export metrics to CSV format for report
    pub fn to_csv_row(&self) -> String {
        format!(
            "{},{},{},{:.3},{:.3},{:.3},{:.1},{},{:.1},{},{}",
            self.total_readings,
            self.dropped_readings,
            self.missed_deadlines,
            self.max_jitter_ms,
            self.max_drift_ms,
            self.max_latency_ms,
            self.buffer_fill_percentage,
            self.fault_count,
            self.cpu_utilization(),
            self.packets_sent,
            self.commands_received
        )
    }
}

/// Spawn metrics reporter task
pub fn spawn_reporter(
    metrics: Arc<Mutex<Metrics>>,
    interval_secs: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(interval_secs));

        loop {
            ticker.tick().await;

            let m = metrics.lock().await;

            let drop_rate = if m.total_readings > 0 {
                (m.dropped_readings as f64 / m.total_readings as f64) * 100.0
            } else { 0.0 };

            let schedulable = if m.cpu_utilization() <= 75.7 { "YES" } else { "NO" };

            // Alerts
            let mut alerts = String::new();
            if m.buffer_fill_percentage > 80.0 {
                alerts.push_str(&format!("  [!] DEGRADED MODE - buffer at {:.1}%
", m.buffer_fill_percentage));
            }
            if m.max_jitter_ms > 1.0 {
                alerts.push_str(&format!("  [!] Jitter exceeded 1ms: {:.3}ms
", m.max_jitter_ms));
            }
            if m.consecutive_thermal_misses >= 3 {
                alerts.push_str(&format!("  [!] Thermal missed {} consecutive readings
", m.consecutive_thermal_misses));
            }
            if alerts.is_empty() {
                alerts.push_str("  All systems nominal
");
            }

            // Build as one string then print atomically
            let report = format!(
"
+------------------------------------------------------------------+
|              SATELLITE OCS  --  PERIODIC METRICS                 |
+------------------------------------------------------------------+
| SENSOR         Total: {tr:<10} Dropped: {dr:<10} Loss: {loss:.2}% |
| TIMING         Avg Jitter: {aj:.3}ms    Max Jitter: {mj:.3}ms        |
|                Avg Drift:  {ad:.3}ms    Max Drift:  {md:.3}ms       |
|                Max Latency (sensor->buf): {ml:.3}ms                |
| DEADLINES      Missed: {miss:<10} Faults: {faults:<10}             |
| BUFFER         Current: {cbf:.1}%      Peak: {pbf:.1}%                     |
| CPU            Utilization: {cpu:.1}%   RM Schedulable: {sched}           |
| NETWORK        Sent: {ps:<10} Failed: {pf:<10} Rate: {psr:.1}%  |
| COMMANDS       Received: {cr:<10} Processed: {cp:<10}        |
+------------------------------------------------------------------+
| STATUS
{alerts}+------------------------------------------------------------------+
",
                tr    = m.total_readings,
                dr    = m.dropped_readings,
                loss  = drop_rate,
                aj    = m.avg_jitter(),
                mj    = m.max_jitter_ms,
                ad    = m.avg_drift(),
                md    = m.max_drift_ms,
                ml    = m.max_latency_ms,
                miss  = m.missed_deadlines,
                faults = m.fault_count,
                cbf   = m.buffer_fill_percentage,
                pbf   = m.max_buffer_fill,
                cpu   = m.cpu_utilization(),
                sched = schedulable,
                ps    = m.packets_sent,
                pf    = m.packets_failed,
                psr   = m.packet_success_rate(),
                cr    = m.commands_received,
                cp    = m.commands_processed,
                alerts = alerts,
            );

            eprintln!("{}", report);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_recording() {
        let mut m = Metrics::new();
        
        m.record_jitter(0.5);
        m.record_jitter(1.2);
        m.record_jitter(0.8);
        
        assert_eq!(m.max_jitter_ms, 1.2);
        assert_eq!(m.jitter_samples.len(), 3);
        assert!((m.avg_jitter() - 0.833).abs() < 0.01);
    }

    #[test]
    fn test_cpu_utilization() {
        let mut m = Metrics::new();
        m.cpu_active_time_ms = 750;
        m.cpu_total_time_ms = 1000;
        
        assert_eq!(m.cpu_utilization(), 75.0);
    }
}