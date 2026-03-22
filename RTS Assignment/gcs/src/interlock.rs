// src/interlock.rs

use crate::protocol::{FaultType, AlertSeverity, CommandPacket, CommandUrgency};
use crate::metrics::{GcsMetrics, RejectionRecord};
use tokio::sync::Mutex;
use tokio::time::Instant;
use std::sync::Arc;
use chrono::Utc;
use tracing::{info, warn, error};

const FAULT_RESPONSE_DEADLINE_MS: f64 = 100.0;

// ── Active fault description ──────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct ActiveFault {
    pub fault_type:  FaultType,
    pub severity:    AlertSeverity,
    pub detected_at: Instant,
}

// ── Interlock state ───────────────────────────────────────────────────────────
#[derive(Debug)]
pub struct InterlockState {
    pub active:       bool,
    pub reason:       String,
    pub engaged_at:   Option<Instant>,
    pub active_fault: Option<ActiveFault>,
}

impl InterlockState {
    pub fn new() -> Self {
        Self { active: false, reason: String::new(), engaged_at: None, active_fault: None }
    }

    pub fn engage(&mut self, reason: &str, fault: ActiveFault) {
        if !self.active {
            warn!("🔒 INTERLOCK ENGAGED: {}", reason);
            self.active       = true;
            self.reason       = reason.to_string();
            self.engaged_at   = Some(Instant::now());
            self.active_fault = Some(fault);
        }
    }

    pub fn release(&mut self) -> f64 {
        let held = self.engaged_at
            .map(|t| t.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        self.active       = false;
        self.reason       = String::new();
        self.engaged_at   = None;
        self.active_fault = None;
        info!("🔓 INTERLOCK RELEASED (held {:.3}ms)", held);
        held
    }
}

// ── Decision ──────────────────────────────────────────────────────────────────
pub enum InterlockDecision {
    Approved,
    Blocked { reason: String },
}

pub async fn check(
    cmd:     &CommandPacket,
    state:   &Arc<Mutex<InterlockState>>,
    metrics: &Arc<Mutex<GcsMetrics>>,
) -> InterlockDecision {
    // Emergency always passes
    if matches!(cmd.urgency, CommandUrgency::Emergency) {
        info!("⚡ Emergency cmd #{} bypasses interlock", cmd.command_id);
        return InterlockDecision::Approved;
    }

    let lock = state.lock().await;
    if !lock.active {
        return InterlockDecision::Approved;
    }

    let reason             = format!("Interlock active: {}", lock.reason);
    let interlock_latency  = lock.engaged_at
        .map(|t| t.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    drop(lock);

    let record = RejectionRecord {
        command_id:           cmd.command_id,
        urgency:              format!("{:?}", cmd.urgency),
        payload_summary:      describe(cmd),
        reason:               reason.clone(),
        timestamp:            Utc::now().to_rfc3339(),
        interlock_latency_ms: interlock_latency,
    };

    let mut m = metrics.lock().await;
    m.commands_blocked += 1;
    m.record_interlock_latency(interlock_latency);
    m.log_rejection(record);

    warn!("🚫 BLOCKED cmd #{} ({:?}) — {} | latency {:.3}ms",
        cmd.command_id, cmd.urgency, reason, interlock_latency);

    InterlockDecision::Blocked { reason }
}

pub async fn engage(
    state:   &Arc<Mutex<InterlockState>>,
    metrics: &Arc<Mutex<GcsMetrics>>,
    reason:  &str,
    fault:   ActiveFault,
) {
    let latency = fault.detected_at.elapsed().as_secs_f64() * 1000.0;
    state.lock().await.engage(reason, fault);

    let mut m = metrics.lock().await;
    m.interlocks_engaged += 1;
    m.record_interlock_latency(latency);

    if latency > FAULT_RESPONSE_DEADLINE_MS {
        error!("🚨 CRITICAL: Interlock engage took {:.3}ms > {}ms deadline!",
            latency, FAULT_RESPONSE_DEADLINE_MS);
        m.missed_fault_response_deadlines += 1;
    }
}

pub async fn release(
    state:   &Arc<Mutex<InterlockState>>,
    metrics: &Arc<Mutex<GcsMetrics>>,
) {
    let held = state.lock().await.release();
    let mut m = metrics.lock().await;
    m.interlocks_released += 1;
    info!("Interlock held for {:.3}ms", held);
}

fn describe(cmd: &CommandPacket) -> String {
    use crate::protocol::CommandPayload;
    match &cmd.payload {
        CommandPayload::HealthCheck                          => "HealthCheck".into(),
        CommandPayload::SetMode { mode }                     => format!("SetMode({:?})", mode),
        CommandPayload::SetSensorInterval { sensor_id, .. }  => format!("SetSensorInterval({})", sensor_id),
        CommandPayload::ResetSubsystem { subsystem }         => format!("ResetSubsystem({})", subsystem),
        CommandPayload::ClearFault { fault_id }              => format!("ClearFault({})", fault_id),
        CommandPayload::RequestData { data_type, .. }        => format!("RequestData({})", data_type),
        CommandPayload::EmergencyShutdown { .. }             => "EmergencyShutdown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CommandPayload, CommandUrgency};
    use chrono::Utc;

    fn cmd(id: u64, urgency: CommandUrgency) -> CommandPacket {
        CommandPacket { command_id: id, timestamp: Utc::now(), urgency, payload: CommandPayload::HealthCheck }
    }

    #[tokio::test]
    async fn emergency_bypasses_interlock() {
        let state   = Arc::new(Mutex::new(InterlockState::new()));
        let metrics = Arc::new(Mutex::new(GcsMetrics::new()));
        state.lock().await.engage("test", ActiveFault {
            fault_type: FaultType::ThermalAnomaly,
            severity: AlertSeverity::Critical,
            detected_at: Instant::now(),
        });
        assert!(matches!(check(&cmd(1, CommandUrgency::Emergency), &state, &metrics).await, InterlockDecision::Approved));
    }

    #[tokio::test]
    async fn routine_blocked_during_interlock() {
        let state   = Arc::new(Mutex::new(InterlockState::new()));
        let metrics = Arc::new(Mutex::new(GcsMetrics::new()));
        state.lock().await.engage("test", ActiveFault {
            fault_type: FaultType::BufferOverflow,
            severity: AlertSeverity::Warning,
            detected_at: Instant::now(),
        });
        assert!(matches!(check(&cmd(2, CommandUrgency::Routine), &state, &metrics).await, InterlockDecision::Blocked { .. }));
        assert_eq!(metrics.lock().await.commands_blocked, 1);
    }
}