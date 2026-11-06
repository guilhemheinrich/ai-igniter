use crate::cli::Cli;
use crate::config::{CONFIG_FILE_NAME, Config, sanitize_name};
use anyhow::{Context, Result, bail};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct WorkspaceContext {
    pub workspace_path: PathBuf,
    pub root_path: PathBuf,
    pub slug: String,
    pub hash: String,
    pub compose_project: String,
    pub base_port: u16,
    pub config_path: PathBuf,
    pub config: Config,
    pub port_allocations: BTreeMap<String, u16>,
    pub parent_path: Option<PathBuf>,
    pub parent_template_vars: Option<BTreeMap<String, String>>,
    pub warnings: Vec<String>,
}

impl WorkspaceContext {
    pub fn print_warnings(&self) {
        for warning in &self.warnings {
            eprintln!("{} Warning: {warning}", "[warning]".yellow().bold());
        }
    }

    pub fn resolve(cli: &Cli) -> Result<Self> {
        let current_dir = std::env::current_dir().context("Failed to get current directory")?;
        let mut warnings = Vec::new();

        // 1. Workspace: --dir, else current git worktree, else current directory
        let requested = cli
            .dir
            .clone()
            .unwrap_or_else(|| locate_workspace(&current_dir));
        let workspace_path = std::fs::canonicalize(&requested)
            .with_context(|| format!("Workspace directory {:?} does not exist", requested))?;
        let main_checkout = git_main_checkout(&workspace_path);

        // 2. Config: workspace first, then the main checkout (worktrees may not carry an untracked config)
        let (config_path, config) = match &cli.config {
            Some(path) => {
                let local_path = Config::find_local_override_for_path(
                    path,
                    &workspace_path,
                    main_checkout.as_deref(),
                );
                (
                    path.clone(),
                    Config::load_from_file_with_override(path, local_path.as_deref())?,
                )
            }
            None => Config::find_config(
                &workspace_path,
                main_checkout.as_deref().unwrap_or(&workspace_path),
            )?,
        };

        // 3. Root: --root, else the main git checkout, else fallback to workspace
        let root_path = resolve_root_path(
            cli.root.as_deref(),
            main_checkout.as_deref(),
            &workspace_path,
        );

        let digest = Sha256::digest(workspace_path.to_string_lossy().as_bytes());
        let (base_port, port_warning) =
            crate::ports::resolve_base_port(cli.port, config.base_port, &digest)?;
        if let Some(w) = port_warning {
            warnings.push(w);
        }

        Self::from_parts_with_warnings(
            workspace_path,
            root_path,
            config_path,
            config,
            Some(base_port),
            warnings,
        )
    }

    /// Builds the context from resolved paths; a missing `base_port` is derived from the workspace path.
    #[allow(dead_code)]
    pub fn from_parts(
        workspace_path: PathBuf,
        root_path: PathBuf,
        config_path: PathBuf,
        config: Config,
        base_port: Option<u16>,
    ) -> Result<Self> {
        Self::from_parts_with_warnings(
            workspace_path,
            root_path,
            config_path,
            config,
            base_port,
            Vec::new(),
        )
    }

