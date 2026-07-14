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
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const REGISTRATION_SCHEMA: &str = "qgh.schedule-registration.v2";
const LEGACY_REGISTRATION_SCHEMA: &str = "qgh.schedule-registration.v1";
const OWNER_STATE_DIR: &str = ".local/state/qgh/schedule-owner";
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
    let executable = invoked_executable()?;
    let environment = background_environment(&executable)?;
    let identity = current_owner_identity(adapter.kind(), environment.clone())?;
    let owner_paths = LifecycleOwnerPaths::for_identity(&identity);
    let _lifecycle_lease = FileLease::acquire_schedule_lifecycle(&owner_paths.lock)?;
    let existing = resolve_existing_schedule(&owner_paths, &adapter, &identity)?;
    let desired = adapter.prepare(&identity, profile_ids, interval, &executable, &environment)?;
    let action = reconcile_start_owned(
        &owner_paths,
        &adapter,
        &desired,
        existing.as_ref(),
        &SystemManagerRunner,
    )?;
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
    let current_adapter = PlatformAdapter::current()?;
    let identity = current_owner_identity(current_adapter.kind(), current_environment_capture()?)?;
    let owner_paths = LifecycleOwnerPaths::for_identity(&identity);
    let existing = resolve_existing_schedule(&owner_paths, &current_adapter, &identity)?;
    let adapter = existing
        .as_ref()
        .map(|existing| &existing.adapter)
        .unwrap_or(&current_adapter);
    let registration = existing.as_ref().map(|existing| &existing.registration);
    let state_path = existing
        .as_ref()
        .and_then(|existing| existing.legacy_path.as_ref())
        .unwrap_or(&owner_paths.registration);
    let (schedule_state, artifact_state) =
        inspect_schedule_state(adapter, registration, state_path)?;

    Ok(outcome(
        "status",
        "inspected",
        schedule_state,
        adapter,
        registration,
        false,
        schedule_state == "active",
    )
    .with_artifact_state(artifact_state))
}

fn inspect_schedule_state(
    adapter: &PlatformAdapter,
    registration: Option<&ScheduleRegistration>,
    state_path: &Path,
) -> Result<(&'static str, &'static str), QghError> {
    let snapshots = snapshot_artifacts(&adapter.artifact_paths())?;
    let any_artifact = snapshots.iter().any(|snapshot| snapshot.bytes.is_some());
    let all_artifacts = snapshots.iter().all(|snapshot| snapshot.bytes.is_some());
    let artifacts_private = snapshots
        .iter()
        .filter(|snapshot| snapshot.bytes.is_some())
        .all(|snapshot| is_private_file(&snapshot.path));
    let runtime_present = adapter.runtime_files_present();
    let runtime_ready = adapter.runtime_files_ready();
    Ok(match registration {
        None if !any_artifact => ("not_installed", "missing"),
        None => ("drifted", "orphaned"),
        Some(registration) if registration.platform != adapter.kind() => {
            ("drifted", "platform_mismatch")
        }
        Some(registration) => {
            let hash_matches = all_artifacts
                && artifact_bundle_hash_from_snapshots(&snapshots)
                    .is_some_and(|hash| hash == registration.artifact_hash);
            if hash_matches && artifacts_private && is_private_file(state_path) && runtime_ready {
                ("active", "ready")
            } else if !all_artifacts || !runtime_present {
                ("drifted", "missing")
            } else {
                ("drifted", "changed")
            }
        }
    })
}

