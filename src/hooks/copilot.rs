use super::{
    BinaryLocation, EventSpec, GroupOwnership, HookEntry, HookState, HookStatus, Map,
    ReconcileMode, Value, canonical_command, classify_command, empty_status, parse_json_root,
    read_optional_bytes, write_agent_config,
};
use crate::protocol::Agent;
use std::{io, path::Path};

fn owned_command<'a>(entry: &'a Value, binary: BinaryLocation<'_>, event: &str) -> Option<&'a str> {
    if entry
        .get("type")
        .is_some_and(|kind| kind.as_str() != Some("command"))
    {
        return None;
    }
    entry
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| {
            matches!(
                classify_command(command, binary, Agent::Copilot, event),
                GroupOwnership::Ours { .. }
            )
        })
}

fn unsupported(root: &Map<String, Value>, binary: BinaryLocation<'_>) -> Option<&'static str> {
    if root.get("version") != Some(&Value::from(1)) {
        return Some("Copilot hook files require version 1");
    }
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return Some("the hooks key is not an object");
    };
    for (event, entries) in hooks {
        let Some(entries) = entries.as_array() else {
            return Some("a Copilot event's value is not an array");
        };
        for entry in entries {
            if !entry.is_object() || entry.get("hooks").is_some() {
                return Some("Copilot hooks require direct handler objects");
            }
            if owned_command(entry, binary, event).is_some()
                && ["bash", "powershell", "exec", "args", "env", "cwd"]
                    .iter()
                    .any(|key| entry.get(key).is_some())
            {
                return Some(
                    "a recognized command has execution overrides; ownership is ambiguous",
                );
            }
        }
    }
    None
}

fn handler(spec: &EventSpec, command: &str) -> Value {
    serde_json::json!({"type": "command", "command": command, "timeoutSec": spec.timeout.as_secs()})
}

fn installed(entries: &[Value], spec: &EventSpec, binary: BinaryLocation<'_>) -> bool {
    let owned: Vec<_> = entries
        .iter()
        .filter_map(|entry| owned_command(entry, binary, spec.name).map(|command| (entry, command)))
        .collect();
    match owned.as_slice() {
        [(entry, command)] => {
            matches!(
                classify_command(command, binary, Agent::Copilot, spec.name),
                GroupOwnership::Ours { exact: true }
            ) && **entry == handler(spec, command)
        }
        _ => false,
    }
}

pub(super) fn status(
    binary: BinaryLocation<'_>,
    path: &Path,
    events: &[EventSpec],
) -> io::Result<HookStatus> {
    let Some(bytes) = read_optional_bytes(path)? else {
        return Ok(empty_status(Agent::Copilot, HookState::Missing, path, None));
    };
    let root = parse_json_root(path, &bytes)?;
    if let Some(reason) = unsupported(&root, binary) {
        return Ok(empty_status(
            Agent::Copilot,
            HookState::Unsupported,
            path,
            Some(reason),
        ));
    }
    let hooks = root["hooks"].as_object().expect("validated hook object");
    let items_for = |spec: &EventSpec| {
        hooks
            .get(spec.name)
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
    };
    let complete = events
        .iter()
        .all(|spec| installed(items_for(spec), spec, binary));
    let entries: Vec<_> = events
        .iter()
        .flat_map(|spec| {
            items_for(spec)
                .iter()
                .enumerate()
                .filter_map(move |(index, entry)| {
                    owned_command(entry, binary, spec.name).map(|command| HookEntry {
                        event: spec.name.into(),
                        group_index: index,
                        command: command.into(),
                    })
                })
        })
        .collect();
    Ok(HookStatus {
        agent: Agent::Copilot,
        state: if complete {
            HookState::Installed
        } else if entries.is_empty() {
            HookState::Missing
        } else {
            HookState::Drifted
        },
        path: path.into(),
        entries,
        reason: None,
    })
}

