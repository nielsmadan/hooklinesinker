use crate::protocol::Agent;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MARKER_PREFIX: &str = "// hooklinesinker-generated protocol=";
const BIN_PLACEHOLDER: &str = "__HOOKLINESINKER_BIN__";

const OPENCODE_TEMPLATE: &str = include_str!("../assets/opencode-hooklinesinker.ts");
const PI_TEMPLATE: &str = include_str!("../assets/pi-hooklinesinker.ts");

struct EventSpec {
    name: &'static str,
    matcher: Option<&'static str>,
    timeout: u64,
}

const CLAUDE_EVENTS: &[EventSpec] = &[
    EventSpec {
        name: "SessionStart",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "SessionEnd",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "UserPromptSubmit",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "PreToolUse",
        matcher: Some("*"),
        timeout: 5,
    },
    EventSpec {
        name: "PostToolUse",
        matcher: Some("*"),
        timeout: 5,
    },
    EventSpec {
        name: "PostToolUseFailure",
        matcher: Some("*"),
        timeout: 5,
    },
    EventSpec {
        name: "PermissionRequest",
        matcher: Some("*"),
        timeout: 5,
    },
    EventSpec {
        name: "SubagentStart",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "Stop",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "StopFailure",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "PreCompact",
        matcher: Some("*"),
        timeout: 5,
    },
];

