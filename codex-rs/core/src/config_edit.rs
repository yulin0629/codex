use crate::config::CONFIG_TOML_FILE;
use crate::config_types::McpServerConfig;
use crate::config_types::Notice;
use anyhow::Context;
use codex_protocol::config_types::ReasoningEffort;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use tempfile::NamedTempFile;
use tokio::task;
use toml_edit::DocumentMut;
use toml_edit::Item as TomlItem;
use toml_edit::Table as TomlTable;
use toml_edit::value;

/// Discrete config mutations supported by the persistence engine.
#[derive(Clone, Debug)]
pub enum ConfigEdit {
    /// Update the active (or default) model selection and optional reasoning effort.
    SetModel {
        model: Option<String>,
        effort: Option<ReasoningEffort>,
    },
    /// Toggle the acknowledgement flag under `[notice]`.
    SetNoticeHideFullAccessWarning(bool),
    /// Toggle the Windows onboarding acknowledgement flag.
    SetWindowsWslSetupAcknowledged(bool),
    /// Replace the entire `[mcp_servers]` table.
    ReplaceMcpServers(BTreeMap<String, McpServerConfig>),
    /// Set trust_level = "trusted" under `[projects."<path>"]`,
    /// migrating inline tables to explicit tables.
    SetProjectTrusted(PathBuf),
    /// Set the value stored at the exact dotted path.
    SetPath {
        segments: Vec<String>,
        value: TomlItem,
    },
    /// Remove the value stored at the exact dotted path.
    ClearPath { segments: Vec<String> },
}

// TODO(jif) move to a dedicated file
mod document_helpers {
    use crate::config_types::McpServerConfig;
    use crate::config_types::McpServerTransportConfig;
    use toml_edit::Array as TomlArray;
    use toml_edit::InlineTable;
    use toml_edit::Item as TomlItem;
    use toml_edit::Table as TomlTable;
    use toml_edit::value;

    pub(super) fn ensure_table_for_write(item: &mut TomlItem) -> Option<&mut TomlTable> {
        match item {
            TomlItem::Table(table) => Some(table),
            TomlItem::Value(value) => {
                if let Some(inline) = value.as_inline_table() {
                    *item = TomlItem::Table(table_from_inline(inline));
                    item.as_table_mut()
                } else {
                    *item = TomlItem::Table(new_implicit_table());
                    item.as_table_mut()
                }
            }
            TomlItem::None => {
                *item = TomlItem::Table(new_implicit_table());
                item.as_table_mut()
            }
            _ => None,
        }
    }

    pub(super) fn ensure_table_for_read(item: &mut TomlItem) -> Option<&mut TomlTable> {
        match item {
            TomlItem::Table(table) => Some(table),
            TomlItem::Value(value) => {
                let inline = value.as_inline_table()?;
                *item = TomlItem::Table(table_from_inline(inline));
                item.as_table_mut()
            }
            _ => None,
        }
    }

    pub(super) fn serialize_mcp_server(config: &McpServerConfig) -> TomlItem {
        let mut entry = TomlTable::new();
        entry.set_implicit(false);

        match &config.transport {
            McpServerTransportConfig::Stdio {
                command,
                args,
                env,
                env_vars,
                cwd,
            } => {
                entry["command"] = value(command.clone());
                if !args.is_empty() {
                    entry["args"] = array_from_iter(args.iter().cloned());
                }
                if let Some(env) = env
                    && !env.is_empty()
                {
                    entry["env"] = table_from_pairs(env.iter());
                }
                if !env_vars.is_empty() {
                    entry["env_vars"] = array_from_iter(env_vars.iter().cloned());
                }
                if let Some(cwd) = cwd {
                    entry["cwd"] = value(cwd.to_string_lossy().to_string());
                }
            }
            McpServerTransportConfig::StreamableHttp {
                url,
                bearer_token_env_var,
                http_headers,
                env_http_headers,
            } => {
                entry["url"] = value(url.clone());
                if let Some(env_var) = bearer_token_env_var {
                    entry["bearer_token_env_var"] = value(env_var.clone());
                }
                if let Some(headers) = http_headers
                    && !headers.is_empty()
                {
                    entry["http_headers"] = table_from_pairs(headers.iter());
                }
                if let Some(headers) = env_http_headers
                    && !headers.is_empty()
                {
                    entry["env_http_headers"] = table_from_pairs(headers.iter());
                }
            }
        }

        if !config.enabled {
            entry["enabled"] = value(false);
        }
        if let Some(timeout) = config.startup_timeout_sec {
            entry["startup_timeout_sec"] = value(timeout.as_secs_f64());
        }
        if let Some(timeout) = config.tool_timeout_sec {
            entry["tool_timeout_sec"] = value(timeout.as_secs_f64());
        }
        if let Some(enabled_tools) = &config.enabled_tools
            && !enabled_tools.is_empty()
        {
            entry["enabled_tools"] = array_from_iter(enabled_tools.iter().cloned());
        }
        if let Some(disabled_tools) = &config.disabled_tools
            && !disabled_tools.is_empty()
        {
            entry["disabled_tools"] = array_from_iter(disabled_tools.iter().cloned());
        }

        TomlItem::Table(entry)
    }