pub(super) fn reconcile(
    binary: BinaryLocation<'_>,
    path: &Path,
    events: &[EventSpec],
    mode: ReconcileMode,
) -> io::Result<HookStatus> {
    let original = read_optional_bytes(path)?;
    if original.is_none() && matches!(mode, ReconcileMode::Uninstall) {
        return Ok(empty_status(Agent::Copilot, HookState::Missing, path, None));
    }
    let mut root = original
        .as_deref()
        .map(|bytes| parse_json_root(path, bytes))
        .transpose()?
        .unwrap_or_else(|| {
            serde_json::json!({"version": 1, "hooks": {}})
                .as_object()
                .unwrap()
                .clone()
        });
    if let Some(reason) = unsupported(&root, binary) {
        return Ok(empty_status(
            Agent::Copilot,
            HookState::Unsupported,
            path,
            Some(reason),
        ));
    }
    let before = root.clone();
    let hooks = root
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .expect("validated hook object");
    for spec in events {
        if matches!(mode, ReconcileMode::Uninstall) && !hooks.contains_key(spec.name) {
            continue;
        }
        let items = hooks
            .entry(spec.name)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("validated hook array");
        if matches!(mode, ReconcileMode::Install) && installed(items, spec, binary) {
            continue;
        }
        let original_len = items.len();
        items.retain(|entry| owned_command(entry, binary, spec.name).is_none());
        if matches!(mode, ReconcileMode::Install) {
            items.push(handler(
                spec,
                &canonical_command(binary.path, Agent::Copilot, spec.name),
            ));
        } else if original_len > 0 && items.is_empty() {
            hooks.remove(spec.name);
        }
    }
    if root != before {
        let mut bytes = serde_json::to_vec_pretty(&root).map_err(io::Error::other)?;
        bytes.push(b'\n');
        write_agent_config(path, original.as_deref(), &bytes)?;
    }
    status(binary, path, events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::tests::{config_path, manager};
    use std::fs;

    #[test]
    fn direct_hooks_preserve_foreign_entries_and_reconcile_duplicates() {
        let (manager, root) = manager();
        let path = config_path(&manager, Agent::Copilot);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let foreign = serde_json::json!({"type":"command","bash":"echo foreign","timeoutSec":9});
        fs::write(&path, serde_json::json!({"version":1,"custom":true,"hooks":{"SessionStart":[foreign],"customEvent":[]}}).to_string()).unwrap();
        assert_eq!(
            manager.install(Agent::Copilot).unwrap().state,
            HookState::Installed
        );
        let bytes = fs::read(&path).unwrap();
        manager.install(Agent::Copilot).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["hooks"]["SessionStart"][0], foreign);
        assert_eq!(value["hooks"]["PreToolUse"][0]["timeoutSec"], 5);
        assert_eq!(
            value["hooks"]["notification"][0].as_object().unwrap().len(),
            3
        );
        let owned = value["hooks"]["SessionStart"][1].clone();
        value["hooks"]["SessionStart"]
            .as_array_mut()
            .unwrap()
            .push(owned);
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            manager.status(Agent::Copilot).unwrap().state,
            HookState::Drifted
        );
        assert_eq!(
            manager.install(Agent::Copilot).unwrap().state,
            HookState::Installed
        );
        assert_eq!(
            manager.uninstall(Agent::Copilot).unwrap().state,
            HookState::Missing
        );
        let remaining: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            remaining,
            serde_json::json!({"version":1,"custom":true,"hooks":{"SessionStart":[foreign],"customEvent":[]}})
        );
        let remaining_bytes = fs::read(&path).unwrap();
        manager.uninstall(Agent::Copilot).unwrap();
        assert_eq!(fs::read(&path).unwrap(), remaining_bytes);
        fs::write(&path, r#"{"version":1,"hooks":{"Stop":[]}}"#).unwrap();
        manager.uninstall(Agent::Copilot).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            r#"{"version":1,"hooks":{"Stop":[]}}"#
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ambiguous_overrides_and_structural_errors_are_unchanged() {
        let (manager, root) = manager();
        let path = config_path(&manager, Agent::Copilot);
        manager.install(Agent::Copilot).unwrap();
        let original: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut cases = Vec::new();
        for key in ["bash", "powershell", "exec", "args", "env", "cwd"] {
            let mut value = original.clone();
            value["hooks"]["SessionStart"][0][key] = Value::String("foreign".into());
            cases.push(value);
        }
        cases.extend([
            serde_json::json!({"version":2,"hooks":{}}),
            serde_json::json!({"hooks":{}}),
            serde_json::json!({"version":1,"hooks":[]}),
            serde_json::json!({"version":1,"hooks":{"Stop":{}}}),
            serde_json::json!({"version":1,"hooks":{"Stop":[{"hooks":[]}]}}),
        ]);
        for value in cases {
            let bytes = value.to_string();
            fs::write(&path, &bytes).unwrap();
            assert_eq!(
                manager.status(Agent::Copilot).unwrap().state,
                HookState::Unsupported
            );
            assert_eq!(
                manager.install(Agent::Copilot).unwrap().state,
                HookState::Unsupported
            );
            assert_eq!(
                manager.uninstall(Agent::Copilot).unwrap().state,
                HookState::Unsupported
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn owned_entry_shape_and_binary_path_drift_are_repaired() {
        let (manager, root) = manager();
        let path = config_path(&manager, Agent::Copilot);
        manager.install(Agent::Copilot).unwrap();
        let original: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for (key, value) in [
            ("timeoutSec", Value::from(9)),
            ("matcher", Value::from("*")),
            ("timeout", Value::from(9)),
            (
                "command",
                Value::from("/old/hooklinesinker ingest --agent copilot --event Stop"),
            ),
        ] {
            let mut changed = original.clone();
            changed["hooks"]["Stop"][0][key] = value;
            fs::write(&path, changed.to_string()).unwrap();
            assert_eq!(
                manager.status(Agent::Copilot).unwrap().state,
                HookState::Drifted
            );
            assert_eq!(
                manager.install(Agent::Copilot).unwrap().state,
                HookState::Installed
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn home_relative_commands_preserve_bytes_and_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (mut manager, root) = manager();
        manager.roots.home = root.clone();
        let path = config_path(&manager, Agent::Copilot);
        manager.install(Agent::Copilot).unwrap();
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for (event, entries) in value["hooks"].as_object_mut().unwrap() {
            entries[0]["command"] = Value::from(format!(
                "$HOME/bin/hooklinesinker ingest --agent copilot --event {event}"
            ));
        }
        let text = value.to_string();
        fs::write(&path, &text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            manager.status(Agent::Copilot).unwrap().state,
            HookState::Installed
        );
        manager.install(Agent::Copilot).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        value["hooks"]["Stop"][0]["timeoutSec"] = Value::from(9);
        fs::write(&path, value.to_string()).unwrap();
        manager.install(Agent::Copilot).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn foreign_handler_types_with_a_command_field_are_preserved() {
        let (manager, root) = manager();
        let path = config_path(&manager, Agent::Copilot);
        manager.install(Agent::Copilot).unwrap();
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut foreign = value["hooks"]["Stop"][0].clone();
        foreign["type"] = Value::from("http");
        foreign["url"] = Value::from("https://example.invalid/hook");
        value["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(foreign.clone());
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            manager.install(Agent::Copilot).unwrap().state,
            HookState::Installed
        );
        manager.uninstall(Agent::Copilot).unwrap();
        let remaining: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(remaining["hooks"]["Stop"], serde_json::json!([foreign]));
        fs::remove_dir_all(root).unwrap();
    }
}
