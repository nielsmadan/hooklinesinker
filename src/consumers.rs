use crate::state::{LockGuard, write_private_atomic};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

pub const SUPPORTED_PROTOCOL_MAJOR: u16 = crate::protocol::PROTOCOL_VERSION;
pub const SUPPORTED_CAPABILITIES: &[&str] = &["status"];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Consumer {
    pub name: String,
    pub protocol: u16,
    pub capabilities: Vec<String>,
    pub sink: Option<String>,
}

pub struct ConsumerStore {
    consumers_dir: PathBuf,
    lock_path: PathBuf,
}

impl ConsumerStore {
    pub fn open(root: PathBuf) -> io::Result<Self> {
        let consumers_dir = root.join("consumers");
        crate::paths::ensure_private_dir(&root)?;
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

    pub fn register(&self, consumer: Consumer) -> io::Result<()> {
        validate_name(&consumer.name)?;
        validate_protocol(consumer.protocol)?;
        validate_capabilities(&consumer.capabilities)?;
        validate_sink(consumer.sink.as_deref())?;

        let _guard = self.lock()?;
        let path = self.consumer_path(&consumer.name);
        let bytes = serde_json::to_vec_pretty(&consumer)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private_atomic(&path, &bytes)
    }

    pub fn remove(&self, name: &str) -> io::Result<()> {
        let _guard = self.lock()?;
        match fs::remove_file(self.consumer_path(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn list(&self) -> io::Result<Vec<Consumer>> {
        let _guard = self.lock()?;
        Ok(self.read_all_locked()?.0)
    }

    pub fn requested_capabilities(&self, capability: &str) -> io::Result<Vec<Consumer>> {
        let _guard = self.lock()?;
        Ok(self
            .read_all_locked()?
            .0
            .into_iter()
            .filter(|c| c.capabilities.iter().any(|cap| cap == capability))
            .collect())
    }

    pub fn parse_problems(&self) -> io::Result<Vec<String>> {
        let _guard = self.lock()?;
        Ok(self.read_all_locked()?.1)
    }

    fn read_all_locked(&self) -> io::Result<(Vec<Consumer>, Vec<String>)> {
        let mut out = Vec::new();
        let mut problems = Vec::new();
        let entries = match fs::read_dir(&self.consumers_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((out, problems)),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            match serde_json::from_slice::<Consumer>(&bytes) {
                Ok(consumer) => out.push(consumer),
                Err(_) => {
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("<unknown>");
                    problems.push(format!("failed to parse consumer record {name}"));
                }
            }
        }
        Ok((out, problems))
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

fn validate_capabilities(capabilities: &[String]) -> io::Result<()> {
    if capabilities.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "consumer must request at least one capability",
        ));
    }
    if capabilities
        .iter()
        .all(|c| SUPPORTED_CAPABILITIES.contains(&c.as_str()))
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported capability in {capabilities:?}"),
        ))
    }
}

fn validate_sink(sink: Option<&str>) -> io::Result<()> {
    let Some(sink) = sink else {
        return Ok(());
    };
    match sink.split_once("://") {
        Some((scheme, _))
            if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") =>
        {
            Ok(())
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported sink scheme: {sink}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "hooklinesinker-consumers-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn store() -> ConsumerStore {
        ConsumerStore::open(temp_root()).unwrap()
    }

    fn consumer(name: &str, capabilities: &[&str], sink: Option<&str>) -> Consumer {
        Consumer {
            name: name.to_string(),
            protocol: SUPPORTED_PROTOCOL_MAJOR,
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            sink: sink.map(str::to_string),
        }
    }

    #[test]
    fn invalid_names_are_rejected() {
        let store = store();
        assert!(
            store
                .register(consumer("Juggler", &["status"], None))
                .is_err()
        );
        assert!(store.register(consumer("", &["status"], None)).is_err());
        assert!(
            store
                .register(consumer("-juggler", &["status"], None))
                .is_err()
        );
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn invalid_sink_scheme_is_rejected() {
        let store = store();
        assert!(
            store
                .register(consumer("juggler", &["status"], Some("ftp://example.com")))
                .is_err()
        );
    }

    #[test]
    fn unsupported_protocol_major_is_rejected() {
        let store = store();
        let mut future = consumer("juggler", &["status"], None);
        future.protocol = 2;
        assert!(store.register(future).is_err());
    }

    #[test]
    fn requested_capabilities_filters_by_capability() {
        let store = store();
        store
            .register(consumer(
                "juggler",
                &["status"],
                Some("http://127.0.0.1/hook"),
            ))
            .unwrap();
        let matches = store.requested_capabilities("status").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "juggler");
        assert!(store.requested_capabilities("raw").unwrap().is_empty());
    }

    #[test]
    fn remove_is_idempotent_for_a_missing_consumer() {
        let store = store();
        assert!(store.remove("never-registered").is_ok());
    }
}
