// src/communication.rs
// UDP communication with Ground Control (Lab 8)

use crate::protocol::{TelemetryPacket, CommandPacket, CommandUrgency};
use crate::sensors::{SensorReading, reading_to_telemetry};
use crate::metrics::Metrics;
use crate::config::Config;
use tokio::net::UdpSocket;
use tokio::time::{timeout, Duration, Instant};
use tokio::sync::mpsc;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn, error, debug};
use std::net::SocketAddr;

// =============================================================================
// TELEMETRY DOWNLINK (Satellite -> Ground)
// =============================================================================

/// Spawn telemetry downlink task
pub fn spawn_downlink(
    mut rx: mpsc::Receiver<SensorReading>,
    config: &Config,
    metrics: Arc<Mutex<Metrics>>,
) -> tokio::task::JoinHandle<()> {
    let ground_addr = config.get_network_addrs().ground_addr.clone();
    let downlink_deadline_ms = config.system.downlink_deadline_ms;
    let degraded_threshold = config.system.buffer_degraded_threshold_percent;
    
    tokio::spawn(async move {
        // Create UDP socket (connectionless - Lab 8)
        let socket = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to create downlink socket: {}", e);
                return;
            }
        };
        
        info!("[DOWNLINK] Telemetry downlink started -> {}", ground_addr);
        
        let mut packet_id = 0u64;
        
        while let Some(reading) = rx.recv().await {
            let downlink_start = Instant::now();
            
            // Convert reading to telemetry packet
            let packet = reading_to_telemetry(&reading, packet_id);
            
            // Serialize to JSON
            match packet.to_bytes() {
                Ok(bytes) => {
                    // Send via UDP (fire-and-forget, Lab 8)
                    match socket.send_to(&bytes, &ground_addr).await {
                        Ok(sent_bytes) => {
                            let latency = downlink_start.elapsed();
                            let latency_ms = latency.as_secs_f64() * 1000.0;
                            
                            // Check downlink deadline (30ms)
                            if latency_ms > downlink_deadline_ms as f64 {
                                warn!(
                                    "[WARN]  Downlink packet {} exceeded {}ms deadline: {:.3}ms",
                                    packet_id, downlink_deadline_ms, latency_ms
                                );
                                metrics.lock().await.missed_deadlines += 1;
                            }
                            
                            // Update metrics
                            {
                                let mut m = metrics.lock().await;
                                m.packets_sent += 1;
                            }
                            
                            debug!(
                                "[DOWNLINK] Sent telemetry #{} | {} bytes | latency: {:.3}ms",
                                packet_id, sent_bytes, latency_ms
                            );
                        }
                        Err(e) => {
                            error!("[ERR] Failed to send telemetry #{}: {}", packet_id, e);
                            metrics.lock().await.packets_failed += 1;
                        }
                    }
                }
                Err(e) => {
                    error!("[ERR] Failed to serialize telemetry: {}", e);
                }
            }
            
            packet_id += 1;
            
            // Update buffer fill estimate
            let current_fill = (rx.len() as f32 / 100.0) * 100.0;
            {
                let mut m = metrics.lock().await;
                m.update_buffer_fill(current_fill);
            }
            
            // Degraded mode warning
            if current_fill > degraded_threshold {
                warn!(
                    "[WARN]  Buffer at {:.1}% - DEGRADED MODE (threshold: {:.1}%)",
                    current_fill, degraded_threshold
                );
            }
        }
        
        info!("Telemetry downlink task terminated");
    })
}

// =============================================================================
// COMMAND UPLINK (Ground -> Satellite)
// =============================================================================

/// Spawn command uplink receiver task
pub fn spawn_uplink(
    config: &Config,
    metrics: Arc<Mutex<Metrics>>,
) -> tokio::task::JoinHandle<()> {
    let bind_addr = config.get_network_addrs().satellite_addr.clone();
    let dead_mans_switch_timeout = Duration::from_secs(config.system.dead_mans_switch_timeout_s);
    let urgent_deadline_ms = config.system.urgent_command_deadline_ms;
    
    tokio::spawn(async move {
        // Bind UDP socket to receive commands
        let socket = match UdpSocket::bind(&bind_addr).await {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to bind uplink socket on {}: {}", bind_addr, e);
                return;
            }
        };
        
        info!("[UPLINK] Command uplink started on {}", bind_addr);
        info!("   Dead man's switch: {} seconds", dead_mans_switch_timeout.as_secs());
        
        let mut buffer = vec![0u8; 65535]; // Max UDP packet size

        // Latch: true while contact is lost. We record ONE fault on the first
        // timeout and stay silent until contact is restored, preventing the
        // 2-second tick from inflating fault_count by 30/minute.
        let mut contact_lost = false;

        loop {
            // Lab 8 Exercise 2: Dead Man's Switch timeout
            match timeout(dead_mans_switch_timeout, socket.recv_from(&mut buffer)).await {
                Ok(Ok((len, addr))) => {
                    if contact_lost {
                        info!("[OK] Ground contact restored from {}", addr);
                        contact_lost = false;
                    }
                    handle_command(&buffer[..len], addr, &socket, &metrics, urgent_deadline_ms).await;
                }
                Ok(Err(e)) => {
                    error!("[ERR] Socket receive error: {}", e);
                }
                Err(_) => {
                    if !contact_lost {
                        // First timeout only — log and count once
                        error!(
                            "[ALERT] CRITICAL ALERT: No ground contact for {} seconds - LOSS OF SIGNAL",
                            dead_mans_switch_timeout.as_secs()
                        );
                        metrics.lock().await.fault_count += 1;
                        contact_lost = true;
                    }
                    // While contact remains lost, subsequent ticks are silent
                }
            }
        }
    })
}

