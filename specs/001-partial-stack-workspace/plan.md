# Plan — SPEC-001: Partial-Stack Workspace Mode

**Post adversarial review R1 — corrections integrated.**

## Approach

Inject partial-stack awareness into the existing resolution pipeline at the narrowest
possible points. The current flow is linear:

```
Config::load → WorkspaceContext::from_parts → allocate_ports → build_compose → EnvWriter
```

Partial mode adds a **split-phase resolution**:

1. **Config-time (in `resolve`):** parse the `[workspace]` section, resolve the parent path, allocate ports for local services only.
2. **Dev-time (in `execute_dev`):** read the parent's state file, validate liveness, merge inherited ports and template variables into the context.

This split is the central design choice. Status and teardown work with the config-time context alone. Only `dev` needs the parent alive.

## Shared File Analysis

| File | Current callers | Partial-mode change | Risk |
|------|----------------|---------------------|------|
| `src/config.rs` | `Config::load` from `context.rs:37` | Add `WorkspaceConfig` struct + serde; new validation rules in `Config::validate` (line 171) | Low — additive struct, validation extends existing method |
| `src/context.rs` | `resolve` (line 24), `from_parts` (line 67), `allocate_ports` (line 251), `template_vars` (line 106) | Add `WorkspaceState`/`ParentContext` structs, state file I/O, modify `allocate_ports` to accept optional filter, add inherited port/var merge method | Medium — see callers audit below |
| `src/docker/compose.rs` | `build_compose` (line 240), `write_generated_files` (line 223) | Guard `contribute_compose` calls by `services_local` membership; skip when empty | Low |
| `src/env_writer.rs` | `compute_env` (line 84), `write_workspace_env` (line 110) | No structural change — `template_vars()` already drives it; inherited vars arrive through the context merge | None |
| `src/commands/dev.rs` | `execute_dev` (line 10) | Insert parent validation + state merge after resolve, before compose; write state after post-start; skip compose.up when no local services | Medium — new logic in the main flow |
| `src/commands/teardown.rs` | `execute_teardown` (line 8) | Guard: skip compose when no local services, print message | Low |
| `src/commands/status.rs` | `execute_status` (line 6) | Add inherited services section; read parent state gracefully | Low |
| `src/supervisor.rs` | `Supervisor::run_dev` (line 48) | Write state file update on clean shutdown | Low |
| `src/services/mod.rs` | `get_active_services` (line 77) | Filter by `services_local` in partial mode | Low |

## `from_parts` Callers Audit

The plan modifies `allocate_ports` (called from `from_parts`) to accept an optional
service filter. This does NOT change `from_parts` signature — the filter is derived
from `config.workspace` inside `allocate_ports`.

**Approach: no signature change to `from_parts`.** Instead, `allocate_ports` reads
`config.workspace.services_local` internally when `config.workspace.mode == Partial`.
All callers remain unchanged:

| Caller | Location | Impact |
|--------|----------|--------|
| `WorkspaceContext::resolve` | `context.rs:57` via `from_parts` | None — filter derived from config |
| `test_ctx` | `context.rs:353` | None — test configs default to `WorkspaceMode::Full` |
| `rejects_port_overflow` test | `context.rs:430` | None — same |
| env_writer tests | various | None — use `test_ctx` |

This avoids the compilation breakage both reviewers flagged.

## Implementation Strategy

### Step 1: Config parsing (`config.rs`)

Add after the `Config` struct fields (line 44):

```rust
#[serde(default)]
pub workspace: WorkspaceConfig,
```

