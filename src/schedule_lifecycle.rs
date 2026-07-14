use crate::commands::LocalReadOutcome;
use crate::config::{load_profile, TokenSource};
use crate::error::QghError;
use crate::lease::FileLease;
use crate::paths::{ensure_private_dir, qgh_data_dir, set_private_file};
use crate::time::now_run_id_suffix;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const REGISTRATION_SCHEMA: &str = "qgh.schedule-registration.v1";
const FIXED_INTERVAL: &str = "1h";
const JITTER_WINDOW_SECONDS: u32 = 15 * 60;
const MACOS_LABEL: &str = "com.juicyjusung.qgh.schedule";
const SYSTEMD_SERVICE: &str = "qgh-schedule.service";
const SYSTEMD_TIMER: &str = "qgh-schedule.timer";

pub(crate) fn start(profile_ids: &[String], interval: &str) -> Result<LocalReadOutcome, QghError> {
    validate_start_request(profile_ids, interval)?;
    for profile_id in profile_ids {
        let profile = load_profile(profile_id)?;
        match profile.token_source {
            TokenSource::GithubCli => {}
            TokenSource::Env { .. } | TokenSource::Unsupported => {
                return Err(QghError::new(
                    "schedule.credentials_unsupported",
                    "Scheduled profiles must use the github_cli token source.",
                    2,
                )
                .with_details(json!({
                    "profile_id": profile_id,
                    "supported_token_source": "github_cli"
                }))
                .with_hint(
                    "Update the profile to use GitHub CLI credentials available to the user manager.",
                ));
            }
        }
    }

    let adapter = PlatformAdapter::current()?;
    let _lifecycle_lease = FileLease::acquire_schedule_lifecycle(&lifecycle_lock_path()?)?;
    let executable = invoked_executable()?;
    let environment = background_environment(&executable)?;
    let desired = adapter.prepare(profile_ids, interval, &executable, &environment)?;
    let action = reconcile_start(&adapter, &desired, &SystemManagerRunner)?;
    Ok(outcome(
        "start",
        action,
        "active",
        &adapter,
        Some(&desired.registration),
        true,
        true,
    ))
}

pub(crate) fn status() -> Result<LocalReadOutcome, QghError> {
    let adapter = PlatformAdapter::current()?;
    let registration = read_registration(&adapter.registration_path())?;
    let snapshots = snapshot_artifacts(&adapter.artifact_paths())?;
    let any_artifact = snapshots.iter().any(|snapshot| snapshot.bytes.is_some());
    let all_artifacts = snapshots.iter().all(|snapshot| snapshot.bytes.is_some());
    let artifacts_private = snapshots
        .iter()
        .filter(|snapshot| snapshot.bytes.is_some())
        .all(|snapshot| is_private_file(&snapshot.path));

    let (schedule_state, artifact_state) = match registration.as_ref() {
        None if !any_artifact => ("not_installed", "missing"),
        None => ("drifted", "orphaned"),
        Some(registration) if registration.platform != adapter.kind() => {
            ("drifted", "platform_mismatch")
        }
        Some(registration) => {
            let hash_matches = all_artifacts
                && artifact_bundle_hash_from_snapshots(&snapshots)
                    .is_some_and(|hash| hash == registration.artifact_hash);
            let state_private = is_private_file(&adapter.registration_path());
            if hash_matches && artifacts_private && state_private {
                ("active", "ready")
            } else if !all_artifacts {
                ("drifted", "missing")
            } else {
                ("drifted", "changed")
            }
        }
    };

    Ok(outcome(
        "status",
        "inspected",
        schedule_state,
        &adapter,
        registration.as_ref(),
        false,
        schedule_state == "active",
    )
    .with_artifact_state(artifact_state))
}

pub(crate) fn stop() -> Result<LocalReadOutcome, QghError> {
    let adapter = PlatformAdapter::current()?;
    let _lifecycle_lease = FileLease::acquire_schedule_lifecycle(&lifecycle_lock_path()?)?;
    let registration = read_registration(&adapter.registration_path())?;
    if registration
        .as_ref()
        .is_some_and(|registration| registration.platform != adapter.kind())
    {
        return Err(QghError::new(
            "schedule.platform_mismatch",
            "The local schedule was registered by another platform adapter.",
            2,
        )
        .with_details(json!({
            "registered_platform": registration.as_ref().map(|value| value.platform.as_str()),
            "current_platform": adapter.kind().as_str()
        }))
        .with_hint("Stop the schedule on the platform that registered it."));
    }
    let prior_registration = registration.clone();
    let action = reconcile_stop(&adapter, &SystemManagerRunner)?;
    Ok(outcome(
        "stop",
        action,
        "not_installed",
        &adapter,
        prior_registration.as_ref(),
        action == "removed",
        false,
    ))
}

trait OutcomeExt {
    fn with_artifact_state(self, artifact_state: &str) -> Self;
}

impl OutcomeExt for LocalReadOutcome {
    fn with_artifact_state(mut self, artifact_state: &str) -> Self {
        self.data["artifact_state"] = json!(artifact_state);
        self
    }
}

fn outcome(
    operation: &str,
    action: &str,
    schedule_state: &str,
    adapter: &PlatformAdapter,
    registration: Option<&ScheduleRegistration>,
    manager_checked: bool,
    installed: bool,
) -> LocalReadOutcome {
    let profile_ids = registration
        .map(|registration| registration.profile_ids.clone())
        .unwrap_or_default();
    let interval = registration.map(|registration| registration.interval.as_str());
    let jitter = registration.map(|registration| {
        json!({
            "strategy": registration.jitter_strategy,
            "offset_seconds": registration.jitter_offset_seconds,
            "max_seconds": registration.jitter_max_seconds
        })
    });
    LocalReadOutcome {
        data: json!({
            "operation": operation,
            "action": action,
            "schedule_state": schedule_state,
            "installed": installed,
            "platform": adapter.kind().as_str(),
            "manager_scope": "user",
            "profile_ids": profile_ids,
            "interval": interval,
            "jitter": jitter,
            "manager_checked": manager_checked,
            "network_access": false,
            "foreground_command": "schedule run"
        }),
        warnings: Vec::new(),
    }
}

fn validate_start_request(profile_ids: &[String], interval: &str) -> Result<(), QghError> {
    if interval != FIXED_INTERVAL {
        return Err(QghError::validation(
            "validation.schedule_interval",
            "The v1 scheduler supports only the fixed 1h interval.",
        )
        .with_details(json!({ "interval": interval, "supported": [FIXED_INTERVAL] })));
    }
    if profile_ids.is_empty() {
        return Err(QghError::validation(
            "validation.schedule_profiles",
            "At least one explicit profile id is required.",
        ));
    }
    let mut seen = BTreeSet::new();
    for profile_id in profile_ids {
        if !seen.insert(profile_id) {
            return Err(QghError::validation(
                "validation.duplicate_profile",
                "Schedule profile ids must be unique.",
            )
            .with_details(json!({ "profile_id": profile_id })));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PlatformKind {
    MacosLaunchd,
    LinuxSystemdUser,
}

impl PlatformKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::MacosLaunchd => "macos_launchd",
            Self::LinuxSystemdUser => "linux_systemd_user",
        }
    }
}

