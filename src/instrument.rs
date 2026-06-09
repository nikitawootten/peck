use std::time::Instant;

/// Initialize the global tracing subscriber.
///
/// `RUST_LOG=peck=debug` surfaces the per-phase breakdown.
pub fn init() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

/// A scoped timer that logs how long a named phase took when it goes out of
/// scope. Use one per pipeline stage:
///
/// ```ignore
/// let _p = Phase::start("tree");
/// // ... do work ...
/// // dropping `_p` logs: phase=tree elapsed=12.3ms
/// ```
#[must_use]
pub struct Phase {
    name: &'static str,
    start: Instant,
}

impl Phase {
    pub fn start(name: &'static str) -> Self {
        Self {
            name,
            start: Instant::now(),
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        tracing::debug!(
            phase = self.name,
            elapsed_ms = elapsed.as_secs_f64() * 1e3,
            "phase complete"
        );
    }
}
