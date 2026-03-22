// src/interlock.rs
// Safety Interlock Gate  (Assignment §GCS-3)
//
// This file sits between command_brain.rs (decision) and uplink.rs (send).
// Every proposed CommandPacket passes through check() before it reaches the wire.
//
// Responsibilities:
//   • Block unsafe commands while a fault is active
//   • Track interlock latency: time from fault detection → command blocked
//   • Log every rejection with command ID, reason, and timestamp
//   • Allow Emergency commands to bypass (they cannot be blocked)
//   • Release interlock when brain signals system is safe again

use crate::protocol::{CommandPacket, CommandUrgency, FaultType, AlertSeverity};
use crate::metrics::GcsMetrics;
use tokio::sync::Mutex;
use tokio::time::Instant;
use std::sync::Arc;
use chrono::Utc;
use tracing::{info, warn, error};

// ─── Interlock state ──────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct InterlockState {
    pub active: bool,
    pub reason: String,
    pub engaged_at: Option<Instant>,   // for latency measurement
    pub active_fault: Option<ActiveFault>,
}

#[derive(Debug, Clone)]
pub struct ActiveFault {
    pub fault_type: FaultType,
    pub severity:   AlertSeverity,
    pub detected_at: Instant,
}

impl InterlockState {
    pub fn new() -> Self {
        Self {
            active: false,
            reason: String::new(),
            engaged_at: None,
            active_fault: None,
        }
    }

    /// Engage the interlock. Records engagement time for latency tracking.
    pub fn engage(&mut self, reason: &str, fault: ActiveFault) {
        if !self.active {
            self.active = true;
            self.reason = reason.to_string();
            self.engaged_at = Some(Instant::now());
            self.active_fault = Some(fault);
            warn!("🔒 INTERLOCK ENGAGED: {}", reason);
        }
    }

    /// Release the interlock. Logs how long it was held.
    pub fn release(&mut self) -> f64 {
        let held_ms = self.engaged_at
            .map(|t| t.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or(0.0);

        self.active = false;
        self.reason.clear();
        self.engaged_at = None;
        self.active_fault = None;

        info!("🔓 INTERLOCK RELEASED (held for {:.3}ms)", held_ms);
        held_ms
    }
}

// ─── Decision ─────────────────────────────────────────────────────────────────

pub enum InterlockDecision {
    /// Command is safe to send
    Approved,
    /// Command is blocked — reason and timestamp are logged
    Blocked { reason: String },
}

/// Gate every proposed command through this before sending.
/// Returns Approved or Blocked. Caller should never send a Blocked command.
pub async fn check(
    cmd: &CommandPacket,
    state: &Arc<Mutex<InterlockState>>,
    metrics: &Arc<Mutex<GcsMetrics>>,
) -> InterlockDecision {
    // Emergency commands always pass — they are the escape hatch
    if matches!(cmd.urgency, CommandUrgency::Emergency) {
        info!("⚡ Emergency cmd #{} bypasses interlock", cmd.command_id);
        return InterlockDecision::Approved;
    }

    let lock = state.lock().await;

    if !lock.active {
        return InterlockDecision::Approved;
    }

    // Interlock is active — block the command
    let reason = format!(
        "Interlock active (fault: {})",
        lock.reason
    );

    // Measure interlock latency: fault detection → this block event
    let interlock_latency_ms = lock.engaged_at
        .map(|t| t.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0);

    drop(lock); // release mutex before awaiting metrics lock

    // Record the block
    {
        let mut m = metrics.lock().await;
        m.commands_blocked += 1;
        m.record_interlock_latency(interlock_latency_ms);
        m.log_rejection(RejectionRecord {
            command_id: cmd.command_id,
            urgency: format!("{:?}", cmd.urgency),
            payload_summary: describe_payload(cmd),
            reason: reason.clone(),
            timestamp: Utc::now().to_rfc3339(),
            interlock_latency_ms,
        });
    }

    warn!(
        "🚫 BLOCKED cmd #{} ({:?}) — {} | interlock latency: {:.3}ms",
        cmd.command_id, cmd.urgency, reason, interlock_latency_ms
    );

    InterlockDecision::Blocked { reason }
}

/// Engage interlock from outside (called by communication.rs on fault receipt).
pub async fn engage(
    state: &Arc<Mutex<InterlockState>>,
    metrics: &Arc<Mutex<GcsMetrics>>,
    reason: &str,
    fault: ActiveFault,
) {
    let detection_to_engage = fault.detected_at.elapsed().as_secs_f64() * 1000.0;
    state.lock().await.engage(reason, fault);

    let mut m = metrics.lock().await;
    m.interlocks_engaged += 1;
    m.record_interlock_latency(detection_to_engage);

    if detection_to_engage > 100.0 {
        error!(
            "🚨 CRITICAL GROUND ALERT: Interlock engagement took {:.3}ms > 100ms deadline!",
            detection_to_engage
        );
        m.missed_fault_response_deadlines += 1;
    }
}

/// Release interlock from outside (called by command_brain on safe ACK/health).
pub async fn release(
    state: &Arc<Mutex<InterlockState>>,
    metrics: &Arc<Mutex<GcsMetrics>>,
) {
    let held_ms = state.lock().await.release();
    metrics.lock().await.interlocks_released += 1;
    info!("Interlock held for {:.3}ms total", held_ms);
}

// ─── Rejection log entry ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RejectionRecord {
    pub command_id:           u64,
    pub urgency:              String,
    pub payload_summary:      String,
    pub reason:               String,
    pub timestamp:            String,
    pub interlock_latency_ms: f64,
}

