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
    Delayed,    // reading skipped - simulates sensor timeout
    Corrupted,  // out-of-range value sent - simulates bad data
}

impl FaultKind {
    fn label(&self) -> &str {
        match self {
            FaultKind::Delayed   => "DELAYED   (reading skipped)",
            FaultKind::Corrupted => "CORRUPTED (bad value injected)",
        }
    }
}

/// Out-of-range corrupt values - obviously wrong so ground can detect them
fn corrupt_value(sensor_type: &SensorType) -> f64 {
    match sensor_type {
        SensorType::Thermal  => 999.9,  // normal: 20-85C
        SensorType::Attitude => 9999.0, // normal: -180 to 180
        SensorType::Power    => -99.9,  // normal: 10-14V
    }
}

// ============================================================
// FAULT BANNER
// ============================================================

/// Prints a visible fault banner atomically so threads never interleave
fn display_fault_banner(
    sensor_id:    u8,
    sensor_type:  &SensorType,
    fault_kind:   &FaultKind,
    fault_number: u64,
    corrupt_val:  Option<f64>,
) {
    let timestamp = Utc::now().format("%H:%M:%S%.3f");
    let value_str = match corrupt_val {
        Some(v) => format!("{:.1} (out of valid range)", v),
        None    => "N/A (reading skipped)".to_string(),
    };

    let _lock = crate::print_lock::acquire();
    eprintln!(
"\n+------------------------------------------------------------+\
\n|  !! FAULT INJECTION #{}  --  Sensor {} ({:?})\
\n+------------------------------------------------------------+\
\n|  Fault Type   : {}\
\n|  Inject Value : {}\
\n|  Timestamp    : {}\
\n+------------------------------------------------------------+\n",
        fault_number, sensor_id, sensor_type,
        fault_kind.label(),
        value_str,
        timestamp,
    );
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
    black_box(match sensor_type {
        SensorType::Thermal  => rng.gen_range(20.0..85.0),
        SensorType::Attitude => rng.gen_range(-180.0..180.0),
        SensorType::Power    => rng.gen_range(10.0..14.0),
    })
}

// ============================================================
// SENSOR TASK
// ============================================================