New types (with `Default` derive for backward compat):

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub mode: WorkspaceMode,
    pub inherit_from: Option<String>,
    #[serde(default)]
    pub services_local: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceMode {
    #[default]
    Full,
    Partial,
}
```

Validation additions in `Config::validate` (after line 219):
- `mode == Partial` requires `inherit_from` to be `Some`.
- Each entry in `services_local` must match a builtin name or a key in `services.custom`.
- `mode == Partial` and `compose_file.is_some()` → reject.
- `mode == Full` ignores `inherit_from` and `services_local`.

Self-inheritance check happens later, in `resolve_parent_path`, not in config validation
(because it requires the resolved workspace path).

### Step 2: State file (`context.rs`)

New structs `WorkspaceState`, `ServiceState`, `ParentContext` as specified.

New functions:
- `write_state(dir: &Path, state: &WorkspaceState) -> Result<()>`: atomic write via
  temp file + `fs::rename`. On Windows, remove destination first.
- `read_state(dir: &Path) -> Result<WorkspaceState>`: read + deserialize + version check.
- `check_pid_alive(pid: u32) -> bool`:
  - Unix: `unsafe { libc::kill(pid as i32, 0) }` — returns true if 0 OR `EPERM`
    (process exists but different user). Only `ESRCH` means dead.
  - Windows: use `OpenProcess` or `tasklist` — return true on failure (best-effort).

### Step 3: Parent resolution (`context.rs`)

New function `resolve_parent_path`:

```rust
pub fn resolve_parent_path(
    inherit_from: &str,
    workspace_path: &Path,
) -> Result<PathBuf>
```

Three cases:
1. `"root"` → `git_main_checkout(workspace_path)`. Fail if `None` (bare repo or non-git).
2. Absolute path → validate it contains a config file.
3. Relative name → parse `git worktree list --porcelain`, match basename.

After resolution: `if parent == workspace_path { bail!("cannot inherit from itself") }`.

### Step 4: Port allocation split (`context.rs`)

Modify `allocate_ports` (line 251). The function currently iterates all `config.port_offsets()`.

In partial mode, filter the offsets:

```rust
fn allocate_ports(config: &Config, base_port: u16) -> Result<BTreeMap<String, u16>> {
    let mut ports = BTreeMap::from([("base".to_string(), base_port)]);
    let offsets = config.port_offsets();
    let filter = partial_port_filter(config);
    for (name, offset) in &offsets {
        if !filter(&name) { continue; }
        // ... existing allocation logic
    }
    Ok(ports)
}
```

The `partial_port_filter` returns a closure that:
- In full mode: accepts all names.
- In partial mode: accepts a name if it matches a `services_local` entry or is a
  companion (prefix match: `name == svc` or `name.starts_with(&format!("{svc}_"))`).

### Step 5: Inherited state merge in `execute_dev`

New method on `WorkspaceContext`:

```rust
pub fn merge_inherited_state(&mut self, parent: &WorkspaceState) {
    let locals: HashSet<&str> = /* services_local + companions */;
    for (key, port) in &parent.port_allocations {
        if key == "base" || locals.contains(key.as_str()) { continue; }
        self.port_allocations.insert(key.clone(), *port);
    }
    // template_vars are merged at render time, not stored on ctx
}
```

`template_vars()` gains a `parent_vars: Option<&BTreeMap<String, String>>` stored
on the context. When present, inherited vars are added before local provider vars,
so local vars win on collision.

### Step 6: Compose filtering (`docker/compose.rs`)

In `build_compose` (line 240), guard the loops at lines 245 and 249:
- If `mode == Partial`, skip providers and custom services not in `services_local`.

In `write_generated_files` (line 223), guard `write_auxiliary_files` at line 228.

When `services_local` is empty and `mode == Partial`, `build_compose` produces
`{ "services": {} }`. `execute_dev` skips `compose.up()` entirely in this case.

### Step 7: Command changes

**`dev.rs`** — between `ShutdownSignal::install()` and `reclaim_stale_ports`:
1. If partial mode:
   a. `read_state(parent_path)` — fail if absent.
   b. `check_pid_alive(state.pid)` — fail if dead (warn on Windows).
   c. Validate all required inherited services exist in parent state.
   d. `ctx.merge_inherited_state(&state)`.
2. If `services_local` is empty: skip `reclaim_stale_ports`, `compose.up()`, and post-start hooks. Go straight to env write and supervisor.

State write: after post-start hooks succeed (or immediately after env write when no
compose), build `WorkspaceState` from `ctx.template_vars()` + `ctx.service_images()`.

**`teardown.rs`** — when partial + `services_local` empty, skip `compose.down()`.
Always delete `.igniter/`. Print message.

**`status.rs`** — after local section, attempt `read_state(parent_path)`:
- Success + PID alive: print inherited ports with "running (via parent)".
- Success + PID dead: print inherited ports with "unknown (parent not responding)".
- File absent: print "(parent state unavailable)".

### Step 8: Service filtering

`get_active_services` (services/mod.rs:77): add a `local_only` filter parameter.
In partial mode, only providers whose name is in `services_local` return their service.

## Constitution Waivers

None.

## Risks

1. **`template_vars()` receives parent vars** — a new `Option` field on `WorkspaceContext`.
   Mitigated: `None` in full mode preserves current behavior exactly.

2. **PID liveness on macOS/Linux** — `libc::kill` with `EPERM` now returns true (process
   exists, different user). Previous plan returned false on `EPERM`, which would reject
   a live parent started by a different sudo elevation. Fixed.

3. **Empty compose in partial mode** — `compose.up()` on an empty services map is a Docker
   no-op, not an error. But we skip it explicitly for clarity and performance.

4. **State file left behind on crash** — PID check detects this. `teardown` still works
   because it deletes `.igniter/` unconditionally.

5. **`reclaim_stale_ports` timing** — In partial mode, `reclaim_stale_ports` runs BEFORE
   inherited ports are merged. It only reclaims ports in `ctx.port_allocations`, which
   at that point contains only local ports. Parent ports are never reclaimed.

## Test Plan

Unit tests (Rust `#[cfg(test)]`):
- `WorkspaceConfig` parsing: full (default), partial with all fields, partial missing
  `inherit_from` (error), partial with `compose_file` (error), unknown service name
  in `services_local` (error).
- `WorkspaceState` serde roundtrip (including `template_vars` field).
- `allocate_ports` with partial filter: only local + companion ports allocated.
- `build_compose` with partial config: inherited services absent from compose output.
- `build_compose` with empty `services_local`: empty services map.
- `template_vars` with inherited parent vars: inherited vars present, local vars win
  on collision.
- Self-inheritance detection.
- Companion port prefix matching.
- PID liveness: `EPERM` returns true (process exists).

Integration tests (Docker, `#[test]` with `docker compose`):
- Skipped in CI unless `AI_IGNITER_INTEGRATION=1` is set.
- Full dev → partial dev → verify partial only starts local containers.
- Partial dev with empty `services_local` → no Docker operations.
- Teardown partial → verify parent containers survive.
- Status with dead parent → shows "unknown".
