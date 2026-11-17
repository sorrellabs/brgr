use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

const MAX_CONFIG_BYTES: u64 = 16 * 1024;

/// The pane name brgr gives a worker unless the user changes it.
pub(crate) const DEFAULT_PANE_NAME_TEMPLATE: &str = "{harness}/{provider}/{model}: {objective}";
/// Every placeholder a pane name template may use.
pub(crate) const PANE_NAME_PLACEHOLDERS: [&str; 6] = [
    "harness",
    "provider",
    "model",
    "effort",
    "objective",
    "task",
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum WorkerPlacement {
    #[default]
    Adjacent,
    Below,
    Tab,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClaudeRoute {
    #[default]
    ClaudeCodeOnly,
    Any,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutesConfig {
    pub claude: ClaudeRoute,
}

impl RoutesConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub herdr: HerdrConfig,
    pub worker: WorkerConfig,
    // The tables below did not exist in earlier releases, which refuse unknown
    // keys and share this file. They are written only once they hold something.
    #[serde(skip_serializing_if = "IssuesConfig::is_default")]
    pub issues: IssuesConfig,
    #[serde(skip_serializing_if = "RecertifyConfig::is_default")]
    pub recertify: RecertifyConfig,
    #[serde(skip_serializing_if = "HandoffConfig::is_default")]
    pub handoff: HandoffConfig,
    #[serde(skip_serializing_if = "RoutesConfig::is_default")]
    pub routes: RoutesConfig,
    #[serde(skip_serializing_if = "CallingOptions::is_unset")]
    pub defaults: CallingOptions,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub harnesses: BTreeMap<String, CallingOptions>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, crate::aliases::Alias>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CallingOptions {
    pub(crate) harness: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) permission: Option<brgr_protocol::PermissionLevel>,
    pub(crate) deadline_seconds: Option<u64>,
    pub(crate) argv: Vec<String>,
    pub(crate) notifications: Option<bool>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerConfig {
    /// The most a worker may be given. A task asking for more is refused, and a
    /// task asking for nothing runs at this level instead of the harness's
    /// full default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_permission: Option<brgr_protocol::PermissionLevel>,
}

/// Whether `brgr run` re-certifies a harness whose CLI was updated instead of
/// stopping. Off by default: each re-certification is a paid scratch run and
/// trusts the changed binary.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecertifyConfig {
    pub auto: bool,
}

impl RecertifyConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Whether brgr files GitHub issues for the failures it records. Off by default:
/// issue text leaves the machine.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IssuesConfig {
    pub auto_file: bool,
    /// `OWNER/NAME` of the repository that receives the issues.
    pub repo: Option<String>,
    /// How many times a failure must be seen before its issue is filed.
    pub min_count: u64,
}

impl IssuesConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for IssuesConfig {
    fn default() -> Self {
        Self {
            auto_file: false,
            repo: None,
            min_count: 2,
        }
    }
}

/// When a head is told to hand its command to another harness. Off until a
/// target `kind` is named; brgr never picks a harness for the owner.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HandoffConfig {
    /// Herdr agent kind of the incoming head, for example `codex`.
    pub kind: Option<String>,
    /// Approve closing the outgoing head's pane after the handoff.
    pub exit_old: bool,
    /// Read and dropped: a full context window is never a reason to hand off.
    /// Kept so a config written by 2.15.5 or 2.15.6 still loads.
    #[serde(skip_serializing)]
    pub context_percent: Option<u8>,
    /// Tell the head when either plan window is this used, in percent.
    pub quota_percent: Option<u8>,
    /// Extra arguments for the incoming agent, for example a permission flag.
    pub agent_args: Vec<String>,
    /// Run the handoff itself, at the end of the head's next brgr command,
    /// instead of only telling the head to.
    pub auto: bool,
}

