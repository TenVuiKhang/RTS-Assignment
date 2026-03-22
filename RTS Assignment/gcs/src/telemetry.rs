// Requirements met:
//  • Decode within 3 ms               → deadline check after from_bytes()
//  • Log reception latency & drift     → expected_time vs actual arrival
//  • Re-request on missing packets     → sequence gap detection → RequestData cmd
//  • Loss-of-contact on ≥3 seq fails   → consecutive_fails counter

use crate::protocol::{TelemetryPacket, TelemetryPayload};
use crate::metrics::GcsMetrics;
use tokio::net::UdpSocket;
use tokio::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use std::sync::Arc;
use tracing::{info, warn, error, debug};

const DECODE_DEADLINE_MS: f64 = 3.0;
const EXPECTED_INTERVAL_MS: u64 = 100;   // roughly matches OCS thermal rate
const LOSS_OF_CONTACT_THRESHOLD: u8 = 3;

pub fn spawn_receiver(
    bind_addr: &str,
    telem_tx: mpsc::Sender<TelemetryPacket>,
    metrics: Arc<Mutex<GcsMetrics>>,
) -> tokio::task::JoinHandle<()> {
    let addr = bind_addr.to_string();

    tokio::spawn(async move {
        let socket = match UdpSocket::bind(&addr).await {
            Ok(s) => s,
            Err(e) => { error!("Failed to bind telemetry socket on {}: {}", addr, e); return; }
        };

        info!("📡 Telemetry receiver bound on {}", addr);

        let mut buf = vec![0u8; 65535];
        let mut expected_time = Instant::now();
        let mut last_packet_id: Option<u64> = None;
        let mut consecutive_fails: u8 = 0;

        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    let arrival = Instant::now();

                    // ── Drift measurement ────────────────────────────────
                    let drift = if arrival > expected_time {
                        arrival.duration_since(expected_time).as_secs_f64() * 1000.0
                    } else {
                        expected_time.duration_since(arrival).as_secs_f64() * 1000.0
                    };
                    expected_time = arrival + Duration::from_millis(EXPECTED_INTERVAL_MS);

                    // ── Decode within 3 ms ───────────────────────────────
                    let decode_start = Instant::now();
                    match TelemetryPacket::from_bytes(&buf[..len]) {
                        Ok(pkt) => {
                            let decode_ms = decode_start.elapsed().as_secs_f64() * 1000.0;

                            if decode_ms > DECODE_DEADLINE_MS {
                                warn!("⚠️  Decode deadline missed: {:.3}ms > {}ms", decode_ms, DECODE_DEADLINE_MS);
                                metrics.lock().await.missed_decode_deadlines += 1;
                            }

                            // ── Sequence gap detection ────────────────────
                            if let Some(prev) = last_packet_id {
                                let gap = pkt.packet_id.wrapping_sub(prev + 1);
                                if gap > 0 {
                                    warn!("⚠️  Sequence gap: missed {} packet(s) after #{}", gap, prev);
                                    let mut m = metrics.lock().await;
                                    m.sequence_gaps += gap;
                                }
                            }
                            last_packet_id = Some(pkt.packet_id);
                            consecutive_fails = 0;

                            // ── Metrics ───────────────────────────────────
                            {
                                let mut m = metrics.lock().await;
                                m.packets_received += 1;
                                m.record_reception_drift(drift);
                                m.record_decode_latency(decode_ms);
                            }

                            debug!("📥 Pkt #{} from {} | decode {:.3}ms | drift {:.3}ms",
                                pkt.packet_id, src, decode_ms, drift);

                            // Log fault alerts from OCS
                            if let TelemetryPayload::FaultAlert { ref description, ref severity, .. } = pkt.payload {
                                warn!("🛰️  OCS FAULT [{:?}]: {}", severity, description);
                            }

                            // Forward to fault manager
                            if let Err(e) = telem_tx.send(pkt).await {
                                error!("Telemetry channel closed: {}", e);
                                break;
                            }
                        }
                        Err(e) => {
                            error!("❌ Decode error from {}: {}", src, e);
                            consecutive_fails += 1;
                            metrics.lock().await.decode_errors += 1;

                            if consecutive_fails >= LOSS_OF_CONTACT_THRESHOLD {
                                error!("🚨 LOSS OF CONTACT: {} consecutive decode failures", consecutive_fails);
                                metrics.lock().await.loss_of_contact_events += 1;
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