//! Small, dependency-free helpers shared by the CLI and the TUI.

use std::cmp::Ordering;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// First 7 characters of a commit sha (or the whole string when shorter).
pub fn short_sha(sha: &str) -> &str {
    let end = sha
        .char_indices()
        .nth(7)
        .map(|(i, _)| i)
        .unwrap_or(sha.len());
    &sha[..end]
}

/// A full 40 (SHA-1) or 64 (SHA-256) hex object id.
pub fn is_full_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Something that could be an abbreviated commit id.
pub fn looks_like_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// FNV-1a 64-bit, hex encoded. Stable across runs and platforms; used to name
/// cache directories, never for anything security related.
pub fn fnv1a64(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// A readable, filesystem-safe fragment: `ssh://git@host/a/b.git` → `host-a-b`.
pub fn slug(value: &str, max: usize) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            out.push(c);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= max {
            break;
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '.').to_string();
    if trimmed.is_empty() {
        "x".to_string()
    } else {
        trimmed
    }
}

pub fn current_platform() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "unknown"
    }
}

/// Minimal semantic version: `major.minor.patch[-pre][+build]`. Herdr's own
/// versions look like `0.9.5` or `0.9.5-preview.abc`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Option<String>,
}

impl Version {
    pub fn parse(value: &str) -> Option<Version> {
        let value = value.trim().trim_start_matches('v');
        let value = value.split('+').next()?;
        let (core, pre) = match value.split_once('-') {
            Some((core, pre)) => (core, Some(pre.to_string())),
            None => (value, None),
        };
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = u64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// `2026-10-25 14:03 UTC`
pub fn format_unix(ts: u64) -> String {
    if ts == 0 {
        return "-".to_string();
    }
    let days = (ts / 86_400) as i64;
    let secs = ts % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// Parses the ISO 8601 timestamps forges return (`2026-10-08T12:34:56Z`,
/// `2026-10-08T12:34:56.123+02:00`). Returns unix seconds.
pub fn parse_iso8601(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.len() < 10 {
        return None;
    }
    let y: i64 = value.get(0..4)?.parse().ok()?;
    let m: u32 = value.get(5..7)?.parse().ok()?;
    let d: u32 = value.get(8..10)?.parse().ok()?;
    let (mut hh, mut mm, mut ss) = (0u64, 0u64, 0u64);
    let mut offset: i64 = 0;
    if value.len() >= 19 {
        hh = value.get(11..13)?.parse().ok()?;
        mm = value.get(14..16)?.parse().ok()?;
        ss = value.get(17..19)?.parse().ok()?;
        let rest = &value[19..];
        let rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
        if let Some(sign) = rest.chars().next() {
            if (sign == '+' || sign == '-') && rest.len() >= 6 {
                let oh: i64 = rest.get(1..3)?.parse().ok()?;
                let om: i64 = rest.get(4..6)?.parse().ok()?;
                offset = (oh * 3600 + om * 60) * if sign == '-' { -1 } else { 1 };
            }
        }
    }
    let days = days_from_civil(y, m, d);
    let total = days * 86_400 + (hh * 3600 + mm * 60 + ss) as i64 - offset;
    u64::try_from(total).ok()
}

/// `3d ago`, `5h ago`, `just now`.
pub fn relative_age(ts: u64) -> String {
    if ts == 0 {
        return "-".to_string();
    }
    let now = now_unix();
    let delta = now.saturating_sub(ts);
    match delta {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86_399 => format!("{}h ago", delta / 3600),
        86_400..=2_591_999 => format!("{}d ago", delta / 86_400),
        2_592_000..=31_535_999 => format!("{}mo ago", delta / 2_592_000),
        _ => format!("{}y ago", delta / 31_536_000),
    }
}

/// Truncates to `max` characters, ending with `…` when something was cut.
pub fn truncate(value: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = value.chars().count();
    if count <= max {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Replaces control characters (including tabs/newlines) with spaces so text
/// from the network can never move the cursor or break a TUI row.
pub fn sanitize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_control() {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn stdin_is_tty() -> bool {
    io::stdin().is_terminal()
}

/// `[y/N]` prompt on stderr. Anything but y/yes is a no.
pub fn confirm(prompt: &str) -> Result<bool> {
    eprint!("{prompt} [y/N] ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
}

/// Splits an input line like a shell would for simple cases: whitespace
/// separates words, single or double quotes group them, backslash escapes.
pub fn split_words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(ch);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    in_word = true;
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            (None, c) => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

/// Waits for a child with a deadline, killing it when the deadline passes.
pub fn wait_with_timeout(mut child: Child, timeout: Duration) -> Result<Output> {
    let start = Instant::now();
    // Drain pipes on threads so a chatty child can never block on a full pipe.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader = stdout.map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut s, &mut buf);
            buf
        })
    });
    let err_reader = stderr.map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut s, &mut buf);
            buf
        })
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

pub fn run_captured(mut command: Command, timeout: Option<Duration>) -> Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let program = format!("{:?}", command.get_program());
    let child = command
        .spawn()
        .with_context(|| format!("could not start {program}"))?;
    match timeout {
        Some(timeout) => wait_with_timeout(child, timeout),
        None => Ok(child.wait_with_output()?),
    }
}