#[derive(Debug, Clone)]
enum PlatformAdapter {
    #[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
    Macos {
        registration_path: PathBuf,
        plist_path: PathBuf,
        stdout_path: PathBuf,
        stderr_path: PathBuf,
    },
    #[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
    Linux {
        registration_path: PathBuf,
        service_path: PathBuf,
        timer_path: PathBuf,
    },
}

impl PlatformAdapter {
    fn current() -> Result<Self, QghError> {
        #[cfg(target_os = "macos")]
        {
            let home = home_dir()?;
            let log_dir = crate::paths::qgh_cache_dir()?.join("schedule");
            Ok(Self::Macos {
                registration_path: registration_path()?,
                plist_path: home
                    .join("Library")
                    .join("LaunchAgents")
                    .join(format!("{MACOS_LABEL}.plist")),
                stdout_path: log_dir.join("stdout.log"),
                stderr_path: log_dir.join("stderr.log"),
            })
        }
        #[cfg(target_os = "linux")]
        {
            let config_home = env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or(home_dir()?.join(".config"));
            let unit_dir = config_home.join("systemd").join("user");
            Ok(Self::Linux {
                registration_path: registration_path()?,
                service_path: unit_dir.join(SYSTEMD_SERVICE),
                timer_path: unit_dir.join(SYSTEMD_TIMER),
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Err(QghError::new(
                "schedule.platform_unsupported",
                "User schedule lifecycle is supported only on macOS and Linux.",
                2,
            )
            .with_hint(
                "Run `qgh schedule run <profile>...` manually; qgh does not install a cron fallback.",
            ))
        }
    }

    fn kind(&self) -> PlatformKind {
        match self {
            Self::Macos { .. } => PlatformKind::MacosLaunchd,
            Self::Linux { .. } => PlatformKind::LinuxSystemdUser,
        }
    }

    fn registration_path(&self) -> PathBuf {
        match self {
            Self::Macos {
                registration_path, ..
            }
            | Self::Linux {
                registration_path, ..
            } => registration_path.clone(),
        }
    }

    fn artifact_paths(&self) -> Vec<ArtifactPath> {
        match self {
            Self::Macos { plist_path, .. } => vec![ArtifactPath {
                logical_name: "launchagent",
                path: plist_path.clone(),
            }],
            Self::Linux {
                service_path,
                timer_path,
                ..
            } => vec![
                ArtifactPath {
                    logical_name: "service",
                    path: service_path.clone(),
                },
                ArtifactPath {
                    logical_name: "timer",
                    path: timer_path.clone(),
                },
            ],
        }
    }

    fn prepare(
        &self,
        profile_ids: &[String],
        interval: &str,
        executable: &Path,
        environment: &BTreeMap<String, String>,
    ) -> Result<PreparedSchedule, QghError> {
        let jitter_offset_seconds = deterministic_jitter_seconds(profile_ids);
        let artifacts = match self {
            Self::Macos {
                plist_path,
                stdout_path,
                stderr_path,
                ..
            } => vec![ManagedArtifact {
                logical_name: "launchagent",
                path: plist_path.clone(),
                bytes: render_macos_launch_agent(
                    profile_ids,
                    executable,
                    environment,
                    stdout_path,
                    stderr_path,
                    jitter_offset_seconds,
                )?
                .into_bytes(),
            }],
            Self::Linux {
                service_path,
                timer_path,
                ..
            } => vec![
                ManagedArtifact {
                    logical_name: "service",
                    path: service_path.clone(),
                    bytes: render_systemd_service(profile_ids, executable, environment)?
                        .into_bytes(),
                },
                ManagedArtifact {
                    logical_name: "timer",
                    path: timer_path.clone(),
                    bytes: render_systemd_timer().into_bytes(),
                },
            ],
        };
        let artifact_hash = artifact_bundle_hash(&artifacts);
        let (jitter_strategy, jitter_offset_seconds) = match self {
            Self::Macos { .. } => (
                "deterministic_minute_offset".to_string(),
                Some(jitter_offset_seconds),
            ),
            Self::Linux { .. } => ("systemd_fixed_random_delay".to_string(), None),
        };
        Ok(PreparedSchedule {
            registration: ScheduleRegistration {
                schema_version: REGISTRATION_SCHEMA.to_string(),
                platform: self.kind(),
                profile_ids: profile_ids.to_vec(),
                interval: interval.to_string(),
                jitter_strategy,
                jitter_offset_seconds,
                jitter_max_seconds: JITTER_WINDOW_SECONDS,
                artifact_hash,
            },
            artifacts,
        })
    }

    fn prepare_runtime_files(&self) -> Result<(), QghError> {
        match self {
            Self::Macos {
                stdout_path,
                stderr_path,
                ..
            } => {
                let log_dir = stdout_path
                    .parent()
                    .ok_or_else(|| storage_error("Schedule log path is invalid."))?;
                ensure_private_dir(log_dir)?;
                for path in [stdout_path, stderr_path] {
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .map_err(|_| storage_error("Could not create a private schedule log."))?;
                    set_private_file(path)?;
                }
                Ok(())
            }
            Self::Linux { .. } => Ok(()),
        }
    }

    fn activate(&self, runner: &dyn ManagerRunner) -> Result<(), QghError> {
        match self {
            Self::Macos { plist_path, .. } => {
                let domain = macos_user_domain()?;
                run_required(
                    runner,
                    Path::new("/bin/launchctl"),
                    &["bootstrap".into(), domain, path_arg(plist_path)?],
                    self.kind(),
                    "bootstrap",
                )
            }
            Self::Linux { .. } => {
                let systemctl = systemctl_path();
                run_required(
                    runner,
                    &systemctl,
                    &["--user".into(), "daemon-reload".into()],
                    self.kind(),
                    "daemon_reload",
                )?;
                run_required(
                    runner,
                    &systemctl,
                    &[
                        "--user".into(),
                        "enable".into(),
                        "--now".into(),
                        SYSTEMD_TIMER.into(),
                    ],
                    self.kind(),
                    "enable",
                )
            }
        }
    }

    fn is_active(&self, runner: &dyn ManagerRunner) -> Result<bool, QghError> {
        match self {
            Self::Macos { .. } => {
                let target = format!("{}/{MACOS_LABEL}", macos_user_domain()?);
                run_state_check(
                    runner,
                    Path::new("/bin/launchctl"),
                    &["print".into(), target],
                    self.kind(),
                    "inspect",
                )
            }
            Self::Linux { .. } => {
                let systemctl = systemctl_path();
                let enabled = run_state_check(
                    runner,
                    &systemctl,
                    &[
                        "--user".into(),
                        "is-enabled".into(),
                        "--quiet".into(),
                        SYSTEMD_TIMER.into(),
                    ],
                    self.kind(),
                    "inspect_enabled",
                )?;
                if !enabled {
                    return Ok(false);
                }
                run_state_check(
                    runner,
                    &systemctl,
                    &[
                        "--user".into(),
                        "is-active".into(),
                        "--quiet".into(),
                        SYSTEMD_TIMER.into(),
                    ],
                    self.kind(),
                    "inspect_active",
                )
            }
        }
    }

    fn deactivate(&self, runner: &dyn ManagerRunner) -> Result<(), QghError> {
        match self {
            Self::Macos { .. } => {
                let domain = macos_user_domain()?;
                run_allowing_absent(
                    runner,
                    Path::new("/bin/launchctl"),
                    &["bootout".into(), format!("{domain}/{MACOS_LABEL}")],
                    self.kind(),
                    "bootout",
                )
            }
            Self::Linux { .. } => {
                let systemctl = systemctl_path();
                run_allowing_absent(
                    runner,
                    &systemctl,
                    &[
                        "--user".into(),
                        "disable".into(),
                        "--now".into(),
                        SYSTEMD_TIMER.into(),
                    ],
                    self.kind(),
                    "disable",
                )?;
                run_allowing_absent(
                    runner,
                    &systemctl,
                    &["--user".into(), "stop".into(), SYSTEMD_SERVICE.into()],
                    self.kind(),
                    "stop_service",
                )
            }
        }
    }

    fn reload_after_remove(&self, runner: &dyn ManagerRunner) -> Result<(), QghError> {
        match self {
            Self::Macos { .. } => Ok(()),
            Self::Linux { .. } => run_required(
                runner,
                &systemctl_path(),
                &["--user".into(), "daemon-reload".into()],
                self.kind(),
                "daemon_reload",
            ),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct ScheduleRegistration {
    schema_version: String,
    platform: PlatformKind,
    profile_ids: Vec<String>,
    interval: String,
    jitter_strategy: String,
    jitter_offset_seconds: Option<u32>,
    jitter_max_seconds: u32,
    artifact_hash: String,
}

#[derive(Debug, Clone)]
struct ArtifactPath {
    logical_name: &'static str,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct ManagedArtifact {
    logical_name: &'static str,
    path: PathBuf,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct PreparedSchedule {
    registration: ScheduleRegistration,
    artifacts: Vec<ManagedArtifact>,
}

#[derive(Debug, Clone)]
struct FileSnapshot {
    logical_name: &'static str,
    path: PathBuf,
    bytes: Option<Vec<u8>>,
}

fn reconcile_start(
    adapter: &PlatformAdapter,
    desired: &PreparedSchedule,
    runner: &dyn ManagerRunner,
) -> Result<&'static str, QghError> {
    let registration_path = adapter.registration_path();
    let old_registration_bytes = read_optional_file(&registration_path)?;
    let old_registration = old_registration_bytes
        .as_deref()
        .map(parse_registration)
        .transpose()?;
    if old_registration
        .as_ref()
        .is_some_and(|registration| registration.platform != adapter.kind())
    {
        return Err(QghError::new(
            "schedule.platform_mismatch",
            "The local schedule was registered by another platform adapter.",
            2,
        )
        .with_hint("Stop the schedule on the platform that registered it."));
    }
    let old_artifacts = snapshot_artifacts(&adapter.artifact_paths())?;
    let desired_matches = old_registration.as_ref() == Some(&desired.registration)
        && desired.artifacts.iter().all(|artifact| {
            old_artifacts.iter().any(|snapshot| {
                snapshot.logical_name == artifact.logical_name
                    && snapshot.bytes.as_deref() == Some(artifact.bytes.as_slice())
                    && is_private_file(&snapshot.path)
            })
        })
        && is_private_file(&registration_path);
    if desired_matches {
        if adapter.is_active(runner)? {
            return Ok("unchanged");
        }
        adapter.activate(runner)?;
        return Ok("reloaded");
    }

    let had_existing = old_registration.is_some()
        || old_artifacts
            .iter()
            .any(|snapshot| snapshot.bytes.is_some());
    if had_existing {
        if let Err(error) = adapter.deactivate(runner) {
            let _ = adapter.activate(runner);
            return Err(error);
        }
    }
    let attempt = (|| -> Result<(), QghError> {
        adapter.prepare_runtime_files()?;
        write_artifacts(&desired.artifacts)?;
        adapter.activate(runner)?;
        write_registration(&registration_path, &desired.registration)?;
        Ok(())
    })();
    if let Err(error) = attempt {
        rollback_start(
            adapter,
            runner,
            &old_artifacts,
            old_registration_bytes.as_deref(),
        );
        return Err(error);
    }
    Ok(if had_existing { "updated" } else { "installed" })
}

fn reconcile_stop(
    adapter: &PlatformAdapter,
    runner: &dyn ManagerRunner,
) -> Result<&'static str, QghError> {
    let registration_path = adapter.registration_path();
    let old_registration = read_optional_file(&registration_path)?;
    let old_artifacts = snapshot_artifacts(&adapter.artifact_paths())?;
    let had_existing = old_registration.is_some()
        || old_artifacts
            .iter()
            .any(|snapshot| snapshot.bytes.is_some());
    if !had_existing {
        return Ok("unchanged");
    }

    if let Err(error) = adapter.deactivate(runner) {
        let _ = adapter.activate(runner);
        return Err(error);
    }
    let attempt = (|| -> Result<(), QghError> {
        remove_snapshots(&old_artifacts)?;
        remove_file_if_exists(&registration_path)?;
        adapter.reload_after_remove(runner)?;
        Ok(())
    })();
    if let Err(error) = attempt {
        restore_snapshots(&old_artifacts);
        restore_optional_file(&registration_path, old_registration.as_deref());
        let _ = adapter.activate(runner);
        return Err(error);
    }
    Ok("removed")
}

fn rollback_start(
    adapter: &PlatformAdapter,
    runner: &dyn ManagerRunner,
    old_artifacts: &[FileSnapshot],
    old_registration: Option<&[u8]>,
) {
    let _ = adapter.deactivate(runner);
    restore_snapshots(old_artifacts);
    restore_optional_file(&adapter.registration_path(), old_registration);
    if old_artifacts
        .iter()
        .any(|snapshot| snapshot.bytes.is_some())
    {
        let _ = adapter.activate(runner);
    } else {
        let _ = adapter.reload_after_remove(runner);
    }
}

fn render_macos_launch_agent(
    profile_ids: &[String],
    executable: &Path,
    environment: &BTreeMap<String, String>,
    stdout_path: &Path,
    stderr_path: &Path,
    jitter_offset_seconds: u32,
) -> Result<String, QghError> {
    let executable = utf8_path(executable)?;
    let stdout_path = utf8_path(stdout_path)?;
    let stderr_path = utf8_path(stderr_path)?;
    let mut arguments = vec![
        executable.to_string(),
        "schedule".to_string(),
        "run".to_string(),
        "--json".to_string(),
    ];
    arguments.extend(profile_ids.iter().cloned());
    let arguments = arguments
        .iter()
        .map(|argument| format!("        <string>{}</string>\n", xml_escape(argument)))
        .collect::<String>();
    let environment = environment
        .iter()
        .map(|(key, value)| {
            format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                xml_escape(key),
                xml_escape(value)
            )
        })
        .collect::<String>();
    let minute = jitter_offset_seconds / 60;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
    <key>Label</key>\n\
    <string>{MACOS_LABEL}</string>\n\
    <key>ProgramArguments</key>\n\
    <array>\n{arguments}    </array>\n\
    <key>EnvironmentVariables</key>\n\
    <dict>\n{environment}    </dict>\n\
    <key>StartCalendarInterval</key>\n\
    <dict>\n\
        <key>Minute</key>\n\
        <integer>{minute}</integer>\n\
    </dict>\n\
    <key>RunAtLoad</key>\n\
    <true/>\n\
    <key>ProcessType</key>\n\
    <string>Background</string>\n\
    <key>Umask</key>\n\
    <integer>63</integer>\n\
    <key>StandardOutPath</key>\n\
    <string>{}</string>\n\
    <key>StandardErrorPath</key>\n\
    <string>{}</string>\n\
</dict>\n\
</plist>\n",
        xml_escape(stdout_path),
        xml_escape(stderr_path)
    ))
}

fn render_systemd_service(
    profile_ids: &[String],
    executable: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<String, QghError> {
    let mut arguments = vec![
        utf8_path(executable)?.to_string(),
        "schedule".to_string(),
        "run".to_string(),
        "--json".to_string(),
    ];
    arguments.extend(profile_ids.iter().cloned());
    let command = arguments
        .iter()
        .map(|argument| systemd_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let environment = environment
        .iter()
        .map(|(key, value)| format!("Environment={}\n", systemd_quote(&format!("{key}={value}"))))
        .collect::<String>();
    Ok(format!(
        "[Unit]\n\
Description=qgh bounded foreground schedule coordinator\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=oneshot\n\
UMask=0077\n\
{environment}\
ExecStart={command}\n"
    ))
}

fn render_systemd_timer() -> String {
    format!(
        "[Unit]\n\
Description=Run qgh bounded foreground schedule coordinator hourly\n\
\n\
[Timer]\n\
OnCalendar=hourly\n\
Persistent=true\n\
RandomizedDelaySec=15m\n\
FixedRandomDelay=true\n\
AccuracySec=1m\n\
Unit={SYSTEMD_SERVICE}\n\
\n\
[Install]\n\
WantedBy=timers.target\n"
    )
}

fn background_environment(executable: &Path) -> Result<BTreeMap<String, String>, QghError> {
    let mut environment = BTreeMap::new();
    let home = env_utf8("HOME")?.ok_or_else(|| {
        QghError::new(
            "schedule.credentials_unavailable",
            "HOME is required for scheduled GitHub CLI credentials.",
            2,
        )
    })?;
    environment.insert("HOME".to_string(), home);
    for key in [
        "GH_CONFIG_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ] {
        if let Some(value) = env_utf8(key)? {
            environment.insert(key.to_string(), value);
        }
    }

    let mut path_entries = Vec::<PathBuf>::new();
    if let Some(parent) = executable.parent() {
        path_entries.push(parent.to_path_buf());
    }
    if let Some(gh) = find_program("gh") {
        if let Some(parent) = gh.parent() {
            path_entries.push(parent.to_path_buf());
        }
    } else {
        return Err(QghError::new(
            "schedule.credentials_unavailable",
            "GitHub CLI is required for scheduled github_cli credentials.",
            2,
        )
        .with_hint("Install and authenticate `gh`, then retry `qgh schedule start`."));
    }
    path_entries.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/sbin"),
    ]);
    let mut seen = BTreeSet::new();
    path_entries.retain(|path| seen.insert(path.clone()));
    let joined = env::join_paths(path_entries).map_err(|_| {
        QghError::new(
            "schedule.environment_invalid",
            "The background PATH could not be encoded safely.",
            2,
        )
    })?;
    environment.insert(
        "PATH".to_string(),
        joined.into_string().map_err(|_| {
            QghError::new(
                "schedule.environment_invalid",
                "The background PATH is not valid UTF-8.",
                2,
            )
        })?,
    );
    Ok(environment)
}

fn invoked_executable() -> Result<PathBuf, QghError> {
    let invoked = env::args_os().next().ok_or_else(|| {
        QghError::new(
            "schedule.binary_unavailable",
            "The invoked qgh executable path is unavailable.",
            2,
        )
    })?;
    let invoked_path = PathBuf::from(&invoked);
    let absolute = if invoked_path.is_absolute() {
        invoked_path
    } else if invoked_path.components().count() > 1 {
        env::current_dir()
            .map_err(|_| {
                QghError::new(
                    "schedule.binary_unavailable",
                    "The qgh executable path could not be resolved.",
                    2,
                )
            })?
            .join(invoked_path)
    } else {
        find_program_path(&invoked).ok_or_else(|| {
            QghError::new(
                "schedule.binary_unavailable",
                "The invoked qgh executable could not be found on PATH.",
                2,
            )
        })?
    };
    if !absolute.is_absolute() || !absolute.is_file() {
        return Err(QghError::new(
            "schedule.binary_unavailable",
            "The scheduled qgh executable must be an existing absolute file path.",
            2,
        ));
    }
    Ok(absolute)
}

fn find_program(name: &str) -> Option<PathBuf> {
    find_program_path(&OsString::from(name))
}

fn find_program_path(name: &OsString) -> Option<PathBuf> {
    env::var_os("PATH")
        .into_iter()
        .flat_map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

fn deterministic_jitter_seconds(profile_ids: &[String]) -> u32 {
    let mut sorted = profile_ids.to_vec();
    sorted.sort();
    let mut hasher = Sha256::new();
    for profile_id in sorted {
        hasher.update(profile_id.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let value = u64::from_be_bytes(digest[..8].try_into().expect("sha256 prefix is fixed"));
    ((value % (JITTER_WINDOW_SECONDS / 60) as u64) as u32) * 60
}

fn artifact_bundle_hash(artifacts: &[ManagedArtifact]) -> String {
    let mut hasher = Sha256::new();
    for artifact in artifacts {
        hash_artifact(&mut hasher, artifact.logical_name, &artifact.bytes);
    }
    hex_digest(hasher.finalize())
}

fn artifact_bundle_hash_from_snapshots(snapshots: &[FileSnapshot]) -> Option<String> {
    let mut hasher = Sha256::new();
    for snapshot in snapshots {
        hash_artifact(
            &mut hasher,
            snapshot.logical_name,
            snapshot.bytes.as_deref()?,
        );
    }
    Some(hex_digest(hasher.finalize()))
}

fn hash_artifact(hasher: &mut Sha256, logical_name: &str, bytes: &[u8]) {
    hasher.update(logical_name.as_bytes());
    hasher.update([0]);
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_registration(path: &Path) -> Result<Option<ScheduleRegistration>, QghError> {
    read_optional_file(path)?
        .as_deref()
        .map(parse_registration)
        .transpose()
}

fn parse_registration(bytes: &[u8]) -> Result<ScheduleRegistration, QghError> {
    let registration: ScheduleRegistration = serde_json::from_slice(bytes)
        .map_err(|_| state_error("Schedule registration state is invalid."))?;
    if registration.schema_version != REGISTRATION_SCHEMA
        || registration.interval != FIXED_INTERVAL
        || registration.profile_ids.is_empty()
        || registration.artifact_hash.len() != 64
        || !registration
            .artifact_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !valid_jitter_contract(&registration)
    {
        return Err(state_error(
            "Schedule registration state has an unsupported contract.",
        ));
    }
    let mut seen = BTreeSet::new();
    if registration
        .profile_ids
        .iter()
        .any(|profile_id| !valid_profile_id(profile_id) || !seen.insert(profile_id))
    {
        return Err(state_error(
            "Schedule registration state contains invalid profile ids.",
        ));
    }
    Ok(registration)
}

fn valid_jitter_contract(registration: &ScheduleRegistration) -> bool {
    if registration.jitter_max_seconds != JITTER_WINDOW_SECONDS {
        return false;
    }
    match registration.platform {
        PlatformKind::MacosLaunchd => {
            registration.jitter_strategy == "deterministic_minute_offset"
                && registration
                    .jitter_offset_seconds
                    .is_some_and(|offset| offset < JITTER_WINDOW_SECONDS && offset % 60 == 0)
        }
        PlatformKind::LinuxSystemdUser => {
            registration.jitter_strategy == "systemd_fixed_random_delay"
                && registration.jitter_offset_seconds.is_none()
        }
    }
}

fn valid_profile_id(profile_id: &str) -> bool {
    let mut characters = profile_id.chars();
    characters.next().is_some_and(|first| {
        (first.is_ascii_lowercase() || first.is_ascii_digit())
            && profile_id.len() <= 64
            && characters.all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '.' | '_' | '-')
            })
    })
}

fn write_registration(path: &Path, registration: &ScheduleRegistration) -> Result<(), QghError> {
    let parent = path
        .parent()
        .ok_or_else(|| storage_error("Schedule registration path is invalid."))?;
    ensure_private_dir(parent)?;
    let bytes = serde_json::to_vec(registration)
        .map_err(|_| storage_error("Could not serialize schedule registration state."))?;
    write_atomic_private(path, &bytes)
}

fn write_artifacts(artifacts: &[ManagedArtifact]) -> Result<(), QghError> {
    for artifact in artifacts {
        write_atomic_private(&artifact.path, &artifact.bytes)?;
    }
    Ok(())
}

fn snapshot_artifacts(paths: &[ArtifactPath]) -> Result<Vec<FileSnapshot>, QghError> {
    paths
        .iter()
        .map(|path| {
            Ok(FileSnapshot {
                logical_name: path.logical_name,
                path: path.path.clone(),
                bytes: read_optional_file(&path.path)?,
            })
        })
        .collect()
}

fn read_optional_file(path: &Path) -> Result<Option<Vec<u8>>, QghError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(storage_error("Could not read schedule lifecycle state.")),
    }
}

