use crate::persistence::{LockGuard, write_private_atomic};
pub(crate) use crate::protocol::{Capability, Consumer};
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

pub(crate) const SUPPORTED_PROTOCOL_MAJOR: u16 = crate::protocol::PROTOCOL_VERSION;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoveOutcome {
    Removed,
    Missing,
}

impl Consumer {
    pub fn new(
        name: impl Into<String>,
        capabilities: Vec<Capability>,
        sink: Option<String>,
    ) -> io::Result<Self> {
        let consumer = Self {
            name: name.into(),
            protocol: SUPPORTED_PROTOCOL_MAJOR,
            capabilities,
            sink,
        };
        consumer.validate()?;
        Ok(consumer)
    }

    pub fn validate(&self) -> io::Result<()> {
        validate_name(&self.name)?;
        validate_protocol(self.protocol)?;
        validate_capabilities(&self.capabilities)?;
        validate_sink(self.sink.as_deref())
    }
}

pub(crate) struct ConsumerSnapshot {
    pub consumers: Vec<Consumer>,
    pub problems: Vec<String>,
}

pub(crate) struct ConsumerStore {
    consumers_dir: PathBuf,
    lock_path: PathBuf,
}

impl ConsumerStore {
    pub(crate) fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let consumers_dir = root.join("consumers");
        crate::paths::ensure_private_dir(root)?;
        crate::paths::ensure_private_dir(&consumers_dir)?;
        Ok(Self {
            lock_path: root.join("consumers.lock"),
            consumers_dir,
        })
    }

    fn lock(&self) -> io::Result<LockGuard> {
        LockGuard::acquire(&self.lock_path)
    }

    fn consumer_path(&self, name: &str) -> PathBuf {
        self.consumers_dir.join(format!("{name}.json"))
    }

    pub(crate) fn register(&self, consumer: &Consumer) -> io::Result<()> {
        consumer.validate()?;

        let _guard = self.lock()?;
        let path = self.consumer_path(&consumer.name);
        let bytes = serde_json::to_vec_pretty(consumer)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private_atomic(&path, &bytes)
    }

    pub(crate) fn remove(&self, name: &str) -> io::Result<RemoveOutcome> {
        validate_name(name)?;
        let _guard = self.lock()?;
        match fs::remove_file(self.consumer_path(name)) {
            Ok(()) => Ok(RemoveOutcome::Removed),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(RemoveOutcome::Missing),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn snapshot(&self) -> io::Result<ConsumerSnapshot> {
        let _guard = self.lock()?;
        self.read_all_locked()
    }

    pub(crate) fn list(&self) -> io::Result<Vec<Consumer>> {
        let snapshot = self.snapshot()?;
        if !snapshot.problems.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                snapshot.problems.join("; "),
            ));
        }
        Ok(snapshot.consumers)
    }

    #[cfg(test)]
    pub(crate) fn requested_capabilities(
        &self,
        capability: Capability,
    ) -> io::Result<Vec<Consumer>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|consumer| consumer.capabilities.contains(&capability))
            .collect())
    }

    pub(crate) fn parse_problems(&self) -> io::Result<Vec<String>> {
        Ok(self.snapshot()?.problems)
    }

    fn read_all_locked(&self) -> io::Result<ConsumerSnapshot> {
        let mut out = Vec::new();
        let mut problems = Vec::new();
        let entries = match fs::read_dir(&self.consumers_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(ConsumerSnapshot {
                    consumers: out,
                    problems,
                });
            }
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    problems.push(format!("failed to read consumer directory entry: {e}"));
                    continue;
                }
            };
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    problems.push(format!(
                        "failed to read consumer record {}: {e}",
                        path.display()
                    ));
                    continue;
                }
            };
            match serde_json::from_slice::<Consumer>(&bytes) {
                Ok(consumer) => match consumer.validate() {
                    Ok(()) => out.push(consumer),
                    Err(e) => {
                        problems.push(format!("invalid consumer record {}: {e}", path.display()));
                    }
                },
                Err(e) => {
                    problems.push(format!(
                        "failed to parse consumer record {}: {e}",
                        path.display()
                    ));
                }
            }
        }
        Ok(ConsumerSnapshot {
            consumers: out,
            problems,
        })
    }
}

fn validate_name(name: &str) -> io::Result<()> {
    let mut chars = name.chars();
    let starts_ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if starts_ok && rest_ok {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid consumer name: {name}"),
        ))
    }
}

fn validate_protocol(protocol: u16) -> io::Result<()> {
    if protocol == SUPPORTED_PROTOCOL_MAJOR {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported protocol major: {protocol}"),
        ))
    }
}

fn validate_capabilities(capabilities: &[Capability]) -> io::Result<()> {
    if capabilities.is_empty() {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "consumer must request at least one capability",
        ))
    } else {
        Ok(())
    }
}