impl std::fmt::Display for RejectionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] cmd #{} ({} / {}) BLOCKED — {} | latency {:.3}ms",
            self.timestamp,
            self.command_id,
            self.urgency,
            self.payload_summary,
            self.reason,
            self.interlock_latency_ms,
        )
    }
}

fn describe_payload(cmd: &CommandPacket) -> String {
    use crate::protocol::CommandPayload;
    match &cmd.payload {
        CommandPayload::HealthCheck                         => "HealthCheck".into(),
        CommandPayload::SetMode { mode }                    => format!("SetMode({:?})", mode),
        CommandPayload::SetSensorInterval { sensor_id, .. } => format!("SetSensorInterval({})", sensor_id),
        CommandPayload::ResetSubsystem { subsystem }        => format!("ResetSubsystem({})", subsystem),
        CommandPayload::ClearFault { fault_id }             => format!("ClearFault({})", fault_id),
        CommandPayload::RequestData { data_type, .. }       => format!("RequestData({})", data_type),
        CommandPayload::EmergencyShutdown { .. }            => "EmergencyShutdown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CommandPayload, CommandUrgency};
    use chrono::Utc;

    fn make_cmd(id: u64, urgency: CommandUrgency) -> CommandPacket {
        CommandPacket {
            command_id: id,
            timestamp: Utc::now(),
            urgency,
            payload: CommandPayload::HealthCheck,
        }
    }

    #[tokio::test]
    async fn test_emergency_bypasses_interlock() {
        let state = Arc::new(Mutex::new(InterlockState::new()));
        let metrics = Arc::new(Mutex::new(crate::metrics::GcsMetrics::new()));

        // Engage interlock
        state.lock().await.engage("test fault", ActiveFault {
            fault_type: FaultType::ThermalAnomaly,
            severity: AlertSeverity::Critical,
            detected_at: Instant::now(),
        });

        // Emergency must pass
        let cmd = make_cmd(1, CommandUrgency::Emergency);
        let decision = check(&cmd, &state, &metrics).await;
        assert!(matches!(decision, InterlockDecision::Approved));
    }

    #[tokio::test]
    async fn test_routine_blocked_during_interlock() {
        let state = Arc::new(Mutex::new(InterlockState::new()));
        let metrics = Arc::new(Mutex::new(crate::metrics::GcsMetrics::new()));

        state.lock().await.engage("test fault", ActiveFault {
            fault_type: FaultType::BufferOverflow,
            severity: AlertSeverity::Warning,
            detected_at: Instant::now(),
        });

        let cmd = make_cmd(2, CommandUrgency::Routine);
        let decision = check(&cmd, &state, &metrics).await;
        assert!(matches!(decision, InterlockDecision::Blocked { .. }));

        // Verify rejection was logged
        assert_eq!(metrics.lock().await.commands_blocked, 1);
    }

    #[tokio::test]
    async fn test_approved_when_no_interlock() {
        let state = Arc::new(Mutex::new(InterlockState::new()));
        let metrics = Arc::new(Mutex::new(crate::metrics::GcsMetrics::new()));

        let cmd = make_cmd(3, CommandUrgency::Urgent);
        let decision = check(&cmd, &state, &metrics).await;
        assert!(matches!(decision, InterlockDecision::Approved));
    }
}