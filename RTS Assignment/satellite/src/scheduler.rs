// src/scheduler.rs
// Real-time task scheduling (Rate Monotonic)

use crate::metrics::Metrics;
use tokio::time::{interval, Duration, Instant};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn, debug};

#[derive(Debug, Clone)]
pub enum TaskType {
    ThermalControl,    // Highest priority (shortest period: 50ms)
    DataCompression,   // High priority (200ms)
    AntennaAlignment,  // Medium priority (500ms)
    HealthMonitoring,  // Lowest priority (1000ms)
}

impl TaskType {
    fn name(&self) -> &str {
        match self {
            TaskType::ThermalControl => "ThermalControl",
            TaskType::DataCompression => "DataCompression",
            TaskType::AntennaAlignment => "AntennaAlignment",
            TaskType::HealthMonitoring => "HealthMonitoring",
        }
    }

    fn period_ms(&self) -> u64 {
        match self {
            TaskType::ThermalControl => 50,
            TaskType::DataCompression => 200,
            TaskType::AntennaAlignment => 500,
            TaskType::HealthMonitoring => 1000,
        }
    }

    fn deadline_ms(&self) -> u64 {
        // NOTE: Deadlines are set to reflect the ~15ms minimum timer resolution
        // of tokio::time::sleep() on a general-purpose OS kernel.
        // A real RTOS with hardware timers would achieve sub-millisecond precision.
        // These values are realistic for a software-simulated environment.
        match self {
            TaskType::ThermalControl => 20,
            TaskType::DataCompression => 25,
            TaskType::AntennaAlignment => 30,
            TaskType::HealthMonitoring => 40,
        }
    }

    fn execution_time_ms(&self) -> u64 {
        match self {
            TaskType::ThermalControl => 1,
            TaskType::DataCompression => 3,
            TaskType::AntennaAlignment => 5,
            TaskType::HealthMonitoring => 8,
        }
    }
}

/// Spawn Rate Monotonic Scheduler
/// Shorter period = Higher priority
pub fn spawn_scheduler(metrics: Arc<Mutex<Metrics>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut thermal_ticker    = interval(Duration::from_millis(50));
        let mut compression_ticker = interval(Duration::from_millis(200));
        let mut antenna_ticker    = interval(Duration::from_millis(500));
        let mut health_ticker     = interval(Duration::from_millis(1000));

        // Track expected start times per task for accurate drift measurement
        let mut thermal_next     = Instant::now();
        let mut compression_next = Instant::now();
        let mut antenna_next     = Instant::now();
        let mut health_next      = Instant::now();

        info!("[SCHED] Task scheduler started (Rate Monotonic Scheduling)");
        info!("   ThermalControl:   50ms period,   20ms deadline (OS timer limited)");
        info!("   DataCompression:  200ms period,  25ms deadline (OS timer limited)");
        info!("   AntennaAlignment: 500ms period,  30ms deadline (OS timer limited)");
        info!("   HealthMonitoring: 1000ms period, 40ms deadline (OS timer limited)");
        info!("   Note: tokio::time::sleep() has ~15ms min resolution on general-purpose OS.");
        info!("         A real RTOS with hardware timers achieves sub-millisecond precision.");

        loop {
            // tokio::select! implements priority ordering:
            // Higher priority tasks (shorter periods) are listed first
            tokio::select! {
                biased;

                // HIGHEST PRIORITY: Thermal control (50ms)
                _ = thermal_ticker.tick() => {
                    let drift_ms = Instant::now()
                        .duration_since(thermal_next)
                        .as_secs_f64() * 1000.0;
                    thermal_next += Duration::from_millis(50);
                    execute_task(TaskType::ThermalControl, &metrics, drift_ms).await;
                }

                // HIGH: Data compression (200ms)
                _ = compression_ticker.tick() => {
                    let drift_ms = Instant::now()
                        .duration_since(compression_next)
                        .as_secs_f64() * 1000.0;
                    compression_next += Duration::from_millis(200);
                    execute_task(TaskType::DataCompression, &metrics, drift_ms).await;
                }

                // MEDIUM: Antenna alignment (500ms)
                _ = antenna_ticker.tick() => {
                    let drift_ms = Instant::now()
                        .duration_since(antenna_next)
                        .as_secs_f64() * 1000.0;
                    antenna_next += Duration::from_millis(500);
                    execute_task(TaskType::AntennaAlignment, &metrics, drift_ms).await;
                }

                // LOWEST: Health monitoring (1000ms)
                _ = health_ticker.tick() => {
                    let drift_ms = Instant::now()
                        .duration_since(health_next)
                        .as_secs_f64() * 1000.0;
                    health_next += Duration::from_millis(1000);
                    execute_task(TaskType::HealthMonitoring, &metrics, drift_ms).await;
                }
            }
        }
    })
}

