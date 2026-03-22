// benches/benchmarks.rs
// Criterion benchmarks for Satellite OCS
// Run with: cargo bench
// Results saved to: target/criterion/
 
use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use std::time::Duration;
 
// ============================================================
// We re-implement lightweight versions of the core logic here
// so benchmarks don't need the full async Tokio runtime.
// Each benchmark isolates one measurable subsystem.
// ============================================================
 
// ------------------------------------
// Sensor value simulation (from sensors.rs)
// ------------------------------------
fn bench_sensor_reading(sensor_type: u8) -> f64 {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    black_box(match sensor_type {
        0 => rng.gen_range(20.0..85.0),   // Thermal
        1 => rng.gen_range(-180.0..180.0), // Attitude
        _ => rng.gen_range(10.0..14.0),   // Power
    })
}
 
// ------------------------------------
// Packet serialization (from protocol.rs)
// ------------------------------------
fn bench_serialize_telemetry() -> Vec<u8> {
    use serde_json;
    use serde::{Serialize, Deserialize};
    use chrono::Utc;
 
    #[derive(Serialize, Deserialize)]
    struct MockPacket {
        packet_id: u64,
        timestamp: String,
        sensor_id: u8,
        value: f64,
        priority: u8,
    }
 
    let packet = MockPacket {
        packet_id: black_box(42),
        timestamp: Utc::now().to_rfc3339(),
        sensor_id: 1,
        value: black_box(72.5),
        priority: 3,
    };
 
    serde_json::to_vec(&packet).unwrap()
}
 
fn bench_deserialize_telemetry(bytes: &[u8]) {
    use serde_json;
    use serde::{Serialize, Deserialize};
 
    #[derive(Serialize, Deserialize)]
    struct MockPacket {
        packet_id: u64,
        timestamp: String,
        sensor_id: u8,
        value: f64,
        priority: u8,
    }
 
    let _: MockPacket = serde_json::from_slice(black_box(bytes)).unwrap();
}
 
// ------------------------------------
// Jitter calculation (from sensors.rs)
// ------------------------------------
fn bench_jitter_calculation(
    actual_ms: f64,
    expected_ms: f64,
) -> f64 {
    black_box((actual_ms - expected_ms).abs())
}
 
// ------------------------------------
// Metrics recording (from metrics.rs)
// ------------------------------------
struct MockMetrics {
    max_jitter_ms: f64,
    jitter_samples: Vec<f64>,
    max_drift_ms: f64,
    missed_deadlines: u64,
}
 
impl MockMetrics {
    fn new() -> Self {
        Self {
            max_jitter_ms: 0.0,
            jitter_samples: Vec::with_capacity(100),
            max_drift_ms: 0.0,
            missed_deadlines: 0,
        }
    }
 
    fn record_jitter(&mut self, jitter_ms: f64) {
        if jitter_ms > self.max_jitter_ms {
            self.max_jitter_ms = jitter_ms;
        }
        self.jitter_samples.push(jitter_ms);
        if self.jitter_samples.len() > 100 {
            self.jitter_samples.remove(0);
        }
    }
 
    fn avg_jitter(&self) -> f64 {
        if self.jitter_samples.is_empty() {
            return 0.0;
        }
        self.jitter_samples.iter().sum::<f64>() / self.jitter_samples.len() as f64
    }
}
 
// ------------------------------------
// Deadline check (from scheduler.rs)
// ------------------------------------
fn bench_deadline_check(execution_ms: f64, deadline_ms: f64) -> bool {
    black_box(execution_ms > deadline_ms)
}
 
// ============================================================
// BENCHMARK GROUPS
// ============================================================
 
fn benchmark_sensor_reading(c: &mut Criterion) {
    let mut group = c.benchmark_group("sensor_reading");
    group.measurement_time(Duration::from_secs(5));
 
    for (id, sensor_type) in [("thermal", 0u8), ("attitude", 1u8), ("power", 2u8)] {
        group.bench_with_input(
            BenchmarkId::new("read_value", id),
            &sensor_type,
            |b, &st| b.iter(|| bench_sensor_reading(st)),
        );
    }
 
    group.finish();
}
 
fn benchmark_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("packet_serialization");
    group.measurement_time(Duration::from_secs(5));
 
    group.bench_function("serialize_telemetry", |b| {
        b.iter(|| bench_serialize_telemetry())
    });
 
    // Pre-serialize once, then benchmark deserialization
    let bytes = bench_serialize_telemetry();
    group.bench_function("deserialize_telemetry", |b| {
        b.iter(|| bench_deserialize_telemetry(black_box(&bytes)))
    });
 
    // Measure packet size
    let size = bytes.len();
    println!("\n  Telemetry packet size: {} bytes", size);
 
    group.finish();
}
 
