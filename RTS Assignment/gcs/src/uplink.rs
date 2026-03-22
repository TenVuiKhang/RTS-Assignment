// src/uplink.rs
// GCS Command Uplink Sender  (Assignment §GCS-2)
//
// Responsibilities:
//   • Hold the outbound UDP socket (sends TO OCS :8001)
//   • Receive ScheduledCommand from scheduler.rs via channel
//   • Call interlock::check() — block if unsafe
//   • Serialize and send the CommandPacket
//   • Measure uplink jitter (scheduler enqueue time → actual send time)
//   • Enforce ≤2ms dispatch deadline for Urgent/Emergency commands
//   • Wait for CommandAck from OCS and log result

use crate::protocol::{TelemetryPacket, TelemetryPayload, CommandUrgency};
use crate::scheduler::{ScheduledCommand, measure_uplink_jitter};
use crate::interlock::{self, InterlockState, InterlockDecision};
use crate::metrics::GcsMetrics;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{Duration, Instant};
use std::sync::Arc;
use tracing::{info, warn, error, debug};

const URGENT_DISPATCH_DEADLINE_MS: f64 = 2.0;
const ACK_TIMEOUT_MS: u64 = 100;

pub fn spawn_uplink(
    satellite_addr: &str,
    mut cmd_rx: mpsc::Receiver<ScheduledCommand>,
    interlock_state: Arc<Mutex<InterlockState>>,
    metrics: Arc<Mutex<GcsMetrics>>,
) -> tokio::task::JoinHandle<()> {
    let addr = satellite_addr.to_string();

    tokio::spawn(async move {
        // Outbound socket — ephemeral port, sends to OCS :8001
        let socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s)  => s,
            Err(e) => { error!("Failed to create uplink socket: {}", e); return; }
        };

        info!("📤 Command uplink ready → {}", addr);
        info!("   Urgent dispatch deadline: {}ms", URGENT_DISPATCH_DEADLINE_MS);
        info!("   ACK timeout: {}ms", ACK_TIMEOUT_MS);

        while let Some(scheduled) = cmd_rx.recv().await {
            let cmd = &scheduled.packet;
            let is_urgent = matches!(cmd.urgency, CommandUrgency::Urgent | CommandUrgency::Emergency);

            // ── Interlock gate ────────────────────────────────────────────
            // Every command passes through interlock::check() before the wire.
            // Emergency commands always pass. Everything else is blocked if a
            // fault is active.
            match interlock::check(cmd, &interlock_state, &metrics).await {
                InterlockDecision::Blocked { reason } => {
                    // Already logged by interlock::check() — nothing more to do here
                    debug!("Uplink dropped cmd #{}: {}", cmd.command_id, reason);
                    continue;
                }
                InterlockDecision::Approved => {}
            }

            // ── Uplink jitter ─────────────────────────────────────────────
            // Time from when the scheduler enqueued this command to right now.
            // High jitter means the channel was backed up or the runtime was busy.
            let jitter_ms = measure_uplink_jitter(&scheduled);
            if jitter_ms > 5.0 {
                warn!("⚠️  Uplink jitter: {:.3}ms for cmd #{}", jitter_ms, cmd.command_id);
            }
            metrics.lock().await.record_uplink_jitter(jitter_ms);

            // ── Serialize and send ────────────────────────────────────────
            let send_start = Instant::now();

            match cmd.to_bytes() {
                Ok(bytes) => {
                    match socket.send_to(&bytes, &addr).await {
                        Ok(sent) => {
                            let dispatch_ms = send_start.elapsed().as_secs_f64() * 1000.0;

                            // ── Dispatch deadline check ───────────────────
                            if is_urgent && dispatch_ms > URGENT_DISPATCH_DEADLINE_MS {
                                warn!(
                                    "⚠️  Urgent cmd #{} dispatch {:.3}ms > {}ms deadline",
                                    cmd.command_id, dispatch_ms, URGENT_DISPATCH_DEADLINE_MS
                                );
                                metrics.lock().await.missed_urgent_deadlines += 1;
                            }

                            {
                                let mut m = metrics.lock().await;
                                m.commands_sent += 1;
                                m.record_dispatch_latency(dispatch_ms);
                            }

                            debug!(
                                "📤 Sent cmd #{} ({:?}) | {} bytes | dispatch {:.3}ms | jitter {:.3}ms",
                                cmd.command_id, cmd.urgency, sent, dispatch_ms, jitter_ms
                            );

                            // ── Wait for ACK ──────────────────────────────
                            await_ack(&socket, cmd.command_id, &metrics).await;
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

// ─── Wait for CommandAck ──────────────────────────────────────────────────────
// The OCS sends a TelemetryPayload::CommandAck back on the same socket
// after processing a command. We wait up to ACK_TIMEOUT_MS for it.
async fn await_ack(
    socket:     &UdpSocket,
    cmd_id:     u64,
    metrics:    &Arc<Mutex<GcsMetrics>>,
) {
    let mut buf = vec![0u8; 65535];

    match tokio::time::timeout(
        Duration::from_millis(ACK_TIMEOUT_MS),
        socket.recv_from(&mut buf),
    ).await {
        Ok(Ok((len, _src))) => {
            match TelemetryPacket::from_bytes(&buf[..len]) {
                Ok(pkt) => {
                    if let TelemetryPayload::CommandAck { command_id, success, execution_time_ms, message } = pkt.payload {
                        if command_id == cmd_id {
                            if success {
                                info!(
                                    "✅ ACK cmd #{}: OCS executed in {:.3}ms — {}",
                                    cmd_id, execution_time_ms, message
                                );
                            } else {
                                warn!("❌ NACK cmd #{}: {}", cmd_id, message);
                                metrics.lock().await.commands_nacked += 1;
                            }
                            metrics.lock().await.acks_received += 1;
                        }
                    }
                }
                Err(e) => debug!("Non-ACK packet received while waiting for cmd #{}: {}", cmd_id, e),
            }
        }
        Ok(Err(e)) => debug!("ACK recv error for cmd #{}: {}", cmd_id, e),
        Err(_) => {
            warn!("⏱️  No ACK for cmd #{} within {}ms", cmd_id, ACK_TIMEOUT_MS);
            metrics.lock().await.acks_missed += 1;
        }
    }
}