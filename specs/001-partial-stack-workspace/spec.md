# SPEC-001: Partial-Stack Workspace Mode

**Status:** DRAFT — post adversarial review (R1: GPT-Sol + Grok convergence)
**Priority:** P1
**Author:** Guilhem Heinrich
**Created:** 2026-09-24

---

## Summary

AI Igniter assumes full isolation per workspace: every worktree gets its own complete set of Docker containers. Partial-stack mode lets a workspace inherit running services from another workspace (typically the root checkout) and only spin up the services it needs to iterate on.

Concrete scenario: a Claude Code session opens a worktree to tweak a React component. It needs its own `npm run dev` on a free port, but PostgreSQL, Redis, and the backend API from the root checkout are fine to share. Today AI Igniter forces full stack duplication — 4+ extra containers, 500 MB+ RAM, 30 s startup — for a CSS change.

---

## Functional Requirements

### FR-1: Workspace Mode Configuration

The `ai-igniter.toml` accepts a new `[workspace]` section:

```toml
# Typical case: only the host dev command runs locally, all Docker services inherited
[workspace]
mode = "partial"
inherit_from = "root"

# Alternate case: run a local mail server alongside the host dev command
[workspace]
mode = "partial"
inherit_from = "root"
services_local = ["mail"]
```

**Behavior:**
- `mode = "full"` or absent: current behavior, no change.
- `mode = "partial"`: only services listed in `services_local` get their own containers. All other configured services are inherited from the workspace identified by `inherit_from`. An empty `services_local` (the default) means "inherit all Docker services" — the workspace runs only its `dev_command` on the host, which is the primary use case.

**Clarification:** `services_local` names Docker Compose services, not the host `dev_command`. The `dev_command` always runs locally on the workspace's own `base_port` — it is not a Docker service and is never inherited.

**Validation rules:**
- `mode = "partial"` requires `inherit_from` to be present.
- Each name in `services_local` must match either a builtin service name (`postgres`, `garage`) or a key in `[services.custom.*]`.
- `inherit_from = "root"` resolves to `git_main_checkout()` — the same mechanism `context.rs:338` already uses.
- `inherit_from` as an absolute path must point to a directory containing an `ai-igniter.toml` or `.ai-igniter.toml`.
- `inherit_from` as a relative name resolves to a sibling worktree via `git worktree list`.
- The resolved parent path must differ from `workspace_path`. Self-inheritance is rejected:
  ```
  Error: partial workspace cannot inherit from itself
  Hint: use a worktree (`git worktree add`) and configure partial mode there
  ```
- `mode = "partial"` is incompatible with `compose_file`. When both are set, validation fails:
  ```
  Error: partial mode cannot be used with an external compose_file
  Hint: remove compose_file or set mode = "full"
  ```

### FR-2: Root Workspace State File

When `ai-igniter dev` starts in any mode, it writes a state file at `.igniter/state.json`:

```json
{
  "version": 1,
  "pid": 12345,
  "started_at": "2026-09-24T10:30:00Z",
  "workspace_path": "/home/dev/project",
  "compose_project": "myapp-main-a1b2c3d4",
  "base_port": 20000,
  "port_allocations": {
    "base": 20000,
    "postgres": 20001,
    "postgres_neon": 20002,
    "garage": 20003,
    "garage_web": 20004
  },
  "template_vars": {
    "services.postgres.url": "postgresql://app:pass@127.0.0.1:20001/app",
    "services.postgres.e2e_url": "postgresql://app:pass@127.0.0.1:20001/app_e2e",
    "services.postgres.neon_port": "20002",
    "services.garage.web_port": "20004"
  },
  "services": {
    "postgres": { "status": "running", "image": "postgres:16" },
    "garage": { "status": "running", "image": "dxflrs/garage:v2.4.1" }
  }
}
```

**Lifecycle:**
- Written after `compose.up()` succeeds and post-start hooks complete.
- Updated on clean shutdown (status set to `"stopped"`).
- Deleted by `teardown`.
- The `pid` field records the supervisor process PID for liveness checks.

