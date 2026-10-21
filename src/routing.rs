//! Where a Mac capture of a browser tab goes. When the capture ends on a browser showing a dev
//! site, the bundle is sent to that project's OpenCode session instead of only the clipboard:
//!
//! 1. The page registered itself through Scrawl's `/sdk.js` (`web`): its session, or the session
//!    for the directory it named.
//! 2. The tab is on a loopback host: the process listening on that port, its working directory,
//!    that directory's git root, and the most recently active root session in it.
//! 3. Otherwise nothing: the capture stays on the clipboard.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::capture::scrawl_log;
use crate::handoff::{ApiGet, OpenCodeSession};
use crate::model::{Destination, ScrawlBundle};
use crate::web::Registration;

/// The parts of an `http(s)` URL routing needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebUrl {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl WebUrl {
    /// `scheme://host[:port]`, the port left out when it's the scheme's default (as browsers send
    /// `Origin`).
    pub fn origin(&self) -> String {
        let default = if self.scheme == "https" { 443 } else { 80 };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == default {
            format!("{}://{host}", self.scheme)
        } else {
            format!("{}://{host}:{}", self.scheme, self.port)
        }
    }

    pub fn is_loopback(&self) -> bool {
        is_loopback_host(&self.host)
    }
}

/// Parses `http://` and `https://` URLs (and bare origins). Hosts are lowercased; IPv6 hosts
/// lose their brackets.
pub fn parse_url(url: &str) -> Option<WebUrl> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(port) => Some(port.parse().ok()?),
            None if after.is_empty() => None,
            None => return None,
        };
        (host.to_owned(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port.parse().ok()?)),
            None => (authority.to_owned(), None),
        }
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_:".contains(&b))
    {
        return None;
    }
    let port = port.unwrap_or(if scheme == "https" { 443 } else { 80 });
    Some(WebUrl {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// `localhost`, `*.localhost`, `127.0.0.0/8`, `::1`, and `0.0.0.0` (which browsers send to this
/// machine).
pub fn is_loopback_host(host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host == "::1" || host == "0.0.0.0" {
        return true;
    }
    host.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.is_loopback())
}

// MARK: The browser tab

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Browser {
    /// Chrome, Brave, Arc, Edge, Vivaldi, Chromium: `URL of active tab of front window`.
    Chromium,
    /// Safari: `URL of current tab of front window`.
    Safari,
    /// Firefox, Zen, etc.
    Firefox,
}

pub fn browser_kind(bundle_id: &str) -> Option<Browser> {
    const CHROMIUM_MAC: [&str; 6] = [
        "com.google.chrome",
        "com.brave.browser",
        "company.thebrowser.browser",
        "com.microsoft.edgemac",
        "com.vivaldi.vivaldi",
        "org.chromium.chromium",
    ];
    let id = bundle_id.trim().to_ascii_lowercase();
    if id == "com.apple.safari" || id == "com.apple.safaritechnologypreview" {
        return Some(Browser::Safari);
    }
    // Beta, dev, nightly, and canary channels add a suffix.
    if CHROMIUM_MAC
        .iter()
        .any(|base| id == *base || id.strip_prefix(base).is_some_and(|s| s.starts_with('.')))
    {
        return Some(Browser::Chromium);
    }

    // Firefox / Zen on Linux or macOS
    const FIREFOX_NAMES: [&str; 6] = [
        "firefox",
        "org.mozilla.firefox",
        "zen",
        "zen-browser",
        "zen-alpha",
        "app.zen_browser.zen",
    ];
    if FIREFOX_NAMES.iter().any(|base| {
        id == *base
            || id.strip_prefix(base).is_some_and(|s| s.starts_with('-') || s.starts_with('.'))
    }) {
        return Some(Browser::Firefox);
    }

    // Linux WM_CLASS and desktop names
    const LINUX_CHROMIUM: [&str; 9] = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "brave-browser",
        "vivaldi",
        "vivaldi-stable",
        "microsoft-edge",
        "microsoft-edge-stable",
    ];
    if LINUX_CHROMIUM.iter().any(|base| {
        id == *base
            || id.strip_prefix(base).is_some_and(|s| s.starts_with('-') || s.starts_with('.'))
    }) {
        return Some(Browser::Chromium);
    }

    None
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn safe_bundle_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
}

