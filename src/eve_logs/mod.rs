//! EVE gamelog alerts: tail each client's gamelog and flag its preview
//! when something needs attention (mining hold full, warp scrambled,
//! taking damage, fleet invite, …).
//!
//! `tailer` finds and reads the logs, `parser` turns lines into
//! `AlertKind`s, `alerts` holds per-character state for the preview
//! managers to paint.

pub mod alerts;
mod parser;
pub mod status;
mod tailer;

pub use tailer::gamelog_dirs;

/// Start the background log tailer. It re-reads `config.toml` every
/// couple of seconds, so enabling/disabling alerts (or individual
/// kinds) from the panel takes effect without a restart.
pub fn spawn() {
    let res = std::thread::Builder::new()
        .name("eve-log-alerts".into())
        .spawn(tailer::run);
    if let Err(e) = res {
        eprintln!("Warning: could not start EVE log alert thread: {}", e);
    }
}
