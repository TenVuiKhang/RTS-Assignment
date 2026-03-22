// benches/gcs_benchmarks.rs
// Criterion benchmarks for the GCS  (Assignment §GCS-4 + Presentation 10%)
//
// Run with:  cargo bench
//
// Measures:
//   1. Telemetry decode latency           (must stay < 3 ms)
//   2. Command serialize + dispatch time  (Urgent must stay < 2 ms)
//   3. Fault detection to interlock time
//   4. Metrics recording throughput

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use std::hint;

// ── Inline the structs we need so the bench compiles independently ────────────
// In your real project, just add   gcs = { path = ".." }  to [dev-dependencies]
// and use   use gcs::protocol::*;

use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelemetryPacket {
    packet_id: u64,
    timestamp: DateTime<Utc>,
    payload: TelemetryPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum TelemetryPayload {
    SensorData { sensor_id: u8, value: f64, priority: u8 },
    FaultAlert  { description: String },
    HealthStatus { cpu_utilization: f32 },
    CommandAck   { command_id: u64, success: bool, execution_time_ms: f64, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommandPacket {
    command_id: u64,
    timestamp: DateTime<Utc>,
    urgency: CommandUrgency,
    payload: CommandPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum CommandUrgency { Routine, Urgent, Emergency }

#[derive(Debug, Clone, Serialize, Deserialize)]
enum CommandPayload {
    HealthCheck,
    SetMode { mode: String },
    EmergencyShutdown { reason: String },
}

impl TelemetryPacket {
    fn to_bytes(&self) -> Vec<u8> { serde_json::to_vec(self).unwrap() }
    fn from_bytes(b: &[u8]) -> Self { serde_json::from_slice(b).unwrap() }
}
impl CommandPacket {
    fn to_bytes(&self) -> Vec<u8> { serde_json::to_vec(self).unwrap() }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Bench 1: Telemetry decode (simulate the 3 ms deadline path)
fn bench_telemetry_decode(c: &mut Criterion) {
    let packet = TelemetryPacket {
        packet_id: 42,
        timestamp: Utc::now(),
        payload: TelemetryPayload::SensorData { sensor_id: 1, value: 67.4, priority: 3 },
    };
    let bytes = packet.to_bytes();

    let mut group = c.benchmark_group("telemetry_decode");
    group.measurement_time(std::time::Duration::from_secs(5));

    group.bench_function("sensor_data_packet", |b| {
        b.iter(|| {
            let pkt = TelemetryPacket::from_bytes(black_box(&bytes));
            hint::black_box(pkt);
        })
    });

    // Also bench a fault-alert packet (larger payload)
    let fault_pkt = TelemetryPacket {
        packet_id: 99,
        timestamp: Utc::now(),
        payload: TelemetryPayload::FaultAlert {
            description: "Thermal sensor failed — consecutive misses: 3".to_string(),
        },
    };
    let fault_bytes = fault_pkt.to_bytes();

    group.bench_function("fault_alert_packet", |b| {
        b.iter(|| {
            let pkt = TelemetryPacket::from_bytes(black_box(&fault_bytes));
            hint::black_box(pkt);
        })
    });

    group.finish();
}

/// Bench 2: Command serialization (before UDP send — measures our contribution to dispatch latency)
fn bench_command_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("command_dispatch");

    for urgency in [("routine", CommandUrgency::Routine), ("urgent", CommandUrgency::Urgent)] {
        let cmd = CommandPacket {
            command_id: 1,
            timestamp: Utc::now(),
            urgency: urgency.1,
            payload: CommandPayload::HealthCheck,
        };

        group.bench_with_input(
            BenchmarkId::new("serialize", urgency.0),
            &cmd,
            |b, c| { b.iter(|| { let v = c.to_bytes(); hint::black_box(v); }) },
        );
    }

    // Emergency shutdown (larger payload)
    let emergency = CommandPacket {
        command_id: 999,
        timestamp: Utc::now(),
        urgency: CommandUrgency::Emergency,
        payload: CommandPayload::EmergencyShutdown { reason: "Thermal runaway detected".to_string() },
    };
    group.bench_with_input(
        BenchmarkId::new("serialize", "emergency"),
        &emergency,
        |b, c| { b.iter(|| { let v = c.to_bytes(); hint::black_box(v); }) },
    );

    group.finish();
}

/// Bench 3: Fault detection logic (pure computation — no I/O)
fn bench_fault_detection(c: &mut Criterion) {
    // Simulate the fault_manager match arm classification
    let payloads = vec![
        TelemetryPayload::FaultAlert { description: "thermal anomaly".to_string() },
        TelemetryPayload::FaultAlert { description: "buffer overflow".to_string() },
        TelemetryPayload::HealthStatus { cpu_utilization: 35.0 },
    ];

    c.bench_function("fault_classification", |b| {
        b.iter(|| {
            for p in &payloads {
                let response = match p {
                    TelemetryPayload::FaultAlert { description } => {
                        if description.contains("thermal") { "SetSensorInterval" }
                        else if description.contains("buffer") { "SetMode" }
                        else { "HealthCheck" }
                    }
                    TelemetryPayload::HealthStatus { .. } => "none",
                    _ => "ignore",
                };
                hint::black_box(response);
            }
        })
    });
}

/// Bench 4: Metrics struct updates under contention pressure (no lock — pure field writes)
fn bench_metrics_update(c: &mut Criterion) {
    // Simplified metrics struct for the bench
    struct M { pkts: u64, drift_total: f64, drift_max: f64, drift_n: u64 }
    impl M {
        fn record(&mut self, drift: f64) {
            self.pkts += 1;
            if drift > self.drift_max { self.drift_max = drift; }
            self.drift_total += drift;
            self.drift_n += 1;
        }
    }
    let mut m = M { pkts: 0, drift_total: 0.0, drift_max: 0.0, drift_n: 0 };

    c.bench_function("metrics_record_drift", |b| {
        b.iter(|| m.record(black_box(0.75)))
    });
}

criterion_group!(
    benches,
    bench_telemetry_decode,
    bench_command_serialize,
    bench_fault_detection,
    bench_metrics_update,
);
criterion_main!(benches);