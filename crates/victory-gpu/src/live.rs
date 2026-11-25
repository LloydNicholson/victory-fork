//! Switches changed while the title runs, and markers the player sets.
//!
//! `VICTORY_LIVE=<file>`: a file of `NAME=value` lines (`#` starts a
//! comment; the `VICTORY_` prefix may be left out), looked at four times a
//! second by a thread of its own. A switch read through [`number`] or
//! [`flag`] takes the file's value while the file names it, and its usual
//! one (the environment's) otherwise, so it is changed in the middle of a
//! session without a restart. Only switches whose readers say so are live:
//! most are read once at the start.
//!
//! `ab=NAME:a/b:secs` alternates NAME between a and b every `secs` seconds
//! (counted from when the line appeared) and logs each phase's frames and
//! frame rate, with the running totals per value: an A/B comparison in one
//! walk, the scenes changing under both alike.
//!
//! F12 (with `VICTORY_LIVE` set): a marker line in the log with the frame
//! number and, in diagnostics builds with `VICTORY_CAPTURE`, the next two
//! frames saved as pictures. For the player to say "this, now" without
//! stopping to describe it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

type Values = BTreeMap<String, String>;

/// Frames presented, counted here (every build).
static FRAMES: AtomicU64 = AtomicU64::new(0);

/// The values in force: the file's, with an A/B switch's current one.
static VALUES: RwLock<Option<Arc<Values>>> = RwLock::new(None);

fn path() -> Option<&'static Path> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| std::env::var_os("VICTORY_LIVE").map(PathBuf::from)).as_deref()
}

/// At each present: counts the frame, and starts the thread the first time.
pub(crate) fn frame() {
    FRAMES.fetch_add(1, Relaxed);
    if let Some(path) = path() {
        static STARTED: OnceLock<()> = OnceLock::new();
        STARTED.get_or_init(|| {
            tracing::info!(file = %path.display(), "live switches: the file is read four times a second, F12 sets a marker");
            let path = path.to_owned();
            let _ = std::thread::Builder::new().name("victory-live".into()).spawn(move || run(&path));
        });
    }
}

/// The file's value for `name` (upper case, without `VICTORY_`), if it
/// names it.
fn with<R>(name: &str, f: impl FnOnce(&str) -> Option<R>) -> Option<R> {
    path()?;
    let values = VALUES.read().ok()?;
    f(values.as_ref()?.get(name)?.trim())
}

/// A live number switch: `None` while the file does not name it (the
/// caller's usual value applies).
pub(crate) fn number(name: &str) -> Option<u64> {
    with(name, |v| v.parse().ok())
}

/// A live text switch, as the file gives it (trimmed).
pub(crate) fn text(name: &str) -> Option<String> {
    with(name, |v| Some(v.to_owned()))
}

/// A live on/off switch (`1`/`0`, `on`/`off`, `true`/`false`).
pub(crate) fn flag(name: &str) -> Option<bool> {
    with(name, parse_flag)
}

