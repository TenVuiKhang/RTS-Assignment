
// Shared protocol definitions between Satellite and Ground Control


use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

// =============================================================================
// TELEMETRY (Satellite → Ground)
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryPacket {
    pub packet_id: u64,
    pub timestamp: DateTime<Utc>,
    pub payload: TelemetryPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TelemetryPayload {
    /// Sensor data reading
    SensorData {
        sensor_id: u8,
        sensor_type: SensorType,
        value: f64,
        priority: u8,
    },

    /// System health status
    HealthStatus {
        cpu_utilization: f32,
        buffer_fill_percentage: f32,
        active_tasks: u8,
        mode: SystemMode,
    },

    /// Fault alert notification
    FaultAlert {
        fault_type: FaultType,
        severity: AlertSeverity,
        description: String,
        timestamp: DateTime<Utc>,
    },

    /// Command acknowledgment
    CommandAck {
        command_id: u64,
        success: bool,
        execution_time_ms: f64,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SensorType {
    Thermal,
    Attitude,
    Power,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SystemMode {
    Normal,
    Degraded,
    SafeMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FaultType {
    SensorFailure,
    BufferOverflow,
    MissedDeadline,
    CommunicationTimeout,
    ThermalAnomaly,
    ConsecutiveDataLoss,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

// ==========================================
// COMMANDS (Ground → Satellite)


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandPacket {
    pub command_id: u64,
    pub timestamp: DateTime<Utc>,
    pub urgency: CommandUrgency,
    pub payload: CommandPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CommandUrgency {
    Routine,      // Can wait
    Urgent,       // Must process within 2ms
    Emergency,    // Immediate execution
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommandPayload {
    /// Health check / ping
    HealthCheck,

    /// Adjust sensor sampling rate
    SetSensorInterval {
        sensor_id: u8,
        interval_ms: u64,
    },

    /// Request specific data dump
    RequestData {
        data_type: String,
        time_range_seconds: Option<u64>,
    },

    /// Change operating mode
    SetMode {
        mode: SystemMode,
    },

    /// Reset a subsystem
    ResetSubsystem {
        subsystem: String,
    },

    /// Clear a fault condition
    ClearFault {
        fault_id: u64,
    },

    /// Emergency shutdown
    EmergencyShutdown {
        reason: String,
    },
}

// ==========================================
// SERIALIZATION HELPERS

impl TelemetryPacket {
    /// Serialize to JSON bytes for UDP transmission
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let json = serde_json::to_string(self)?;
        Ok(json.into_bytes())
    }

    /// Deserialize from received bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Get packet size in bytes
    pub fn size_bytes(&self) -> usize {
        self.to_bytes().unwrap_or_default().len()
    }
}

impl CommandPacket {
    /// Serialize to JSON bytes for UDP transmission
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let json = serde_json::to_string(self)?;
        Ok(json.into_bytes())
    }

    /// Deserialize from received bytes
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// Check if command requires urgent processing
    pub fn is_urgent(&self) -> bool {
        matches!(self.urgency, CommandUrgency::Urgent | CommandUrgency::Emergency)
    }
}

// ==========================================
// TESTS

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_telemetry_serialization() {
        let packet = TelemetryPacket {
            packet_id: 123,
            timestamp: Utc::now(),
            payload: TelemetryPayload::SensorData {
                sensor_id: 1,
                sensor_type: SensorType::Thermal,
                value: 42.5,
                priority: 3,
            },
        };

        let bytes = packet.to_bytes().unwrap();
        let decoded = TelemetryPacket::from_bytes(&bytes).unwrap();

        assert_eq!(packet.packet_id, decoded.packet_id);
    }

    #[test]
    fn test_command_serialization() {
        let cmd = CommandPacket {
            command_id: 456,
            timestamp: Utc::now(),
            urgency: CommandUrgency::Urgent,
            payload: CommandPayload::HealthCheck,
        };

        let bytes = cmd.to_bytes().unwrap();
        let decoded = CommandPacket::from_bytes(&bytes).unwrap();

        assert_eq!(cmd.command_id, decoded.command_id);
        assert!(decoded.is_urgent());
    }
}