    fn table_from_inline(inline: &InlineTable) -> TomlTable {
        let mut table = new_implicit_table();
        for (key, value) in inline.iter() {
            let mut value = value.clone();
            let decor = value.decor_mut();
            decor.set_suffix("");
            table.insert(key, TomlItem::Value(value));
        }
        table
    }

    pub(super) fn new_implicit_table() -> TomlTable {
        let mut table = TomlTable::new();
        table.set_implicit(true);
        table
    }

    fn array_from_iter<I>(iter: I) -> TomlItem
    where
        I: Iterator<Item = String>,
    {
        let mut array = TomlArray::new();
        for value in iter {
            array.push(value);
        }
        TomlItem::Value(array.into())
    }

    fn table_from_pairs<'a, I>(pairs: I) -> TomlItem
    where
        I: IntoIterator<Item = (&'a String, &'a String)>,
    {
        let mut entries: Vec<_> = pairs.into_iter().collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let mut table = TomlTable::new();
        table.set_implicit(false);
        for (key, val) in entries {
            table.insert(key, value(val.clone()));
        }
        TomlItem::Table(table)
    }
}

struct ConfigDocument {
    doc: DocumentMut,
    profile: Option<String>,
}

#[derive(Copy, Clone)]
enum Scope {
    Global,
    Profile,
}

#[derive(Copy, Clone)]
enum TraversalMode {
    Create,
    Existing,
}

impl ConfigDocument {
    fn new(doc: DocumentMut, profile: Option<String>) -> Self {
        Self { doc, profile }
    }

    fn apply(&mut self, edit: &ConfigEdit) -> anyhow::Result<bool> {
        match edit {
            ConfigEdit::SetModel { model, effort } => Ok({
                let mut mutated = false;
                mutated |= self.write_profile_value(
                    &["model"],
                    model.as_ref().map(|model_value| value(model_value.clone())),
                );
                mutated |= self.write_profile_value(
                    &["model_reasoning_effort"],
                    effort.map(|effort| value(effort.to_string())),
                );
                mutated
            }),
            ConfigEdit::SetNoticeHideFullAccessWarning(acknowledged) => Ok(self.write_value(
                Scope::Global,
                &[Notice::TABLE_KEY, "hide_full_access_warning"],
                value(*acknowledged),
            )),
            ConfigEdit::SetWindowsWslSetupAcknowledged(acknowledged) => Ok(self.write_value(
                Scope::Global,
                &["windows_wsl_setup_acknowledged"],
                value(*acknowledged),
            )),
            ConfigEdit::ReplaceMcpServers(servers) => Ok(self.replace_mcp_servers(servers)),
            ConfigEdit::SetPath { segments, value } => Ok(self.insert(segments, value.clone())),
            ConfigEdit::ClearPath { segments } => Ok(self.clear_owned(segments)),
            ConfigEdit::SetProjectTrusted(project_path) => {
                // Delegate to the existing, tested logic in config.rs to
                // ensure tables are explicit and migration is preserved.
                crate::config::set_project_trusted_inner(&mut self.doc, project_path.as_path())?;
                Ok(true)
            }
        }
    }

    fn write_profile_value(&mut self, segments: &[&str], value: Option<TomlItem>) -> bool {
        match value {
            Some(item) => self.write_value(Scope::Profile, segments, item),
            None => self.clear(Scope::Profile, segments),
        }
    }

    fn write_value(&mut self, scope: Scope, segments: &[&str], value: TomlItem) -> bool {
        let resolved = self.scoped_segments(scope, segments);
        self.insert(&resolved, value)
    }

