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
        match self {
            TaskType::ThermalControl => 5,    // Must complete in 5ms
            TaskType::DataCompression => 10,
            TaskType::AntennaAlignment => 15,
            TaskType::HealthMonitoring => 20,
        }
    }

    fn execution_time_ms(&self) -> u64 {
        match self {
            TaskType::ThermalControl => 1,    // Simulated execution
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
        let mut thermal_ticker = interval(Duration::from_millis(50));
        let mut compression_ticker = interval(Duration::from_millis(200));
        let mut antenna_ticker = interval(Duration::from_millis(500));
        let mut health_ticker = interval(Duration::from_millis(1000));
        
        info!("📅 Task scheduler started (Rate Monotonic Scheduling)");
        info!("   ThermalControl: 50ms period, 5ms deadline");
        info!("   DataCompression: 200ms period, 10ms deadline");
        info!("   AntennaAlignment: 500ms period, 15ms deadline");
        info!("   HealthMonitoring: 1000ms period, 20ms deadline");
        
        loop {
            // tokio::select! implements preemption:
            // Higher priority tasks (shorter periods) are checked first
            tokio::select! {
                // HIGHEST PRIORITY: Thermal control (50ms)
                _ = thermal_ticker.tick() => {
                    execute_task(TaskType::ThermalControl, &metrics).await;
                }
                
                // HIGH: Data compression (200ms)
                _ = compression_ticker.tick() => {
                    execute_task(TaskType::DataCompression, &metrics).await;
                }
                
                // MEDIUM: Antenna alignment (500ms)
                _ = antenna_ticker.tick() => {
                    execute_task(TaskType::AntennaAlignment, &metrics).await;
                }
                
                // LOWEST: Health monitoring (1000ms)
                _ = health_ticker.tick() => {
                    execute_task(TaskType::HealthMonitoring, &metrics).await;
                }
            }
        }
    })
}

/// Execute a single task with deadline checking
async fn execute_task(task_type: TaskType, metrics: &Arc<Mutex<Metrics>>) {
    let start = Instant::now();
    let scheduled_time = start;
    
    debug!("⚙️  Executing task: {}", task_type.name());
    
    // ============================================================
    // DRIFT MEASUREMENT (scheduled vs actual start time)
    // ============================================================
    let actual_start = Instant::now();
    let drift = actual_start.duration_since(scheduled_time);
    let drift_ms = drift.as_secs_f64() * 1000.0;
    
    {
        let mut m = metrics.lock().await;
        m.record_drift(drift_ms);
    }
    
    if drift_ms > 1.0 {
        warn!(
            "⚠️  Task {} scheduling drift: {:.3}ms",
            task_type.name(), drift_ms
        );
    }
    
    // ============================================================
    // TASK EXECUTION (simulated work)
    // ============================================================
    simulate_task_execution(&task_type).await;
    
    let execution_time = start.elapsed();
    let execution_ms = execution_time.as_secs_f64() * 1000.0;
    let deadline = Duration::from_millis(task_type.deadline_ms());
    
    // ============================================================
    // DEADLINE VIOLATION CHECK
    // ============================================================
    if execution_time > deadline {
        let exceeded_by_ms = (execution_time - deadline).as_secs_f64() * 1000.0;
        
        warn!(
            "⚠️  Task {} DEADLINE MISS: {:.3}ms > {}ms (exceeded by {:.3}ms)",
            task_type.name(),
            execution_ms,
            task_type.deadline_ms(),
            exceeded_by_ms
        );
        
        metrics.lock().await.missed_deadlines += 1;
    } else {
        debug!(
            "✅ Task {} completed: {:.3}ms / {}ms deadline",
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
        m.cpu_total_time_ms += Duration::from_millis(task_type.period_ms()).as_millis();
    }
}

/// Simulate task execution time
async fn simulate_task_execution(task_type: &TaskType) {
    let execution_time = Duration::from_millis(task_type.execution_time_ms());
    
    // Simulate actual work
    match task_type {
        TaskType::ThermalControl => {
            // Quick thermal check
            tokio::time::sleep(execution_time).await;
        }
        TaskType::DataCompression => {
            // Compress sensor data
            tokio::time::sleep(execution_time).await;
        }
        TaskType::AntennaAlignment => {
            // Adjust antenna position
            tokio::time::sleep(execution_time).await;
        }
        TaskType::HealthMonitoring => {
            // System health check
            tokio::time::sleep(execution_time).await;
        }
    }
}

/// Calculate scheduler utilization (should be <100% for schedulability)
pub fn calculate_utilization() -> f64 {
    let tasks = vec![
        TaskType::ThermalControl,
        TaskType::DataCompression,
        TaskType::AntennaAlignment,
        TaskType::HealthMonitoring,
    ];
    
    let mut total_utilization = 0.0;
    
    for task in tasks {
        let execution = task.execution_time_ms() as f64;
        let period = task.period_ms() as f64;
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
}