//! Finds EVE's `Gamelogs` directory and tails every recently-active
//! gamelog, feeding complete lines to the parser and raising alerts.
//!
//! Each EVE client writes its own `YYYYMMDD_HHMMSS_<charid>.txt` with a
//! header naming the character (`Listener: Name`). That name equals the
//! client's window title minus `EVE - `, which is how alerts find their
//! preview.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::alerts::{self, AlertKind};
use super::parser;
use super::status::{self, CharacterLog, LastEvent, Outcome};
use crate::config::{AlertConfig, Config};

/// How often tracked files are checked for growth.
const READ_INTERVAL: Duration = Duration::from_millis(250);
/// How often the directory is rescanned and config.toml re-read.
const SCAN_INTERVAL: Duration = Duration::from_secs(2);
/// Gamelogs untouched for longer than this are ignored. Generous so an
/// idle client that hasn't logged anything for hours is still tracked
/// when it finally does.
const ACTIVE_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
/// When a file is first seen we read it whole (to find the header) but
/// only alert on lines this recent, so startup doesn't replay a whole
/// session's worth of old events.
const FRESH_LINE_WINDOW_SECS: i64 = 30;
/// Never read more than this from a single file in one pass; a huge
/// first read is capped to the tail.
const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;

struct TrackedFile {
    pos: u64,
    character: Option<String>,
    /// True until the first read completes; first-read lines are
    /// filtered by timestamp.
    first_read: bool,
    /// When a complete line was last read (for the panel's status).
    last_line: Option<SystemTime>,
    last_event: Option<LastEvent>,
}

impl TrackedFile {
    fn new() -> Self {
        Self {
            pos: 0,
            character: None,
            first_read: true,
            last_line: None,
            last_event: None,
        }
    }
}

pub fn run() {
    let mut tracked: HashMap<PathBuf, TrackedFile> = HashMap::new();
    let mut cfg = AlertConfig::default();
    let mut last_scan: Option<Instant> = None;
    let mut logged_dirs: Option<Vec<PathBuf>> = None;
    let mut dirs: Vec<PathBuf> = Vec::new();

    loop {
        let mut changed = false;
        if last_scan.is_none_or(|t| t.elapsed() >= SCAN_INTERVAL) {
            changed = true;
            last_scan = Some(Instant::now());
            if let Ok(fresh) = Config::load() {
                cfg = fresh.alerts;
            }
            alerts::apply_config(&cfg);

            if cfg.enabled {
                dirs = gamelog_dirs(cfg.logs_dir.as_deref());
                if logged_dirs.as_ref() != Some(&dirs) {
                    if dirs.is_empty() {
                        println!(
                            "EVE log alerts: no Gamelogs directory found \
                             (set alerts.logs_dir in config.toml)"
                        );
                    } else {
                        for d in &dirs {
                            println!("EVE log alerts: watching {}", d.display());
                        }
                    }
                    logged_dirs = Some(dirs.clone());
                }
                scan(&dirs, &mut tracked);
            } else {
                tracked.clear();
                logged_dirs = None;
                dirs.clear();
            }
        }

        if cfg.enabled {
            for (path, file) in tracked.iter_mut() {
                let before = file.last_line;
                for (character, kind) in read_new(path, file) {
                    let outcome = if cfg.kind_enabled(kind) {
                        alerts::raise_outcome(&character, kind)
                    } else {
                        Outcome::KindDisabled
                    };
                    println!(
                        "EVE log alert: {} — {} ({})",
                        character,
                        kind.label(),
                        outcome.describe()
                    );
                    file.last_event = Some(LastEvent {
                        kind,
                        at: SystemTime::now(),
                        outcome,
                    });
                }
                changed |= file.last_line != before;
            }
        }

        if changed {
            let logs = tracked
                .iter()
                .map(|(path, f)| CharacterLog {
                    character: f.character.clone(),
                    file: path.clone(),
                    // EVE's write time, not ours: a day-old log read at
                    // startup shouldn't look active.
                    last_line: fs::metadata(path)
                        .and_then(|m| m.modified())
                        .ok()
                        .or(f.last_line),
                    last_event: f.last_event.clone(),
                })
                .collect();
            status::publish(cfg.enabled, dirs.clone(), logs);
        }

        std::thread::sleep(READ_INTERVAL);
    }
}

/// Add newly-active gamelogs to `tracked`, drop ones that went stale or
/// vanished.
fn scan(dirs: &[PathBuf], tracked: &mut HashMap<PathBuf, TrackedFile>) {
    let now = SystemTime::now();
    let is_recent = |p: &Path| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age <= ACTIVE_WINDOW)
    };

    tracked.retain(|p, _| is_recent(p));

    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("txt") {
                continue;
            }
            if tracked.contains_key(&path) || !is_recent(&path) {
                continue;
            }
            tracked.insert(path, TrackedFile::new());
        }
    }
}