**The `template_vars` field** captures all service-contributed template variables at the moment the parent starts. A partial workspace uses these directly instead of calling `contribute_template_vars` for inherited services — which would produce URLs with the child's credentials instead of the parent's. This resolves the config-mismatch problem (EC-6) without requiring the child to read the parent's config.

### FR-3: Inherited Service Discovery

When `mode = "partial"`, `ai-igniter dev` reads the parent workspace's `.igniter/state.json` before starting.

**Discovery steps:**
1. Resolve `inherit_from` to an absolute path.
2. Read `<parent_path>/.igniter/state.json`.
3. Validate: file exists, `version` is supported, PID is alive (`kill -0`).
4. For each service NOT in `services_local`, look it up in the parent's `services` map. If the service exists and its status is `"running"`, inherit its ports from `port_allocations` and its template variables from `template_vars`.
5. If any required inherited service is missing from the parent's state, fail with:
   ```
   Error: service "redis" is not running in parent workspace at /home/dev/project
   Hint: start the parent with `ai-igniter dev` first, or add "redis" to services_local
   ```

**Required inherited services** are those configured and enabled in the local `ai-igniter.toml` that are NOT in `services_local`. A service disabled in the local config (e.g. `[services.postgres] enabled = false`) is not required from the parent.

**Port inheritance includes companion ports.** When `postgres` is inherited, `postgres_neon` (if present in the parent's allocations) is also inherited. When `garage` is inherited, `garage_web` is also inherited. The companion mapping is: service name → all port allocation keys that start with `{service_name}_`.

### FR-4: Partial Compose Generation

`DockerCompose::new(ctx)` in partial mode generates a compose file containing only the services listed in `services_local`.

- Custom services in `services_local` contribute their compose definitions as today.
- Builtin services in `services_local` contribute their compose definitions as today.
- Services NOT in `services_local` are omitted from compose entirely.
- The compose project name includes a `partial` marker: `{name}-{slug}-{hash}-partial`.
- When `services_local` is empty, the compose file contains no services. `compose.up()` is skipped — no Docker operations occur.

### FR-5: Partial Env Writing

`EnvWriter::write_workspace_env(ctx)` in partial mode:

- Template variables for local services come from the local port allocation and `contribute_template_vars` (as today).
- Template variables for inherited services come from the parent's `state.json` `template_vars` field. These include full URLs with the parent's credentials.
- Both sets merge into the same template context, so `{{ports.postgres}}` resolves to the parent's postgres port and `{{ports.base}}` resolves to the local base port.

The managed `.env` block contains the computed values. No distinction between local and inherited ports appears in the env file — the application does not care where a service runs.

### FR-6: Partial Teardown

`ai-igniter teardown` in partial mode:

- Tears down only the local compose project (the one with the `-partial` suffix).
- Never touches the parent workspace's containers.
- Deletes the local `.igniter/` directory (compose file, state file).
- Prints a message: `Tore down local services. Inherited services from <parent_path> are still running.`

**`teardown` and `status` must work even when the parent is dead.** Parent liveness validation happens only in `ai-igniter dev`, not during workspace context resolution. `teardown` reads the config to know it is partial mode, but does not validate the parent. `status` reads the parent's state file if available, and prints `"unknown (parent not responding)"` if it is stale or absent.

### FR-7: Partial Status Display

`ai-igniter status` in partial mode displays two sections:

```
=== Local services (this workspace) ===
  mail        localhost:20068    running (healthy)

=== Inherited services (from /home/dev/project) ===
  postgres    localhost:20001    running (via parent)
  garage      localhost:20003    running (via parent)
```

When `services_local` is empty:
```
=== Local services (this workspace) ===
  (none — only dev_command runs locally)

=== Inherited services (from /home/dev/project) ===
  postgres    localhost:20001    running (via parent)
  garage      localhost:20003    running (via parent)
```

- Local services show live Docker container state (as today).
- Inherited services show port allocations from the parent's state file. Status is `"running (via parent)"` if the parent PID is alive and the state file says running, or `"unknown (parent not responding)"` if the PID is dead or the state file is stale.

### FR-8: Parent Liveness Validation

**Only `ai-igniter dev` validates parent liveness.** This check is NOT in `WorkspaceContext::resolve` — it is in `execute_dev` after resolution succeeds.

1. Read parent's `state.json`.
2. Check `pid` with `kill -0 <pid>`.
3. If the PID is dead, fail:
   ```
   Error: parent workspace at /home/dev/project is not running (PID 12345 is dead)
   Hint: start it with `cd /home/dev/project && ai-igniter dev`
   ```

No background polling during the dev loop. If the parent dies mid-session, the partial workspace's services keep running — the inherited URLs become unreachable, which the developer will notice immediately via connection errors.

### FR-9: Post-start Hook Filtering

In partial mode, `get_active_services(ctx)` returns only services in `services_local`. Inherited services do not get their `post_start` called — the parent already ran migrations, seeds, and bucket setup.

---

## Non-Functional Requirements

### NFR-1: Backward Compatibility

The default `mode` is `"full"` when the `[workspace]` section is absent. Existing `ai-igniter.toml` files require zero changes.

### NFR-2: Startup Overhead

Partial mode startup must be faster than full mode by at least the time saved from not starting inherited containers. No additional Docker operations are allowed for inherited services.

### NFR-3: State File Atomicity

`state.json` is written atomically (write to temp file, then rename) to prevent partial reads by concurrent workspaces. On Windows, `fs::rename` does not replace an existing file — use `tempfile` crate or manual remove-then-rename with a brief race window documented.

### NFR-4: Additive ServiceProvider Changes

The `ServiceProvider` trait must not gain new required methods. All partial-mode logic is handled by the caller (`WorkspaceContext`, `DockerCompose`, `EnvWriter`), not by individual providers.

### NFR-5: Platform Support

Partial mode is fully supported on macOS and Linux. On Windows, PID liveness checks are best-effort (using `tasklist` or `OpenProcess`). A stale state file on Windows produces a warning rather than a hard failure:
```
Warning: cannot verify parent liveness on Windows; assuming parent is running
```

---

## Technical Design

### Affected Files and Scope

| File | Change Type | Description |
|------|-------------|-------------|
| `src/config.rs` | Modified | Add `WorkspaceConfig` struct, parse `[workspace]` section, validate partial-mode constraints |
| `src/context.rs` | Modified | Add `WorkspaceState` + `ParentContext`, parent resolution, split port allocation, state file write/read |
| `src/docker/compose.rs` | Modified | Filter services by `services_local` in partial mode; skip compose.up when empty |
| `src/env_writer.rs` | Modified | No structural change — `template_vars()` already drives it; the partial merge happens upstream in context |
| `src/commands/dev.rs` | Modified | Parent liveness check (here, not in resolve), write state file after start, skip compose.up when no local services |
| `src/commands/teardown.rs` | Modified | Skip parent teardown, clean local state only |
| `src/commands/status.rs` | Modified | Two-section display for partial mode |
| `src/supervisor.rs` | Modified | Write state file on clean shutdown (status update) |
| `src/services/mod.rs` | Modified | Filter `get_active_services` by `services_local` in partial mode |

### New Types

```rust
// config.rs
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

// context.rs
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

/// Resolved parent workspace info, used only during `dev` startup.
#[derive(Debug, Clone)]
pub struct ParentContext {
    pub path: PathBuf,
    pub state: WorkspaceState,
}
```

### Workspace Resolution Flow (partial mode)

```
WorkspaceContext::resolve(args)
  ├─ resolve workspace_path, config, root_path (unchanged)
  ├─ if config.workspace.mode == Partial:
  │   ├─ resolve_parent_path(inherit_from) → store in ctx
  │   ├─ allocate_ports() for services_local only
  │   └─ (parent state NOT read here — defer to execute_dev)
  └─ else:
      └─ allocate_ports() for all services (unchanged)

execute_dev(ctx):
  ├─ if partial:
  │   ├─ read_parent_state(ctx.parent_path)
  │   ├─ validate_parent_liveness(state.pid)
  │   ├─ validate_inherited_services(config, state)
  │   └─ merge inherited ports + template_vars into ctx
  ├─ (rest of dev flow)
```

**Key design decision:** Parent state is read in `execute_dev`, NOT in `WorkspaceContext::resolve`. This allows `status` and `teardown` to proceed even when the parent is dead. The `WorkspaceContext` in partial mode initially has only local port allocations; `execute_dev` enriches it with inherited ports before generating compose/env.

### State File Path Convention

`<workspace_path>/.igniter/state.json`

Every workspace writes its own state file during `dev`. The file exists only while `dev` is active (or after unclean shutdown — stale files are detected by PID check).

### Parent Resolution (`inherit_from`)

| Value | Resolution |
|-------|-----------|
| `"root"` | `git_main_checkout()` — already implemented in `context.rs:338`. Fails if returns `None` (bare repo). |
| Absolute path | Used directly; must contain an ai-igniter config |
| Relative name | `git worktree list --porcelain`, match by basename |

After resolution, validate `parent_path != workspace_path` (self-inheritance guard).

### Port Allocation Split

In partial mode, `allocate_ports` computes ports only for services in `services_local`. When `services_local` is empty, only `("base", base_port)` is allocated.

Inherited ports are merged later, during `execute_dev`, after reading the parent's state. The merge:
1. Strips the parent's `base` entry (the child's base port is always local).
2. Strips entries whose key matches a `services_local` member or its companions.
3. Inserts the remaining parent entries into `port_allocations`.

