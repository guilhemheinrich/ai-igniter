use crate::context::{WorkspaceContext, check_pid_alive, read_state};
use crate::docker::DockerCompose;
use anyhow::Result;
use colored::Colorize;

pub fn execute_status(ctx: &WorkspaceContext) -> Result<()> {
    println!(
        "{}",
        "═══════════════════════════════════════════════════════".cyan()
    );
    println!("  {} Workspace Status", "ai-igniter:".bold());
    println!(
        "{}",
        "═══════════════════════════════════════════════════════".cyan()
    );
    println!("  Project Name:     {}", ctx.config.name.bold());
    println!("  Compose Project:  {}", ctx.compose_project.yellow());
    println!("  Workspace Path:   {}", ctx.workspace_path.display());
    println!("  Root Path:        {}", ctx.root_path.display());
    println!("  Config Path:      {}", ctx.config_path.display());
    println!("  Base Port:        {}", ctx.base_port);
    println!();
    let images = ctx.service_images();
    println!("  Allocated Ports:");
    for (name, port) in &ctx.port_allocations {
        let image_info = images
            .get(name)
            .map(|img| format!(" ({img})"))
            .unwrap_or_default();
        println!(
            "    - {:<16} : {}{}",
            name.bold(),
            port.to_string().cyan(),
            image_info.dimmed()
        );
    }
    println!();

    let compose = DockerCompose::new(ctx)?;
    match compose.ps() {
        Ok(containers) if containers.is_empty() => {
            println!(
                "  Containers: {}",
                "No containers for this project".yellow()
            );
        }
        Ok(containers) => {
            println!("  Containers:");
            for c in &containers {
                let field = |key: &str| c[key].as_str().unwrap_or("").to_string();
                let state = field("State");
                let state_colored = if state == "running" {
                    state.green()
                } else {
                    state.red()
                };
                println!(
                    "    - {:<14} (service: {:<10}) [{}] {} {}",
                    field("Name").cyan(),
                    field("Service"),
                    state_colored,
                    field("Health").yellow(),
                    field("Status").dimmed()
                );
            }
        }
        Err(e) => {
            println!(
                "  Containers: {} ({:#})",
                "Could not inspect Docker containers".red(),
                e
            );
        }
    }

    // Inherited services section for partial workspaces
    if ctx.config.workspace.is_partial() {
        println!();
        if ctx.config.workspace.services_local.is_empty() {
            println!(
                "  Local services: {}",
                "(none — only dev_command runs locally)".dimmed()
            );
        }
        println!("  Inherited services:");
        if let Some(parent_path) = &ctx.parent_path {
            match read_state(parent_path) {
                Ok(parent_state) => {
                    let parent_alive = check_pid_alive(parent_state.pid);
                    let inherited: Vec<_> = parent_state
                        .services
                        .iter()
                        .filter(|(name, _)| !ctx.config.workspace.services_local.contains(name))
                        .collect();
                    if inherited.is_empty() {
                        println!("    (no inherited services)");
                    } else {
                        for (name, svc) in inherited {
                            let port_info = parent_state
                                .port_allocations
                                .get(name)
                                .map(|p| format!(":{p}"))
                                .unwrap_or_default();
                            let status = if parent_alive && svc.status == "running" {
                                "running (via parent)".green()
                            } else {
                                "unknown (parent not responding)".yellow()
                            };
                            println!("    - {:<16}{} [{}]", name.bold(), port_info.cyan(), status);
                        }
                    }
                }
                Err(_) => {
                    println!(
                        "    {}",
                        "Could not read parent state (parent not running?)".yellow()
                    );
                }
            }
        } else {
            println!("    {}", "Parent path not resolved".yellow());
        }
    }

    println!(
        "{}",
        "═══════════════════════════════════════════════════════".cyan()
    );
    Ok(())
}