const CODEX_EVENTS: &[EventSpec] = &[
    EventSpec {
        name: "SessionStart",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "UserPromptSubmit",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "PreToolUse",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "PostToolUse",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "PreCompact",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "PostCompact",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "PermissionRequest",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "Stop",
        matcher: None,
        timeout: 5,
    },
    EventSpec {
        name: "SessionEnd",
        matcher: None,
        timeout: 3,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookState {
    Missing,
    Installed,
    Drifted,
    Unsupported,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookEntry {
    pub event: String,
    pub group_index: usize,
    pub command: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookStatus {
    pub agent: Agent,
    pub state: HookState,
    pub path: PathBuf,
    pub entries: Vec<HookEntry>,
}

#[derive(Clone, Debug)]
pub struct HookRoots {
    pub claude_dir: PathBuf,
    pub codex_dir: PathBuf,
    pub opencode_config_dir: PathBuf,
    pub pi_agent_dir: PathBuf,
    pub binary_path: PathBuf,
}

impl HookRoots {
    pub fn from_env(binary_path: PathBuf) -> Self {
        let home = crate::paths::home_dir().unwrap_or_else(|| ".".to_string());
        let claude_dir = PathBuf::from(&home).join(".claude");
        let codex_dir = PathBuf::from(&home).join(".codex");
        let opencode_config_dir = nonempty_env("OPENCODE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                nonempty_env("XDG_CONFIG_HOME")
                    .map(|dir| PathBuf::from(dir).join("opencode"))
                    .unwrap_or_else(|| PathBuf::from(&home).join(".config/opencode"))
            });
        let pi_agent_dir = nonempty_env("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(".pi/agent"));
        Self {
            claude_dir,
            codex_dir,
            opencode_config_dir,
            pi_agent_dir,
            binary_path,
        }
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

pub struct HookManager {
    roots: HookRoots,
}

impl HookManager {
    pub fn new(roots: HookRoots) -> Self {
        Self { roots }
    }

    pub fn install(&self, agent: Agent) -> io::Result<HookStatus> {
        match agent {
            Agent::Claude => self.reconcile_json(
                agent,
                &self.claude_settings_path(),
                CLAUDE_EVENTS,
                ReconcileMode::Install,
            ),
            Agent::Codex => self.reconcile_json(
                agent,
                &self.codex_hooks_path(),
                CODEX_EVENTS,
                ReconcileMode::Install,
            ),
            Agent::Opencode => self.install_ts(
                agent,
                &self.opencode_plugin_path(),
                OPENCODE_TEMPLATE,
                &self.opencode_legacy_path(),
            ),
            Agent::Pi => self.install_ts(
                agent,
                &self.pi_extension_path(),
                PI_TEMPLATE,
                &self.pi_legacy_path(),
            ),
        }
    }

    pub fn status(&self, agent: Agent) -> io::Result<HookStatus> {
        match agent {
            Agent::Claude => self.status_json(agent, &self.claude_settings_path(), CLAUDE_EVENTS),
            Agent::Codex => self.status_json(agent, &self.codex_hooks_path(), CODEX_EVENTS),
            Agent::Opencode => {
                self.status_ts(agent, &self.opencode_plugin_path(), OPENCODE_TEMPLATE)
            }
            Agent::Pi => self.status_ts(agent, &self.pi_extension_path(), PI_TEMPLATE),
        }
    }

    pub fn uninstall(&self, agent: Agent) -> io::Result<HookStatus> {
        match agent {
            Agent::Claude => self.reconcile_json(
                agent,
                &self.claude_settings_path(),
                CLAUDE_EVENTS,
                ReconcileMode::Uninstall,
            ),
            Agent::Codex => self.reconcile_json(
                agent,
                &self.codex_hooks_path(),
                CODEX_EVENTS,
                ReconcileMode::Uninstall,
            ),
            Agent::Opencode => self.uninstall_ts(
                agent,
                &self.opencode_plugin_path(),
                OPENCODE_TEMPLATE,
                &self.opencode_legacy_path(),
            ),
            Agent::Pi => self.uninstall_ts(
                agent,
                &self.pi_extension_path(),
                PI_TEMPLATE,
                &self.pi_legacy_path(),
            ),
        }
    }

    fn claude_settings_path(&self) -> PathBuf {
        self.roots.claude_dir.join("settings.json")
    }

    fn codex_hooks_path(&self) -> PathBuf {
        self.roots.codex_dir.join("hooks.json")
    }

    fn opencode_plugin_path(&self) -> PathBuf {
        self.roots
            .opencode_config_dir
            .join("plugins")
            .join("hooklinesinker-opencode.ts")
    }

    fn opencode_legacy_path(&self) -> PathBuf {
        self.roots
            .opencode_config_dir
            .join("plugins")
            .join("juggler-opencode.ts")
    }

    fn pi_extension_path(&self) -> PathBuf {
        self.roots
            .pi_agent_dir
            .join("extensions")
            .join("hooklinesinker-pi.ts")
    }

    fn pi_legacy_path(&self) -> PathBuf {
        self.roots
            .pi_agent_dir
            .join("extensions")
            .join("juggler-pi.ts")
    }

    fn reconcile_json(
        &self,
        agent: Agent,
        path: &Path,
        events: &[EventSpec],
        mode: ReconcileMode,
    ) -> io::Result<HookStatus> {
        let root = read_json_root(path)?;
        if root.is_none() && matches!(mode, ReconcileMode::Uninstall) {
            return Ok(HookStatus {
                agent,
                state: HookState::Missing,
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        }
        let mut root = root.unwrap_or_default();

        let hooks_value = root.remove("hooks");
        let mut hooks = match hooks_value {
            None => Map::new(),
            Some(Value::Object(map)) => map,
            Some(other) => {
                root.insert("hooks".to_string(), other);
                return Ok(HookStatus {
                    agent,
                    state: HookState::Unsupported,
                    path: path.to_path_buf(),
                    entries: Vec::new(),
                });
            }
        };

        if managed_event_shape_is_unsupported(&hooks, events) {
            root.insert("hooks".to_string(), Value::Object(hooks));
            return Ok(HookStatus {
                agent,
                state: HookState::Unsupported,
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        }

        let keys: Vec<String> = hooks.keys().cloned().collect();
        for key in &keys {
            if let Some(Value::Array(arr)) = hooks.get(key) {
                let filtered: Vec<Value> = arr
                    .iter()
                    .filter(|g| !group_is_legacy(g))
                    .cloned()
                    .collect();
                hooks.insert(key.clone(), Value::Array(filtered));
            }
        }

        for spec in events {
            let canonical = canonical_command(&self.roots.binary_path, agent, spec.name);
            let existing = hooks
                .remove(spec.name)
                .unwrap_or_else(|| Value::Array(Vec::new()));
            let Value::Array(arr) = existing else {
                unreachable!("non-array managed event values are rejected above");
            };
            let mut filtered: Vec<Value> = arr
                .into_iter()
                .filter(|g| {
                    !matches!(
                        classify_group(g, &canonical, agent, spec.name),
                        GroupOwnership::Ours { .. }
                    )
                })
                .collect();
            if matches!(mode, ReconcileMode::Install) {
                filtered.push(build_group(spec, &canonical));
            }
            hooks.insert(spec.name.to_string(), Value::Array(filtered));
        }

        let keys: Vec<String> = hooks.keys().cloned().collect();
        for key in keys {
            let is_empty = matches!(hooks.get(&key), Some(Value::Array(a)) if a.is_empty());
            if is_empty {
                hooks.remove(&key);
            }
        }

        root.insert("hooks".to_string(), Value::Object(hooks));
        let mut bytes = serde_json::to_vec_pretty(&Value::Object(root))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        bytes.push(b'\n');
        write_agent_config(path, &bytes)?;

        self.status_json(agent, path, events)
    }

    fn status_json(
        &self,
        agent: Agent,
        path: &Path,
        events: &[EventSpec],
    ) -> io::Result<HookStatus> {
        let Some(root) = read_json_root(path)? else {
            return Ok(HookStatus {
                agent,
                state: HookState::Missing,
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        };

        let hooks = match root.get("hooks") {
            None => {
                return Ok(HookStatus {
                    agent,
                    state: HookState::Missing,
                    path: path.to_path_buf(),
                    entries: Vec::new(),
                });
            }
            Some(Value::Object(map)) => map,
            Some(_) => {
                return Ok(HookStatus {
                    agent,
                    state: HookState::Unsupported,
                    path: path.to_path_buf(),
                    entries: Vec::new(),
                });
            }
        };

        if managed_event_shape_is_unsupported(hooks, events) {
            return Ok(HookStatus {
                agent,
                state: HookState::Unsupported,
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        }

        let mut entries = Vec::new();
        let mut installed_events: HashSet<&str> = HashSet::new();
        let mut any_found = false;

        for spec in events {
            let canonical = canonical_command(&self.roots.binary_path, agent, spec.name);
            if let Some(Value::Array(arr)) = hooks.get(spec.name) {
                for (index, group) in arr.iter().enumerate() {
                    if let GroupOwnership::Ours { exact } =
                        classify_group(group, &canonical, agent, spec.name)
                    {
                        any_found = true;
                        if exact {
                            installed_events.insert(spec.name);
                        }
                        let command = group_handler_commands(group)
                            .into_iter()
                            .next()
                            .unwrap_or_default();
                        entries.push(HookEntry {
                            event: spec.name.to_string(),
                            group_index: index,
                            command,
                        });
                    }
                }
            }
        }

        let state = if installed_events.len() == events.len() {
            HookState::Installed
        } else if any_found {
            HookState::Drifted
        } else {
            HookState::Missing
        };

        Ok(HookStatus {
            agent,
            state,
            path: path.to_path_buf(),
            entries,
        })
    }

    fn generated_content(&self, template: &str) -> String {
        let marker = format!("{MARKER_PREFIX}{}\n", crate::protocol::PROTOCOL_VERSION);
        let bin = escape_ts_string(&self.roots.binary_path.display().to_string());
        let body = template.replace(BIN_PLACEHOLDER, &bin);
        format!("{marker}{body}")
    }

    fn status_ts(&self, agent: Agent, path: &Path, template: &str) -> io::Result<HookStatus> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(HookStatus {
                    agent,
                    state: HookState::Missing,
                    path: path.to_path_buf(),
                    entries: Vec::new(),
                });
            }
            Err(e) => return Err(e),
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if !text.starts_with(MARKER_PREFIX) {
            return Ok(HookStatus {
                agent,
                state: HookState::Unsupported,
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        }
        let expected = self.generated_content(template);
        let state = if text == expected {
            HookState::Installed
        } else {
            HookState::Drifted
        };
        Ok(HookStatus {
            agent,
            state,
            path: path.to_path_buf(),
            entries: Vec::new(),
        })
    }

    fn install_ts(
        &self,
        agent: Agent,
        path: &Path,
        template: &str,
        legacy_path: &Path,
    ) -> io::Result<HookStatus> {
        remove_if_exists(legacy_path)?;
        let current = self.status_ts(agent, path, template)?;
        if current.state == HookState::Unsupported {
            return Ok(current);
        }
        let contents = self.generated_content(template);
        write_generated_file(path, &contents)?;
        self.status_ts(agent, path, template)
    }

    fn uninstall_ts(
        &self,
        agent: Agent,
        path: &Path,
        template: &str,
        legacy_path: &Path,
    ) -> io::Result<HookStatus> {
        remove_if_exists(legacy_path)?;
        let current = self.status_ts(agent, path, template)?;
        match current.state {
            HookState::Missing | HookState::Unsupported => Ok(current),
            HookState::Installed | HookState::Drifted => {
                let text = fs::read_to_string(path)?;
                if file_protocol(&text) == Some(crate::protocol::PROTOCOL_VERSION) {
                    fs::remove_file(path)?;
                    self.status_ts(agent, path, template)
                } else {
                    Ok(current)
                }
            }
        }
    }
}

enum ReconcileMode {
    Install,
    Uninstall,
}

enum GroupOwnership {
    Ours { exact: bool },
    Foreign,
}

fn wire_name(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Opencode => "opencode",
        Agent::Pi => "pi",
    }
}

fn canonical_command(binary_path: &Path, agent: Agent, event: &str) -> String {
    format!(
        "{} ingest --agent {} --event {}",
        binary_path.display(),
        wire_name(agent),
        event
    )
}

fn is_legacy_command(command: &str) -> bool {
    command.contains("hooks/juggler/notify.sh") || command.contains("codex/hooks/juggler/notify.sh")
}

fn group_handler_commands(group: &Value) -> Vec<String> {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|handlers| {
            handlers
                .iter()
                .filter_map(|h| h.get("command").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn group_is_legacy(group: &Value) -> bool {
    group_handler_commands(group)
        .iter()
        .any(|c| is_legacy_command(c))
}

fn managed_event_shape_is_unsupported(hooks: &Map<String, Value>, events: &[EventSpec]) -> bool {
    events.iter().any(
        |spec| matches!(hooks.get(spec.name), Some(value) if !matches!(value, Value::Array(_))),
    )
}

fn classify_group(group: &Value, canonical: &str, agent: Agent, event: &str) -> GroupOwnership {
    let commands = group_handler_commands(group);
    if commands.len() == 1 {
        let command = &commands[0];
        if command == canonical {
            return GroupOwnership::Ours { exact: true };
        }
        let suffix = format!(" ingest --agent {} --event {}", wire_name(agent), event);
        if command.ends_with(&suffix)
            && command[..command.len() - suffix.len()].ends_with("hooklinesinker")
        {
            return GroupOwnership::Ours { exact: false };
        }
    }
    GroupOwnership::Foreign
}

fn build_group(spec: &EventSpec, canonical: &str) -> Value {
    let mut handler = Map::new();
    handler.insert("type".to_string(), Value::String("command".to_string()));
    handler.insert("command".to_string(), Value::String(canonical.to_string()));
    handler.insert("timeout".to_string(), Value::from(spec.timeout));
    let mut group = Map::new();
    if let Some(matcher) = spec.matcher {
        group.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    group.insert(
        "hooks".to_string(),
        Value::Array(vec![Value::Object(handler)]),
    );
    Value::Object(group)
}

fn read_json_root(path: &Path) -> io::Result<Option<Map<String, Value>>> {
    match fs::read(path) {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not valid JSON: {e}", path.display()),
                )
            })?;
            match value {
                Value::Object(map) => Ok(Some(map)),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} must contain a JSON object", path.display()),
                )),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn write_agent_config(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("hooklinesinker-tmp");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn write_generated_file(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("hooklinesinker-tmp");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn file_protocol(text: &str) -> Option<u16> {
    text.lines()
        .next()?
        .strip_prefix(MARKER_PREFIX)?
        .trim()
        .parse()
        .ok()
}

fn escape_ts_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_TEMP_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "hooklinesinker-hooks-test-{label}-{}-{nanos}-{id}",
            std::process::id()
        ))
    }

    fn manager() -> (HookManager, PathBuf) {
        let base = temp_root("manager");
        let binary_path = base.join("bin").join("hooklinesinker");
        let roots = HookRoots {
            claude_dir: base.join("claude"),
            codex_dir: base.join("codex"),
            opencode_config_dir: base.join("opencode"),
            pi_agent_dir: base.join("pi"),
            binary_path,
        };
        (HookManager::new(roots), base)
    }

    #[test]
    fn claude_install_on_a_missing_file_creates_all_eleven_events() {
        let (manager, base) = manager();
        let status = manager.install(Agent::Claude).unwrap();
        assert_eq!(status.state, HookState::Installed);
        assert_eq!(status.entries.len(), 11);

        let path = base.join("claude").join("settings.json");
        let value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let session_start = &value["hooks"]["SessionStart"][0];
        assert_eq!(
            session_start["hooks"][0]["command"],
            format!(
                "{} ingest --agent claude --event SessionStart",
                base.join("bin").join("hooklinesinker").display()
            )
        );
        assert!(session_start.get("matcher").is_none());
        let pre_tool_use = &value["hooks"]["PreToolUse"][0];
        assert_eq!(pre_tool_use["matcher"], "*");
    }

    #[test]
    fn claude_install_preserves_unrelated_hook_groups_and_removes_legacy_juggler_ones() {
        let (manager, base) = manager();
        let settings_path = base.join("claude").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            serde_json::json!({
                "hooks": {
                    "PreToolUse": [
                        {"matcher": "Bash", "hooks": [{"type": "command", "command": "~/.claude/hooks/user/lint.sh PreToolUse", "timeout": 5}]},
                        {"matcher": "*", "hooks": [{"type": "command", "command": "~/.claude/hooks/juggler/notify.sh PreToolUse", "timeout": 5}]}
                    ],
                    "SessionStart": [
                        {"hooks": [{"type": "command", "command": "~/.claude/hooks/juggler/notify.sh SessionStart", "timeout": 5}]}
                    ]
                },
                "permissions": {"allow": ["Bash(ls:*)"]}
            })
            .to_string(),
        )
        .unwrap();

        let status = manager.install(Agent::Claude).unwrap();
        assert_eq!(status.state, HookState::Installed);

        let value: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(value["permissions"]["allow"][0], "Bash(ls:*)");

        let pre_tool_use = value["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 2);
        assert_eq!(pre_tool_use[0]["matcher"], "Bash");
        assert_eq!(
            pre_tool_use[0]["hooks"][0]["command"],
            "~/.claude/hooks/user/lint.sh PreToolUse"
        );
        assert!(pre_tool_use.iter().all(|g| {
            !g["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("juggler")
        }));

        let session_start = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_start.len(), 1);
        assert!(
            session_start[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("hooklinesinker")
        );
    }

    #[test]
    fn a_second_claude_install_is_byte_for_byte_idempotent() {
        let (manager, base) = manager();
        manager.install(Agent::Claude).unwrap();
        let settings_path = base.join("claude").join("settings.json");
        let first = fs::read(&settings_path).unwrap();

        manager.install(Agent::Claude).unwrap();
        let second = fs::read(&settings_path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn claude_status_reports_missing_installed_and_drifted() {
        let (manager, base) = manager();
        assert_eq!(
            manager.status(Agent::Claude).unwrap().state,
            HookState::Missing
        );

        manager.install(Agent::Claude).unwrap();
        assert_eq!(
            manager.status(Agent::Claude).unwrap().state,
            HookState::Installed
        );

        let settings_path = base.join("claude").join("settings.json");
        let mut value: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        value["hooks"]["SessionStart"] = serde_json::json!([]);
        fs::write(&settings_path, value.to_string()).unwrap();
        assert_eq!(
            manager.status(Agent::Claude).unwrap().state,
            HookState::Drifted
        );
    }

    #[test]
    fn claude_status_reports_unsupported_when_hooks_field_has_the_wrong_shape() {
        let (manager, base) = manager();
        let settings_path = base.join("claude").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(&settings_path, r#"{"hooks": "not an object"}"#).unwrap();
        assert_eq!(
            manager.status(Agent::Claude).unwrap().state,
            HookState::Unsupported
        );

        let install_result = manager.install(Agent::Claude).unwrap();
        assert_eq!(install_result.state, HookState::Unsupported);
        let untouched = fs::read_to_string(&settings_path).unwrap();
        assert_eq!(untouched, r#"{"hooks": "not an object"}"#);
    }

    #[test]
    fn claude_install_never_drops_a_non_array_value_under_a_managed_event_key() {
        let (manager, base) = manager();
        let settings_path = base.join("claude").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let original = serde_json::json!({
            "hooks": {
                "SessionStart": {"unexpected": "shape"},
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "~/.claude/hooks/user/lint.sh PreToolUse", "timeout": 5}]}
                ]
            }
        })
        .to_string();
        fs::write(&settings_path, &original).unwrap();

        let result = manager.install(Agent::Claude).unwrap();
        assert_eq!(result.state, HookState::Unsupported);
        assert_eq!(
            fs::read_to_string(&settings_path).unwrap(),
            original,
            "a non-array value under a managed event key must block every write, not just that key"
        );
    }

    #[test]
    fn claude_uninstall_never_drops_a_non_array_value_under_a_managed_event_key() {
        let (manager, base) = manager();
        let settings_path = base.join("claude").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let original = serde_json::json!({
            "hooks": {
                "SessionStart": {"unexpected": "shape"}
            }
        })
        .to_string();
        fs::write(&settings_path, &original).unwrap();

        let result = manager.uninstall(Agent::Claude).unwrap();
        assert_eq!(result.state, HookState::Unsupported);
        assert_eq!(
            fs::read_to_string(&settings_path).unwrap(),
            original,
            "uninstall must never silently drop foreign-shaped data either"
        );
    }

    #[test]
    fn claude_status_reports_unsupported_for_a_non_array_value_under_a_managed_event_key() {
        let (manager, base) = manager();
        let settings_path = base.join("claude").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            serde_json::json!({"hooks": {"SessionStart": {"unexpected": "shape"}}}).to_string(),
        )
        .unwrap();

        assert_eq!(
            manager.status(Agent::Claude).unwrap().state,
            HookState::Unsupported
        );
    }

    #[test]
    fn claude_uninstall_removes_only_hooklinesinker_and_legacy_groups() {
        let (manager, base) = manager();
        manager.install(Agent::Claude).unwrap();
        let settings_path = base.join("claude").join("settings.json");
        let mut value: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        value["hooks"]["PreToolUse"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": "~/.claude/hooks/user/lint.sh PreToolUse", "timeout": 5}]
            }));
        fs::write(&settings_path, value.to_string()).unwrap();

        let status = manager.uninstall(Agent::Claude).unwrap();
        assert_eq!(status.state, HookState::Missing);

        let value: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        let pre_tool_use = value["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 1);
        assert_eq!(pre_tool_use[0]["matcher"], "Bash");
    }

    #[test]
    fn codex_install_has_no_matcher_and_clamps_session_end_timeout() {
        let (manager, base) = manager();
        let status = manager.install(Agent::Codex).unwrap();
        assert_eq!(status.state, HookState::Installed);
        assert_eq!(status.entries.len(), 9);

        let path = base.join("codex").join("hooks.json");
        let value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let session_start = &value["hooks"]["SessionStart"][0];
        assert!(session_start.get("matcher").is_none());
        assert_eq!(session_start["hooks"][0]["timeout"], 5);
        let session_end = &value["hooks"]["SessionEnd"][0];
        assert_eq!(session_end["hooks"][0]["timeout"], 3);
    }

    #[test]
    fn codex_status_exposes_the_group_index_of_the_owned_entry_for_trust_computation() {
        let (manager, base) = manager();
        let hooks_path = base.join("codex").join("hooks.json");
        fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "SessionStart": [
                        {"hooks": [{"type": "command", "command": "~/.codex/hooks/user/audit.sh SessionStart", "timeout": 5}]}
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();

        manager.install(Agent::Codex).unwrap();
        let status = manager.status(Agent::Codex).unwrap();
        let session_start_entry = status
            .entries
            .iter()
            .find(|e| e.event == "SessionStart")
            .unwrap();
        assert_eq!(
            session_start_entry.group_index, 1,
            "our group must land after the user's pre-existing SessionStart group"
        );
        let other_entry = status.entries.iter().find(|e| e.event == "Stop").unwrap();
        assert_eq!(other_entry.group_index, 0);
    }

    #[test]
    fn codex_install_removes_legacy_juggler_notify_commands() {
        let (manager, base) = manager();
        let hooks_path = base.join("codex").join("hooks.json");
        fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "SessionStart": [
                        {"hooks": [{"type": "command", "command": "/Users/x/.codex/hooks/juggler/notify.sh SessionStart", "timeout": 5}]}
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();

        manager.install(Agent::Codex).unwrap();
        let value: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
        let session_start = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_start.len(), 1);
        assert!(
            session_start[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("hooklinesinker")
        );
    }

    #[test]
    fn opencode_install_writes_a_marked_file_and_is_idempotent() {
        let (manager, base) = manager();
        let status = manager.install(Agent::Opencode).unwrap();
        assert_eq!(status.state, HookState::Installed);

        let path = base
            .join("opencode")
            .join("plugins")
            .join("hooklinesinker-opencode.ts");
        let first = fs::read(&path).unwrap();
        assert!(
            String::from_utf8_lossy(&first).starts_with("// hooklinesinker-generated protocol=1\n")
        );
        assert!(
            String::from_utf8_lossy(&first).contains(
                base.join("bin")
                    .join("hooklinesinker")
                    .display()
                    .to_string()
                    .as_str()
            )
        );

        manager.install(Agent::Opencode).unwrap();
        let second = fs::read(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn opencode_install_removes_the_legacy_juggler_plugin_file() {
        let (manager, base) = manager();
        let legacy_path = base
            .join("opencode")
            .join("plugins")
            .join("juggler-opencode.ts");
        fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        fs::write(&legacy_path, "// legacy juggler plugin\n").unwrap();

        manager.install(Agent::Opencode).unwrap();
        assert!(!legacy_path.exists());
    }

    #[test]
    fn opencode_status_reports_unsupported_for_a_foreign_file_and_never_touches_it() {
        let (manager, base) = manager();
        let path = base
            .join("opencode")
            .join("plugins")
            .join("hooklinesinker-opencode.ts");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "// some unrelated user plugin\nexport default () => {};\n",
        )
        .unwrap();

        assert_eq!(
            manager.status(Agent::Opencode).unwrap().state,
            HookState::Unsupported
        );
        let install_result = manager.install(Agent::Opencode).unwrap();
        assert_eq!(install_result.state, HookState::Unsupported);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "// some unrelated user plugin\nexport default () => {};\n"
        );

        let uninstall_result = manager.uninstall(Agent::Opencode).unwrap();
        assert_eq!(uninstall_result.state, HookState::Unsupported);
        assert!(path.exists());
    }

    #[test]
    fn opencode_uninstall_removes_our_file_only_when_the_protocol_marker_matches() {
        let (manager, base) = manager();
        manager.install(Agent::Opencode).unwrap();
        let path = base
            .join("opencode")
            .join("plugins")
            .join("hooklinesinker-opencode.ts");

        let stale = fs::read_to_string(&path)
            .unwrap()
            .replacen("protocol=1", "protocol=99", 1);
        fs::write(&path, stale).unwrap();

        let result = manager.uninstall(Agent::Opencode).unwrap();
        assert_eq!(result.state, HookState::Drifted);
        assert!(
            path.exists(),
            "a file from an unrecognized protocol must not be removed"
        );
    }

    #[test]
    fn pi_install_writes_a_marked_extension_file() {
        let (manager, base) = manager();
        let status = manager.install(Agent::Pi).unwrap();
        assert_eq!(status.state, HookState::Installed);
        let path = base
            .join("pi")
            .join("extensions")
            .join("hooklinesinker-pi.ts");
        assert!(path.exists());
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .starts_with("// hooklinesinker-generated protocol=1\n")
        );
    }

    #[test]
    fn last_consumer_uninstall_removes_hooks_for_every_agent() {
        let (manager, base) = manager();
        for agent in [Agent::Claude, Agent::Codex, Agent::Opencode, Agent::Pi] {
            manager.install(agent).unwrap();
        }
        for agent in [Agent::Claude, Agent::Codex, Agent::Opencode, Agent::Pi] {
            manager.uninstall(agent).unwrap();
            assert_eq!(manager.status(agent).unwrap().state, HookState::Missing);
        }
        let _ = base;
    }
}
