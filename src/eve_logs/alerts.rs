//! Alert kinds raised from EVE gamelog lines, plus the process-wide
//! store the preview managers read when painting chrome.
//!
//! The store is a global (like `preview_windows::MANAGER_PTR`) rather
//! than an `Arc` threaded through every spawn signature: the writer is
//! the log-tailer thread, the readers are whichever preview manager is
//! running, and none of them otherwise share state.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use crate::config::AlertConfig;

/// How loud an alert is. Drives the chrome color and whether a newer
/// alert may replace an older one (never downgrade a pending warp
/// scramble to "asteroid depleted").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    /// RGB used for the flashing title strip + border. All three are
    /// dark enough that the cream character-name text stays readable,
    /// and the critical red is brighter than Nicotine red so an
    /// alerting preview never reads as "this is the active client".
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            Severity::Critical => (235, 30, 30),
            Severity::Warning => (215, 120, 0),
            Severity::Info => (30, 110, 200),
        }
    }
}

/// Every event Nicotine can surface from a gamelog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertKind {
    /// Incoming damage or a "misses you" line.
    Attacked,
    /// Inside a warp disruption bubble, or pointed/scrammed by a ship.
    WarpScrambled,
    /// Cloak dropped by proximity or a decloak pulse.
    Decloaked,
    /// Mining module stopped because the ore/cargo hold is full.
    CargoFull,
    /// Mining crystal destroyed, module deactivated.
    CrystalBroken,
    /// The rock is gone: depleted, or the target lock was lost mid-cycle.
    AsteroidDepleted,
    /// Someone invited this character to a fleet.
    FleetInvite,
    /// Someone opened a private conversation with this character.
    ConvoRequest,
}

impl AlertKind {
    pub const ALL: [AlertKind; 8] = [
        AlertKind::Attacked,
        AlertKind::WarpScrambled,
        AlertKind::Decloaked,
        AlertKind::CargoFull,
        AlertKind::CrystalBroken,
        AlertKind::AsteroidDepleted,
        AlertKind::FleetInvite,
        AlertKind::ConvoRequest,
    ];

    pub fn severity(self) -> Severity {
        match self {
            AlertKind::Attacked | AlertKind::WarpScrambled | AlertKind::Decloaked => {
                Severity::Critical
            }
            AlertKind::CargoFull
            | AlertKind::CrystalBroken
            | AlertKind::FleetInvite
            | AlertKind::ConvoRequest => Severity::Warning,
            AlertKind::AsteroidDepleted => Severity::Info,
        }
    }

    /// Short upper-case tag drawn in the preview's title strip.
    pub fn label(self) -> &'static str {
        match self {
            AlertKind::Attacked => "ATTACKED",
            AlertKind::WarpScrambled => "SCRAMBLED",
            AlertKind::Decloaked => "DECLOAKED",
            AlertKind::CargoFull => "CARGO FULL",
            AlertKind::CrystalBroken => "CRYSTAL",
            AlertKind::AsteroidDepleted => "ROCK GONE",
            AlertKind::FleetInvite => "FLEET INVITE",
            AlertKind::ConvoRequest => "CONVO",
        }
    }

    /// Human description for the config panel checkbox.
    pub fn description(self) -> &'static str {
        match self {
            AlertKind::Attacked => "Taking damage (or being shot at)",
            AlertKind::WarpScrambled => "Warp scrambled / disrupted / in a bubble",
            AlertKind::Decloaked => "Cloak dropped",
            AlertKind::CargoFull => "Mining hold full",
            AlertKind::CrystalBroken => "Mining crystal destroyed",
            AlertKind::AsteroidDepleted => "Asteroid depleted / mining target lost",
            AlertKind::FleetInvite => "Fleet invite",
            AlertKind::ConvoRequest => "Conversation request",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveAlert {
    pub kind: AlertKind,
    pub since: Instant,
}

struct Store {
    alerts: HashMap<String, ActiveAlert>,
    /// Character of the currently-focused EVE client, as reported by
    /// the preview manager. Its alerts are cleared and new ones are
    /// dropped — you're already looking at that client.
    active_character: Option<String>,
    /// Bumped on every change so renderers can cheaply tell whether a
    /// repaint is needed.
    generation: u64,
    flash: bool,
}

fn store() -> MutexGuard<'static, Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            Mutex::new(Store {
                alerts: HashMap::new(),
                active_character: None,
                generation: 0,
                flash: true,
            })
        })
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Record an alert for `character`. Ignored when that character is the
/// focused client, or when a more severe alert is already pending.
/// Returns true if the store changed.
pub fn raise(character: &str, kind: AlertKind) -> bool {
    let mut s = store();
    if s.active_character.as_deref() == Some(character) {
        return false;
    }
    if let Some(existing) = s.alerts.get(character) {
        if existing.kind.severity() > kind.severity() {
            return false;
        }
        if existing.kind == kind {
            // Already showing exactly this — don't restart the flash.
            return false;
        }
    }
    s.alerts.insert(
        character.to_string(),
        ActiveAlert {
            kind,
            since: Instant::now(),
        },
    );
    s.generation += 1;
    true
}

