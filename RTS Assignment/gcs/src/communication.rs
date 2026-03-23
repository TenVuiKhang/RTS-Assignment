// src/communication.rs

use crate::protocol::{TelemetryPacket, CommandPacket, CommandPayload, CommandUrgency};
use crate::metrics::GcsMetrics;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex, oneshot};
use tokio::time::{Duration, Instant, sleep, timeout};
use std::sync::Arc;
use chrono::Utc;
use tracing::{info, warn, error, debug};

const DECODE_DEADLINE_MS:        f64 = 3.0;
const EXPECTED_INTERVAL_MS:      u64 = 100;

// How long to wait for ANY packet before considering it a missed heartbeat
const RECV_TIMEOUT_MS:           u64 = 500;

// How many consecutive timeouts before declaring loss of contact
const LOSS_OF_CONTACT_THRESHOLD: u8  = 3;

const RECONNECT_ATTEMPTS:        u8  = 5;
const RECONNECT_WAIT_MS:         u64 = 2000;

pub fn spawn_receiver(
    bind_addr:      &str,
    satellite_addr: &str,
    telem_tx:       mpsc::Sender<TelemetryPacket>,
    metrics:        Arc<Mutex<GcsMetrics>>,
    shutdown_tx:    oneshot::Sender<&'static str>,
) -> tokio::task::JoinHandle<()> {
    let bind = bind_addr.to_string();
    let sat  = satellite_addr.to_string();

    tokio::spawn(async move {
        let socket = match UdpSocket::bind(&bind).await {
            Ok(s)  => s,
            Err(e) => { error!("Failed to bind on {}: {}", bind, e); return; }
        };

        info!("[INFO]  Telemetry receiver on {}", bind);
        info!("[INFO]   Recv timeout:             {}ms (silence detection)", RECV_TIMEOUT_MS);
        info!("[INFO]   Loss-of-contact after:    {} consecutive timeouts", LOSS_OF_CONTACT_THRESHOLD);
        info!("[INFO]   Max reconnect attempts:   {}", RECONNECT_ATTEMPTS);

        let mut buf                   = vec![0u8; 65535];
        let mut expected_arrival      = Instant::now() + Duration::from_millis(EXPECTED_INTERVAL_MS);
        let mut last_packet_id: Option<u64> = None;
        let mut consecutive_misses:   u8    = 0;   // timeouts OR decode errors
        let mut reconnect_attempts:   u8    = 0;
        let mut rerequest_id:         u64   = 80_000;
        let mut shutdown_tx           = Some(shutdown_tx);

        'recv: loop {
            // ── Wrap recv_from in a timeout so silence is detectable ──────
            match timeout(Duration::from_millis(RECV_TIMEOUT_MS), socket.recv_from(&mut buf)).await {

                // ── Got data ─────────────────────────────────────────────
                Ok(Ok((len, src))) => {
                    consecutive_misses = 0;
                    reconnect_attempts = 0;
                    let arrived_at = Instant::now();

                    // Reception drift
                    let drift_ms = if arrived_at > expected_arrival {
                        arrived_at.duration_since(expected_arrival).as_secs_f64() * 1000.0
                    } else { 0.0 };
                    expected_arrival = arrived_at + Duration::from_millis(EXPECTED_INTERVAL_MS);
                    if drift_ms > 5.0 {
                        warn!("[WARN]   Reception drift: {:.3}ms", drift_ms);
                    }

                    // Decode within 3ms
                    let decode_start = Instant::now();
                    match TelemetryPacket::from_bytes(&buf[..len]) {
                        Ok(pkt) => {
                            let decode_ms  = decode_start.elapsed().as_secs_f64() * 1000.0;
                            let latency_ms = drift_ms + decode_ms;

                            if decode_ms > DECODE_DEADLINE_MS {
                                warn!("[WARN]   Decode {:.3}ms > {}ms deadline", decode_ms, DECODE_DEADLINE_MS);
                                metrics.lock().await.missed_decode_deadlines += 1;
                            }

                            // Sequence gap → re-request
                            if let Some(prev) = last_packet_id {
                                if pkt.packet_id > prev + 1 {
                                    let gap = pkt.packet_id - (prev + 1);
                                    warn!("[WARN]   Gap: {} packet(s) missing after #{}", gap, prev);
                                    send_rerequest(&socket, &sat, prev + 1, pkt.packet_id - 1,
                                        &mut rerequest_id, &metrics).await;
                                    metrics.lock().await.sequence_gaps += gap;
                                }
                            }
                            last_packet_id = Some(pkt.packet_id);

                            {
                                let mut m = metrics.lock().await;
                                m.packets_received += 1;
                                m.record_reception_drift(drift_ms);
                                m.record_decode_latency(decode_ms);
                                m.record_reception_latency(latency_ms);
                            }

                            debug!("[DEBUG]  Packet #{} from {} | decode {:.3}ms | drift {:.3}ms",
                                pkt.packet_id, src, decode_ms, drift_ms);

                            if telem_tx.send(pkt).await.is_err() {
                                error!("[ERROR]    Telemetry channel closed");
                                break 'recv;
                            }
                        }
                        Err(e) => {
                            error!("[ERROR]    Decode error from {}: {}", src, e);
                            metrics.lock().await.decode_errors += 1;
                            consecutive_misses += 1;
                        }
                    }
                }

                // ── Timeout — OCS sent nothing within RECV_TIMEOUT_MS ────
                Err(_) => {
                    consecutive_misses += 1;
                    warn!("[WARN]   No telemetry for {}ms (miss {}/{})",
                        RECV_TIMEOUT_MS, consecutive_misses, LOSS_OF_CONTACT_THRESHOLD);
                }

                // ── Socket error ─────────────────────────────────────────
                Ok(Err(e)) => {
                    error!("[ERROR]    Socket error: {}", e);
                    consecutive_misses += 1;
                }
            }

            // ── Loss of contact → reconnect loop ─────────────────────────
            if consecutive_misses >= LOSS_OF_CONTACT_THRESHOLD {
                error!("[ERROR]    LOSS OF CONTACT after {} missed heartbeats", consecutive_misses);
                metrics.lock().await.loss_of_contact_events += 1;
                consecutive_misses = 0;

                loop {
                    reconnect_attempts += 1;
                    warn!("[WARN]   Reconnect attempt {}/{} — waiting {}ms...",
                        reconnect_attempts, RECONNECT_ATTEMPTS, RECONNECT_WAIT_MS);
                    sleep(Duration::from_millis(RECONNECT_WAIT_MS)).await;

                    // Probe: send a health-check and wait up to 1s for any reply
                    let probe = CommandPacket {
                        command_id: 99_999,
                        timestamp:  Utc::now(),
                        urgency:    CommandUrgency::Urgent,
                        payload:    CommandPayload::HealthCheck,
                    };
                    if let Ok(bytes) = probe.to_bytes() {
                        let _ = socket.send_to(&bytes, &sat).await;
                    }

                    match timeout(Duration::from_secs(1), socket.recv_from(&mut buf)).await {
                        Ok(Ok((len, _))) if TelemetryPacket::from_bytes(&buf[..len]).is_ok() => {
                            info!("[INFO]  Reconnected to OCS after {} attempt(s)", reconnect_attempts);
                            reconnect_attempts = 0;
                            consecutive_misses = 0;
                            expected_arrival   = Instant::now()
                                + Duration::from_millis(EXPECTED_INTERVAL_MS);
                            break; // back to normal recv loop
                        }
                        _ => {} // no response — try again
                    }

                    if reconnect_attempts >= RECONNECT_ATTEMPTS {
                        error!("[ERROR]    GCS SHUTDOWN: OCS unreachable after {} attempts",
                            RECONNECT_ATTEMPTS);
                        if let Some(tx) = shutdown_tx.take() {
                            let _ = tx.send("OCS unreachable — max reconnects exceeded");
                        }
                        break 'recv;
                    }
                }
            }
        }

        info!("[INFO]  Telemetry receiver task terminated");
    })
}

async fn send_rerequest(
    socket:         &UdpSocket,
    satellite_addr: &str,
    from_id:        u64,
    to_id:          u64,
    id:             &mut u64,
    metrics:        &Arc<Mutex<GcsMetrics>>,
) {
    let cmd = CommandPacket {
        command_id: *id,
        timestamp:  Utc::now(),
        urgency:    CommandUrgency::Urgent,
        payload:    CommandPayload::RequestData {
            data_type: format!("retransmit:{}:{}", from_id, to_id),
            time_range_seconds: None,
        },
    };
    if let Ok(bytes) = cmd.to_bytes() {
        if socket.send_to(&bytes, satellite_addr).await.is_ok() {
            info!("[INFO]  Re-request #{}-{} (cmd #{})", from_id, to_id, id);
            metrics.lock().await.rerequests_sent += 1;
            *id += 1;
        }
    }
}