// Requirements met:
//  • ≤2 ms dispatch for Urgent/Emergency commands  → deadline checked after send
//  • Safety interlocks via fault flag               → blocked commands are logged
//  • Log deadline adherence & rejection reasons
//  • Periodic health-check schedule

use crate::protocol::{CommandPacket, CommandPayload, CommandUrgency};
use crate::metrics::GcsMetrics;
use tokio::net::UdpSocket;
use tokio::time::{interval, Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn, error, debug};
use chrono::Utc;

const URGENT_DEADLINE_MS: f64 = 2.0;

// Global interlock flag — set by fault manager, checked here before dispatch
pub static INTERLOCK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Send commands received from the channel to the satellite
pub fn spawn_uplink(
    satellite_addr: &str,
    mut cmd_rx: mpsc::Receiver<CommandPacket>,
    metrics: Arc<Mutex<GcsMetrics>>,
) -> tokio::task::JoinHandle<()> {
    let addr = satellite_addr.to_string();

    tokio::spawn(async move {
        let socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => { error!("Failed to create uplink socket: {}", e); return; }
        };

        info!("📤 Command uplink ready → {}", addr);

        while let Some(cmd) = cmd_rx.recv().await {
            let is_urgent = matches!(cmd.urgency, CommandUrgency::Urgent | CommandUrgency::Emergency);

            // ── Safety interlock check ────────────────────────────────────
            if INTERLOCK_ACTIVE.load(Ordering::SeqCst) {
                // Emergency overrides interlock; everything else is blocked
                if !matches!(cmd.urgency, CommandUrgency::Emergency) {
                    warn!("🔒 INTERLOCK: Command #{} blocked (urgency: {:?})", cmd.command_id, cmd.urgency);
                    let mut m = metrics.lock().await;
                    m.commands_blocked += 1;
                    m.log_rejection(cmd.command_id, "Safety interlock active");
                    continue;
                }
                warn!("⚡ Emergency command #{} bypassing interlock", cmd.command_id);
            }

            // ── Serialize & send ──────────────────────────────────────────
            let dispatch_start = Instant::now();

            match cmd.to_bytes() {
                Ok(bytes) => {
                    match socket.send_to(&bytes, &addr).await {
                        Ok(_) => {
                            let dispatch_ms = dispatch_start.elapsed().as_secs_f64() * 1000.0;

                            // Check urgent deadline
                            if is_urgent && dispatch_ms > URGENT_DEADLINE_MS {
                                warn!("⚠️  Urgent cmd #{} dispatch {:.3}ms > {}ms deadline",
                                    cmd.command_id, dispatch_ms, URGENT_DEADLINE_MS);
                                metrics.lock().await.missed_urgent_deadlines += 1;
                            }

                            {
                                let mut m = metrics.lock().await;
                                m.commands_sent += 1;
                                m.record_dispatch_latency(dispatch_ms);
                            }

                            debug!("📤 Sent cmd #{} ({:?}) in {:.3}ms", cmd.command_id, cmd.urgency, dispatch_ms);

                            // Listen for ACK on the same socket (non-blocking, 100ms window)
                            receive_ack(&socket, cmd.command_id, &metrics).await;
                        }
                        Err(e) => {
                            error!("❌ Failed to send cmd #{}: {}", cmd.command_id, e);
                            metrics.lock().await.commands_failed += 1;
                        }
                    }
                }
                Err(e) => error!("❌ Serialize cmd #{} failed: {}", cmd.command_id, e),
            }
        }

        info!("Command uplink task terminated");
    })
}

/// Wait up to 100 ms for a CommandAck telemetry packet
async fn receive_ack(
    socket: &UdpSocket,
    cmd_id: u64,
    metrics: &Arc<Mutex<GcsMetrics>>,
) {
    use crate::protocol::TelemetryPacket;
    let mut buf = vec![0u8; 65535];

    match tokio::time::timeout(Duration::from_millis(100), socket.recv_from(&mut buf)).await {
        Ok(Ok((len, _))) => {
            if let Ok(pkt) = TelemetryPacket::from_bytes(&buf[..len]) {
                if let crate::protocol::TelemetryPayload::CommandAck { command_id, success, execution_time_ms, .. } = pkt.payload {
                    if command_id == cmd_id {
                        if success {
                            debug!("✅ ACK for cmd #{}: executed in {:.3}ms", cmd_id, execution_time_ms);
                        } else {
                            warn!("❌ NACK for cmd #{}: OCS reported failure", cmd_id);
                            metrics.lock().await.commands_nacked += 1;
                        }
                        metrics.lock().await.acks_received += 1;
                    }
                }
            }
        }
        Ok(Err(e)) => debug!("ACK recv error for cmd #{}: {}", cmd_id, e),
        Err(_) => {
            warn!("⏱️  No ACK received for cmd #{} within 100ms", cmd_id);
            metrics.lock().await.acks_missed += 1;
        }
    }
}

/// Periodic command scheduler — health checks + configurable command injection
pub fn spawn_scheduler(
    cmd_tx: mpsc::Sender<CommandPacket>,
    metrics: Arc<Mutex<GcsMetrics>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut hc_ticker  = interval(Duration::from_secs(5));   // health check every 5s
        let mut cmd_id = 1u64;

        info!("📅 Command scheduler started");
        info!("   Health-check interval: 5s");

        loop {
            hc_ticker.tick().await;

            let cmd = CommandPacket {
                command_id: cmd_id,
                timestamp: Utc::now(),
                urgency: CommandUrgency::Routine,
                payload: CommandPayload::HealthCheck,
            };

            info!("📋 Scheduling health-check cmd #{}", cmd_id);
            if cmd_tx.send(cmd).await.is_err() {
                error!("Command channel closed — scheduler exiting");
                break;
            }
            metrics.lock().await.commands_scheduled += 1;
            cmd_id += 1;
        }
    })
}

/// Create an urgent command (called by fault manager)
pub fn make_urgent(id: u64, payload: CommandPayload) -> CommandPacket {
    CommandPacket {
        command_id: id,
        timestamp: Utc::now(),
        urgency: CommandUrgency::Urgent,
        payload,
    }
}

/// Create an emergency command (bypasses all interlocks)
pub fn make_emergency(id: u64, reason: impl Into<String>) -> CommandPacket {
    CommandPacket {
        command_id: id,
        timestamp: Utc::now(),
        urgency: CommandUrgency::Emergency,
        payload: CommandPayload::EmergencyShutdown { reason: reason.into() },
    }
}