    fn clear(&mut self, scope: Scope, segments: &[&str]) -> bool {
        let resolved = self.scoped_segments(scope, segments);
        self.remove(&resolved)
    }

    fn clear_owned(&mut self, segments: &[String]) -> bool {
        self.remove(segments)
    }

    fn replace_mcp_servers(&mut self, servers: &BTreeMap<String, McpServerConfig>) -> bool {
        if servers.is_empty() {
            return self.clear(Scope::Global, &["mcp_servers"]);
        }

        let mut table = TomlTable::new();
        table.set_implicit(true);

        for (name, config) in servers {
            table.insert(name, document_helpers::serialize_mcp_server(config));
        }

        let item = TomlItem::Table(table);
        self.write_value(Scope::Global, &["mcp_servers"], item)
    }

    fn scoped_segments(&self, scope: Scope, segments: &[&str]) -> Vec<String> {
        let resolved: Vec<String> = segments
            .iter()
            .map(|segment| (*segment).to_string())
            .collect();

        if matches!(scope, Scope::Profile)
            && resolved.first().is_none_or(|segment| segment != "profiles")
            && let Some(profile) = self.profile.as_deref()
        {
            let mut scoped = Vec::with_capacity(resolved.len() + 2);
            scoped.push("profiles".to_string());
            scoped.push(profile.to_string());
            scoped.extend(resolved);
            return scoped;
        }

        resolved
    }

    fn insert(&mut self, segments: &[String], value: TomlItem) -> bool {
        let Some((last, parents)) = segments.split_last() else {
            return false;
        };

        let Some(parent) = self.descend(parents, TraversalMode::Create) else {
            return false;
        };

        parent[last] = value;
        true
    }

    fn remove(&mut self, segments: &[String]) -> bool {
        let Some((last, parents)) = segments.split_last() else {
            return false;
        };

        let Some(parent) = self.descend(parents, TraversalMode::Existing) else {
            return false;
        };

        parent.remove(last).is_some()
    }

    fn descend(&mut self, segments: &[String], mode: TraversalMode) -> Option<&mut TomlTable> {
        let mut current = self.doc.as_table_mut();

        for segment in segments {
            match mode {
                TraversalMode::Create => {
                    if !current.contains_key(segment.as_str()) {
                        current.insert(
                            segment.as_str(),
                            TomlItem::Table(document_helpers::new_implicit_table()),
                        );
                    }

                    let item = current.get_mut(segment.as_str())?;
                    current = document_helpers::ensure_table_for_write(item)?;
                }
                TraversalMode::Existing => {
                    let item = current.get_mut(segment.as_str())?;
                    current = document_helpers::ensure_table_for_read(item)?;
                }
            }
        }

        Some(current)
    }
}

/// Persist edits using a blocking strategy.
pub fn apply_blocking(
    codex_home: &Path,
    profile: Option<&str>,
    edits: &[ConfigEdit],
) -> anyhow::Result<()> {
    if edits.is_empty() {
        return Ok(());
    }

    let config_path = codex_home.join(CONFIG_TOML_FILE);
    let serialized = match std::fs::read_to_string(&config_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err.into()),
    };

    let doc = if serialized.is_empty() {
        DocumentMut::new()
    } else {
        serialized.parse::<DocumentMut>()?
    };

    let profile = profile.map(ToOwned::to_owned).or_else(|| {
        doc.get("profile")
            .and_then(|item| item.as_str())
            .map(ToOwned::to_owned)
    });

    let mut document = ConfigDocument::new(doc, profile);
    let mut mutated = false;

    for edit in edits {
        mutated |= document.apply(edit)?;
    }

    if !mutated {
        return Ok(());
    }

    std::fs::create_dir_all(codex_home).with_context(|| {
        format!(
            "failed to create Codex home directory at {}",
            codex_home.display()
        )
    })?;

    let tmp = NamedTempFile::new_in(codex_home)?;
    std::fs::write(tmp.path(), document.doc.to_string()).with_context(|| {
        format!(
            "failed to write temporary config file at {}",
            tmp.path().display()
        )
    })?;
    tmp.persist(config_path)?;

    Ok(())
}