### Companion Port Mapping

A service in `services_local` needs all its port allocation keys, not just the service name. The mapping is derived from the existing `port_offsets` convention:
- `postgres` → also `postgres_neon` (if Neon is enabled)
- `garage` → also `garage_web`
- Custom services → only their own name (no companions)

The implementation uses prefix matching: for each name in `services_local`, include all allocation keys where `key == name` or `key.starts_with(&format!("{name}_"))`.

### Compose Project Naming

- Full mode: `{name}-{slug}-{hash}` (unchanged)
- Partial mode: `{name}-{slug}-{hash}-partial`

### Docker Networking Caveat

**Inherited services are reachable from the host `dev_command` via `127.0.0.1:{port}`.** This covers the primary use case — a React dev server on the host talking to a PostgreSQL container via `DATABASE_URL=postgresql://…@127.0.0.1:20001/…`.

**Local Docker containers in `services_local` CANNOT reach inherited containers via Docker Compose DNS.** They run on different Compose networks. If a local container needs to reach an inherited service, it must use the host gateway (`host.docker.internal` or `172.17.0.1`), which the user must configure themselves. This limitation is documented but not solved — solving it requires shared network mode (out of scope).

---

## Edge Cases

### EC-1: Parent Restarts with New Ports

**Scenario:** Root workspace restarts and gets new ports. The partial workspace's `.env` contains stale ports.

