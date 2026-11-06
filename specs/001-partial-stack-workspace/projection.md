# Change Projection — SPEC-001: Partial-Stack Workspace Mode

<!-- projection:begin -->
```json
{
  "spec": "001-partial-stack-workspace",
  "dirs": {
    "src/": "the CLI source tree — all changes land here",
    "src/commands/": "one file per CLI subcommand",
    "src/docker/": "Docker Compose generation and reclaim"
  },
  "files": {
    "src/config.rs": {
      "weight": "added",
      "role": "logic",
      "intent": "Add WorkspaceConfig + WorkspaceMode structs, serde parsing for [workspace] section, 5 new validation rules (inherit_from required, services_local names, self-inheritance, compose_file incompatibility)."
    },
    "src/context.rs": {
      "weight": "heavy",
      "role": "logic",
      "intent": "The heavy one. Add WorkspaceState + ParentContext + ServiceState structs, state file atomic I/O, PID liveness check (Unix + Windows), resolve_parent_path (3 modes), companion port prefix matching, partial port allocation filter, inherited state merge method, parent template_vars storage."
    },
    "src/docker/compose.rs": {
      "weight": "touch",
      "role": "logic",
      "intent": "Guard contribute_compose and write_auxiliary_files loops by services_local membership. Skip when empty. Append -partial to compose project name."
    },
    "src/commands/dev.rs": {
      "weight": "heavy",
      "role": "logic",
      "intent": "The second heavy one. Insert parent state read + liveness validation + inherited service check + port merge between resolve and compose.up. Skip compose.up/post_start when services_local empty. Write state.json after post-start hooks."
    },
    "src/commands/teardown.rs": {
      "weight": "touch",
      "role": "logic",
      "intent": "Skip compose.down when services_local empty. Print inherited services message."
    },
    "src/commands/status.rs": {
      "weight": "added",
      "role": "logic",
      "intent": "Add inherited services section after local section. Read parent state.json gracefully (absent/stale -> unknown). Handle empty services_local display."
    },
    "src/supervisor.rs": {
      "weight": "touch",
      "role": "logic",
      "intent": "Write state.json status update to stopped on clean shutdown. ~5 lines."
    },
    "src/services/mod.rs": {
      "weight": "touch",
      "role": "logic",
      "intent": "Add optional filter parameter to get_active_services. In partial mode, only services_local providers return their Service."
    }
  },
  "bookkeeping": [
    "specs/001-partial-stack-workspace/spec.md",
    "specs/001-partial-stack-workspace/plan.md",
    "specs/001-partial-stack-workspace/tasks.md",
    "specs/001-partial-stack-workspace/projection.md"
  ],
  "not_touched": {
    "src/commands/init.rs": "the init wizard does not learn partial mode in this change — a future follow-up",
    "src/commands/update.rs": "self-updater, no relation",
    "src/commands/env.rs": "env print/write already consumes template_vars unchanged — no structural change needed",
    "src/env_writer.rs": "the env writer reads ctx.template_vars() which already includes inherited vars after the merge in execute_dev — zero lines changed",
    "src/docker/reclaim.rs": "port reclaim runs before inherited ports merge, operates on local ports only — untouched",
    "src/services/postgres.rs": "provider implementation unchanged — no new trait methods",
    "src/services/garage.rs": "provider implementation unchanged — no new trait methods",
    "src/orchestrators.rs": "known orchestrators unaffected",
    "src/cli.rs": "no new CLI flags in this change",
    "src/main.rs": "dispatch table unchanged"
  }
}
```
<!-- projection:end -->

```
src/
├── config.rs                          + Add WorkspaceConfig + WorkspaceMode, serde parsing
│                                      for [workspace], 5 new validation rules.
│
├── context.rs                         ★ The heavy one. WorkspaceState + ParentContext +
│                                      state file I/O, PID liveness, resolve_parent_path,
│                                      companion port matching, partial allocation filter,
│                                      inherited state merge.
│
├── docker/
│   └── compose.rs                     ~ Guard compose loops by services_local. Skip when
│                                      empty. -partial project suffix.
│
├── commands/
│   ├── dev.rs                         ★ The second heavy one. Parent state read + liveness +
│   │                                  inherited merge between resolve and compose.up. Skip
│   │                                  compose when no local services. Write state.json.
│   │
│   ├── teardown.rs                    ~ Skip compose.down when no local services. Print
│   │                                  inherited services message.
│   │
│   └── status.rs                      + Inherited services section. Graceful parent state
│                                      read. Empty services_local display.
│
├── supervisor.rs                      ~ State file status update on clean shutdown. ~5 lines.
│
└── services/
    └── mod.rs                         ~ Optional filter on get_active_services. ~10 lines.

bookkeeping — not part of the change:
  specs/001-partial-stack-workspace/{spec,plan,tasks,projection}.md

deliberately NOT touched:
  src/commands/init.rs — init wizard does not learn partial mode in this change
  src/commands/env.rs — env print/write consumes template_vars unchanged
  src/env_writer.rs — reads ctx.template_vars() which includes inherited vars after merge
  src/docker/reclaim.rs — runs before inherited ports merge, operates on local ports only
  src/services/postgres.rs — provider implementation unchanged
  src/services/garage.rs — provider implementation unchanged
  src/orchestrators.rs — known orchestrators unaffected
  src/cli.rs — no new CLI flags
  src/main.rs — dispatch table unchanged
```