/// Extracts a localhost or 127.0.0.1 development server URL from a browser window title.
pub fn extract_localhost_url_from_title(title: &str) -> Option<String> {
    let tokens = title.split(|c: char| {
        c.is_whitespace() || c == '—' || c == '–' || c == '|' || c == '(' || c == ')' || c == '[' || c == ']'
    });

    const SCHEMED_PREFIXES: [&str; 8] = [
        "http://localhost:",
        "https://localhost:",
        "http://127.0.0.1:",
        "https://127.0.0.1:",
        "http://[::1]:",
        "https://[::1]:",
        "http://0.0.0.0:",
        "https://0.0.0.0:",
    ];

    const BARE_PREFIXES: [&str; 4] = [
        "localhost:",
        "127.0.0.1:",
        "[::1]:",
        "0.0.0.0:",
    ];

    for raw in tokens {
        let token = raw.trim_matches(|c: char| {
            c.is_whitespace() || "()[]{}<>\"',;|".contains(c)
        });
        if token.is_empty() {
            continue;
        }

        let token = token.trim_start_matches('-');

        for prefix in SCHEMED_PREFIXES {
            if let Some(rest) = token.strip_prefix(prefix) {
                if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    let cleaned = token.trim_end_matches(|c: char| ".,;:!?)>]'\"-".contains(c));
                    return Some(cleaned.to_string());
                }
            }
        }

        for prefix in BARE_PREFIXES {
            if let Some(rest) = token.strip_prefix(prefix) {
                if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    let cleaned = token.trim_end_matches(|c: char| ".,;:!?)>]'\"-".contains(c));
                    return Some(format!("http://{cleaned}"));
                }
            }
        }
    }

    None
}

/// The front tab's URL in the browser `bundle_id`, through AppleScript. The first time macOS
/// asks whether Scrawl may control the browser; while that prompt is up this waits a few
/// seconds, then gives up for this capture. A denial just returns `None`.
#[cfg(target_os = "macos")]
pub fn browser_tab_url(bundle_id: &str) -> Option<String> {
    let kind = browser_kind(bundle_id)?;
    if !safe_bundle_id(bundle_id) {
        return None;
    }
    let tab = match kind {
        Browser::Chromium => "active tab",
        Browser::Safari => "current tab",
        Browser::Firefox => return None,
    };
    let script = format!(
        "tell application id \"{bundle_id}\"\nif (count of windows) is 0 then return \"\"\nreturn URL of {tab} of front window\nend tell"
    );
    let output = run_with_timeout(
        Command::new("/usr/bin/osascript").args(["-e", &script]),
        Duration::from_secs(6),
    )?;
    if !output.status.success() {
        scrawl_log(format!(
            "routing: couldn't read the {bundle_id} tab ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!url.is_empty()).then_some(url)
}

/// The front tab's URL in the browser `bundle_id` on Linux.
#[cfg(target_os = "linux")]
pub fn browser_tab_url(bundle_id: &str) -> Option<String> {
    let kind = browser_kind(bundle_id)?;

    // 1. Scan open windows (front to back) for a localhost URL in the title
    let windows = crate::windows::raw_windows();
    let title_url = windows.iter().find_map(|w| {
        let is_browser = browser_kind(&w.owner) == Some(kind)
            || bundle_id.eq_ignore_ascii_case(&w.owner)
            || browser_kind(&w.owner).is_some();
        if is_browser && !w.title.is_empty() {
            extract_localhost_url_from_title(&w.title)
        } else {
            None
        }
    });

    if let Some(url) = title_url {
        return Some(url);
    }

    // 2. Fall back to the most recent registration from web::recent_origin()
    crate::web::recent_origin()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn browser_tab_url(_bundle_id: &str) -> Option<String> {
    crate::web::recent_origin()
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                scrawl_log("routing: gave up waiting on a subprocess");
                return None;
            }
        }
    }
}

/// The URL a capture was of: the browser in front when it finished, else the captured window's
/// browser.
pub fn capture_url(
    front_bundle_id: Option<&str>,
    target_bundle_id: Option<&str>,
) -> Option<String> {
    [front_bundle_id, target_bundle_id]
        .into_iter()
        .flatten()
        .filter(|id| browser_kind(id).is_some())
        .find_map(browser_tab_url)
}

// MARK: Port → project

/// Parses `/proc/net/tcp` and `/proc/net/tcp6` lines in state `0A` (`TCP_LISTEN`) matching `port`.
pub fn parse_proc_net_tcp_inodes(content: &str, port: u16) -> Vec<u64> {
    let mut inodes = Vec::new();
    for line in content.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 10 {
            continue;
        }
        if !cols[0].ends_with(':') {
            continue;
        }
        if !cols[3].eq_ignore_ascii_case("0A") {
            continue;
        }
        let Some((_, port_hex)) = cols[1].split_once(':') else {
            continue;
        };
        let Ok(parsed_port) = u16::from_str_radix(port_hex, 16) else {
            continue;
        };
        if parsed_port != port {
            continue;
        }
        let Ok(inode) = cols[9].parse::<u64>() else {
            continue;
        };
        if inode > 0 && !inodes.contains(&inode) {
            inodes.push(inode);
        }
    }
    inodes
}

