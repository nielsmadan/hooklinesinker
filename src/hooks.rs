use crate::agents::{HookTarget, profile};
#[cfg(test)]
use crate::events::native_event_specs;
use crate::events::{EventSpec, subscribed_event_specs};
use crate::persistence::{LockGuard, write_preserving_atomic_if_unchanged, write_private_atomic};
use crate::protocol::{Agent, Capability};
pub(crate) use crate::protocol::{HookEntry, HookState, HookStatus};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value as toml_value};

const MARKER_PREFIX: &str = "// hooklinesinker-generated protocol=";
const BIN_PLACEHOLDER: &str = "\"__HOOKLINESINKER_BIN__\"";
const BIN_EXPRESSION: &str = "join(homedir(), \"__HOOKLINESINKER_BIN__\")";

struct AdapterTemplate {
    source: &'static str,
    // The absolute rendering swaps these so it ships no import only `BIN_EXPRESSION` uses.
    home_imports: &'static str,
    absolute_imports: &'static str,
}

const OPENCODE_ADAPTER: AdapterTemplate = AdapterTemplate {
    source: include_str!("../adapters/opencode-hooklinesinker.ts"),
    home_imports: "import { homedir } from \"node:os\";\nimport { join } from \"node:path\";\n",
    absolute_imports: "",
};

const PI_ADAPTER: AdapterTemplate = AdapterTemplate {
    source: include_str!("../adapters/pi-hooklinesinker.ts"),
    home_imports: "import { homedir, tmpdir } from \"node:os\";",
    absolute_imports: "import { tmpdir } from \"node:os\";",
};

fn empty_status(
    agent: Agent,
    state: HookState,
    path: &Path,
    reason: Option<&'static str>,
) -> HookStatus {
    HookStatus {
        agent,
        state,
        path: path.to_path_buf(),
        entries: Vec::new(),
        reason,
    }
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
    pub home: PathBuf,
}

impl HookRoots {
    pub(crate) fn from_env(binary_path: PathBuf) -> io::Result<Self> {
        let defaults = Self::under_home(crate::paths::home_dir()?, binary_path);
        let opencode_config_dir =
            if let Some(path) = crate::paths::absolute_env_path("OPENCODE_CONFIG_DIR")? {
                path
            } else if let Some(path) = crate::paths::absolute_env_path("XDG_CONFIG_HOME")? {
                path.join("opencode")
            } else {
                defaults.opencode_config_dir.clone()
            };
        let pi_agent_dir = crate::paths::absolute_env_path("PI_CODING_AGENT_DIR")?
            .unwrap_or_else(|| defaults.pi_agent_dir.clone());
        let qwen_config_dir = crate::paths::absolute_env_path("QWEN_HOME")?
            .unwrap_or_else(|| defaults.qwen_config_dir.clone());
        let kimi_code_dir = crate::paths::absolute_env_path("KIMI_CODE_HOME")?
            .unwrap_or_else(|| defaults.kimi_code_dir.clone());
        Ok(Self {
            opencode_config_dir,
            pi_agent_dir,
            qwen_config_dir,
            kimi_code_dir,
            ..defaults
        })
    }

    pub(crate) fn under_home(home: PathBuf, binary_path: PathBuf) -> Self {
        Self {
            claude_dir: home.join(".claude"),
            codex_dir: home.join(".codex"),
            opencode_config_dir: home.join(".config/opencode"),
            pi_agent_dir: home.join(".pi/agent"),
            factory_dir: home.join(".factory"),
            qwen_config_dir: home.join(".qwen"),
            kimi_code_dir: home.join(".kimi-code"),
            binary_path,
            home,
        }
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
        template: &'static AdapterTemplate,
    },
}

impl HookManager {
    pub(crate) const fn new(roots: HookRoots) -> Self {
        Self { roots }
    }