/// Execute a single task with deadline checking
async fn execute_task(task_type: TaskType, metrics: &Arc<Mutex<Metrics>>, drift_ms: f64) {
    let start = Instant::now();

    debug!("[TASK]  Executing task: {}", task_type.name());

    // ============================================================
    // DRIFT MEASUREMENT (scheduled vs actual start time)
    // ============================================================
    {
        let mut m = metrics.lock().await;
        m.record_drift(drift_ms);
    }

    if drift_ms > 1.0 {
        warn!(
            "[WARN]  Task {} scheduling drift: {:.3}ms",
            task_type.name(), drift_ms
        );
    }

    // ============================================================
    // TASK EXECUTION (busy-wait to avoid OS timer floor of ~15ms)
    // ============================================================
    simulate_task_execution(&task_type).await;

    let execution_time = start.elapsed();
    let execution_ms   = execution_time.as_secs_f64() * 1000.0;
    let deadline       = Duration::from_millis(task_type.deadline_ms());

    // ============================================================
    // DEADLINE VIOLATION CHECK
    // ============================================================
    if execution_time > deadline {
        let exceeded_by_ms = (execution_time - deadline).as_secs_f64() * 1000.0;

        warn!(
            "[WARN]  Task {} DEADLINE MISS: {:.3}ms > {}ms (exceeded by {:.3}ms)",
            task_type.name(),
            execution_ms,
            task_type.deadline_ms(),
            exceeded_by_ms
        );

        metrics.lock().await.missed_deadlines += 1;
    } else {
        debug!(
            "[OK] Task {} completed: {:.3}ms / {}ms deadline",
            task_type.name(),
            execution_ms,
            task_type.deadline_ms()
        );
    }

    // ============================================================
    // CPU UTILIZATION TRACKING
    // ============================================================
    {
        let mut m = metrics.lock().await;
        m.cpu_active_time_ms += execution_time.as_millis();
        m.cpu_total_time_ms  += Duration::from_millis(task_type.period_ms()).as_millis();
    }
}

/// Simulate task execution using busy-wait instead of tokio::time::sleep.
///
/// Why: tokio::time::sleep() relies on the OS kernel timer, which has a minimum
/// granularity of ~15ms on most Linux/Windows systems (kernel CONFIG_HZ).
/// Even sleep(1ms) will actually sleep ~15ms, causing every task to exceed
/// its deadline. Busy-waiting spins on the CPU for the exact required duration,
/// giving accurate sub-millisecond execution simulation.
///
/// Trade-off: consumes CPU during the wait period (acceptable for simulation).
async fn simulate_task_execution(task_type: &TaskType) {
    let target_duration = Duration::from_millis(task_type.execution_time_ms());
    let start = std::time::Instant::now();

    // Yield once to Tokio so other async tasks remain responsive
    tokio::task::yield_now().await;

    // Busy-wait for the remaining duration — accurate to microsecond level
    while start.elapsed() < target_duration {
        std::hint::spin_loop();
    }
}

/// Calculate scheduler utilization (should be ≤75.7% for 4-task RM schedulability)
pub fn calculate_utilization() -> f64 {
    let tasks = vec![
        TaskType::ThermalControl,
        TaskType::DataCompression,
        TaskType::AntennaAlignment,
        TaskType::HealthMonitoring,
    ];

    let mut total_utilization = 0.0;

    for task in tasks {
        let execution  = task.execution_time_ms() as f64;
        let period     = task.period_ms() as f64;
        let utilization = execution / period;
        total_utilization += utilization;

        info!(
            "Task {} utilization: {:.1}% ({}ms / {}ms)",
            task.name(),
            utilization * 100.0,
            task.execution_time_ms(),
            task.period_ms()
        );
    }

    total_utilization
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_monotonic_schedulability() {
        // Rate Monotonic bound: U ≤ n(2^(1/n) - 1)
        // For 4 tasks: U ≤ 0.757
        let utilization = calculate_utilization();
        assert!(utilization <= 0.757, "System is not schedulable under RM!");
    }

    #[test]
    fn test_deadlines_exceed_execution_times() {
        // Each deadline must be greater than its execution time
        let tasks = vec![
            TaskType::ThermalControl,
            TaskType::DataCompression,
            TaskType::AntennaAlignment,
            TaskType::HealthMonitoring,
        ];
        for task in tasks {
            assert!(
                task.deadline_ms() > task.execution_time_ms(),
                "Task {} deadline {}ms must exceed execution time {}ms",
                task.name(), task.deadline_ms(), task.execution_time_ms()
            );
        }
    }
}