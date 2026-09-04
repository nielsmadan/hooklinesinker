use std::path::{Path, PathBuf};

pub fn state_root_with(xdg_state_home: Option<&str>, home: Option<&str>) -> PathBuf {
    if let Some(xdg) = xdg_state_home
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("hooklinesinker");
    }
    PathBuf::from(home.unwrap_or(".")).join(".local/state/hooklinesinker")
}

pub fn state_root() -> PathBuf {
    state_root_with(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
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
        let root = state_root_with(Some("/custom/state"), Some("/home/user"));
        assert_eq!(root, PathBuf::from("/custom/state/hooklinesinker"));
    }

    #[test]
    fn empty_xdg_state_home_falls_back_to_home() {
        let root = state_root_with(Some(""), Some("/home/user"));
        assert_eq!(
            root,
            PathBuf::from("/home/user/.local/state/hooklinesinker")
        );
    }

    #[test]
    fn missing_xdg_state_home_falls_back_to_home() {
        let root = state_root_with(None, Some("/home/user"));
        assert_eq!(
            root,
            PathBuf::from("/home/user/.local/state/hooklinesinker")
        );
    }
}