/// Opens a URL with the platform's default handler, detached and silent.
pub fn open_url(url: &str) -> Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().context("could not open a browser")?;
    Ok(())
}

/// Turns a git remote into a browsable https URL when the shape is familiar.
/// `git@host:team/repo.git` → `https://host/team/repo`. Local paths and
/// `git://`/`file://` remotes have no web page and return None.
pub fn web_url_for_remote(remote: &str) -> Option<String> {
    let remote = remote.trim();
    let strip_git = |s: &str| s.trim_end_matches('/').trim_end_matches(".git").to_string();
    if let Some(rest) = remote
        .strip_prefix("https://")
        .or_else(|| remote.strip_prefix("http://"))
    {
        let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
        let scheme = if remote.starts_with("http://") {
            "http"
        } else {
            "https"
        };
        return Some(format!("{scheme}://{}", strip_git(rest)));
    }
    if let Some(rest) = remote.strip_prefix("ssh://") {
        let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
        let (host, path) = rest.split_once('/')?;
        let host = host.split(':').next().unwrap_or(host);
        return Some(format!("https://{host}/{}", strip_git(path)));
    }
    if !remote.contains("://") {
        if let Some((user_host, path)) = remote.split_once(':') {
            if user_host.contains('@') && !path.starts_with('/') {
                let host = user_host
                    .split_once('@')
                    .map(|(_, h)| h)
                    .unwrap_or(user_host);
                return Some(format!("https://{host}/{}", strip_git(path)));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("0.9.5") > v("0.9.4"));
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("0.9.5") > v("0.9.5-preview.1"));
        assert_eq!(v("v1.2"), v("1.2.0"));
        assert!(Version::parse("abc").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
    }

    #[test]
    fn iso_dates_roundtrip() {
        let ts = parse_iso8601("2026-10-08T12:34:56Z").unwrap();
        assert_eq!(format_unix(ts), "2026-10-08 12:34 UTC");
        let with_offset = parse_iso8601("2026-10-08T14:34:56.123+02:00").unwrap();
        assert_eq!(with_offset, ts);
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn words_split_like_a_shell() {
        assert_eq!(
            split_words(r#"ssh://h/a.git --ref "v 1" --subdir 'x y' a\ b"#),
            vec!["ssh://h/a.git", "--ref", "v 1", "--subdir", "x y", "a b"]
        );
        assert!(split_words("   ").is_empty());
    }

    #[test]
    fn slugs_are_safe() {
        assert_eq!(
            slug("ssh://git@host:22/a/B.git", 64),
            "ssh-git-host-22-a-b.git"
        );
        assert_eq!(slug("///", 10), "x");
    }

    #[test]
    fn web_urls() {
        assert_eq!(
            web_url_for_remote("git@git.corp.local:team/repo.git").as_deref(),
            Some("https://git.corp.local/team/repo")
        );
        assert_eq!(
            web_url_for_remote("ssh://git@host:2222/team/repo.git").as_deref(),
            Some("https://host/team/repo")
        );
        assert_eq!(
            web_url_for_remote("https://tok@gitlab.x/a/b.git").as_deref(),
            Some("https://gitlab.x/a/b")
        );
        assert_eq!(web_url_for_remote("/srv/git/a.git"), None);
        assert_eq!(web_url_for_remote("git://host/a.git"), None);
    }

    #[test]
    fn sha_helpers() {
        assert!(is_full_sha(&"a".repeat(40)));
        assert!(!is_full_sha("main"));
        assert!(looks_like_sha("abc1234"));
        assert!(!looks_like_sha("v1.0.0"));
        assert_eq!(short_sha("0123456789"), "0123456");
    }
}
