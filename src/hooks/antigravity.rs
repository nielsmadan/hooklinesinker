use super::{
    BinaryLocation, EventSpec, GroupOwnership, HookEntry, HookState, HookStatus, Map,
    ReconcileMode, Value, canonical_command, classify_command, empty_status, parse_json_root,
    read_optional_bytes, write_agent_config,
};
use crate::protocol::Agent;
use std::{io, path::Path};

const PROVISIONAL: &str =
    "provisional: CLI ownership and eventual background settlement are not verified";

fn owned(entry: &Value, binary: BinaryLocation<'_>, event: &str) -> bool {
    entry.get("type").is_none_or(|kind| kind == "command")
        && entry
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| {
                matches!(
                    classify_command(command, binary, Agent::Antigravity, event),
                    GroupOwnership::Ours { .. }
                )
            })
}

fn handlers(entry: &Value) -> &[Value] {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .map_or_else(|| std::slice::from_ref(entry), Vec::as_slice)
}

fn unsupported(root: &Map<String, Value>, binary: BinaryLocation<'_>) -> Option<&'static str> {
    let value = root.get("hooklinesinker")?;
    let Some(namespace) = value.as_object() else {
        return Some("hooklinesinker namespace is not an object");
    };
    let mut ours = false;
    let mut foreign = false;
    for (event, value) in namespace {
        if event == "enabled" {
            if !value.is_boolean() {
                return Some("namespace enabled must be boolean");
            }
            continue;
        }
        let Some(entries) = value.as_array() else {
            return Some("namespace events must contain arrays");
        };
        for entry in entries {
            if !entry.is_object() || entry.get("hooks").is_some_and(|hooks| !hooks.is_array()) {
                return Some("invalid Antigravity handler structure");
            }
            for handler in handlers(entry) {
                if !handler.is_object() || handler.get("hooks").is_some() {
                    return Some("invalid Antigravity command handler");
                }
                if owned(handler, binary, event) {
                    ours = true;
                } else {
                    foreign = true;
                }
            }
        }
    }
    if !ours && (!namespace.is_empty()) {
        return Some("hooklinesinker namespace belongs to another definition");
    }
    if foreign && namespace.get("enabled") == Some(&Value::Bool(false)) {
        return Some("disabled namespace contains foreign handlers");
    }
    None
}

fn entry(spec: &EventSpec, command: &str) -> Value {
    let handler =
        serde_json::json!({"type":"command", "command":command, "timeout":spec.timeout.as_secs()});
    spec.matcher.map_or_else(
        || handler.clone(),
        |matcher| serde_json::json!({"matcher":matcher,"hooks":[handler]}),
    )
}

fn installed(items: &[Value], spec: &EventSpec, binary: BinaryLocation<'_>) -> bool {
    let ours: Vec<_> = items
        .iter()
        .flat_map(|item| handlers(item).iter().map(move |handler| (item, handler)))
        .filter(|(_, handler)| owned(handler, binary, spec.name))
        .collect();
    match ours.as_slice() {
        [(item, handler)] => {
            let command = handler["command"].as_str().expect("recognized command");
            matches!(
                classify_command(command, binary, Agent::Antigravity, spec.name),
                GroupOwnership::Ours { exact: true }
            ) && **item == entry(spec, command)
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
        return Ok(empty_status(
            Agent::Antigravity,
            HookState::Missing,
            path,
            Some(PROVISIONAL),
        ));
    };
    let root = parse_json_root(path, &bytes)?;
    if let Some(reason) = unsupported(&root, binary) {
        return Ok(empty_status(
            Agent::Antigravity,
            HookState::Unsupported,
            path,
            Some(reason),
        ));
    }
    let namespace = root.get("hooklinesinker").and_then(Value::as_object);
    let mut entries = Vec::new();
    let mut complete = namespace.is_some_and(|map| map.get("enabled") != Some(&Value::Bool(false)));
    for spec in events {
        let items = namespace
            .and_then(|map| map.get(spec.name))
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        complete &= installed(items, spec, binary);
        for (index, item) in items.iter().enumerate() {
            entries.extend(
                handlers(item)
                    .iter()
                    .filter(|handler| owned(handler, binary, spec.name))
                    .map(|handler| HookEntry {
                        event: spec.name.into(),
                        group_index: index,
                        command: handler["command"].as_str().unwrap().into(),
                    }),
            );
        }
    }
    Ok(HookStatus {
        agent: Agent::Antigravity,
        state: if complete {
            HookState::Installed
        } else if entries.is_empty() {
            HookState::Missing
        } else {
            HookState::Drifted
        },
        path: path.into(),
        entries,
        reason: Some(PROVISIONAL),
    })
}

fn remove_owned(items: &[Value], binary: BinaryLocation<'_>, event: &str) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| {
            if let Some(handlers) = item.get("hooks").and_then(Value::as_array) {
                let kept: Vec<_> = handlers
                    .iter()
                    .filter(|handler| !owned(handler, binary, event))
                    .cloned()
                    .collect();
                if kept.len() == handlers.len() {
                    return Some(item.clone());
                }
                if kept.is_empty() {
                    return None;
                }
                let mut group = item.clone();
                group["hooks"] = Value::Array(kept);
                Some(group)
            } else {
                (!owned(item, binary, event)).then(|| item.clone())
            }
        })
        .collect()
}

