use super::*;

pub(super) const GITHUB_LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/dimitribellini/manifold/releases/latest";
pub(super) const GITHUB_RELEASES_FALLBACK_URL: &str =
    "https://github.com/dimitribellini/manifold/releases/latest";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReleaseUpdateInfo {
    pub(super) tag_name: String,
    pub(super) display_version: String,
    pub(super) html_url: String,
    pub(super) notes: String,
    pub(super) upgrade_command: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum UpdateCheckResult {
    UpdateAvailable(ReleaseUpdateInfo),
    UpToDate { current: String, latest: String },
    Failed(String),
}

#[derive(Clone, Debug)]
pub(super) struct UpdateCheckEvent {
    pub(super) manual: bool,
    pub(super) result: UpdateCheckResult,
}

pub(super) fn parse_semver_tuple(raw: &str) -> Option<(u64, u64, u64, u64)> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V', '.']);
    if trimmed.is_empty() {
        return None;
    }
    let core = trimmed.split('+').next().unwrap_or(trimmed);
    if core.contains('-') {
        return None;
    }
    let mut parts = [0u64; 4];
    let mut count = 0usize;
    for seg in core.split('.') {
        if count >= 4 || seg.is_empty() {
            return None;
        }
        parts[count] = seg.parse::<u64>().ok()?;
        count += 1;
    }
    if count == 0 {
        return None;
    }
    Some((parts[0], parts[1], parts[2], parts[3]))
}

pub(super) fn is_newer_version(current: &str, latest: &str) -> bool {
    matches!(
        (parse_semver_tuple(current), parse_semver_tuple(latest)),
        (Some(cur), Some(lat)) if lat > cur
    )
}

pub(super) fn build_upgrade_command_for_os(os: &str) -> String {
    match os {
        "macos" => concat!(
            "rm -rf /tmp/Manifold.app && ",
            "curl -sSL https://github.com/dimitribellini/manifold/releases/latest/download/",
            "manifold-macos-arm64.tar.gz | tar -xz -C /tmp && ",
            "rm -rf /Applications/Manifold.app && mv /tmp/Manifold.app /Applications/ && ",
            "open /Applications/Manifold.app"
        )
        .to_string(),
        "windows" => concat!(
            "Invoke-WebRequest -Uri https://github.com/dimitribellini/manifold/releases/latest/",
            "download/manifold-windows-x86_64.zip -OutFile manifold-windows-x86_64.zip; ",
            "Expand-Archive -Path manifold-windows-x86_64.zip -DestinationPath . -Force"
        )
        .to_string(),
        _ => concat!(
            "curl -sSL -o manifold-linux-x86_64.AppImage ",
            "https://github.com/dimitribellini/manifold/releases/latest/download/",
            "manifold-linux-x86_64.AppImage && chmod +x manifold-linux-x86_64.AppImage && ",
            "./manifold-linux-x86_64.AppImage"
        )
        .to_string(),
    }
}

pub(super) fn build_update_hint_for_os(os: &str) -> String {
    match os {
        "macos" => concat!(
            "Quit Manifold, then run in Terminal to replace /Applications/Manifold.app cleanly ",
            "without browser quarantine (or use: brew upgrade --cask manifold)."
        )
        .to_string(),
        "windows" => {
            "Run in PowerShell or open the GitHub Release page to download the .zip package."
                .to_string()
        }
        _ => "Run in your terminal to download and launch the latest Linux x86_64 AppImage."
            .to_string(),
    }
}

fn format_release_notes(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "See the GitHub release page for changelog and checksums.".to_string();
    }
    let mut out = String::new();
    for (idx, ch) in trimmed.chars().enumerate() {
        if idx >= 1400 {
            out.push_str("\n…");
            break;
        }
        out.push(ch);
    }
    out
}