/// Persist edits asynchronously by offloading the blocking writer.
pub async fn apply(
    codex_home: &Path,
    profile: Option<&str>,
    edits: Vec<ConfigEdit>,
) -> anyhow::Result<()> {
    let codex_home = codex_home.to_path_buf();
    let profile = profile.map(ToOwned::to_owned);
    task::spawn_blocking(move || apply_blocking(&codex_home, profile.as_deref(), &edits))
        .await
        .context("config persistence task panicked")?
}

/// Fluent builder to batch config edits and apply them atomically.
#[derive(Default)]
pub struct ConfigEditsBuilder {
    codex_home: PathBuf,
    profile: Option<String>,
    edits: Vec<ConfigEdit>,
}

impl ConfigEditsBuilder {
    pub fn new(codex_home: &Path) -> Self {
        Self {
            codex_home: codex_home.to_path_buf(),
            profile: None,
            edits: Vec::new(),
        }
    }

    pub fn with_profile(mut self, profile: Option<&str>) -> Self {
        self.profile = profile.map(ToOwned::to_owned);
        self
    }

    pub fn set_model(mut self, model: Option<&str>, effort: Option<ReasoningEffort>) -> Self {
        self.edits.push(ConfigEdit::SetModel {
            model: model.map(ToOwned::to_owned),
            effort,
        });
        self
    }

    pub fn set_hide_full_access_warning(mut self, acknowledged: bool) -> Self {
        self.edits
            .push(ConfigEdit::SetNoticeHideFullAccessWarning(acknowledged));
        self
    }

    pub fn set_windows_wsl_setup_acknowledged(mut self, acknowledged: bool) -> Self {
        self.edits
            .push(ConfigEdit::SetWindowsWslSetupAcknowledged(acknowledged));
        self
    }

    pub fn replace_mcp_servers(mut self, servers: &BTreeMap<String, McpServerConfig>) -> Self {
        self.edits
            .push(ConfigEdit::ReplaceMcpServers(servers.clone()));
        self
    }

    pub fn set_project_trusted<P: Into<PathBuf>>(mut self, project_path: P) -> Self {
        self.edits
            .push(ConfigEdit::SetProjectTrusted(project_path.into()));
        self
    }

    /// Apply edits on a blocking thread.
    pub fn apply_blocking(self) -> anyhow::Result<()> {
        apply_blocking(&self.codex_home, self.profile.as_deref(), &self.edits)
    }

    /// Apply edits asynchronously via a blocking offload.
    pub async fn apply(self) -> anyhow::Result<()> {
        task::spawn_blocking(move || {
            apply_blocking(&self.codex_home, self.profile.as_deref(), &self.edits)
        })
        .await
        .context("config persistence task panicked")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_types::McpServerTransportConfig;
    use codex_protocol::config_types::ReasoningEffort;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;
    use tokio::runtime::Builder;
    use toml::Value as TomlValue;

    #[test]
    fn blocking_set_model_top_level() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path();

        apply_blocking(
            codex_home,
            None,
            &[ConfigEdit::SetModel {
                model: Some("gpt-5-codex".to_string()),
                effort: Some(ReasoningEffort::High),
            }],
        )
        .expect("persist");

        let contents =
            std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        let expected = r#"model = "gpt-5-codex"
model_reasoning_effort = "high"
"#;
        assert_eq!(contents, expected);
    }

    #[test]
    fn blocking_set_model_preserves_inline_table_contents() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path();

        // Seed with inline tables for profiles to simulate common user config.
        std::fs::write(
            codex_home.join(CONFIG_TOML_FILE),
            r#"profile = "fast"

profiles = { fast = { model = "gpt-4o", sandbox_mode = "strict" } }
"#,
        )
        .expect("seed");

        apply_blocking(
            codex_home,
            None,
            &[ConfigEdit::SetModel {
                model: Some("o4-mini".to_string()),
                effort: None,
            }],
        )
        .expect("persist");

        let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        let value: TomlValue = toml::from_str(&raw).expect("parse config");

