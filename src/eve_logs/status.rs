//! What the log tailer currently sees, for the config panel's Alerts
//! tab: which folders are watched, which characters were found in which
//! gamelog, and what happened to the last event read for each.
//!
//! Like the alert store this is a process-global. The tailer runs in
//! the daemon thread of the same process as the panel in the normal
//! `Nicotine start` case; when a separate headless daemon owns the
//! tailer, the panel's copy is never published and says so.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::SystemTime;

use super::alerts::AlertKind;

/// What became of an event the parser recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The preview was flagged.
    Shown,
    /// The event's checkbox is off.
    KindDisabled,
    /// The same alert, or a more severe one, is already showing.
    AlreadyFlagged,
    /// Raised by the panel's "Test" button.
    Test,
}

impl Outcome {
    pub fn describe(self) -> &'static str {
        match self {
            Outcome::Shown => "preview flagged",
            Outcome::KindDisabled => "not flagged: event turned off",
            Outcome::AlreadyFlagged => "already flagged",
            Outcome::Test => "test alert",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LastEvent {
    pub kind: AlertKind,
    pub at: SystemTime,
    pub outcome: Outcome,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CharacterLog {
    /// From the gamelog header; None until the header has been read.
    pub character: Option<String>,
    pub file: PathBuf,
    /// When EVE last wrote to this file.
    pub last_line: Option<SystemTime>,
    pub last_event: Option<LastEvent>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogStatus {
    /// False until the tailer has published once in this process.
    pub running: bool,
    pub enabled: bool,
    pub dirs: Vec<PathBuf>,
    /// One entry per character (its most recently active gamelog), plus
    /// any file whose header hasn't been read yet. Sorted by name.
    pub logs: Vec<CharacterLog>,
}

fn store() -> MutexGuard<'static, LogStatus> {
    static S: OnceLock<Mutex<LogStatus>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(LogStatus::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn get() -> LogStatus {
    store().clone()
}

/// Replace the snapshot. `logs` may hold several files per character
/// (EVE starts a new gamelog per session); only the most recently
/// active one is kept, carrying the newest event across files.
pub fn publish(enabled: bool, dirs: Vec<PathBuf>, logs: Vec<CharacterLog>) {
    let mut s = store();
    let tests: Vec<CharacterLog> = s
        .logs
        .iter()
        .filter(|l| {
            l.last_event
                .as_ref()
                .is_some_and(|e| e.outcome == Outcome::Test)
        })
        .cloned()
        .collect();
    let mut merged = dedupe(logs);
    // Keep a test alert visible until a real event replaces it.
    for t in tests {
        if let Some(m) = merged.iter_mut().find(|m| m.character == t.character) {
            let newer = match (&m.last_event, &t.last_event) {
                (Some(a), Some(b)) => b.at > a.at,
                (None, Some(_)) => true,
                _ => false,
            };
            if newer {
                m.last_event = t.last_event;
            }
        }
    }
    *s = LogStatus {
        running: true,
        enabled,
        dirs,
        logs: merged,
    };
}

/// Note a test alert raised from the panel against `character`.
pub fn record_test(character: &str, kind: AlertKind) {
    let mut s = store();
    if let Some(l) = s
        .logs
        .iter_mut()
        .find(|l| l.character.as_deref() == Some(character))
    {
        l.last_event = Some(LastEvent {
            kind,
            at: SystemTime::now(),
            outcome: Outcome::Test,
        });
    }
}

fn dedupe(mut logs: Vec<CharacterLog>) -> Vec<CharacterLog> {
    // Most recent first so the first entry per character wins.
    logs.sort_by_key(|l| std::cmp::Reverse(l.last_line));
    let mut out: Vec<CharacterLog> = Vec::new();
    for l in logs {
        match l
            .character
            .as_ref()
            .and_then(|c| out.iter_mut().find(|o| o.character.as_ref() == Some(c)))
        {
            Some(existing) => {
                let newer = match (&existing.last_event, &l.last_event) {
                    (Some(a), Some(b)) => b.at > a.at,
                    (None, Some(_)) => true,
                    _ => false,
                };
                if newer {
                    existing.last_event = l.last_event;
                }
            }
            None => out.push(l),
        }
    }
    out.sort_by(|a, b| a.character.cmp(&b.character).then(a.file.cmp(&b.file)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn log(name: &str, file: &str, line: u64, event: Option<u64>) -> CharacterLog {
        let t = |s| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        CharacterLog {
            character: Some(name.into()),
            file: PathBuf::from(file),
            last_line: Some(t(line)),
            last_event: event.map(|s| LastEvent {
                kind: AlertKind::Attacked,
                at: t(s),
                outcome: Outcome::Shown,
            }),
        }
    }

    #[test]
    fn dedupe_keeps_newest_file_and_newest_event() {
        let out = dedupe(vec![
            log("Bob", "old.txt", 10, Some(5)),
            log("Bob", "new.txt", 20, None),
            log("Alice", "a.txt", 1, None),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].character.as_deref(), Some("Alice"));
        assert_eq!(out[1].file, PathBuf::from("new.txt"));
        assert!(
            out[1].last_event.is_some(),
            "event carried from the older session"
        );
    }
}
