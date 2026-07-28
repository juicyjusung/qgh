//! Detects whether this OS user already owns the fixed qgh schedule identity.
//!
//! ADR-0018 makes the user manager label one fixed identity per OS user, so no
//! `HOME` or XDG override can scope it. A developer who ran `qgh schedule start`
//! is therefore visible to any test that asserts absent-schedule behavior, and
//! `stop` must then fail closed with `schedule.ownership_ambiguous` instead of
//! reporting `unchanged`. Tests probe the same identity the adapter probes so
//! they assert whichever documented branch applies rather than assuming an
//! empty user manager.
//!
//! Keep the probed label and unit names in sync with `src/schedule_lifecycle.rs`
//! (`MACOS_LABEL`, `SYSTEMD_TIMER`). They are a documented contract in
//! `docs/scheduling.md`, and the module is private so tests cannot import them.

use std::path::Path;
use std::process::Command;

const MACOS_LABEL: &str = "com.juicyjusung.qgh.schedule";
const SYSTEMD_TIMER: &str = "qgh-schedule.timer";

pub fn fixed_schedule_identity_is_active() -> bool {
    if cfg!(target_os = "macos") {
        let Some(uid) = macos_uid() else {
            return false;
        };
        return succeeds(
            "/bin/launchctl",
            &["print".to_string(), format!("gui/{uid}/{MACOS_LABEL}")],
        );
    }
    let systemctl = ["/usr/bin/systemctl", "/bin/systemctl"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .unwrap_or("/usr/bin/systemctl");
    ["is-enabled", "is-active"].into_iter().all(|check| {
        succeeds(
            systemctl,
            &[
                "--user".to_string(),
                check.to_string(),
                "--quiet".to_string(),
                SYSTEMD_TIMER.to_string(),
            ],
        )
    })
}

fn macos_uid() -> Option<String> {
    let output = Command::new("/usr/bin/id").arg("-u").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() || !uid.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    Some(uid)
}

fn succeeds(program: &str, arguments: &[String]) -> bool {
    Command::new(program)
        .args(arguments)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}
