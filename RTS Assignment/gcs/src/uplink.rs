// src/uplink.rs

use crate::protocol::{TelemetryPacket, TelemetryPayload, CommandUrgency};
use crate::scheduler::{ScheduledCommand, measure_uplink_jitter};
use crate::interlock::{self, InterlockState, InterlockDecision};
use crate::metrics::GcsMetrics;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{Duration, Instant};
use std::sync::Arc;
use tracing::{info, warn, error, debug};

const URGENT_DEADLINE_MS: f64 = 2.0;
const ACK_TIMEOUT_MS:     u64 = 100;

pub fn spawn_uplink(
    satellite_addr:  &str,
    mut cmd_rx:      mpsc::Receiver<ScheduledCommand>,
    interlock_state: Arc<Mutex<InterlockState>>,
    metrics:         Arc<Mutex<GcsMetrics>>,
) -> tokio::task::JoinHandle<()> {
    // cmd_rx is moved into the task — we read is_empty() on it directly
    let addr = satellite_addr.to_string();

    tokio::spawn(async move {
        let socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s)  => s,
            Err(e) => { error!("[ERROR]    Failed to create uplink socket: {}", e); return; }
        };

        info!("[INFO]  Command uplink ready → {}", addr);

        while let Some(scheduled) = cmd_rx.recv().await {
            let cmd       = &scheduled.packet;
            let is_urgent = matches!(cmd.urgency, CommandUrgency::Urgent | CommandUrgency::Emergency);

            // ── Interlock gate ────────────────────────────────────────────
            match interlock::check(cmd, &interlock_state, &metrics).await {
                InterlockDecision::Blocked { .. } => continue,
                InterlockDecision::Approved       => {}
            }

            // ── Uplink jitter (scheduler enqueue → now) ───────────────────
            let jitter_ms = measure_uplink_jitter(&scheduled);
            if jitter_ms > 5.0 {
                warn!("[WARN]   Uplink jitter {:.3}ms for cmd #{}", jitter_ms, cmd.command_id);
            }
            metrics.lock().await.record_uplink_jitter(jitter_ms);

            // ── Serialize and send ────────────────────────────────────────
            let send_start = Instant::now();
            match cmd.to_bytes() {
                Ok(bytes) => {
                    match socket.send_to(&bytes, &addr).await {
                        Ok(n) => {
                            let dispatch_ms = send_start.elapsed().as_secs_f64() * 1000.0;

                            if is_urgent && dispatch_ms > URGENT_DEADLINE_MS {
                                warn!("[WARN]   Urgent cmd #{} dispatch {:.3}ms > {}ms",
                                    cmd.command_id, dispatch_ms, URGENT_DEADLINE_MS);
                                metrics.lock().await.missed_urgent_deadlines += 1;
                            }

                            {
                                let mut m = metrics.lock().await;
                                m.commands_sent += 1;
                                m.record_dispatch_latency(dispatch_ms);
                            }

                            debug!("[INFO]  Sent cmd #{} ({:?}) {} bytes | dispatch {:.3}ms | jitter {:.3}ms",
                                cmd.command_id, cmd.urgency, n, dispatch_ms, jitter_ms);

                            // Only wait for ACK if the channel is empty —
                            // if commands are queued up, skip ACK to drain backpressure
                            if cmd_rx.is_empty() {
                                await_ack(&socket, cmd.command_id, &metrics).await;
                            } else {
                                debug!("[INFO]  Skipping ACK wait for cmd #{} — channel has pending commands",
                                    cmd.command_id);
                                metrics.lock().await.acks_missed += 1;
                            }
                        }
                        Err(e) => {
                            error!("[ERROR]    Send cmd #{} failed: {}", cmd.command_id, e);
                            metrics.lock().await.commands_failed += 1;
                        }
                    }
                }
                Err(e) => error!("[ERROR]    Serialize cmd #{} failed: {}", cmd.command_id, e),
            }
        }

        info!("[INFO]  Uplink task terminated");
    })
}

async fn await_ack(socket: &UdpSocket, cmd_id: u64, metrics: &Arc<Mutex<GcsMetrics>>) {
    let mut buf = vec![0u8; 65535];
    match tokio::time::timeout(Duration::from_millis(ACK_TIMEOUT_MS), socket.recv_from(&mut buf)).await {
        Ok(Ok((len, _))) => {
            if let Ok(pkt) = TelemetryPacket::from_bytes(&buf[..len]) {
                if let TelemetryPayload::CommandAck { command_id, success, execution_time_ms, message } = pkt.payload {
                    if command_id == cmd_id {
                        if success {
                            info!("[INFO]  ACKNOWLEDGE cmd #{}: {:.3}ms — {}", cmd_id, execution_time_ms, message);
                        } else {
                            warn!("[WARN]   NOT ACKNOWLEDGE cmd #{}: {}", cmd_id, message);
                            metrics.lock().await.commands_nacked += 1;
                        }
                        metrics.lock().await.acks_received += 1;
                    }
                }
            }
        }
        Ok(Err(e)) => debug!("[DEBUG]  ACKNOWLEDGE recv error cmd #{}: {}", cmd_id, e),
        Err(_)     => {
            warn!("[WARN]   No ACKNOWLEDGE for cmd #{} within {}ms", cmd_id, ACK_TIMEOUT_MS);
            metrics.lock().await.acks_missed += 1;
        }
    }
}