impl HandoffConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The target and at least one threshold are set.
    pub(crate) fn is_armed(&self) -> bool {
        self.kind.is_some() && self.quota_percent.is_some()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HerdrConfig {
    pub worker_placement: WorkerPlacement,
    pub auto_worker_pane: bool,
    pub codex_executable: Option<PathBuf>,
    /// Keep harnesses in print mode even inside Herdr, instead of running them
    /// as their own TUI in a pane.
    pub prefer_print_mode: bool,
    /// The display name of each worker pane. An empty template leaves panes
    /// unnamed, exactly as before this setting existed.
    pub pane_name_template: String,
}

impl Default for HerdrConfig {
    fn default() -> Self {
        Self {
            worker_placement: WorkerPlacement::default(),
            auto_worker_pane: false,
            codex_executable: None,
            prefer_print_mode: false,
            pane_name_template: DEFAULT_PANE_NAME_TEMPLATE.to_owned(),
        }
    }
}

pub fn validate_codex_executable(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("Codex executable must be an absolute path");
    }
    if !brgr_platform::fs::is_executable(path) {
        bail!("configured Codex executable is not an executable file");
    }
    Ok(())
}

impl CallingOptions {
    fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

impl Config {
    /// Restores the default pane name template.
    pub(crate) fn reset_pane_name_template(&mut self) {
        DEFAULT_PANE_NAME_TEMPLATE.clone_into(&mut self.herdr.pane_name_template);
    }

    pub(crate) fn calling(&self, harness: &str) -> CallingOptions {
        let global = &self.defaults;
        let local = self.harnesses.get(harness).cloned().unwrap_or_default();
        CallingOptions {
            harness: Some(harness.to_owned()),
            model: local.model.or_else(|| global.model.clone()),
            effort: local.effort.or_else(|| global.effort.clone()),
            permission: local.permission.or(global.permission),
            deadline_seconds: local.deadline_seconds.or(global.deadline_seconds),
            argv: global.argv.iter().chain(&local.argv).cloned().collect(),
            notifications: local.notifications.or(global.notifications),
        }
    }

    pub(crate) fn set(&mut self, key: &str, value: &str, harness: Option<&str>) -> Result<()> {
        if key == "claude-route" {
            if harness.is_some() {
                bail!("unknown calling option {key}");
            }
            self.routes.claude = match value {
                "claude-code-only" => ClaudeRoute::ClaudeCodeOnly,
                "any" => ClaudeRoute::Any,
                _ => bail!("claude-route must be claude-code-only or any"),
            };
            if self.routes.claude == ClaudeRoute::ClaudeCodeOnly
                && let Some(note) = crate::claude_route::tightening_note(self)
            {
                eprintln!("{note}");
            }
            return Ok(());
        }
        let options = if let Some(id) = harness {
            validate_id(id)?;
            self.harnesses.entry(id.to_owned()).or_default()
        } else {
            &mut self.defaults
        };
        match key {
            "harness" if harness.is_none() => {
                validate_id(value)?;
                options.harness = Some(value.to_owned());
            }
            "model" => {
                let model = nonempty(value)?.to_owned();
                if let Some(id) = harness
                    && self.routes.claude == ClaudeRoute::ClaudeCodeOnly
                    && id != brgr_registry::CLAUDE_CODE_HARNESS_ID
                    && crate::claude_route::is_claude_model(&model)
                {
                    bail!(
                        "claude-route is claude-code-only: Claude model `{model}` cannot be the default for {id}, which brgr would refuse. Point it at local.claude-code, or first run `brgr config set claude-route any`."
                    );
                }
                options.model = Some(model);
            }
            "effort" => options.effort = Some(nonempty(value)?.to_owned()),
            "notifications" => {
                options.notifications = Some(
                    value
                        .parse()
                        .context("notifications must be true or false")?,
                );
            }
            // Workers always run with full permissions; a stored level is
            // accepted for old scripts and never applied.
            "permission" => {
                eprintln!("brgr: permission is ignored; workers always run with full permissions");
            }
            "deadline-seconds" => {
                let seconds: u64 = value.parse()?;
                if seconds == 0 || seconds > 604_800 {
                    bail!("deadline must be 1..604800 seconds");
                }
                options.deadline_seconds = Some(seconds);
            }
            "argv" => {
                options.argv =
                    serde_json::from_str(value).context("argv must be a JSON string array")?;
                validate_argv(&options.argv)?;
            }
            "pane-name-template" if harness.is_none() => {
                validate_pane_name_template(value)?;
                value.clone_into(&mut self.herdr.pane_name_template);
            }
            "worker-placement" if harness.is_none() => {
                self.herdr.worker_placement = match value {
                    "adjacent" => WorkerPlacement::Adjacent,
                    "below" => WorkerPlacement::Below,
                    "tab" => WorkerPlacement::Tab,
                    _ => bail!("worker-placement must be adjacent, below or tab"),
                }
            }
            _ => bail!("unknown calling option {key}"),
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES {
            bail!("brgr config must be a regular file of at most {MAX_CONFIG_BYTES} bytes");
        }
        let mut source = String::new();
        File::open(path)?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut source)?;
        if source.len() as u64 > MAX_CONFIG_BYTES {
            bail!("brgr config exceeds {MAX_CONFIG_BYTES} bytes");
        }
        let config: Self = toml::from_str(&source).context("brgr config is invalid")?;
        // The validation error names every valid placeholder, so it is more
        // useful than a generic wrapper.
        validate_pane_name_template(&config.herdr.pane_name_template)?;
        Ok(config)
    }

    fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("brgr config has no parent directory")?;
        fs::create_dir_all(parent)?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        temporary.as_file_mut().sync_all()?;
        brgr_platform::fs::set_private_file_mode(temporary.as_file())?;
        temporary.persist(path)?;
        Ok(())
    }

    /// Serialize config changes across brgr processes. The lock lives beside
    /// the config in the control home and is never replaced with the config.
    pub(crate) fn update(
        path: &Path,
        control_home: &Path,
        change: impl FnOnce(&mut Self) -> Result<bool>,
    ) -> Result<Self> {
        fs::create_dir_all(control_home)?;
        let lock_path = control_home.join("config.lock");
        let lock = brgr_platform::fs::private_create_options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock.lock().context("locking brgr config")?;

        let mut config = Self::load(path)?;
        if change(&mut config)? {
            config.save(path)?;
        }
        Ok(config)
    }
}

/// A template may name only the supported placeholders, must close every
/// brace and stay within the size Herdr accepts for a label.
pub(crate) fn validate_pane_name_template(template: &str) -> Result<()> {
    if template.len() > 200 {
        bail!(
            "pane name template must be at most 200 bytes; {}",
            pane_name_placeholders()
        );
    }
    let mut rest = 0;
    while let Some(offset) = template[rest..].find('{') {
        let open = rest + offset;
        let Some(close) = template[open + 1..].find('}').map(|at| open + 1 + at) else {
            bail!(
                "pane name template has an unclosed brace; {}",
                pane_name_placeholders()
            );
        };
        let name = &template[open + 1..close];
        if !PANE_NAME_PLACEHOLDERS.contains(&name) {
            bail!(
                "unknown pane name placeholder {{{name}}}; {}",
                pane_name_placeholders()
            );
        }
        rest = close + 1;
    }
    Ok(())
}

