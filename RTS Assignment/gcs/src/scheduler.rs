// src/scheduler.rs

use crate::protocol::{CommandPacket, CommandPayload, CommandUrgency};
use crate::metrics::GcsMetrics;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{interval, Duration, Instant};
use std::sync::Arc;
use chrono::Utc;
use tracing::{info, warn, debug};

const HEALTH_PERIOD_MS:   u64 = 1000;
const THERMAL_PERIOD_MS:  u64 = 200;
const ANTENNA_PERIOD_MS:  u64 = 500;
const ROUTINE_DEADLINE_MS: f64 = 30.0;

// ── ScheduledCommand carries timing metadata alongside the packet ─────────────
pub struct ScheduledCommand {
    pub packet:       CommandPacket,
    pub scheduled_at: Instant,   // when scheduler created this — used for jitter
    pub deadline_ms:  f64,
}

// Instant is Send, so ScheduledCommand is too
unsafe impl Send for ScheduledCommand {}

pub fn spawn_scheduler(
    cmd_tx:  mpsc::Sender<ScheduledCommand>,
    metrics: Arc<Mutex<GcsMetrics>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut thermal_ticker = interval(Duration::from_millis(THERMAL_PERIOD_MS));
        let mut antenna_ticker = interval(Duration::from_millis(ANTENNA_PERIOD_MS));
        let mut health_ticker  = interval(Duration::from_millis(HEALTH_PERIOD_MS));

        let mut thermal_expected = Instant::now();
        let mut antenna_expected = Instant::now();
        let mut health_expected  = Instant::now();

        let mut cmd_id = 5_000u64;  // scheduler commands start at 5000

        info!("📅 GCS scheduler started");
        info!("   Thermal check: {}ms  Antenna check: {}ms  Health ping: {}ms",
            THERMAL_PERIOD_MS, ANTENNA_PERIOD_MS, HEALTH_PERIOD_MS);

        loop {
            tokio::select! {
                _ = thermal_ticker.tick() => {
                    let now = Instant::now();
                    record_drift(&metrics, "ThermalCheck", now, thermal_expected).await;
                    thermal_expected = now + Duration::from_millis(THERMAL_PERIOD_MS);

                    enqueue(&cmd_tx, &metrics, &mut cmd_id, ROUTINE_DEADLINE_MS,
                        CommandPayload::RequestData {
                            data_type: "thermal".into(),
                            time_range_seconds: Some(1),
                        },
                    ).await;
                }
                _ = antenna_ticker.tick() => {
                    let now = Instant::now();
                    record_drift(&metrics, "AntennaCheck", now, antenna_expected).await;
                    antenna_expected = now + Duration::from_millis(ANTENNA_PERIOD_MS);

                    enqueue(&cmd_tx, &metrics, &mut cmd_id, ROUTINE_DEADLINE_MS,
                        CommandPayload::RequestData {
                            data_type: "attitude".into(),
                            time_range_seconds: Some(1),
                        },
                    ).await;
                }
                _ = health_ticker.tick() => {
                    let now = Instant::now();
                    record_drift(&metrics, "HealthPing", now, health_expected).await;
                    health_expected = now + Duration::from_millis(HEALTH_PERIOD_MS);

                    enqueue(&cmd_tx, &metrics, &mut cmd_id, ROUTINE_DEADLINE_MS,
                        CommandPayload::HealthCheck,
                    ).await;
                }
            }
        }
    })
}

async fn enqueue(
    tx:         &mpsc::Sender<ScheduledCommand>,
    metrics:    &Arc<Mutex<GcsMetrics>>,
    id:         &mut u64,
    deadline:   f64,
    payload:    CommandPayload,
) {
    let sc = ScheduledCommand {
        packet: CommandPacket {
            command_id: *id,
            timestamp:  Utc::now(),
            urgency:    CommandUrgency::Routine,
            payload,
        },
        scheduled_at: Instant::now(),
        deadline_ms:  deadline,
    };
    debug!("📋 Scheduling cmd #{}", id);
    match tx.try_send(sc) {
        Ok(_) => { metrics.lock().await.commands_scheduled += 1; *id += 1; }
        Err(_) => { warn!("⚠️  Scheduler channel full — cmd #{} dropped", id); metrics.lock().await.scheduler_drops += 1; }
    }
}

async fn record_drift(metrics: &Arc<Mutex<GcsMetrics>>, name: &str, actual: Instant, expected: Instant) {
    let drift_ms = if actual > expected {
        actual.duration_since(expected).as_secs_f64() * 1000.0
    } else { 0.0 };
    if drift_ms > 1.0 { warn!("⚠️  GCS '{}' drift: {:.3}ms", name, drift_ms); }
    metrics.lock().await.record_scheduler_drift(drift_ms);
}

/// Called in uplink.rs — jitter = time between scheduler enqueue and actual send
pub fn measure_uplink_jitter(cmd: &ScheduledCommand) -> f64 {
    cmd.scheduled_at.elapsed().as_secs_f64() * 1000.0
}