        // Ensure sandbox_mode is preserved under profiles.fast and model updated.
        let profiles_tbl = value
            .get("profiles")
            .and_then(|v| v.as_table())
            .expect("profiles table");
        let fast_tbl = profiles_tbl
            .get("fast")
            .and_then(|v| v.as_table())
            .expect("fast table");
        assert_eq!(
            fast_tbl.get("sandbox_mode").and_then(|v| v.as_str()),
            Some("strict")
        );
        assert_eq!(
            fast_tbl.get("model").and_then(|v| v.as_str()),
            Some("o4-mini")
        );
    }

    #[test]
    fn blocking_clear_model_removes_inline_table_entry() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path();

        std::fs::write(
            codex_home.join(CONFIG_TOML_FILE),
            r#"profile = "fast"

profiles = { fast = { model = "gpt-4o", sandbox_mode = "strict" } }
"#,
        )
        .expect("seed");

        apply_blocking(
            codex_home,
            None,
            &[ConfigEdit::SetModel {
                model: None,
                effort: Some(ReasoningEffort::High),
            }],
        )
        .expect("persist");

        let contents =
            std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        let expected = r#"profile = "fast"

[profiles.fast]
sandbox_mode = "strict"
model_reasoning_effort = "high"
"#;
        assert_eq!(contents, expected);
    }

    #[test]
    fn blocking_set_model_scopes_to_active_profile() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path();
        std::fs::write(
            codex_home.join(CONFIG_TOML_FILE),
            r#"profile = "team"

[profiles.team]
model_reasoning_effort = "low"
"#,
        )
        .expect("seed");

        apply_blocking(
            codex_home,
            None,
            &[ConfigEdit::SetModel {
                model: Some("o5-preview".to_string()),
                effort: Some(ReasoningEffort::Minimal),
            }],
        )
        .expect("persist");

        let contents =
            std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        let expected = r#"profile = "team"

[profiles.team]
model_reasoning_effort = "minimal"
model = "o5-preview"
"#;
        assert_eq!(contents, expected);
    }

    #[test]
    fn blocking_set_model_with_explicit_profile() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path();
        std::fs::write(
            codex_home.join(CONFIG_TOML_FILE),
            r#"[profiles."team a"]
model = "gpt-5-codex"
"#,
        )
        .expect("seed");

        apply_blocking(
            codex_home,
            Some("team a"),
            &[ConfigEdit::SetModel {
                model: Some("o4-mini".to_string()),
                effort: None,
            }],
        )
        .expect("persist");

        let contents =
            std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        let expected = r#"[profiles."team a"]
model = "o4-mini"
"#;
        assert_eq!(contents, expected);
    }

    #[test]
    fn blocking_set_hide_full_access_warning_preserves_table() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path();
        std::fs::write(
            codex_home.join(CONFIG_TOML_FILE),
            r#"# Global comment

[notice]
# keep me
existing = "value"
"#,
        )
        .expect("seed");

        apply_blocking(
            codex_home,
            None,
            &[ConfigEdit::SetNoticeHideFullAccessWarning(true)],
        )
        .expect("persist");

        let contents =
            std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        let expected = r#"# Global comment