/// Read whatever was appended since last time. Returns the alerts found.
fn read_new(path: &Path, file: &mut TrackedFile) -> Vec<(String, AlertKind)> {
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return Vec::new();
    };
    if len < file.pos {
        // Truncated / replaced — start over.
        file.pos = 0;
        file.first_read = true;
    }
    if len == file.pos {
        return Vec::new();
    }

    let mut out = Vec::new();

    // A first read of a large file: grab the header from the start, then
    // jump to the tail.
    if file.first_read && len > MAX_READ_BYTES {
        if file.character.is_none() {
            let mut head = vec![0u8; 4096];
            if let Ok(n) = f.read(&mut head) {
                file.character = header_character(&String::from_utf8_lossy(&head[..n]));
            }
        }
        file.pos = len - MAX_READ_BYTES;
    }

    if f.seek(SeekFrom::Start(file.pos)).is_err() {
        return out;
    }
    let mut buf = Vec::new();
    if f.take(MAX_READ_BYTES).read_to_end(&mut buf).is_err() {
        return out;
    }
    // Only consume complete lines; a partial trailing line is re-read
    // next time once EVE finishes writing it.
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return out;
    };
    let chunk = String::from_utf8_lossy(&buf[..=last_nl]);
    file.pos += (last_nl + 1) as u64;
    file.last_line = Some(SystemTime::now());

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    for line in chunk.lines() {
        if let Some(name) = parser::parse_listener(line) {
            file.character = Some(name);
            continue;
        }
        let Some(character) = &file.character else {
            continue;
        };
        if file.first_read {
            match line_timestamp(line) {
                Some(ts) if now_secs - ts <= FRESH_LINE_WINDOW_SECS => {}
                _ => continue,
            }
        }
        if let Some(kind) = parser::classify(line) {
            out.push((character.clone(), kind));
        }
    }
    file.first_read = false;
    out
}

fn header_character(text: &str) -> Option<String> {
    text.lines().find_map(parser::parse_listener)
}

/// Unix seconds of a `[ YYYY.MM.DD HH:MM:SS ]` line prefix. EVE logs
/// in EVE time, which is UTC.
fn line_timestamp(line: &str) -> Option<i64> {
    let inner = line.trim_start().strip_prefix('[')?;
    let inner = inner[..inner.find(']')?].trim();
    let (date, time) = inner.split_once(' ')?;
    let mut d = date.split('.').map(|p| p.parse::<i64>());
    let (y, mo, da) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':').map(|p| p.parse::<i64>());
    let (h, mi, s) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    Some(days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + s)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard
/// Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Every existing `Gamelogs` directory: the configured override if set,
/// otherwise the platform's usual EVE locations.
pub fn gamelog_dirs(override_dir: Option<&str>) -> Vec<PathBuf> {
    if let Some(dir) = override_dir.map(str::trim).filter(|d| !d.is_empty()) {
        let p = expand_home(dir);
        // Accept either the `logs` folder or `logs/Gamelogs` itself.
        let nested = p.join("Gamelogs");
        return if nested.is_dir() {
            vec![nested]
        } else if p.is_dir() {
            vec![p]
        } else {
            Vec::new()
        };
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for docs in document_dirs() {
        let g = docs.join("EVE").join("logs").join("Gamelogs");
        if g.is_dir() && !found.contains(&g) {
            found.push(g);
        }
    }
    found
}

fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(p),
    }
}

/// Candidate "Documents" folders an EVE client might write into.
#[cfg(windows)]
fn document_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    // Known-folder lookup — follows OneDrive redirection.
    if let Some(d) = dirs::document_dir() {
        v.push(d);
    }
    if let Some(home) = dirs::home_dir() {
        v.push(home.join("Documents"));
        v.push(home.join("OneDrive").join("Documents"));
    }
    v
}