fn pane_name_placeholders() -> String {
    format!(
        "valid placeholders: {}",
        PANE_NAME_PLACEHOLDERS
            .iter()
            .map(|name| format!("{{{name}}}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn nonempty(value: &str) -> Result<&str> {
    if value.trim().is_empty() {
        bail!("setting must not be empty");
    }
    Ok(value)
}

fn validate_id(id: &str) -> Result<()> {
    if !id.contains('.')
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("invalid harness id {id}");
    }
    Ok(())
}

pub(crate) fn validate_argv(argv: &[String]) -> Result<()> {
    if argv.len() > 128
        || argv
            .iter()
            .any(|arg| arg.contains('\0') || arg.len() > 4096)
    {
        bail!("calling argv exceeds the supported size");
    }
    for arg in argv {
        let flag = arg.split('=').next().unwrap_or(arg);
        if [
            "--model",
            "-m",
            "--effort",
            "--thinking",
            "--permission-mode",
            "--approval-mode",
            "--yolo",
            "--force",
            "-p",
            "--print",
            "--session-id",
            "--resume",
            "--continue",
        ]
        .contains(&flag)
        {
            bail!("{flag} conflicts with brgr calling options; configure its named setting");
        }
    }
    Ok(())
}

pub(crate) fn validate_manifest_argv(
    argv: &[String],
    manifest: &brgr_runner::HarnessManifest,
) -> Result<()> {
    validate_argv(argv)?;
    let selectors = manifest
        .launch
        .model_argv
        .iter()
        .chain(&manifest.launch.effort_argv)
        .chain(manifest.launch.permission_argv.full.iter().flatten())
        .chain(manifest.launch.permission_argv.edits.iter().flatten())
        .chain(manifest.launch.permission_argv.read_only.iter().flatten());
    for selector in selectors.filter(|argument| argument.starts_with('-')) {
        let flag = selector.split('=').next().unwrap_or(selector);
        if argv
            .iter()
            .any(|argument| argument.split('=').next() == Some(flag))
        {
            bail!("{flag} is a native selector owned by brgr's named calling settings");
        }
    }
    Ok(())
}

pub(crate) fn initialize(paths: &crate::Paths, json_output: bool) -> Result<()> {
    let instructions = paths.home.join("BRGR.md");
    let mut config_created = false;
    Config::update(&paths.config, &paths.home, |_| {
        config_created = !paths.config.exists();
        Ok(config_created)
    })?;
    let mut instructions_created = false;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&instructions)
    {
        Ok(mut file) => {
            brgr_platform::fs::set_private_file_mode(&file)?;
            file.write_all("# brgr 사용자 지침\n\n<!-- 이 파일에 오케스트레이션에 전달할 지침을 작성하세요. -->\n".as_bytes())?;
            file.sync_all()?;
            instructions_created = true;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    crate::print_value(
        &serde_json::json!({"config":paths.config,"instructions":instructions,"config_created":config_created,"instructions_created":instructions_created}),
        json_output,
    );
    Ok(())
}

pub(crate) async fn check(paths: &crate::Paths, config: &Config, json_output: bool) -> Result<()> {
    let registry = brgr_registry::Registry::open_with_control_home(&paths.registry, &paths.home)?;
    validate_argv(&config.defaults.argv)?;
    if let Some(error) = crate::claude_route::saved_route_refusal(config) {
        bail!("{error}");
    }
    let mut checked = Vec::new();
    // With no default configured and several harnesses registered, only the
    // per-harness entries are checked: a run would ask for one by name.
    let default =
        crate::harness_commands::default_harness(&registry, config.defaults.harness.as_deref())
            .ok();
    for id in config
        .harnesses
        .keys()
        .chain(default.iter())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let (manifest, _) = crate::admission::load_harness(&registry, id, false).await?;
        let options = config.calling(id);
        validate_manifest_argv(&options.argv, &manifest)?;
        manifest.permission_arguments(options.permission)?;
        if options.model.is_some() && manifest.launch.model_argv.is_empty() {
            bail!("{id} has no model option");
        }
        if options.effort.is_some() && manifest.launch.effort_argv.is_empty() {
            bail!("{id} has no effort option");
        }
        let interactive = manifest
            .launch
            .interactive
            .as_ref()
            .context("configured harness has no native TUI recipe")?;
        if interactive.effort_print_only && options.effort.is_some() {
            bail!("{id} cannot apply effort in its TUI");
        }
        if interactive.model_print_only
            && options.model.is_some()
            && !crate::pane_adapter::session_model_supported(interactive)
        {
            bail!("{id} cannot apply a model in its TUI");
        }
        if options
            .deadline_seconds
            .is_some_and(|seconds| seconds == 0 || seconds > 604_800)
        {
            bail!("invalid deadline setting for {id}");
        }
        registry
            .preflight_model(&manifest, options.model.as_deref())
            .await?;
        checked.push(id);
    }
    crate::print_value(
        &serde_json::json!({
            "status":"ok",
            "harnesses":checked,
            "default_presentation":"tui",
            "default_permission":"full",
            "claude_route":config.routes.claude,
            "instructions":paths.home.join("BRGR.md")
        }),
        json_output,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_defaults_to_adjacent_and_invalid_file_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        assert_eq!(
            Config::load(&path).unwrap().herdr.worker_placement,
            WorkerPlacement::Adjacent
        );
        assert!(!Config::load(&path).unwrap().herdr.auto_worker_pane);
        assert!(
            Config::load(&path)
                .unwrap()
                .herdr
                .codex_executable
                .is_none()
        );
        fs::write(&path, "[herdr]\nworker_placement = 'elsewhere'\n").unwrap();
        assert!(Config::load(&path).is_err());
        fs::write(&path, "[herdr]\nworker_placement = 'tab'\n").unwrap();
        assert_eq!(
            Config::load(&path).unwrap().herdr.worker_placement,
            WorkerPlacement::Tab
        );
        fs::write(
            &path,
            "[herdr]\nworker_placement = 'tab'\nauto_worker_pane = true\n",
        )
        .unwrap();
        assert!(Config::load(&path).unwrap().herdr.auto_worker_pane);
        let linked = root.path().join("linked.toml");
        std::os::unix::fs::symlink(&path, &linked).unwrap();
        assert!(Config::load(&linked).is_err());
    }

    #[test]
    fn config_update_refuses_invalid_toml_without_overwriting_it() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let path = home.join("config.toml");
        let invalid = b"[herdr\nworker_placement = 'tab'\n";
        fs::write(&path, invalid).unwrap();

        assert!(Config::update(&path, &home, |_| Ok(true)).is_err());
        assert_eq!(fs::read(&path).unwrap(), invalid);
    }

    #[test]
    fn the_pane_name_template_defaults_to_the_readable_form_and_empty_turns_it_off() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        assert_eq!(
            Config::load(&path).unwrap().herdr.pane_name_template,
            DEFAULT_PANE_NAME_TEMPLATE
        );
        fs::write(&path, "[herdr]\npane_name_template = ''\n").unwrap();
        assert_eq!(Config::load(&path).unwrap().herdr.pane_name_template, "");
    }

    #[test]
    fn valid_pane_name_templates_load() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        for template in [
            "{harness}/{provider}/{model}: {objective}",
            "{effort} {task}",
            "plain label",
            "",
        ] {
            fs::write(
                &path,
                format!("[herdr]\npane_name_template = {template:?}\n"),
            )
            .unwrap();
            assert_eq!(
                Config::load(&path).unwrap().herdr.pane_name_template,
                template
            );
        }
    }

    #[test]
    fn an_invalid_pane_name_template_is_refused_with_the_placeholder_list() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let overlong = "x".repeat(201);
        for template in ["{unknown}", "{harness", overlong.as_str()] {
            fs::write(
                &path,
                format!("[herdr]\npane_name_template = {template:?}\n"),
            )
            .unwrap();
            let error = Config::load(&path).unwrap_err().to_string();
            for placeholder in PANE_NAME_PLACEHOLDERS {
                assert!(
                    error.contains(&format!("{{{placeholder}}}")),
                    "{template}: {error}"
                );
            }
        }
    }

    #[test]
    fn the_pane_name_template_setter_validates_and_keeps_the_value() {
        let mut config = Config::default();
        config
            .set("pane-name-template", "{harness}: {objective}", None)
            .unwrap();
        assert_eq!(config.herdr.pane_name_template, "{harness}: {objective}");
        let overlong = "x".repeat(201);
        for template in ["{unknown}", "{harness", overlong.as_str()] {
            let error = config
                .set("pane-name-template", template, None)
                .unwrap_err()
                .to_string();
            for placeholder in PANE_NAME_PLACEHOLDERS {
                assert!(
                    error.contains(&format!("{{{placeholder}}}")),
                    "{template}: {error}"
                );
            }
        }
        config.set("pane-name-template", "", None).unwrap();
        assert_eq!(config.herdr.pane_name_template, "");
    }

    #[test]
    fn the_pane_name_template_reset_restores_the_default() {
        let mut config = Config::default();
        config.set("pane-name-template", "", None).unwrap();
        config.reset_pane_name_template();
        assert_eq!(config.herdr.pane_name_template, DEFAULT_PANE_NAME_TEMPLATE);
    }

    #[test]
    fn below_placement_loads_and_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        fs::write(&path, "[herdr]\nworker_placement = 'below'\n").unwrap();
        assert_eq!(
            Config::load(&path).unwrap().herdr.worker_placement,
            WorkerPlacement::Below
        );
        let mut config = Config::default();
        config.set("worker-placement", "below", None).unwrap();
        assert_eq!(config.herdr.worker_placement, WorkerPlacement::Below);
        let error = config
            .set("worker-placement", "elsewhere", None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("adjacent") && error.contains("below") && error.contains("tab"),
            "{error}"
        );
    }

    #[test]
    fn claude_route_round_trips_and_an_unknown_value_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let path = home.join("config.toml");
        Config::update(&path, &home, |config| {
            config.set("claude-route", "any", None)?;
            Ok(true)
        })
        .unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.routes.claude, ClaudeRoute::Any);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("[routes]\nclaude = \"any\"")
        );
        assert!(
            Config::default()
                .set("claude-route", "sometimes", None)
                .is_err()
        );
        fs::write(&path, "[routes]\nclaude = \"sometimes\"\n").unwrap();
        assert!(Config::load(&path).is_err());
        fs::write(&path, "[routes]\nother = \"any\"\n").unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn claude_route_cannot_be_set_per_harness() {
        let error = Config::default()
            .set("claude-route", "any", Some("local.opencode"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unknown calling option claude-route"),
            "{error}"
        );
    }

    #[test]
    fn setting_a_claude_default_model_on_another_harness_is_refused_under_claude_code_only() {
        let mut config = Config::default();
        let error = config
            .set("model", "litellm/claude-opus-5-5", Some("local.opencode"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("claude-route is claude-code-only"),
            "{error}"
        );
        config
            .set("model", "litellm/claude-opus-5-5", None)
            .unwrap();
        assert_eq!(
            config.defaults.model.as_deref(),
            Some("litellm/claude-opus-5-5")
        );
        config
            .set("model", "sonnet", Some("local.claude-code"))
            .unwrap();
        config
            .set("model", "litellm/grok-4.6", Some("local.opencode"))
            .unwrap();
        config.set("claude-route", "any", None).unwrap();
        config
            .set("model", "litellm/claude-opus-5-5", Some("local.opencode"))
            .unwrap();
    }

    #[test]
    fn config_check_lists_every_saved_route_the_setting_would_refuse() {
        let mut config = Config::default();
        config.aliases.insert(
            "fable".into(),
            crate::aliases::Alias {
                harness: "local.opencode".into(),
                model: Some("litellm/claude-fable-5-2".into()),
                effort: None,
            },
        );
        config
            .harnesses
            .entry("local.opencode".into())
            .or_default()
            .model = Some("litellm/claude-opus-5-5".into());
        let error = crate::claude_route::saved_route_refusal(&config).expect("violations");
        assert!(
            error.contains("alias fable (local.opencode, litellm/claude-fable-5-2)"),
            "{error}"
        );
        assert!(
            error.contains("default model of local.opencode (litellm/claude-opus-5-5)"),
            "{error}"
        );
        config.set("claude-route", "any", None).unwrap();
        assert!(crate::claude_route::saved_route_refusal(&config).is_none());
    }
}

#[cfg(test)]
mod serialize_tests {
    use super::*;
    use serde::Deserialize;

    /// Earlier releases refuse unknown tables and share this file, so saving an
    /// unrelated setting must not add the tables they do not know.
    #[test]
    fn a_default_config_writes_no_table_an_older_release_would_refuse() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        for table in ["[issues]", "[defaults]", "[harnesses", "[routes]"] {
            assert!(!text.contains(table), "{table} written:\n{text}");
        }
        let mut config = Config::default();
        config.issues.auto_file = true;
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("[issues]"), "{text}");
    }

    /// Frozen copy of the 2.15.24 enum. A new default value would make older
    /// releases refuse a freshly written config.toml.
    #[test]
    fn only_the_below_choice_is_unreadable_by_an_older_release() {
        #[derive(Debug, Deserialize)]
        struct OldFile {
            herdr: OldHerdr,
        }
        #[derive(Debug, Deserialize)]
        struct OldHerdr {
            #[serde(default)]
            worker_placement: OldWorkerPlacement,
        }
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum OldWorkerPlacement {
            #[default]
            Adjacent,
            Tab,
        }

        let default_text = toml::to_string_pretty(&Config::default()).unwrap();
        let parsed: OldFile = toml::from_str(&default_text).unwrap();
        assert_eq!(parsed.herdr.worker_placement, OldWorkerPlacement::Adjacent);

        let mut tab = Config::default();
        tab.herdr.worker_placement = WorkerPlacement::Tab;
        let tab_text = toml::to_string_pretty(&tab).unwrap();
        let parsed: OldFile = toml::from_str(&tab_text).unwrap();
        assert_eq!(parsed.herdr.worker_placement, OldWorkerPlacement::Tab);

        let mut below = Config::default();
        below.herdr.worker_placement = WorkerPlacement::Below;
        let below_text = toml::to_string_pretty(&below).unwrap();
        assert!(
            toml::from_str::<OldFile>(&below_text).is_err(),
            "{below_text}"
        );
    }

    #[test]
    fn a_default_claude_route_writes_no_routes_table_an_older_release_would_refuse() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!text.contains("[routes]"), "{text}");
        let mut config = Config::default();
        config.set("claude-route", "any", None).unwrap();
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("[routes]"), "{text}");
        assert!(text.contains("claude = \"any\""), "{text}");
        config
            .set("claude-route", "claude-code-only", None)
            .unwrap();
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(!text.contains("[routes]"), "{text}");
    }
}
