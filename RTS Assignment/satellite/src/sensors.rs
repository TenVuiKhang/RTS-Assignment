// src/sensors.rs
// Sensor simulation and data acquisition

use crate::protocol::{SensorType, TelemetryPacket, TelemetryPayload};
use crate::metrics::Metrics;
use tokio::time::{interval, Duration, Instant};
use tokio::sync::mpsc;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn, error, debug};
use chrono::Utc;
use std::hint::black_box; // Lab 2: Prevent optimization

// ============================================================
// FAULT TYPES
// ============================================================

#[derive(Debug)]
enum FaultKind {
    Delayed,    // reading skipped entirely — simulates sensor timeout
    Corrupted,  // reading sent with out-of-range value — simulates bad data
}

impl FaultKind {
    fn label(&self) -> &str {
        match self {
            FaultKind::Delayed   => "DELAYED   (reading skipped)",
            FaultKind::Corrupted => "CORRUPTED (bad value injected)",
        }
    }
}

/// Out-of-range corrupt values — obviously wrong so ground can detect them
fn corrupt_value(sensor_type: &SensorType) -> f64 {
    match sensor_type {
        SensorType::Thermal  => 999.9,   // normal: 20-85C,    corrupt: 999.9C
        SensorType::Attitude => 9999.0,  // normal: -180-180,  corrupt: 9999
        SensorType::Power    => -99.9,   // normal: 10-14V,    corrupt: -99.9V
    }
}

// ============================================================
// SENSOR READING STRUCT
// ============================================================

#[derive(Debug, Clone)]
pub struct SensorReading {
    pub sensor_id:   u8,
    pub sensor_type: SensorType,
    pub value:       f64,
    pub timestamp:   Instant,
    pub priority:    u8,
}

/// Simulate sensor value reading (Lab 2: Stack allocation, no heap)
fn read_sensor_value(sensor_type: &SensorType) -> f64 {
    use rand::Rng;
    let mut rng = rand::thread_rng();

    // Use black_box to prevent compiler optimization (Lab 2)
    black_box(match sensor_type {
        SensorType::Thermal  => rng.gen_range(20.0..85.0),
        SensorType::Attitude => rng.gen_range(-180.0..180.0),
        SensorType::Power    => rng.gen_range(10.0..14.0),
    })
}

// ============================================================
// FAULT BANNER DISPLAY
// ============================================================

/// Prints a clearly visible fault injection report to the terminal.
/// Built as a single string then printed with one eprintln! so
/// multiple sensor threads cannot interleave their output.
fn display_fault_banner(
    sensor_id:    u8,
    sensor_type:  &SensorType,
    fault_kind:   &FaultKind,
    fault_number: u64,
    recovery_ms:  Option<u128>,
    corrupt_val:  Option<f64>,
) {
    let timestamp = Utc::now().format("%H:%M:%S%.3f");

    let recovery_str = match recovery_ms {
        Some(ms) if ms <= 200 => format!("OK  -- recovered in {}ms (limit: 200ms)", ms),
        Some(ms)              => format!("FAIL -- {}ms exceeded 200ms limit!", ms),
        None                  => "N/A".to_string(),
    };

    let value_str = match corrupt_val {
        Some(v) => format!("{:.1} (out of valid range)", v),
        None    => "N/A (reading skipped)".to_string(),
    };

    // Single eprintln! = one atomic write, no interleaving between threads
    eprintln!(
"
+------------------------------------------------------------+
|  !! FAULT INJECTION #{}  --  Sensor {} ({:?})
+------------------------------------------------------------+
|  Fault Type   : {}
|  Inject Value : {}
|  Timestamp    : {}
|  Recovery     : {}
+------------------------------------------------------------+
",
        fault_number, sensor_id, sensor_type,
        fault_kind.label(),
        value_str,
        timestamp,
        recovery_str,
    );
}

// ============================================================
// SENSOR TASK
// ============================================================