    pub(crate) fn install(&self, agent: Agent) -> io::Result<HookStatus> {
        let backend = self.backend(agent);
        let _guard = LockGuard::acquire_for(backend.path())?;
        let events = subscribed_event_specs(agent, Capability::ALL);
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
        let events = subscribed_event_specs(agent, Capability::ALL);
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
        let events = subscribed_event_specs(agent, Capability::ALL);
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
                template: &OPENCODE_ADAPTER,
            },
            HookTarget::PiTypeScript => HookBackend::TypeScript {
                path: self.pi_extension_path(),
                legacy_path: self.pi_legacy_path(),
                template: &PI_ADAPTER,
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
            binary: self.binary_location(),
        }
    }

    fn toml_hooks(&self) -> TomlHooks<'_> {
        TomlHooks {
            binary_path: &self.roots.binary_path,
        }
    }

    fn typescript_hooks(&self) -> TypeScriptHooks<'_> {
        TypeScriptHooks {
            binary: self.binary_location(),
        }
    }

    fn binary_location(&self) -> BinaryLocation<'_> {
        BinaryLocation {
            path: &self.roots.binary_path,
            home: &self.roots.home,
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

// A home-relative spelling of the active binary is as installed as the absolute one where
// the host expands it: a shell for commands, `homedir()` for adapters.
#[derive(Clone, Copy)]
struct BinaryLocation<'a> {
    path: &'a Path,
    home: &'a Path,
}

struct JsonHooks<'a> {
    binary: BinaryLocation<'a>,
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
            return Ok(empty_status(agent, HookState::Missing, path, None));
        }
        let original_root = original_bytes
            .as_deref()
            .map(|bytes| parse_json_root(path, bytes))
            .transpose()?
            .unwrap_or_default();
        let mut root = original_root.clone();

        let Some(hooks) = hook_map_mut(&mut root, location) else {
            return Ok(empty_status(
                agent,
                HookState::Unsupported,
                path,
                Some("the hooks key is not an object"),
            ));
        };

        if managed_event_shape_is_unsupported(hooks, events) {
            return Ok(empty_status(
                agent,
                HookState::Unsupported,
                path,
                Some("a managed event's value is not an array"),
            ));
        }

        remove_legacy_handlers(hooks);
        reconcile_managed_events(hooks, events, mode, self.binary, agent);
        // Reconciliation may empty a group it owns, but a foreign empty array is the user's.
        hooks.retain(|event, value| {
            !matches!(value, Value::Array(items) if items.is_empty())
                || !events.iter().any(|spec| spec.name == event)
        });
        if root == original_root {
            return self.status(agent, path, events, location);
        }
        let mut bytes = serde_json::to_vec_pretty(&Value::Object(root))
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
            return Ok(empty_status(agent, HookState::Missing, path, None));
        };

        let hooks = match location {
            HooksLocation::Nested(key) => match root.get(key) {
                None => {
                    return Ok(empty_status(agent, HookState::Missing, path, None));
                }
                Some(Value::Object(map)) => map,
                Some(_) => {
                    return Ok(empty_status(
                        agent,
                        HookState::Unsupported,
                        path,
                        Some("the hooks key is not an object"),
                    ));
                }
            },
            HooksLocation::TopLevel => &root,
        };

        if managed_event_shape_is_unsupported(hooks, events) {
            return Ok(empty_status(
                agent,
                HookState::Unsupported,
                path,
                Some("a managed event's value is not an array"),
            ));
        }

        let mut entries = Vec::new();
        let mut installed_events: HashSet<&str> = HashSet::new();

        for spec in events {
            let groups = match hooks.get(spec.name) {
                Some(Value::Array(groups)) => groups.as_slice(),
                _ => &[],
            };
            for (index, group) in groups.iter().enumerate() {
                if let Some(command) = owned_group_command(group, self.binary, agent, spec.name) {
                    entries.push(HookEntry {
                        event: spec.name.to_string(),
                        group_index: index,
                        command: command.to_string(),
                    });
                }
            }
            if event_is_installed(groups, spec, self.binary, agent) {
                installed_events.insert(spec.name);
            }
        }

        let state = if installed_events.len() == events.len() {
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
            reason: None,
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
            return Ok(empty_status(agent, HookState::Missing, path, None));
        }
        if matches!(mode, ReconcileMode::Install) {
            let current = self.status(agent, path, events)?;
            if current.state == HookState::Installed {
                ensure_safe_parent_dir(path)?;
                return Ok(current);
            }
        }

        let mut doc = parse_toml_document(path, existing.as_deref().unwrap_or(""))?;

        if let Some(item) = doc.get("hooks")
            && !item.is_array_of_tables()
        {
            return Ok(empty_status(
                agent,
                HookState::Unsupported,
                path,
                Some("the hooks key is not an array of tables"),
            ));
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
                return Ok(empty_status(agent, HookState::Missing, path, None));
            }
            Err(e) => return Err(e),
        };
        let doc = parse_toml_document(path, &text)?;

        let array = match doc.get("hooks") {
            None => {
                return Ok(empty_status(agent, HookState::Missing, path, None));
            }
            Some(item) => match item.as_array_of_tables() {
                Some(array) => array,
                None => {
                    return Ok(empty_status(
                        agent,
                        HookState::Unsupported,
                        path,
                        Some("the hooks key is not an array of tables"),
                    ));
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
            reason: None,
        })
    }
}

