// src/communication.rs

use crate::protocol::{TelemetryPacket, CommandPacket, CommandPayload, CommandUrgency};
use crate::metrics::GcsMetrics;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{Duration, Instant};
use std::sync::Arc;
use chrono::Utc;
use tracing::{info, warn, error, debug};

const DECODE_DEADLINE_MS:        f64 = 3.0;
const EXPECTED_INTERVAL_MS:      u64 = 100;
const LOSS_OF_CONTACT_THRESHOLD: u8  = 3;

pub fn spawn_receiver(
    bind_addr:      &str,
    satellite_addr: &str,
    telem_tx:       mpsc::Sender<TelemetryPacket>,
    metrics:        Arc<Mutex<GcsMetrics>>,
) -> tokio::task::JoinHandle<()> {
    let bind = bind_addr.to_string();
    let sat  = satellite_addr.to_string();

    tokio::spawn(async move {
        let socket = match UdpSocket::bind(&bind).await {
            Ok(s)  => s,
            Err(e) => { error!("Failed to bind on {}: {}", bind, e); return; }
        };

        info!("📡 Telemetry receiver on {}", bind);

        let mut buf                  = vec![0u8; 65535];
        let mut expected_arrival     = Instant::now() + Duration::from_millis(EXPECTED_INTERVAL_MS);
        let mut last_packet_id: Option<u64> = None;
        let mut consecutive_failures: u8    = 0;
        let mut rerequest_id:         u64   = 80_000;

        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    let arrived_at = Instant::now();

                    // ── Reception drift ───────────────────────────────────
                    let drift_ms = if arrived_at > expected_arrival {
                        arrived_at.duration_since(expected_arrival).as_secs_f64() * 1000.0
                    } else { 0.0 };
                    expected_arrival = arrived_at + Duration::from_millis(EXPECTED_INTERVAL_MS);
                    if drift_ms > 5.0 {
                        warn!("⚠️  Reception drift: {:.3}ms", drift_ms);
                    }

                    // ── Decode within 3ms ─────────────────────────────────
                    let decode_start = Instant::now();
                    match TelemetryPacket::from_bytes(&buf[..len]) {
                        Ok(pkt) => {
                            let decode_ms    = decode_start.elapsed().as_secs_f64() * 1000.0;
                            let latency_ms   = drift_ms + decode_ms;

                            if decode_ms > DECODE_DEADLINE_MS {
                                warn!("⚠️  Decode {:.3}ms > {}ms deadline", decode_ms, DECODE_DEADLINE_MS);
                                metrics.lock().await.missed_decode_deadlines += 1;
                            }

                            // ── Sequence gap → re-request ─────────────────
                            if let Some(prev) = last_packet_id {
                                if pkt.packet_id > prev + 1 {
                                    let gap = pkt.packet_id - (prev + 1);
                                    warn!("⚠️  Gap: {} packet(s) missing after #{}", gap, prev);
                                    send_rerequest(&socket, &sat, prev + 1, pkt.packet_id - 1,
                                        &mut rerequest_id, &metrics).await;
                                    metrics.lock().await.sequence_gaps += gap;
                                }
                            }
                            last_packet_id       = Some(pkt.packet_id);
                            consecutive_failures = 0;

                            {
                                let mut m = metrics.lock().await;
                                m.packets_received += 1;
                                m.record_reception_drift(drift_ms);
                                m.record_decode_latency(decode_ms);
                                m.record_reception_latency(latency_ms);
                            }

                            debug!("📥 Pkt #{} from {} | decode {:.3}ms | drift {:.3}ms",
                                pkt.packet_id, src, decode_ms, drift_ms);

                            if telem_tx.send(pkt).await.is_err() {
                                error!("Telemetry channel closed");
                                break;
                            }
                        }
                        Err(e) => {
                            error!("❌ Decode error from {}: {}", src, e);
                            consecutive_failures += 1;
                            metrics.lock().await.decode_errors += 1;

                            if consecutive_failures >= LOSS_OF_CONTACT_THRESHOLD {
                                error!("🚨 LOSS OF CONTACT: {} consecutive failures", consecutive_failures);
                                metrics.lock().await.loss_of_contact_events += 1;
                                consecutive_failures = 0;
                            }
                        }
                    }
                }
                Err(e) => error!("❌ Socket error: {}", e),
            }
        }
    })
}

async fn send_rerequest(
    socket:        &UdpSocket,
    satellite_addr: &str,
    from_id:       u64,
    to_id:         u64,
    id:            &mut u64,
    metrics:       &Arc<Mutex<GcsMetrics>>,
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
            info!("📤 Re-request #{}-{} (cmd #{})", from_id, to_id, id);
            metrics.lock().await.rerequests_sent += 1;
            *id += 1;
        }
    }
}