    pub fn from_parts_with_warnings(
        workspace_path: PathBuf,
        root_path: PathBuf,
        config_path: PathBuf,
        config: Config,
        base_port: Option<u16>,
        mut warnings: Vec<String>,
    ) -> Result<Self> {
        config
            .validate()
            .with_context(|| format!("Invalid configuration in {:?}", config_path))?;

        let digest = Sha256::digest(workspace_path.to_string_lossy().as_bytes());
        let hash = hex::encode(&digest[..4]);
        let slug = workspace_path
            .file_name()
            .map(|n| sanitize_name(&n.to_string_lossy()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "workspace".to_string());
        let compose_project = format!("{}-{}-{}", sanitize_name(&config.name), slug, hash);
        let compose_project = if config.workspace.is_partial() {
            format!("{compose_project}-partial")
        } else {
            compose_project
        };

        let parent_path = if config.workspace.is_partial() {
            let inherit_from = config
                .workspace
                .inherit_from
                .as_deref()
                .expect("validate() ensures inherit_from is Some in partial mode");
            Some(resolve_parent_path(inherit_from, &workspace_path)?)
        } else {
            None
        };

        let base_port = match base_port {
            Some(p) => p,
            None => {
                let (p, w) = crate::ports::resolve_base_port(None, config.base_port, &digest)?;
                if let Some(warn) = w {
                    warnings.push(warn);
                }
                p
            }
        };
        let (port_allocations, alloc_warnings) = crate::ports::allocate_ports(&config, base_port)?;
        warnings.extend(alloc_warnings);

        Ok(Self {
            workspace_path,
            root_path,
            slug,
            hash,
            compose_project,
            base_port,
            config_path,
            config,
            port_allocations,
            parent_path,
            parent_template_vars: None,
            warnings,
        })
    }

    pub fn template_vars(&self) -> BTreeMap<String, String> {
        let mut vars = BTreeMap::new();

        // Inherited vars go first so local vars win on collision
        if let Some(parent_vars) = &self.parent_template_vars {
            vars.extend(parent_vars.clone());
        }

        vars.insert("project.name".into(), self.config.name.clone());
        vars.insert("workspace.slug".into(), self.slug.clone());
        vars.insert("workspace.hash".into(), self.hash.clone());
        vars.insert(
            "workspace.compose_project".into(),
            self.compose_project.clone(),
        );
        vars.insert(
            "workspace.path".into(),
            self.workspace_path.display().to_string(),
        );
        vars.insert(
            "workspace.root".into(),
            self.root_path.display().to_string(),
        );

        for (name, port) in &self.port_allocations {
            vars.insert(format!("ports.{name}"), port.to_string());
        }

        for provider in crate::services::BUILTIN_SERVICES {
            provider.contribute_template_vars(self, &mut vars);
        }

        for name in self.config.services.custom.keys() {
            if let Some(port) = self.port_allocations.get(name) {
                vars.insert(format!("services.{name}.port"), port.to_string());
            }
        }

        vars
    }

    /// Merges inherited ports and template vars from the parent state into this context.
    /// Called by `execute_dev` after reading parent state — never during resolve.
    pub fn merge_inherited_state(&mut self, parent: &WorkspaceState) {
        let locals = &self.config.workspace.services_local;

        for (key, port) in &parent.port_allocations {
            if key == "base" {
                continue;
            }
            if is_local_port_key(key, locals) {
                continue;
            }
            self.port_allocations.insert(key.clone(), *port);
        }

        // Filter parent template_vars: keep only those for inherited services
        let mut inherited_vars = BTreeMap::new();
        for (key, val) in &parent.template_vars {
            // Skip vars whose service is local (the local provider handles those)
            let is_local = locals.iter().any(|svc| {
                key.starts_with(&format!("services.{svc}."))
                    || key.starts_with(&format!("services.{svc}_"))
            });
            if !is_local {
                inherited_vars.insert(key.clone(), val.clone());
            }
        }
        self.parent_template_vars = Some(inherited_vars);
    }

    pub fn interpolate(&self, text: &str) -> Result<String> {
        render_template(text, &self.template_vars()).map_err(|unknown| {
            anyhow::anyhow!(
                "Unknown template placeholder(s) {} in {:?}",
                format_placeholders(&unknown),
                text
            )
        })
    }

    pub fn igniter_dir(&self) -> PathBuf {
        self.workspace_path.join(".igniter")
    }

    pub fn compose_file_path(&self) -> PathBuf {
        self.igniter_dir().join("compose.json")
    }

    /// Image used by each configured and enabled service.
    pub fn service_images(&self) -> BTreeMap<String, String> {
        let mut images = BTreeMap::new();
        if let Some(pg) = self
            .config
            .services
            .postgres
            .as_ref()
            .filter(|pg| pg.enabled)
        {
            images.insert("postgres".to_string(), pg.image.clone());
        }
        if let Some(garage) = self.config.services.garage.as_ref().filter(|g| g.enabled) {
            images.insert("garage".to_string(), garage.image.clone());
        }
        for (name, custom) in &self.config.services.custom {
            images.insert(name.clone(), custom.image.clone());
        }
        images
    }
}

/// Replaces `{{var}}` and `{{var + N}}` placeholders; returns the unknown expressions on failure.
pub fn render_template(text: &str, vars: &BTreeMap<String, String>) -> Result<String, Vec<String>> {
    let mut out = String::with_capacity(text.len());
    let mut unknown = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        let expr = after[..end].trim();
        match eval_expr(expr, vars) {
            Some(value) => out.push_str(&value),
            None => unknown.push(expr.to_string()),
        }
        rest = &after[end + 2..];
    }
    // Either no placeholder left, or an unterminated `{{` kept verbatim
    if let Some(start) = rest.find("{{") {
        out.push_str(&rest[start..]);
    } else {
        out.push_str(rest);
    }

