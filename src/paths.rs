use crate::error::QghError;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ProfilePaths {
    pub config_file: PathBuf,
    pub profile_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub db_path: PathBuf,
    pub index_root: PathBuf,
    pub index_active: PathBuf,
}

impl ProfilePaths {
    pub fn resolve(profile_id: &str) -> Result<Self, QghError> {
        let config_home = xdg_or_home("XDG_CONFIG_HOME", ".config")?;
        let data_home = xdg_or_home("XDG_DATA_HOME", ".local/share")?;
        let cache_home = xdg_or_home("XDG_CACHE_HOME", ".cache")?;
        let profile_dir = data_home.join("qgh").join("profiles").join(profile_id);
        let index_root = profile_dir.join("tantivy");
        Ok(Self {
            config_file: config_home.join("qgh").join("config.toml"),
            profile_dir: profile_dir.clone(),
            cache_dir: cache_home.join("qgh"),
            db_path: profile_dir.join("qgh.sqlite3"),
            index_active: index_root.join("active"),
            index_root,
        })
    }
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