/// PIDs in `lsof -F p` output (`p1234` lines), in order, without repeats.
pub fn parse_lsof_pids(output: &str) -> Vec<i32> {
    let mut pids = Vec::new();
    for pid in output
        .lines()
        .filter_map(|line| line.strip_prefix('p')?.trim().parse::<i32>().ok())
    {
        if pid > 0 && !pids.contains(&pid) {
            pids.push(pid);
        }
    }
    pids
}

/// The directory in `lsof -a -p PID -d cwd -Fn` output (the `n` line).
pub fn parse_lsof_cwd(output: &str) -> Option<PathBuf> {
    output
        .lines()
        .find_map(|line| line.strip_prefix('n'))
        .map(str::trim)
        .filter(|path| path.starts_with('/'))
        .map(PathBuf::from)
}

fn lsof(args: &[&str]) -> Option<String> {
    const BINARIES: [&str; 3] = ["/usr/bin/lsof", "/usr/sbin/lsof", "lsof"];
    for bin in BINARIES {
        if let Some(output) = run_with_timeout(
            Command::new(bin).args(args),
            Duration::from_secs(3),
        ) {
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn lsof_listener_directory(port: u16) -> Option<PathBuf> {
    let pids = parse_lsof_pids(&lsof(&[
        "-nP",
        &format!("-iTCP:{port}"),
        "-sTCP:LISTEN",
        "-Fp",
    ])?);
    pids.into_iter().find_map(|pid| {
        parse_lsof_cwd(&lsof(&["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])?)
            .filter(|dir| dir != Path::new("/"))
    })
}

/// The working directory of the process listening on TCP `port`.
#[cfg(target_os = "linux")]
pub fn listener_directory(port: u16) -> Option<PathBuf> {
    // 1. Try reading /proc/net/tcp and /proc/net/tcp6
    let mut inodes = Vec::new();
    if let Ok(content) = std::fs::read_to_string("/proc/net/tcp") {
        for inode in parse_proc_net_tcp_inodes(&content, port) {
            if !inodes.contains(&inode) {
                inodes.push(inode);
            }
        }
    }
    if let Ok(content) = std::fs::read_to_string("/proc/net/tcp6") {
        for inode in parse_proc_net_tcp_inodes(&content, port) {
            if !inodes.contains(&inode) {
                inodes.push(inode);
            }
        }
    }

    if !inodes.is_empty() {
        if let Ok(entries) = std::fs::read_dir("/proc") {
            let mut pids = Vec::new();
            for entry in entries.flatten() {
                if let Ok(file_name) = entry.file_name().into_string() {
                    if let Ok(pid) = file_name.parse::<i32>() {
                        if pid > 0 {
                            pids.push(pid);
                        }
                    }
                }
            }
            pids.sort_unstable();
            for pid in pids {
                let fd_dir = format!("/proc/{pid}/fd");
                let Ok(fd_entries) = std::fs::read_dir(&fd_dir) else {
                    continue;
                };
                for fd_entry in fd_entries.flatten() {
                    let Ok(target) = std::fs::read_link(fd_entry.path()) else {
                        continue;
                    };
                    let target_str = target.to_string_lossy();
                    for &inode in &inodes {
                        let needle = format!("socket:[{inode}]");
                        if target_str == needle {
                            let cwd_path = format!("/proc/{pid}/cwd");
                            if let Ok(cwd) = std::fs::read_link(&cwd_path) {
                                if cwd != Path::new("/") {
                                    return Some(cwd);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Fall back to lsof if /proc yields nothing
    lsof_listener_directory(port)
}

/// The working directory of the process listening on TCP `port`.
#[cfg(not(target_os = "linux"))]
pub fn listener_directory(port: u16) -> Option<PathBuf> {
    lsof_listener_directory(port)
}

/// The project a directory belongs to: its git root (a `.git` directory or worktree file), or
/// the directory itself with `false` when it isn't in a repository.
pub fn project_root(dir: &Path) -> (PathBuf, bool) {
    let mut current = Some(dir);
    while let Some(path) = current {
        if path.join(".git").exists() {
            return (path.to_path_buf(), true);
        }
        current = path.parent();
    }
    (dir.to_path_buf(), false)
}

/// The most recently active open root session for `root`. Inside a repository a session in any
/// folder of it counts; outside one, only a session in that very folder, so a server started
/// from the home folder doesn't claim every session.
///
/// The server filters by exact directory, so that takes one request. A session in a subfolder
/// only wins if it's newer, so the newest-first scan for one stops at the exact match's age (or
/// after [`ROOT_SEARCH_PAGES`](crate::handoff::ROOT_SEARCH_PAGES)).
pub fn session_for_directory(
    get: &mut ApiGet,
    root: &Path,
    whole_tree: bool,
) -> Result<Option<OpenCodeSession>> {
    use crate::handoff::{ROOT_SEARCH_PAGES, Scan, find_root_session, query_escape};
    let home = dirs::home_dir();
    if root == Path::new("/") || Some(root) == home.as_deref() {
        return Ok(None);
    }
    let directory = format!("&directory={}", query_escape(&root.to_string_lossy()));
    let exact = find_root_session(get, &directory, ROOT_SEARCH_PAGES, |_| Scan::Take)?;
    if !whole_tree {
        return Ok(exact);
    }
    let floor = exact.as_ref().map(|s| s.time.updated);
    let nested = find_root_session(get, "", ROOT_SEARCH_PAGES, |s| {
        if floor.is_some_and(|floor| s.time.updated <= floor) {
            Scan::Stop
        } else if s
            .directory()
            .is_some_and(|dir| Path::new(dir).starts_with(root))
        {
            Scan::Take
        } else {
            Scan::Skip
        }
    })?;
    Ok(nested.or(exact))
}

// MARK: Deciding

/// The outside steps of a decision, swappable in tests.
#[derive(Clone, Copy)]
pub struct Hooks {
    pub sdk: fn(&str) -> Option<Registration>,
    pub listener_dir: fn(u16) -> Option<PathBuf>,
    /// One `GET` against OpenCode's API.
    pub api: fn(&str) -> Result<Vec<u8>>,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            sdk: crate::web::lookup,
            listener_dir: listener_directory,
            api: crate::handoff::api_get,
        }
    }
}

/// Where a capture of `url` goes, or why it goes nowhere.
pub fn route(url: &str, hooks: &Hooks) -> std::result::Result<Destination, String> {
    let parsed = parse_url(url).ok_or_else(|| format!("{url} isn't an http(s) URL"))?;
    let destination = |session: &OpenCodeSession, via: &str, dir: Option<&Path>| Destination {
        session: session.id.clone(),
        title: session.label().to_owned(),
        via: via.to_owned(),
        url: Some(url.to_owned()),
        directory: dir.map(|d| d.display().to_string()),
    };
    let mut api = hooks.api;
    let by_directory = |dir: &Path, via: &str| {
        let (root, git) = project_root(dir);
        let mut api = hooks.api;
        session_for_directory(&mut api, &root, git)
            .map_err(|err| format!("couldn't list sessions: {err:#}"))?
            .map(|s| destination(&s, via, Some(&root)))
            .ok_or_else(|| format!("no open OpenCode session in {}", root.display()))
    };

    if let Some(reg) = (hooks.sdk)(url) {
        if let Some(id) = reg.session.as_deref() {
            let title = crate::handoff::get_session(&mut api, id)
                .map_or_else(|_| id.to_owned(), |s| s.label().to_owned());
            return Ok(Destination {
                session: id.to_owned(),
                title,
                via: "sdk".into(),
                url: Some(url.to_owned()),
                directory: reg.directory,
            });
        }
        if let Some(dir) = reg.directory.as_deref() {
            return by_directory(Path::new(dir), "sdk");
        }
    }
    if !parsed.is_loopback() {
        return Err(format!("{} isn't a local dev server", parsed.origin()));
    }
    let dir = (hooks.listener_dir)(parsed.port)
        .ok_or_else(|| format!("no process found listening on port {}", parsed.port))?;
    by_directory(&dir, "localhost")
}

/// Decides where a capture that just finished goes; `None` (logged) keeps it on the clipboard.
pub fn route_capture(
    front_bundle_id: Option<&str>,
    target_bundle_id: Option<&str>,
) -> Option<Destination> {
    if !crate::handoff::auto_send() || std::env::var_os("SCRAWL_NO_OPENCODE").is_some() {
        return None;
    }
    let url = capture_url(front_bundle_id, target_bundle_id)?;
    match route(&url, &Hooks::default()) {
        Ok(dest) => {
            scrawl_log(format!(
                "routing: {url} → {} ({}) via {}",
                dest.title, dest.session, dest.via
            ));
            Some(dest)
        }
        Err(reason) => {
            scrawl_log(format!("routing: {url} stays on the clipboard: {reason}"));
            None
        }
    }
}

/// Writes `dest` into the bundle's `scrawl.json` and `summary.md`.
pub fn record_destination(dir: &Path, bundle: &mut ScrawlBundle, dest: Destination) -> Result<()> {
    bundle.destination = Some(dest);
    std::fs::write(dir.join("scrawl.json"), serde_json::to_vec_pretty(bundle)?)?;
    std::fs::write(
        dir.join("summary.md"),
        crate::sketch::summary_markdown(bundle, dir),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::{OpenCodeSessionLocation, OpenCodeSessionTime};

    fn session(id: &str, dir: &str, updated: u64) -> OpenCodeSession {
        OpenCodeSession {
            id: id.into(),
            title: id.to_uppercase(),
            parent_id: None,
            time: OpenCodeSessionTime {
                updated,
                archived: None,
            },
            location: Some(OpenCodeSessionLocation {
                directory: Some(dir.into()),
            }),
            directory: None,
        }
    }

    #[test]
    fn urls_parse_to_origins() {
        let u = parse_url("http://localhost:5173/a/b?c#d").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("localhost", 5173));
        assert_eq!(u.origin(), "http://localhost:5173");
        assert!(u.is_loopback());
        assert_eq!(
            parse_url("HTTPS://User:pw@App.Example.com/x")
                .unwrap()
                .origin(),
            "https://app.example.com"
        );
        assert_eq!(parse_url("http://example.com").unwrap().port, 80);
        let v6 = parse_url("http://[::1]:3000/").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 3000));
        assert_eq!(v6.origin(), "http://[::1]:3000");
        assert!(v6.is_loopback());
        assert!(parse_url("http://127.0.0.1:8080").unwrap().is_loopback());
        assert!(
            parse_url("http://my-app.localhost:1355/")
                .unwrap()
                .is_loopback()
        );
        assert!(
            !parse_url("http://studio.example.ts.net:5173/")
                .unwrap()
                .is_loopback()
        );
        assert!(
            !parse_url("http://localhost.evil.com/")
                .unwrap()
                .is_loopback()
        );
        assert!(parse_url("file:///tmp/x.html").is_none());
        assert!(parse_url("chrome://newtab").is_none());
        assert!(parse_url("http://localhost:notaport/").is_none());
        assert!(parse_url("http:///nohost").is_none());
    }

    #[test]
    fn browsers_are_recognized_by_bundle_id() {
        assert_eq!(browser_kind("com.brave.Browser"), Some(Browser::Chromium));
        assert_eq!(
            browser_kind("com.brave.Browser.beta"),
            Some(Browser::Chromium)
        );
        assert_eq!(
            browser_kind("com.google.Chrome.canary"),
            Some(Browser::Chromium)
        );
        assert_eq!(
            browser_kind("company.thebrowser.Browser"),
            Some(Browser::Chromium)
        );
        assert_eq!(browser_kind("com.apple.Safari"), Some(Browser::Safari));
        assert_eq!(browser_kind("com.mitchellh.ghostty"), None);
        assert_eq!(browser_kind("com.google.Chromecast"), None);
    }

    #[test]
    fn proc_net_tcp_parser_finds_listening_ipv4_and_ipv6_inodes_and_skips_established_clients() {
        // Review Focus #5: state 0A (TCP_LISTEN) on IPv4 (127.0.0.1:5173 -> 0100007F:1435),
        // IPv6 ::1:5173, and IPv4-mapped ::ffff:127.0.0.1:3000 (0BB8), while ignoring state 01 (TCP_ESTABLISHED).
        let tcp4 = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 424242 1 0000000000000000 100 0 0 10 0
   1: 0100007F:1435 0100007F:D120 01 00000000:00000000 00:00000000 00000000  1000        0 999999 1 0000000000000000 20 4 30 10 -1
";
        assert_eq!(parse_proc_net_tcp_inodes(tcp4, 5173), vec![424242]);
        assert!(parse_proc_net_tcp_inodes(tcp4, 3000).is_empty());

        let tcp6 = "\
  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:1435 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 555555 1 0000000000000000 100 0 0 10 0
   1: 0000000000000000FFFF00000100007F:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 777777 1 0000000000000000 100 0 0 10 0
";
        assert_eq!(parse_proc_net_tcp_inodes(tcp6, 5173), vec![555555]);
        assert_eq!(parse_proc_net_tcp_inodes(tcp6, 3000), vec![777777]);
    }

    #[test]
    fn browser_kind_and_window_title_url_extraction_work_for_linux_browsers() {
        assert!(browser_kind("google-chrome").is_some());
        assert!(browser_kind("firefox").is_some());
        assert!(browser_kind("brave-browser").is_some());
        assert_eq!(
            extract_localhost_url_from_title("localhost:5173/dashboard - Google Chrome").as_deref(),
            Some("http://localhost:5173/dashboard")
        );
        assert_eq!(
            extract_localhost_url_from_title("http://127.0.0.1:3000 — Mozilla Firefox").as_deref(),
            Some("http://127.0.0.1:3000")
        );
    }

    #[test]
    fn lsof_output_parses() {
        let listen = "p48211\ncnode\np48211\np50002\n";
        assert_eq!(parse_lsof_pids(listen), vec![48211, 50002]);
        assert!(parse_lsof_pids("").is_empty());
        let cwd = "p48211\nfcwd\nn/Users/kit/code/open-source/scrawl/site\n";
        assert_eq!(
            parse_lsof_cwd(cwd),
            Some(PathBuf::from("/Users/kit/code/open-source/scrawl/site"))
        );
        assert_eq!(parse_lsof_cwd("p1\nfcwd\n"), None);
    }

    #[test]
    fn project_root_walks_up_to_git() {
        let base = std::env::temp_dir().join(format!("scrawl-root-{}", crate::make_bundle_id()));
        let site = base.join("repo/site/src");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::create_dir_all(base.join("repo/.git")).unwrap();
        assert_eq!(project_root(&site), (base.join("repo"), true));
        let loose = base.join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        // The temp folder isn't in a repository.
        assert_eq!(project_root(&loose), (loose.clone(), false));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn sessions_are_chosen_by_directory() {
        let mut child = session("ses_child", "/p/app", 900);
        child.parent_id = Some("ses_app".into());
        let mut archived = session("ses_old", "/p/app", 800);
        archived.time.archived = Some(1);
        let mut sessions = vec![
            session("ses_app", "/p/app", 100),
            session("ses_site", "/p/app/site", 200),
            session("ses_other", "/p/application", 300),
            child,
            archived,
        ];
        // Thousands of newer sessions elsewhere.
        sessions.extend((0..3_000).map(|i| session(&format!("ses_x{i}"), "/q", 1_000 + i)));
        let pick = |sessions: &[OpenCodeSession], root: &str, tree| {
            let mut requests = 0;
            let found = session_for_directory(
                &mut |path: &str| {
                    requests += 1;
                    crate::handoff::fake_api::serve(sessions, path)
                },
                Path::new(root),
                tree,
            )
            .unwrap()
            .map(|s| s.id);
            (found, requests)
        };
        // Exact folder: one filtered request.
        assert_eq!(pick(&sessions, "/p/app", false), (Some("ses_app".into()), 1));
        // A subfolder needs a newest-first scan, which is capped: past it the exact match stands.
        let (found, requests) = pick(&sessions, "/p/app", true);
        assert_eq!(found.as_deref(), Some("ses_app"));
        assert_eq!(requests, 1 + crate::handoff::ROOT_SEARCH_PAGES);
        assert_eq!(pick(&sessions, "/p/none", false), (None, 1));
        assert_eq!(pick(&sessions, "/", true), (None, 0));

        // Recent work in a subfolder beats an older exact match; a sibling with a common prefix
        // doesn't count.
        sessions.truncate(5);
        let (found, requests) = pick(&sessions, "/p/app", true);
        assert_eq!(found.as_deref(), Some("ses_site"));
        assert_eq!(requests, 2);
        // An exact match newer than everything else stops the scan at once.
        sessions[0].time.updated = 1_000;
        assert_eq!(pick(&sessions, "/p/app", true), (Some("ses_app".into()), 2));
    }

    fn no_sdk(_: &str) -> Option<Registration> {
        None
    }
    fn sdk_session(url: &str) -> Option<Registration> {
        url.starts_with("http://localhost:5173")
            .then(|| Registration {
                origin: "http://localhost:5173".into(),
                url: url.into(),
                session: Some("ses_sdk".into()),
                ..Registration::default()
            })
    }
    fn sdk_directory(_: &str) -> Option<Registration> {
        Some(Registration {
            origin: "https://studio.ts.net:5173".into(),
            directory: Some("/p/app".into()),
            ..Registration::default()
        })
    }
    fn listener(port: u16) -> Option<PathBuf> {
        (port == 4321).then(|| PathBuf::from("/p/app"))
    }
    fn api(path: &str) -> Result<Vec<u8>> {
        crate::handoff::fake_api::serve(
            &[
                session("ses_sdk", "/elsewhere", 5),
                session("ses_app", "/p/app", 10),
            ],
            path,
        )
    }

    #[test]
    fn route_prefers_sdk_then_localhost_port() {
        let hooks = Hooks {
            sdk: sdk_session,
            listener_dir: listener,
            api,
        };
        let sdk = route("http://localhost:5173/page", &hooks).unwrap();
        assert_eq!((sdk.session.as_str(), sdk.via.as_str()), ("ses_sdk", "sdk"));
        assert_eq!(sdk.title, "SES_SDK");

        let hooks = Hooks {
            sdk: no_sdk,
            ..hooks
        };
        // `/p/app` isn't a repository here, so only a session in exactly that folder matches.
        let local = route("http://127.0.0.1:4321/", &hooks).unwrap();
        assert_eq!(
            (
                local.session.as_str(),
                local.via.as_str(),
                local.directory.as_deref()
            ),
            ("ses_app", "localhost", Some("/p/app"))
        );
        assert!(route("http://localhost:9999/", &hooks).is_err());
        assert!(route("https://example.com/", &hooks).is_err());

        let hooks = Hooks {
            sdk: sdk_directory,
            ..hooks
        };
        let by_dir = route("https://studio.ts.net:5173/", &hooks).unwrap();
        assert_eq!(
            (by_dir.session.as_str(), by_dir.via.as_str()),
            ("ses_app", "sdk")
        );
    }

    #[test]
    fn destination_is_recorded_and_preferred_by_send() {
        let dir =
            std::env::temp_dir().join(format!("scrawl-route-{}.scrawl", crate::make_bundle_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut bundle: ScrawlBundle = serde_json::from_value(serde_json::json!({
            "version": 1, "id": "t", "createdAtMs": 0, "durationMs": 1_000,
            "target": {"mode": "display", "displayId": 1,
                "frame": {"x": 0.0, "y": 0.0, "width": 10.0, "height": 10.0, "scale": 1.0}},
            "transcript": {"source": "none", "text": "", "segments": []},
            "strokes": [], "marks": [], "events": [], "keyframes": [], "contactSheet": "c.png"
        }))
        .unwrap();
        assert_eq!(crate::handoff::recorded_destination(&dir), None);
        record_destination(
            &dir,
            &mut bundle,
            Destination {
                session: "ses_app".into(),
                title: "APP".into(),
                via: "localhost".into(),
                url: Some("http://localhost:4321/".into()),
                directory: None,
            },
        )
        .unwrap();
        let summary = std::fs::read_to_string(dir.join("summary.md")).unwrap();
        assert!(
            summary.contains("For session: APP (ses_app) · page http://localhost:4321/"),
            "{summary}"
        );
        let (_, loaded) = crate::handoff::load_bundle(&dir).unwrap();
        assert_eq!(loaded.destination.unwrap().via, "localhost");
        assert_eq!(
            crate::handoff::recorded_destination(&dir).as_deref(),
            Some("ses_app")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
