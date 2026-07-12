use crate::error::QghError;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ProfilePaths {
    pub config_file: PathBuf,
    pub profile_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub log_dir: PathBuf,
    pub db_path: PathBuf,
    pub index_root: PathBuf,
    pub index_active: PathBuf,
}

impl ProfilePaths {
    pub fn resolve(profile_id: &str) -> Result<Self, QghError> {
        let data_home = xdg_or_home("XDG_DATA_HOME", ".local/share")?;
        let cache_home = xdg_or_home("XDG_CACHE_HOME", ".cache")?;
        let profile_dir = data_home.join("qgh").join("profiles").join(profile_id);
        let index_root = profile_dir.join("tantivy");
        let cache_dir = cache_home.join("qgh");
        Ok(Self {
            config_file: config_file_path()?,
            profile_dir: profile_dir.clone(),
            log_dir: cache_dir.join("logs"),
            cache_dir,
            db_path: profile_dir.join("qgh.sqlite3"),
            index_active: index_root.join("active"),
            index_root,
        })
    }
}

pub fn qgh_cache_dir() -> Result<PathBuf, QghError> {
    Ok(xdg_or_home("XDG_CACHE_HOME", ".cache")?.join("qgh"))
}

pub fn config_file_path() -> Result<PathBuf, QghError> {
    Ok(xdg_or_home("XDG_CONFIG_HOME", ".config")?
        .join("qgh")
        .join("config.toml"))
}

fn xdg_or_home(env_name: &str, suffix: &str) -> Result<PathBuf, QghError> {
    if let Some(value) = std::env::var_os(env_name) {
        return Ok(PathBuf::from(value));
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Err(QghError::config(format!(
            "{env_name} is not set and HOME is unavailable."
        )));
    };
    Ok(PathBuf::from(home).join(suffix))
}

pub fn ensure_private_dir(path: &Path) -> Result<(), QghError> {
    fs::create_dir_all(path)?;
    set_private_dir(path)
}

pub fn set_private_dir(path: &Path) -> Result<(), QghError> {
    set_mode(path, 0o700)
}

pub fn set_private_file(path: &Path) -> Result<(), QghError> {
    if path.exists() {
        set_mode(path, 0o600)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), QghError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), QghError> {
    Ok(())
}