**Decision:** No automatic re-sync. The partial workspace must re-run `ai-igniter dev` (or `ai-igniter env --write`) to pick up the new parent ports.

### EC-2: Multiple Partial Workspaces from Same Parent

**Scenario:** Two partial workspaces inherit from the same root simultaneously.

**Decision:** Fully supported. Each partial workspace reads the same `state.json` (read-only). Each has its own compose project (`-partial` suffix + unique hash). Local port collisions are prevented by different workspace paths producing different base ports via `derived_base_port`.

**Caveat:** The 2000-slot hash space means collisions are possible (~0.05% per pair). This is a pre-existing limitation of `derived_base_port`, not introduced by this feature. When a collision occurs, `reclaim_stale_ports` resolves it — but only if the conflicting workspace is stale. Two active workspaces on the same slot need explicit `base_port` in one config.

### EC-3: Inherited Service Not in Parent

**Scenario:** Partial workspace expects `redis` (custom service) but the parent does not run it.

**Decision:** Hard error at startup. Message names the missing service and suggests either starting it in the parent or adding it to `services_local`.

### EC-4: `--reset` in Partial Mode

**Scenario:** `ai-igniter dev --reset` in a partial workspace.

**Decision:** Reset only local services. `--reset` runs `docker compose down --volumes` on the local compose project only. When `services_local` is empty, `--reset` is a no-op with a message:
```
[dev] No local services to reset in partial mode.
```

