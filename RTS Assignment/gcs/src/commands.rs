// src/command_brain.rs

use crate::protocol::{
    TelemetryPacket, TelemetryPayload, SensorType,
    CommandPacket, CommandPayload, CommandUrgency,
    FaultType, AlertSeverity, SystemMode,
};
use crate::interlock::ActiveFault;
use tokio::time::Instant;
use chrono::Utc;
use tracing::{info, warn};

const THERMAL_HIGH_C:          f64 = 75.0;
const THERMAL_LOW_C:           f64 = 30.0;
const THERMAL_FAST_MS:         u64 = 50;
const THERMAL_NORMAL_MS:       u64 = 100;
const THERMAL_SLOW_MS:         u64 = 200;
const POWER_LOW_V:             f64 = 11.0;
const POWER_HIGH_V:            f64 = 13.5;
const BUFFER_DEGRADED_PCT:     f32 = 80.0;

// ── Brain state ───────────────────────────────────────────────────────────────
#[derive(Debug, Default)]
pub struct GcsBrainState {
    pub interlock_active:        bool,
    pub last_system_mode:        Option<SystemMode>,
    pub command_id_counter:      u64,
    // Signals consumed by main.rs after each analyse() call
    pub should_release_interlock: bool,
    pub pending_interlock:        Option<(String, ActiveFault)>,
}

impl GcsBrainState {
    pub fn new() -> Self { Self::default() }

    fn next_id(&mut self) -> u64 {
        self.command_id_counter += 1;
        self.command_id_counter
    }

    fn request_engage(&mut self, reason: &str, fault_type: FaultType, severity: AlertSeverity) {
        if !self.interlock_active {
            warn!("🔒 Brain requesting interlock: {}", reason);
            self.interlock_active  = true;
            self.pending_interlock = Some((reason.to_string(), ActiveFault {
                fault_type,
                severity,
                detected_at: Instant::now(),
            }));
        }
    }

    fn request_release(&mut self) {
        if self.interlock_active {
            info!("🔓 Brain requesting interlock release");
            self.interlock_active         = false;
            self.should_release_interlock = true;
        }
    }
}

// ── Main entry point ──────────────────────────────────────────────────────────
pub fn analyse(pkt: &TelemetryPacket, state: &mut GcsBrainState) -> Option<CommandPacket> {
    // Reset one-shot signals from last call
    state.should_release_interlock = false;
    state.pending_interlock        = None;

    match &pkt.payload {
        TelemetryPayload::SensorData { sensor_type, value, .. } =>
            analyse_sensor(sensor_type, *value, state),

        TelemetryPayload::HealthStatus { buffer_fill_percentage, mode, .. } =>
            analyse_health(*buffer_fill_percentage, mode, state),

        TelemetryPayload::FaultAlert { fault_type, severity, description, .. } =>
            analyse_fault(fault_type, severity, description, state),

        TelemetryPayload::CommandAck { success, message, .. } => {
            if *success {
                info!("OCS ACKNOWLEDGE: {}", message);
                state.request_release();
            } else {
                warn!("OCS NOT ACKNOWLEDGE: {}", message);
            }
            None
        }
    }
}

// ── Sensor handler ────────────────────────────────────────────────────────────
fn analyse_sensor(sensor_type: &SensorType, value: f64, state: &mut GcsBrainState) -> Option<CommandPacket> {
    match sensor_type {
        SensorType::Thermal => {
            if value > THERMAL_HIGH_C {
                warn!("Thermal HIGH {:.1}°C — slowing sample rate", value);
                state.request_engage("Thermal above threshold", FaultType::ThermalAnomaly, AlertSeverity::Warning);
                Some(cmd(state.next_id(), CommandUrgency::Urgent,
                    CommandPayload::SetSensorInterval { sensor_id: 1, interval_ms: THERMAL_SLOW_MS }))
            } else if value < THERMAL_LOW_C {
                info!("Thermal LOW {:.1}°C — increasing sample rate", value);
                Some(cmd(state.next_id(), CommandUrgency::Routine,
                    CommandPayload::SetSensorInterval { sensor_id: 1, interval_ms: THERMAL_FAST_MS }))
            } else if state.interlock_active {
                info!("Thermal normal {:.1}°C — restoring baseline", value);
                state.request_release();
                Some(cmd(state.next_id(), CommandUrgency::Routine,
                    CommandPayload::SetSensorInterval { sensor_id: 1, interval_ms: THERMAL_NORMAL_MS }))
            } else {
                None
            }
        }
        SensorType::Power => {
            if value < POWER_LOW_V {
                warn!("POWER LOW {:.2}V — switching to Degraded", value);
                state.request_engage("Low voltage", FaultType::SensorFailure, AlertSeverity::Warning);
                Some(cmd(state.next_id(), CommandUrgency::Urgent,
                    CommandPayload::SetMode { mode: SystemMode::Degraded }))
            } else if value > POWER_HIGH_V && state.interlock_active {
                info!("POWER restored {:.2}V — returning to Normal", value);
                state.request_release();
                Some(cmd(state.next_id(), CommandUrgency::Routine,
                    CommandPayload::SetMode { mode: SystemMode::Normal }))
            } else {
                None
            }
        }
        SensorType::Attitude => None,
    }
}

// ── Health handler ────────────────────────────────────────────────────────────
fn analyse_health(buffer_fill: f32, mode: &SystemMode, state: &mut GcsBrainState) -> Option<CommandPacket> {
    if buffer_fill > BUFFER_DEGRADED_PCT {
        if !matches!(state.last_system_mode, Some(SystemMode::Degraded)) {
            warn!("BUFFER {:.1}% — requesting Degraded mode", buffer_fill);
            state.last_system_mode = Some(SystemMode::Degraded);
            state.request_engage("Buffer overflow threshold", FaultType::BufferOverflow, AlertSeverity::Warning);
            return Some(cmd(state.next_id(), CommandUrgency::Urgent,
                CommandPayload::SetMode { mode: SystemMode::Degraded }));
        }
    }
    if matches!(mode, SystemMode::Normal) {
        state.last_system_mode = Some(SystemMode::Normal);
        state.request_release();
    }
    None
}

// ── Fault handler ─────────────────────────────────────────────────────────────
fn analyse_fault(
    fault_type:  &FaultType,
    severity:    &AlertSeverity,
    description: &str,
    state:       &mut GcsBrainState,
) -> Option<CommandPacket> {
    warn!("FAULT [{:?}]: {}", severity, description);
    state.request_engage(description, fault_type.clone(), severity.clone());

    let urgency = if matches!(severity, AlertSeverity::Critical) {
        CommandUrgency::Emergency
    } else {
        CommandUrgency::Urgent
    };

    let payload = match fault_type {
        FaultType::ThermalAnomaly      => CommandPayload::SetSensorInterval { sensor_id: 1, interval_ms: THERMAL_SLOW_MS },
        FaultType::BufferOverflow      => CommandPayload::SetMode { mode: SystemMode::Degraded },
        FaultType::CommunicationTimeout => CommandPayload::ResetSubsystem { subsystem: "communication".into() },
        FaultType::ConsecutiveDataLoss => CommandPayload::ClearFault { fault_id: 0 },
        _                              => CommandPayload::HealthCheck,
    };

    Some(cmd(state.next_id(), urgency, payload))
}

fn cmd(id: u64, urgency: CommandUrgency, payload: CommandPayload) -> CommandPacket {
    CommandPacket { command_id: id, timestamp: Utc::now(), urgency, payload }
}