fn validate_sink(sink: Option<&str>) -> io::Result<()> {
    let Some(sink) = sink else {
        return Ok(());
    };
    let uri = sink.parse::<ureq::http::Uri>().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid sink URL {sink}: {e}"),
        )
    })?;
    match (uri.scheme_str(), uri.authority(), uri.host()) {
        (Some(scheme), Some(_), Some(_)) if scheme.eq_ignore_ascii_case("https") => Ok(()),
        (Some(scheme), Some(_), Some(host))
            if scheme.eq_ignore_ascii_case("http") && is_loopback_host(host) =>
        {
            Ok(())
        }
        (Some(scheme), Some(_), Some(_)) if scheme.eq_ignore_ascii_case("http") => {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("remote sink must use HTTPS; HTTP is allowed only for loopback: {sink}"),
            ))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("sink must be an absolute HTTP(S) URL: {sink}"),
        )),
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host)
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_TEMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_root() -> PathBuf {
        // The counter keeps parallel tests from sharing a store within one clock tick.
        let id = NEXT_TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "hooklinesinker-consumers-test-{}-{nanos}-{id}",
            std::process::id()
        ))
    }

    fn store() -> ConsumerStore {
        ConsumerStore::open(temp_root()).unwrap()
    }

    fn consumer(name: &str, capabilities: &[&str], sink: Option<&str>) -> Consumer {
        Consumer {
            name: name.to_string(),
            protocol: SUPPORTED_PROTOCOL_MAJOR,
            capabilities: capabilities
                .iter()
                .map(|capability| match *capability {
                    "status" => Capability::Status,
                    other => panic!("unsupported test capability {other}"),
                })
                .collect(),
            sink: sink.map(str::to_string),
        }
    }

    #[test]
    fn invalid_names_are_rejected() {
        let store = store();
        assert!(
            store
                .register(&consumer("Juggler", &["status"], None))
                .is_err()
        );
        assert!(store.register(&consumer("", &["status"], None)).is_err());
        assert!(
            store
                .register(&consumer("-juggler", &["status"], None))
                .is_err()
        );
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn invalid_sink_scheme_is_rejected() {
        let store = store();
        for sink in [
            "ftp://example.com",
            "http://",
            "https:///hook",
            "http://example.com/hook",
            "http://192.0.2.1/hook",
        ] {
            assert!(
                store
                    .register(&consumer("juggler", &["status"], Some(sink)))
                    .is_err(),
                "accepted {sink}"
            );
        }
    }

    #[test]
    fn loopback_http_and_remote_https_sinks_are_accepted() {
        let store = store();
        for (name, sink) in [
            ("ipv4", "http://127.0.0.1:7483/hook"),
            ("ipv6", "http://[::1]:7483/hook"),
            ("localhost", "http://localhost:7483/hook"),
            ("remote", "https://example.com/hook"),
        ] {
            store
                .register(&consumer(name, &["status"], Some(sink)))
                .unwrap();
        }
    }

    #[test]
    fn unsupported_protocol_major_is_rejected() {
        let store = store();
        let mut future = consumer("juggler", &["status"], None);
        future.protocol = 2;
        assert!(store.register(&future).is_err());
    }

    #[test]
    fn requested_capabilities_filters_by_capability() {
        let store = store();
        store
            .register(&consumer(
                "juggler",
                &["status"],
                Some("http://127.0.0.1/hook"),
            ))
            .unwrap();
        let matches = store.requested_capabilities(Capability::Status).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "juggler");
    }

    #[test]
    fn remove_is_idempotent_for_a_missing_consumer() {
        let store = store();
        assert!(store.remove("never-registered").is_ok());
    }
    #[test]
    fn remove_rejects_paths_and_preserves_their_targets() {
        let root = temp_root();
        let store = ConsumerStore::open(root.clone()).unwrap();
        let unrelated = root.join("unrelated.json");
        fs::write(&unrelated, "keep me").unwrap();
        for name in [
            "../unrelated".to_string(),
            root.join("unrelated").display().to_string(),
        ] {
            assert_eq!(
                store.remove(&name).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(fs::read_to_string(&unrelated).unwrap(), "keep me");
        }
    }

    #[test]
    fn snapshot_retains_valid_consumers_and_reports_failed_records() {
        let root = temp_root();
        let store = ConsumerStore::open(root.clone()).unwrap();
        store
            .register(&consumer("valid", &["status"], None))
            .unwrap();
        fs::write(root.join("consumers/broken.json"), "{").unwrap();
        fs::write(
            root.join("consumers/unsupported.json"),
            r#"{"name":"unsupported","protocol":2,"capabilities":["status"],"sink":null}"#,
        )
        .unwrap();
        fs::create_dir(root.join("consumers/unreadable.json")).unwrap();
        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot.consumers.len(), 1);
        assert_eq!(snapshot.consumers[0].name, "valid");
        assert_eq!(snapshot.problems.len(), 3);
        assert!(snapshot.problems.iter().any(|p| p.contains("broken.json")));
        assert!(
            snapshot
                .problems
                .iter()
                .any(|p| p.contains("unsupported.json"))
        );
        assert!(
            snapshot
                .problems
                .iter()
                .any(|p| p.contains("unreadable.json"))
        );
        assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
