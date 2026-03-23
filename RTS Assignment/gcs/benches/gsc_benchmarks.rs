// benches/gcs_benchmarks.rs
// Criterion benchmarks for GCS deadline verification
//
// Run:               cargo bench
// View HTML report:  target/criterion/report/index.html

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

// ── Inline protocol types ─────────────────────────────────────────────────────
// Duplicated here so the bench compiles without pulling in the full GCS crate.
// These must stay identical to src/protocol.rs.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelemetryPacket {
    packet_id: u64,
    timestamp: DateTime<Utc>,
    payload:   TelemetryPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum TelemetryPayload {
    SensorData   { sensor_id: u8, value: f64, priority: u8 },
    HealthStatus { cpu_utilization: f32, buffer_fill_percentage: f32 },
    FaultAlert   { description: String },
    CommandAck   { command_id: u64, success: bool, execution_time_ms: f64, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommandPacket {
    command_id: u64,
    timestamp:  DateTime<Utc>,
    urgency:    CommandUrgency,
    payload:    CommandPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum CommandUrgency { Routine, Urgent, Emergency }

#[derive(Debug, Clone, Serialize, Deserialize)]
enum CommandPayload {
    HealthCheck,
    SetSensorInterval { sensor_id: u8, interval_ms: u64 },
    SetMode           { mode: String },
    ResetSubsystem    { subsystem: String },
    EmergencyShutdown { reason: String },
}

impl TelemetryPacket {
    fn to_bytes(&self)          -> Vec<u8>       { serde_json::to_vec(self).unwrap() }
    fn from_bytes(b: &[u8])     -> Self          { serde_json::from_slice(b).unwrap() }
}
impl CommandPacket {
    fn to_bytes(&self)          -> Vec<u8>       { serde_json::to_vec(self).unwrap() }
}

// ── Interlock logic (pure, no async) ─────────────────────────────────────────
fn interlock_check_logic(active: bool, urgency: &CommandUrgency) -> bool {
    if matches!(urgency, CommandUrgency::Emergency) { return true; }
    !active
}

// ── Bench 1: Telemetry decode (3ms deadline) ──────────────────────────────────
fn bench_telemetry_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("telemetry_decode");

    // Sensor data — most frequent packet type
    let sensor_pkt = TelemetryPacket {
        packet_id: 1,
        timestamp: Utc::now(),
        payload:   TelemetryPayload::SensorData { sensor_id: 1, value: 67.4, priority: 3 },
    };
    let sensor_bytes = sensor_pkt.to_bytes();

    group.bench_function("sensor_data", |b| {
        b.iter(|| {
            let p = TelemetryPacket::from_bytes(black_box(&sensor_bytes));
            black_box(p);
        })
    });

    // Fault alert — larger payload, worst case for decode time
    let fault_pkt = TelemetryPacket {
        packet_id: 2,
        timestamp: Utc::now(),
        payload:   TelemetryPayload::FaultAlert {
            description: "Thermal sensor missed 3 consecutive cycles — safety alert triggered".into(),
        },
    };
    let fault_bytes = fault_pkt.to_bytes();

    group.bench_function("fault_alert", |b| {
        b.iter(|| {
            let p = TelemetryPacket::from_bytes(black_box(&fault_bytes));
            black_box(p);
        })
    });

    // Health status
    let health_pkt = TelemetryPacket {
        packet_id: 3,
        timestamp: Utc::now(),
        payload:   TelemetryPayload::HealthStatus {
            cpu_utilization: 35.2,
            buffer_fill_percentage: 12.5,
        },
    };
    let health_bytes = health_pkt.to_bytes();

    group.bench_function("health_status", |b| {
        b.iter(|| {
            let p = TelemetryPacket::from_bytes(black_box(&health_bytes));
            black_box(p);
        })
    });

    group.finish();
}

// ── Bench 2: Command serialize (contributes to 2ms dispatch deadline) ─────────
fn bench_command_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("command_serialize");

    let cases: &[(&str, CommandUrgency, CommandPayload)] = &[
        ("routine_healthcheck",
            CommandUrgency::Routine,
            CommandPayload::HealthCheck),
        ("urgent_set_interval",
            CommandUrgency::Urgent,
            CommandPayload::SetSensorInterval { sensor_id: 1, interval_ms: 50 }),
        ("urgent_set_mode",
            CommandUrgency::Urgent,
            CommandPayload::SetMode { mode: "Degraded".into() }),
        ("emergency_shutdown",
            CommandUrgency::Emergency,
            CommandPayload::EmergencyShutdown { reason: "Thermal runaway detected".into() }),
    ];

    for (name, urgency, payload) in cases {
        let cmd = CommandPacket {
            command_id: 1,
            timestamp:  Utc::now(),
            urgency:    urgency.clone(),
            payload:    payload.clone(),
        };
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &cmd,
            |b, c| b.iter(|| { let v = c.to_bytes(); black_box(v); }),
        );
    }

    group.finish();
}

// ── Bench 3: Interlock check logic (pure computation) ────────────────────────
fn bench_interlock_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("interlock_check");

    // Case: interlock inactive — approved
    group.bench_function("approved_routine", |b| {
        b.iter(|| {
            let result = interlock_check_logic(black_box(false), black_box(&CommandUrgency::Routine));
            black_box(result);
        })
    });

    // Case: interlock active, routine — blocked
    group.bench_function("blocked_routine", |b| {
        b.iter(|| {
            let result = interlock_check_logic(black_box(true), black_box(&CommandUrgency::Routine));
            black_box(result);
        })
    });

    // Case: interlock active, emergency — bypasses
    group.bench_function("emergency_bypass", |b| {
        b.iter(|| {
            let result = interlock_check_logic(black_box(true), black_box(&CommandUrgency::Emergency));
            black_box(result);
        })
    });

    group.finish();
}

// ── Bench 4: Metrics recording (field writes under no contention) ─────────────
fn bench_metrics_record(c: &mut Criterion) {
    struct M {
        total: f64, min: f64, max: f64, n: u64,
    }
    impl M {
        fn record(&mut self, v: f64) {
            if v < self.min { self.min = v; }
            if v > self.max { self.max = v; }
            self.total += v;
            self.n += 1;
        }
    }
    let mut m = M { total: 0.0, min: f64::MAX, max: 0.0, n: 0 };

    c.bench_function("metrics_record_single", |b| {
        b.iter(|| m.record(black_box(0.75)))
    });
}

criterion_group!(
    benches,
    bench_telemetry_decode,
    bench_command_serialize,
    bench_interlock_check,
    bench_metrics_record,
);
criterion_main!(benches);