pub(crate) fn stop() -> Result<LocalReadOutcome, QghError> {
    let current_adapter = PlatformAdapter::current()?;
    let identity = current_owner_identity(current_adapter.kind(), current_environment_capture()?)?;
    let owner_paths = LifecycleOwnerPaths::for_identity(&identity);
    let _lifecycle_lease = FileLease::acquire_schedule_lifecycle(&owner_paths.lock)?;
    let existing = resolve_existing_schedule(&owner_paths, &current_adapter, &identity)?;
    let adapter = existing
        .as_ref()
        .map(|existing| &existing.adapter)
        .unwrap_or(&current_adapter);
    let prior_registration = existing
        .as_ref()
        .map(|existing| existing.registration.clone());
    let action = reconcile_stop_owned(
        &owner_paths,
        adapter,
        existing.as_ref(),
        &SystemManagerRunner,
    )?;
    Ok(outcome(
        "stop",
        action,
        "not_installed",
        adapter,
        prior_registration.as_ref(),
        true,
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

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct ScheduleOwnerIdentity {
    uid: String,
    home: PathBuf,
    manager_identity: String,
    captured_environment: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
struct LifecycleOwnerPaths {
    registration: PathBuf,
    lock: PathBuf,
}

impl LifecycleOwnerPaths {
    fn for_identity(identity: &ScheduleOwnerIdentity) -> Self {
        // This directory intentionally ignores every XDG override. The launchd label and
        // systemd unit name are one per OS user, so their ownership lock and record must have
        // exactly the same scope.
        let root = identity
            .home
            .join(OWNER_STATE_DIR)
            .join(format!("uid-{}", identity.uid));
        Self {
            registration: root.join("registration.json"),
            lock: root.join("lifecycle.lock"),
        }
    }
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
        plist_path: PathBuf,
        stdout_path: PathBuf,
        stderr_path: PathBuf,
    },
    #[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
    Linux {
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

    fn default_for_home(home: &Path, kind: PlatformKind) -> Self {
        match kind {
            PlatformKind::MacosLaunchd => Self::Macos {
                plist_path: home
                    .join("Library/LaunchAgents")
                    .join(format!("{MACOS_LABEL}.plist")),
                stdout_path: home.join(".cache/qgh/schedule/stdout.log"),
                stderr_path: home.join(".cache/qgh/schedule/stderr.log"),
            },
            PlatformKind::LinuxSystemdUser => Self::Linux {
                service_path: home.join(".config/systemd/user").join(SYSTEMD_SERVICE),
                timer_path: home.join(".config/systemd/user").join(SYSTEMD_TIMER),
            },
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

    fn runtime_paths(&self) -> Vec<PathBuf> {
        match self {
            Self::Macos {
                stdout_path,
                stderr_path,
                ..
            } => vec![stdout_path.clone(), stderr_path.clone()],
            Self::Linux { .. } => Vec::new(),
        }
    }

    fn managed_file_paths(&self) -> Vec<PathBuf> {
        self.artifact_paths()
            .into_iter()
            .map(|artifact| artifact.path)
            .chain(self.runtime_paths())
            .collect()
    }

    fn runtime_files_present(&self) -> bool {
        self.runtime_paths()
            .iter()
            .all(|path| is_regular_file_entry(path))
    }

    fn runtime_files_ready(&self) -> bool {
        self.runtime_paths()
            .iter()
            .all(|path| is_private_file(path))
    }

    #[cfg(test)]
    fn registration_path(&self) -> PathBuf {
        test_owner_paths(self).registration
    }

    fn capture(&self) -> CapturedManager {
        match self {
            Self::Macos {
                plist_path,
                stdout_path,
                stderr_path,
            } => CapturedManager::Macos {
                plist_path: plist_path.clone(),
                stdout_path: stdout_path.clone(),
                stderr_path: stderr_path.clone(),
            },
            Self::Linux {
                service_path,
                timer_path,
            } => CapturedManager::Linux {
                service_path: service_path.clone(),
                timer_path: timer_path.clone(),
            },
        }
    }

    fn from_registration(
        registration: &ScheduleRegistration,
        current_identity: &ScheduleOwnerIdentity,
    ) -> Result<Self, QghError> {
        if registration.owner.uid != current_identity.uid
            || registration.owner.home != current_identity.home
            || registration.owner.manager_identity != manager_identity(registration.platform)
        {
            return Err(ownership_ambiguous(
                "The schedule owner record does not match the current OS user identity.",
            ));
        }
        let adapter = match &registration.manager {
            CapturedManager::Macos {
                plist_path,
                stdout_path,
                stderr_path,
            } => Self::Macos {
                plist_path: plist_path.clone(),
                stdout_path: stdout_path.clone(),
                stderr_path: stderr_path.clone(),
            },
            CapturedManager::Linux {
                service_path,
                timer_path,
            } => Self::Linux {
                service_path: service_path.clone(),
                timer_path: timer_path.clone(),
            },
        };
        if adapter.kind() != registration.platform
            || !adapter.has_safe_managed_paths(&registration.owner)?
        {
            return Err(ownership_ambiguous(
                "The schedule owner record contains unsafe or mismatched managed paths.",
            ));
        }
        Ok(adapter)
    }

    fn has_safe_managed_paths(&self, owner: &ScheduleOwnerIdentity) -> Result<bool, QghError> {
        let mut paths = self.artifact_paths();
        match self {
            Self::Macos {
                stdout_path,
                stderr_path,
                ..
            } => {
                paths.push(ArtifactPath {
                    logical_name: "stdout",
                    path: stdout_path.clone(),
                });
                paths.push(ArtifactPath {
                    logical_name: "stderr",
                    path: stderr_path.clone(),
                });
            }
            Self::Linux { .. } => {}
        }
        if paths.iter().any(|path| {
            !path.path.is_absolute()
                || path
                    .path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
        }) {
            return Ok(false);
        }
        Ok(match self {
            Self::Macos {
                plist_path,
                stdout_path,
                stderr_path,
            } => {
                let cache_home = captured_path(owner, "XDG_CACHE_HOME")
                    .unwrap_or_else(|| owner.home.join(".cache"));
                let log_dir = cache_home.join("qgh/schedule");
                plist_path
                    == &owner
                        .home
                        .join("Library/LaunchAgents")
                        .join(format!("{MACOS_LABEL}.plist"))
                    && stdout_path == &log_dir.join("stdout.log")
                    && stderr_path == &log_dir.join("stderr.log")
            }
            Self::Linux {
                service_path,
                timer_path,
            } => {
                let config_home = captured_path(owner, "XDG_CONFIG_HOME")
                    .unwrap_or_else(|| owner.home.join(".config"));
                let unit_dir = config_home.join("systemd/user");
                service_path == &unit_dir.join(SYSTEMD_SERVICE)
                    && timer_path == &unit_dir.join(SYSTEMD_TIMER)
            }
        })
    }

    fn prepare(
        &self,
        owner: &ScheduleOwnerIdentity,
        profile_ids: &[String],
        interval: &str,
        executable: &Path,
        environment: &BTreeMap<String, String>,
    ) -> Result<PreparedSchedule, QghError> {
        if !self.has_safe_managed_paths(owner)? {
            return Err(QghError::new(
                "schedule.environment_invalid",
                "The selected schedule manager paths do not match the captured user environment.",
                2,
            ));
        }
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
                owner: owner.clone(),
                manager: self.capture(),
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
                    open_private_runtime_file(path)?;
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

fn open_private_runtime_file(path: &Path) -> Result<(), QghError> {
    let file = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(|_| storage_error("Could not open a private schedule log."))?,
        Ok(_) => {
            return Err(ownership_ambiguous(
                "A managed schedule runtime path is not a regular file.",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)
            .map_err(|_| storage_error("Could not create a private schedule log."))?,
        Err(_) => return Err(storage_error("Could not inspect a schedule runtime path.")),
    };
    if !path_refers_to_open_file(path, &file)? {
        return Err(ownership_ambiguous(
            "A managed schedule runtime path changed while it was being opened.",
        ));
    }
    set_private_open_file(&file)
}

#[cfg(unix)]
fn path_refers_to_open_file(path: &Path, file: &File) -> Result<bool, QghError> {
    use std::os::unix::fs::MetadataExt;
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|_| storage_error("Could not verify a schedule runtime path."))?;
    let file_metadata = file
        .metadata()
        .map_err(|_| storage_error("Could not verify an open schedule runtime file."))?;
    Ok(path_metadata.file_type().is_file()
        && path_metadata.dev() == file_metadata.dev()
        && path_metadata.ino() == file_metadata.ino())
}

#[cfg(not(unix))]
fn path_refers_to_open_file(path: &Path, _file: &File) -> Result<bool, QghError> {
    Ok(is_regular_file_entry(path))
}

#[cfg(unix)]
fn set_private_open_file(file: &File) -> Result<(), QghError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = file
        .metadata()
        .map_err(|_| storage_error("Could not inspect an open schedule runtime file."))?
        .permissions();
    permissions.set_mode(0o600);
    file.set_permissions(permissions)
        .map_err(|_| storage_error("Could not make a schedule runtime file private."))
}

#[cfg(not(unix))]
fn set_private_open_file(_file: &File) -> Result<(), QghError> {
    Ok(())
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct ScheduleRegistration {
    schema_version: String,
    platform: PlatformKind,
    owner: ScheduleOwnerIdentity,
    manager: CapturedManager,
    profile_ids: Vec<String>,
    interval: String,
    jitter_strategy: String,
    jitter_offset_seconds: Option<u32>,
    jitter_max_seconds: u32,
    artifact_hash: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum CapturedManager {
    Macos {
        plist_path: PathBuf,
        stdout_path: PathBuf,
        stderr_path: PathBuf,
    },
    Linux {
        service_path: PathBuf,
        timer_path: PathBuf,
    },
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct LegacyScheduleRegistration {
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
struct ExistingSchedule {
    registration: ScheduleRegistration,
    adapter: PlatformAdapter,
    legacy_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

fn reconcile_start_owned(
    owner_paths: &LifecycleOwnerPaths,
    adapter: &PlatformAdapter,
    desired: &PreparedSchedule,
    existing: Option<&ExistingSchedule>,
    runner: &dyn ManagerRunner,
) -> Result<&'static str, QghError> {
    let old_adapter = existing
        .map(|existing| &existing.adapter)
        .unwrap_or(adapter);
    validate_runtime_repair_targets(old_adapter)?;
    let old_owner_bytes = read_optional_file(&owner_paths.registration)?;
    let old_legacy_bytes = existing
        .and_then(|existing| existing.legacy_path.as_ref())
        .map(|path| read_optional_file(path))
        .transpose()?
        .flatten();
    let old_artifacts = snapshot_artifacts(&old_adapter.artifact_paths())?;
    reject_unowned_destination_entries(old_adapter, adapter)?;
    let old_managed_paths = old_adapter.managed_file_paths();
    let new_runtime_cleanup = adapter
        .runtime_paths()
        .into_iter()
        .filter(|path| !old_managed_paths.contains(path))
        .collect::<Vec<_>>();
    let new_artifacts = snapshot_artifacts(&adapter.artifact_paths())?;
    let rollback_artifacts = merge_snapshots(&old_artifacts, &new_artifacts);
    if existing.is_none()
        && (rollback_artifacts
            .iter()
            .any(|snapshot| snapshot.bytes.is_some())
            || adapter.is_active(runner)?)
    {
        return Err(ownership_ambiguous(
            "The fixed user manager identity is active or has artifacts without a provable owner record.",
        ));
    }
    let state_path = existing
        .and_then(|existing| existing.legacy_path.as_ref())
        .unwrap_or(&owner_paths.registration);
    let desired_matches = existing.is_some_and(|existing| {
        existing.legacy_path.is_none() && existing.registration == desired.registration
    }) && desired.artifacts.iter().all(|artifact| {
        old_artifacts.iter().any(|snapshot| {
            snapshot.logical_name == artifact.logical_name
                && snapshot.bytes.as_deref() == Some(artifact.bytes.as_slice())
                && is_private_file(&snapshot.path)
        })
    }) && is_private_file(state_path)
        && old_adapter.runtime_files_ready();
    if desired_matches {
        if old_adapter.is_active(runner)? {
            return Ok("unchanged");
        }
        old_adapter.activate(runner)?;
        return Ok("reloaded");
    }

    let had_existing = existing.is_some()
        || rollback_artifacts
            .iter()
            .any(|snapshot| snapshot.bytes.is_some());
    let old_manager_active = if had_existing {
        old_adapter.is_active(runner)?
    } else {
        false
    };
    if had_existing {
        if let Err(error) = old_adapter.deactivate(runner) {
            if old_manager_active {
                let _ = old_adapter.activate(runner);
            }
            return Err(error);
        }
    }
    let attempt = (|| -> Result<(), QghError> {
        remove_snapshots(&old_artifacts)?;
        adapter.prepare_runtime_files()?;
        write_artifacts(&desired.artifacts)?;
        adapter.activate(runner)?;
        if let Some(legacy_path) = existing.and_then(|existing| existing.legacy_path.as_ref()) {
            remove_file_if_exists(legacy_path)?;
        }
        // The fixed owner record is the commit point. Artifacts and manager activation are
        // complete before readers can discover the new ownership.
        write_registration(&owner_paths.registration, &desired.registration)?;
        Ok(())
    })();
    if let Err(error) = attempt {
        rollback_start(StartRollback {
            owner_paths,
            new_adapter: adapter,
            old_adapter,
            runner,
            artifacts: &rollback_artifacts,
            old_owner: old_owner_bytes.as_deref(),
            legacy_path: existing.and_then(|existing| existing.legacy_path.as_deref()),
            old_legacy: old_legacy_bytes.as_deref(),
            old_manager_active,
            new_runtime_cleanup: &new_runtime_cleanup,
        });
        return Err(error);
    }
    Ok(if had_existing { "updated" } else { "installed" })
}

fn reconcile_stop_owned(
    owner_paths: &LifecycleOwnerPaths,
    adapter: &PlatformAdapter,
    existing: Option<&ExistingSchedule>,
    runner: &dyn ManagerRunner,
) -> Result<&'static str, QghError> {
    let old_registration = read_optional_file(&owner_paths.registration)?;
    let old_legacy_registration = existing
        .and_then(|existing| existing.legacy_path.as_ref())
        .map(|path| read_optional_file(path))
        .transpose()?
        .flatten();
    let old_artifacts = snapshot_artifacts(&adapter.artifact_paths())?;
    if existing.is_none() {
        if old_artifacts
            .iter()
            .any(|snapshot| snapshot.bytes.is_some())
            || adapter.is_active(runner)?
        {
            return Err(ownership_ambiguous(
                "The fixed user manager identity is active or has artifacts without a provable owner record.",
            ));
        }
        return Ok("unchanged");
    }
    let had_existing = existing.is_some()
        || old_registration.is_some()
        || old_artifacts
            .iter()
            .any(|snapshot| snapshot.bytes.is_some());
    if !had_existing {
        return Ok("unchanged");
    }

    let old_manager_active = adapter.is_active(runner)?;
    if let Err(error) = adapter.deactivate(runner) {
        if old_manager_active {
            let _ = adapter.activate(runner);
        }
        return Err(error);
    }
    let attempt = (|| -> Result<(), QghError> {
        remove_snapshots(&old_artifacts)?;
        adapter.reload_after_remove(runner)?;
        if let Some(legacy_path) = existing.and_then(|existing| existing.legacy_path.as_ref()) {
            remove_file_if_exists(legacy_path)?;
        }
        remove_file_if_exists(&owner_paths.registration)?;
        Ok(())
    })();
    if let Err(error) = attempt {
        restore_snapshots(&old_artifacts);
        restore_optional_file(&owner_paths.registration, old_registration.as_deref());
        if let Some(legacy_path) = existing.and_then(|existing| existing.legacy_path.as_ref()) {
            restore_optional_file(legacy_path, old_legacy_registration.as_deref());
        }
        restore_manager_after_rollback(adapter, runner, &old_artifacts, old_manager_active);
        return Err(error);
    }
    Ok("removed")
}

struct StartRollback<'a> {
    owner_paths: &'a LifecycleOwnerPaths,
    new_adapter: &'a PlatformAdapter,
    old_adapter: &'a PlatformAdapter,
    runner: &'a dyn ManagerRunner,
    artifacts: &'a [FileSnapshot],
    old_owner: Option<&'a [u8]>,
    legacy_path: Option<&'a Path>,
    old_legacy: Option<&'a [u8]>,
    old_manager_active: bool,
    new_runtime_cleanup: &'a [PathBuf],
}

fn rollback_start(rollback: StartRollback<'_>) {
    let _ = rollback.new_adapter.deactivate(rollback.runner);
    for path in rollback.new_runtime_cleanup {
        let _ = remove_file_if_exists(path);
    }
    restore_snapshots(rollback.artifacts);
    restore_optional_file(&rollback.owner_paths.registration, rollback.old_owner);
    if let Some(legacy_path) = rollback.legacy_path {
        restore_optional_file(legacy_path, rollback.old_legacy);
    }
    restore_manager_after_rollback(
        rollback.old_adapter,
        rollback.runner,
        rollback.artifacts,
        rollback.old_manager_active,
    );
}

fn merge_snapshots(first: &[FileSnapshot], second: &[FileSnapshot]) -> Vec<FileSnapshot> {
    let mut merged = first.to_vec();
    for snapshot in second {
        if !merged.iter().any(|existing| existing.path == snapshot.path) {
            merged.push(snapshot.clone());
        }
    }
    merged
}

fn reject_unowned_destination_entries(
    old_adapter: &PlatformAdapter,
    new_adapter: &PlatformAdapter,
) -> Result<(), QghError> {
    let old_paths = old_adapter.managed_file_paths();
    for destination in new_adapter.managed_file_paths() {
        if old_paths.contains(&destination) {
            continue;
        }
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                return Err(ownership_ambiguous(
                    "The destination manager location contains a file not owned by the current schedule record.",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(storage_error(
                    "Could not inspect the destination schedule lifecycle state.",
                ));
            }
        }
    }
    Ok(())
}

fn validate_runtime_repair_targets(adapter: &PlatformAdapter) -> Result<(), QghError> {
    for path in adapter.runtime_paths() {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => {
                return Err(ownership_ambiguous(
                    "A managed schedule runtime path is not a regular file.",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(storage_error("Could not inspect a schedule runtime path."));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn reconcile_start(
    adapter: &PlatformAdapter,
    desired: &PreparedSchedule,
    runner: &dyn ManagerRunner,
) -> Result<&'static str, QghError> {
    let owner_paths = test_owner_paths(adapter);
    let existing = test_existing_schedule(&owner_paths, &desired.registration.owner)?;
    reconcile_start_owned(&owner_paths, adapter, desired, existing.as_ref(), runner)
}

#[cfg(test)]
fn reconcile_stop(
    adapter: &PlatformAdapter,
    runner: &dyn ManagerRunner,
) -> Result<&'static str, QghError> {
    let owner_paths = test_owner_paths(adapter);
    let identity = test_identity_for_adapter(adapter);
    let existing = test_existing_schedule(&owner_paths, &identity)?;
    let owned_adapter = existing
        .as_ref()
        .map(|existing| &existing.adapter)
        .unwrap_or(adapter);
    reconcile_stop_owned(&owner_paths, owned_adapter, existing.as_ref(), runner)
}

#[cfg(test)]
fn test_existing_schedule(
    owner_paths: &LifecycleOwnerPaths,
    identity: &ScheduleOwnerIdentity,
) -> Result<Option<ExistingSchedule>, QghError> {
    let Some(bytes) = read_optional_file(&owner_paths.registration)? else {
        return Ok(None);
    };
    let registration = parse_registration(&bytes)?;
    let adapter = PlatformAdapter::from_registration(&registration, identity)?;
    Ok(Some(ExistingSchedule {
        registration,
        adapter,
        legacy_path: None,
    }))
}

#[cfg(test)]
fn test_owner_paths(adapter: &PlatformAdapter) -> LifecycleOwnerPaths {
    let root = match adapter {
        PlatformAdapter::Linux { service_path, .. } => service_path
            .ancestors()
            .nth(4)
            .expect("test Linux adapter root"),
        PlatformAdapter::Macos { plist_path, .. } => plist_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("test macOS adapter root"),
    };
    LifecycleOwnerPaths {
        registration: root.join("data/registration.json"),
        lock: root.join("data/lifecycle.lock"),
    }
}

#[cfg(test)]
fn test_identity_for_adapter(adapter: &PlatformAdapter) -> ScheduleOwnerIdentity {
    let root = test_owner_paths(adapter)
        .registration
        .parent()
        .and_then(Path::parent)
        .expect("test owner root")
        .to_path_buf();
    let captured_environment = match adapter {
        PlatformAdapter::Linux { service_path, .. } => BTreeMap::from([(
            "XDG_CONFIG_HOME".to_string(),
            service_path
                .ancestors()
                .nth(3)
                .expect("test config home")
                .display()
                .to_string(),
        )]),
        PlatformAdapter::Macos { stdout_path, .. } => BTreeMap::from([(
            "XDG_CACHE_HOME".to_string(),
            stdout_path
                .ancestors()
                .nth(3)
                .expect("test cache home")
                .display()
                .to_string(),
        )]),
    };
    ScheduleOwnerIdentity {
        uid: "1000".to_string(),
        home: root,
        manager_identity: manager_identity(adapter.kind()).to_string(),
        captured_environment,
    }
}

fn restore_manager_after_rollback(
    adapter: &PlatformAdapter,
    runner: &dyn ManagerRunner,
    old_artifacts: &[FileSnapshot],
    old_manager_active: bool,
) {
    if old_manager_active
        && old_artifacts
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
        "--manager-invoked".to_string(),
    ];
    arguments.extend(profile_ids.iter().cloned());
    let arguments = arguments
        .iter()
        .map(|argument| {
            Ok(format!(
                "        <string>{}</string>\n",
                xml_escape(argument)?
            ))
        })
        .collect::<Result<String, QghError>>()?;
    let environment = environment
        .iter()
        .map(|(key, value)| {
            Ok(format!(
                "        <key>{}</key>\n        <string>{}</string>\n",
                xml_escape(key)?,
                xml_escape(value)?
            ))
        })
        .collect::<Result<String, QghError>>()?;
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
        xml_escape(stdout_path)?,
        xml_escape(stderr_path)?
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
        "--manager-invoked".to_string(),
    ];
    arguments.extend(profile_ids.iter().cloned());
    let command = arguments
        .iter()
        .map(|argument| systemd_exec_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let environment = environment
        .iter()
        .map(|(key, value)| {
            format!(
                "Environment={}\n",
                systemd_environment_quote(&format!("{key}={value}"))
            )
        })
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
            if key == "GH_CONFIG_DIR" {
                validate_scheduled_gh_config_dir(&value)?;
            }
            environment.insert(key.to_string(), value);
        }
    }

    let mut path_entries = Vec::<PathBuf>::new();
    if let Some(parent) = executable.parent() {
        path_entries.push(parent.to_path_buf());
    }
    if let Some(gh) = find_program("gh").or_else(|| find_program_in_fixed_locations("gh")) {
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

fn validate_scheduled_gh_config_dir(value: &str) -> Result<(), QghError> {
    if safe_absolute_path(Path::new(value)) {
        return Ok(());
    }
    Err(QghError::new(
        "schedule.environment_invalid",
        "GH_CONFIG_DIR must be an absolute normalized path for the user schedule.",
        2,
    ))
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
    if !absolute.is_absolute() || !is_executable_file(&absolute) {
        return Err(QghError::new(
            "schedule.binary_unavailable",
            "The scheduled qgh executable must be an executable absolute file path.",
            2,
        ));
    }
    Ok(absolute)
}

fn find_program(name: &str) -> Option<PathBuf> {
    find_program_path(&OsString::from(name))
}

fn find_program_path(name: &OsString) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|path| find_program_in_path(name, &path))
}

fn find_program_in_path(name: &OsStr, path: &OsStr) -> Option<PathBuf> {
    find_program_in_directories(name, env::split_paths(path))
}

fn find_program_in_fixed_locations(name: &str) -> Option<PathBuf> {
    find_program_in_directories(
        OsStr::new(name),
        [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
        .into_iter()
        .map(PathBuf::from),
    )
}

fn find_program_in_directories(
    name: &OsStr,
    directories: impl Iterator<Item = PathBuf>,
) -> Option<PathBuf> {
    directories
        .filter(|directory| safe_absolute_path(directory))
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
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
        || owner_path_environment(
            registration.platform,
            &registration.owner.captured_environment,
        ) != registration.owner.captured_environment
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

fn parse_legacy_registration(bytes: &[u8]) -> Result<LegacyScheduleRegistration, QghError> {
    let registration: LegacyScheduleRegistration = serde_json::from_slice(bytes)
        .map_err(|_| state_error("Legacy schedule registration state is invalid."))?;
    if registration.schema_version != LEGACY_REGISTRATION_SCHEMA
        || registration.interval != FIXED_INTERVAL
        || registration.profile_ids.is_empty()
        || !valid_artifact_hash(&registration.artifact_hash)
    {
        return Err(state_error(
            "Legacy schedule registration state has an unsupported contract.",
        ));
    }
    let mut seen = BTreeSet::new();
    if registration
        .profile_ids
        .iter()
        .any(|profile_id| !valid_profile_id(profile_id) || !seen.insert(profile_id))
    {
        return Err(state_error(
            "Legacy schedule registration state contains invalid profile ids.",
        ));
    }
    let probe = ScheduleRegistration {
        schema_version: REGISTRATION_SCHEMA.to_string(),
        platform: registration.platform,
        owner: ScheduleOwnerIdentity {
            uid: "0".to_string(),
            home: PathBuf::from("/"),
            manager_identity: manager_identity(registration.platform).to_string(),
            captured_environment: BTreeMap::new(),
        },
        manager: match registration.platform {
            PlatformKind::MacosLaunchd => CapturedManager::Macos {
                plist_path: PathBuf::from(format!("/{MACOS_LABEL}.plist")),
                stdout_path: PathBuf::from("/stdout"),
                stderr_path: PathBuf::from("/stderr"),
            },
            PlatformKind::LinuxSystemdUser => CapturedManager::Linux {
                service_path: PathBuf::from(format!("/{SYSTEMD_SERVICE}")),
                timer_path: PathBuf::from(format!("/{SYSTEMD_TIMER}")),
            },
        },
        profile_ids: registration.profile_ids.clone(),
        interval: registration.interval.clone(),
        jitter_strategy: registration.jitter_strategy.clone(),
        jitter_offset_seconds: registration.jitter_offset_seconds,
        jitter_max_seconds: registration.jitter_max_seconds,
        artifact_hash: registration.artifact_hash.clone(),
    };
    if !valid_jitter_contract(&probe) {
        return Err(state_error(
            "Legacy schedule registration state has an unsupported jitter contract.",
        ));
    }
    Ok(registration)
}

fn valid_artifact_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn resolve_existing_schedule(
    owner_paths: &LifecycleOwnerPaths,
    current_adapter: &PlatformAdapter,
    identity: &ScheduleOwnerIdentity,
) -> Result<Option<ExistingSchedule>, QghError> {
    if let Some(bytes) = read_optional_file(&owner_paths.registration)? {
        if !is_private_file(&owner_paths.registration) {
            return Err(ownership_ambiguous(
                "The fixed schedule owner record is not private.",
            ));
        }
        let version = registration_version(&bytes)?;
        if version != REGISTRATION_SCHEMA {
            return Err(ownership_ambiguous(
                "The fixed schedule owner record uses a legacy or unknown schema.",
            ));
        }
        let registration = parse_registration(&bytes)?;
        if registration.platform != current_adapter.kind() {
            return Err(QghError::new(
                "schedule.platform_mismatch",
                "The local schedule was registered by another platform adapter.",
                2,
            ));
        }
        let adapter = PlatformAdapter::from_registration(&registration, identity)?;
        return Ok(Some(ExistingSchedule {
            registration,
            adapter,
            legacy_path: None,
        }));
    }

    resolve_legacy_schedule(current_adapter, identity)
}

fn registration_version(bytes: &[u8]) -> Result<&str, QghError> {
    #[derive(Deserialize)]
    struct Version<'a> {
        #[serde(borrow)]
        schema_version: &'a str,
    }
    serde_json::from_slice::<Version<'_>>(bytes)
        .map(|version| version.schema_version)
        .map_err(|_| state_error("Schedule registration state is invalid."))
}

fn resolve_legacy_schedule(
    current_adapter: &PlatformAdapter,
    identity: &ScheduleOwnerIdentity,
) -> Result<Option<ExistingSchedule>, QghError> {
    let mut registration_paths = vec![legacy_registration_path_current()?];
    let default_registration = identity
        .home
        .join(".local/share/qgh/schedule/registration.json");
    if !registration_paths.contains(&default_registration) {
        registration_paths.push(default_registration);
    }

    let mut legacy_records = Vec::new();
    for path in registration_paths {
        if let Some(bytes) = read_optional_file(&path)? {
            if !is_private_file(&path) {
                return Err(ownership_ambiguous(
                    "A legacy schedule owner record is not private.",
                ));
            }
            if registration_version(&bytes)? != LEGACY_REGISTRATION_SCHEMA {
                return Err(ownership_ambiguous(
                    "A legacy schedule state location contains an unknown owner record.",
                ));
            }
            legacy_records.push((path, parse_legacy_registration(&bytes)?));
        }
    }
    if legacy_records.is_empty() {
        return Ok(None);
    }

    let default_adapter = PlatformAdapter::default_for_home(&identity.home, current_adapter.kind());
    let mut adapters = vec![current_adapter.clone()];
    if default_adapter.artifact_paths() != current_adapter.artifact_paths() {
        adapters.push(default_adapter);
    }
    prove_legacy_owner(legacy_records, adapters, identity)
}

fn prove_legacy_owner(
    legacy_records: Vec<(PathBuf, LegacyScheduleRegistration)>,
    adapters: Vec<PlatformAdapter>,
    identity: &ScheduleOwnerIdentity,
) -> Result<Option<ExistingSchedule>, QghError> {
    let mut proven = Vec::new();
    for (path, legacy) in &legacy_records {
        for adapter in &adapters {
            if adapter.kind() != legacy.platform || !legacy_artifacts_prove_owner(adapter, legacy)?
            {
                continue;
            }
            proven.push((path.clone(), legacy.clone(), adapter.clone()));
        }
    }
    if proven.len() != 1 {
        return Err(ownership_ambiguous(
            "Legacy schedule ownership is not uniquely provable from the current or default manager artifacts.",
        ));
    }
    let (legacy_path, legacy, adapter) = proven.pop().expect("one proven legacy owner");
    let mut migrated_owner = identity.clone();
    migrated_owner.captured_environment = inferred_environment(&migrated_owner.home, &adapter);
    let registration = ScheduleRegistration {
        schema_version: REGISTRATION_SCHEMA.to_string(),
        platform: legacy.platform,
        owner: migrated_owner,
        manager: adapter.capture(),
        profile_ids: legacy.profile_ids,
        interval: legacy.interval,
        jitter_strategy: legacy.jitter_strategy,
        jitter_offset_seconds: legacy.jitter_offset_seconds,
        jitter_max_seconds: legacy.jitter_max_seconds,
        artifact_hash: legacy.artifact_hash,
    };
    Ok(Some(ExistingSchedule {
        registration,
        adapter,
        legacy_path: Some(legacy_path),
    }))
}

fn legacy_artifacts_prove_owner(
    adapter: &PlatformAdapter,
    registration: &LegacyScheduleRegistration,
) -> Result<bool, QghError> {
    let snapshots = snapshot_artifacts(&adapter.artifact_paths())?;
    Ok(snapshots
        .iter()
        .all(|snapshot| snapshot.bytes.is_some() && is_private_file(&snapshot.path))
        && artifact_bundle_hash_from_snapshots(&snapshots)
            .is_some_and(|hash| hash == registration.artifact_hash))
}

fn inferred_environment(home: &Path, adapter: &PlatformAdapter) -> BTreeMap<String, String> {
    let mut captured = BTreeMap::new();
    match adapter {
        PlatformAdapter::Macos { stdout_path, .. } => {
            if let Some(cache_home) = stdout_path.ancestors().nth(3) {
                if cache_home != home.join(".cache") {
                    captured.insert(
                        "XDG_CACHE_HOME".to_string(),
                        cache_home.display().to_string(),
                    );
                }
            }
        }
        PlatformAdapter::Linux { service_path, .. } => {
            if let Some(config_home) = service_path.ancestors().nth(3) {
                if config_home != home.join(".config") {
                    captured.insert(
                        "XDG_CONFIG_HOME".to_string(),
                        config_home.display().to_string(),
                    );
                }
            }
        }
    }
    captured
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
    fs::symlink_metadata(path)
        .map(|metadata| {
            metadata.file_type().is_file() && metadata.permissions().mode() & 0o077 == 0
        })
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_private_file(path: &Path) -> bool {
    path.is_file()
}

fn is_regular_file_entry(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
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
    if manager_state_inactive(platform, operation, &result) {
        return Ok(false);
    }
    Err(classify_manager_failure(platform, operation, &result))
}

fn manager_state_inactive(platform: PlatformKind, operation: &str, result: &ManagerResult) -> bool {
    if manager_target_absent(platform, result) {
        return true;
    }
    let stderr = result.stderr.trim().to_ascii_lowercase();
    match (platform, operation, result.status_code) {
        (PlatformKind::LinuxSystemdUser, "inspect_enabled", Some(1)) => {
            stderr.is_empty()
                || stderr.contains("disabled")
                || stderr.contains("masked")
                || stderr.contains("static")
        }
        (PlatformKind::LinuxSystemdUser, "inspect_active", Some(3)) => {
            stderr.is_empty() || stderr.contains("inactive") || stderr.contains("dead")
        }
        _ => false,
    }
}

fn manager_target_absent(platform: PlatformKind, result: &ManagerResult) -> bool {
    let stderr = result.stderr.to_ascii_lowercase();
    match platform {
        PlatformKind::MacosLaunchd => {
            result.status_code == Some(3)
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

fn legacy_registration_path_current() -> Result<PathBuf, QghError> {
    Ok(qgh_data_dir()?.join("schedule").join("registration.json"))
}

fn manager_identity(platform: PlatformKind) -> &'static str {
    match platform {
        PlatformKind::MacosLaunchd => MACOS_LABEL,
        PlatformKind::LinuxSystemdUser => SYSTEMD_TIMER,
    }
}

fn captured_path(owner: &ScheduleOwnerIdentity, key: &str) -> Option<PathBuf> {
    owner.captured_environment.get(key).map(PathBuf::from)
}

fn current_owner_identity(
    platform: PlatformKind,
    captured_environment: BTreeMap<String, String>,
) -> Result<ScheduleOwnerIdentity, QghError> {
    let home = home_dir()?;
    validate_owner_environment(&home, &captured_environment)?;
    let captured_environment = owner_path_environment(platform, &captured_environment);
    Ok(ScheduleOwnerIdentity {
        uid: current_user_id()?,
        home,
        manager_identity: manager_identity(platform).to_string(),
        captured_environment,
    })
}

fn owner_path_environment(
    platform: PlatformKind,
    environment: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let key = match platform {
        PlatformKind::MacosLaunchd => "XDG_CACHE_HOME",
        PlatformKind::LinuxSystemdUser => "XDG_CONFIG_HOME",
    };
    environment
        .get(key)
        .map(|value| BTreeMap::from([(key.to_string(), value.clone())]))
        .unwrap_or_default()
}

fn validate_owner_environment(
    home: &Path,
    captured_environment: &BTreeMap<String, String>,
) -> Result<(), QghError> {
    if !safe_absolute_path(home)
        || ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"]
            .into_iter()
            .filter_map(|key| captured_environment.get(key))
            .map(Path::new)
            .any(|path| !safe_absolute_path(path))
    {
        return Err(QghError::new(
            "schedule.environment_invalid",
            "HOME and captured XDG schedule paths must be absolute and normalized.",
            2,
        ));
    }
    Ok(())
}

fn safe_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
}

fn current_environment_capture() -> Result<BTreeMap<String, String>, QghError> {
    let mut captured = BTreeMap::new();
    for key in [
        "HOME",
        "GH_CONFIG_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ] {
        if let Some(value) = env_utf8(key)? {
            captured.insert(key.to_string(), value);
        }
    }
    Ok(captured)
}

fn current_user_id() -> Result<String, QghError> {
    let id = ["/usr/bin/id", "/bin/id"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .unwrap_or_else(|| Path::new("/usr/bin/id"));
    let output = Command::new(id).arg("-u").output().map_err(|_| {
        QghError::new(
            "schedule.environment_invalid",
            "The OS user id is required for schedule ownership.",
            2,
        )
    })?;
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success()
        || uid.is_empty()
        || !uid.chars().all(|character| character.is_ascii_digit())
    {
        return Err(QghError::new(
            "schedule.environment_invalid",
            "The OS user id is invalid for schedule ownership.",
            2,
        ));
    }
    Ok(uid)
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

fn xml_escape(value: &str) -> Result<String, QghError> {
    if value.chars().any(|character| {
        !matches!(
            character,
            '\u{9}' | '\u{A}' | '\u{D}' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}'
        )
    }) {
        return Err(QghError::new(
            "schedule.environment_invalid",
            "Schedule manager values must contain only XML 1.0 characters.",
            2,
        ));
    }

    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

fn systemd_exec_quote(value: &str) -> String {
    systemd_quote(value, true)
}

fn systemd_environment_quote(value: &str) -> String {
    systemd_quote(value, false)
}

fn systemd_quote(value: &str, escape_dollar: bool) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '%' => quoted.push_str("%%"),
            '$' if escape_dollar => quoted.push_str("$$"),
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

fn ownership_ambiguous(message: &str) -> QghError {
    QghError::new("schedule.ownership_ambiguous", message, 6)
        .with_hint("Inspect and remove stale schedule state only after proving which user manager artifact owns the fixed manager identity.")
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
        active: Cell<bool>,
        fail_enable_once: Cell<bool>,
        fail_bootstrap_once: Cell<bool>,
        fail_disable_once: Cell<bool>,
        fail_stop_once: Cell<bool>,
        fail_inspect_once: Cell<bool>,
        inactive_check_once: Cell<bool>,
    }

    impl ManagerRunner for FakeRunner {
        fn run(&self, _program: &Path, arguments: &[String]) -> std::io::Result<ManagerResult> {
            self.calls.borrow_mut().push(arguments.to_vec());
            let forced_inactive = arguments
                .iter()
                .any(|argument| argument == "is-enabled" || argument == "print")
                && self.inactive_check_once.replace(false);
            if forced_inactive {
                self.active.set(false);
            }
            let state_check = arguments.iter().any(|argument| {
                argument == "is-enabled" || argument == "is-active" || argument == "print"
            });
            if state_check && self.fail_inspect_once.replace(false) {
                return Ok(ManagerResult {
                    success: false,
                    status_code: Some(5),
                    stderr: "Input/output error".to_string(),
                });
            }
            let should_fail = arguments.iter().any(|argument| argument == "enable")
                && self.fail_enable_once.replace(false);
            let should_fail_bootstrap = arguments.iter().any(|argument| argument == "bootstrap")
                && self.fail_bootstrap_once.replace(false);
            let should_fail_disable = arguments.iter().any(|argument| argument == "disable")
                && self.fail_disable_once.replace(false);
            let should_fail_stop = arguments.iter().any(|argument| argument == "stop")
                && self.fail_stop_once.replace(false);
            let failed =
                should_fail || should_fail_bootstrap || should_fail_disable || should_fail_stop;
            let absent = state_check && !self.active.get();
            let success = !failed && !absent;
            if success
                && arguments
                    .iter()
                    .any(|argument| argument == "enable" || argument == "bootstrap")
            {
                self.active.set(true);
            }
            if success
                && arguments
                    .iter()
                    .any(|argument| argument == "disable" || argument == "bootout")
            {
                self.active.set(false);
            }
            Ok(ManagerResult {
                success,
                status_code: Some(if failed {
                    1
                } else if absent {
                    if arguments.iter().any(|argument| argument == "is-enabled") {
                        1
                    } else {
                        3
                    }
                } else {
                    0
                }),
                stderr: String::new(),
            })
        }
    }

    fn linux_adapter(root: &Path) -> PlatformAdapter {
        PlatformAdapter::Linux {
            service_path: root.join("config/systemd/user/qgh-schedule.service"),
            timer_path: root.join("config/systemd/user/qgh-schedule.timer"),
        }
    }

    fn macos_adapter(root: &Path) -> PlatformAdapter {
        PlatformAdapter::Macos {
            plist_path: root
                .join("Library/LaunchAgents")
                .join(format!("{MACOS_LABEL}.plist")),
            stdout_path: root.join("cache/qgh/schedule/stdout.log"),
            stderr_path: root.join("cache/qgh/schedule/stderr.log"),
        }
    }

    fn prepared(adapter: &PlatformAdapter, marker: &str) -> PreparedSchedule {
        prepared_with_identity(adapter, &test_identity_for_adapter(adapter), marker)
    }

    fn prepared_with_identity(
        adapter: &PlatformAdapter,
        identity: &ScheduleOwnerIdentity,
        marker: &str,
    ) -> PreparedSchedule {
        let artifacts = adapter
            .artifact_paths()
            .into_iter()
            .map(|path| ManagedArtifact {
                logical_name: path.logical_name,
                path: path.path,
                bytes: format!("{marker}-{}", path.logical_name).into_bytes(),
            })
            .collect::<Vec<_>>();
        let (jitter_strategy, jitter_offset_seconds) = match adapter.kind() {
            PlatformKind::MacosLaunchd => ("deterministic_minute_offset".to_string(), Some(0)),
            PlatformKind::LinuxSystemdUser => ("systemd_fixed_random_delay".to_string(), None),
        };
        PreparedSchedule {
            registration: ScheduleRegistration {
                schema_version: REGISTRATION_SCHEMA.to_string(),
                platform: adapter.kind(),
                owner: identity.clone(),
                manager: adapter.capture(),
                profile_ids: vec!["work".to_string()],
                interval: FIXED_INTERVAL.to_string(),
                jitter_strategy,
                jitter_offset_seconds,
                jitter_max_seconds: JITTER_WINDOW_SECONDS,
                artifact_hash: artifact_bundle_hash(&artifacts),
            },
            artifacts,
        }
    }

    fn legacy_from_registration(registration: &ScheduleRegistration) -> LegacyScheduleRegistration {
        LegacyScheduleRegistration {
            schema_version: LEGACY_REGISTRATION_SCHEMA.to_string(),
            platform: registration.platform,
            profile_ids: registration.profile_ids.clone(),
            interval: registration.interval.clone(),
            jitter_strategy: registration.jitter_strategy.clone(),
            jitter_offset_seconds: registration.jitter_offset_seconds,
            jitter_max_seconds: registration.jitter_max_seconds,
            artifact_hash: registration.artifact_hash.clone(),
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
        assert!(rendered.contains("<string>--manager-invoked</string>"));
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
    fn macos_artifact_rejects_xml_forbidden_control_characters() {
        let environment = BTreeMap::from([
            ("HOME".to_string(), "/Users/test\u{1}".to_string()),
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ]);

        let error = render_macos_launch_agent(
            &["work".to_string()],
            Path::new("/opt/homebrew/bin/qgh"),
            &environment,
            Path::new("/tmp/qgh-out"),
            Path::new("/tmp/qgh-err"),
            12 * 60,
        )
        .unwrap_err();

        assert_eq!(error.code, "schedule.environment_invalid");
        assert_eq!(error.exit_code, 2);
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
            .contains("ExecStart=\"/home/test/bin/qgh\" \"schedule\" \"run\" \"--json\" \"--manager-invoked\" \"work\""));
        assert!(!service.contains("/bin/sh"));
        assert!(timer.contains("Persistent=true"));
        assert!(timer.contains("RandomizedDelaySec=15m"));
        assert!(timer.contains("FixedRandomDelay=true"));
    }

    #[test]
    fn systemd_artifact_uses_directive_specific_dollar_and_percent_escaping() {
        let environment = BTreeMap::from([
            ("HOME".to_string(), "/home/$USER/%n".to_string()),
            ("PATH".to_string(), "$PATH:/usr/bin/%n".to_string()),
        ]);
        let service = render_systemd_service(
            &["work".to_string()],
            Path::new("/home/$USER/%n/bin/qgh"),
            &environment,
        )
        .unwrap();
        assert!(service.contains("\"/home/$$USER/%%n/bin/qgh\""));
        assert!(service.contains("\"HOME=/home/$USER/%%n\""));
        assert!(service.contains("\"PATH=$PATH:/usr/bin/%%n\""));
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
        assert_eq!(runner.calls.borrow().len(), calls_after_stop + 1);
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 11);
        assert_eq!(calls[0], ["--user", "is-enabled", "--quiet", SYSTEMD_TIMER]);
        assert_eq!(calls[1], ["--user", "daemon-reload"]);
        assert_eq!(calls[2], ["--user", "enable", "--now", SYSTEMD_TIMER]);
        assert_eq!(calls[3], ["--user", "is-enabled", "--quiet", SYSTEMD_TIMER]);
        assert_eq!(calls[4], ["--user", "is-active", "--quiet", SYSTEMD_TIMER]);
        assert_eq!(calls[5], ["--user", "is-enabled", "--quiet", SYSTEMD_TIMER]);
        assert_eq!(calls[6], ["--user", "is-active", "--quiet", SYSTEMD_TIMER]);
        assert_eq!(calls[7], ["--user", "disable", "--now", SYSTEMD_TIMER]);
        assert_eq!(calls[8], ["--user", "stop", SYSTEMD_SERVICE]);
        assert_eq!(calls[9], ["--user", "daemon-reload"]);
        assert_eq!(
            calls[10],
            ["--user", "is-enabled", "--quiet", SYSTEMD_TIMER]
        );
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

    fn xdg_linux_adapter(root: &Path, name: &str) -> PlatformAdapter {
        PlatformAdapter::Linux {
            service_path: root.join(name).join("systemd/user").join(SYSTEMD_SERVICE),
            timer_path: root.join(name).join("systemd/user").join(SYSTEMD_TIMER),
        }
    }

    fn xdg_identity(root: &Path, name: &str) -> ScheduleOwnerIdentity {
        ScheduleOwnerIdentity {
            uid: "1000".to_string(),
            home: root.join("home"),
            manager_identity: manager_identity(PlatformKind::LinuxSystemdUser).to_string(),
            captured_environment: BTreeMap::from([(
                "XDG_CONFIG_HOME".to_string(),
                root.join(name).display().to_string(),
            )]),
        }
    }

    #[test]
    fn owner_record_environment_keeps_only_the_managed_path_root() {
        let environment = BTreeMap::from([
            ("HOME".to_string(), "/home/user".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
            (
                "GH_CONFIG_DIR".to_string(),
                "/home/user/.config/gh".to_string(),
            ),
            ("XDG_CONFIG_HOME".to_string(), "/config".to_string()),
            ("XDG_DATA_HOME".to_string(), "/data".to_string()),
            ("XDG_CACHE_HOME".to_string(), "/cache".to_string()),
        ]);

        assert_eq!(
            owner_path_environment(PlatformKind::LinuxSystemdUser, &environment),
            BTreeMap::from([("XDG_CONFIG_HOME".to_string(), "/config".to_string())])
        );
        assert_eq!(
            owner_path_environment(PlatformKind::MacosLaunchd, &environment),
            BTreeMap::from([("XDG_CACHE_HOME".to_string(), "/cache".to_string())])
        );
    }

    #[test]
    fn xdg_b_resolves_and_stops_artifacts_owned_by_xdg_a() {
        let root = TestDirectory::new("xdg-stop-recorded-owner");
        let adapter_a = xdg_linux_adapter(&root.0, "config-a");
        let adapter_b = xdg_linux_adapter(&root.0, "config-b");
        let identity_a = xdg_identity(&root.0, "config-a");
        let identity_b = xdg_identity(&root.0, "config-b");
        let owner_paths = LifecycleOwnerPaths::for_identity(&identity_a);
        assert_eq!(
            owner_paths.lock,
            LifecycleOwnerPaths::for_identity(&identity_b).lock
        );
        let desired = prepared_with_identity(&adapter_a, &identity_a, "a");
        let runner = FakeRunner::default();

        reconcile_start_owned(&owner_paths, &adapter_a, &desired, None, &runner).unwrap();
        let existing = resolve_existing_schedule(&owner_paths, &adapter_b, &identity_b)
            .unwrap()
            .expect("fixed owner record");
        assert_eq!(
            existing.adapter.artifact_paths(),
            adapter_a.artifact_paths()
        );
        assert!(snapshot_artifacts(&existing.adapter.artifact_paths())
            .unwrap()
            .iter()
            .all(|snapshot| snapshot.bytes.is_some()));

        assert_eq!(
            reconcile_stop_owned(&owner_paths, &existing.adapter, Some(&existing), &runner,)
                .unwrap(),
            "removed"
        );
        assert!(!owner_paths.registration.exists());
        assert!(adapter_a
            .artifact_paths()
            .iter()
            .all(|artifact| !artifact.path.exists()));
        assert!(adapter_b
            .artifact_paths()
            .iter()
            .all(|artifact| !artifact.path.exists()));
        assert!(!runner.active.get());
        assert!(runner
            .calls
            .borrow()
            .iter()
            .any(|call| call.iter().any(|argument| argument == "disable")));
    }

    #[test]
    fn xdg_update_moves_artifacts_and_publishes_new_owner_last() {
        let root = TestDirectory::new("xdg-update");
        let adapter_a = xdg_linux_adapter(&root.0, "config-a");
        let adapter_b = xdg_linux_adapter(&root.0, "config-b");
        let identity_a = xdg_identity(&root.0, "config-a");
        let identity_b = xdg_identity(&root.0, "config-b");
        let owner_paths = LifecycleOwnerPaths::for_identity(&identity_a);
        let first = prepared_with_identity(&adapter_a, &identity_a, "a");
        let second = prepared_with_identity(&adapter_b, &identity_b, "b");
        let runner = FakeRunner::default();
        reconcile_start_owned(&owner_paths, &adapter_a, &first, None, &runner).unwrap();
        let existing = resolve_existing_schedule(&owner_paths, &adapter_b, &identity_b)
            .unwrap()
            .unwrap();

        assert_eq!(
            reconcile_start_owned(&owner_paths, &adapter_b, &second, Some(&existing), &runner,)
                .unwrap(),
            "updated"
        );
        assert!(adapter_a
            .artifact_paths()
            .iter()
            .all(|artifact| !artifact.path.exists()));
        assert!(adapter_b
            .artifact_paths()
            .iter()
            .all(|artifact| artifact.path.exists()));
        let registration =
            parse_registration(&fs::read(&owner_paths.registration).unwrap()).unwrap();
        assert_eq!(registration.manager, adapter_b.capture());
    }

    #[test]
    fn xdg_update_rejects_a_foreign_destination_without_touching_a() {
        let root = TestDirectory::new("xdg-update-destination-collision");
        let adapter_a = xdg_linux_adapter(&root.0, "config-a");
        let adapter_b = xdg_linux_adapter(&root.0, "config-b");
        let identity_a = xdg_identity(&root.0, "config-a");
        let identity_b = xdg_identity(&root.0, "config-b");
        let owner_paths = LifecycleOwnerPaths::for_identity(&identity_a);
        let first = prepared_with_identity(&adapter_a, &identity_a, "a");
        let second = prepared_with_identity(&adapter_b, &identity_b, "b");
        let runner = FakeRunner::default();
        reconcile_start_owned(&owner_paths, &adapter_a, &first, None, &runner).unwrap();
        let before_owner = fs::read(&owner_paths.registration).unwrap();
        let before_a = snapshot_artifacts(&adapter_a.artifact_paths()).unwrap();
        let foreign_path = &adapter_b.artifact_paths()[0].path;
        write_atomic_private(foreign_path, b"foreign-owner").unwrap();
        let calls_before = runner.calls.borrow().len();
        let existing = resolve_existing_schedule(&owner_paths, &adapter_b, &identity_b)
            .unwrap()
            .unwrap();

        let error =
            reconcile_start_owned(&owner_paths, &adapter_b, &second, Some(&existing), &runner)
                .unwrap_err();
        assert_eq!(error.code, "schedule.ownership_ambiguous");
        assert_eq!(fs::read(&owner_paths.registration).unwrap(), before_owner);
        assert_eq!(fs::read(foreign_path).unwrap(), b"foreign-owner");
        assert_eq!(
            snapshot_artifacts(&adapter_a.artifact_paths())
                .unwrap()
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>(),
            before_a
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(runner.calls.borrow().len(), calls_before);
        assert!(runner.active.get());
    }

    #[test]
    fn xdg_activation_failure_restores_full_a_state() {
        let root = TestDirectory::new("xdg-update-rollback");
        let adapter_a = xdg_linux_adapter(&root.0, "config-a");
        let adapter_b = xdg_linux_adapter(&root.0, "config-b");
        let identity_a = xdg_identity(&root.0, "config-a");
        let identity_b = xdg_identity(&root.0, "config-b");
        let owner_paths = LifecycleOwnerPaths::for_identity(&identity_a);
        let first = prepared_with_identity(&adapter_a, &identity_a, "a");
        let second = prepared_with_identity(&adapter_b, &identity_b, "b");
        let runner = FakeRunner::default();
        reconcile_start_owned(&owner_paths, &adapter_a, &first, None, &runner).unwrap();
        let before_owner = fs::read(&owner_paths.registration).unwrap();
        let before_a = snapshot_artifacts(&adapter_a.artifact_paths()).unwrap();
        let existing = resolve_existing_schedule(&owner_paths, &adapter_b, &identity_b)
            .unwrap()
            .unwrap();
        runner.fail_enable_once.set(true);

        let error =
            reconcile_start_owned(&owner_paths, &adapter_b, &second, Some(&existing), &runner)
                .unwrap_err();
        assert_eq!(error.code, "schedule.manager_failed");
        assert_eq!(fs::read(&owner_paths.registration).unwrap(), before_owner);
        assert_eq!(
            snapshot_artifacts(&adapter_a.artifact_paths())
                .unwrap()
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>(),
            before_a
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>()
        );
        assert!(adapter_b
            .artifact_paths()
            .iter()
            .all(|artifact| !artifact.path.exists()));
        assert!(runner.active.get());
    }

    #[test]
    fn xdg_variants_share_one_home_and_uid_anchored_lifecycle_lock() {
        let root = TestDirectory::new("xdg-shared-lock");
        let identity_a = xdg_identity(&root.0, "config-a");
        let identity_b = xdg_identity(&root.0, "config-b");
        let paths_a = LifecycleOwnerPaths::for_identity(&identity_a);
        let paths_b = LifecycleOwnerPaths::for_identity(&identity_b);
        assert_eq!(paths_a.lock, paths_b.lock);

        let owner = FileLease::acquire_schedule_lifecycle(&paths_a.lock).unwrap();
        let error = FileLease::acquire_schedule_lifecycle(&paths_b.lock).unwrap_err();
        assert_eq!(error.code, "schedule.busy");
        drop(owner);
    }

    #[cfg(unix)]
    #[test]
    fn non_private_fixed_owner_record_is_not_trusted() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDirectory::new("owner-permissions");
        let adapter = xdg_linux_adapter(&root.0, "config-a");
        let identity = xdg_identity(&root.0, "config-a");
        let owner_paths = LifecycleOwnerPaths::for_identity(&identity);
        let desired = prepared_with_identity(&adapter, &identity, "a");
        let runner = FakeRunner::default();
        reconcile_start_owned(&owner_paths, &adapter, &desired, None, &runner).unwrap();
        fs::set_permissions(&owner_paths.registration, fs::Permissions::from_mode(0o644)).unwrap();

        let error = resolve_existing_schedule(&owner_paths, &adapter, &identity).unwrap_err();
        assert_eq!(error.code, "schedule.ownership_ambiguous");
        assert!(runner.active.get());
    }

    #[test]
    fn ambiguous_legacy_ownership_fails_without_mutation() {
        let root = TestDirectory::new("legacy-ambiguous");
        let adapter_a = xdg_linux_adapter(&root.0, "config-a");
        let adapter_b = xdg_linux_adapter(&root.0, "config-b");
        let identity = xdg_identity(&root.0, "config-a");
        let prepared_a = prepared_with_identity(&adapter_a, &identity, "same");
        let prepared_b = prepared_with_identity(&adapter_b, &identity, "same");
        write_artifacts(&prepared_a.artifacts).unwrap();
        write_artifacts(&prepared_b.artifacts).unwrap();
        let legacy_path = root.0.join("legacy/registration.json");
        let legacy = legacy_from_registration(&prepared_a.registration);
        write_atomic_private(&legacy_path, &serde_json::to_vec(&legacy).unwrap()).unwrap();
        let before_a = snapshot_artifacts(&adapter_a.artifact_paths()).unwrap();
        let before_b = snapshot_artifacts(&adapter_b.artifact_paths()).unwrap();
        let before_legacy = fs::read(&legacy_path).unwrap();
        let runner = FakeRunner::default();

        let error = prove_legacy_owner(
            vec![(legacy_path.clone(), legacy)],
            vec![adapter_a.clone(), adapter_b.clone()],
            &identity,
        )
        .unwrap_err();
        assert_eq!(error.code, "schedule.ownership_ambiguous");
        assert_eq!(fs::read(&legacy_path).unwrap(), before_legacy);
        assert_eq!(
            snapshot_artifacts(&adapter_a.artifact_paths())
                .unwrap()
                .iter()
                .map(|s| s.bytes.clone())
                .collect::<Vec<_>>(),
            before_a.iter().map(|s| s.bytes.clone()).collect::<Vec<_>>()
        );
        assert_eq!(
            snapshot_artifacts(&adapter_b.artifact_paths())
                .unwrap()
                .iter()
                .map(|s| s.bytes.clone())
                .collect::<Vec<_>>(),
            before_b.iter().map(|s| s.bytes.clone()).collect::<Vec<_>>()
        );
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn uniquely_proven_legacy_owner_migrates_to_v2_on_update() {
        let root = TestDirectory::new("legacy-unique");
        let adapter = xdg_linux_adapter(&root.0, "config-a");
        let identity = xdg_identity(&root.0, "config-a");
        let desired = prepared_with_identity(&adapter, &identity, "same");
        write_artifacts(&desired.artifacts).unwrap();
        let legacy_path = root.0.join("legacy-data/qgh/schedule/registration.json");
        let legacy = legacy_from_registration(&desired.registration);
        write_atomic_private(&legacy_path, &serde_json::to_vec(&legacy).unwrap()).unwrap();
        let existing = prove_legacy_owner(
            vec![(legacy_path.clone(), legacy)],
            vec![adapter.clone()],
            &identity,
        )
        .unwrap()
        .expect("unique legacy owner");
        let owner_paths = LifecycleOwnerPaths::for_identity(&identity);
        let runner = FakeRunner::default();

        assert_eq!(
            reconcile_start_owned(&owner_paths, &adapter, &desired, Some(&existing), &runner,)
                .unwrap(),
            "updated"
        );
        assert!(!legacy_path.exists());
        let registration =
            parse_registration(&fs::read(owner_paths.registration).unwrap()).unwrap();
        assert_eq!(registration.schema_version, REGISTRATION_SCHEMA);
        assert_eq!(registration.manager, adapter.capture());
    }

    #[test]
    fn desired_adapter_must_match_captured_xdg_environment() {
        let root = TestDirectory::new("desired-path-validation");
        let adapter = xdg_linux_adapter(&root.0, "config-a");
        let mismatched_identity = xdg_identity(&root.0, "config-b");

        let error = adapter
            .prepare(
                &mismatched_identity,
                &["work".to_string()],
                FIXED_INTERVAL,
                Path::new("/usr/local/bin/qgh"),
                &BTreeMap::new(),
            )
            .unwrap_err();
        assert_eq!(error.code, "schedule.environment_invalid");
        assert!(adapter
            .artifact_paths()
            .iter()
            .all(|artifact| !artifact.path.exists()));
    }

    #[test]
    fn relative_xdg_owner_environment_is_rejected_before_lifecycle_work() {
        let root = TestDirectory::new("relative-xdg");
        let captured =
            BTreeMap::from([("XDG_CONFIG_HOME".to_string(), "relative/config".to_string())]);

        let error = validate_owner_environment(&root.0.join("home"), &captured).unwrap_err();
        assert_eq!(error.code, "schedule.environment_invalid");
    }

    #[test]
    fn active_fixed_manager_without_owner_record_fails_closed() {
        let root = TestDirectory::new("active-without-owner");
        let adapter = xdg_linux_adapter(&root.0, "config-a");
        let identity = xdg_identity(&root.0, "config-a");
        let owner_paths = LifecycleOwnerPaths::for_identity(&identity);
        let desired = prepared_with_identity(&adapter, &identity, "a");
        let runner = FakeRunner::default();
        runner.active.set(true);

        let error =
            reconcile_start_owned(&owner_paths, &adapter, &desired, None, &runner).unwrap_err();
        assert_eq!(error.code, "schedule.ownership_ambiguous");
        assert!(!owner_paths.registration.exists());
        assert!(adapter
            .artifact_paths()
            .iter()
            .all(|artifact| !artifact.path.exists()));
        assert!(runner.active.get());

        let stop_error = reconcile_stop_owned(&owner_paths, &adapter, None, &runner).unwrap_err();
        assert_eq!(stop_error.code, "schedule.ownership_ambiguous");
        assert!(runner.active.get());
        assert!(runner.calls.borrow().iter().all(|call| {
            !call
                .iter()
                .any(|argument| argument == "disable" || argument == "bootout")
        }));
    }

    #[test]
    fn indeterminate_macos_inspection_fails_closed_before_ownerless_start() {
        let root = TestDirectory::new("macos-inspect-failure");
        let adapter = macos_adapter(&root.0);
        let identity = test_identity_for_adapter(&adapter);
        let owner_paths = test_owner_paths(&adapter);
        let desired = prepared_with_identity(&adapter, &identity, "a");
        let runner = FakeRunner::default();
        runner.fail_inspect_once.set(true);

        let error =
            reconcile_start_owned(&owner_paths, &adapter, &desired, None, &runner).unwrap_err();
        assert_eq!(error.code, "schedule.manager_failed");
        assert!(!owner_paths.registration.exists());
        assert!(adapter
            .managed_file_paths()
            .iter()
            .all(|path| !path.exists()));
        assert_eq!(runner.calls.borrow().len(), 1);
        assert_eq!(runner.calls.borrow()[0][0], "print");
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
                &test_identity_for_adapter(&adapter),
                &["work".to_string()],
                FIXED_INTERVAL,
                Path::new("/usr/local/bin/qgh"),
                &environment,
            )
            .unwrap();
        let updated = adapter
            .prepare(
                &test_identity_for_adapter(&adapter),
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
        assert_eq!(calls.len(), 7);
        assert_eq!(calls[0][0], "print");
        assert_eq!(calls[1][0], "bootstrap");
        assert_eq!(calls[2][0], "print");
        assert_eq!(calls[3][0], "bootout");
        assert_eq!(calls[4][0], "bootstrap");
        assert_eq!(calls[5][0], "print");
        assert_eq!(calls[6][0], "bootout");
        assert!(calls[1]
            .last()
            .is_some_and(|path| path.ends_with(&format!("{MACOS_LABEL}.plist"))));
        assert!(calls[4]
            .last()
            .is_some_and(|path| path.ends_with(&format!("{MACOS_LABEL}.plist"))));
        assert!(calls[3][1].ends_with(MACOS_LABEL));
        assert!(calls[6][1].ends_with(MACOS_LABEL));
        assert!(!adapter.registration_path().exists());
        assert!(adapter
            .artifact_paths()
            .iter()
            .all(|artifact| !artifact.path.exists()));
    }

    #[cfg(unix)]
    #[test]
    fn macos_runtime_logs_must_exist_and_be_private_before_ready() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDirectory::new("macos-runtime-integrity");
        let adapter = macos_adapter(&root.0);
        let desired = prepared(&adapter, "v1");
        let runner = FakeRunner::default();
        reconcile_start(&adapter, &desired, &runner).unwrap();
        let registration_path = adapter.registration_path();
        let runtime_paths = adapter.runtime_paths();

        fs::set_permissions(&runtime_paths[0], fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            inspect_schedule_state(&adapter, Some(&desired.registration), &registration_path,)
                .unwrap(),
            ("drifted", "changed")
        );
        assert_eq!(
            reconcile_start(&adapter, &desired, &runner).unwrap(),
            "updated"
        );
        assert!(is_private_file(&runtime_paths[0]));

        fs::remove_file(&runtime_paths[1]).unwrap();
        assert_eq!(
            inspect_schedule_state(&adapter, Some(&desired.registration), &registration_path,)
                .unwrap(),
            ("drifted", "missing")
        );
        assert_eq!(
            reconcile_start(&adapter, &desired, &runner).unwrap(),
            "updated"
        );
        assert!(runtime_paths[1].is_file());
        assert!(is_private_file(&runtime_paths[1]));

        let symlink_target = root.0.join("foreign-log-target");
        fs::write(&symlink_target, b"foreign").unwrap();
        let target_mode_before = fs::metadata(&symlink_target).unwrap().permissions().mode();
        fs::remove_file(&runtime_paths[0]).unwrap();
        std::os::unix::fs::symlink(&symlink_target, &runtime_paths[0]).unwrap();
        let calls_before = runner.calls.borrow().len();
        let error = reconcile_start(&adapter, &desired, &runner).unwrap_err();
        assert_eq!(error.code, "schedule.ownership_ambiguous");
        assert!(fs::symlink_metadata(&runtime_paths[0])
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&symlink_target).unwrap(), b"foreign");
        assert_eq!(
            fs::metadata(&symlink_target).unwrap().permissions().mode(),
            target_mode_before
        );
        assert_eq!(runner.calls.borrow().len(), calls_before);
        assert!(runner.active.get());
    }

    #[test]
    fn failed_macos_cache_move_removes_new_logs_and_allows_retry() {
        let root = TestDirectory::new("macos-cache-move-rollback");
        let make_adapter = |cache_name: &str| PlatformAdapter::Macos {
            plist_path: root
                .0
                .join("home/Library/LaunchAgents")
                .join(format!("{MACOS_LABEL}.plist")),
            stdout_path: root.0.join(cache_name).join("qgh/schedule/stdout.log"),
            stderr_path: root.0.join(cache_name).join("qgh/schedule/stderr.log"),
        };
        let make_identity = |cache_name: &str| ScheduleOwnerIdentity {
            uid: "1000".to_string(),
            home: root.0.join("home"),
            manager_identity: manager_identity(PlatformKind::MacosLaunchd).to_string(),
            captured_environment: BTreeMap::from([(
                "XDG_CACHE_HOME".to_string(),
                root.0.join(cache_name).display().to_string(),
            )]),
        };
        let adapter_a = make_adapter("cache-a");
        let adapter_b = make_adapter("cache-b");
        let identity_a = make_identity("cache-a");
        let identity_b = make_identity("cache-b");
        let owner_paths = LifecycleOwnerPaths::for_identity(&identity_a);
        let first = prepared_with_identity(&adapter_a, &identity_a, "a");
        let second = prepared_with_identity(&adapter_b, &identity_b, "b");
        let runner = FakeRunner::default();
        reconcile_start_owned(&owner_paths, &adapter_a, &first, None, &runner).unwrap();
        let before_owner = fs::read(&owner_paths.registration).unwrap();
        let before_a = snapshot_artifacts(&adapter_a.artifact_paths()).unwrap();
        let existing = resolve_existing_schedule(&owner_paths, &adapter_b, &identity_b)
            .unwrap()
            .unwrap();
        runner.fail_bootstrap_once.set(true);

        let error =
            reconcile_start_owned(&owner_paths, &adapter_b, &second, Some(&existing), &runner)
                .unwrap_err();
        assert_eq!(error.code, "schedule.manager_failed");
        assert_eq!(fs::read(&owner_paths.registration).unwrap(), before_owner);
        assert_eq!(
            snapshot_artifacts(&adapter_a.artifact_paths())
                .unwrap()
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>(),
            before_a
                .iter()
                .map(|snapshot| snapshot.bytes.clone())
                .collect::<Vec<_>>()
        );
        assert!(adapter_a.runtime_files_ready());
        assert!(adapter_b.runtime_paths().iter().all(|path| !path.exists()));
        assert!(runner.active.get());

        let existing = resolve_existing_schedule(&owner_paths, &adapter_b, &identity_b)
            .unwrap()
            .unwrap();
        assert_eq!(
            reconcile_start_owned(&owner_paths, &adapter_b, &second, Some(&existing), &runner,)
                .unwrap(),
            "updated"
        );
        assert!(adapter_b.runtime_files_ready());
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
    fn failed_stop_preserves_a_previously_inactive_linux_timer() {
        let root = TestDirectory::new("inactive-stop-rollback");
        let adapter = linux_adapter(&root.0);
        let desired = prepared(&adapter, "v1");
        let runner = FakeRunner::default();
        reconcile_start(&adapter, &desired, &runner).unwrap();
        runner.calls.borrow_mut().clear();
        runner.inactive_check_once.set(true);
        runner.fail_stop_once.set(true);

        let error = reconcile_stop(&adapter, &runner).unwrap_err();
        assert_eq!(error.code, "schedule.manager_failed");
        let calls = runner.calls.borrow();
        assert_eq!(calls[0], ["--user", "is-enabled", "--quiet", SYSTEMD_TIMER]);
        assert!(calls
            .iter()
            .all(|call| !call.iter().any(|arg| arg == "enable")));
    }

    #[test]
    fn failed_update_restores_a_previously_inactive_linux_timer() {
        let root = TestDirectory::new("inactive-update-rollback");
        let adapter = linux_adapter(&root.0);
        let first = prepared(&adapter, "v1");
        let second = prepared(&adapter, "v2");
        let runner = FakeRunner::default();
        reconcile_start(&adapter, &first, &runner).unwrap();
        runner.calls.borrow_mut().clear();
        runner.inactive_check_once.set(true);
        runner.fail_enable_once.set(true);

        let error = reconcile_start(&adapter, &second, &runner).unwrap_err();
        assert_eq!(error.code, "schedule.manager_failed");
        let calls = runner.calls.borrow();
        let enable_attempts = calls
            .iter()
            .filter(|call| call.iter().any(|argument| argument == "enable"))
            .count();
        assert_eq!(enable_attempts, 1, "rollback must not enable the old timer");
        assert_eq!(calls.last().unwrap(), &["--user", "daemon-reload"]);
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
        let root = TestDirectory::new("registration-unknown-field");
        let adapter = linux_adapter(&root.0);
        let mut value = serde_json::to_value(prepared(&adapter, "v1").registration).unwrap();
        value["token"] = json!("must-not-be-accepted");
        let error = parse_registration(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert_eq!(error.code, "schedule.state_invalid");
    }

    #[test]
    fn macos_exit_five_without_an_absence_message_is_not_treated_as_absent() {
        let result = ManagerResult {
            success: false,
            status_code: Some(5),
            stderr: "Input/output error".to_string(),
        };

        assert!(!manager_target_absent(PlatformKind::MacosLaunchd, &result));
    }

    #[cfg(unix)]
    #[test]
    fn program_lookup_skips_a_non_executable_file_earlier_on_path() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDirectory::new("program-lookup");
        let first = root.0.join("first");
        let second = root.0.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let non_executable = first.join("gh");
        let executable = second.join("gh");
        fs::write(&non_executable, b"not executable").unwrap();
        fs::write(&executable, b"executable").unwrap();
        fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let path = env::join_paths([first, second]).unwrap();

        assert_eq!(
            find_program_in_path(&OsString::from("gh"), &path),
            Some(executable)
        );
    }

    #[test]
    fn scheduled_gh_config_dir_requires_an_absolute_normalized_path() {
        for invalid in ["relative/gh", "../gh", "/tmp/../gh"] {
            let error = validate_scheduled_gh_config_dir(invalid).unwrap_err();
            assert_eq!(error.code, "schedule.environment_invalid");
        }
        validate_scheduled_gh_config_dir("/Users/test/.config/gh").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn program_lookup_skips_an_executable_from_a_relative_path_entry() {
        use std::os::unix::fs::PermissionsExt;

        let cwd = env::current_dir().unwrap();
        let directory = cwd
            .join("target")
            .join(format!("qgh-relative-path-{}", now_run_id_suffix()));
        let root = TestDirectory(directory.clone());
        fs::create_dir_all(&root.0).unwrap();
        let executable = root.0.join("gh");
        fs::write(&executable, b"executable").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let relative = root.0.strip_prefix(&cwd).unwrap();
        let path = env::join_paths([relative]).unwrap();

        assert_eq!(find_program_in_path(OsStr::new("gh"), &path), None);
        assert_eq!(
            find_program_in_directories(OsStr::new("gh"), [root.0.clone()].into_iter()),
            Some(executable)
        );
    }
}