pub(super) fn parse_github_latest_release_json(
    json_str: &str,
    current_version: &str,
    os: &str,
) -> Result<UpdateCheckResult, String> {
    let val: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| format!("Invalid release JSON: {e}"))?;
    let is_draft = val.get("draft").and_then(|b| b.as_bool()).unwrap_or(false);
    let is_pre = val
        .get("prerelease")
        .and_then(|b| b.as_bool())
        .unwrap_or(false);
    let cur_label = format_app_version_label(current_version);
    if is_draft || is_pre {
        return Ok(UpdateCheckResult::UpToDate {
            current: cur_label.clone(),
            latest: cur_label,
        });
    }
    let tag_name = val
        .get("tag_name")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Missing tag_name in GitHub release payload".to_string())?;
    let display_version = format_app_version_label(tag_name);
    if !is_newer_version(current_version, tag_name) {
        return Ok(UpdateCheckResult::UpToDate {
            current: cur_label,
            latest: display_version,
        });
    }
    let raw_url = val
        .get("html_url")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .unwrap_or("");
    let html_url = if raw_url.starts_with("https://github.com/dimitribellini/manifold/") {
        raw_url.to_string()
    } else {
        GITHUB_RELEASES_FALLBACK_URL.to_string()
    };
    let notes = val
        .get("body")
        .and_then(|s| s.as_str())
        .map(format_release_notes)
        .unwrap_or_else(|| format_release_notes(""));
    let upgrade_command = build_upgrade_command_for_os(os);
    Ok(UpdateCheckResult::UpdateAvailable(ReleaseUpdateInfo {
        tag_name: tag_name.to_string(),
        display_version,
        html_url,
        notes,
        upgrade_command,
    }))
}

pub(super) fn parse_release_tag_from_redirect_url(url: &str) -> Option<String> {
    let prefix = "https://github.com/dimitribellini/manifold/releases/tag/";
    let rest = url.trim().strip_prefix(prefix)?;
    let tag = rest.split(['?', '#', '/']).next().unwrap_or("").trim();
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

fn check_latest_release_via_redirect(
    curl_bin: &str,
    cur_ver: &str,
    os: &str,
) -> Option<UpdateCheckResult> {
    let null_sink = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let out = Command::new(curl_bin)
        .args([
            "-fsSL",
            "--connect-timeout",
            "5",
            "--max-time",
            "10",
            "-o",
            null_sink,
            "-w",
            "%{url_effective}",
            GITHUB_RELEASES_FALLBACK_URL,
        ])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let eff_url = String::from_utf8_lossy(&out.stdout);
    let tag = parse_release_tag_from_redirect_url(&eff_url)?;
    let display_version = format_app_version_label(&tag);
    if !is_newer_version(cur_ver, &tag) {
        return Some(UpdateCheckResult::UpToDate {
            current: format_app_version_label(cur_ver),
            latest: display_version,
        });
    }
    Some(UpdateCheckResult::UpdateAvailable(ReleaseUpdateInfo {
        html_url: format!("https://github.com/dimitribellini/manifold/releases/tag/{tag}"),
        tag_name: tag,
        display_version,
        notes: format_release_notes(""),
        upgrade_command: build_upgrade_command_for_os(os),
    }))
}

pub(super) fn spawn_update_check(tx: mpsc::Sender<UpdateCheckEvent>, manual: bool) {
    if !manual && std::env::var_os("MANIFOLD_DISABLE_UPDATE_CHECK").as_deref() == Some("1".as_ref())
    {
        return;
    }
    std::thread::spawn(move || {
        let cur_ver = option_env!("APP_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"));
        let os = std::env::consts::OS;
        let curl_bin = if cfg!(windows) {
            "curl.exe"
        } else if cfg!(target_os = "macos") {
            "/usr/bin/curl"
        } else {
            "curl"
        };
        let output = Command::new(curl_bin)
            .args([
                "-fsSL",
                "--connect-timeout",
                "5",
                "--max-time",
                "10",
                "-H",
                "Accept: application/vnd.github+json",
                "-H",
                "User-Agent: Manifold-File-Manager",
                GITHUB_LATEST_RELEASE_API,
            ])
            .stdin(Stdio::null())
            .output();
        let result = match output {
            Ok(out) if out.status.success() => {
                let body = String::from_utf8_lossy(&out.stdout);
                parse_github_latest_release_json(&body, cur_ver, os).unwrap_or_else(|err| {
                    check_latest_release_via_redirect(curl_bin, cur_ver, os)
                        .unwrap_or(UpdateCheckResult::Failed(err))
                })
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let msg = if err.is_empty() {
                    format!("Update check failed ({})", out.status)
                } else {
                    err
                };
                check_latest_release_via_redirect(curl_bin, cur_ver, os)
                    .unwrap_or(UpdateCheckResult::Failed(msg))
            }
            Err(err) => check_latest_release_via_redirect(curl_bin, cur_ver, os)
                .unwrap_or_else(|| UpdateCheckResult::Failed(format!("curl unavailable: {err}"))),
        };
        let _ = tx.send(UpdateCheckEvent { manual, result });
    });
}

pub(super) fn open_url_in_browser(url: &str) {
    if !url.starts_with("https://") {
        return;
    }
    if cfg!(target_os = "macos") {
        let _ = Command::new("open").arg(url).spawn();
    } else if cfg!(target_os = "windows") {
        let _ = Command::new("explorer").arg(url).spawn();
    } else {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}