/// Spawn a periodic sensor task with jitter measurement and latency tracking
pub fn spawn_sensor_task(
    sensor_id:             u8,
    sensor_type:           SensorType,
    interval_ms:           u64,
    priority:              u8,
    tx:                    mpsc::Sender<SensorReading>,
    metrics:               Arc<Mutex<Metrics>>,
    _fault_injection_every: u64, // unused: faults handled by spawn_fault_injector
    jitter_threshold_ms:   f64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker        = interval(Duration::from_millis(interval_ms));
        let mut expected_time = Instant::now();

        info!(
            "Sensor {} ({:?}) started: interval={}ms, priority={}",
            sensor_id, sensor_type, interval_ms, priority
        );

        loop {
            // Capture the Tokio scheduled tick (used only for advancing expected_time)
            let tick_time = ticker.tick().await;

            // Capture actual wall-clock wake time immediately - this reflects
            // real OS scheduling delay. Tokio's tick_time is the *scheduled*
            // instant, so tick_time ≈ expected_time always, giving false 0ms jitter.
            let actual_wake = Instant::now();

            // ============================================================
            // JITTER MEASUREMENT (Lab 1: OS scheduling unpredictability)
            // Jitter = actual wake time − scheduled wake time
            // ============================================================
            let jitter_ms = if actual_wake > expected_time {
                actual_wake.duration_since(expected_time).as_secs_f64() * 1000.0
            } else {
                0.0 // woke early (rare) - count as no jitter
            };

            // Record jitter immediately - single lock, no other work inside
            metrics.lock().await.record_jitter(jitter_ms);

            if matches!(sensor_type, SensorType::Thermal) && jitter_ms > jitter_threshold_ms {
                warn!(
                    "Sensor {} CRITICAL JITTER: {:.3}ms exceeds {:.3}ms threshold",
                    sensor_id, jitter_ms, jitter_threshold_ms
                );
            }

            expected_time = tick_time + Duration::from_millis(interval_ms);

            // ============================================================
            // SENSOR READING
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
                        // Single lock covers all post-send metric updates
                        let mut m = metrics.lock().await;
                        m.total_readings += 1;
                        m.record_latency(latency_ms);
                        if matches!(sensor_type, SensorType::Thermal) {
                            m.consecutive_thermal_misses = 0;
                        }

                        // ====================================================
                        // FAULT RECOVERY - clear interlock and measure time
                        // ====================================================
                        if m.interlock_active {
                            if let Some(inject_time) = m.fault_inject_time.take() {
                                let recovery_ms = inject_time.elapsed().as_secs_f64() * 1000.0;
                                let within_limit = m.record_recovery(recovery_ms);
                                m.interlock_active = false;
                                m.interlock_reason = String::new();

                                if within_limit {
                                    info!(
                                        "[OK] Sensor {} fault recovered in {:.1}ms (limit: 200ms)",
                                        sensor_id, recovery_ms
                                    );
                                } else {
                                    error!(
                                        "[ALERT] MISSION ABORT: Sensor {} recovery took {:.1}ms - exceeds 200ms limit!",
                                        sensor_id, recovery_ms
                                    );
                                }
                            }
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
                    m.dropped_readings      += 1;
                    m.buffer_fill_percentage = 100.0;

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

// ============================================================
// FAULT INJECTOR TASK
// ============================================================

/// Picks ONE random sensor every 60 seconds and injects a fault.
/// Alternates between Delayed and Corrupted each time.
/// Waits 60 seconds before the first fault so startup is clean.
pub fn spawn_fault_injector(
    tx_thermal:  mpsc::Sender<SensorReading>,
    tx_attitude: mpsc::Sender<SensorReading>,
    tx_power:    mpsc::Sender<SensorReading>,
    metrics:     Arc<Mutex<Metrics>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use rand::Rng;

        // Grace period - no faults at startup
        tokio::time::sleep(Duration::from_secs(60)).await;

        let mut fault_counter     = 0u64;
        let mut next_is_corrupted = false;

        loop {
            let chosen = rand::thread_rng().gen_range(1u8..=3u8);

            let (sensor_type, tx) = match chosen {
                1 => (SensorType::Thermal,  &tx_thermal),
                2 => (SensorType::Attitude, &tx_attitude),
                _ => (SensorType::Power,    &tx_power),
            };

            fault_counter += 1;
            let fault_time = Utc::now().format("%H:%M:%S%.3f").to_string();

            if next_is_corrupted {
                let bad_value = corrupt_value(&sensor_type);
                display_fault_banner(chosen, &sensor_type, &FaultKind::Corrupted, fault_counter, Some(bad_value));

                {
                    let mut m = metrics.lock().await;
                    m.fault_count += 1;
                    let desc = format!(
                        "CORRUPTED | Sensor {} ({:?}) | injected value: {:.1}",
                        chosen, sensor_type, bad_value
                    );
                    m.fault_log.push(format!("[{}] #{:>3} {}", fault_time, fault_counter, desc));
                    m.last_fault_description = desc;
                    m.last_fault_time = fault_time;
                    m.interlock_active = true;
                    m.interlock_reason = format!(
                        "Corrupted sensor {} ({:?}) data detected", chosen, sensor_type
                    );
                    m.fault_inject_time = Some(std::time::Instant::now());
                }

                let corrupted = SensorReading {
                    sensor_id:   chosen,
                    sensor_type: sensor_type.clone(),
                    value:       bad_value,
                    timestamp:   Instant::now(),
                    priority:    match chosen { 1 => 3, 2 => 2, _ => 1 },
                };
                let _ = tx.try_send(corrupted);
            } else {
                display_fault_banner(chosen, &sensor_type, &FaultKind::Delayed, fault_counter, None);

                {
                    let mut m = metrics.lock().await;
                    m.fault_count += 1;
                    let desc = format!(
                        "DELAYED   | Sensor {} ({:?}) | reading skipped",
                        chosen, sensor_type
                    );
                    m.fault_log.push(format!("[{}] #{:>3} {}", fault_time, fault_counter, desc));
                    m.last_fault_description = desc;
                    m.last_fault_time = fault_time;
                    m.interlock_active = true;
                    m.interlock_reason = format!(
                        "Delayed/missing sensor {} ({:?}) data", chosen, sensor_type
                    );
                    m.fault_inject_time = Some(std::time::Instant::now());
                }
            }

            next_is_corrupted = !next_is_corrupted;

            // Exactly one fault per 60 seconds
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    })
}

// ============================================================
// TESTS
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sensor_value_ranges() {
        for _ in 0..100 {
            let v = read_sensor_value(&SensorType::Thermal);
            assert!(v >= 20.0 && v <= 85.0, "Thermal out of range: {}", v);

            let v = read_sensor_value(&SensorType::Attitude);
            assert!(v >= -180.0 && v <= 180.0, "Attitude out of range: {}", v);

            let v = read_sensor_value(&SensorType::Power);
            assert!(v >= 10.0 && v <= 14.0, "Power out of range: {}", v);
        }
    }

    #[test]
    fn test_corrupt_values_are_out_of_range() {
        assert!(corrupt_value(&SensorType::Thermal)      >  85.0);
        assert!(corrupt_value(&SensorType::Attitude).abs() > 180.0);
        assert!(corrupt_value(&SensorType::Power)        < 10.0);
    }
}