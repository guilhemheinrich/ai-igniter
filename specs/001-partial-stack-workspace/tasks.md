# Tasks — SPEC-001: Partial-Stack Workspace Mode

## Phase 1 — Foundation (config + state file)

- [ ] T1: Add WorkspaceConfig to config parsing (`src/config.rs`) — S
- [ ] T2: Implement WorkspaceState serialization + PID check (`src/context.rs`) — S
- [ ] T3: Write state file during dev lifecycle (`src/commands/dev.rs`, `src/supervisor.rs`) — S

## Phase 2 — Partial resolution (parent + ports)

- [ ] T4: Implement parent resolution and liveness check (`src/context.rs`) — M
- [ ] T5: Split port allocation for partial mode with companion ports (`src/context.rs`) — M

## Phase 3 — Compose + env + hooks

- [ ] T6: Filter compose generation for partial mode (`src/docker/compose.rs`) — S
- [ ] T7: Merge inherited state in execute_dev (`src/commands/dev.rs`, `src/context.rs`) — M
- [ ] T8: Filter post-start hooks by services_local (`src/services/mod.rs`) — S

## Phase 4 — Commands

- [ ] T9: Update teardown for partial mode (`src/commands/teardown.rs`) — S
- [ ] T10: Update status display for partial mode (`src/commands/status.rs`) — S

## Phase 5 — Tests

- [ ] T11: Add unit and integration tests — M

## Dependency Graph

```
T1 (config parsing)
 └─> T2 (state serialization)
      └─> T3 (state lifecycle in dev/supervisor)
           └─> T4 (parent resolution)
                └─> T5 (port allocation split)
                     ├─> T6 (compose filtering)
                     ├─> T7 (inherited state merge) ─> T8 (post-start filtering)
                     ├─> T9 (teardown)
                     └─> T10 (status)
                          └─> T11 (tests)
```

Phase 3 tasks (T6, T7, T8) are parallelizable after T5.
Phase 4 tasks (T9, T10) are parallelizable after T5.
T11 runs last.

## Key Implementation Notes

**No signature change to `from_parts`.** The adversarial review flagged that adding
a parameter would break test callers at `context.rs:430` and env_writer tests.
Instead, `allocate_ports` reads `config.workspace` internally. All existing callers
compile unchanged.

**Parent state read is in `execute_dev`, not `resolve`.** This ensures `status` and
`teardown` work when the parent is dead. The `WorkspaceContext` starts with local
ports only; `execute_dev` enriches it before compose/env generation.

**Empty `services_local` = no Docker.** When the list is empty, `compose.up()`,
`post_start`, `reclaim_stale_ports`, and `compose.stop()` are all skipped. Only
env write and the dev command supervisor run.