### EC-5: Stale State File (Unclean Shutdown)

**Scenario:** Parent crashed without updating `state.json`. File says `"running"` but the PID is dead.

**Decision:** The PID liveness check detects this. `dev` refuses to start. `status` prints `"unknown (parent not responding)"`. `teardown` proceeds normally — it only touches local containers.

### EC-6: Config Mismatch Between Parent and Child

**Scenario:** The partial workspace's `ai-igniter.toml` defines postgres with different credentials than the parent's.

**Decision:** Inherited services use the parent's `template_vars` from `state.json`, which embed the parent's credentials and URLs. The child's config for inherited services is ignored entirely — no warning needed because the parent's runtime values are authoritative.

### EC-7: Worktree Without Its Own Config

**Scenario:** A worktree has no `ai-igniter.toml`. Config discovery (`Config::find_config`) falls back to the root checkout's config. That config sets `mode = "partial"` and `inherit_from = "root"`.

**Decision:** The self-inheritance guard catches this. The resolved parent path equals the workspace path (both resolve to root), and validation fails with a clear message. Partial mode requires a workspace-local config file (`ai-igniter.toml` in the worktree itself, not inherited from root).

---

## Scope: OUT

- **Multi-level inheritance** (workspace A inherits from B which inherits from C).
- **Automatic service detection** (inferring which services to localize from code changes).
- **ServiceProvider trait changes** (no new required methods).
- **Background parent monitoring** (`--watch-parent` polling).
- **Service-level health proxying** (reporting inherited service health via Docker health checks).
- **Shared Docker network mode** (joining the parent's Docker network for container-to-container communication).
- **`compose_file` support in partial mode** (incompatible by design — cannot filter an external compose file).

---

## Tasks

### T1: Add WorkspaceConfig to config parsing
**File:** `src/config.rs`
**Effort:** S

Add `WorkspaceConfig` struct with `mode`, `inherit_from`, `services_local` fields. Parse from `[workspace]` section. Default to `WorkspaceMode::Full` when absent. Validation: `inherit_from` required when partial, `services_local` names match known services, self-inheritance rejected, `compose_file` incompatible with partial.

### T2: Implement WorkspaceState serialization
**File:** `src/context.rs`
**Effort:** S

Add `WorkspaceState`, `ServiceState`, `ParentContext` structs with serde derives. Add `template_vars` field to state. Implement `write_state(path, state)` with atomic write. Implement `read_state(path) -> Result<WorkspaceState>`. Implement `check_pid_alive(pid) -> bool` with `cfg(unix)` / `cfg(windows)`.

### T3: Write state file during dev lifecycle
**Files:** `src/commands/dev.rs`, `src/supervisor.rs`
**Effort:** S

After `compose.up()` and post-start hooks succeed, build `WorkspaceState` from `ctx.template_vars()` + `ctx.service_images()` and write to `.igniter/state.json`. On clean shutdown in supervisor, update status to `"stopped"`. On teardown, file is deleted with `.igniter/` directory.

### T4: Implement parent resolution and liveness check
**File:** `src/context.rs`
**Effort:** M

Add `resolve_parent_path(inherit_from, workspace_path) -> Result<PathBuf>`. Handle `"root"` (reuse `git_main_checkout()`), absolute paths (validate config exists), relative names (`git worktree list`). Validate `parent_path != workspace_path`. Add `validate_parent_liveness(state: &WorkspaceState) -> Result<()>` — Unix: `libc::kill`, Windows: best-effort with warning.

### T5: Split port allocation for partial mode
**File:** `src/context.rs`
**Effort:** M

When partial, `allocate_ports` computes only for `services_local` (with companion ports). The inherited ports are NOT merged here — that happens in `execute_dev` after reading parent state. Use companion prefix matching to include `{service}_*` ports.

### T6: Filter compose generation for partial mode
**File:** `src/docker/compose.rs`
**Effort:** S

When `WorkspaceMode::Partial`, skip `contribute_compose` and `write_auxiliary_files` calls for services not in `services_local`. When `services_local` is empty, produce an empty compose file. Add `-partial` suffix to compose project name.

### T7: Merge inherited state in execute_dev
**Files:** `src/commands/dev.rs`, `src/context.rs`
**Effort:** M

In `execute_dev`, when partial: read parent state, validate liveness, validate required services exist, merge inherited ports (excluding `base` and local service ports) into `ctx.port_allocations`, merge inherited `template_vars` into ctx. Skip `compose.up()` and `post_start` when `services_local` is empty.

### T8: Filter post-start hooks
**File:** `src/services/mod.rs`
**Effort:** S

`get_active_services` accepts an optional filter. In partial mode, only services in `services_local` are returned. Inherited services do not get post-start hooks.

### T9: Update teardown for partial mode
**File:** `src/commands/teardown.rs`
**Effort:** S

In partial mode, tear down only the local compose project. When `services_local` is empty, skip compose teardown. Print message about inherited services.

### T10: Update status display for partial mode
**File:** `src/commands/status.rs`
**Effort:** S

Two-section output. Read parent state file gracefully (absent/stale → "unknown"). Show inherited ports + liveness status. Handle empty `services_local`.

### T11: Add tests
**Effort:** M

Unit tests:
- `WorkspaceConfig` parsing and validation (all error cases).
- `WorkspaceState` serde roundtrip with `template_vars`.
- Port allocation with `services_local` filter + companion ports.
- Compose generation filtering (empty and non-empty `services_local`).
- Self-inheritance rejection.
- `compose_file` + partial rejection.
- Inherited port merge (base stripped, locals stripped, companions kept).

All callers of `from_parts` (`context.rs:67`) updated:
- `WorkspaceContext::resolve` (line 24)
- `test_ctx` (line 353)
- `rejects_port_overflow` test (line 430)
- Any env_writer test callers

---

## Adversarial Review Log

### Round 1 (2026-09-24) — GPT-Sol + Grok-4.7

**Confirmed findings integrated:**

| # | Finding | Source | Fix |
|---|---------|--------|-----|
| 1 | `services_local = ["frontend"]` invalid — frontend is dev_command, not a Compose service | Both | Rewrote FR-1: empty `services_local` allowed, examples fixed |
| 2 | Parent validation in `resolve()` blocks `status`/`teardown` when parent dead | Both | Moved liveness check to `execute_dev` only (FR-8) |
| 3 | `compose_file` bypasses partial filter | Both | Added validation: `compose_file` incompatible with partial |
| 4 | Companion ports (`garage_web`, `postgres_neon`) dropped by name filter | Both | Added companion port mapping in FR-3, prefix matching rule |
| 5 | Self-inheritance when running from root checkout | Both | Added self-inheritance guard in FR-1 validation |
| 6 | `from_parts` callers missed (test code) | Both | T11 lists all callers explicitly |
| 7 | `post_start` runs for inherited services | Grok | Added FR-9, T8 |
| 8 | Inherited URLs built from child credentials | Grok | Added `template_vars` to state file (FR-2, FR-5) |
| 9 | Windows `fs::rename` doesn't replace | GPT | NFR-3 updated, NFR-5 added |
| 10 | Docker networking: containers can't reach inherited services | Both | Documented in Technical Design caveat (out of scope) |

**Dismissed findings:**

| # | Finding | Source | Reason |
|---|---------|--------|--------|
| 1 | Deterministic hashing has 2000 slots → collisions | GPT | Pre-existing limitation, documented in EC-2 caveat |
| 2 | Two supervisors racing on same workspace | GPT | Pre-existing — not introduced by this feature |
| 3 | `reclaim_stale_ports` may kill parent ports | Grok (SPECULATIVE) | Reclaim operates on `ctx.port_allocations`; in partial mode at reclaim time, only local ports are allocated. Parent ports merge later. |

---

## Open Questions

None. All edge cases resolved above.
