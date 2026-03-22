// src/print_lock.rs
// Global mutex to prevent interleaved terminal output across threads.
// Any module that prints a multi-line block should acquire this lock first.
//
// Usage:
//   let _lock = print_lock::acquire();
//   eprintln!("{}", my_banner);
//   // lock released automatically when _lock goes out of scope
 
use std::sync::{Mutex, OnceLock};
 
static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
 
pub fn acquire() -> std::sync::MutexGuard<'static, ()> {
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}
 