fn benchmark_jitter_calculation(c: &mut Criterion) {
    let mut group = c.benchmark_group("jitter");
    group.measurement_time(Duration::from_secs(5));
 
    // Simulate typical jitter values seen in logs
    for (label, actual, expected) in [
        ("low_jitter",    100.3_f64, 100.0_f64),
        ("medium_jitter", 100.8,     100.0),
        ("high_jitter",   101.5,     100.0),
    ] {
        group.bench_with_input(
            BenchmarkId::new("calculate", label),
            &(actual, expected),
            |b, &(a, e)| b.iter(|| bench_jitter_calculation(a, e)),
        );
    }
 
    group.finish();
}
 
fn benchmark_metrics_recording(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics");
    group.measurement_time(Duration::from_secs(5));
 
    group.bench_function("record_jitter_sample", |b| {
        let mut metrics = MockMetrics::new();
        b.iter(|| metrics.record_jitter(black_box(0.42)))
    });
 
    group.bench_function("avg_jitter_100_samples", |b| {
        let mut metrics = MockMetrics::new();
        for i in 0..100 {
            metrics.record_jitter(i as f64 * 0.01);
        }
        b.iter(|| metrics.avg_jitter())
    });
 
    group.finish();
}
 
fn benchmark_deadline_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("deadline_check");
    group.measurement_time(Duration::from_secs(5));
 
    // Benchmark deadline check for each task type
    for (task, exec_ms, deadline_ms) in [
        ("thermal_pass",       2.0_f64, 20.0_f64),
        ("thermal_miss",       22.0,    20.0),
        ("compression_pass",   12.0,    25.0),
        ("compression_miss",   27.0,    25.0),
        ("antenna_pass",       18.0,    30.0),
        ("health_pass",        10.0,    40.0),
    ] {
        group.bench_with_input(
            BenchmarkId::new("check", task),
            &(exec_ms, deadline_ms),
            |b, &(e, d)| b.iter(|| bench_deadline_check(e, d)),
        );
    }
 
    group.finish();
}
 
fn benchmark_scheduler_utilization(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_utilization");
    group.measurement_time(Duration::from_secs(3));
 
    // Benchmark the utilization calculation itself
    group.bench_function("calculate_rm_utilization", |b| {
        b.iter(|| {
            // Matches your actual task set
            let tasks: &[(u64, u64)] = &[
                (1, 50),    // ThermalControl:   1ms / 50ms
                (3, 200),   // DataCompression:  3ms / 200ms
                (5, 500),   // AntennaAlignment: 5ms / 500ms
                (8, 1000),  // HealthMonitoring: 8ms / 1000ms
            ];
            let utilization: f64 = tasks
                .iter()
                .map(|(exec, period)| *exec as f64 / *period as f64)
                .sum();
            black_box(utilization)
        })
    });
 
    // Verify RM bound: U <= n(2^(1/n) - 1), n=4 => 0.757
    group.bench_function("check_rm_schedulability_bound", |b| {
        b.iter(|| {
            let utilization = black_box(0.053_f64); // your actual utilization
            let n = 4.0_f64;
            let rm_bound = n * (2.0_f64.powf(1.0 / n) - 1.0);
            black_box(utilization <= rm_bound)
        })
    });
 
    group.finish();
}
 
fn benchmark_buffer_operations(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer");
    group.measurement_time(Duration::from_secs(5));
 
    // Simulate buffer fill percentage calculation
    group.bench_function("calculate_fill_percent", |b| {
        b.iter(|| {
            let current_len = black_box(42usize);
            let capacity    = black_box(100usize);
            let fill = (current_len as f32 / capacity as f32) * 100.0;
            black_box(fill)
        })
    });
 
    // Simulate degraded mode threshold check
    group.bench_function("degraded_mode_check", |b| {
        b.iter(|| {
            let fill      = black_box(82.0_f32);
            let threshold = black_box(80.0_f32);
            black_box(fill > threshold)
        })
    });
 
    group.finish();
}
 
// ============================================================
// REGISTER ALL BENCHMARKS
// ============================================================
 
criterion_group!(
    benches,
    benchmark_sensor_reading,
    benchmark_serialization,
    benchmark_jitter_calculation,
    benchmark_metrics_recording,
    benchmark_deadline_check,
    benchmark_scheduler_utilization,
    benchmark_buffer_operations,
);
 
criterion_main!(benches);