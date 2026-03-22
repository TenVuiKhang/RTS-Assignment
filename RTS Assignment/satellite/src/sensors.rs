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

#[derive(Debug, Clone)]
pub struct SensorReading {
    pub sensor_id: u8,
    pub sensor_type: SensorType,
    pub value: f64,
    pub timestamp: Instant,
    pub priority: u8,
}

/// Simulate sensor value reading (Lab 2: Stack allocation, no heap)
fn read_sensor_value(sensor_type: &SensorType) -> f64 {
    use rand::Rng;
    let mut rng = rand::thread_rng();

    // Use black_box to prevent compiler optimization (Lab 2)
    black_box(match sensor_type {
        SensorType::Thermal => {
            // Temperature in Celsius (20-85°C)
            rng.gen_range(20.0..85.0)
        }
        SensorType::Attitude => {
            // Angle in degrees (-180 to 180)
            rng.gen_range(-180.0..180.0)
        }
        SensorType::Power => {
            // Voltage (10-14V)
            rng.gen_range(10.0..14.0)
        }
    })
}

/// Spawn sensor task (Lab 1, 6, 8: Jitter, resilient loops, timing)
pub fn spawn_sensor_task(
    sensor_id: u8,
    sensor_type: SensorType,
    interval_ms: u64,
    priority: u8,
    tx: mpsc::Sender<SensorReading>,
    metrics: Arc<Mutex<Metrics>>,
    fault_injection_every: u64,
    jitter_threshold_ms: f64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker             = interval(Duration::from_millis(interval_ms));
        let mut expected_time      = Instant::now();
        let mut iteration          = 0u64;
        let mut consecutive_failures = 0u8;
        // Time-based fault injection: inject every 60 seconds
        let mut last_fault_time    = Instant::now();
        let fault_interval         = Duration::from_secs(60);

        info!(
            "🛰️  Sensor {} ({:?}) started: interval={}ms, priority={}",
            sensor_id, sensor_type, interval_ms, priority
        );

        loop {
            let tick_time = ticker.tick().await;
            iteration += 1;

            // ============================================================
            // FAULT INJECTION CHECK (time-based: every 60 seconds)
            // Must be checked BEFORE jitter measurement so we can reset
            // expected_time cleanly and avoid fake jitter spikes.
            // ============================================================
            let should_inject_fault = last_fault_time.elapsed() >= fault_interval;

            if should_inject_fault {
                last_fault_time = Instant::now();

                warn!("💥 FAULT INJECTION: Sensor {} simulated failure", sensor_id);
                metrics.lock().await.fault_count += 1;
                consecutive_failures += 1;

                // Lab 6: Resilient loop - recover after 3 consecutive failures
                if consecutive_failures >= 3 {
                    error!(
                        "🔄 Sensor {} entering recovery mode after {} consecutive failures",
                        sensor_id, consecutive_failures
                    );

                    let recovery_start = std::time::Instant::now();
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let recovery_ms = recovery_start.elapsed().as_millis();

                    if recovery_ms > 200 {
                        error!(
                            "🚨 MISSION ABORT: Sensor {} recovery {}ms exceeded 200ms limit",
                            sensor_id, recovery_ms
                        );
                    } else {
                        info!(
                            "✅ Sensor {} recovered in {}ms",
                            sensor_id, recovery_ms
                        );
                    }

                    consecutive_failures = 0;
                }

                // Reset expected_time to current tick + one interval so the
                // NEXT tick measures jitter cleanly from this point forward.
                // Without this, the next tick sees jitter = recovery_sleep_time
                // which is not real scheduling jitter — it's intentional delay.
                expected_time = tick_time + Duration::from_millis(interval_ms);
                continue; // Skip this reading
            }

            // Reset consecutive failures on any successful (non-fault) tick
            consecutive_failures = 0;

            // ============================================================
            // JITTER MEASUREMENT (Lab 1: OS scheduling unpredictability)
            // Only measured on normal ticks — fault/recovery ticks are
            // excluded because their delay is intentional, not OS jitter.
            // ============================================================
            let jitter = if tick_time > expected_time {
                tick_time.duration_since(expected_time)
            } else {
                expected_time.duration_since(tick_time)
            };
            let jitter_ms = jitter.as_secs_f64() * 1000.0;

            // Record jitter
            {
                let mut m = metrics.lock().await;
                m.record_jitter(jitter_ms);
            }

            // Check critical jitter threshold (Lab 1: <1ms for thermal)
            if matches!(sensor_type, SensorType::Thermal) && jitter_ms > jitter_threshold_ms {
                warn!(
                    "⚠️  Sensor {} CRITICAL JITTER: {:.3}ms exceeds {:.3}ms threshold",
                    sensor_id, jitter_ms, jitter_threshold_ms
                );
            }

            // Advance expected time by one interval for next tick
            expected_time = tick_time + Duration::from_millis(interval_ms);

            // ============================================================
            // SENSOR READING
            // ============================================================
            let reading = SensorReading {
                sensor_id,
                sensor_type: sensor_type.clone(),
                value: read_sensor_value(&sensor_type),
                timestamp: tick_time,
                priority,
            };

            // ============================================================
            // LATENCY TRACKING (Sensor → Buffer)
            // ============================================================
            let send_start = Instant::now();

            match tx.try_send(reading.clone()) {
                Ok(_) => {
                    let latency    = send_start.elapsed();
                    let latency_ms = latency.as_secs_f64() * 1000.0;

                    {
                        let mut m = metrics.lock().await;
                        m.total_readings += 1;
                        m.record_latency(latency_ms);

                        // Reset consecutive thermal misses on successful send
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
                    // BUFFER OVERFLOW - Data loss!
                    error!(
                        "❌ Sensor {} DATA DROPPED - buffer full at {}",
                        sensor_id,
                        chrono::Utc::now().format("%H:%M:%S%.3f")
                    );

                    let mut m = metrics.lock().await;
                    m.dropped_readings += 1;
                    m.buffer_fill_percentage = 100.0;

                    // SAFETY ALERT: Critical thermal data missed?
                    if matches!(sensor_type, SensorType::Thermal) {
                        m.consecutive_thermal_misses += 1;

                        if m.consecutive_thermal_misses >= 3 {
                            error!(
                                "🚨 SAFETY ALERT: Thermal data missed {} consecutive cycles!",
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
            sensor_id: reading.sensor_id,
            sensor_type: reading.sensor_type.clone(),
            value: reading.value,
            priority: reading.priority,
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
            assert!(thermal >= 20.0 && thermal <= 85.0);

            let attitude = read_sensor_value(&SensorType::Attitude);
            assert!(attitude >= -180.0 && attitude <= 180.0);

            let power = read_sensor_value(&SensorType::Power);
            assert!(power >= 10.0 && power <= 14.0);
        }
    }
}