/// On Linux EVE runs under Wine/Proton, so its Documents folder lives
/// inside a Wine prefix. Look in Steam's Proton prefix for EVE (app
/// 8500) in every Steam library, plus plain Wine and Lutris prefixes.
#[cfg(unix)]
fn document_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    let mut prefixes: Vec<PathBuf> = Vec::new();

    let steam_roots = [
        home.join(".steam/steam"),
        home.join(".local/share/Steam"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        home.join(".var/app/com.valvesoftware.Steam/.steam/steam"),
    ];
    let mut libraries: Vec<PathBuf> = Vec::new();
    for root in &steam_roots {
        libraries.push(root.clone());
        let vdf = root.join("steamapps/libraryfolders.vdf");
        if let Ok(text) = fs::read_to_string(vdf) {
            libraries.extend(steam_library_paths(&text));
        }
    }
    for lib in libraries {
        prefixes.push(lib.join("steamapps/compatdata/8500/pfx"));
    }

    prefixes.push(home.join(".wine"));
    // Lutris / Bottles / hand-made prefixes commonly live under ~/Games.
    if let Ok(entries) = fs::read_dir(home.join("Games")) {
        for e in entries.flatten() {
            prefixes.push(e.path());
            prefixes.push(e.path().join("prefix"));
        }
    }

    let mut docs = Vec::new();
    for prefix in prefixes {
        let Ok(users) = fs::read_dir(prefix.join("drive_c/users")) else {
            continue;
        };
        for user in users.flatten() {
            docs.push(user.path().join("Documents"));
            docs.push(user.path().join("My Documents"));
        }
    }
    docs
}

/// Library paths from a Steam `libraryfolders.vdf` (`"path"  "/x/y"`).
#[cfg_attr(windows, allow(dead_code))]
fn steam_library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .filter_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("\"path\"")?.trim();
            let value = rest.strip_prefix('"')?.strip_suffix('"')?;
            Some(PathBuf::from(value.replace("\\\\", "\\")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn timestamp_parses_eve_prefix() {
        // 2026-11-03 18:21:07 UTC
        assert_eq!(
            line_timestamp("[ 2026.11.03 18:21:07 ] (notify) x"),
            Some(1_793_730_067)
        );
        assert_eq!(line_timestamp("  Listener: Bob"), None);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn parses_steam_library_vdf() {
        let vdf = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"/home/u/.local/share/Steam"
	}
	"1"
	{
		"path"		"/mnt/games/SteamLibrary"
	}
}"#;
        assert_eq!(
            steam_library_paths(vdf),
            vec![
                PathBuf::from("/home/u/.local/share/Steam"),
                PathBuf::from("/mnt/games/SteamLibrary"),
            ]
        );
    }

    fn eve_now() -> String {
        // Format the current UTC time the way EVE does.
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        // Inverse of days_from_civil.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
        format!(
            "[ {:04}.{:02}.{:02} {:02}:{:02}:{:02} ]",
            y,
            m,
            d,
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        )
    }

    #[test]
    fn tails_file_skipping_stale_lines_and_partial_writes() {
        let dir = std::env::temp_dir().join(format!("nicotine-tail-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("20261103_000000_1.txt");
        let mut f = File::create(&path).unwrap();
        writeln!(
            f,
            "------------------------------------------------------------\n  Gamelog\n  Listener: Miner Alt\n  Session Started: 2026.11.03 00:00:00\n------------------------------------------------------------"
        )
        .unwrap();
        // Old event — must not fire on first read.
        writeln!(
            f,
            "[ 2020.01.01 00:00:00 ] (notify) Ship's cargo hold is full."
        )
        .unwrap();
        f.flush().unwrap();

        let mut tf = TrackedFile::new();
        assert!(read_new(&path, &mut tf).is_empty());
        assert_eq!(tf.character.as_deref(), Some("Miner Alt"));

        // New event, written in two halves: nothing until the newline.
        write!(
            f,
            "{} (notify) Miner II has completed operations. ",
            eve_now()
        )
        .unwrap();
        f.flush().unwrap();
        assert!(read_new(&path, &mut tf).is_empty());
        writeln!(f, "Ship's cargo hold is full.").unwrap();
        f.flush().unwrap();
        assert_eq!(
            read_new(&path, &mut tf),
            vec![("Miner Alt".to_string(), AlertKind::CargoFull)]
        );

        // After the first read, lines alert regardless of timestamp
        // (no clock-skew sensitivity in steady state).
        writeln!(f, "[ 2020.01.01 00:00:00 ] (notify) Your cloak deactivates due to proximity to a nearby object.").unwrap();
        f.flush().unwrap();
        assert_eq!(
            read_new(&path, &mut tf),
            vec![("Miner Alt".to_string(), AlertKind::Decloaked)]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn override_dir_accepts_logs_or_gamelogs() {
        let base = std::env::temp_dir().join(format!("nicotine-ovr-{}", std::process::id()));
        let game = base.join("Gamelogs");
        fs::create_dir_all(&game).unwrap();
        assert_eq!(
            gamelog_dirs(Some(base.to_str().unwrap())),
            vec![game.clone()]
        );
        assert_eq!(
            gamelog_dirs(Some(game.to_str().unwrap())),
            vec![game.clone()]
        );
        assert!(gamelog_dirs(Some("/definitely/not/here")).is_empty());
        let _ = fs::remove_dir_all(&base);
    }
}
