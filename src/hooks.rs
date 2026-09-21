use crate::agents::{HookTarget, profile};
#[cfg(test)]
use crate::events::native_event_specs;
use crate::events::{EventSpec, subscribed_event_specs};
use crate::persistence::{LockGuard, write_preserving_atomic_if_unchanged, write_private_atomic};
use crate::protocol::{Agent, Capability};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value as toml_value};

const MARKER_PREFIX: &str = "// hooklinesinker-generated protocol=";
const BIN_PLACEHOLDER: &str = "__HOOKLINESINKER_BIN__";

const OPENCODE_TEMPLATE: &str = include_str!("../adapters/opencode-hooklinesinker.ts");
const PI_TEMPLATE: &str = include_str!("../adapters/pi-hooklinesinker.ts");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HookState {
    Missing,
    Installed,
    Drifted,
    Unsupported,
}

impl HookState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Installed => "installed",
            Self::Drifted => "drifted",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HookEntry {
    pub event: String,
    pub group_index: usize,
    pub command: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HookStatus {
    pub agent: Agent,
    pub state: HookState,
    pub path: PathBuf,
    pub entries: Vec<HookEntry>,
}

#[derive(Clone, Debug)]
pub(crate) struct HookRoots {
    pub claude_dir: PathBuf,
    pub codex_dir: PathBuf,
    pub opencode_config_dir: PathBuf,
    pub pi_agent_dir: PathBuf,
    pub factory_dir: PathBuf,
    pub qwen_config_dir: PathBuf,
    pub kimi_code_dir: PathBuf,
    pub binary_path: PathBuf,
}

impl HookRoots {
    pub(crate) fn from_env(binary_path: PathBuf) -> io::Result<Self> {
        let home = crate::paths::home_dir()?;
        let claude_dir = home.join(".claude");
        let codex_dir = home.join(".codex");
        let opencode_config_dir =
            if let Some(path) = crate::paths::absolute_env_path("OPENCODE_CONFIG_DIR")? {
                path
            } else if let Some(path) = crate::paths::absolute_env_path("XDG_CONFIG_HOME")? {
                path.join("opencode")
            } else {
                home.join(".config/opencode")
            };
        let pi_agent_dir = crate::paths::absolute_env_path("PI_CODING_AGENT_DIR")?
            .unwrap_or_else(|| home.join(".pi/agent"));
        let factory_dir = home.join(".factory");
        let qwen_config_dir =
            crate::paths::absolute_env_path("QWEN_HOME")?.unwrap_or_else(|| home.join(".qwen"));
        let kimi_code_dir = crate::paths::absolute_env_path("KIMI_CODE_HOME")?
            .unwrap_or_else(|| home.join(".kimi-code"));
        Ok(Self {
            claude_dir,
            codex_dir,
            opencode_config_dir,
            pi_agent_dir,
            factory_dir,
            qwen_config_dir,
            kimi_code_dir,
            binary_path,
        })
    }
}

pub(crate) struct HookManager {
    roots: HookRoots,
}

enum HookBackend {
    Json {
        path: PathBuf,
        location: HooksLocation,
    },
    Toml {
        path: PathBuf,
    },
    TypeScript {
        path: PathBuf,
        legacy_path: PathBuf,
        template: &'static str,
    },
}

impl HookManager {
    pub(crate) const fn new(roots: HookRoots) -> Self {
        Self { roots }
    }

    pub(crate) fn install(&self, agent: Agent) -> io::Result<HookStatus> {
        let backend = self.backend(agent);
        let _guard = LockGuard::acquire_for(backend.path())?;
        let events = subscribed_event_specs(agent, &Capability::ALL);
        match backend {
            HookBackend::Json { path, location } => {
                self.json_hooks()
                    .reconcile(agent, &path, &events, ReconcileMode::Install, location)
            }
            HookBackend::Toml { path } => {
                self.toml_hooks()
                    .reconcile(agent, &path, &events, ReconcileMode::Install)
            }
            HookBackend::TypeScript {
                path,
                legacy_path,
                template,
            } => self
                .typescript_hooks()
                .install(agent, &path, template, &legacy_path),
        }
    }

    pub(crate) fn status(&self, agent: Agent) -> io::Result<HookStatus> {
        let events = subscribed_event_specs(agent, &Capability::ALL);
        match self.backend(agent) {
            HookBackend::Json { path, location } => {
                self.json_hooks().status(agent, &path, &events, location)
            }
            HookBackend::Toml { path } => self.toml_hooks().status(agent, &path, &events),
            HookBackend::TypeScript { path, template, .. } => {
                self.typescript_hooks().status(agent, &path, template)
            }
        }
    }

    pub(crate) fn uninstall(&self, agent: Agent) -> io::Result<HookStatus> {
        let backend = self.backend(agent);
        let _guard = LockGuard::acquire_for(backend.path())?;
        let events = subscribed_event_specs(agent, &Capability::ALL);
        match backend {
            HookBackend::Json { path, location } => self.json_hooks().reconcile(
                agent,
                &path,
                &events,
                ReconcileMode::Uninstall,
                location,
            ),
            HookBackend::Toml { path } => {
                self.toml_hooks()
                    .reconcile(agent, &path, &events, ReconcileMode::Uninstall)
            }
            HookBackend::TypeScript {
                path,
                legacy_path,
                template,
            } => self
                .typescript_hooks()
                .uninstall(agent, &path, template, &legacy_path),
        }
    }

    fn backend(&self, agent: Agent) -> HookBackend {
        match profile(agent).hook_target {
            HookTarget::ClaudeJson => HookBackend::Json {
                path: self.claude_settings_path(),
                location: HooksLocation::Nested("hooks"),
            },
            HookTarget::CodexJson => HookBackend::Json {
                path: self.codex_hooks_path(),
                location: HooksLocation::Nested("hooks"),
            },
            HookTarget::OpenCodeTypeScript => HookBackend::TypeScript {
                path: self.opencode_plugin_path(),
                legacy_path: self.opencode_legacy_path(),
                template: OPENCODE_TEMPLATE,
            },
            HookTarget::PiTypeScript => HookBackend::TypeScript {
                path: self.pi_extension_path(),
                legacy_path: self.pi_legacy_path(),
                template: PI_TEMPLATE,
            },
            HookTarget::DroidJson => HookBackend::Json {
                path: self.droid_hooks_path(),
                location: HooksLocation::TopLevel,
            },
            HookTarget::QwenJson => HookBackend::Json {
                path: self.qwen_settings_path(),
                location: HooksLocation::Nested("hooks"),
            },
            HookTarget::KimiToml => HookBackend::Toml {
                path: self.kimi_config_path(),
            },
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

    fn droid_hooks_path(&self) -> PathBuf {
        self.roots.factory_dir.join("hooks.json")
    }

    fn qwen_settings_path(&self) -> PathBuf {
        self.roots.qwen_config_dir.join("settings.json")
    }

    fn kimi_config_path(&self) -> PathBuf {
        self.roots.kimi_code_dir.join("config.toml")
    }

    fn json_hooks(&self) -> JsonHooks<'_> {
        JsonHooks {
            binary_path: &self.roots.binary_path,
        }
    }

    fn toml_hooks(&self) -> TomlHooks<'_> {
        TomlHooks {
            binary_path: &self.roots.binary_path,
        }
    }

    fn typescript_hooks(&self) -> TypeScriptHooks<'_> {
        TypeScriptHooks {
            binary_path: &self.roots.binary_path,
        }
    }
}

