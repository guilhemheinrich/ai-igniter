use crate::context::WorkspaceContext;
use crate::docker::DockerCompose;
use crate::lock::DevLock;
use anyhow::{Context, Result};
use colored::Colorize;
use std::fs;
use std::io::ErrorKind;

/// Only removes this workspace's own project: other projects are never touched, even if they share ports.
pub fn execute_teardown(ctx: &WorkspaceContext) -> Result<()> {
    let is_partial = ctx.config.workspace.is_partial();
    let has_local_services = !is_partial || !ctx.config.workspace.services_local.is_empty();

    if has_local_services {
        let compose = DockerCompose::new(ctx)?;
        compose.down(true)?;
    } else {
        println!(
            "{} No local Docker services to tear down in partial mode.",
            "[teardown]".blue().bold(),
        );
    }

    // Always remove .igniter/ (contains state.json and generated compose)
    if ctx.config.compose_file.is_none() {
        match fs::remove_dir_all(ctx.igniter_dir()) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("Failed to remove {:?}", ctx.igniter_dir()));
            }
            _ => {}
        }
    }

    DevLock::remove(&ctx.compose_project);
    if is_partial && let Some(parent_path) = &ctx.parent_path {
        println!(
            "{} Tore down local services. Inherited services from {} are still running.",
            "[teardown]".blue().bold(),
            parent_path.display(),
        );
    }

    Ok(())
}