[notice]
# keep me
existing = "value"
hide_full_access_warning = true
"#;
        assert_eq!(contents, expected);
    }

    #[test]
    fn blocking_replace_mcp_servers_round_trips() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path();

        let mut servers = BTreeMap::new();
        servers.insert(
            "stdio".to_string(),
            McpServerConfig {
                transport: McpServerTransportConfig::Stdio {
                    command: "cmd".to_string(),
                    args: vec!["--flag".to_string()],
                    env: Some(
                        [
                            ("B".to_string(), "2".to_string()),
                            ("A".to_string(), "1".to_string()),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                    env_vars: vec!["FOO".to_string()],
                    cwd: None,
                },
                enabled: true,
                startup_timeout_sec: None,
                tool_timeout_sec: None,
                enabled_tools: Some(vec!["one".to_string(), "two".to_string()]),
                disabled_tools: None,
            },
        );

        servers.insert(
            "http".to_string(),
            McpServerConfig {
                transport: McpServerTransportConfig::StreamableHttp {
                    url: "https://example.com".to_string(),
                    bearer_token_env_var: Some("TOKEN".to_string()),
                    http_headers: Some(
                        [("Z-Header".to_string(), "z".to_string())]
                            .into_iter()
                            .collect(),
                    ),
                    env_http_headers: None,
                },
                enabled: false,
                startup_timeout_sec: Some(std::time::Duration::from_secs(5)),
                tool_timeout_sec: None,
                enabled_tools: None,
                disabled_tools: Some(vec!["forbidden".to_string()]),
            },
        );

        apply_blocking(
            codex_home,
            None,
            &[ConfigEdit::ReplaceMcpServers(servers.clone())],
        )
        .expect("persist");

        let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        let expected = "\
[mcp_servers.http]
url = \"https://example.com\"
bearer_token_env_var = \"TOKEN\"
enabled = false
startup_timeout_sec = 5.0
disabled_tools = [\"forbidden\"]

[mcp_servers.http.http_headers]
Z-Header = \"z\"

[mcp_servers.stdio]
command = \"cmd\"
args = [\"--flag\"]
env_vars = [\"FOO\"]
enabled_tools = [\"one\", \"two\"]

[mcp_servers.stdio.env]
A = \"1\"
B = \"2\"
";
        assert_eq!(raw, expected);
    }

    #[test]
    fn blocking_clear_path_noop_when_missing() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path();

        apply_blocking(
            codex_home,
            None,
            &[ConfigEdit::ClearPath {
                segments: vec!["missing".to_string()],
            }],
        )
        .expect("apply");

        assert!(
            !codex_home.join(CONFIG_TOML_FILE).exists(),
            "config.toml should not be created on noop"
        );
    }

    #[test]
    fn blocking_set_path_updates_notifications() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path();

        let item = value(false);
        apply_blocking(
            codex_home,
            None,
            &[ConfigEdit::SetPath {
                segments: vec!["tui".to_string(), "notifications".to_string()],
                value: item,
            }],
        )
        .expect("apply");

        let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        let config: TomlValue = toml::from_str(&raw).expect("parse config");
        let notifications = config
            .get("tui")
            .and_then(|item| item.as_table())
            .and_then(|tbl| tbl.get("notifications"))
            .and_then(toml::Value::as_bool);
        assert_eq!(notifications, Some(false));
    }

    #[tokio::test]
    async fn async_builder_set_model_persists() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path().to_path_buf();

        ConfigEditsBuilder::new(&codex_home)
            .set_model(Some("gpt-5-codex"), Some(ReasoningEffort::High))
            .apply()
            .await
            .expect("persist");

        let contents =
            std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        let expected = r#"model = "gpt-5-codex"
model_reasoning_effort = "high"
"#;
        assert_eq!(contents, expected);
    }

    #[test]
    fn blocking_builder_set_model_round_trips_back_and_forth() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path();

        let initial_expected = r#"model = "o4-mini"
model_reasoning_effort = "low"
"#;
        ConfigEditsBuilder::new(codex_home)
            .set_model(Some("o4-mini"), Some(ReasoningEffort::Low))
            .apply_blocking()
            .expect("persist initial");
        let mut contents =
            std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        assert_eq!(contents, initial_expected);

        let updated_expected = r#"model = "gpt-5-codex"
model_reasoning_effort = "high"
"#;
        ConfigEditsBuilder::new(codex_home)
            .set_model(Some("gpt-5-codex"), Some(ReasoningEffort::High))
            .apply_blocking()
            .expect("persist update");
        contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        assert_eq!(contents, updated_expected);

        ConfigEditsBuilder::new(codex_home)
            .set_model(Some("o4-mini"), Some(ReasoningEffort::Low))
            .apply_blocking()
            .expect("persist revert");
        contents = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        assert_eq!(contents, initial_expected);
    }

    #[test]
    fn blocking_set_asynchronous_helpers_available() {
        let rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path().to_path_buf();

        rt.block_on(async {
            ConfigEditsBuilder::new(&codex_home)
                .set_hide_full_access_warning(true)
                .apply()
                .await
                .expect("persist");
        });

        let raw = std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        let notice = toml::from_str::<TomlValue>(&raw)
            .expect("parse config")
            .get("notice")
            .and_then(|item| item.as_table())
            .and_then(|tbl| tbl.get("hide_full_access_warning"))
            .and_then(toml::Value::as_bool);
        assert_eq!(notice, Some(true));
    }

    #[test]
    fn replace_mcp_servers_blocking_clears_table_when_empty() {
        let tmp = tempdir().expect("tmpdir");
        let codex_home = tmp.path();
        std::fs::write(
            codex_home.join(CONFIG_TOML_FILE),
            "[mcp_servers]\nfoo = { command = \"cmd\" }\n",
        )
        .expect("seed");

        apply_blocking(
            codex_home,
            None,
            &[ConfigEdit::ReplaceMcpServers(BTreeMap::new())],
        )
        .expect("persist");

        let contents =
            std::fs::read_to_string(codex_home.join(CONFIG_TOML_FILE)).expect("read config");
        assert!(!contents.contains("mcp_servers"));
    }
}