pub(super) fn reconcile(
    binary: BinaryLocation<'_>,
    path: &Path,
    events: &[EventSpec],
    mode: ReconcileMode,
) -> io::Result<HookStatus> {
    let original = read_optional_bytes(path)?;
    let mut root = original
        .as_deref()
        .map(|bytes| parse_json_root(path, bytes))
        .transpose()?
        .unwrap_or_default();
    if let Some(reason) = unsupported(&root, binary) {
        return Ok(empty_status(
            Agent::Antigravity,
            HookState::Unsupported,
            path,
            Some(reason),
        ));
    }
    if !root.contains_key("hooklinesinker") && matches!(mode, ReconcileMode::Uninstall) {
        return status(binary, path, events);
    }
    let before = root.clone();
    let namespace = root
        .entry("hooklinesinker")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("validated namespace");
    if matches!(mode, ReconcileMode::Install)
        && namespace.get("enabled") == Some(&Value::Bool(false))
    {
        namespace.insert("enabled".into(), Value::Bool(true));
    }
    for spec in events {
        let items = namespace
            .get(spec.name)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if matches!(mode, ReconcileMode::Install) && installed(&items, spec, binary) {
            continue;
        }
        let mut kept = remove_owned(&items, binary, spec.name);
        if matches!(mode, ReconcileMode::Install) {
            kept.push(entry(
                spec,
                &canonical_command(binary.path, Agent::Antigravity, spec.name),
            ));
        }
        if kept != items {
            if kept.is_empty() {
                namespace.remove(spec.name);
            } else {
                namespace.insert(spec.name.into(), Value::Array(kept));
            }
        }
    }
    if namespace.keys().all(|key| key == "enabled") {
        root.remove("hooklinesinker");
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
    fn named_map_preserves_foreign_names_and_mixed_handlers() {
        let (manager, root) = manager();
        let path = config_path(&manager, Agent::Antigravity);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"foreign":{"enabled":false,"Stop":[{"command":"echo keep"}]}}"#,
        )
        .unwrap();
        let installed = manager.install(Agent::Antigravity).unwrap();
        assert_eq!(installed.state, HookState::Installed);
        assert!(installed.reason.unwrap().starts_with("provisional:"));
        let bytes = fs::read(&path).unwrap();
        manager.install(Agent::Antigravity).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["hooklinesinker"]["Stop"][0]["timeout"], 5);
        assert_eq!(value["hooklinesinker"]["PostToolUse"][0]["matcher"], ".*");
        let foreign = serde_json::json!({"command":"echo foreign", "timeout":9});
        value["hooklinesinker"]["PostToolUse"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(foreign.clone());
        value["hooklinesinker"]["PostToolUse"][0]["custom"] = Value::Bool(true);
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            manager.status(Agent::Antigravity).unwrap().state,
            HookState::Drifted
        );
        assert_eq!(
            manager.install(Agent::Antigravity).unwrap().state,
            HookState::Installed
        );
        manager.uninstall(Agent::Antigravity).unwrap();
        let remaining: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(remaining["foreign"], value["foreign"]);
        assert_eq!(
            remaining["hooklinesinker"]["PostToolUse"],
            serde_json::json!([{"matcher":".*","custom":true,"hooks":[foreign]}])
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn disabled_owned_namespace_can_be_enabled_but_collisions_are_unchanged() {
        let (manager, root) = manager();
        let path = config_path(&manager, Agent::Antigravity);
        manager.install(Agent::Antigravity).unwrap();
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["hooklinesinker"]["enabled"] = Value::Bool(false);
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            manager.status(Agent::Antigravity).unwrap().state,
            HookState::Drifted
        );
        assert_eq!(
            manager.install(Agent::Antigravity).unwrap().state,
            HookState::Installed
        );
        value["hooklinesinker"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"command":"echo foreign"}));
        let cases = [
            value,
            serde_json::json!({"hooklinesinker":{"Stop":[{"command":"echo foreign"}]}}),
            serde_json::json!({"hooklinesinker":{"Stop":false}}),
            serde_json::json!({"hooklinesinker":{"enabled":"yes"}}),
        ];
        for case in cases {
            let bytes = case.to_string();
            fs::write(&path, &bytes).unwrap();
            assert_eq!(
                manager.status(Agent::Antigravity).unwrap().state,
                HookState::Unsupported
            );
            assert_eq!(
                manager.install(Agent::Antigravity).unwrap().state,
                HookState::Unsupported
            );
            assert_eq!(
                manager.uninstall(Agent::Antigravity).unwrap().state,
                HookState::Unsupported
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_drift_and_home_relative_commands_reconcile() {
        use std::os::unix::fs::PermissionsExt;
        let (mut manager, root) = manager();
        manager.roots.home = root.clone();
        let path = config_path(&manager, Agent::Antigravity);
        manager.install(Agent::Antigravity).unwrap();
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for spec in crate::events::ANTIGRAVITY {
            value["hooklinesinker"][spec.name] = serde_json::json!([entry(
                spec,
                &format!(
                    "$HOME/bin/hooklinesinker ingest --agent antigravity --event {}",
                    spec.name
                )
            )]);
        }
        let bytes = value.to_string();
        fs::write(&path, &bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            manager.install(Agent::Antigravity).unwrap().state,
            HookState::Installed
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), bytes);
        let duplicate = value["hooklinesinker"]["Stop"][0].clone();
        value["hooklinesinker"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        value["hooklinesinker"]["PreInvocation"][0]["timeout"] = Value::from(20);
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            manager.status(Agent::Antigravity).unwrap().state,
            HookState::Drifted
        );
        assert_eq!(
            manager.install(Agent::Antigravity).unwrap().state,
            HookState::Installed
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            manager.uninstall(Agent::Antigravity).unwrap().state,
            HookState::Missing
        );
        fs::remove_dir_all(root).unwrap();
    }
}