    if unknown.is_empty() {
        Ok(out)
    } else {
        Err(unknown)
    }
}

pub fn format_placeholders(unknown: &[String]) -> String {
    unknown
        .iter()
        .map(|u| format!("{{{{{u}}}}}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn eval_expr(expr: &str, vars: &BTreeMap<String, String>) -> Option<String> {
    if let Some((lhs, rhs)) = expr.split_once('+') {
        let base: u32 = vars.get(lhs.trim())?.parse().ok()?;
        let offset: u32 = rhs.trim().parse().ok()?;
        return u16::try_from(base + offset).ok().map(|p| p.to_string());
    }
    vars.get(expr).cloned()
}

/// Percent-encodes everything but RFC 3986 unreserved characters (for URL userinfo and paths).
pub(crate) fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Workspace state file — written by `dev`, read by partial workspaces
// ---------------------------------------------------------------------------

const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub version: u32,
    pub pid: u32,
    pub started_at: String,
    pub workspace_path: PathBuf,
    pub compose_project: String,
    pub base_port: u16,
    pub port_allocations: BTreeMap<String, u16>,
    pub template_vars: BTreeMap<String, String>,
    pub services: BTreeMap<String, ServiceState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceState {
    pub status: String,
    pub image: String,
}

pub fn state_file_path(workspace_path: &Path) -> PathBuf {
    workspace_path.join(".igniter").join("state.json")
}

pub fn write_state(workspace_path: &Path, state: &WorkspaceState) -> Result<()> {
    let dir = workspace_path.join(".igniter");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create .igniter directory at {:?}", dir))?;

    let path = state_file_path(workspace_path);
    let content =
        serde_json::to_string_pretty(state).context("Failed to serialize workspace state")?;

    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &content)
        .with_context(|| format!("Failed to write temp state file {:?}", tmp_path))?;

    // On Windows, fs::rename does not replace an existing file — remove first.
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(&path);
    }

    std::fs::rename(&tmp_path, &path)
        .with_context(|| format!("Failed to rename state file {:?}", path))?;
    Ok(())
}

pub fn read_state(workspace_path: &Path) -> Result<WorkspaceState> {
    let path = state_file_path(workspace_path);
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read state file {:?}", path))?;
    let state: WorkspaceState = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse state file {:?}", path))?;
    if state.version > STATE_VERSION {
        bail!(
            "State file version {} is newer than supported version {}. Update ai-igniter.",
            state.version,
            STATE_VERSION
        );
    }
    Ok(state)
}

pub fn build_workspace_state(ctx: &WorkspaceContext) -> WorkspaceState {
    let services = ctx
        .service_images()
        .into_iter()
        .map(|(name, image)| {
            (
                name,
                ServiceState {
                    status: "running".to_string(),
                    image,
                },
            )
        })
        .collect();

    WorkspaceState {
        version: STATE_VERSION,
        pid: std::process::id(),
        started_at: chrono::Utc::now().to_rfc3339(),
        workspace_path: ctx.workspace_path.clone(),
        compose_project: ctx.compose_project.clone(),
        base_port: ctx.base_port,
        port_allocations: ctx.port_allocations.clone(),
        template_vars: ctx.template_vars(),
        services,
    }
}

#[cfg(unix)]
pub fn check_pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 checks process existence without delivering a signal.
    let ret = unsafe {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        kill(pid as i32, 0)
    };
    if ret == 0 {
        return true;
    }
    // EPERM means the process exists but belongs to another user — still alive.
    std::io::Error::last_os_error().raw_os_error() == Some(1) // EPERM
}