fn write_atomic_private(path: &Path, bytes: &[u8]) -> Result<(), QghError> {
    let parent = path
        .parent()
        .ok_or_else(|| storage_error("Schedule lifecycle path is invalid."))?;
    fs::create_dir_all(parent)
        .map_err(|_| storage_error("Could not create schedule lifecycle directory."))?;
    let temporary = parent.join(format!(".qgh-schedule-{}.tmp", now_run_id_suffix()));
    let result = (|| -> Result<(), QghError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|_| storage_error("Could not create schedule lifecycle state."))?;
        set_private_file(&temporary)?;
        file.write_all(bytes)
            .map_err(|_| storage_error("Could not write schedule lifecycle state."))?;
        file.sync_all()
            .map_err(|_| storage_error("Could not sync schedule lifecycle state."))?;
        fs::rename(&temporary, path)
            .map_err(|_| storage_error("Could not publish schedule lifecycle state."))?;
        set_private_file(path)?;
        sync_parent(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn remove_snapshots(snapshots: &[FileSnapshot]) -> Result<(), QghError> {
    for snapshot in snapshots {
        remove_file_if_exists(&snapshot.path)?;
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<(), QghError> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_parent(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(storage_error("Could not remove schedule lifecycle state.")),
    }
}

fn restore_snapshots(snapshots: &[FileSnapshot]) {
    for snapshot in snapshots {
        restore_optional_file(&snapshot.path, snapshot.bytes.as_deref());
    }
}

fn restore_optional_file(path: &Path, bytes: Option<&[u8]>) {
    match bytes {
        Some(bytes) => {
            let _ = write_atomic_private(path, bytes);
        }
        None => {
            let _ = remove_file_if_exists(path);
        }
    }
}

fn sync_parent(path: &Path) -> Result<(), QghError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| storage_error("Could not sync schedule lifecycle directory."))
}

#[cfg(unix)]
fn is_private_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o077 == 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_private_file(path: &Path) -> bool {
    path.is_file()
}

trait ManagerRunner {
    fn run(&self, program: &Path, arguments: &[String]) -> std::io::Result<ManagerResult>;
}

struct SystemManagerRunner;

impl ManagerRunner for SystemManagerRunner {
    fn run(&self, program: &Path, arguments: &[String]) -> std::io::Result<ManagerResult> {
        let output = Command::new(program).args(arguments).output()?;
        Ok(ManagerResult {
            success: output.status.success(),
            status_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[derive(Debug)]
struct ManagerResult {
    success: bool,
    status_code: Option<i32>,
    stderr: String,
}

fn run_required(
    runner: &dyn ManagerRunner,
    program: &Path,
    arguments: &[String],
    platform: PlatformKind,
    operation: &str,
) -> Result<(), QghError> {
    let result = runner.run(program, arguments).map_err(|_| {
        manager_unsupported(
            platform,
            operation,
            "The user lifecycle manager is unavailable.",
        )
    })?;
    if result.success {
        return Ok(());
    }
    Err(classify_manager_failure(platform, operation, &result))
}

fn run_allowing_absent(
    runner: &dyn ManagerRunner,
    program: &Path,
    arguments: &[String],
    platform: PlatformKind,
    operation: &str,
) -> Result<(), QghError> {
    let result = runner.run(program, arguments).map_err(|_| {
        manager_unsupported(
            platform,
            operation,
            "The user lifecycle manager is unavailable.",
        )
    })?;
    if result.success || manager_target_absent(platform, &result) {
        return Ok(());
    }
    Err(classify_manager_failure(platform, operation, &result))
}

fn run_state_check(
    runner: &dyn ManagerRunner,
    program: &Path,
    arguments: &[String],
    platform: PlatformKind,
    operation: &str,
) -> Result<bool, QghError> {
    let result = runner.run(program, arguments).map_err(|_| {
        manager_unsupported(
            platform,
            operation,
            "The user lifecycle manager is unavailable.",
        )
    })?;
    if result.success {
        return Ok(true);
    }
    if manager_session_unavailable(&result.stderr) {
        return Err(manager_unsupported(
            platform,
            operation,
            "The user lifecycle manager session is unavailable.",
        ));
    }
    Ok(false)
}

fn manager_target_absent(platform: PlatformKind, result: &ManagerResult) -> bool {
    let stderr = result.stderr.to_ascii_lowercase();
    match platform {
        PlatformKind::MacosLaunchd => {
            matches!(result.status_code, Some(3 | 5))
                || stderr.contains("could not find service")
                || stderr.contains("no such process")
        }
        PlatformKind::LinuxSystemdUser => {
            stderr.contains("does not exist")
                || stderr.contains("not loaded")
                || stderr.contains("not-found")
                || stderr.contains("not found")
        }
    }
}

fn classify_manager_failure(
    platform: PlatformKind,
    operation: &str,
    result: &ManagerResult,
) -> QghError {
    if manager_session_unavailable(&result.stderr) {
        return manager_unsupported(
            platform,
            operation,
            "The user lifecycle manager session is unavailable.",
        );
    }
    QghError::new(
        "schedule.manager_failed",
        "The user lifecycle manager could not apply the qgh schedule.",
        6,
    )
    .with_details(json!({
        "platform": platform.as_str(),
        "operation": operation,
        "status_code": result.status_code
    }))
    .with_hint("Inspect the user manager diagnostics, fix the reported capability, and retry.")
    .with_retryable(true)
}

fn manager_session_unavailable(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("failed to connect to bus")
        || stderr.contains("no medium found")
        || stderr.contains("not bootstrapped")
}

fn manager_unsupported(platform: PlatformKind, operation: &str, message: &str) -> QghError {
    QghError::new("schedule.manager_unsupported", message, 2)
        .with_details(json!({
            "platform": platform.as_str(),
            "operation": operation,
            "fallback_installed": false
        }))
        .with_hint(
            "Use an active user manager session or run `qgh schedule run <profile>...` manually; qgh does not install cron or system services.",
        )
}

fn macos_user_domain() -> Result<String, QghError> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .map_err(|_| {
            manager_unsupported(
                PlatformKind::MacosLaunchd,
                "resolve_user",
                "The user id is unavailable.",
            )
        })?;
    if !output.status.success() {
        return Err(manager_unsupported(
            PlatformKind::MacosLaunchd,
            "resolve_user",
            "The user id is unavailable.",
        ));
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() || !uid.chars().all(|character| character.is_ascii_digit()) {
        return Err(manager_unsupported(
            PlatformKind::MacosLaunchd,
            "resolve_user",
            "The user id is invalid.",
        ));
    }
    Ok(format!("gui/{uid}"))
}

fn systemctl_path() -> PathBuf {
    ["/usr/bin/systemctl", "/bin/systemctl"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("/usr/bin/systemctl"))
}

fn registration_path() -> Result<PathBuf, QghError> {
    Ok(qgh_data_dir()?.join("schedule").join("registration.json"))
}

fn lifecycle_lock_path() -> Result<PathBuf, QghError> {
    Ok(qgh_data_dir()?.join("schedule").join("lifecycle.lock"))
}

fn home_dir() -> Result<PathBuf, QghError> {
    env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        QghError::new(
            "schedule.environment_invalid",
            "HOME is required for user schedule lifecycle operations.",
            2,
        )
    })
}

fn path_arg(path: &Path) -> Result<String, QghError> {
    Ok(utf8_path(path)?.to_string())
}

fn utf8_path(path: &Path) -> Result<&str, QghError> {
    path.to_str().ok_or_else(|| {
        QghError::new(
            "schedule.environment_invalid",
            "Schedule lifecycle paths must be valid UTF-8.",
            2,
        )
    })
}

fn env_utf8(key: &str) -> Result<Option<String>, QghError> {
    env::var_os(key)
        .map(|value| {
            value.into_string().map_err(|_| {
                QghError::new(
                    "schedule.environment_invalid",
                    format!("{key} must be valid UTF-8 for the user schedule."),
                    2,
                )
            })
        })
        .transpose()
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_quote(value: &str) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' => quoted.push_str("$$"),
            '\n' => quoted.push_str("\\x0a"),
            '\r' => quoted.push_str("\\x0d"),
            '\t' => quoted.push_str("\\x09"),
            character if character.is_control() => {
                for byte in character.to_string().as_bytes() {
                    quoted.push_str(&format!("\\x{byte:02x}"));
                }
            }
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn storage_error(message: &str) -> QghError {
    QghError::new("schedule.storage_failed", message, 6)
}

fn state_error(message: &str) -> QghError {
    QghError::new("schedule.state_invalid", message, 6)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "qgh-schedule-lifecycle-{name}-{}",
                now_run_id_suffix()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        calls: RefCell<Vec<Vec<String>>>,
        fail_enable_once: Cell<bool>,
        fail_disable_once: Cell<bool>,
        fail_stop_once: Cell<bool>,
        inactive_check_once: Cell<bool>,
    }

    impl ManagerRunner for FakeRunner {
        fn run(&self, _program: &Path, arguments: &[String]) -> std::io::Result<ManagerResult> {
            self.calls.borrow_mut().push(arguments.to_vec());
            let inactive = arguments
                .iter()
                .any(|argument| argument == "is-enabled" || argument == "print")
                && self.inactive_check_once.replace(false);
            let should_fail = arguments.iter().any(|argument| argument == "enable")
                && self.fail_enable_once.replace(false);
            let should_fail_disable = arguments.iter().any(|argument| argument == "disable")
                && self.fail_disable_once.replace(false);
            let should_fail_stop = arguments.iter().any(|argument| argument == "stop")
                && self.fail_stop_once.replace(false);
            Ok(ManagerResult {
                success: !should_fail && !should_fail_disable && !should_fail_stop && !inactive,
                status_code: Some(if should_fail || should_fail_disable || should_fail_stop {
                    1
                } else if inactive {
                    3
                } else {
                    0
                }),
                stderr: String::new(),
            })
        }
    }

    fn linux_adapter(root: &Path) -> PlatformAdapter {
        PlatformAdapter::Linux {
            registration_path: root.join("data/registration.json"),
            service_path: root.join("config/qgh-schedule.service"),
            timer_path: root.join("config/qgh-schedule.timer"),
        }
    }

    fn macos_adapter(root: &Path) -> PlatformAdapter {
        PlatformAdapter::Macos {
            registration_path: root.join("data/registration.json"),
            plist_path: root.join("Library/LaunchAgents/qgh-schedule.plist"),
            stdout_path: root.join("cache/stdout.log"),
            stderr_path: root.join("cache/stderr.log"),
        }
    }

    fn prepared(adapter: &PlatformAdapter, marker: &str) -> PreparedSchedule {
        let artifacts = adapter
            .artifact_paths()
            .into_iter()
            .map(|path| ManagedArtifact {
                logical_name: path.logical_name,
                path: path.path,
                bytes: format!("{marker}-{}", path.logical_name).into_bytes(),
            })
            .collect::<Vec<_>>();
        PreparedSchedule {
            registration: ScheduleRegistration {
                schema_version: REGISTRATION_SCHEMA.to_string(),
                platform: adapter.kind(),
                profile_ids: vec!["work".to_string()],
                interval: FIXED_INTERVAL.to_string(),
                jitter_strategy: "systemd_fixed_random_delay".to_string(),
                jitter_offset_seconds: None,
                jitter_max_seconds: JITTER_WINDOW_SECONDS,
                artifact_hash: artifact_bundle_hash(&artifacts),
            },
            artifacts,
        }
    }

    #[test]
    fn macos_artifact_uses_direct_arguments_and_calendar_catch_up() {
        let environment = BTreeMap::from([
            ("HOME".to_string(), "/Users/test".to_string()),
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ]);
        let rendered = render_macos_launch_agent(
            &["work".to_string(), "personal".to_string()],
            Path::new("/opt/homebrew/bin/qgh"),
            &environment,
            Path::new("/tmp/qgh-out"),
            Path::new("/tmp/qgh-err"),
            12 * 60,
        )
        .unwrap();
        assert!(rendered.contains("<key>ProgramArguments</key>"));
        assert!(rendered.contains("<string>/opt/homebrew/bin/qgh</string>"));
        assert!(rendered.contains("<string>schedule</string>"));
        assert!(rendered.contains("<string>run</string>"));
        assert!(rendered.contains("<key>StartCalendarInterval</key>"));
        assert!(rendered.contains("<integer>12</integer>"));
        assert!(rendered.contains("<key>RunAtLoad</key>"));
        assert!(rendered.contains("<key>Umask</key>"));
        assert!(rendered.contains("<integer>63</integer>"));
        assert!(!rendered.contains("/bin/sh"));
        assert!(!rendered.contains("GH_TOKEN"));
        assert!(!rendered.contains("GITHUB_TOKEN"));
    }

    #[test]
    fn systemd_artifacts_are_persistent_private_oneshot_contracts() {
        let environment = BTreeMap::from([
            ("HOME".to_string(), "/home/test".to_string()),
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ]);
        let service = render_systemd_service(
            &["work".to_string()],
            Path::new("/home/test/bin/qgh"),
            &environment,
        )
        .unwrap();
        let timer = render_systemd_timer();
        assert!(service.contains("Type=oneshot"));
        assert!(service.contains("UMask=0077"));
        assert!(service
            .contains("ExecStart=\"/home/test/bin/qgh\" \"schedule\" \"run\" \"--json\" \"work\""));
        assert!(!service.contains("/bin/sh"));
        assert!(timer.contains("Persistent=true"));
        assert!(timer.contains("RandomizedDelaySec=15m"));
        assert!(timer.contains("FixedRandomDelay=true"));
    }

    #[test]
    fn systemd_artifact_escapes_dollar_expansion() {
        let environment = BTreeMap::from([
            ("HOME".to_string(), "/home/$USER".to_string()),
            ("PATH".to_string(), "$PATH:/usr/bin".to_string()),
        ]);
        let service = render_systemd_service(
            &["work".to_string()],
            Path::new("/home/$USER/bin/qgh"),
            &environment,
        )
        .unwrap();
        assert!(service.contains("\"/home/$$USER/bin/qgh\""));
        assert!(service.contains("\"HOME=/home/$$USER\""));
        assert!(service.contains("\"PATH=$$PATH:/usr/bin\""));
    }

    #[test]
    fn lifecycle_reconciliation_is_idempotent() {
        let root = TestDirectory::new("idempotent");
        let adapter = linux_adapter(&root.0);
        let desired = prepared(&adapter, "v1");
        let runner = FakeRunner::default();

        assert_eq!(
            reconcile_start(&adapter, &desired, &runner).unwrap(),
            "installed"
        );
        let calls_after_install = runner.calls.borrow().len();
        assert_eq!(
            reconcile_start(&adapter, &desired, &runner).unwrap(),
            "unchanged"
        );
        assert_eq!(runner.calls.borrow().len(), calls_after_install + 2);
        assert_eq!(reconcile_stop(&adapter, &runner).unwrap(), "removed");
        let calls_after_stop = runner.calls.borrow().len();
        assert_eq!(reconcile_stop(&adapter, &runner).unwrap(), "unchanged");
        assert_eq!(runner.calls.borrow().len(), calls_after_stop);
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 7);
        assert_eq!(calls[0], ["--user", "daemon-reload"]);
        assert_eq!(calls[1], ["--user", "enable", "--now", SYSTEMD_TIMER]);
        assert_eq!(calls[2], ["--user", "is-enabled", "--quiet", SYSTEMD_TIMER]);
        assert_eq!(calls[3], ["--user", "is-active", "--quiet", SYSTEMD_TIMER]);
        assert_eq!(calls[4], ["--user", "disable", "--now", SYSTEMD_TIMER]);
        assert_eq!(calls[5], ["--user", "stop", SYSTEMD_SERVICE]);
        assert_eq!(calls[6], ["--user", "daemon-reload"]);
    }

    #[test]
    fn unchanged_artifacts_reload_an_externally_disabled_user_timer() {
        let root = TestDirectory::new("manager-reload");
        let adapter = linux_adapter(&root.0);
        let desired = prepared(&adapter, "v1");
        let runner = FakeRunner::default();
        reconcile_start(&adapter, &desired, &runner).unwrap();
        runner.inactive_check_once.set(true);

        assert_eq!(
            reconcile_start(&adapter, &desired, &runner).unwrap(),
            "reloaded"
        );
        let calls = runner.calls.borrow();
        assert_eq!(
            calls[calls.len() - 3],
            ["--user", "is-enabled", "--quiet", SYSTEMD_TIMER]
        );
        assert_eq!(calls[calls.len() - 2], ["--user", "daemon-reload"]);
        assert_eq!(
            calls[calls.len() - 1],
            ["--user", "enable", "--now", SYSTEMD_TIMER]
        );
    }

    #[test]
    fn lifecycle_lease_rejects_overlap_and_recovers_after_release() {
        let root = TestDirectory::new("lifecycle-lease");
        let path = root.0.join("data/lifecycle.lock");
        let owner = FileLease::acquire_schedule_lifecycle(&path).unwrap();

        let busy = FileLease::acquire_schedule_lifecycle(&path).unwrap_err();
        assert_eq!(busy.code, "schedule.busy");
        assert!(busy.retryable);
        assert_eq!(busy.exit_code, 5);

        drop(owner);
        FileLease::acquire_schedule_lifecycle(&path).unwrap();
        assert!(path.exists());
        assert!(is_private_file(&path));
    }

    #[test]
    fn macos_lifecycle_installs_reloads_and_uninstalls_one_launch_agent() {
        let root = TestDirectory::new("macos-lifecycle");
        let adapter = macos_adapter(&root.0);
        let environment = BTreeMap::from([
            ("HOME".to_string(), root.0.display().to_string()),
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ]);
        let first = adapter
            .prepare(
                &["work".to_string()],
                FIXED_INTERVAL,
                Path::new("/usr/local/bin/qgh"),
                &environment,
            )
            .unwrap();
        let updated = adapter
            .prepare(
                &["work".to_string(), "personal".to_string()],
                FIXED_INTERVAL,
                Path::new("/usr/local/bin/qgh"),
                &environment,
            )
            .unwrap();
        let runner = FakeRunner::default();

        assert_eq!(
            reconcile_start(&adapter, &first, &runner).unwrap(),
            "installed"
        );
        assert_eq!(
            reconcile_start(&adapter, &updated, &runner).unwrap(),
            "updated"
        );
        assert_eq!(reconcile_stop(&adapter, &runner).unwrap(), "removed");

        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0][0], "bootstrap");
        assert_eq!(calls[1][0], "bootout");
        assert_eq!(calls[2][0], "bootstrap");
        assert_eq!(calls[3][0], "bootout");
        assert!(calls[0]
            .last()
            .is_some_and(|path| path.ends_with("qgh-schedule.plist")));
        assert!(calls[2]
            .last()
            .is_some_and(|path| path.ends_with("qgh-schedule.plist")));
        assert!(calls[1][1].ends_with(MACOS_LABEL));
        assert!(calls[3][1].ends_with(MACOS_LABEL));
        assert!(!adapter.registration_path().exists());
        assert!(adapter
            .artifact_paths()
            .iter()
            .all(|artifact| !artifact.path.exists()));
    }

    #[test]
    fn failed_manager_update_rolls_back_artifacts_and_registration() {
        let root = TestDirectory::new("rollback");
        let adapter = linux_adapter(&root.0);
        let first = prepared(&adapter, "v1");
        let second = prepared(&adapter, "v2");
        let runner = FakeRunner::default();
        reconcile_start(&adapter, &first, &runner).unwrap();
        let before_registration = fs::read(adapter.registration_path()).unwrap();
        let before_artifacts = snapshot_artifacts(&adapter.artifact_paths()).unwrap();

        runner.fail_enable_once.set(true);
        let error = reconcile_start(&adapter, &second, &runner).unwrap_err();
        assert_eq!(error.code, "schedule.manager_failed");
        assert_eq!(
            fs::read(adapter.registration_path()).unwrap(),
            before_registration
        );
        let after_artifacts = snapshot_artifacts(&adapter.artifact_paths()).unwrap();
        assert_eq!(
            after_artifacts
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>(),
            before_artifacts
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn failed_linux_service_stop_reenables_timer_and_preserves_local_state() {
        let root = TestDirectory::new("stop-service-rollback");
        let adapter = linux_adapter(&root.0);
        let desired = prepared(&adapter, "v1");
        let runner = FakeRunner::default();
        reconcile_start(&adapter, &desired, &runner).unwrap();
        let registration = fs::read(adapter.registration_path()).unwrap();
        let artifacts = snapshot_artifacts(&adapter.artifact_paths()).unwrap();
        runner.fail_stop_once.set(true);

        let error = reconcile_stop(&adapter, &runner).unwrap_err();
        assert_eq!(error.code, "schedule.manager_failed");
        assert_eq!(fs::read(adapter.registration_path()).unwrap(), registration);
        assert_eq!(
            snapshot_artifacts(&adapter.artifact_paths())
                .unwrap()
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>(),
            artifacts
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>()
        );
        let calls = runner.calls.borrow();
        assert_eq!(
            calls[calls.len() - 4],
            ["--user", "disable", "--now", SYSTEMD_TIMER]
        );
        assert_eq!(calls[calls.len() - 3], ["--user", "stop", SYSTEMD_SERVICE]);
        assert_eq!(calls[calls.len() - 2], ["--user", "daemon-reload"]);
        assert_eq!(
            calls[calls.len() - 1],
            ["--user", "enable", "--now", SYSTEMD_TIMER]
        );
    }

    #[test]
    fn failed_linux_timer_disable_reenables_timer_and_preserves_local_state() {
        let root = TestDirectory::new("disable-timer-rollback");
        let adapter = linux_adapter(&root.0);
        let desired = prepared(&adapter, "v1");
        let runner = FakeRunner::default();
        reconcile_start(&adapter, &desired, &runner).unwrap();
        let registration = fs::read(adapter.registration_path()).unwrap();
        let artifacts = snapshot_artifacts(&adapter.artifact_paths()).unwrap();
        runner.fail_disable_once.set(true);

        let error = reconcile_stop(&adapter, &runner).unwrap_err();
        assert_eq!(error.code, "schedule.manager_failed");
        assert_eq!(fs::read(adapter.registration_path()).unwrap(), registration);
        assert_eq!(
            snapshot_artifacts(&adapter.artifact_paths())
                .unwrap()
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>(),
            artifacts
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>()
        );
        let calls = runner.calls.borrow();
        assert_eq!(
            calls[calls.len() - 3],
            ["--user", "disable", "--now", SYSTEMD_TIMER]
        );
        assert_eq!(calls[calls.len() - 2], ["--user", "daemon-reload"]);
        assert_eq!(
            calls[calls.len() - 1],
            ["--user", "enable", "--now", SYSTEMD_TIMER]
        );
    }

    #[test]
    fn failed_first_linux_install_removes_units_and_reloads_manager_cache() {
        let root = TestDirectory::new("first-install-rollback");
        let adapter = linux_adapter(&root.0);
        let desired = prepared(&adapter, "v1");
        let runner = FakeRunner::default();
        runner.fail_enable_once.set(true);

        let error = reconcile_start(&adapter, &desired, &runner).unwrap_err();
        assert_eq!(error.code, "schedule.manager_failed");
        assert!(!adapter.registration_path().exists());
        assert!(adapter
            .artifact_paths()
            .iter()
            .all(|artifact| !artifact.path.exists()));
        let calls = runner.calls.borrow();
        assert_eq!(calls.last().unwrap(), &["--user", "daemon-reload"]);
    }

    #[test]
    fn registration_parser_rejects_unknown_fields() {
        let bytes = br#"{
            "schema_version":"qgh.schedule-registration.v1",
            "platform":"linux_systemd_user",
            "profile_ids":["work"],
            "interval":"1h",
            "jitter_strategy":"systemd_fixed_random_delay",
            "jitter_offset_seconds":null,
            "jitter_max_seconds":900,
            "artifact_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "token":"must-not-be-accepted"
        }"#;
        let error = parse_registration(bytes).unwrap_err();
        assert_eq!(error.code, "schedule.state_invalid");
    }
}
