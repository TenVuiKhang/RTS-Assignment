// src/config.rs
// Configuration with default fallback - NEVER FAILS

use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub network: NetworkConfig,
    pub sensors: SensorConfig,
    pub system: SystemConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    pub mode: String,
    pub localhost: NetworkAddresses,
    pub wifi: NetworkAddresses,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkAddresses {
    pub satellite_addr: String,
    pub ground_addr: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SensorConfig {
    pub thermal_interval_ms: u64,
    pub thermal_priority: u8,
    pub attitude_interval_ms: u64,
    pub attitude_priority: u8,
    pub power_interval_ms: u64,
    pub power_priority: u8,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SystemConfig {
    pub buffer_size: usize,
    pub fault_injection_every: u64,
    pub downlink_deadline_ms: u64,
    pub urgent_command_deadline_ms: u64,
    pub thermal_jitter_threshold_ms: f64,
    pub dead_mans_switch_timeout_s: u64,
    pub metrics_report_interval_s: u64,
    pub buffer_degraded_threshold_percent: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub log_to_file: bool,
    pub log_file_path: String,
}

impl Config {
    /// Default configuration (fallback)
    pub fn default_config() -> Self {
        const DEFAULT_CONFIG: &str = r#"
[network]
mode = "localhost"

[network.localhost]
satellite_addr = "127.0.0.1:8001"
ground_addr = "127.0.0.1:8002"

[network.wifi]
satellite_addr = "0.0.0.0:8001"
ground_addr = "192.168.1.105:8002"

[sensors]
thermal_interval_ms = 100
thermal_priority = 3
attitude_interval_ms = 200
attitude_priority = 2
power_interval_ms = 500
power_priority = 1

[system]
buffer_size = 100
fault_injection_every = 600
downlink_deadline_ms = 30
urgent_command_deadline_ms = 2
thermal_jitter_threshold_ms = 1.0
dead_mans_switch_timeout_s = 2
metrics_report_interval_s = 10
buffer_degraded_threshold_percent = 80.0

[logging]
level = "info"
log_to_file = true
log_file_path = "logs/satellite.log"
"#;
        
        toml::from_str(DEFAULT_CONFIG).expect("Default config is valid")
    }

    /// Load configuration from file, with fallback to default
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        // Try multiple possible locations
        let possible_paths = vec![
            PathBuf::from(path),
            PathBuf::from(format!("./{}", path)),
            PathBuf::from(format!("../{}", path)),
            std::env::current_dir()?.join(path),
        ];
        
        for path_buf in &possible_paths {
            if path_buf.exists() {
                let contents = fs::read_to_string(path_buf)?;
                let config: Config = toml::from_str(&contents)?;
                tracing::info!("✅ Loaded config from: {:?}", path_buf);
                return Ok(config);
            }
        }
        
        // Not found - return error with helpful message
        Err(format!(
            "Config file '{}' not found. Searched in:\n{}",
            path,
            possible_paths.iter()
                .map(|p| format!("  - {:?}", p))
                .collect::<Vec<_>>()
                .join("\n")
        ).into())
    }

    /// Load with default path, fallback to embedded config
    pub fn load_default() -> Result<Self, Box<dyn std::error::Error>> {
        match Self::load("config.toml") {
            Ok(config) => Ok(config),
            Err(e) => {
                tracing::warn!("⚠️  Could not load config.toml: {}", e);
                tracing::warn!("⚠️  Using default embedded configuration");
                Ok(Self::default_config())
            }
        }
    }

    /// Get active network addresses based on mode
    pub fn get_network_addrs(&self) -> &NetworkAddresses {
        match self.network.mode.as_str() {
            "wifi" => &self.network.wifi,
            _ => &self.network.localhost,
        }
    }

    /// Check if in WiFi mode
    pub fn is_wifi_mode(&self) -> bool {
        self.network.mode == "wifi"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_works() {
        let config = Config::default_config();
        assert_eq!(config.network.mode, "localhost");
        assert_eq!(config.sensors.thermal_priority, 3);
    }

    #[test]
    fn test_config_load_or_default() {
        // Should never fail - either loads file or uses default
        let config = Config::load_default().unwrap();
        assert!(config.system.buffer_size > 0);
    }
}