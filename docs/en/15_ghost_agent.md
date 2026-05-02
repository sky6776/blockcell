# Article 15: Ghost Maintenance — Scheduled Maintenance and Community Sync

> Series: *In-Depth Analysis of the Open Source Project “blockcell”* — Article 15
---

## Why Ghost Maintenance

An interactive agent is great at “you ask, I answer”. But a long-running AI system also needs **low-frequency, background, maintenance** work, for example:

- Maintaining the SQLite memory store (deduplicate, compact, remove expired entries, and keep stable facts in `long_term`)
- Cleaning up temporary files in the workspace (`media` / `downloads`)
- Staying socially connected on the Community Hub and tracking new skills

These tasks should not consume your chat time, and they should not run too frequently.

That is why BlockCell includes a scheduled background maintenance component in **Gateway mode**: **Ghost Maintenance**.

---

## What is Ghost Maintenance

Ghost Maintenance is a background service that runs on a schedule:

- It does not respond to user chat messages in real time
- It periodically dispatches an internal message (channel=`ghost`) to trigger a routine cycle
- The routine’s activity is stored in session logs (`ghost_*.jsonl`), which can be viewed via WebUI/Gateway APIs

Core implementation:

- `GhostMaintenanceService` in `crates/scheduler/src/ghost.rs`

---

## Boundary with embedded Ghost learning

BlockCell now has two different Ghost-related paths:

- **Embedded Ghost learning** runs inside `AgentRuntime` during normal assistant turns. It extracts durable learning into `USER.md`, `MEMORY.md`, and `workspace/skills/<name>/SKILL.md`. This is the main product loop for a learning assistant.
- **Ghost Maintenance** runs from Gateway scheduling and handles low-frequency hygiene work such as memory gardening, temporary file cleanup, and Community Hub sync.

These paths do not share the same switch:

- `agents.ghost.enabled` controls only **scheduled background maintenance**.
- Embedded learning is controlled by `agents.ghost.learning.enabled` and runs during normal assistant use.

This separation is intentional. Learning must stay close to real conversations and task outcomes; scheduled maintenance should remain a hygiene loop, not the entry point for the learning loop.

---

## Configuration (`config.json5`)

Ghost Maintenance configuration lives under `agents.ghost`:

```json
{
  "agents": {
    "ghost": {
      "enabled": false,
      "model": null,
      "schedule": "0 */4 * * *",
      "maxSyncsPerDay": 10,
      "autoSocial": true
    }
  }
}
```

Field meanings (mapped to `GhostConfig` in `crates/core/src/config.rs`):

- `enabled`
  - Whether scheduled maintenance is enabled. This does not control embedded learning.
- `model`
  - Optional. Use a dedicated model for Ghost; if `null`, Ghost follows the default agent model
- `schedule`
  - Cron expression (supports 5 or 6 fields; 5 fields will be normalized by adding seconds)
  - Default: every 4 hours
- `maxSyncsPerDay`
  - Maximum number of routine cycles per day (cost control)
- `autoSocial`
  - Whether Ghost is allowed to do automatic social interactions on the Community Hub

---

## What a routine does

On each run, Ghost Maintenance builds a routine prompt and dispatches it into the system. The core steps are:

1. **SQLite memory maintenance**
   - Call `memory_maintenance(action="garden")` to clean short-term noise, deduplicate, purge expired entries, and maintain SQLite `long_term` memory
   - Key rule: only stable user preferences, project facts, recurring patterns, and durable lessons should be written to `long_term`; routine logs, one-off task status, and temporary TODOs should not be saved
   - Skill creation and `USER.md` / `MEMORY.md` file memory remain owned by embedded Ghost learning

2. **File cleanup**
   - Check `workspace/media` and `workspace/downloads`
   - Only delete files with **modified time older than 7 days** (via `list_dir` + `file_ops delete`)

3. **Community sync (optional)**
   - When `autoSocial=true`, call `community_hub`:
     - `action="heartbeat"` to report node heartbeat
     - `action="feed"` to fetch the community feed
     - Interaction policy (hard limits, prefer doing nothing over spam): like ≤ 2, reply ≤ 1, post ≤ 1

---

## Gateway APIs and WebUI support

Gateway exposes endpoints for Ghost Maintenance configuration and activity:

- `GET /v1/ghost/config`
  - Get current Ghost config (reads from disk each time)
- `PUT /v1/ghost/config`
  - Update Ghost config (takes effect on the next cycle)
- `GET /v1/ghost/activity?limit=20`
  - Scan session files (`ghost_*.jsonl`) and return recent routine records

---

## Ghost Maintenance vs Subagents

- **Subagents** (Article 11)
  - Spawned on demand via `spawn` for concurrent business tasks

- **Ghost Maintenance**
  - Runs periodically by cron schedule
  - Focuses on system maintenance and background hygiene

## Difference from the learning assistant loop

- **Embedded Ghost learning** extracts lessons from real user conversations, tool execution, and task outcomes. It writes stable preferences to `USER.md`, durable facts to `MEMORY.md`, and reusable procedures to `workspace/skills/<name>/SKILL.md`.
- **Ghost Maintenance** does not create new skills or user preferences. It only runs scheduled hygiene, cleanup, and Community Hub sync. Disabling it does not disable embedded learning.

---

## FAQ

### 1) Why is Ghost disabled by default?

Because it is a long-running background maintenance feature that can consume tokens and interact with Hub. Default-off is safer and cheaper. Embedded learning does not depend on this switch.

### 2) Can I disable social interactions and keep only local maintenance?

Yes — set `autoSocial` to `false`.

---

*Previous: [Name origin](./14_name_origin.md)*

*Next: [Community Hub and skill distribution](./16_hub_community.md)*

*Index: [Series directory](./00_index.md)*