#[cfg(windows)]
pub fn check_pid_alive(_pid: u32) -> bool {
    // Best-effort on Windows: assume alive and warn the caller.
    true
}

// ---------------------------------------------------------------------------
// Parent resolution for partial mode
// ---------------------------------------------------------------------------

pub fn resolve_parent_path(inherit_from: &str, workspace_path: &Path) -> Result<PathBuf> {
    let parent = if inherit_from == "root" {
        git_main_checkout(workspace_path).with_context(
            || "inherit_from = \"root\" requires a git repository with a main checkout",
        )?
    } else {
        let path = PathBuf::from(inherit_from);
        if path.is_absolute() {
            if !path.join(CONFIG_FILE_NAME).exists()
                && !path.join(format!(".{CONFIG_FILE_NAME}")).exists()
            {
                bail!(
                    "inherit_from path {:?} does not contain an ai-igniter config",
                    path
                );
            }
            path
        } else {
            resolve_worktree_by_name(inherit_from, workspace_path)?
        }
    };

    let parent = std::fs::canonicalize(&parent).unwrap_or(parent);
    let ws = std::fs::canonicalize(workspace_path).unwrap_or_else(|_| workspace_path.to_path_buf());
    if parent == ws {
        bail!(
            "partial workspace cannot inherit from itself\n\
             Hint: use a worktree (`git worktree add`) and configure partial mode there"
        );
    }
    Ok(parent)
}

fn resolve_worktree_by_name(name: &str, workspace_path: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_path)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("Failed to run `git worktree list`")?;
    if !output.status.success() {
        bail!("`git worktree list` failed");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            let path = PathBuf::from(path_str);
            if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                return Ok(path);
            }
        }
    }
    bail!("No git worktree named '{name}' found (check `git worktree list`)")
}

/// Returns true when `key` belongs to a service listed in `services_local` — including
/// companion keys like `postgres_neon` for service `postgres` (prefix match: `{svc}_`).
pub(crate) fn is_local_port_key(key: &str, services_local: &[String]) -> bool {
    services_local
        .iter()
        .any(|svc| key == svc || key.starts_with(&format!("{svc}_")))
}

fn resolve_root_path(
    cli_root: Option<&Path>,
    main_checkout: Option<&Path>,
    workspace_path: &Path,
) -> PathBuf {
    if let Some(explicit) = cli_root {
        return std::fs::canonicalize(explicit).unwrap_or_else(|_| explicit.to_path_buf());
    }
    let fallback_path = main_checkout.unwrap_or(workspace_path);
    std::fs::canonicalize(fallback_path).unwrap_or_else(|_| fallback_path.to_path_buf())
}

fn locate_workspace(current_dir: &Path) -> PathBuf {
    let toplevel = git_toplevel(current_dir);
    // Stop at the worktree boundary: a worktree nested in the main checkout must not adopt its project
    if let Some(dir) = find_nearest_config_dir(current_dir, toplevel.as_deref()) {
        return dir;
    }
    // Inside a git worktree without its own config: the config may live in the main checkout
    if let Some(toplevel) = toplevel {
        return toplevel;
    }
    current_dir.to_path_buf()
}

fn find_nearest_config_dir(start: &Path, boundary: Option<&Path>) -> Option<PathBuf> {
    let boundary = boundary.map(|b| std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf()));
    let mut current = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    loop {
        if current.join(CONFIG_FILE_NAME).exists()
            || current.join(format!(".{CONFIG_FILE_NAME}")).exists()
        {
            return Some(current);
        }
        if boundary.as_deref() == Some(current.as_path()) || !current.pop() {
            return None;
        }
    }
}

fn git(path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !stdout.is_empty()).then_some(stdout)
}

