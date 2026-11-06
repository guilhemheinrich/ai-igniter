use crate::cli::DevArgs;
use crate::context::{
    WorkspaceContext, build_workspace_state, check_pid_alive, read_state, write_state,
};
use crate::docker::{DockerCompose, reclaim_stale_ports};
use crate::env_writer::EnvWriter;
use crate::lock::DevLock;
use crate::services::get_active_services;
use crate::supervisor::{ShutdownSignal, Supervisor};
use anyhow::{Context, Result, bail};
use colored::Colorize;

pub fn execute_dev(ctx: &mut WorkspaceContext, args: &DevArgs) -> Result<()> {
    let _lock = DevLock::acquire(&ctx.compose_project)?;
    let shutdown = ShutdownSignal::install()?;
    let is_partial = ctx.config.workspace.is_partial();
    let services_local = ctx.config.workspace.services_local.clone();
    let has_local_services = !is_partial || !services_local.is_empty();

    // --- Partial mode: validate parent and merge inherited state ---
    if is_partial {
        let parent_path = ctx
            .parent_path
            .as_ref()
            .expect("partial mode always has parent_path")
            .clone();

        let parent_state = read_state(&parent_path).with_context(|| {
            format!(
                "Parent workspace at {} is not running.\n\
                 Hint: start it with `cd {} && ai-igniter dev`",
                parent_path.display(),
                parent_path.display()
            )
        })?;

        if !check_pid_alive(parent_state.pid) {
            #[cfg(windows)]
            eprintln!(
                "{} Warning: cannot verify parent liveness on Windows; assuming parent is running",
                "[dev]".yellow().bold()
            );
            #[cfg(not(windows))]
            bail!(
                "Parent workspace at {} is not running (PID {} is dead).\n\
                 Hint: start it with `cd {} && ai-igniter dev`",
                parent_path.display(),
                parent_state.pid,
                parent_path.display()
            );
        }

        // Validate required inherited services exist in parent
        for (name, _) in ctx.config.port_offsets() {
            if is_local_service(&name, &services_local) {
                continue;
            }
            let base_service = name.split('_').next().unwrap_or(&name);
            if !parent_state.services.contains_key(base_service)
                && !parent_state.port_allocations.contains_key(&name)
            {
                bail!(
                    "Service \"{name}\" is not running in parent workspace at {}.\n\
                     Hint: start the parent with `ai-igniter dev` first, or add \"{name}\" to services_local",
                    parent_path.display()
                );
            }
        }

        ctx.merge_inherited_state(&parent_state);

        println!(
            "{} Partial mode: inheriting services from {}",
            "[dev]".blue().bold(),
            parent_path.display().to_string().cyan()
        );
    }

    println!(
        "{} Starting dev environment for project '{}' (project: {})...",
        "[dev]".blue().bold(),
        ctx.config.name,
        ctx.compose_project.cyan()
    );

    if has_local_services {
        // 1. Free our ports from other ai-igniter projects
        reclaim_stale_ports(ctx);

        // 2. Regenerate compose from the current config
        let compose = DockerCompose::new(ctx)?;
        if args.reset {
            println!(
                "{} Resetting services for project '{}' (wiping volumes)...",
                "[dev]".yellow().bold(),
                ctx.compose_project.cyan()
            );
            compose.down(true)?;
        }

        // 3. Seed workspace files and write the managed environment file
        EnvWriter::write_workspace_env(ctx)?;

        // 4. Write state file
        let state = build_workspace_state(ctx);
        write_state(&ctx.workspace_path, &state)?;

        // 5. Start, initialize and supervise
        let dev_command = resolve_dev_command(ctx, args)?;
        let result = start_and_supervise(ctx, &compose, &shutdown, dev_command.as_deref());

        // Update state on clean shutdown
        update_state_stopped(ctx);

        if let Err(e) = compose.stop() {
            eprintln!("{} Warning: {:#}", "[dev]".yellow().bold(), e);
        }

        if shutdown.requested() { Ok(()) } else { result }
    } else {
        if args.reset {
            println!(
                "{} No local services to reset in partial mode.",
                "[dev]".yellow().bold()
            );
        }

        // Seed workspace files and write the managed environment file
        EnvWriter::write_workspace_env(ctx)?;

        // Write state file (no compose services, just the dev command)
        let state = build_workspace_state(ctx);
        write_state(&ctx.workspace_path, &state)?;

        // Only run the dev command — no Docker operations
        let dev_command = resolve_dev_command(ctx, args)?;

        if let Some(cmd) = &dev_command {
            ctx.interpolate(cmd)?;
        }

        let result = Supervisor::run_dev(ctx, None, &shutdown, dev_command.as_deref());

        update_state_stopped(ctx);

        if shutdown.requested() { Ok(()) } else { result }
    }
}

fn is_local_service(name: &str, services_local: &[String]) -> bool {
    services_local
        .iter()
        .any(|svc| name == svc || name.starts_with(&format!("{svc}_")))
}

fn resolve_dev_command(ctx: &WorkspaceContext, args: &DevArgs) -> Result<Option<String>> {
    let dev_command = if args.no_command {
        None
    } else if !args.command.is_empty() {
        Some(args.command.join(" "))
    } else {
        ctx.config.dev_command.clone()
    };

    if let Some(cmd) = &dev_command {
        ctx.interpolate(cmd)?;
    }

    Ok(dev_command)
}

fn update_state_stopped(ctx: &WorkspaceContext) {
    if let Ok(mut state) = read_state(&ctx.workspace_path) {
        for svc in state.services.values_mut() {
            svc.status = "stopped".to_string();
        }
        let _ = write_state(&ctx.workspace_path, &state);
    }
}

fn start_and_supervise(
    ctx: &WorkspaceContext,
    compose: &DockerCompose<'_>,
    shutdown: &ShutdownSignal,
    dev_command: Option<&str>,
) -> Result<()> {
    compose.up()?;

    let filter: Option<&[String]> = if ctx.config.workspace.is_partial() {
        Some(&ctx.config.workspace.services_local)
    } else {
        None
    };

    for service in get_active_services(ctx, filter) {
        if shutdown.requested() {
            return Ok(());
        }
        service
            .post_start(ctx, compose)
            .with_context(|| format!("Service '{}' failed to initialize", service.name()))?;
    }

    if shutdown.requested() {
        return Ok(());
    }
    Supervisor::run_dev(ctx, Some(compose), shutdown, dev_command)
}
