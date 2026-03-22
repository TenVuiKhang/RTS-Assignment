// src/communication.rs
// GCS Telemetry Receiver  (Assignment §GCS-1)
//
// Responsibilities:
//   • Bind UDP socket on :8002 — the address OCS sends telemetry TO
//   • Decode every incoming TelemetryPacket within 3ms
//   • Measure reception latency (socket recv → decoded)
//   • Measure reception drift  (actual arrival vs expected arrival)
//   • Detect sequence gaps → send RequestData back to OCS
//   • Declare loss of contact after 3 consecutive failures
//   • Forward decoded packets to command_brain via channel

use crate::protocol::{
    TelemetryPacket, CommandPacket, CommandPayload, CommandUrgency,
};
use crate::metrics::GcsMetrics;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{Duration, Instant};
use std::sync::Arc;
use chrono::Utc;
use tracing::{info, warn, error, debug};

const DECODE_DEADLINE_MS:        f64 = 3.0;
const EXPECTED_INTERVAL_MS:      u64 = 100;   // OCS thermal fires every 100ms — fastest stream
const LOSS_OF_CONTACT_THRESHOLD: u8  = 3;     // consecutive failures before alert

pub fn spawn_receiver(
    bind_addr: &str,
    satellite_addr: &str,              // needed to send re-request commands back
    telem_tx: mpsc::Sender<TelemetryPacket>,
    metrics: Arc<Mutex<GcsMetrics>>,
) -> tokio::task::JoinHandle<()> {
    let bind    = bind_addr.to_string();
    let sat     = satellite_addr.to_string();

    tokio::spawn(async move {
        // ── Bind the receiving socket ─────────────────────────────────────
        let socket = match UdpSocket::bind(&bind).await {
            Ok(s)  => s,
            Err(e) => { error!("Failed to bind telemetry socket on {}: {}", bind, e); return; }
        };

        info!("📡 Telemetry receiver bound on {}", bind);
        info!("   Decode deadline:  {}ms", DECODE_DEADLINE_MS);
        info!("   Expected interval: {}ms", EXPECTED_INTERVAL_MS);
        info!("   Loss-of-contact threshold: {} consecutive failures", LOSS_OF_CONTACT_THRESHOLD);

        let mut buf = vec![0u8; 65535];

        // ── State for drift, sequence tracking, and loss-of-contact ──────
        let mut expected_arrival   = Instant::now() + Duration::from_millis(EXPECTED_INTERVAL_MS);
        let mut last_packet_id: Option<u64> = None;
        let mut consecutive_failures: u8 = 0;
        let mut re_request_id: u64 = 90_000; // re-request commands start here to avoid ID clash

        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    let arrived_at = Instant::now();

                    // ── Reception drift ───────────────────────────────────
                    // How late (or early) did this packet arrive vs expected?
                    let drift_ms = if arrived_at > expected_arrival {
                        arrived_at.duration_since(expected_arrival).as_secs_f64() * 1000.0
                    } else {
                        // arrived early — negative drift, record as 0 (not a problem)
                        0.0
                    };
                    // Slide the window forward regardless of whether packet was valid
                    expected_arrival = arrived_at + Duration::from_millis(EXPECTED_INTERVAL_MS);

                    if drift_ms > 5.0 {
                        warn!("⚠️  Reception drift: {:.3}ms (expected every {}ms)", drift_ms, EXPECTED_INTERVAL_MS);
                    }

                    // ── Decode within 3ms ─────────────────────────────────
                    let decode_start = Instant::now();

                    match TelemetryPacket::from_bytes(&buf[..len]) {
                        Ok(pkt) => {
                            let decode_ms = decode_start.elapsed().as_secs_f64() * 1000.0;
                            // Reception latency = drift + decode time
                            let reception_latency_ms = drift_ms + decode_ms;

                            if decode_ms > DECODE_DEADLINE_MS {
                                warn!(
                                    "⚠️  Decode deadline missed: {:.3}ms > {}ms",
                                    decode_ms, DECODE_DEADLINE_MS
                                );
                                metrics.lock().await.missed_decode_deadlines += 1;
                            }

                            // ── Sequence gap detection ────────────────────
                            // OCS increments packet_id by 1 each time.
                            // A gap means one or more packets were lost in transit.
                            if let Some(prev_id) = last_packet_id {
                                let expected_id = prev_id + 1;
                                if pkt.packet_id > expected_id {
                                    let gap = pkt.packet_id - expected_id;
                                    warn!(
                                        "⚠️  Sequence gap: expected #{}, got #{} ({} packet(s) missing)",
                                        expected_id, pkt.packet_id, gap
                                    );

                                    // Send re-request for the missing range
                                    send_rerequest(
                                        &socket, &sat,
                                        prev_id + 1, pkt.packet_id - 1,
                                        &mut re_request_id,
                                        &metrics,
                                    ).await;

                                    metrics.lock().await.sequence_gaps += gap;
                                }
                            }
                            last_packet_id = Some(pkt.packet_id);

                            // ── Reset consecutive failure counter ─────────
                            consecutive_failures = 0;

                            // ── Update metrics ────────────────────────────
                            {
                                let mut m = metrics.lock().await;
                                m.packets_received += 1;
                                m.record_reception_drift(drift_ms);
                                m.record_decode_latency(decode_ms);
                                m.record_reception_latency(reception_latency_ms);
                            }

                            debug!(
                                "📥 Pkt #{} from {} | decode {:.3}ms | drift {:.3}ms | latency {:.3}ms",
                                pkt.packet_id, src, decode_ms, drift_ms, reception_latency_ms
                            );

                            // ── Forward to command_brain ──────────────────
                            if telem_tx.send(pkt).await.is_err() {
                                error!("Telemetry channel closed — receiver shutting down");
                                break;
                            }
                        }

                        Err(e) => {
                            // ── Decode failure ────────────────────────────
                            error!("❌ Decode error from {}: {}", src, e);
                            consecutive_failures += 1;
                            metrics.lock().await.decode_errors += 1;

                            // ── Loss of contact detection ─────────────────
                            if consecutive_failures >= LOSS_OF_CONTACT_THRESHOLD {
                                error!(
                                    "🚨 LOSS OF CONTACT: {} consecutive decode failures — \
                                     satellite may be unreachable",
                                    consecutive_failures
                                );
                                metrics.lock().await.loss_of_contact_events += 1;
                                // Reset so we don't fire this every single subsequent failure
                                consecutive_failures = 0;
                            }
                        }
                    }
                }

                Err(e) => {
                    error!("❌ Socket recv error: {}", e);
                }
            }
        }

        info!("Telemetry receiver task terminated");
    })
}

// ─── Re-request missing packets ───────────────────────────────────────────────
// Sends a RequestData command back to the OCS asking it to retransmit
// the packet range [from_id, to_id].
async fn send_rerequest(
    socket:       &UdpSocket,
    satellite_addr: &str,
    from_id:      u64,
    to_id:        u64,
    re_request_id: &mut u64,
    metrics:      &Arc<Mutex<GcsMetrics>>,
) {
    let cmd = CommandPacket {
        command_id: *re_request_id,
        timestamp:  Utc::now(),
        urgency:    CommandUrgency::Urgent,
        payload:    CommandPayload::RequestData {
            data_type: format!("retransmit:{}:{}", from_id, to_id),
            time_range_seconds: None,
        },
    };

    match cmd.to_bytes() {
        Ok(bytes) => {
            match socket.send_to(&bytes, satellite_addr).await {
                Ok(_) => {
                    info!(
                        "📤 Re-request sent for packets #{} to #{} (cmd #{})",
                        from_id, to_id, re_request_id
                    );
                    metrics.lock().await.rerequests_sent += 1;
                    *re_request_id += 1;
                }
                Err(e) => error!("Failed to send re-request: {}", e),
            }
        }
        Err(e) => error!("Failed to serialize re-request: {}", e),
    }
}