/// Tell the store which character is focused (None = no EVE client).
/// Clears that character's pending alert.
pub fn set_active_character(character: Option<&str>) {
    let mut s = store();
    if s.active_character.as_deref() != character {
        s.active_character = character.map(str::to_string);
    }
    if let Some(c) = character {
        if s.alerts.remove(c).is_some() {
            s.generation += 1;
        }
    }
}

pub fn get(character: &str) -> Option<ActiveAlert> {
    store().alerts.get(character).copied()
}

/// Drop pending alerts that the current settings no longer allow, and
/// apply the flash preference. Called by the tailer on config reload.
pub fn apply_config(cfg: &AlertConfig) {
    let mut s = store();
    let before = s.alerts.len();
    s.alerts
        .retain(|_, a| cfg.enabled && cfg.kind_enabled(a.kind));
    let mut changed = s.alerts.len() != before;
    if s.flash != cfg.flash {
        s.flash = cfg.flash;
        changed = true;
    }
    if changed {
        s.generation += 1;
    }
}

/// Flash cadence. 500 ms on / 500 ms off.
const FLASH_HALF_PERIOD_MS: u128 = 500;

/// Whether an alert should be drawn in its "lit" color right now.
/// Always true when flashing is turned off.
pub fn lit(alert: &ActiveAlert, now: Instant) -> bool {
    if !store().flash {
        return true;
    }
    lit_at(alert.since, now)
}

fn lit_at(since: Instant, now: Instant) -> bool {
    let elapsed = now.saturating_duration_since(since).as_millis();
    (elapsed / FLASH_HALF_PERIOD_MS).is_multiple_of(2)
}

/// A value that changes every flash half-period while any alert is
/// pending, and stays constant otherwise. Renderers compare it against
/// the last value they painted to decide when to repaint.
pub fn paint_token(now: Instant) -> (u64, u128) {
    let s = store();
    let phase = if s.flash && !s.alerts.is_empty() {
        // Phase relative to process start is fine: each alert's own
        // `lit` uses its own `since`, this only needs to tick.
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        let epoch = *EPOCH.get_or_init(Instant::now);
        now.saturating_duration_since(epoch).as_millis() / (FLASH_HALF_PERIOD_MS / 2)
    } else {
        0
    };
    (s.generation, phase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn flash_alternates_every_half_period() {
        let t0 = Instant::now();
        assert!(lit_at(t0, t0));
        assert!(lit_at(t0, t0 + Duration::from_millis(499)));
        assert!(!lit_at(t0, t0 + Duration::from_millis(500)));
        assert!(lit_at(t0, t0 + Duration::from_millis(1000)));
    }

    #[test]
    fn severity_orders_critical_highest() {
        assert!(Severity::Critical > Severity::Warning);
        assert!(Severity::Warning > Severity::Info);
    }

    // The store is process-global, so exercise it in one test with
    // character names no other test uses.
    #[test]
    fn store_lifecycle() {
        set_active_character(None);
        assert!(raise("store-test-a", AlertKind::AsteroidDepleted));
        // Upgrade to a more severe alert is accepted...
        assert!(raise("store-test-a", AlertKind::CargoFull));
        assert_eq!(get("store-test-a").unwrap().kind, AlertKind::CargoFull);
        // ...a downgrade is not.
        assert!(!raise("store-test-a", AlertKind::AsteroidDepleted));
        // Same kind again doesn't restart it.
        assert!(!raise("store-test-a", AlertKind::CargoFull));

        // Focusing the client clears its alert and suppresses new ones.
        set_active_character(Some("store-test-a"));
        assert!(get("store-test-a").is_none());
        assert!(!raise("store-test-a", AlertKind::Attacked));

        // Other characters are unaffected.
        assert!(raise("store-test-b", AlertKind::Attacked));
        set_active_character(None);
        assert!(get("store-test-b").is_some());
    }
}