impl HookBackend {
    fn path(&self) -> &Path {
        match self {
            Self::Json { path, .. } | Self::Toml { path } | Self::TypeScript { path, .. } => path,
        }
    }
}

struct JsonHooks<'a> {
    binary_path: &'a Path,
}

impl JsonHooks<'_> {
    fn reconcile(
        &self,
        agent: Agent,
        path: &Path,
        events: &[EventSpec],
        mode: ReconcileMode,
        location: HooksLocation,
    ) -> io::Result<HookStatus> {
        let original_bytes = read_optional_bytes(path)?;
        if original_bytes.is_none() && matches!(mode, ReconcileMode::Uninstall) {
            return Ok(HookStatus {
                agent,
                state: HookState::Missing,
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        }
        let root = original_bytes
            .as_deref()
            .map(|bytes| parse_json_root(path, bytes))
            .transpose()?
            .unwrap_or_default();
        let original_root = root.clone();

        let Ok((root, mut hooks)) = take_hook_map(root, location) else {
            return Ok(HookStatus {
                agent,
                state: HookState::Unsupported,
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        };

        if managed_event_shape_is_unsupported(&hooks, events) {
            return Ok(HookStatus {
                agent,
                state: HookState::Unsupported,
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        }

        remove_legacy_handlers(&mut hooks);
        reconcile_managed_events(&mut hooks, events, mode, self.binary_path, agent);
        hooks.retain(|_, value| !matches!(value, Value::Array(items) if items.is_empty()));
        let final_root = place_hook_map(root, hooks, location);
        if final_root == original_root {
            return self.status(agent, path, events, location);
        }
        let mut bytes = serde_json::to_vec_pretty(&Value::Object(final_root))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        bytes.push(b'\n');
        write_agent_config(path, original_bytes.as_deref(), &bytes)?;

        self.status(agent, path, events, location)
    }

    fn status(
        &self,
        agent: Agent,
        path: &Path,
        events: &[EventSpec],
        location: HooksLocation,
    ) -> io::Result<HookStatus> {
        let Some(root) = read_json_root(path)? else {
            return Ok(HookStatus {
                agent,
                state: HookState::Missing,
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        };

        let hooks = match location {
            HooksLocation::Nested(key) => match root.get(key) {
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
            },
            HooksLocation::TopLevel => &root,
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
            let canonical = canonical_command(self.binary_path, agent, spec.name);
            let expected = build_group(spec, &canonical, agent);
            let mut owned_count = 0;
            let mut exact_count = 0;
            if let Some(Value::Array(arr)) = hooks.get(spec.name) {
                for (index, group) in arr.iter().enumerate() {
                    if let GroupOwnership::Ours { exact } =
                        classify_group(group, &canonical, agent, spec.name)
                    {
                        any_found = true;
                        owned_count += 1;
                        if exact && group == &expected {
                            exact_count += 1;
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
            if owned_count == 1 && exact_count == 1 {
                installed_events.insert(spec.name);
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
}

struct TomlHooks<'a> {
    binary_path: &'a Path,
}

impl TomlHooks<'_> {
    // Kimi rejects the entire config if any hook entry is malformed.
    fn reconcile(
        &self,
        agent: Agent,
        path: &Path,
        events: &[EventSpec],
        mode: ReconcileMode,
    ) -> io::Result<HookStatus> {
        let existing = match fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if existing.is_none() && matches!(mode, ReconcileMode::Uninstall) {
            return Ok(HookStatus {
                agent,
                state: HookState::Missing,
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        }

        let mut doc = parse_toml_document(path, existing.as_deref().unwrap_or(""))?;

        if let Some(item) = doc.get("hooks")
            && !item.is_array_of_tables()
        {
            return Ok(HookStatus {
                agent,
                state: HookState::Unsupported,
                path: path.to_path_buf(),
                entries: Vec::new(),
            });
        }

        let mut kept = ArrayOfTables::new();
        let mut any_found = false;
        if let Some(item) = doc.get("hooks") {
            let array = item
                .as_array_of_tables()
                .expect("non-array-of-tables shape rejected above");
            for table in array {
                let command = table.get("command").and_then(Item::as_str);
                let is_ours = events.iter().any(|spec| {
                    Some(canonical_command(self.binary_path, agent, spec.name).as_str()) == command
                });
                if is_ours {
                    any_found = true;
                } else {
                    kept.push(table.clone());
                }
            }
        }

        let should_write = match mode {
            ReconcileMode::Uninstall => {
                if any_found {
                    if kept.is_empty() {
                        doc.remove("hooks");
                    } else {
                        doc["hooks"] = Item::ArrayOfTables(kept);
                    }
                }
                any_found
            }
            ReconcileMode::Install => {
                for spec in events {
                    kept.push(kimi_hook_table(self.binary_path, agent, spec));
                }
                doc["hooks"] = Item::ArrayOfTables(kept);
                true
            }
        };

        if should_write {
            let rendered = doc.to_string();
            parse_toml_document(path, &rendered)?;
            write_agent_config(
                path,
                existing.as_deref().map(str::as_bytes),
                rendered.as_bytes(),
            )?;
            let on_disk = fs::read_to_string(path)?;
            parse_toml_document(path, &on_disk)?;
        }

        self.status(agent, path, events)
    }

    fn status(&self, agent: Agent, path: &Path, events: &[EventSpec]) -> io::Result<HookStatus> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
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
        let doc = parse_toml_document(path, &text)?;

        let array = match doc.get("hooks") {
            None => {
                return Ok(HookStatus {
                    agent,
                    state: HookState::Missing,
                    path: path.to_path_buf(),
                    entries: Vec::new(),
                });
            }
            Some(item) => match item.as_array_of_tables() {
                Some(array) => array,
                None => {
                    return Ok(HookStatus {
                        agent,
                        state: HookState::Unsupported,
                        path: path.to_path_buf(),
                        entries: Vec::new(),
                    });
                }
            },
        };

        let mut entries = Vec::new();
        let mut installed_events: HashSet<&str> = HashSet::new();
        for spec in events {
            let canonical = canonical_command(self.binary_path, agent, spec.name);
            let mut owned_count = 0;
            let mut exact_count = 0;
            for (index, table) in array.iter().enumerate() {
                if table.get("command").and_then(Item::as_str) != Some(canonical.as_str()) {
                    continue;
                }
                owned_count += 1;
                if kimi_table_is_exact(table, spec, &canonical) {
                    exact_count += 1;
                }
                entries.push(HookEntry {
                    event: spec.name.to_string(),
                    group_index: index,
                    command: canonical.clone(),
                });
            }
            if owned_count == 1 && exact_count == 1 {
                installed_events.insert(spec.name);
            }
        }

        let state = if !events.is_empty() && installed_events.len() == events.len() {
            HookState::Installed
        } else if !entries.is_empty() {
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
}

struct TypeScriptHooks<'a> {
    binary_path: &'a Path,
}

impl TypeScriptHooks<'_> {
    fn generated_content(&self, template: &str) -> String {
        let marker = format!("{MARKER_PREFIX}{}\n", crate::protocol::PROTOCOL_VERSION);
        let bin = serde_json::to_string(&self.binary_path.display().to_string())
            .expect("a path string always serializes");
        let body = template.replace(&format!("\"{BIN_PLACEHOLDER}\""), &bin);
        format!("{marker}{body}")
    }

    fn status(&self, agent: Agent, path: &Path, template: &str) -> io::Result<HookStatus> {
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

    fn install(
        &self,
        agent: Agent,
        path: &Path,
        template: &str,
        legacy_path: &Path,
    ) -> io::Result<HookStatus> {
        remove_legacy_generated_file(legacy_path)?;
        let current = self.status(agent, path, template)?;
        if current.state == HookState::Unsupported {
            return Ok(current);
        }
        let contents = self.generated_content(template);
        write_generated_file(path, &contents)?;
        self.status(agent, path, template)
    }

    fn uninstall(
        &self,
        agent: Agent,
        path: &Path,
        template: &str,
        legacy_path: &Path,
    ) -> io::Result<HookStatus> {
        remove_legacy_generated_file(legacy_path)?;
        let current = self.status(agent, path, template)?;
        match current.state {
            HookState::Missing | HookState::Unsupported => Ok(current),
            HookState::Installed | HookState::Drifted => {
                let text = fs::read_to_string(path)?;
                if file_protocol(&text) == Some(crate::protocol::PROTOCOL_VERSION) {
                    fs::remove_file(path)?;
                    self.status(agent, path, template)
                } else {
                    Ok(current)
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ReconcileMode {
    Install,
    Uninstall,
}

#[derive(Clone, Copy)]
enum HooksLocation {
    Nested(&'static str),
    TopLevel,
}

enum GroupOwnership {
    Ours { exact: bool },
    Foreign,
}

type HookMaps = (Map<String, Value>, Map<String, Value>);

fn take_hook_map(mut root: Map<String, Value>, location: HooksLocation) -> Result<HookMaps, ()> {
    match location {
        HooksLocation::Nested(key) => match root.remove(key) {
            None => Ok((root, Map::new())),
            Some(Value::Object(hooks)) => Ok((root, hooks)),
            Some(_) => Err(()),
        },
        HooksLocation::TopLevel => Ok((Map::new(), root)),
    }
}

fn place_hook_map(
    mut root: Map<String, Value>,
    hooks: Map<String, Value>,
    location: HooksLocation,
) -> Map<String, Value> {
    match location {
        HooksLocation::Nested(key) => {
            root.insert(key.to_string(), Value::Object(hooks));
            root
        }
        HooksLocation::TopLevel => hooks,
    }
}

fn remove_legacy_handlers(hooks: &mut Map<String, Value>) {
    for value in hooks.values_mut() {
        let Value::Array(groups) = value else {
            continue;
        };
        *groups = groups
            .drain(..)
            .filter_map(|group| retain_foreign_handlers(group, is_legacy_command))
            .collect();
    }
}

fn reconcile_managed_events(
    hooks: &mut Map<String, Value>,
    events: &[EventSpec],
    mode: ReconcileMode,
    binary_path: &Path,
    agent: Agent,
) {
    for spec in events {
        let canonical = canonical_command(binary_path, agent, spec.name);
        let existing = hooks
            .remove(spec.name)
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let Value::Array(groups) = existing else {
            unreachable!("non-array managed event values are rejected before reconciliation");
        };
        let mut kept: Vec<Value> = groups
            .into_iter()
            .filter_map(|group| {
                retain_foreign_handlers(group, |command| {
                    matches!(
                        classify_command(command, &canonical, agent, spec.name),
                        GroupOwnership::Ours { .. }
                    )
                })
            })
            .collect();
        if matches!(mode, ReconcileMode::Install) {
            kept.push(build_group(spec, &canonical, agent));
        }
        hooks.insert(spec.name.to_string(), Value::Array(kept));
    }
}

fn canonical_command(binary_path: &Path, agent: Agent, event: &str) -> String {
    format!(
        "{} ingest --agent {} --event {}",
        shell_quote(&binary_path.display().to_string()),
        agent.as_str(),
        event
    )
}

fn shell_quote(word: &str) -> String {
    if is_unquoted_shell_word(word) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn is_legacy_command(command: &str) -> bool {
    let mut words = command.split(' ');
    let binary = words.next().unwrap_or_default();
    (binary == "hooks/juggler/notify.sh" || binary.ends_with("/hooks/juggler/notify.sh"))
        && is_unquoted_shell_word(binary)
        && words.all(is_unquoted_shell_word)
}

fn is_unquoted_shell_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~'))
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

fn handler_command(handler: &Value) -> Option<&str> {
    (handler.get("type").and_then(Value::as_str) == Some("command"))
        .then(|| handler.get("command").and_then(Value::as_str))
        .flatten()
}

fn retain_foreign_handlers(mut group: Value, is_owned: impl Fn(&str) -> bool) -> Option<Value> {
    if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
        let original_len = handlers.len();
        handlers.retain(|handler| !handler_command(handler).is_some_and(&is_owned));
        if original_len > 0 && handlers.is_empty() {
            return None;
        }
    }
    Some(group)
}

fn managed_event_shape_is_unsupported(hooks: &Map<String, Value>, events: &[EventSpec]) -> bool {
    events.iter().any(
        |spec| matches!(hooks.get(spec.name), Some(value) if !matches!(value, Value::Array(_))),
    )
}

fn classify_group(group: &Value, canonical: &str, agent: Agent, event: &str) -> GroupOwnership {
    let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
        return GroupOwnership::Foreign;
    };
    let [handler] = handlers.as_slice() else {
        return GroupOwnership::Foreign;
    };
    let Some(command) = handler_command(handler) else {
        return GroupOwnership::Foreign;
    };
    classify_command(command, canonical, agent, event)
}

fn classify_command(command: &str, canonical: &str, agent: Agent, event: &str) -> GroupOwnership {
    if command == canonical {
        return GroupOwnership::Ours { exact: true };
    }
    let suffix = format!(" ingest --agent {} --event {}", agent.as_str(), event);
    if command.strip_suffix(&suffix).is_some_and(|binary| {
        decode_shell_word(binary)
            .is_some_and(|binary| binary == "hooklinesinker" || binary.ends_with("/hooklinesinker"))
    }) {
        return GroupOwnership::Ours { exact: false };
    }
    GroupOwnership::Foreign
}

fn build_group(spec: &EventSpec, canonical: &str, agent: Agent) -> Value {
    let mut handler = Map::new();
    handler.insert("type".to_string(), Value::String("command".to_string()));
    handler.insert("command".to_string(), Value::String(canonical.to_string()));
    let timeout = profile(agent).hook_target.timeout_value(spec.timeout);
    handler.insert("timeout".to_string(), Value::from(timeout));
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

fn kimi_hook_table(binary_path: &Path, agent: Agent, spec: &EventSpec) -> Table {
    let mut table = Table::new();
    table.insert("event", toml_value(spec.name));
    table.insert(
        "command",
        toml_value(canonical_command(binary_path, agent, spec.name)),
    );
    table.insert(
        "timeout",
        toml_value(i64::try_from(spec.timeout.as_secs()).expect("hook timeout fits i64 seconds")),
    );
    table
}

fn kimi_table_is_exact(table: &Table, spec: &EventSpec, canonical: &str) -> bool {
    table.len() == 3
        && table.get("event").and_then(Item::as_str) == Some(spec.name)
        && table.get("command").and_then(Item::as_str) == Some(canonical)
        && table.get("timeout").and_then(Item::as_integer)
            == i64::try_from(spec.timeout.as_secs()).ok()
}

fn parse_toml_document(path: &Path, text: &str) -> io::Result<DocumentMut> {
    if text.is_empty() {
        return Ok(DocumentMut::new());
    }
    text.parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not valid TOML: {e}", path.display()),
        )
    })
}

fn read_json_root(path: &Path) -> io::Result<Option<Map<String, Value>>> {
    read_optional_bytes(path)?
        .as_deref()
        .map(|bytes| parse_json_root(path, bytes))
        .transpose()
}

fn read_optional_bytes(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn parse_json_root(path: &Path, bytes: &[u8]) -> io::Result<Map<String, Value>> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not valid JSON: {e}", path.display()),
        )
    })?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} must contain a JSON object", path.display()),
        )),
    }
}

fn write_agent_config(path: &Path, expected: Option<&[u8]>, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_safe_config_dir(parent)?;
    }
    write_preserving_atomic_if_unchanged(path, expected, bytes)
}

fn write_generated_file(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_safe_config_dir(parent)?;
    }
    write_private_atomic(path, contents.as_bytes())
}

fn ensure_safe_config_dir(path: &Path) -> io::Result<()> {
    let existed = path.exists();
    fs::create_dir_all(path)?;
    if !existed {
        crate::paths::ensure_private_dir(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode();
        if mode & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to write hooks in writable directory {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn remove_legacy_generated_file(path: &Path) -> io::Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let recognized = text.starts_with("// Juggler plugin for OpenCode\n")
        || text.starts_with("// Juggler extension for Pi ");
    if recognized {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn file_protocol(text: &str) -> Option<u16> {
    text.lines()
        .next()?
        .strip_prefix(MARKER_PREFIX)?
        .trim()
        .parse()
        .ok()
}

fn decode_shell_word(word: &str) -> Option<String> {
    let mut decoded = String::new();
    let mut chars = word.chars();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            if c == '\'' {
                quoted = false;
            } else {
                decoded.push(c);
            }
        } else {
            match c {
                '\'' => quoted = true,
                '\\' => decoded.push(chars.next()?),
                c if c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~') => {
                    decoded.push(c);
                }
                _ => return None,
            }
        }
    }
    (!quoted && !decoded.is_empty()).then_some(decoded)
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
            factory_dir: base.join("factory"),
            qwen_config_dir: base.join("qwen"),
            kimi_code_dir: base.join("kimi-code"),
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
    fn claude_status_rejects_duplicate_and_noncanonical_owned_groups() {
        for mutation in ["duplicate", "extra-field"] {
            let (manager, base) = manager();
            manager.install(Agent::Claude).unwrap();
            let path = base.join("claude").join("settings.json");
            let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let groups = value["hooks"]["SessionStart"].as_array_mut().unwrap();
            if mutation == "duplicate" {
                groups.push(groups[0].clone());
            } else {
                groups[0]["hooks"][0]["extra"] = Value::Bool(true);
            }
            fs::write(&path, value.to_string()).unwrap();
            assert_eq!(
                manager.status(Agent::Claude).unwrap().state,
                HookState::Drifted,
                "accepted {mutation} ownership"
            );
        }
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
    fn codex_install_has_no_matcher_and_clamps_terminal_hook_timeouts() {
        let (manager, base) = manager();
        let status = manager.install(Agent::Codex).unwrap();
        assert_eq!(status.state, HookState::Installed);
        assert_eq!(status.entries.len(), 10);

        let path = base.join("codex").join("hooks.json");
        let value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let session_start = &value["hooks"]["SessionStart"][0];
        assert!(session_start.get("matcher").is_none());
        assert_eq!(session_start["hooks"][0]["timeout"], 5);
        let session_end = &value["hooks"]["SessionEnd"][0];
        assert_eq!(session_end["hooks"][0]["timeout"], 3);
        assert_eq!(
            value["hooks"]["Interrupt"][0],
            serde_json::json!({"hooks": [{
                "type": "command",
                "command": format!("{} ingest --agent codex --event Interrupt", base.join("bin/hooklinesinker").display()),
                "timeout": 3
            }]})
        );
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
    fn droid_install_on_a_missing_file_creates_all_eight_events_with_no_hooks_wrapper() {
        let (manager, base) = manager();
        let status = manager.install(Agent::Droid).unwrap();
        assert_eq!(status.state, HookState::Installed);
        assert_eq!(status.entries.len(), 8);

        let path = base.join("factory").join("hooks.json");
        let value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert!(
            value.get("hooks").is_none(),
            "droid's hooks.json is keyed directly by event name, with no wrapper"
        );
        let session_start = &value["SessionStart"][0];
        assert_eq!(
            session_start["hooks"][0]["command"],
            format!(
                "{} ingest --agent droid --event SessionStart",
                base.join("bin").join("hooklinesinker").display()
            )
        );
        assert_eq!(session_start["hooks"][0]["timeout"], 5);
        assert!(session_start.get("matcher").is_none());
        let pre_tool_use = &value["PreToolUse"][0];
        assert_eq!(pre_tool_use["matcher"], "*");
        let session_end = &value["SessionEnd"][0];
        assert_eq!(session_end["hooks"][0]["timeout"], 3);
    }

    #[test]
    fn droid_install_preserves_unrelated_top_level_content() {
        let (manager, base) = manager();
        let hooks_path = base.join("factory").join("hooks.json");
        fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        fs::write(
            &hooks_path,
            serde_json::json!({
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "~/.factory/hooks/user/lint.sh", "timeout": 5}]}
                ],
                "someFutureTopLevelKey": {"kept": true}
            })
            .to_string(),
        )
        .unwrap();

        let status = manager.install(Agent::Droid).unwrap();
        assert_eq!(status.state, HookState::Installed);

        let value: Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
        assert_eq!(value["someFutureTopLevelKey"]["kept"], true);
        let pre_tool_use = value["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 2);
        assert_eq!(pre_tool_use[0]["matcher"], "Bash");
    }

    #[test]
    fn a_second_droid_install_is_byte_for_byte_idempotent() {
        let (manager, base) = manager();
        manager.install(Agent::Droid).unwrap();
        let path = base.join("factory").join("hooks.json");
        let first = fs::read(&path).unwrap();

        manager.install(Agent::Droid).unwrap();
        let second = fs::read(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn droid_status_reports_missing_installed_and_drifted() {
        let (manager, base) = manager();
        assert_eq!(
            manager.status(Agent::Droid).unwrap().state,
            HookState::Missing
        );

        manager.install(Agent::Droid).unwrap();
        assert_eq!(
            manager.status(Agent::Droid).unwrap().state,
            HookState::Installed
        );

        let path = base.join("factory").join("hooks.json");
        let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["SessionStart"] = serde_json::json!([]);
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            manager.status(Agent::Droid).unwrap().state,
            HookState::Drifted
        );
    }

    #[test]
    fn droid_status_reports_unsupported_for_a_non_array_value_under_a_managed_event_key() {
        let (manager, base) = manager();
        let path = base.join("factory").join("hooks.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = serde_json::json!({"SessionStart": {"unexpected": "shape"}}).to_string();
        fs::write(&path, &original).unwrap();

        assert_eq!(
            manager.status(Agent::Droid).unwrap().state,
            HookState::Unsupported
        );
        let install_result = manager.install(Agent::Droid).unwrap();
        assert_eq!(install_result.state, HookState::Unsupported);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn droid_uninstall_removes_only_hooklinesinker_groups() {
        let (manager, base) = manager();
        manager.install(Agent::Droid).unwrap();
        let path = base.join("factory").join("hooks.json");
        let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["PreToolUse"].as_array_mut().unwrap().push(serde_json::json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": "~/.factory/hooks/user/lint.sh", "timeout": 5}]
        }));
        fs::write(&path, value.to_string()).unwrap();

        let status = manager.uninstall(Agent::Droid).unwrap();
        assert_eq!(status.state, HookState::Missing);

        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let pre_tool_use = value["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 1);
        assert_eq!(pre_tool_use[0]["matcher"], "Bash");
    }

    #[test]
    fn qwen_install_creates_all_thirteen_events_with_millisecond_timeouts() {
        let (manager, base) = manager();
        let status = manager.install(Agent::Qwen).unwrap();
        assert_eq!(status.state, HookState::Installed);
        assert_eq!(status.entries.len(), 13);

        let path = base.join("qwen").join("settings.json");
        let value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let session_start = &value["hooks"]["SessionStart"][0];
        assert_eq!(
            session_start["hooks"][0]["command"],
            format!(
                "{} ingest --agent qwen --event SessionStart",
                base.join("bin").join("hooklinesinker").display()
            )
        );
        assert_eq!(session_start["hooks"][0]["timeout"], 5000);
        let session_end = &value["hooks"]["SessionEnd"][0];
        assert_eq!(session_end["hooks"][0]["timeout"], 3000);
        let pre_tool_use = &value["hooks"]["PreToolUse"][0];
        assert_eq!(pre_tool_use["matcher"], "*");
    }

    #[test]
    fn qwen_install_preserves_unrelated_top_level_keys_and_matcher_groups() {
        let (manager, base) = manager();
        let settings_path = base.join("qwen").join("settings.json");
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(
            &settings_path,
            serde_json::json!({
                "hooks": {
                    "PreToolUse": [
                        {"matcher": "Bash", "hooks": [{"type": "command", "command": "~/.qwen/hooks/user/lint.sh", "timeout": 5000}]}
                    ]
                },
                "disableAllHooks": false
            })
            .to_string(),
        )
        .unwrap();

        let status = manager.install(Agent::Qwen).unwrap();
        assert_eq!(status.state, HookState::Installed);

        let value: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(value["disableAllHooks"], false);
        let pre_tool_use = value["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 2);
        assert_eq!(pre_tool_use[0]["matcher"], "Bash");
    }

    #[test]
    fn a_second_qwen_install_is_byte_for_byte_idempotent() {
        let (manager, base) = manager();
        manager.install(Agent::Qwen).unwrap();
        let path = base.join("qwen").join("settings.json");
        let first = fs::read(&path).unwrap();

        manager.install(Agent::Qwen).unwrap();
        let second = fs::read(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn qwen_status_reports_missing_installed_and_drifted() {
        let (manager, base) = manager();
        assert_eq!(
            manager.status(Agent::Qwen).unwrap().state,
            HookState::Missing
        );

        manager.install(Agent::Qwen).unwrap();
        assert_eq!(
            manager.status(Agent::Qwen).unwrap().state,
            HookState::Installed
        );

        let path = base.join("qwen").join("settings.json");
        let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["hooks"]["SessionStart"] = serde_json::json!([]);
        fs::write(&path, value.to_string()).unwrap();
        assert_eq!(
            manager.status(Agent::Qwen).unwrap().state,
            HookState::Drifted
        );
    }

    #[test]
    fn qwen_uninstall_removes_only_hooklinesinker_groups() {
        let (manager, base) = manager();
        manager.install(Agent::Qwen).unwrap();
        let path = base.join("qwen").join("settings.json");
        let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["hooks"]["PreToolUse"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": "~/.qwen/hooks/user/lint.sh", "timeout": 5000}]
            }));
        fs::write(&path, value.to_string()).unwrap();

        let status = manager.uninstall(Agent::Qwen).unwrap();
        assert_eq!(status.state, HookState::Missing);

        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let pre_tool_use = value["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 1);
        assert_eq!(pre_tool_use[0]["matcher"], "Bash");
    }

    #[test]
    fn kimi_install_creates_fourteen_entries_with_the_strict_three_field_shape() {
        let (manager, base) = manager();
        let status = manager.install(Agent::Kimi).unwrap();
        assert_eq!(status.state, HookState::Installed);
        assert_eq!(status.entries.len(), 14);

        let path = base.join("kimi-code").join("config.toml");
        let text = fs::read_to_string(&path).unwrap();
        let doc: DocumentMut = text.parse().unwrap();
        let array = doc["hooks"].as_array_of_tables().unwrap();
        assert_eq!(array.iter().count(), 14);
        for table in array {
            let mut keys: Vec<&str> = table.iter().map(|(k, _)| k).collect();
            keys.sort_unstable();
            assert_eq!(keys, vec!["command", "event", "timeout"]);
        }
        let session_end = array
            .iter()
            .find(|t| t.get("event").and_then(Item::as_str) == Some("SessionEnd"))
            .unwrap();
        assert_eq!(
            session_end.get("timeout").and_then(Item::as_integer),
            Some(3)
        );
        let session_start = array
            .iter()
            .find(|t| t.get("event").and_then(Item::as_str) == Some("SessionStart"))
            .unwrap();
        assert_eq!(
            session_start.get("timeout").and_then(Item::as_integer),
            Some(5)
        );
        assert_eq!(
            session_start.get("command").and_then(Item::as_str),
            Some(
                format!(
                    "{} ingest --agent kimi --event SessionStart",
                    base.join("bin").join("hooklinesinker").display()
                )
                .as_str()
            )
        );
    }

    #[test]
    fn kimi_install_preserves_unrelated_toml_content_byte_for_byte() {
        let (manager, base) = manager();
        let path = base.join("kimi-code").join("config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "# a user comment\n[oauth]\nprovider = \"default\" # inline comment\n\n[[hooks]]\nevent = \"Stop\"\nmatcher = \"exit\"\ncommand = \"~/bin/notify Stop\"\n";
        fs::write(&path, original).unwrap();

        let status = manager.install(Agent::Kimi).unwrap();
        assert_eq!(status.state, HookState::Installed);

        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# a user comment\n[oauth]\nprovider = \"default\" # inline comment")
        );
        assert!(
            text.contains("event = \"Stop\"\nmatcher = \"exit\"\ncommand = \"~/bin/notify Stop\"")
        );

        let doc: DocumentMut = text.parse().unwrap();
        let array = doc["hooks"].as_array_of_tables().unwrap();
        assert_eq!(array.iter().count(), 15);
    }

    #[test]
    fn a_second_kimi_install_is_byte_for_byte_idempotent() {
        let (manager, base) = manager();
        let path = base.join("kimi-code").join("config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "# kept\n[[hooks]]\nevent = \"Stop\"\ncommand = \"~/bin/notify Stop\"\n",
        )
        .unwrap();

        manager.install(Agent::Kimi).unwrap();
        let first = fs::read(&path).unwrap();

        manager.install(Agent::Kimi).unwrap();
        let second = fs::read(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn kimi_status_reports_missing_installed_and_drifted() {
        let (manager, base) = manager();
        assert_eq!(
            manager.status(Agent::Kimi).unwrap().state,
            HookState::Missing
        );

        manager.install(Agent::Kimi).unwrap();
        assert_eq!(
            manager.status(Agent::Kimi).unwrap().state,
            HookState::Installed
        );

        let path = base.join("kimi-code").join("config.toml");
        let text = fs::read_to_string(&path).unwrap();
        let mut doc: DocumentMut = text.parse().unwrap();
        {
            let array = doc["hooks"].as_array_of_tables_mut().unwrap();
            let removed = array.remove(0);
            assert!(removed.contains_key("event"));
        }
        fs::write(&path, doc.to_string()).unwrap();
        assert_eq!(
            manager.status(Agent::Kimi).unwrap().state,
            HookState::Drifted
        );
    }

    #[test]
    fn kimi_status_rejects_duplicate_owned_entries() {
        let (manager, base) = manager();
        manager.install(Agent::Kimi).unwrap();
        let path = base.join("kimi-code").join("config.toml");
        let text = fs::read_to_string(&path).unwrap();
        let mut doc: DocumentMut = text.parse().unwrap();
        let duplicate = doc["hooks"]
            .as_array_of_tables()
            .unwrap()
            .iter()
            .next()
            .unwrap()
            .clone();
        doc["hooks"]
            .as_array_of_tables_mut()
            .unwrap()
            .push(duplicate);
        fs::write(&path, doc.to_string()).unwrap();

        assert_eq!(
            manager.status(Agent::Kimi).unwrap().state,
            HookState::Drifted
        );
    }

    #[test]
    fn kimi_status_reports_unsupported_when_hooks_is_not_an_array_of_tables() {
        let (manager, base) = manager();
        let path = base.join("kimi-code").join("config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = "[hooks]\nevent = \"nope\"\n";
        fs::write(&path, original).unwrap();

        assert_eq!(
            manager.status(Agent::Kimi).unwrap().state,
            HookState::Unsupported
        );
        let install_result = manager.install(Agent::Kimi).unwrap();
        assert_eq!(install_result.state, HookState::Unsupported);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn kimi_uninstall_removes_only_exact_canonical_matches_and_keeps_foreign_entries() {
        let (manager, base) = manager();
        let path = base.join("kimi-code").join("config.toml");
        manager.install(Agent::Kimi).unwrap();
        let mut value = fs::read_to_string(&path).unwrap();
        value.push_str("\n[[hooks]]\nevent = \"Stop\"\ncommand = \"~/bin/notify Stop\"\n");
        fs::write(&path, value).unwrap();

        let status = manager.uninstall(Agent::Kimi).unwrap();
        assert_eq!(status.state, HookState::Missing);

        let text = fs::read_to_string(&path).unwrap();
        let doc: DocumentMut = text.parse().unwrap();
        let array = doc["hooks"].as_array_of_tables().unwrap();
        assert_eq!(array.iter().count(), 1);
        assert_eq!(
            array
                .iter()
                .next()
                .unwrap()
                .get("command")
                .and_then(Item::as_str),
            Some("~/bin/notify Stop")
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
        fs::write(&legacy_path, "// Juggler plugin for OpenCode\n").unwrap();

        manager.install(Agent::Opencode).unwrap();
        assert!(!legacy_path.exists());
    }

    #[test]
    fn opencode_install_preserves_a_foreign_legacy_filename() {
        let (manager, base) = manager();
        let legacy_path = base
            .join("opencode")
            .join("plugins")
            .join("juggler-opencode.ts");
        fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        fs::write(&legacy_path, "// user plugin\n").unwrap();

        manager.install(Agent::Opencode).unwrap();
        assert_eq!(fs::read_to_string(legacy_path).unwrap(), "// user plugin\n");
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
    fn concurrent_hook_installs_serialize_without_torn_configuration() {
        let (manager, base) = manager();
        let manager = std::sync::Arc::new(manager);
        let workers = 12;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(workers));
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let manager = std::sync::Arc::clone(&manager);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    manager.install(Agent::Claude)
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap().unwrap().state, HookState::Installed);
        }

        let status = manager.status(Agent::Claude).unwrap();
        assert_eq!(status.state, HookState::Installed);
        let value: Value =
            serde_json::from_slice(&fs::read(manager.claude_settings_path()).unwrap()).unwrap();
        assert_eq!(
            value["hooks"].as_object().unwrap().len(),
            native_event_specs(Agent::Claude).len()
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn command_paths_are_shell_quoted_and_remain_recognizable() {
        let path = Path::new("/Applications/Hook line's/bin/hooklinesinker");
        let command = canonical_command(path, Agent::Claude, "Stop");
        assert_eq!(
            command,
            "'/Applications/Hook line'\\''s/bin/hooklinesinker' ingest --agent claude --event Stop"
        );
        assert!(matches!(
            classify_command(&command, &command, Agent::Claude, "Stop"),
            GroupOwnership::Ours { exact: true }
        ));

        let moved = canonical_command(
            Path::new("/Applications/Older Hook/hooklinesinker"),
            Agent::Claude,
            "Stop",
        );
        assert!(matches!(
            classify_command(&moved, &command, Agent::Claude, "Stop"),
            GroupOwnership::Ours { exact: false }
        ));
    }

    #[test]
    fn generated_typescript_uses_a_complete_string_literal_escape() {
        let path = Path::new("/tmp/hook\"line\n\u{2028}sinker");
        let hooks = TypeScriptHooks { binary_path: path };
        let content = hooks.generated_content("const bin = \"__HOOKLINESINKER_BIN__\";");
        let encoded = serde_json::to_string(&path.display().to_string()).unwrap();
        assert!(content.contains(&format!("const bin = {encoded};")));
    }

    #[test]
    fn last_consumer_uninstall_removes_hooks_for_every_agent() {
        let (manager, base) = manager();
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
            manager.install(agent).unwrap();
        }
        for agent in agents {
            manager.uninstall(agent).unwrap();
            assert_eq!(manager.status(agent).unwrap().state, HookState::Missing);
        }
        let _ = base;
    }
    #[test]
    fn reconciliation_preserves_compound_commands_while_replacing_old_paths() {
        for install in [true, false] {
            let (manager, _) = manager();
            let old = canonical_command(Path::new("/old/hooklinesinker"), Agent::Claude, "Stop");
            let legacy = "~/.claude/hooks/juggler/notify.sh Stop";
            let commands = [
                format!("policy-check && {old}"),
                format!("policy-check&&{old}"),
                format!("policy-check;{old}"),
                format!("policy-check\n{old}"),
                format!("env FLAG=1 {old}"),
                format!("echo {old}"),
                format!("{old} && audit-hook"),
                format!("{old} >audit.log"),
                format!("policy-check && {legacy}"),
                format!("{legacy} && audit-hook"),
                format!("echo {legacy}"),
                "/old/not-hooklinesinker ingest --agent claude --event Stop".to_string(),
                "audit-hook".to_string(),
            ];
            let foreign: Vec<_> = commands
                .iter()
                .map(|command| serde_json::json!({"type": "command", "command": command}))
                .collect();
            let mut handlers = foreign.clone();
            handlers.push(serde_json::json!({"type": "command", "command": old}));
            handlers.push(serde_json::json!({"type": "command", "command": legacy}));
            let original = serde_json::json!({"hooks": {"Stop": [{"hooks": handlers}]}});
            let path = manager.claude_settings_path();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, original.to_string()).unwrap();

            if install {
                manager.install(Agent::Claude).unwrap();
            } else {
                manager.uninstall(Agent::Claude).unwrap();
            }

            let result: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                result["hooks"]["Stop"][0]["hooks"],
                serde_json::json!(foreign)
            );
            assert_eq!(
                result["hooks"]["Stop"].as_array().unwrap().len(),
                if install { 2 } else { 1 }
            );
            if install {
                assert_eq!(
                    result["hooks"]["Stop"][1]["hooks"][0]["command"],
                    canonical_command(&manager.roots.binary_path, Agent::Claude, "Stop")
                );
            }
        }
    }

    #[test]
    fn reconciliation_preserves_foreign_handlers_in_owned_and_legacy_groups() {
        for install in [true, false] {
            let (manager, _) = manager();
            let canonical = canonical_command(&manager.roots.binary_path, Agent::Claude, "Stop");
            let foreign = serde_json::json!([
                {"type": "prompt", "prompt": "Keep this prompt"},
                {"type": "agent", "prompt": "Keep this agent"},
                {"future": "handler", "command": canonical}
            ]);
            let mut handlers = foreign.as_array().unwrap().clone();
            handlers.push(serde_json::json!({"type": "command", "command": canonical}));
            handlers.push(serde_json::json!({"type": "command", "command": "~/.claude/hooks/juggler/notify.sh Stop"}));
            let original = serde_json::json!({"hooks": {"Stop": [{"hooks": handlers, "custom": "keep group metadata"}]}});
            let path = manager.claude_settings_path();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, original.to_string()).unwrap();
            let status = if install {
                manager.install(Agent::Claude)
            } else {
                manager.uninstall(Agent::Claude)
            }
            .unwrap();
            assert_eq!(
                status.state,
                if install {
                    HookState::Installed
                } else {
                    HookState::Missing
                }
            );
            let result: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(result["hooks"]["Stop"][0]["hooks"], foreign);
            assert_eq!(result["hooks"]["Stop"][0]["custom"], "keep group metadata");
            assert_eq!(
                result["hooks"]["Stop"].as_array().unwrap().len(),
                if install { 2 } else { 1 }
            );
        }
    }
}