/// Spawn sensor task (Lab 1, 6, 8: Jitter, resilient loops, timing)
pub fn spawn_sensor_task(
    sensor_id:             u8,
    sensor_type:           SensorType,
    interval_ms:           u64,
    priority:              u8,
    tx:                    mpsc::Sender<SensorReading>,
    metrics:               Arc<Mutex<Metrics>>,
    fault_injection_every: u64,
    jitter_threshold_ms:   f64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker               = interval(Duration::from_millis(interval_ms));
        let mut expected_time        = Instant::now();
        let mut consecutive_failures = 0u8;
        let mut fault_counter        = 0u64;

        // Time-based fault injection: every 60 seconds
        let mut last_fault_time     = Instant::now();
        let fault_interval          = Duration::from_secs(60);

        // Alternate Delayed / Corrupted each injection
        let mut next_is_corrupted = false;

        info!(
            "Sensor {} ({:?}) started: interval={}ms, priority={}",
            sensor_id, sensor_type, interval_ms, priority
        );

        loop {
            let tick_time = ticker.tick().await;

            // ============================================================
            // FAULT INJECTION (every 60 seconds, time-based)
            // Checked BEFORE jitter so expected_time resets cleanly
            // and the next tick does not record a false jitter spike.
            // ============================================================
            if last_fault_time.elapsed() >= fault_interval {
                last_fault_time   = Instant::now();
                fault_counter    += 1;
                consecutive_failures += 1;

                let fault_kind = if next_is_corrupted {
                    FaultKind::Corrupted
                } else {
                    FaultKind::Delayed
                };
                next_is_corrupted = !next_is_corrupted;

                // Recovery only triggered after 3 consecutive failures
                let recovery_ms: Option<u128> = if consecutive_failures >= 3 {
                    error!(
                        "Sensor {} entering recovery mode after {} consecutive failures",
                        sensor_id, consecutive_failures
                    );
                    let t = std::time::Instant::now();
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let ms = t.elapsed().as_millis();
                    consecutive_failures = 0;

                    if ms > 200 {
                        error!(
                            "MISSION ABORT: Sensor {} recovery {}ms exceeded 200ms limit",
                            sensor_id, ms
                        );
                    }
                    Some(ms)
                } else {
                    None
                };

                match fault_kind {
                    FaultKind::Delayed => {
                        display_fault_banner(
                            sensor_id, &sensor_type,
                            &FaultKind::Delayed,
                            fault_counter, recovery_ms, None,
                        );
                        metrics.lock().await.fault_count += 1;

                        // Reset baseline so next jitter is measured cleanly
                        expected_time = tick_time + Duration::from_millis(interval_ms);
                        continue; // skip this reading
                    }
                    FaultKind::Corrupted => {
                        let bad_value = corrupt_value(&sensor_type);

                        display_fault_banner(
                            sensor_id, &sensor_type,
                            &FaultKind::Corrupted,
                            fault_counter, recovery_ms, Some(bad_value),
                        );
                        metrics.lock().await.fault_count += 1;

                        // Send corrupted reading — ground station should
                        // detect out-of-range value and flag it
                        let corrupted = SensorReading {
                            sensor_id,
                            sensor_type: sensor_type.clone(),
                            value:       bad_value,
                            timestamp:   tick_time,
                            priority,
                        };
                        let _ = tx.try_send(corrupted);

                        expected_time = tick_time + Duration::from_millis(interval_ms);
                        continue;
                    }
                }
            }

            // Reset consecutive failures on any successful tick
            consecutive_failures = 0;

            // ============================================================
            // JITTER MEASUREMENT (Lab 1: OS scheduling unpredictability)
            // Only on normal ticks — fault ticks excluded intentionally
            // ============================================================
            let jitter = if tick_time > expected_time {
                tick_time.duration_since(expected_time)
            } else {
                expected_time.duration_since(tick_time)
            };
            let jitter_ms = jitter.as_secs_f64() * 1000.0;

            {
                let mut m = metrics.lock().await;
                m.record_jitter(jitter_ms);
            }

            if matches!(sensor_type, SensorType::Thermal) && jitter_ms > jitter_threshold_ms {
                warn!(
                    "Sensor {} CRITICAL JITTER: {:.3}ms exceeds {:.3}ms threshold",
                    sensor_id, jitter_ms, jitter_threshold_ms
                );
            }

            expected_time = tick_time + Duration::from_millis(interval_ms);

            // ============================================================
            // NORMAL SENSOR READING
            // ============================================================
            let reading = SensorReading {
                sensor_id,
                sensor_type: sensor_type.clone(),
                value:       read_sensor_value(&sensor_type),
                timestamp:   tick_time,
                priority,
            };

            // ============================================================
            // LATENCY TRACKING (Sensor -> Buffer)
            // ============================================================
            let send_start = Instant::now();

            match tx.try_send(reading.clone()) {
                Ok(_) => {
                    let latency_ms = send_start.elapsed().as_secs_f64() * 1000.0;

                    {
                        let mut m = metrics.lock().await;
                        m.total_readings += 1;
                        m.record_latency(latency_ms);

                        if matches!(sensor_type, SensorType::Thermal) {
                            m.consecutive_thermal_misses = 0;
                        }
                    }

                    debug!(
                        "Sensor {} reading: {:.2} | jitter: {:.3}ms | latency: {:.3}ms",
                        sensor_id, reading.value, jitter_ms, latency_ms
                    );
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    error!(
                        "Sensor {} DATA DROPPED - buffer full at {}",
                        sensor_id,
                        chrono::Utc::now().format("%H:%M:%S%.3f")
                    );

                    let mut m = metrics.lock().await;
                    m.dropped_readings       += 1;
                    m.buffer_fill_percentage  = 100.0;

                    if matches!(sensor_type, SensorType::Thermal) {
                        m.consecutive_thermal_misses += 1;

                        if m.consecutive_thermal_misses >= 3 {
                            error!(
                                "SAFETY ALERT: Thermal data missed {} consecutive cycles!",
                                m.consecutive_thermal_misses
                            );
                        }
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    error!("Sensor {} channel closed - shutting down", sensor_id);
                    break;
                }
            }
        }

        info!("Sensor {} task terminated", sensor_id);
    })
}

