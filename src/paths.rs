use std::io;
use std::path::{Path, PathBuf};

pub fn state_root_with(xdg_state_home: Option<&str>, home: Option<&str>) -> io::Result<PathBuf> {
    if let Some(xdg) = xdg_state_home
        && !xdg.is_empty()
    {
        return absolute_root("XDG_STATE_HOME", xdg).map(|path| path.join("hooklinesinker"));
    }
    home_root(home).map(|path| path.join(".local/state/hooklinesinker"))
}

pub fn state_root() -> io::Result<PathBuf> {
    state_root_with(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

pub fn data_root_with(xdg_data_home: Option<&str>, home: Option<&str>) -> io::Result<PathBuf> {
    if let Some(xdg) = xdg_data_home
        && !xdg.is_empty()
    {
        return absolute_root("XDG_DATA_HOME", xdg).map(|path| path.join("hooklinesinker"));
    }
    home_root(home).map(|path| path.join(".local/share/hooklinesinker"))
}

pub fn data_root() -> io::Result<PathBuf> {
    data_root_with(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

pub fn home_dir() -> io::Result<PathBuf> {
    home_root(std::env::var("HOME").ok().as_deref())
}

pub fn absolute_env_path(name: &str) -> io::Result<Option<PathBuf>> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| absolute_root(name, &value))
        .transpose()
}

fn home_root(home: Option<&str>) -> io::Result<PathBuf> {
    let home = home.filter(|value| !value.is_empty()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "HOME must name an absolute directory",
        )
    })?;
    absolute_root("HOME", home)
}

fn absolute_root(name: &str, value: &str) -> io::Result<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be an absolute path, got {value:?}"),
        ))
    }
}

pub fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    set_private_dir_mode(path)
}

#[cfg(unix)]
fn set_private_dir_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir_mode(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_state_home_wins_when_set() {
        let root = state_root_with(Some("/custom/state"), Some("/home/user")).unwrap();
        assert_eq!(root, PathBuf::from("/custom/state/hooklinesinker"));
    }

    #[test]
    fn empty_xdg_state_home_falls_back_to_home() {
        let root = state_root_with(Some(""), Some("/home/user")).unwrap();
        assert_eq!(
            root,
            PathBuf::from("/home/user/.local/state/hooklinesinker")
        );
    }

    #[test]
    fn missing_xdg_state_home_falls_back_to_home() {
        let root = state_root_with(None, Some("/home/user")).unwrap();
        assert_eq!(
            root,
            PathBuf::from("/home/user/.local/state/hooklinesinker")
        );
    }

    #[test]
    fn xdg_data_home_wins_when_set() {
        let root = data_root_with(Some("/custom/data"), Some("/home/user")).unwrap();
        assert_eq!(root, PathBuf::from("/custom/data/hooklinesinker"));
    }

    #[test]
    fn empty_xdg_data_home_falls_back_to_home() {
        let root = data_root_with(Some(""), Some("/home/user")).unwrap();
        assert_eq!(
            root,
            PathBuf::from("/home/user/.local/share/hooklinesinker")
        );
    }

    #[test]
    fn missing_xdg_data_home_falls_back_to_home() {
        let root = data_root_with(None, Some("/home/user")).unwrap();
        assert_eq!(
            root,
            PathBuf::from("/home/user/.local/share/hooklinesinker")
        );
    }

    #[test]
    fn relative_xdg_roots_are_rejected() {
        let error = state_root_with(Some("relative/state"), Some("/home/user")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("XDG_STATE_HOME"));

        let error = data_root_with(Some("relative/data"), Some("/home/user")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("XDG_DATA_HOME"));
    }

    #[test]
    fn missing_or_relative_home_is_rejected() {
        assert_eq!(
            state_root_with(None, None).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            data_root_with(None, Some("relative/home"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
