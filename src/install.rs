use crate::consumers::{ConsumerStore, RemoveOutcome};
use crate::hooks::HookManager;
use crate::persistence::{LockGuard, write_private_atomic};
#[cfg(test)]
use crate::protocol::Capability;
use crate::protocol::{Agent, Consumer};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
}

impl SemVer {
    pub(crate) fn parse(value: &str) -> Option<Self> {
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

pub(crate) struct Candidate {
    pub version: SemVer,
    pub protocol_major: u16,
    pub binary_path: PathBuf,
}

impl Candidate {
    pub(crate) fn current(binary_path: PathBuf) -> io::Result<Self> {
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
pub(crate) struct InstalledVersion {
    pub active_version: String,
    pub protocol_major: u16,
}

struct ActiveVersion {
    version: SemVer,
    version_str: String,
    protocol_major: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UninstallOutcome {
    pub was_last_consumer: bool,
    pub active_version: Option<String>,
}

pub(crate) struct Installer {
    data_root: PathBuf,
}

impl Installer {
    pub(crate) fn open(data_root: PathBuf) -> io::Result<Self> {
        crate::paths::ensure_private_dir(&data_root)?;
        Ok(Self { data_root })
    }

    fn versions_dir(&self) -> PathBuf {
        self.data_root.join("versions")
    }

    fn bin_dir(&self) -> PathBuf {
        self.data_root.join("bin")
    }

    pub(crate) fn binary_path(&self) -> PathBuf {
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

    fn active_binary_is_usable(&self, active: &ActiveVersion) -> bool {
        let binary = self
            .versions_dir()
            .join(&active.version_str)
            .join("hooklinesinker");
        let Ok(metadata) = fs::metadata(binary) else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return false;
            }
        }
        true
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

    pub(crate) fn active_version_summary(&self) -> io::Result<Option<InstalledVersion>> {
        let _guard = self.lock()?;
        let Some(active) = self.active_version()? else {
            return Ok(None);
        };
        if !self.active_binary_is_usable(&active) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "active hooklinesinker {} is missing or not executable",
                    active.version_str
                ),
            ));
        }
        Ok(Some(InstalledVersion {
            active_version: active.version_str,
            protocol_major: active.protocol_major,
        }))
    }

    fn lock(&self) -> io::Result<LockGuard> {
        LockGuard::acquire(&self.data_root.join("install.lock"))
    }

    #[cfg(test)]
    pub(crate) fn install_candidate(&self, candidate: &Candidate) -> io::Result<InstalledVersion> {
        let _guard = self.lock()?;
        self.install_candidate_locked(candidate)
    }

    fn install_candidate_locked(&self, candidate: &Candidate) -> io::Result<InstalledVersion> {
        match self.active_version()? {
            None => self.activate(candidate),
            Some(active) => {
                if !self.active_binary_is_usable(&active) {
                    return self.activate(candidate);
                }
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
                    self.activate(candidate)
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

    pub(crate) fn install_current(
        &self,
        consumers: &ConsumerStore,
        consumer: &Consumer,
    ) -> io::Result<InstalledVersion> {
        consumer.validate()?;
        let current_exe = std::env::current_exe()?;
        let candidate = Candidate::current(current_exe)?;
        let _guard = self.lock()?;
        let installed = self.install_candidate_locked(&candidate)?;
        consumers.register(consumer)?;
        Ok(installed)
    }

    pub(crate) fn uninstall_consumer(
        &self,
        consumers: &ConsumerStore,
        hooks: &HookManager,
        name: &str,
    ) -> io::Result<UninstallOutcome> {
        let _guard = self.lock()?;
        let registered = consumers.list()?;
        if !registered.iter().any(|consumer| consumer.name == name) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("consumer {name} is not registered"),
            ));
        }
        if registered.len() > 1 {
            let removed = consumers.remove(name)?;
            debug_assert_eq!(removed, RemoveOutcome::Removed);
            let active_version = self.active_version()?.map(|a| a.version_str);
            return Ok(UninstallOutcome {
                was_last_consumer: false,
                active_version,
            });
        }

        for agent in Agent::ALL {
            let status = hooks.uninstall(agent)?;
            if status.state != crate::hooks::HookState::Missing {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} hooks remain {} after uninstall",
                        agent.as_str(),
                        status.state.as_str()
                    ),
                ));
            }
        }

        match fs::remove_file(self.binary_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let removed = consumers.remove(name)?;
        debug_assert_eq!(removed, RemoveOutcome::Removed);

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
    fs::copy(src, dst)?;
    let file = File::open(dst)?;
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
    use crate::hooks::{HookManager, HookRoots, HookState};

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
            .install_candidate(&candidate(version, protocol_major))
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
        let result = install.install_candidate(&candidate("0.1.0", 1)).unwrap();
        assert_eq!(result.active_version, "0.1.0");
        assert_eq!(result.protocol_major, 1);
        assert!(install.binary_path().exists());
    }

    #[test]
    fn installing_the_same_version_again_is_a_no_op() {
        let install = installation_with_active("0.2.0", 1);
        let result = install.install_candidate(&candidate("0.2.0", 1)).unwrap();
        assert_eq!(result.active_version, "0.2.0");
    }

    #[test]
    fn older_bundled_binary_never_downgrades_compatible_active_version() {
        let install = installation_with_active("0.2.0", 1);
        let result = install.install_candidate(&candidate("0.1.0", 1)).unwrap();
        assert_eq!(result.active_version, "0.2.0");
    }

    #[test]
    fn a_damaged_active_binary_is_repaired_from_the_candidate() {
        let install = installation_with_active("0.2.0", 1);
        let candidate = candidate("0.1.0", 1);
        let active_target = install.versions_dir().join("0.2.0").join("hooklinesinker");
        fs::remove_file(&active_target).unwrap();

        let result = install.install_candidate(&candidate).unwrap();
        assert_eq!(result.active_version, "0.1.0");
        assert_eq!(
            fs::read(install.binary_path()).unwrap(),
            fs::read(candidate.binary_path).unwrap()
        );
    }

    #[test]
    fn newer_compatible_candidate_replaces_the_active_version() {
        let install = installation_with_active("0.2.0", 1);
        let result = install.install_candidate(&candidate("0.3.0", 1)).unwrap();
        assert_eq!(result.active_version, "0.3.0");
    }

    #[test]
    fn incompatible_protocol_major_is_rejected() {
        let install = installation_with_active("0.2.0", 1);
        let result = install.install_candidate(&candidate("0.3.0", 2));
        assert!(result.is_err());
        let active = install.install_candidate(&candidate("0.2.0", 1)).unwrap();
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

        install.install_candidate(&candidate("0.5.0", 1)).unwrap();
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
            capabilities: vec![Capability::Status],
            sink: None,
        };
        install.install_current(&consumers, &consumer).unwrap();
        assert!(install.binary_path().exists());
        assert_eq!(consumers.list().unwrap().len(), 1);
    }

    #[test]
    fn install_current_validates_the_consumer_before_activation() {
        let install = installation();
        let consumers = ConsumerStore::open(temp_root("consumers-invalid")).unwrap();
        let consumer = Consumer {
            name: "Invalid".to_string(),
            protocol: crate::protocol::PROTOCOL_VERSION,
            capabilities: vec![Capability::Status],
            sink: None,
        };
        assert!(install.install_current(&consumers, &consumer).is_err());
        assert!(!install.binary_path().exists());
    }

    #[test]
    fn a_second_consumer_can_register_without_reactivating() {
        let install = installation();
        let consumers = ConsumerStore::open(temp_root("consumers-second")).unwrap();
        install
            .install_current(
                &consumers,
                &Consumer {
                    name: "juggler".to_string(),
                    protocol: crate::protocol::PROTOCOL_VERSION,
                    capabilities: vec![Capability::Status],
                    sink: None,
                },
            )
            .unwrap();
        install
            .install_current(
                &consumers,
                &Consumer {
                    name: "ringleader".to_string(),
                    protocol: crate::protocol::PROTOCOL_VERSION,
                    capabilities: vec![Capability::Status],
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
            .register(&Consumer {
                name: "juggler".to_string(),
                protocol: crate::protocol::PROTOCOL_VERSION,
                capabilities: vec![Capability::Status],
                sink: None,
            })
            .unwrap();
        consumers
            .register(&Consumer {
                name: "ringleader".to_string(),
                protocol: crate::protocol::PROTOCOL_VERSION,
                capabilities: vec![Capability::Status],
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
            .register(&Consumer {
                name: "juggler".to_string(),
                protocol: crate::protocol::PROTOCOL_VERSION,
                capabilities: vec![Capability::Status],
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

    #[test]
    fn last_consumer_uninstall_removes_hooks_for_every_agent() {
        let install = installation_with_active("0.7.0", 1);
        let consumers = ConsumerStore::open(temp_root("consumers-all-hooks")).unwrap();
        consumers
            .register(&Consumer {
                name: "juggler".to_string(),
                protocol: crate::protocol::PROTOCOL_VERSION,
                capabilities: vec![Capability::Status],
                sink: None,
            })
            .unwrap();
        let hooks = hook_manager_for(&install);
        let agents = [
            Agent::Claude,
            Agent::Codex,
            Agent::Opencode,
            Agent::Pi,
            Agent::Droid,
            Agent::Qwen,
            Agent::Kimi,
        ];
        for agent in agents {
            hooks.install(agent).unwrap();
        }

        install
            .uninstall_consumer(&consumers, &hooks, "juggler")
            .unwrap();

        for agent in agents {
            assert_eq!(hooks.status(agent).unwrap().state, HookState::Missing);
        }
        assert!(!install.binary_path().exists());
    }

    #[test]
    fn damaged_remaining_registration_preserves_shared_installation() {
        for unreadable in [false, true] {
            let install = installation_with_active("0.7.0", 1);
            let root = temp_root("damaged-consumer");
            let consumers = ConsumerStore::open(root.clone()).unwrap();
            consumers
                .register(&Consumer {
                    name: "one".into(),
                    protocol: 1,
                    capabilities: vec![Capability::Status],
                    sink: None,
                })
                .unwrap();
            let remaining = root.join("consumers/two.json");
            if unreadable {
                fs::create_dir(&remaining).unwrap();
            } else {
                fs::write(&remaining, "{").unwrap();
            }
            let hooks = hook_manager_for(&install);
            hooks.install(Agent::Claude).unwrap();
            let error = install
                .uninstall_consumer(&consumers, &hooks, "one")
                .unwrap_err();
            assert!(error.to_string().contains("two.json"));
            assert!(remaining.exists());
            assert_eq!(
                install
                    .active_version_summary()
                    .unwrap()
                    .unwrap()
                    .active_version,
                "0.7.0"
            );
            assert_eq!(
                hooks.status(Agent::Claude).unwrap().state,
                crate::hooks::HookState::Installed
            );
        }
    }

    #[test]
    fn waiting_installer_rechecks_the_version_after_acquiring_the_lock() {
        use std::sync::mpsc;
        use std::time::Duration;
        let install = installation_with_active("1.0.2", 1);
        let older = candidate("1.0.3", 1);
        let newer = candidate("1.0.4", 1);
        let guard = install.lock().unwrap();
        let root = install.data_root.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let installer = Installer::open(root).unwrap();
            started_tx.send(()).unwrap();
            let result = installer.install_candidate(&older);
            done_tx.send(result).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let premature = done_rx.recv_timeout(Duration::from_millis(100));
        install.install_candidate_locked(&newer).unwrap();
        drop(guard);
        worker.join().unwrap();
        assert!(matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)));
        assert_eq!(done_rx.recv().unwrap().unwrap().active_version, "1.0.4");
        assert_eq!(
            fs::read(install.binary_path()).unwrap(),
            fs::read(&newer.binary_path).unwrap()
        );
    }

    #[test]
    fn active_version_summary_waits_for_the_installation_transaction() {
        use std::sync::mpsc;
        use std::time::Duration;

        let install = installation_with_active("1.0.2", 1);
        let newer = candidate("1.0.3", 1);
        let guard = install.lock().unwrap();
        let root = install.data_root.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let installer = Installer::open(root).unwrap();
            started_tx.send(()).unwrap();
            done_tx.send(installer.active_version_summary()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let premature = done_rx.recv_timeout(Duration::from_millis(100));
        install.install_candidate_locked(&newer).unwrap();
        drop(guard);
        reader.join().unwrap();
        assert!(matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)));
        assert_eq!(
            done_rx.recv().unwrap().unwrap(),
            Some(InstalledVersion {
                active_version: "1.0.3".into(),
                protocol_major: 1,
            })
        );
    }

    #[test]
    fn install_current_keeps_registration_inside_the_uninstall_transaction() {
        use fs2::FileExt;
        use std::time::{Duration, Instant};

        let install = installation_with_active("0.0.1", crate::protocol::PROTOCOL_VERSION);
        let state_root = temp_root("blocked-registration");
        let consumers = ConsumerStore::open(&state_root).unwrap();
        let existing = Consumer {
            name: "existing".into(),
            protocol: crate::protocol::PROTOCOL_VERSION,
            capabilities: vec![Capability::Status],
            sink: None,
        };
        consumers.register(&existing).unwrap();
        let registering = Consumer {
            name: "registering".into(),
            ..existing
        };
        let hooks = hook_manager_for(&install);
        hooks.install(Agent::Claude).unwrap();
        let registration_guard = LockGuard::acquire(&state_root.join("consumers.lock")).unwrap();
        let current_binary_len = fs::metadata(std::env::current_exe().unwrap())
            .unwrap()
            .len();
        let root = install.data_root.clone();
        let registering_store = ConsumerStore::open(&state_root).unwrap();
        let installer = std::thread::spawn(move || {
            Installer::open(root)
                .unwrap()
                .install_current(&registering_store, &registering)
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match fs::metadata(install.binary_path()) {
                Ok(metadata) if metadata.len() == current_binary_len => break,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
                    ) => {}
                Err(e) => panic!("failed to inspect active binary: {e}"),
            }
            assert!(Instant::now() < deadline, "installation did not activate");
            std::thread::sleep(Duration::from_millis(5));
        }
        let lock = File::open(install.data_root.join("install.lock")).unwrap();
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            match lock.try_lock_exclusive() {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("failed to inspect installation lock: {e}"),
                Ok(()) => {
                    FileExt::unlock(&lock).unwrap();
                    panic!("installation lock released before consumer registration");
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        let root = install.data_root.clone();
        let removing_store = ConsumerStore::open(&state_root).unwrap();
        let uninstaller = std::thread::spawn(move || {
            let outcome = Installer::open(root).unwrap().uninstall_consumer(
                &removing_store,
                &hooks,
                "existing",
            );
            (outcome, hooks)
        });
        drop(registration_guard);
        assert_eq!(
            installer.join().unwrap().unwrap().active_version,
            env!("CARGO_PKG_VERSION")
        );
        let (outcome, hooks) = uninstaller.join().unwrap();
        assert!(!outcome.unwrap().was_last_consumer);
        assert!(install.binary_path().exists());
        assert_eq!(consumers.list().unwrap()[0].name, "registering");
        assert_eq!(
            hooks.status(Agent::Claude).unwrap().state,
            crate::hooks::HookState::Installed
        );
    }

    #[test]
    fn uninstall_rechecks_consumers_after_acquiring_the_installation_lock() {
        use std::sync::mpsc;
        use std::time::Duration;
        let install = installation_with_active("1.0.2", 1);
        let state_root = temp_root("waiting-uninstall");
        let consumers = ConsumerStore::open(state_root.clone()).unwrap();
        consumers
            .register(&Consumer {
                name: "one".into(),
                protocol: 1,
                capabilities: vec![Capability::Status],
                sink: None,
            })
            .unwrap();
        let hooks = hook_manager_for(&install);
        hooks.install(Agent::Claude).unwrap();
        let guard = install.lock().unwrap();
        let root = install.data_root.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let installer = Installer::open(root).unwrap();
            let store = ConsumerStore::open(state_root).unwrap();
            started_tx.send(()).unwrap();
            let result = installer.uninstall_consumer(&store, &hooks, "one");
            done_tx.send((result, hooks)).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let premature = done_rx.recv_timeout(Duration::from_millis(100));
        consumers
            .register(&Consumer {
                name: "two".into(),
                protocol: 1,
                capabilities: vec![Capability::Status],
                sink: None,
            })
            .unwrap();
        drop(guard);
        worker.join().unwrap();
        assert!(matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)));
        let (result, hooks) = done_rx.recv().unwrap();
        assert!(!result.unwrap().was_last_consumer);
        assert!(install.binary_path().exists());
        assert_eq!(
            hooks.status(Agent::Claude).unwrap().state,
            crate::hooks::HookState::Installed
        );
        assert_eq!(consumers.list().unwrap()[0].name, "two");
    }
}
