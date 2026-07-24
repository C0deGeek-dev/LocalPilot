//! Discovering and launching a headless Chromium-family browser.
//!
//! No browser is bundled or downloaded: the renderer drives whatever
//! Chromium/Chrome/Edge the machine already has, discovered by an environment
//! override or the platform's well-known install paths. The browser runs
//! headless with an ephemeral, throwaway profile (a fresh temp `--user-data-dir`
//! removed on drop), so no cookies, storage, or authentication state survive the
//! render. It is killed on drop.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::time::Instant;

use crate::RenderError;

/// Environment override naming the browser executable to use, ahead of the
/// well-known install paths — for a portable install or a pinned binary.
const BROWSER_ENV: &str = "LOCALPILOT_RENDER_BROWSER";

/// A launched headless browser and its CDP browser-endpoint URL. Killed and its
/// ephemeral profile removed on drop.
pub(crate) struct Browser {
    child: tokio::process::Child,
    ws_url: String,
    _profile: tempfile::TempDir,
}

impl Browser {
    /// The CDP browser-endpoint WebSocket URL.
    pub(crate) fn ws_url(&self) -> &str {
        &self.ws_url
    }

    /// Whether a usable browser executable can be found — used to answer
    /// "is a renderer available?" without launching anything.
    pub(crate) fn discover() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(BROWSER_ENV) {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Some(path);
            }
        }
        candidate_paths()
            .into_iter()
            .find(|candidate| candidate.is_file())
    }

    /// Launch a headless browser and wait (up to `timeout`) for its DevTools
    /// endpoint to come up.
    pub(crate) async fn launch(timeout: Duration) -> Result<Self, RenderError> {
        let exe = Self::discover().ok_or(RenderError::NoBrowser)?;
        let profile = tempfile::tempdir()?;
        let child = tokio::process::Command::new(&exe)
            .arg("--headless=new")
            .arg("--disable-gpu")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-extensions")
            .arg("--disable-background-networking")
            .arg("--disable-sync")
            .arg("--disable-component-update")
            .arg("--mute-audio")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| RenderError::Launch(error.to_string()))?;

        // Chromium writes the chosen port and browser path to DevToolsActivePort
        // in the profile once the debug server is listening. Poll for it rather
        // than scraping stderr (locale- and format-stable across builds).
        let port_file = profile.path().join("DevToolsActivePort");
        let ws_url = wait_for_devtools(&port_file, timeout).await?;
        Ok(Self {
            child,
            ws_url,
            _profile: profile,
        })
    }

    /// Kill the browser explicitly (drop also does this via `kill_on_drop`).
    pub(crate) async fn close(mut self) {
        let _ = self.child.kill().await;
    }
}

/// Poll the `DevToolsActivePort` file until it holds the port and browser path,
/// then compose the browser-endpoint WebSocket URL. The file's first line is the
/// port; its second line is the `/devtools/browser/<id>` path.
async fn wait_for_devtools(
    port_file: &std::path::Path,
    timeout: Duration,
) -> Result<String, RenderError> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(mut file) = tokio::fs::File::open(port_file).await {
            let mut contents = String::new();
            if file.read_to_string(&mut contents).await.is_ok() {
                let mut lines = contents.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                    let port = port.trim();
                    let path = path.trim();
                    if !port.is_empty() && path.starts_with('/') {
                        return Ok(format!("ws://127.0.0.1:{port}{path}"));
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(RenderError::DevToolsTimeout);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Well-known Chromium/Chrome/Edge install paths for the current platform, most
/// preferred first (Chrome/Chromium before Edge). `which`-style PATH lookup
/// covers Linux distro installs.
fn candidate_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let mut out = Vec::new();
        let program_files = [
            std::env::var_os("ProgramFiles"),
            std::env::var_os("ProgramFiles(x86)"),
            std::env::var_os("LocalAppData"),
        ];
        let suffixes = [
            r"Google\Chrome\Application\chrome.exe",
            r"Chromium\Application\chrome.exe",
            r"Microsoft\Edge\Application\msedge.exe",
        ];
        for base in program_files.into_iter().flatten() {
            for suffix in suffixes {
                out.push(PathBuf::from(&base).join(suffix));
            }
        }
        out
    }
    #[cfg(target_os = "macos")]
    {
        vec![
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
            PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
        ]
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let mut out = vec![
            PathBuf::from("/usr/bin/google-chrome"),
            PathBuf::from("/usr/bin/google-chrome-stable"),
            PathBuf::from("/usr/bin/chromium"),
            PathBuf::from("/usr/bin/chromium-browser"),
            PathBuf::from("/usr/bin/microsoft-edge"),
            PathBuf::from("/snap/bin/chromium"),
        ];
        // PATH lookup for distro-specific locations.
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                for name in [
                    "google-chrome",
                    "chromium",
                    "chromium-browser",
                    "microsoft-edge",
                ] {
                    out.push(dir.join(name));
                }
            }
        }
        out
    }
}