fn parse_flag(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Some(true),
        "0" | "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// `NAME=value` lines; names in upper case without `VICTORY_`.
fn parse(text: &str) -> Values {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| {
            let k = k.trim().to_ascii_uppercase();
            let k = k.strip_prefix("VICTORY_").map(str::to_owned).unwrap_or(k);
            (k, v.trim().to_owned())
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// An `ab=NAME:a/b:secs` line in force.
struct Ab {
    line: String,
    name: String,
    values: [String; 2],
    every: Duration,
    began: Instant,
    /// The phase in force, when it began and the frame count then.
    phase: u64,
    phase_began: (Instant, u64),
    /// Per value: time and frames over the phases ended.
    totals: [(Duration, u64); 2],
}

impl Ab {
    fn parse(line: &str, now: Instant, frames: u64) -> Option<Self> {
        let mut parts = line.split(':');
        let name = parts.next()?.trim().to_ascii_uppercase();
        let name = name.strip_prefix("VICTORY_").map(str::to_owned).unwrap_or(name);
        let (a, b) = parts.next()?.split_once('/')?;
        let secs: f64 = parts.next().unwrap_or("10").trim().trim_end_matches('s').parse().ok()?;
        if name.is_empty() || parts.next().is_some() || !(secs >= 1.0) {
            return None;
        }
        Some(Self {
            line: line.to_owned(),
            name,
            values: [a.trim().to_owned(), b.trim().to_owned()],
            every: Duration::from_secs_f64(secs),
            began: now,
            phase: 0,
            phase_began: (now, frames),
            totals: [(Duration::ZERO, 0); 2],
        })
    }

    /// The value in force at `now`; a phase that ended is added to its
    /// value's totals and logged (the line is returned for tests).
    fn tick(&mut self, now: Instant, frames: u64) -> (&str, Option<String>) {
        let phase = (now.duration_since(self.began).as_secs_f64() / self.every.as_secs_f64()) as u64;
        let mut ended = None;
        if phase != self.phase {
            let (at, from) = self.phase_began;
            let time = now.duration_since(at);
            let count = frames - from;
            let i = (self.phase % 2) as usize;
            self.totals[i].0 += time;
            self.totals[i].1 += count;
            let fps = |t: Duration, n: u64| if t.is_zero() { 0.0 } else { n as f64 / t.as_secs_f64() };
            let so_far = (0..2)
                .map(|j| {
                    let (t, n) = self.totals[j];
                    format!("{}={}: {:.2} fps over {:.0} s", self.name, self.values[j], fps(t, n), t.as_secs_f64())
                })
                .collect::<Vec<_>>()
                .join(", ");
            ended = Some(format!(
                "live A/B: {}={} for {:.1} s: {count} frames, {:.2} fps; so far {so_far}",
                self.name,
                self.values[i],
                time.as_secs_f64(),
                fps(time, count),
            ));
            self.phase = phase;
            self.phase_began = (now, frames);
        }
        (&self.values[(self.phase % 2) as usize], ended)
    }
}

/// The thread: the file four times a second, the A/B phases, F12.
fn run(path: &Path) {
    let mut seen: Option<(Option<std::time::SystemTime>, u64)> = None;
    let mut file = Values::new();
    let mut ab: Option<Ab> = None;
    let mut published = Values::new();
    let mut read_at = Instant::now() - Duration::from_secs(1);
    let mut key_was = false;
    loop {
        std::thread::sleep(Duration::from_millis(25));
        let down = f12_down();
        if down && !key_was {
            marker();
        }
        key_was = down;
        let now = Instant::now();
        if now.duration_since(read_at) < Duration::from_millis(250) {
            continue;
        }
        read_at = now;
        let meta = std::fs::metadata(path).ok().map(|m| (m.modified().ok(), m.len()));
        if meta != seen {
            seen = meta;
            let new = parse(&std::fs::read_to_string(path).unwrap_or_default());
            if new != file {
                let frame = FRAMES.load(Relaxed);
                for (k, v) in &new {
                    if file.get(k) != Some(v) {
                        tracing::info!(frame, "live: {k}={v} from now");
                    }
                }
                for k in file.keys().filter(|k| !new.contains_key(*k)) {
                    tracing::info!(frame, "live: {k} back to its usual value");
                }
                file = new;
            }
            let line = file.get("AB").map(String::as_str).unwrap_or("");
            if ab.as_ref().map_or("", |a| a.line.as_str()) != line {
                ab = Ab::parse(line, now, FRAMES.load(Relaxed));
                match &ab {
                    Some(a) => tracing::info!(
                        "live A/B: {} alternates {} and {} every {:.0} s",
                        a.name,
                        a.values[0],
                        a.values[1],
                        a.every.as_secs_f64()
                    ),
                    None if !line.is_empty() => tracing::warn!("live A/B: not understood (ab=NAME:a/b:secs): {line}"),
                    None => {}
                }
            }
        }
        let mut values = file.clone();
        values.remove("AB");
        if let Some(a) = &mut ab {
            let (value, ended) = a.tick(now, FRAMES.load(Relaxed));
            let value = value.to_owned();
            values.insert(a.name.clone(), value);
            if let Some(line) = ended {
                tracing::info!("{line}");
            }
        }
        if values != published {
            if let Ok(mut v) = VALUES.write() {
                *v = Some(Arc::new(values.clone()));
            }
            published = values;
        }
    }
}

#[cfg(windows)]
fn f12_down() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F12};
    // SAFETY: a plain query.
    unsafe { GetAsyncKeyState(i32::from(VK_F12)) < 0 }
}

#[cfg(not(windows))]
fn f12_down() -> bool {
    false
}

/// F12: a line to find in the log, and the frames after it saved.
fn marker() {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let n = COUNT.fetch_add(1, Relaxed) + 1;
    let frame = FRAMES.load(Relaxed);
    #[cfg(all(windows, feature = "diagnostics"))]
    let saved = crate::capture::request();
    #[cfg(not(all(windows, feature = "diagnostics")))]
    let saved: Option<String> = None;
    tracing::warn!(marker = n, frame, saved = saved.as_deref().unwrap_or("nothing (no VICTORY_CAPTURE)"), "live marker (F12)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_name_switches_with_or_without_the_prefix() {
        let v = parse("# a comment\nVICTORY_SUBMIT_GROUP = 8\ntable_cbv_fill=1 # fill them\n\nnot a line\nab=SUBMIT_GROUP:1/8:10\n");
        assert_eq!(v.get("SUBMIT_GROUP").map(String::as_str), Some("8"));
        assert_eq!(v.get("TABLE_CBV_FILL").map(String::as_str), Some("1"));
        assert_eq!(v.get("AB").map(String::as_str), Some("SUBMIT_GROUP:1/8:10"));
        assert_eq!(v.len(), 3);
        assert_eq!(parse_flag("On"), Some(true));
        assert_eq!(parse_flag("0"), Some(false));
        assert_eq!(parse_flag("maybe"), None);
    }

    #[test]
    fn ab_alternates_and_counts_each_value() {
        let t0 = Instant::now();
        assert!(Ab::parse("SUBMIT_GROUP:1/8", t0, 0).is_some_and(|a| a.every == Duration::from_secs(10)));
        assert!(Ab::parse("SUBMIT_GROUP:18:10", t0, 0).is_none());
        assert!(Ab::parse("SUBMIT_GROUP:1/8:0.5", t0, 0).is_none());
        let mut ab = Ab::parse("victory_submit_group:1/8:10s", t0, 100).unwrap();
        assert_eq!(ab.name, "SUBMIT_GROUP");
        let s = Duration::from_secs;
        assert_eq!(ab.tick(t0 + s(5), 120), ("1", None));
        let (v, line) = ab.tick(t0 + s(10), 140);
        assert_eq!(v, "8");
        let line = line.unwrap();
        assert!(line.contains("SUBMIT_GROUP=1 for 10.0 s: 40 frames, 4.00 fps"), "{line}");
        assert_eq!(ab.tick(t0 + s(15), 150).0, "8");
        let (v, line) = ab.tick(t0 + s(20), 160);
        assert_eq!(v, "1");
        assert!(line.unwrap().contains("SUBMIT_GROUP=1: 4.00 fps over 10 s, SUBMIT_GROUP=8: 2.00 fps over 10 s"));
    }
}