fn git_toplevel(path: &Path) -> Option<PathBuf> {
    git(path, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// Main checkout of the repository, also when `path` is a linked worktree.
fn git_main_checkout(path: &Path) -> Option<PathBuf> {
    let common_dir = PathBuf::from(git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    if !common_dir.ends_with(".git") {
        return None; // bare repository
    }
    common_dir.parent().map(Path::to_path_buf)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn test_ctx(config_toml: &str, base_port: Option<u16>) -> WorkspaceContext {
        let mut prefix = String::new();
        if !config_toml.contains("env_file") {
            prefix.push_str("env_file = \".env\"\n");
        }
        if !config_toml.contains("copy_files") {
            prefix.push_str("copy_files = []\n");
        }
        let doc = format!("{prefix}{config_toml}");
        let config: Config = toml::from_str(&doc).unwrap();
        WorkspaceContext::from_parts(
            PathBuf::from("/work/Feature X"),
            PathBuf::from("/work/main"),
            PathBuf::from("/work/main/ai-igniter.toml"),
            config,
            base_port,
        )
        .unwrap()
    }

    const FULL: &str = r#"
name = "My App"
env_file = ".env"
copy_files = []
[services.postgres]
database = "my-app"
user = "my-app"
password = "p@ss word"
e2e_database = "my-app_e2e"
[services.garage]
access_key = "k"
secret_key = "s"
[services.custom.mail]
image = "axllent/mailpit"
port_offset = 8
target_port = 8025
"#;

    #[test]
    fn allocates_ports_from_offsets() {
        let ctx = test_ctx(FULL, Some(4000));
        let expected = [
            ("base", 4000),
            ("garage", 4003),
            ("garage_web", 4004),
            ("mail", 4008),
            ("postgres", 4001),
        ];
        let actual: Vec<(&str, u16)> = ctx
            .port_allocations
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn sanitizes_compose_project() {
        let ctx = test_ctx(FULL, Some(4100));
        assert_eq!(
            ctx.compose_project,
            format!("my-app-feature-x-{}", ctx.hash)
        );
    }

    #[test]
    fn derives_stable_base_port_when_unset() {
        let a = test_ctx(FULL, None);
        let b = test_ctx(FULL, None);
        assert_eq!(a.base_port, b.base_port);
        assert!((20000..60000).contains(&a.base_port) && a.base_port.is_multiple_of(20));
    }

    #[test]
    fn handles_port_offset_overflow_gracefully() {
        let config: Config = toml::from_str(FULL).unwrap();
        let ctx = WorkspaceContext::from_parts(
            "/w".into(),
            "/w".into(),
            "/w/c".into(),
            config,
            Some(65530),
        )
        .unwrap();
        assert_eq!(ctx.base_port, 65530);
        assert!(
            ctx.warnings
                .iter()
                .any(|w| w.contains("dynamically allocated"))
        );
    }

    #[test]
    fn exposes_service_vars_with_encoded_credentials() {
        let vars = test_ctx(FULL, Some(4200)).template_vars();
        assert_eq!(
            vars["services.postgres.url"],
            "postgresql://my-app:p%40ss%20word@127.0.0.1:4201/my-app"
        );
        assert_eq!(
            vars["services.postgres.e2e_url"],
            "postgresql://my-app:p%40ss%20word@127.0.0.1:4201/my-app_e2e"
        );
        assert_eq!(vars["services.garage.web_port"], "4204");
        assert_eq!(vars["services.mail.port"], "4208");
    }

    #[test]
    fn exposes_neon_proxy_vars_and_ports() {
        let toml = r#"
name = "My App"
[services.postgres]
database = "my-app"
user = "my-app"
password = "p@ss word"
e2e_database = "my-app_e2e"
neon_proxy = true
"#;
        let ctx = test_ctx(toml, Some(4400));
        assert_eq!(ctx.port_allocations["postgres"], 4401);
        assert_eq!(ctx.port_allocations["postgres_neon"], 4402);

        let vars = ctx.template_vars();
        assert_eq!(vars["services.postgres.neon_port"], "4402");
        assert_eq!(
            vars["services.postgres.neon_url"],
            "postgresql://my-app:p%40ss%20word@127.0.0.1:4402/my-app"
        );
        assert_eq!(
            vars["services.postgres.neon_e2e_url"],
            "postgresql://my-app:p%40ss%20word@127.0.0.1:4402/my-app_e2e"
        );
    }

    #[test]
    fn renders_placeholders_and_arithmetic() {
        let vars = test_ctx(FULL, Some(4000)).template_vars();
        let rendered = render_template(
            "http://localhost:{{ ports.base + 10 }}/{{project.name}}",
            &vars,
        )
        .unwrap();
        assert_eq!(rendered, "http://localhost:4010/My App");
        assert_eq!(
            render_template("a {{ unterminated", &vars).unwrap(),
            "a {{ unterminated"
        );
    }

    #[test]
    fn reports_unknown_placeholders() {
        let ctx = test_ctx("name = \"p\"", Some(4300));
        let err = render_template(
            "{{services.unknown.port}}-{{ports.base + 70000}}",
            &ctx.template_vars(),
        )
        .unwrap_err();
        assert_eq!(err, ["services.unknown.port", "ports.base + 70000"]);
    }

    #[test]
    fn find_config_merges_local_override_file() {
        let temp =
            std::env::temp_dir().join(format!("ai-igniter-override-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let base_toml = r#"
name = "base-app"
env_file = ".env"
copy_files = []
base_port = 3000
"#;
        let local_toml = r#"
base_port = 4500
"#;
        std::fs::write(temp.join("ai-igniter.toml"), base_toml).unwrap();
        std::fs::write(temp.join("ai-igniter.local.toml"), local_toml).unwrap();

        let (loaded_path, config) = Config::find_config(&temp, &temp).unwrap();
        assert_eq!(loaded_path, temp.join("ai-igniter.toml"));
        assert_eq!(config.base_port, Some(4500));

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn resolve_root_path_resolves_main_or_workspace() {
        let ws = Path::new("/tmp/test-workspace");
        let main = Path::new("/tmp/main-checkout");

        // When main checkout is found by git
        let root_main = resolve_root_path(None, Some(main), ws);
        assert_eq!(root_main, main);

        // When main checkout is not found, falls back to workspace directory
        let root = resolve_root_path(None, None, ws);
        assert_eq!(root, ws);

        // When explicit CLI root is given
        let explicit = Path::new("/tmp/explicit-root");
        let root_cli = resolve_root_path(Some(explicit), Some(main), ws);
        assert_eq!(root_cli, explicit);
    }

    #[test]
    fn workspace_state_serde_roundtrip() {
        let state = WorkspaceState {
            version: 1,
            pid: 12345,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            workspace_path: PathBuf::from("/work/main"),
            compose_project: "my-app-main-abcd".to_string(),
            base_port: 4000,
            port_allocations: BTreeMap::from([
                ("base".to_string(), 4000u16),
                ("postgres".to_string(), 4001),
            ]),
            template_vars: BTreeMap::from([(
                "services.postgres.url".to_string(),
                "postgresql://u:p@127.0.0.1:4001/db".to_string(),
            )]),
            services: BTreeMap::from([(
                "postgres".to_string(),
                ServiceState {
                    status: "running".to_string(),
                    image: "postgres:17".to_string(),
                },
            )]),
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: WorkspaceState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.pid, 12345);
        assert_eq!(parsed.port_allocations["postgres"], 4001);
        assert_eq!(
            parsed.template_vars["services.postgres.url"],
            "postgresql://u:p@127.0.0.1:4001/db"
        );
        assert_eq!(parsed.services["postgres"].status, "running");
        assert_eq!(parsed.services["postgres"].image, "postgres:17");
    }

    #[test]
    fn workspace_state_write_read_roundtrip() {
        let dir = std::env::temp_dir().join("igniter-test-state-roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let state = WorkspaceState {
            version: 1,
            pid: 99,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            workspace_path: dir.clone(),
            compose_project: "test-proj".to_string(),
            base_port: 5000,
            port_allocations: BTreeMap::from([("base".to_string(), 5000u16)]),
            template_vars: BTreeMap::from([("key".to_string(), "val".to_string())]),
            services: BTreeMap::new(),
        };
        write_state(&dir, &state).unwrap();
        let loaded = read_state(&dir).unwrap();
        assert_eq!(loaded.pid, 99);
        assert_eq!(loaded.base_port, 5000);
        assert_eq!(loaded.template_vars["key"], "val");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_local_port_key_exact_and_prefix() {
        let locals = vec!["postgres".to_string()];
        assert!(is_local_port_key("postgres", &locals));
        assert!(is_local_port_key("postgres_neon", &locals));
        assert!(!is_local_port_key("garage", &locals));
        assert!(!is_local_port_key("garage_web", &locals));
    }

    #[test]
    fn is_local_port_key_empty_locals() {
        let locals: Vec<String> = vec![];
        assert!(!is_local_port_key("postgres", &locals));
        assert!(!is_local_port_key("base", &locals));
    }

    #[test]
    fn is_local_port_key_multiple_locals() {
        let locals = vec!["postgres".to_string(), "garage".to_string()];
        assert!(is_local_port_key("postgres", &locals));
        assert!(is_local_port_key("postgres_neon", &locals));
        assert!(is_local_port_key("garage", &locals));
        assert!(is_local_port_key("garage_web", &locals));
        assert!(!is_local_port_key("mail", &locals));
    }

    #[test]
    fn allocate_ports_partial_filters_non_local() {
        let toml_str = r#"
name = "app"
env_file = ".env"
copy_files = []
[services.postgres]
database = "app"
user = "app"
password = "p"
[services.garage]
access_key = "k"
secret_key = "s"
[workspace]
mode = "partial"
inherit_from = "root"
services_local = ["postgres"]
"#;
        let config: crate::config::Config = toml::from_str(toml_str).unwrap();
        let (ports, _) = crate::ports::allocate_ports(&config, 4000).unwrap();
        assert!(ports.contains_key("base"));
        assert!(ports.contains_key("postgres"));
        assert!(
            !ports.contains_key("garage"),
            "garage should be filtered out"
        );
        assert!(
            !ports.contains_key("garage_web"),
            "garage_web should be filtered out"
        );
    }

    #[test]
    fn allocate_ports_partial_empty_locals_only_base() {
        let toml_str = r#"
name = "app"
env_file = ".env"
copy_files = []
[services.postgres]
database = "app"
user = "app"
password = "p"
[workspace]
mode = "partial"
inherit_from = "root"
services_local = []
"#;
        let config: crate::config::Config = toml::from_str(toml_str).unwrap();
        let (ports, _) = crate::ports::allocate_ports(&config, 4000).unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports["base"], 4000);
    }

    #[test]
    fn merge_inherited_state_strips_base_and_locals() {
        let mut ctx = test_ctx(
            r#"
name = "app"
[services.postgres]
database = "app"
user = "app"
password = "p"
[services.garage]
access_key = "k"
secret_key = "s"
"#,
            Some(4000),
        );
        ctx.config.workspace.services_local = vec!["postgres".to_string()];

        let parent_state = WorkspaceState {
            version: 1,
            pid: 1000,
            started_at: "2026-01-01T00:00:00Z".to_string(),
            workspace_path: PathBuf::from("/work/main"),
            compose_project: "app-main-abcd".to_string(),
            base_port: 5000,
            port_allocations: BTreeMap::from([
                ("base".to_string(), 5000u16),
                ("postgres".to_string(), 5001),
                ("postgres_neon".to_string(), 5002),
                ("garage".to_string(), 5003),
                ("garage_web".to_string(), 5004),
            ]),
            template_vars: BTreeMap::from([
                (
                    "services.postgres.url".to_string(),
                    "parent-pg-url".to_string(),
                ),
                ("services.garage.web_port".to_string(), "5004".to_string()),
                ("project.name".to_string(), "app".to_string()),
            ]),
            services: BTreeMap::new(),
        };

        ctx.merge_inherited_state(&parent_state);

        // base never inherited
        assert_eq!(ctx.port_allocations["base"], 4000);
        // postgres keeps local allocation (not overwritten by parent 5001)
        assert_eq!(ctx.port_allocations["postgres"], 4001);
        // garage + garage_web come from parent
        assert_eq!(ctx.port_allocations["garage"], 5003);
        assert_eq!(ctx.port_allocations["garage_web"], 5004);

        let vars = ctx.parent_template_vars.as_ref().unwrap();
        // postgres vars filtered (local service)
        assert!(!vars.contains_key("services.postgres.url"));
        // garage vars inherited
        assert_eq!(vars["services.garage.web_port"], "5004");
        // non-service vars inherited
        assert_eq!(vars["project.name"], "app");
    }
}
