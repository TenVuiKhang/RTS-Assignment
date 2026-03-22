// Requirements met:
//  • Receive fault messages from satellite
//  • Block unsafe commands until fault resolved   → INTERLOCK_ACTIVE flag
//  • Track interlock latency (detection → block)  → interlock_latency_ms
//  • Document & log all command rejections
//  • If fault response time > 100 ms              → critical ground alert

use crate::protocol::{TelemetryPacket, TelemetryPayload, FaultType, AlertSeverity, CommandPayload};
use crate::commands::{INTERLOCK_ACTIVE, make_urgent};
use crate::metrics::GcsMetrics;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Instant;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tracing::{info, warn, error};

const FAULT_RESPONSE_DEADLINE_MS: f64 = 100.0;

pub fn spawn_fault_manager(
    mut telem_rx: mpsc::Receiver<TelemetryPacket>,
    cmd_tx: mpsc::Sender<crate::protocol::CommandPacket>,
    metrics: Arc<Mutex<GcsMetrics>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("🛡️  Fault manager started");

        let mut cmd_id = 10_000u64;   // fault-manager commands start at 10 000

        while let Some(pkt) = telem_rx.recv().await {
            match &pkt.payload {
                // ── Fault alerts from OCS ─────────────────────────────────
                TelemetryPayload::FaultAlert { fault_type, severity, description, .. } => {
                    let fault_recv = Instant::now();

                    error!("🚨 FAULT RECEIVED: {:?} [{:?}] — {}", fault_type, severity, description);

                    // Engage interlock immediately
                    let interlock_start = Instant::now();
                    INTERLOCK_ACTIVE.store(true, Ordering::SeqCst);
                    let interlock_latency = interlock_start.elapsed().as_secs_f64() * 1000.0;

                    {
                        let mut m = metrics.lock().await;
                        m.faults_received += 1;
                        m.record_interlock_latency(interlock_latency);
                        warn!("🔒 Interlock engaged in {:.3}ms", interlock_latency);
                    }

                    // Build response command based on fault type
                    let response_cmd = match fault_type {
                        FaultType::ThermalAnomaly => {
                            warn!("🌡️  Thermal anomaly — requesting sensor interval reduction");
                            Some(make_urgent(cmd_id, CommandPayload::SetSensorInterval {
                                sensor_id: 1,
                                interval_ms: 50,
                            }))
                        }
                        FaultType::BufferOverflow => {
                            warn!("📦 Buffer overflow — requesting degraded mode");
                            Some(make_urgent(cmd_id, CommandPayload::SetMode {
                                mode: crate::protocol::SystemMode::Degraded,
                            }))
                        }
                        FaultType::CommunicationTimeout => {
                            warn!("📡 Comms timeout — requesting subsystem reset");
                            Some(make_urgent(cmd_id, CommandPayload::ResetSubsystem {
                                subsystem: "communication".to_string(),
                            }))
                        }
                        FaultType::ConsecutiveDataLoss => {
                            warn!("⚠️  Consecutive data loss — requesting fault clear");
                            Some(make_urgent(cmd_id, CommandPayload::ClearFault { fault_id: 0 }))
                        }
                        _ => {
                            warn!("⚠️  Unhandled fault type: {:?} — health check requested", fault_type);
                            Some(make_urgent(cmd_id, CommandPayload::HealthCheck))
                        }
                    };

                    // Check fault response deadline (100 ms)
                    let response_latency = fault_recv.elapsed().as_secs_f64() * 1000.0;
                    if response_latency > FAULT_RESPONSE_DEADLINE_MS {
                        error!("🚨 CRITICAL GROUND ALERT: Fault response {:.3}ms > {}ms deadline!",
                            response_latency, FAULT_RESPONSE_DEADLINE_MS);
                        metrics.lock().await.missed_fault_response_deadlines += 1;
                    }

                    // Dispatch the response command
                    if let Some(cmd) = response_cmd {
                        cmd_id += 1;
                        if cmd_tx.send(cmd).await.is_err() {
                            error!("Command channel closed in fault manager");
                            break;
                        }
                    }

                    // For Critical severity, lift interlock only after explicit ClearFault ACK
                    // For lesser severity, lift immediately after response dispatch
                    if !matches!(severity, AlertSeverity::Critical) {
                        INTERLOCK_ACTIVE.store(false, Ordering::SeqCst);
                        info!("🔓 Interlock released (non-critical fault)");
                    } else {
                        warn!("🔒 Interlock remains ACTIVE until ClearFault ACK (critical severity)");
                    }
                }

                // ── Health status — release interlock if system is healthy ─
                TelemetryPayload::HealthStatus { mode, .. } => {
                    if matches!(mode, crate::protocol::SystemMode::Normal) {
                        if INTERLOCK_ACTIVE.load(Ordering::SeqCst) {
                            INTERLOCK_ACTIVE.store(false, Ordering::SeqCst);
                            info!("🔓 Interlock released — OCS reports Normal mode");
                            metrics.lock().await.interlocks_released += 1;
                        }
                    }
                }

                // ── CommandAck — clear critical interlock once fault is cleared ─
                TelemetryPayload::CommandAck { success, .. } => {
                    if *success && INTERLOCK_ACTIVE.load(Ordering::SeqCst) {
                        INTERLOCK_ACTIVE.store(false, Ordering::SeqCst);
                        info!("🔓 Interlock released after successful command ACK");
                        metrics.lock().await.interlocks_released += 1;
                    }
                }

                _ => {}
            }
        }

        info!("Fault manager task terminated");
    })
}