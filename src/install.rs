use crate::consumers::{Consumer, ConsumerStore};
use crate::hooks::HookManager;
use crate::protocol::Agent;
use crate::state::write_private_atomic;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
}

impl SemVer {
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for SemVer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

pub struct Candidate {
    pub version: SemVer,
    pub protocol_major: u16,
    pub binary_path: PathBuf,
}

impl Candidate {
    pub fn current(binary_path: PathBuf) -> io::Result<Self> {
        let version = SemVer::parse(env!("CARGO_PKG_VERSION")).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "hooklinesinker's own CARGO_PKG_VERSION is not a valid semver",
            )
        })?;
        Ok(Self {
            version,
            protocol_major: crate::protocol::PROTOCOL_VERSION,
            binary_path,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledVersion {
    pub active_version: String,
    pub protocol_major: u16,
}

struct ActiveVersion {
    version: SemVer,
    version_str: String,
    protocol_major: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UninstallOutcome {
    pub was_last_consumer: bool,
    pub active_version: Option<String>,
}

pub struct Installer {
    data_root: PathBuf,
}

impl Installer {
    pub fn open(data_root: PathBuf) -> io::Result<Self> {
        crate::paths::ensure_private_dir(&data_root)?;
        Ok(Self { data_root })
    }

    fn versions_dir(&self) -> PathBuf {
        self.data_root.join("versions")
    }

    fn bin_dir(&self) -> PathBuf {
        self.data_root.join("bin")
    }

    pub fn binary_path(&self) -> PathBuf {
        self.bin_dir().join("hooklinesinker")
    }

    fn active_version(&self) -> io::Result<Option<ActiveVersion>> {
        let target = match fs::read_link(self.binary_path()) {
            Ok(target) => target,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let version_str = target
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("malformed active symlink target: {}", target.display()),
                )
            })?
            .to_string();
        let version = SemVer::parse(&version_str).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed active version directory name: {version_str}"),
            )
        })?;
        let protocol_major = self.read_protocol(&version_str)?;
        Ok(Some(ActiveVersion {
            version,
            version_str,
            protocol_major,
        }))
    }

    fn read_protocol(&self, version_str: &str) -> io::Result<u16> {
        let path = self.versions_dir().join(version_str).join("protocol");
        let text = fs::read_to_string(path)?;
        text.trim().parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed protocol marker for version {version_str}"),
            )
        })
    }

    pub fn active_version_summary(&self) -> io::Result<Option<InstalledVersion>> {
        Ok(self.active_version()?.map(|a| InstalledVersion {
            active_version: a.version_str,
            protocol_major: a.protocol_major,
        }))
    }

    pub fn install_candidate(&self, candidate: Candidate) -> io::Result<InstalledVersion> {
        match self.active_version()? {
            None => self.activate(&candidate),
            Some(active) => {
                if candidate.protocol_major != active.protocol_major {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "candidate protocol {} is incompatible with active protocol {}",
                            candidate.protocol_major, active.protocol_major
                        ),
                    ));
                }
                if candidate.version > active.version {
                    self.activate(&candidate)
                } else {
                    Ok(InstalledVersion {
                        active_version: active.version_str,
                        protocol_major: active.protocol_major,
                    })
                }
            }
        }
    }

    fn activate(&self, candidate: &Candidate) -> io::Result<InstalledVersion> {
        let version_str = candidate.version.to_string();
        let version_dir = self.versions_dir().join(&version_str);
        crate::paths::ensure_private_dir(&self.versions_dir())?;
        crate::paths::ensure_private_dir(&version_dir)?;

        let target_binary = version_dir.join("hooklinesinker");
        copy_executable_and_fsync(&candidate.binary_path, &target_binary)?;

        let protocol_path = version_dir.join("protocol");
        write_private_atomic(
            &protocol_path,
            candidate.protocol_major.to_string().as_bytes(),
        )?;

        self.switch_symlink(&version_str)?;

        Ok(InstalledVersion {
            active_version: version_str,
            protocol_major: candidate.protocol_major,
        })
    }

    fn switch_symlink(&self, version_str: &str) -> io::Result<()> {
        let bin_dir = self.bin_dir();
        crate::paths::ensure_private_dir(&bin_dir)?;
        let target = Path::new("..")
            .join("versions")
            .join(version_str)
            .join("hooklinesinker");
        let tmp_link = bin_dir.join(format!(".hooklinesinker.tmp-{}", std::process::id()));
        let _ = fs::remove_file(&tmp_link);
        symlink(&target, &tmp_link)?;
        fs::rename(&tmp_link, self.binary_path())?;
        Ok(())
    }

    pub fn install_current(
        &self,
        consumers: &ConsumerStore,
        consumer: Consumer,
    ) -> io::Result<InstalledVersion> {
        let current_exe = std::env::current_exe()?;
        let candidate = Candidate::current(current_exe)?;
        let installed = self.install_candidate(candidate)?;
        consumers.register(consumer)?;
        Ok(installed)
    }

    pub fn uninstall_consumer(
        &self,
        consumers: &ConsumerStore,
        hooks: &HookManager,
        name: &str,
    ) -> io::Result<UninstallOutcome> {
        consumers.remove(name)?;
        let remaining = consumers.list()?;
        if !remaining.is_empty() {
            let active_version = self.active_version()?.map(|a| a.version_str);
            return Ok(UninstallOutcome {
                was_last_consumer: false,
                active_version,
            });
        }

        for agent in [
            Agent::Claude,
            Agent::Codex,
            Agent::Opencode,
            Agent::Pi,
            Agent::Droid,
            Agent::Qwen,
            Agent::Kimi,
        ] {
            hooks.uninstall(agent)?;
        }

        match fs::remove_file(self.binary_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }

        Ok(UninstallOutcome {
            was_last_consumer: true,
            active_version: None,
        })
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "hooklinesinker self-install requires a unix symlink-capable filesystem",
    ))
}