struct TypeScriptHooks<'a> {
    binary: BinaryLocation<'a>,
}

impl TypeScriptHooks<'_> {
    fn generated_content(&self, template: &AdapterTemplate) -> String {
        let source = template
            .source
            .replace(template.home_imports, template.absolute_imports);
        render_adapter(&source, &string_literal(self.binary.path))
    }

    fn home_relative_content(&self, template: &AdapterTemplate) -> Option<String> {
        let relative = self.binary.path.strip_prefix(self.binary.home).ok()?;
        let bin_expression = BIN_EXPRESSION.replace(BIN_PLACEHOLDER, &string_literal(relative));
        Some(render_adapter(template.source, &bin_expression))
    }

    fn status(
        &self,
        agent: Agent,
        path: &Path,
        template: &AdapterTemplate,
    ) -> io::Result<HookStatus> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(empty_status(agent, HookState::Missing, path, None));
            }
            Err(e) => return Err(e),
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if !text.starts_with(MARKER_PREFIX) {
            return Ok(empty_status(
                agent,
                HookState::Unsupported,
                path,
                Some("the file is not one this tool generated"),
            ));
        }
        let state = if text == self.generated_content(template)
            || self.home_relative_content(template).as_deref() == Some(text.as_str())
        {
            HookState::Installed
        } else {
            HookState::Drifted
        };
        Ok(HookStatus {
            agent,
            state,
            path: path.to_path_buf(),
            entries: Vec::new(),
            reason: None,
        })
    }

    fn install(
        &self,
        agent: Agent,
        path: &Path,
        template: &AdapterTemplate,
        legacy_path: &Path,
    ) -> io::Result<HookStatus> {
        remove_legacy_generated_file(legacy_path)?;
        let current = self.status(agent, path, template)?;
        match current.state {
            HookState::Unsupported => return Ok(current),
            HookState::Installed if !is_group_or_world_writable(path)? => {
                ensure_safe_parent_dir(path)?;
                return Ok(current);
            }
            _ => {}
        }
        let contents = self.generated_content(template);
        write_generated_file(path, &contents)?;
        self.status(agent, path, template)
    }

    fn uninstall(
        &self,
        agent: Agent,
        path: &Path,
        template: &AdapterTemplate,
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

fn hook_map_mut(
    root: &mut Map<String, Value>,
    location: HooksLocation,
) -> Option<&mut Map<String, Value>> {
    match location {
        HooksLocation::Nested(key) => root
            .entry(key)
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut(),
        HooksLocation::TopLevel => Some(root),
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
    binary: BinaryLocation<'_>,
    agent: Agent,
) {
    for spec in events {
        if matches!(mode, ReconcileMode::Install)
            && hooks
                .get(spec.name)
                .and_then(Value::as_array)
                .is_some_and(|groups| event_is_installed(groups, spec, binary, agent))
        {
            continue;
        }
        let groups = match hooks.get_mut(spec.name) {
            None => Vec::new(),
            Some(Value::Array(groups)) => std::mem::take(groups),
            Some(_) => {
                unreachable!("non-array managed event values are rejected before reconciliation")
            }
        };
        let mut kept: Vec<Value> = groups
            .into_iter()
            .filter_map(|group| {
                retain_foreign_handlers(group, |command| {
                    matches!(
                        classify_command(command, binary, agent, spec.name),
                        GroupOwnership::Ours { .. }
                    )
                })
            })
            .collect();
        if matches!(mode, ReconcileMode::Install) {
            let canonical = canonical_command(binary.path, agent, spec.name);
            kept.push(build_group(spec, &canonical, agent));
        }
        // Inserting over the existing key keeps its position in the document.
        hooks.insert(spec.name.to_string(), Value::Array(kept));
    }
}

fn event_is_installed(
    groups: &[Value],
    spec: &EventSpec,
    binary: BinaryLocation<'_>,
    agent: Agent,
) -> bool {
    let owned_handlers = groups
        .iter()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(handler_command)
        .filter(|command| {
            matches!(
                classify_command(command, binary, agent, spec.name),
                GroupOwnership::Ours { .. }
            )
        })
        .count();
    owned_handlers == 1
        && groups
            .iter()
            .any(|group| group_is_installed(group, spec, binary, agent))
}

fn group_is_installed(
    group: &Value,
    spec: &EventSpec,
    binary: BinaryLocation<'_>,
    agent: Agent,
) -> bool {
    single_handler_command(group).is_some_and(|command| {
        matches!(
            classify_command(command, binary, agent, spec.name),
            GroupOwnership::Ours { exact: true }
        ) && group == &build_group(spec, command, agent)
    })
}

fn owned_group_command<'a>(
    group: &'a Value,
    binary: BinaryLocation<'_>,
    agent: Agent,
    event: &str,
) -> Option<&'a str> {
    single_handler_command(group).filter(|command| {
        matches!(
            classify_command(command, binary, agent, event),
            GroupOwnership::Ours { .. }
        )
    })
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

fn single_handler_command(group: &Value) -> Option<&str> {
    match group.get("hooks").and_then(Value::as_array)?.as_slice() {
        [handler] => handler_command(handler),
        _ => None,
    }
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

fn classify_command(
    command: &str,
    binary: BinaryLocation<'_>,
    agent: Agent,
    event: &str,
) -> GroupOwnership {
    let suffix = format!(" ingest --agent {} --event {}", agent.as_str(), event);
    let home = profile(agent)
        .hook_target
        .accepts_home_relative_commands()
        .then_some(binary.home);
    let Some(path) = command
        .strip_suffix(&suffix)
        .and_then(|word| resolve_command_binary(word, home))
    else {
        return GroupOwnership::Foreign;
    };
    if path == binary.path {
        GroupOwnership::Ours { exact: true }
    } else if path.file_name() == Some(std::ffi::OsStr::new("hooklinesinker")) {
        GroupOwnership::Ours { exact: false }
    } else {
        GroupOwnership::Foreign
    }
}

fn resolve_command_binary(word: &str, home: Option<&Path>) -> Option<PathBuf> {
    let home_relative = home.and_then(|home| {
        ["~/", "$HOME/"]
            .into_iter()
            .find_map(|prefix| word.strip_prefix(prefix))
            .map(|rest| (home, rest))
    });
    home_relative.map_or_else(
        || decode_shell_word(word).map(PathBuf::from),
        |(home, rest)| decode_shell_word(rest).map(|rest| home.join(rest)),
    )
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
    ensure_safe_parent_dir(path)?;
    write_preserving_atomic_if_unchanged(path, expected, bytes)
}

fn write_generated_file(path: &Path, contents: &str) -> io::Result<()> {
    ensure_safe_parent_dir(path)?;
    write_private_atomic(path, contents.as_bytes())
}

fn ensure_safe_parent_dir(path: &Path) -> io::Result<()> {
    path.parent().map_or(Ok(()), ensure_safe_config_dir)
}

fn is_group_or_world_writable(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(path)?.permissions().mode() & 0o022 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

fn render_adapter(template: &str, bin_expression: &str) -> String {
    let marker = format!("{MARKER_PREFIX}{}\n", crate::protocol::PROTOCOL_VERSION);
    format!(
        "{marker}{}",
        template.replace(BIN_EXPRESSION, bin_expression)
    )
}

fn string_literal(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string()).expect("a path string always serializes")
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
            home: base.join("home"),
        };
        (HookManager::new(roots), base)
    }

    #[test]
    fn claude_install_on_a_missing_file_creates_all_twelve_events() {
        let (manager, base) = manager();
        let status = manager.install(Agent::Claude).unwrap();
        assert_eq!(status.state, HookState::Installed);
        assert_eq!(status.entries.len(), 12);

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
        let model_switch = &value["hooks"]["PostModelSwitch"][0];
        assert!(model_switch.get("matcher").is_none());
        assert_eq!(model_switch["hooks"][0]["timeout"], 5);
    }

    #[test]
    fn claude_install_without_post_model_switch_is_drifted() {
        let (manager, base) = manager();
        manager.install(Agent::Claude).unwrap();
        let path = base.join("claude").join("settings.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("PostModelSwitch");
        fs::write(&path, value.to_string()).unwrap();
        let status = manager.status(Agent::Claude).unwrap();
        assert_eq!(status.state, HookState::Drifted);
        assert_eq!(status.entries.len(), 11);
        let repaired = manager.install(Agent::Claude).unwrap();
        assert_eq!(repaired.state, HookState::Installed);
        assert_eq!(repaired.entries.len(), 12);
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
        assert_eq!(status.entries.len(), 12);

        let path = base.join("codex").join("hooks.json");
        let value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let session_start = &value["hooks"]["SessionStart"][0];
        assert!(session_start.get("matcher").is_none());
        assert_eq!(session_start["hooks"][0]["timeout"], 5);
        let session_end = &value["hooks"]["SessionEnd"][0];
        assert_eq!(session_end["hooks"][0]["timeout"], 3);
        let subagent_start = &value["hooks"]["SubagentStart"][0];
        assert!(subagent_start.get("matcher").is_none());
        assert_eq!(subagent_start["hooks"][0]["timeout"], 5);
        let subagent_stop = &value["hooks"]["SubagentStop"][0];
        assert!(subagent_stop.get("matcher").is_none());
        assert_eq!(subagent_stop["hooks"][0]["timeout"], 5);
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
    fn adapter_templates_keep_the_binary_expression_verbatim() {
        // generated_content replaces the whole expression, and String::replace no-ops
        // silently, so a reworded template would ship an adapter spawning the placeholder.
        for (agent, template) in [("opencode", &OPENCODE_ADAPTER), ("pi", &PI_ADAPTER)] {
            assert!(
                template.source.contains(BIN_EXPRESSION),
                "{agent} adapter must keep {BIN_EXPRESSION} verbatim"
            );
            let generated = TypeScriptHooks {
                binary: BinaryLocation {
                    path: Path::new("/tmp/hooklinesinker"),
                    home: Path::new("/Users/someone"),
                },
            }
            .generated_content(template);
            assert!(
                !generated.contains("__HOOKLINESINKER_BIN__"),
                "{agent} adapter still carries the placeholder after substitution"
            );
            assert!(
                generated.contains("\"/tmp/hooklinesinker\""),
                "{agent} adapter is missing the substituted binary path"
            );
        }
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
        let binary = BinaryLocation {
            path,
            home: Path::new("/Users/someone"),
        };
        let command = canonical_command(path, Agent::Claude, "Stop");
        assert_eq!(
            command,
            "'/Applications/Hook line'\\''s/bin/hooklinesinker' ingest --agent claude --event Stop"
        );
        assert!(matches!(
            classify_command(&command, binary, Agent::Claude, "Stop"),
            GroupOwnership::Ours { exact: true }
        ));

        let moved = canonical_command(
            Path::new("/Applications/Older Hook/hooklinesinker"),
            Agent::Claude,
            "Stop",
        );
        assert!(matches!(
            classify_command(&moved, binary, Agent::Claude, "Stop"),
            GroupOwnership::Ours { exact: false }
        ));
    }

    #[test]
    fn generated_typescript_uses_a_complete_string_literal_escape() {
        let path = Path::new("/tmp/hook\"line\n\u{2028}sinker");
        let hooks = TypeScriptHooks {
            binary: BinaryLocation {
                path,
                home: Path::new("/Users/someone"),
            },
        };
        let template = AdapterTemplate {
            source: "const bin = join(homedir(), \"__HOOKLINESINKER_BIN__\");",
            home_imports: "",
            absolute_imports: "",
        };
        let content = hooks.generated_content(&template);
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
    fn droid_reconciliation_preserves_foreign_handlers_in_its_top_level_shape() {
        // Droid stores the same array-of-groups under a top-level event key instead of a
        // "hooks" wrapper, so the preservation logic meets a different root here.
        for install in [true, false] {
            let (manager, base) = manager();
            let canonical = canonical_command(&manager.roots.binary_path, Agent::Droid, "Stop");
            let foreign = serde_json::json!([
                {"type": "prompt", "prompt": "Keep this prompt"},
                {"future": "handler", "command": canonical}
            ]);
            let mut handlers = foreign.as_array().unwrap().clone();
            handlers.push(serde_json::json!({"type": "command", "command": canonical}));
            let original =
                serde_json::json!({"Stop": [{"hooks": handlers, "custom": "keep group metadata"}]});
            let path = base.join("factory").join("hooks.json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, original.to_string()).unwrap();

            let status = if install {
                manager.install(Agent::Droid)
            } else {
                manager.uninstall(Agent::Droid)
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
            assert_eq!(result["Stop"][0]["hooks"], foreign);
            assert_eq!(result["Stop"][0]["custom"], "keep group metadata");
            assert!(result.get("hooks").is_none(), "droid has no hooks wrapper");
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

    const HOME_BINARY: &str = ".local/share/hooklinesinker/bin/hooklinesinker";

    fn manager_with_home_binary() -> (HookManager, PathBuf) {
        let home = temp_root("home");
        let roots = HookRoots::under_home(home.clone(), home.join(HOME_BINARY));
        (HookManager::new(roots), home)
    }

    fn write_with_mode(path: &Path, contents: &[u8], mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn file_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn config_path(manager: &HookManager, agent: Agent) -> PathBuf {
        manager.backend(agent).path().to_path_buf()
    }

    // The shape an external config manager renders: home-relative commands with its own key
    // order and formatting.
    fn home_relative_config(manager: &HookManager, agent: Agent, prefix: &str) -> Vec<u8> {
        manager.install(agent).unwrap();
        let installed: Value =
            serde_json::from_slice(&fs::read(config_path(manager, agent)).unwrap()).unwrap();
        let mut root = Map::new();
        root.insert("zeta".to_string(), Value::from(1));
        root.extend(installed.as_object().unwrap().clone());
        root.insert("alpha".to_string(), Value::from(2));
        let absolute = manager.roots.binary_path.display().to_string();
        serde_json::to_string(&Value::Object(root))
            .unwrap()
            .replace(&absolute, &format!("{prefix}{HOME_BINARY}"))
            .into_bytes()
    }

    fn home_relative_adapter(template: &AdapterTemplate, relative: &str) -> String {
        render_adapter(template.source, &format!("join(homedir(), \"{relative}\")"))
    }

    #[test]
    fn home_relative_commands_count_as_installed_and_are_left_untouched() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Droid, Agent::Qwen] {
            for prefix in ["~/", "$HOME/"] {
                let (manager, home) = manager_with_home_binary();
                let path = config_path(&manager, agent);
                let original = home_relative_config(&manager, agent, prefix);
                write_with_mode(&path, &original, 0o644);

                let status = manager.status(agent).unwrap();
                assert_eq!(status.state, HookState::Installed, "{agent:?} {prefix}");
                assert_eq!(
                    status.entries.len(),
                    subscribed_event_specs(agent, Capability::ALL).len()
                );
                assert!(
                    status.entries.iter().all(|e| e.command.starts_with(prefix)),
                    "status must report the command as written"
                );

                assert_eq!(manager.install(agent).unwrap().state, HookState::Installed);
                assert_eq!(fs::read(&path).unwrap(), original, "{agent:?} {prefix}");
                assert_eq!(file_mode(&path), 0o644);
                fs::remove_dir_all(home).unwrap();
            }
        }
    }

    #[test]
    fn a_home_relative_command_for_another_binary_is_replaced() {
        let (manager, home) = manager_with_home_binary();
        let path = manager.claude_settings_path();
        let original = String::from_utf8(home_relative_config(&manager, Agent::Claude, "~/"))
            .unwrap()
            .replace("~/.local/share/", "~/old/");
        write_with_mode(&path, original.as_bytes(), 0o644);

        assert_eq!(
            manager.status(Agent::Claude).unwrap().state,
            HookState::Drifted
        );
        assert_eq!(
            manager.install(Agent::Claude).unwrap().state,
            HookState::Installed
        );
        let after = fs::read_to_string(&path).unwrap();
        assert!(!after.contains("~/old/"));
        assert!(after.contains(&manager.roots.binary_path.display().to_string()));
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn reconciling_a_drifted_file_keeps_key_order_and_file_mode() {
        let (manager, home) = manager_with_home_binary();
        let path = manager.claude_settings_path();
        let mut value: Value =
            serde_json::from_slice(&home_relative_config(&manager, Agent::Claude, "~/")).unwrap();
        let hooks = value["hooks"].as_object_mut().unwrap();
        let first_event = hooks.keys().next().unwrap().clone();
        let untouched_event = hooks.keys().nth(1).unwrap().clone();
        hooks[&first_event] = serde_json::json!([]);
        let user_event = serde_json::json!([{"hooks": [{"type": "command", "command": "true"}]}]);
        hooks.insert("AUserEvent".to_string(), user_event);
        let before_events: Vec<String> = hooks.keys().cloned().collect();
        write_with_mode(&path, value.to_string().as_bytes(), 0o644);

        assert_eq!(
            manager.install(Agent::Claude).unwrap().state,
            HookState::Installed
        );
        let after: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let root_keys: Vec<&String> = after.as_object().unwrap().keys().collect();
        assert_eq!(root_keys, ["zeta", "hooks", "alpha"]);
        let after_events: Vec<String> = after["hooks"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(after_events, before_events);
        assert_eq!(
            after["hooks"][&untouched_event][0]["hooks"][0]["command"],
            format!("~/{HOME_BINARY} ingest --agent claude --event {untouched_event}")
        );
        assert_eq!(file_mode(&path), 0o644);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_stale_owned_group_beside_the_installed_one_is_collapsed() {
        let (manager, home) = manager_with_home_binary();
        manager.install(Agent::Claude).unwrap();
        let path = manager.claude_settings_path();
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let stale = serde_json::json!({"hooks": [{
            "type": "command",
            "command": "/old/hooklinesinker ingest --agent claude --event Stop",
            "timeout": 5
        }]});
        value["hooks"]["Stop"].as_array_mut().unwrap().push(stale);
        fs::write(&path, value.to_string()).unwrap();

        assert_eq!(
            manager.status(Agent::Claude).unwrap().state,
            HookState::Drifted
        );
        assert_eq!(
            manager.install(Agent::Claude).unwrap().state,
            HookState::Installed
        );
        let after: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let stop = after["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            canonical_command(&manager.roots.binary_path, Agent::Claude, "Stop")
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_owned_handler_inside_a_mixed_group_is_drift_that_install_resolves() {
        let (manager, home) = manager_with_home_binary();
        manager.install(Agent::Claude).unwrap();
        let path = manager.claude_settings_path();
        let canonical = canonical_command(&manager.roots.binary_path, Agent::Claude, "Stop");
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mixed = serde_json::json!({"hooks": [
            {"type": "command", "command": "true"},
            {"type": "command", "command": canonical}
        ]});
        value["hooks"]["Stop"].as_array_mut().unwrap().push(mixed);
        fs::write(&path, value.to_string()).unwrap();

        assert_eq!(
            manager.status(Agent::Claude).unwrap().state,
            HookState::Drifted
        );
        assert_eq!(
            manager.install(Agent::Claude).unwrap().state,
            HookState::Installed
        );
        let after: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let stop = after["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop[0]["hooks"],
            serde_json::json!([{"type": "command", "command": "true"}])
        );
        assert_eq!(stop[1]["hooks"][0]["command"], canonical);
        assert_eq!(stop.len(), 2);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_installed_kimi_config_with_a_trailing_foreign_entry_is_left_untouched() {
        use std::os::unix::fs::MetadataExt;
        let (manager, home) = manager_with_home_binary();
        manager.install(Agent::Kimi).unwrap();
        let path = manager.kimi_config_path();
        let text = fs::read_to_string(&path).unwrap()
            + "\n[[hooks]]\nevent = \"Stop\"\ncommand = \"~/bin/notify Stop\"\ntimeout = 5\n";
        fs::write(&path, &text).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();

        assert_eq!(
            manager.install(Agent::Kimi).unwrap().state,
            HookState::Installed
        );
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_fresh_adapter_install_writes_the_absolute_binary_literal() {
        let (manager, home) = manager_with_home_binary();
        let literal = format!(
            "const HOOKLINESINKER_BIN: string = {};",
            string_literal(&manager.roots.binary_path)
        );
        for (agent, path) in [
            (Agent::Opencode, manager.opencode_plugin_path()),
            (Agent::Pi, manager.pi_extension_path()),
        ] {
            assert_eq!(manager.install(agent).unwrap().state, HookState::Installed);
            assert!(fs::read_to_string(&path).unwrap().contains(&literal));
            assert_eq!(file_mode(&path), 0o600);
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn the_absolute_adapter_rendering_imports_only_what_it_uses() {
        let (manager, home) = manager_with_home_binary();
        let literal = string_literal(&manager.roots.binary_path);
        let marker = format!("{MARKER_PREFIX}{}\n", crate::protocol::PROTOCOL_VERSION);
        let opencode_head = format!(
            "{marker}import {{ spawn }} from \"node:child_process\";\n\nconst HOOKLINESINKER_BIN: string = {literal};\n"
        );
        let pi_head = format!(
            "{marker}import {{ spawn }} from \"node:child_process\";\nimport {{ readdir, readFile }} from \"node:fs/promises\";\nimport {{ tmpdir }} from \"node:os\";\nimport {{ join, resolve as resolvePath }} from \"node:path\";\n\nconst HOOKLINESINKER_BIN: string = {literal};\n"
        );
        for (agent, path, template, head) in [
            (
                Agent::Opencode,
                manager.opencode_plugin_path(),
                &OPENCODE_ADAPTER,
                opencode_head,
            ),
            (Agent::Pi, manager.pi_extension_path(), &PI_ADAPTER, pi_head),
        ] {
            assert_eq!(template.source.matches(template.home_imports).count(), 1);
            manager.install(agent).unwrap();
            assert!(fs::read_to_string(&path).unwrap().starts_with(&head));
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_home_relative_adapter_is_installed_and_never_rewritten() {
        use std::os::unix::fs::MetadataExt;
        let (manager, home) = manager_with_home_binary();
        for (agent, path, template) in [
            (
                Agent::Opencode,
                manager.opencode_plugin_path(),
                &OPENCODE_ADAPTER,
            ),
            (Agent::Pi, manager.pi_extension_path(), &PI_ADAPTER),
        ] {
            let rendered = home_relative_adapter(template, HOME_BINARY);
            write_with_mode(&path, rendered.as_bytes(), 0o644);
            let inode = fs::metadata(&path).unwrap().ino();

            assert_eq!(manager.status(agent).unwrap().state, HookState::Installed);
            assert_eq!(manager.install(agent).unwrap().state, HookState::Installed);
            let after = fs::metadata(&path).unwrap();
            assert_eq!(after.ino(), inode);
            assert_eq!(fs::read_to_string(&path).unwrap(), rendered);
            assert_eq!(file_mode(&path), 0o644);
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_home_relative_adapter_for_another_binary_is_drifted() {
        let (manager, home) = manager_with_home_binary();
        for (agent, path, template) in [
            (
                Agent::Opencode,
                manager.opencode_plugin_path(),
                &OPENCODE_ADAPTER,
            ),
            (Agent::Pi, manager.pi_extension_path(), &PI_ADAPTER),
        ] {
            let rendered = home_relative_adapter(template, "old/hooklinesinker");
            write_with_mode(&path, rendered.as_bytes(), 0o644);
            assert_eq!(manager.status(agent).unwrap().state, HookState::Drifted);
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn a_home_relative_adapter_is_drifted_when_the_binary_lives_outside_home() {
        let base = temp_root("outside-home");
        let manager = HookManager::new(HookRoots::under_home(
            base.join("home"),
            base.join("opt/hooklinesinker"),
        ));
        let path = manager.opencode_plugin_path();
        let rendered = home_relative_adapter(&OPENCODE_ADAPTER, "../opt/hooklinesinker");
        write_with_mode(&path, rendered.as_bytes(), 0o644);

        assert_eq!(
            manager.status(Agent::Opencode).unwrap().state,
            HookState::Drifted
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_drifted_adapter_is_rewritten_privately() {
        let (manager, home) = manager_with_home_binary();
        for (agent, path) in [
            (Agent::Opencode, manager.opencode_plugin_path()),
            (Agent::Pi, manager.pi_extension_path()),
        ] {
            manager.install(agent).unwrap();
            let generated = fs::read(&path).unwrap();
            let drifted = String::from_utf8(generated.clone()).unwrap() + "// edited\n";
            write_with_mode(&path, drifted.as_bytes(), 0o666);

            assert_eq!(manager.install(agent).unwrap().state, HookState::Installed);
            assert_eq!(fs::read(&path).unwrap(), generated);
            assert_eq!(file_mode(&path), 0o600);
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_installed_but_shared_writable_adapter_is_rewritten_privately() {
        let (manager, home) = manager_with_home_binary();
        for (agent, path) in [
            (Agent::Opencode, manager.opencode_plugin_path()),
            (Agent::Pi, manager.pi_extension_path()),
        ] {
            manager.install(agent).unwrap();
            let generated = fs::read(&path).unwrap();
            write_with_mode(&path, &generated, 0o666);

            assert_eq!(manager.install(agent).unwrap().state, HookState::Installed);
            assert_eq!(fs::read(&path).unwrap(), generated);
            assert_eq!(file_mode(&path), 0o600);
        }
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn installing_into_a_writable_directory_is_refused_even_when_already_installed() {
        use std::os::unix::fs::PermissionsExt;
        let (manager, home) = manager_with_home_binary();
        for agent in [Agent::Opencode, Agent::Pi, Agent::Kimi] {
            manager.install(agent).unwrap();
            let dir = config_path(&manager, agent).parent().unwrap().to_path_buf();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();

            let error = manager.install(agent).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{agent:?}");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::remove_dir_all(home).unwrap();
    }
}
