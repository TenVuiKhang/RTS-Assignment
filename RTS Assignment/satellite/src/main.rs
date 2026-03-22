// src/main.rs
// Satellite Onboard Control System - Main Entry Point
// Real-Time Systems Assignment - Student A

mod protocol;
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
    
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .with_thread_ids(true)
        .with_ansi(true)
        .init();
    
    info!("=================================================");
    info!("  SATELLITE ONBOARD CONTROL SYSTEM");
    info!("=================================================");
    info!("   Course: CT087-3-3 Real-Time Systems");
    info!("   Student: [Ten Vui Khang] - [TP074341]");
    info!("   Component: Satellite OCS (Student A)");
    info!("=================================================");
    
    // Load configuration
    let config = match config::Config::load_default() {
        Ok(c) => {
            info!("  Configuration loaded successfully");
            c
        }
        Err(e) => {
            error!(" Failed to load configuration: {}", e);
            error!("   Make sure config.toml exists in the project root");
            return Err(e);
        }
    };
    
    // Display configuration
    info!(" Configuration:");
    info!("   Network mode: {}", config.network.mode);
    info!("   Satellite addr: {}", config.get_network_addrs().satellite_addr);
    info!("   Ground addr: {}", config.get_network_addrs().ground_addr);
    info!("   Buffer size: {}", config.system.buffer_size);
    info!("   Fault injection every: {} iterations", config.system.fault_injection_every);
    
    if config.is_wifi_mode() {
        info!("   WiFi mode active - ensure ground station IP is correct!");
    } else {
        info!("  Localhost mode - testing locally");
    }
    
    // Check scheduler utilization
    info!("=================================================");
    info!("  Rate Monotonic Scheduler Analysis:");
    let utilization = scheduler::calculate_utilization();
    info!("   Total utilization: {:.1}%", utilization * 100.0);
    
    if utilization <= 0.757 {
        info!("     System is schedulable under RM");
    } else {
        error!("     WARNING: System may not be schedulable!");
    }
    info!("=================================================");
    
    // =========================================================================
    // SHARED STATE
    // =========================================================================
    
    let metrics = Arc::new(Mutex::new(metrics::Metrics::new()));
    
    // =========================================================================
    // SENSOR DATA PIPELINE
    // =========================================================================
    
    info!("  Starting subsystems...");
    
    // Create bounded channel for sensor readings (buffer)
    let (sensor_tx, sensor_rx) = mpsc::channel::<sensors::SensorReading>(
        config.system.buffer_size
    );
    
    // Spawn sensor tasks
    info!("   Starting sensor subsystem...");
    
    let thermal_task = sensors::spawn_sensor_task(
        1,
        protocol::SensorType::Thermal,
        config.sensors.thermal_interval_ms,
        config.sensors.thermal_priority,
        sensor_tx.clone(),
        metrics.clone(),
        config.system.fault_injection_every,
        config.system.thermal_jitter_threshold_ms,
    );
    
    let attitude_task = sensors::spawn_sensor_task(
        2,
        protocol::SensorType::Attitude,
        config.sensors.attitude_interval_ms,
        config.sensors.attitude_priority,
        sensor_tx.clone(),
        metrics.clone(),
        config.system.fault_injection_every,
        999.0, // Not critical for jitter
    );
    
    let power_task = sensors::spawn_sensor_task(
        3,
        protocol::SensorType::Power,
        config.sensors.power_interval_ms,
        config.sensors.power_priority,
        sensor_tx.clone(),
        metrics.clone(),
        config.system.fault_injection_every,
        999.0, // Not critical for jitter
    );
    
    // Drop original sender so channel can close when all sensors stop
    drop(sensor_tx);
    
    // =========================================================================
    // TASK SCHEDULER
    // =========================================================================
    
    info!("   Starting task scheduler...");
    let scheduler_task = scheduler::spawn_scheduler(metrics.clone());
    
    // =========================================================================
    // COMMUNICATION
    // =========================================================================
    
    info!("   Starting communication subsystem...");
    
    // Telemetry downlink (Satellite → Ground)
    let downlink_task = communication::spawn_downlink(
        sensor_rx,
        &config,
        metrics.clone(),
    );
    
    // Command uplink (Ground → Satellite)
    let uplink_task = communication::spawn_uplink(
        &config,
        metrics.clone(),
    );
    
    // =========================================================================
    // METRICS REPORTER
    // =========================================================================
    
    info!("   Starting metrics reporter...");
    let reporter_task = metrics::spawn_reporter(
        metrics.clone(),
        config.system.metrics_report_interval_s,
    );
    
    // =========================================================================
    // SYSTEM RUNNING
    // =========================================================================
    
    info!("=================================================");
    info!("  All subsystems started successfully!");
    info!("=================================================");
    info!("Press Ctrl+C to shutdown");
    info!("=================================================");
    
    // Wait for shutdown signal or task completion
    tokio::select! {
        _ = thermal_task => {
            error!("Thermal sensor task terminated unexpectedly");
        }
        _ = attitude_task => {
            error!("Attitude sensor task terminated unexpectedly");
        }
        _ = power_task => {
            error!("Power sensor task terminated unexpectedly");
        }
        _ = scheduler_task => {
            error!("Task scheduler terminated unexpectedly");
        }
        _ = downlink_task => {
            error!("Telemetry downlink terminated unexpectedly");
        }
        _ = uplink_task => {
            error!("Command uplink terminated unexpectedly");
        }
        _ = reporter_task => {
            error!("Metrics reporter terminated unexpectedly");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("=================================================");
            info!("  Shutdown signal received");
            info!("=================================================");
        }
    }
    
    // =========================================================================
    // SHUTDOWN
    // =========================================================================
    
    info!("Shutting down gracefully...");

    // Print final report atomically - build as one string then print once
    // so other threads cannot interleave their output through it
    let m = metrics.lock().await;

    let drop_rate = if m.total_readings > 0 {
        (m.dropped_readings as f64 / m.total_readings as f64) * 100.0
    } else { 0.0 };
    let schedulable = if m.cpu_utilization() <= 75.7 { "YES" } else { "NO" };

    let report = format!(
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
        tr    = m.total_readings,
        dr    = m.dropped_readings,
        drate = drop_rate,
        aj    = m.avg_jitter(),
        mj    = m.max_jitter_ms,
        ad    = m.avg_drift(),
        md    = m.max_drift_ms,
        ml    = m.max_latency_ms,
        miss  = m.missed_deadlines,
        faults = m.fault_count,
        ctm   = m.consecutive_thermal_misses,
        cbf   = m.buffer_fill_percentage,
        pbf   = m.max_buffer_fill,
        cpu   = m.cpu_utilization(),
        ps    = m.packets_sent,
        pf    = m.packets_failed,
        psr   = m.packet_success_rate(),
        cr    = m.commands_received,
        cp    = m.commands_processed,
        cpu2  = m.cpu_utilization(),
        rmb   = 75.7_f64,
        sched = schedulable,
    );

    eprintln!("{}", report);

    Ok(())
}