/// Handle received command
async fn handle_command(
    bytes: &[u8],
    addr: SocketAddr,
    socket: &UdpSocket,
    metrics: &Arc<Mutex<Metrics>>,
    urgent_deadline_ms: u64,
) {
    let recv_time = Instant::now(); // start of full command latency

    // Deserialize command
    match CommandPacket::from_bytes(bytes) {
        Ok(cmd) => {
            info!(
                "[CMD] Command #{} from {} | urgency: {:?}",
                cmd.command_id, addr, cmd.urgency
            );

            {
                let mut m = metrics.lock().await;
                m.commands_received += 1;
            }

            // ================================================================
            // SAFETY INTERLOCK — block commands while a fault is active
            // EmergencyShutdown and ClearFault are always allowed through
            // ================================================================
            let interlock_active = {
                let m = metrics.lock().await;
                m.interlock_active
            };

            let is_exempt = matches!(
                cmd.payload,
                crate::protocol::CommandPayload::EmergencyShutdown { .. }
                    | crate::protocol::CommandPayload::ClearFault { .. }
            );

            if interlock_active && !is_exempt {
                let reason = {
                    let m = metrics.lock().await;
                    m.interlock_reason.clone()
                };
                warn!(
                    "[LOCK] Command #{} REJECTED by safety interlock: {}",
                    cmd.command_id, reason
                );
                metrics.lock().await.commands_rejected += 1;
                send_acknowledgment(socket, addr, cmd.command_id, false, 0.0).await;
                return;
            }

            // ================================================================
            // PROCESS COMMAND
            // ================================================================
            let process_start = Instant::now();
            let success = process_command(&cmd).await;
            let processing_ms = process_start.elapsed().as_secs_f64() * 1000.0;

            // Full command latency: recv -> ACK sent
            let full_latency_ms = recv_time.elapsed().as_secs_f64() * 1000.0;

            // Check urgent command deadline (2ms)
            if matches!(cmd.urgency, CommandUrgency::Urgent | CommandUrgency::Emergency) {
                if processing_ms > urgent_deadline_ms as f64 {
                    warn!(
                        "[WARN]  Urgent command #{} processing exceeded {}ms: {:.3}ms",
                        cmd.command_id, urgent_deadline_ms, processing_ms
                    );
                    metrics.lock().await.missed_deadlines += 1;
                }
            }

            {
                let mut m = metrics.lock().await;
                if success {
                    m.commands_processed += 1;
                }
                m.record_command_latency(full_latency_ms);
            }

            debug!(
                "[CMD] Command #{} latency: {:.3}ms (processing: {:.3}ms)",
                cmd.command_id, full_latency_ms, processing_ms
            );

            send_acknowledgment(socket, addr, cmd.command_id, success, processing_ms).await;
        }
        Err(e) => {
            error!("[ERR] Failed to deserialize command from {}: {}", addr, e);
        }
    }
}

/// Process command based on type
async fn process_command(cmd: &CommandPacket) -> bool {
    use crate::protocol::CommandPayload;
    
    match &cmd.payload {
        CommandPayload::HealthCheck => {
            info!("[OK] Health check OK");
            true
        }
        CommandPayload::SetMode { mode } => {
            info!("[RESET] Mode changed to: {:?}", mode);
            // In real system: actually change mode
            true
        }
        CommandPayload::SetSensorInterval { sensor_id, interval_ms } => {
            info!("[TASK]  Sensor {} interval -> {}ms", sensor_id, interval_ms);
            // In real system: update sensor configuration
            true
        }
        CommandPayload::ClearFault { fault_id } => {
            info!("[FIX] Cleared fault #{}", fault_id);
            // In real system: clear fault state
            true
        }
        CommandPayload::ResetSubsystem { subsystem } => {
            info!("[RESET] Resetting subsystem: {}", subsystem);
            // In real system: reset the subsystem
            true
        }
        CommandPayload::RequestData { data_type, time_range_seconds } => {
            info!("[DATA] Data request: {} (range: {:?}s)", data_type, time_range_seconds);
            // In real system: prepare data dump
            true
        }
        CommandPayload::EmergencyShutdown { reason } => {
            error!("[ALERT] EMERGENCY SHUTDOWN: {}", reason);
            // In real system: initiate safe shutdown
            false
        }
    }
}

/// Send acknowledgment back to ground
async fn send_acknowledgment(
    socket: &UdpSocket,
    addr: SocketAddr,
    command_id: u64,
    success: bool,
    execution_time_ms: f64,
) {
    use crate::protocol::{TelemetryPacket, TelemetryPayload};
    use chrono::Utc;
    
    let ack = TelemetryPacket {
        packet_id: command_id,
        timestamp: Utc::now(),
        payload: TelemetryPayload::CommandAck {
            command_id,
            success,
            execution_time_ms,
            message: if success { "OK".to_string() } else { "FAILED".to_string() },
        },
    };
    
    match ack.to_bytes() {
        Ok(bytes) => {
            if let Err(e) = socket.send_to(&bytes, addr).await {
                error!("Failed to send ACK for command {}: {}", command_id, e);
            } else {
                debug!("[OK] Sent ACK for command #{}", command_id);
            }
        }
        Err(e) => {
            error!("Failed to serialize ACK: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_processing() {
        // Test command processing logic
        let cmd = CommandPacket {
            command_id: 1,
            timestamp: chrono::Utc::now(),
            urgency: CommandUrgency::Routine,
            payload: crate::protocol::CommandPayload::HealthCheck,
        };
        
        // Should be able to serialize/deserialize
        let bytes = cmd.to_bytes().unwrap();
        let decoded = CommandPacket::from_bytes(&bytes).unwrap();
        assert_eq!(cmd.command_id, decoded.command_id);
    }
}