/// Convert sensor reading to telemetry packet
pub fn reading_to_telemetry(reading: &SensorReading, packet_id: u64) -> TelemetryPacket {
    TelemetryPacket {
        packet_id,
        timestamp: Utc::now(),
        payload: TelemetryPayload::SensorData {
            sensor_id:   reading.sensor_id,
            sensor_type: reading.sensor_type.clone(),
            value:       reading.value,
            priority:    reading.priority,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sensor_value_ranges() {
        for _ in 0..100 {
            let thermal = read_sensor_value(&SensorType::Thermal);
            assert!(thermal >= 20.0 && thermal <= 85.0,
                "Thermal out of range: {}", thermal);

            let attitude = read_sensor_value(&SensorType::Attitude);
            assert!(attitude >= -180.0 && attitude <= 180.0,
                "Attitude out of range: {}", attitude);

            let power = read_sensor_value(&SensorType::Power);
            assert!(power >= 10.0 && power <= 14.0,
                "Power out of range: {}", power);
        }
    }

    #[test]
    fn test_corrupt_values_are_out_of_range() {
        // Corrupt values must be clearly outside normal operating ranges
        assert!(corrupt_value(&SensorType::Thermal)  > 85.0,   "Thermal corrupt value not high enough");
        assert!(corrupt_value(&SensorType::Attitude).abs() > 180.0, "Attitude corrupt value not out of range");
        assert!(corrupt_value(&SensorType::Power)    < 10.0,   "Power corrupt value not low enough");
    }
}