fn copy_executable_and_fsync(src: &Path, dst: &Path) -> io::Result<()> {
    let bytes = fs::read(src)?;
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o755);
    }
    let mut file: File = options.open(dst)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dst, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{HookManager, HookRoots};

    static NEXT_TEMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "hooklinesinker-install-test-{label}-{}-{nanos}-{id}",
            std::process::id()
        ))
    }

    fn fake_binary(version: &str) -> PathBuf {
        let path = temp_root(&format!("candidate-{version}")).join("hooklinesinker-src");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("#!/bin/sh\n# candidate {version}\n")).unwrap();
        path
    }

    fn candidate(version: &str, protocol_major: u16) -> Candidate {
        Candidate {
            version: SemVer::parse(version).unwrap(),
            protocol_major,
            binary_path: fake_binary(version),
        }
    }

    fn installation() -> Installer {
        Installer::open(temp_root("installer")).unwrap()
    }

    fn installation_with_active(version: &str, protocol_major: u16) -> Installer {
        let install = installation();
        install
            .install_candidate(candidate(version, protocol_major))
            .unwrap();
        install
    }

    fn hook_manager_for(install: &Installer) -> HookManager {
        let base = temp_root("hooks-for-uninstall");
        HookManager::new(HookRoots {
            claude_dir: base.join("claude"),
            codex_dir: base.join("codex"),
            opencode_config_dir: base.join("opencode"),
            pi_agent_dir: base.join("pi"),
            factory_dir: base.join("factory"),
            qwen_config_dir: base.join("qwen"),
            kimi_code_dir: base.join("kimi-code"),
            binary_path: install.binary_path(),
        })
    }

    #[test]
    fn missing_install_activates_the_candidate() {
        let install = installation();
        let result = install.install_candidate(candidate("0.1.0", 1)).unwrap();
        assert_eq!(result.active_version, "0.1.0");
        assert_eq!(result.protocol_major, 1);
        assert!(install.binary_path().exists());
    }

    #[test]
    fn installing_the_same_version_again_is_a_no_op() {
        let install = installation_with_active("0.2.0", 1);
        let result = install.install_candidate(candidate("0.2.0", 1)).unwrap();
        assert_eq!(result.active_version, "0.2.0");
    }

    #[test]
    fn older_bundled_binary_never_downgrades_compatible_active_version() {
        let install = installation_with_active("0.2.0", 1);
        let result = install.install_candidate(candidate("0.1.0", 1)).unwrap();
        assert_eq!(result.active_version, "0.2.0");
    }

    #[test]
    fn newer_compatible_candidate_replaces_the_active_version() {
        let install = installation_with_active("0.2.0", 1);
        let result = install.install_candidate(candidate("0.3.0", 1)).unwrap();
        assert_eq!(result.active_version, "0.3.0");
    }

    #[test]
    fn incompatible_protocol_major_is_rejected() {
        let install = installation_with_active("0.2.0", 1);
        let result = install.install_candidate(candidate("0.3.0", 2));
        assert!(result.is_err());
        let active = install.install_candidate(candidate("0.2.0", 1)).unwrap();
        assert_eq!(active.active_version, "0.2.0");
    }

    #[test]
    fn activation_switches_a_relative_symlink_atomically() {
        let install = installation_with_active("0.4.0", 1);
        let link_meta = fs::symlink_metadata(install.binary_path()).unwrap();
        assert!(link_meta.file_type().is_symlink());
        let target = fs::read_link(install.binary_path()).unwrap();
        assert!(!target.is_absolute());
        assert_eq!(
            target,
            PathBuf::from("..")
                .join("versions")
                .join("0.4.0")
                .join("hooklinesinker")
        );

        install.install_candidate(candidate("0.5.0", 1)).unwrap();
        let target = fs::read_link(install.binary_path()).unwrap();
        assert_eq!(
            target,
            PathBuf::from("..")
                .join("versions")
                .join("0.5.0")
                .join("hooklinesinker")
        );
        assert!(
            install
                .versions_dir()
                .join("0.4.0")
                .join("hooklinesinker")
                .exists(),
            "the previous version directory must be kept for rollback"
        );
    }

    #[test]
    fn install_current_registers_the_consumer_only_after_activation() {
        let install = installation();
        let consumers = ConsumerStore::open(temp_root("consumers-first")).unwrap();
        let consumer = Consumer {
            name: "juggler".to_string(),
            protocol: crate::protocol::PROTOCOL_VERSION,
            capabilities: vec!["status".to_string()],
            sink: None,
        };
        install.install_current(&consumers, consumer).unwrap();
        assert!(install.binary_path().exists());
        assert_eq!(consumers.list().unwrap().len(), 1);
    }

    #[test]
    fn a_second_consumer_can_register_without_reactivating() {
        let install = installation();
        let consumers = ConsumerStore::open(temp_root("consumers-second")).unwrap();
        install
            .install_current(
                &consumers,
                Consumer {
                    name: "juggler".to_string(),
                    protocol: crate::protocol::PROTOCOL_VERSION,
                    capabilities: vec!["status".to_string()],
                    sink: None,
                },
            )
            .unwrap();
        install
            .install_current(
                &consumers,
                Consumer {
                    name: "ringleader".to_string(),
                    protocol: crate::protocol::PROTOCOL_VERSION,
                    capabilities: vec!["status".to_string()],
                    sink: None,
                },
            )
            .unwrap();
        assert_eq!(consumers.list().unwrap().len(), 2);
    }

    #[test]
    fn uninstalling_one_of_two_consumers_keeps_the_active_symlink() {
        let install = installation_with_active("0.6.0", 1);
        let consumers = ConsumerStore::open(temp_root("consumers-uninstall-one")).unwrap();
        consumers
            .register(Consumer {
                name: "juggler".to_string(),
                protocol: crate::protocol::PROTOCOL_VERSION,
                capabilities: vec!["status".to_string()],
                sink: None,
            })
            .unwrap();
        consumers
            .register(Consumer {
                name: "ringleader".to_string(),
                protocol: crate::protocol::PROTOCOL_VERSION,
                capabilities: vec!["status".to_string()],
                sink: None,
            })
            .unwrap();
        let hooks = hook_manager_for(&install);

        let outcome = install
            .uninstall_consumer(&consumers, &hooks, "juggler")
            .unwrap();
        assert!(!outcome.was_last_consumer);
        assert!(install.binary_path().exists());
    }

    #[test]
    fn uninstalling_the_last_consumer_removes_hooks_then_the_symlink_but_keeps_versions() {
        let install = installation_with_active("0.7.0", 1);
        let consumers = ConsumerStore::open(temp_root("consumers-last")).unwrap();
        consumers
            .register(Consumer {
                name: "juggler".to_string(),
                protocol: crate::protocol::PROTOCOL_VERSION,
                capabilities: vec!["status".to_string()],
                sink: None,
            })
            .unwrap();
        let hooks = hook_manager_for(&install);
        hooks.install(Agent::Claude).unwrap();

        let outcome = install
            .uninstall_consumer(&consumers, &hooks, "juggler")
            .unwrap();
        assert!(outcome.was_last_consumer);
        assert!(!install.binary_path().exists());
        assert!(
            install
                .versions_dir()
                .join("0.7.0")
                .join("hooklinesinker")
                .exists(),
            "version directories must survive last-consumer cleanup for repair/rollback"
        );
        let status = hooks.status(Agent::Claude).unwrap();
        assert_eq!(status.state, crate::hooks::HookState::Missing);
    }
}
