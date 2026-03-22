// src/main.rs  —  Ground Control Station

mod protocol;
mod metrics;
mod communication;
mod scheduler;
mod commands;
mod interlock;
mod uplink;

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
    info!("  Course : CT087-3-3 Real-Time Systems");
    info!("=================================================");

    let satellite_addr = "127.0.0.1:8001";
    let gcs_bind_addr  = "127.0.0.1:8002";

    // ── Shared state ──────────────────────────────────────────────────────
    let metrics         = Arc::new(Mutex::new(metrics::GcsMetrics::new()));
    let interlock_state = Arc::new(Mutex::new(interlock::InterlockState::new()));

    // ── Channels ──────────────────────────────────────────────────────────
    // communication → brain
    let (telem_tx, mut telem_rx) = mpsc::channel::<protocol::TelemetryPacket>(256);
    // scheduler → uplink  (merged with brain commands below)
    let (sched_tx, sched_rx)     = mpsc::channel::<scheduler::ScheduledCommand>(64);
    // brain → uplink
    let (brain_tx, brain_rx)     = mpsc::channel::<scheduler::ScheduledCommand>(64);
    // merged channel → uplink
    let (merged_tx, merged_rx)   = mpsc::channel::<scheduler::ScheduledCommand>(128);

    info!("  Starting subsystems...");

    // ── 1. Telemetry receiver ─────────────────────────────────────────────
    let comm_task = communication::spawn_receiver(
        gcs_bind_addr, satellite_addr, telem_tx, metrics.clone(),
    );

    // ── 2. Command Center ──────────────────────────────────────────────────
    let brain_metrics   = metrics.clone();
    let brain_interlock = interlock_state.clone();
    let brain_task = tokio::spawn(async move {
        let mut state = commands::GcsBrainState::new();
        while let Some(pkt) = telem_rx.recv().await {
            if let Some(cmd) = commands::analyse(&pkt, &mut state) {
                // Handle interlock signals BEFORE sending the command
                if state.should_release_interlock {
                    interlock::release(&brain_interlock, &brain_metrics).await;
                }
                if let Some((reason, fault)) = state.pending_interlock.take() {
                    interlock::engage(&brain_interlock, &brain_metrics, &reason, fault).await;
                }

                let sc = scheduler::ScheduledCommand {
                    packet:       cmd,
                    scheduled_at: tokio::time::Instant::now(),
                    deadline_ms:  2.0,
                };
                if brain_tx.send(sc).await.is_err() { break; }
            } else {
                // Still handle interlock signals even when no command is produced
                if state.should_release_interlock {
                    interlock::release(&brain_interlock, &brain_metrics).await;
                }
                if let Some((reason, fault)) = state.pending_interlock.take() {
                    interlock::engage(&brain_interlock, &brain_metrics, &reason, fault).await;
                }
            }
        }
    });

    // ── 3. Scheduler ──────────────────────────────────────────────────────
    let sched_task = scheduler::spawn_scheduler(sched_tx, metrics.clone());

    // ── 4. Fan-in: merge scheduler + brain → single uplink channel ────────
    let mt1 = merged_tx.clone();
    let mt2 = merged_tx.clone();
    let fanin_task = tokio::spawn(async move {
        let mut s = sched_rx;
        let mut b = brain_rx;
        loop {
            tokio::select! {
                Some(c) = s.recv() => { let _ = mt1.send(c).await; }
                Some(c) = b.recv() => { let _ = mt2.send(c).await; }
                else => break,
            }
        }
    });

    // ── 5. Uplink sender ──────────────────────────────────────────────────
    let uplink_task = uplink::spawn_uplink(
        satellite_addr, merged_rx, interlock_state.clone(), metrics.clone(),
    );

    // ── 6. Metrics reporter ───────────────────────────────────────────────
    let reporter_task = metrics::spawn_reporter(metrics.clone(), 10);

    info!("=================================================");
    info!("  Telemetry on  : {}", gcs_bind_addr);
    info!("  Commands to   : {}", satellite_addr);
    info!("  Press Ctrl+C to shutdown");
    info!("=================================================");

    tokio::select! {
        _ = comm_task     => error!("Telemetry receiver terminated"),
        _ = brain_task    => error!("Command brain terminated"),
        _ = sched_task    => error!("Scheduler terminated"),
        _ = fanin_task    => error!("Fan-in terminated"),
        _ = uplink_task   => error!("Uplink terminated"),
        _ = reporter_task => error!("Metrics reporter terminated"),
        _ = tokio::signal::ctrl_c() => info!("Shutdown signal received"),
    }

    let m = metrics.lock().await;
    info!("=================================================");
    info!("  FINAL GCS METRICS:\n{}", m.summary());
    info!("=================================================");

    Ok(())
}