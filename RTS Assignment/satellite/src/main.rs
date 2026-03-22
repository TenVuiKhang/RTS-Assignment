// src/main.rs
// Satellite Onboard Control System - Main Entry Point
// CT087-3-3 Real-Time Systems | Student A | Ten Vui Khang | TP074341

mod protocol;
mod print_lock;
mod config;
mod metrics;
mod sensors;
mod scheduler;
mod communication;

use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, error};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {

    // =========================================================================
    // INITIALIZATION
    // =========================================================================

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .with_thread_ids(true)
        .with_ansi(true)
        .init();

    info!("+==================================================+");
    info!("|      SATELLITE ONBOARD CONTROL SYSTEM            |");
    info!("+==================================================+");
    info!("|  Course  : CT087-3-3 Real-Time Systems           |");
    info!("|  Student : Ten Vui Khang  |  TP074341            |");
    info!("|  Role    : Student A -- Satellite OCS            |");
    info!("+==================================================+");

    // =========================================================================
    // CONFIGURATION
    // =========================================================================

    let config = match config::Config::load_default() {
        Ok(c)  => { info!("Configuration loaded successfully"); c }
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            return Err(e);
        }
    };

    info!("Network mode    : {}", config.network.mode);
    info!("Satellite addr  : {}", config.get_network_addrs().satellite_addr);
    info!("Ground addr     : {}", config.get_network_addrs().ground_addr);
    info!("Buffer size     : {}", config.system.buffer_size);
    info!("Fault interval  : every 60 seconds (random sensor)");

    if config.is_wifi_mode() {
        info!("WiFi mode active - ensure ground station IP is correct!");
    } else {
        info!("Localhost mode  : testing locally");
    }

    // =========================================================================
    // RATE MONOTONIC SCHEDULABILITY CHECK
    // =========================================================================

    info!("+--------------------------------------------------+");
    info!("|  Rate Monotonic Scheduler Analysis               |");
    info!("+--------------------------------------------------+");
    let utilization = scheduler::calculate_utilization();
    info!("Total utilization : {:.1}%", utilization * 100.0);
    if utilization <= 0.757 {
        info!("Schedulable       : YES (within RM bound of 75.7%)");
    } else {
        error!("Schedulable       : NO  (exceeds RM bound!)");
    }
    info!("+--------------------------------------------------+");

    // =========================================================================
    // SHARED STATE
    // =========================================================================

    let metrics = Arc::new(Mutex::new(metrics::Metrics::new()));

    // =========================================================================
    // SENSOR PIPELINE
    // =========================================================================

    info!("Starting subsystems...");

    // Separate senders per sensor so fault injector can target each one
    let (tx_thermal,  rx_thermal)  = mpsc::channel::<sensors::SensorReading>(config.system.buffer_size);
    let (tx_attitude, rx_attitude) = mpsc::channel::<sensors::SensorReading>(config.system.buffer_size);
    let (tx_power,    rx_power)    = mpsc::channel::<sensors::SensorReading>(config.system.buffer_size);

    // Merge all sensor channels into one downlink channel
    let (sensor_tx, sensor_rx) = mpsc::channel::<sensors::SensorReading>(config.system.buffer_size);

    // Forwarder tasks — bridge individual sensor channels to the shared downlink
    let stx1 = sensor_tx.clone();
    tokio::spawn(async move {
        let mut rx = rx_thermal;
        while let Some(r) = rx.recv().await { let _ = stx1.send(r).await; }
    });
    let stx2 = sensor_tx.clone();
    tokio::spawn(async move {
        let mut rx = rx_attitude;
        while let Some(r) = rx.recv().await { let _ = stx2.send(r).await; }
    });
    let stx3 = sensor_tx.clone();
    tokio::spawn(async move {
        let mut rx = rx_power;
        while let Some(r) = rx.recv().await { let _ = stx3.send(r).await; }
    });
    drop(sensor_tx);

    // Spawn sensor tasks
    let thermal_task = sensors::spawn_sensor_task(
        1, protocol::SensorType::Thermal,
        config.sensors.thermal_interval_ms,
        config.sensors.thermal_priority,
        tx_thermal.clone(),
        metrics.clone(),
        0, // fault timing handled by fault injector
        config.system.thermal_jitter_threshold_ms,
    );

    let attitude_task = sensors::spawn_sensor_task(
        2, protocol::SensorType::Attitude,
        config.sensors.attitude_interval_ms,
        config.sensors.attitude_priority,
        tx_attitude.clone(),
        metrics.clone(),
        0,
        999.0,
    );

    let power_task = sensors::spawn_sensor_task(
        3, protocol::SensorType::Power,
        config.sensors.power_interval_ms,
        config.sensors.power_priority,
        tx_power.clone(),
        metrics.clone(),
        0,
        999.0,
    );

    // Fault injector — one random sensor fault every 60 seconds
    let fault_task = sensors::spawn_fault_injector(
        tx_thermal,
        tx_attitude,
        tx_power,
        metrics.clone(),
    );

    // =========================================================================
    // SCHEDULER
    // =========================================================================

    let scheduler_task = scheduler::spawn_scheduler(metrics.clone());

    // =========================================================================
    // COMMUNICATION
    // =========================================================================

    let downlink_task = communication::spawn_downlink(sensor_rx, &config, metrics.clone());
    let uplink_task   = communication::spawn_uplink(&config, metrics.clone());

    // =========================================================================
    // METRICS REPORTER
    // =========================================================================

    let reporter_task = metrics::spawn_reporter(
        metrics.clone(),
        config.system.metrics_report_interval_s,
    );

    info!("+==================================================+");
    info!("|  All subsystems started -- press Ctrl+C to stop |");
    info!("+==================================================+");

    // =========================================================================
    // RUN UNTIL SHUTDOWN
    // =========================================================================

    tokio::select! {
        _ = thermal_task   => { error!("Thermal sensor terminated unexpectedly"); }
        _ = attitude_task  => { error!("Attitude sensor terminated unexpectedly"); }
        _ = power_task     => { error!("Power sensor terminated unexpectedly"); }
        _ = fault_task     => { error!("Fault injector terminated unexpectedly"); }
        _ = scheduler_task => { error!("Task scheduler terminated unexpectedly"); }
        _ = downlink_task  => { error!("Telemetry downlink terminated unexpectedly"); }
        _ = uplink_task    => { error!("Command uplink terminated unexpectedly"); }
        _ = reporter_task  => { error!("Metrics reporter terminated unexpectedly"); }
        _ = tokio::signal::ctrl_c() => { info!("Shutdown signal received"); }
    }

    // =========================================================================
    // FINAL REPORT
    // =========================================================================

    let m = metrics.lock().await;

    let drop_rate  = if m.total_readings > 0 {
        (m.dropped_readings as f64 / m.total_readings as f64) * 100.0
    } else { 0.0 };
    let schedulable = if m.cpu_utilization() <= 75.7 { "YES" } else { "NO" };

    let _lock = print_lock::acquire();
    eprintln!("{}", format!(
"
+------------------------------------------------------------------+
|          SATELLITE OCS  --  FINAL SYSTEM REPORT                  |
+------------------------------------------------------------------+
| SENSOR METRICS                                                   |
|   Total Readings          : {tr:<35}  |
|   Dropped Readings        : {dr:<35}  |
|   Drop Rate               : {drate:<34.2}%  |
+------------------------------------------------------------------+
| TIMING METRICS                                                   |
|   Avg Jitter              : {aj:<31.3} ms   |
|   Max Jitter              : {mj:<31.3} ms   |
|   Avg Scheduling Drift    : {ad:<31.3} ms   |
|   Max Scheduling Drift    : {md:<31.3} ms   |
|   Max Latency (s->buffer) : {ml:<31.3} ms   |
+------------------------------------------------------------------+
| DEADLINE & FAULT METRICS                                         |
|   Missed Deadlines        : {miss:<35}  |
|   Total Faults Injected   : {faults:<35}  |
|   Consec. Thermal Misses  : {ctm:<35}  |
+------------------------------------------------------------------+
| BUFFER & CPU                                                     |
|   Current Buffer Fill     : {cbf:<34.1}%  |
|   Peak Buffer Fill        : {pbf:<34.1}%  |
|   CPU Utilization         : {cpu:<34.1}%  |
+------------------------------------------------------------------+
| NETWORK METRICS                                                  |
|   Packets Sent            : {ps:<35}  |
|   Packets Failed          : {pf:<35}  |
|   Packet Success Rate     : {psr:<34.1}%  |
|   Commands Received       : {cr:<35}  |
|   Commands Processed      : {cp:<35}  |
+------------------------------------------------------------------+
| SCHEDULABILITY (Rate Monotonic)                                  |
|   CPU Utilization         : {cpu2:<34.1}%  |
|   RM Bound (n=4)          : {rmb:<34.1}%  |
|   Schedulable?            : {sched:<35}  |
+------------------------------------------------------------------+
  Student : Ten Vui Khang  |  TP074341
  Role    : Student A -- Satellite Onboard Control System (OCS)
  Course  : CT087-3-3 Real-Time Systems
+------------------------------------------------------------------+
",
        tr     = m.total_readings,
        dr     = m.dropped_readings,
        drate  = drop_rate,
        aj     = m.avg_jitter(),
        mj     = m.max_jitter_ms,
        ad     = m.avg_drift(),
        md     = m.max_drift_ms,
        ml     = m.max_latency_ms,
        miss   = m.missed_deadlines,
        faults = m.fault_count,
        ctm    = m.consecutive_thermal_misses,
        cbf    = m.buffer_fill_percentage,
        pbf    = m.max_buffer_fill,
        cpu    = m.cpu_utilization(),
        ps     = m.packets_sent,
        pf     = m.packets_failed,
        psr    = m.packet_success_rate(),
        cr     = m.commands_received,
        cp     = m.commands_processed,
        cpu2   = m.cpu_utilization(),
        rmb    = 75.7_f64,
        sched  = schedulable,
    ));

    Ok(())
}