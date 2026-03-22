mod protocol;   // MUST be identical to OCS protocol.rs
mod metrics;
mod telemetry;
mod commands;
mod faults;

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, error};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .with_thread_ids(true)
        .with_ansi(true)
        .init();

    info!("=================================================");
    info!("  GROUND CONTROL STATION");
    info!("=================================================");
    info!("  Course: CT087-3-3 Real-Time Systems");
    info!("  Component: GCS (Student B)");
    info!("=================================================");

    // Addresses must match OCS config.toml exactly
    let satellite_addr = "127.0.0.1:8001";   // OCS listens here (we send commands TO it)
    let gcs_bind_addr  = "127.0.0.1:8002";   // We listen here (OCS sends telemetry TO us)

    let metrics = Arc::new(Mutex::new(metrics::GcsMetrics::new()));

    // Channel: telemetry receiver → fault manager
    let (telem_tx, telem_rx) = mpsc::channel::<protocol::TelemetryPacket>(256);

    // Channel: command scheduler → uplink sender
    let (cmd_tx, cmd_rx) = mpsc::channel::<protocol::CommandPacket>(64);

    info!("  Starting subsystems...");

    // 1. Telemetry receiver  (binds :8002, decodes within 3 ms)
    let telem_task = telemetry::spawn_receiver(
        gcs_bind_addr,
        telem_tx,
        metrics.clone(),
    );

    // 2. Fault manager  (watches telemetry stream, applies interlocks)
    let fault_task = faults::spawn_fault_manager(
        telem_rx,
        cmd_tx.clone(),
        metrics.clone(),
    );

    // 3. Command uplink  (sends CommandPackets to OCS :8001)
    let uplink_task = commands::spawn_uplink(
        satellite_addr,
        cmd_rx,
        metrics.clone(),
    );

    // 4. Command scheduler  (periodic health-checks + demo commands)
    let scheduler_task = commands::spawn_scheduler(cmd_tx, metrics.clone());

    // 5. Metrics reporter
    let reporter_task = metrics::spawn_reporter(metrics.clone(), 10);

    info!("=================================================");
    info!("  All GCS subsystems started!");
    info!("  Receiving telemetry from: {}", gcs_bind_addr);
    info!("  Sending commands to:      {}", satellite_addr);
    info!("=================================================");
    info!("Press Ctrl+C to shutdown");

    tokio::select! {
        _ = telem_task    => error!("Telemetry receiver terminated"),
        _ = fault_task    => error!("Fault manager terminated"),
        _ = uplink_task   => error!("Command uplink terminated"),
        _ = scheduler_task => error!("Command scheduler terminated"),
        _ = reporter_task  => error!("Metrics reporter terminated"),
        _ = tokio::signal::ctrl_c() => {
            info!("Shutdown signal received");
        }
    }

    let m = metrics.lock().await;
    info!("=================================================");
    info!("  FINAL GCS METRICS:\n{}", m.summary());
    info!("=================================================");

    Ok(())
}