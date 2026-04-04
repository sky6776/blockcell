# Skill Rhai Unified Script Runtime Design

**Date:** 2026-04-04

**Goal:** Unify `SKILL.rhai` with the existing `SKILL.md`-driven skill runtime so Rhai, Python, shell, and CLI assets are all invoked through one skill execution model instead of separate entry paths.

## Scope

Included:
- Keep skill activation and cron routing on the current unified runtime entry
- Make `SKILL.md` the only place that decides when a skill script asset should run
- Preserve compatibility for top-level `SKILL.rhai`, top-level `SKILL.py`, `scripts/*.rhai`, `scripts/*.py`, and `bin/*`
- Treat CLI-style executables as first-class skill assets rather than incidental local scripts
- Add one unified skill-scoped script tool that dispatches to the correct backend

Excluded:
- Rewriting the full skill evolution system in the same change
- Removing `SkillType::Rhai` / `SkillType::Python` from storage and audit in the first pass
- Forcing a required directory layout such as only `scripts/` or only `bin/`
- Restoring `SKILL.rhai` as a direct runtime entry mode

## Current Reality

The active chat and cron path is already unified around named skill activation:
- `forced_skill_name` and normal skill activation flow into `run_skill_for_turn()` in `crates/agent/src/runtime.rs`
- Skill tools are resolved from the active skill card and prompt bundle
- Prompt-driven skills can already invoke local assets through `exec_local`

What is not unified is the mental model:
- `SKILL.rhai` still has a retained standalone executor in `crates/agent/src/runtime.rs`, but it is not the mainline path
- Rhai is still treated as a separate skill flavor in several places even when runtime behavior is already converging
- `supports_local_exec` in `crates/skills/src/manager.rs` conflates "skill can run external processes" with "skill has script-like assets", and it currently counts `SKILL.rhai`
- Tooling, tests, and evolution logic still assume that a top-level `SKILL.rhai` is special in ways that prompt-driven script skills are not

This creates an asymmetry:
- `SKILL.py`, `scripts/*.py`, and `bin/*` are effectively script assets described by `SKILL.md`
- `SKILL.rhai` still carries legacy identity as both an asset and a skill type

## Options Considered

### Option 1: Keep Rhai as a Separate Runtime Mode

Keep `SKILL.rhai` as an alternate execution entry while improving docs and compatibility.

Pros:
- Lowest immediate code churn
- Leaves current evolution and audit assumptions mostly intact

Cons:
- Preserves the split mental model
- Keeps cron/testing/runtime behaviors inconsistent
- Makes CLI-oriented skill assets second-class compared with top-level Rhai

### Option 2: Unify All Script Assets Behind `SKILL.md` and a Dedicated Script Tool

Keep one skill runtime entry and let `SKILL.md` decide when to invoke a script asset. Add a unified tool that can execute either Rhai or external scripts depending on the referenced path.

Pros:
- One stable runtime model for prompt, Rhai, Python, shell, and CLI assets
- Keeps path choice flexible and skill-local
- Matches the user requirement that CLI support remains central
- Allows gradual migration without breaking top-level compatibility

Cons:
- Requires a new tool abstraction and some metadata renaming
- Leaves temporary dual semantics in evolution and audit until a later cleanup

### Option 3: Force Rhai Into `exec_local`

Treat `.rhai` like any other file and push it through the existing `exec_local` tool.

Pros:
- Smaller surface area in the tool registry

Cons:
- Incorrect abstraction: Rhai is in-process interpreter execution, not external process execution
- Loses Rhai-specific tracing and error semantics
- Pushes incompatible runtime models into one tool contract

### Recommendation

Choose Option 2.

It gives one user-facing model without pretending that Rhai and external process execution are the same implementation. The runtime stays unified, `SKILL.md` becomes authoritative, and CLI assets remain first-class without locking the protocol to a specific directory layout.

## Proposed Architecture

### Runtime Entry

Do not add a new runtime entry path.

The only supported activation model remains:
- resolve active skill by name
- load prompt/manual content from `SKILL.md`
- let the model decide whether to call tools during skill execution

This keeps `run_skill_for_turn()` in `crates/agent/src/runtime.rs` as the single execution entry for both normal skill activation and cron-routed skill execution.

### Unified Script Tool

Introduce a new tool in `blockcell-tools`, tentatively named `exec_skill_script`.

Contract:
- input path is a relative path inside the active skill directory
- no absolute paths
- no `..` traversal
- `SKILL.md` provides the exact path to call
- no required folder convention is enforced by runtime

Dispatch rules:
- `.rhai` -> execute via `blockcell_skills::dispatcher::SkillDispatcher::execute_sync()`
- `.py`, `.sh`, `.js`, `.php`, or executable files -> execute as local processes using the existing `exec_local` mechanics

The tool should return one normalized result shape:
- `runtime`: `rhai` or `process`
- `path`
- `resolved_path`
- `success`
- `stdout` / `stderr` / `exit_code` for process execution
- `output` / `error` / `tool_calls` for Rhai execution

The backend difference remains real, but it is hidden behind one skill-facing tool contract.

### Skill-Scope Semantics

Script execution is only available inside an active skill scope.

That means:
- no global free-form execution by asset path
- all script assets are resolved relative to the active skill directory
- a skill may reference top-level files or nested files, but only because `SKILL.md` instructs the model to do so

Examples that remain valid:
- `SKILL.rhai`
- `SKILL.py`
- `scripts/analyze.rhai`
- `scripts/report.py`
- `bin/cli.sh`
- `tools/custom-wrapper`

What changes is not file availability, but the control plane: `SKILL.md` decides when any of them are invoked.

### Capability Metadata

Rename the runtime-facing capability from `supports_local_exec` to `supports_script_exec`.

New meaning:
- the skill exposes local script or CLI assets that may be invoked from `SKILL.md`

This avoids equating Rhai with external local process execution.

Detection should remain permissive in the first pass:
- existing script assets can imply capability
- `SKILL.md` keywords can still help infer capability for legacy skills

But the prompt/runtime surface should stop advertising `exec_local` as the canonical script entry and instead advertise the unified script tool.

## Compatibility Rules

Compatibility must be preserved for existing layouts, but compatibility is about asset discovery, not runtime entry modes.

Supported assets after the refactor:
- top-level `SKILL.rhai`
- `scripts/*.rhai`
- top-level `SKILL.py`
- `scripts/*.py`
- `bin/*`
- any other skill-local relative path that passes validation and is referenced by `SKILL.md`

Compatibility rules:
- top-level `SKILL.rhai` remains valid as a script asset
- top-level `SKILL.py` remains valid as a script asset
- existing skills using `exec_local` should continue to work during migration
- no runtime rule hardcodes `scripts/` or `bin/` as the only legal location

## Evolution, Audit, and Testing Implications

`SkillType::Rhai`, `SkillType::Python`, and `SkillType::PromptOnly` can remain for now, but their meaning should narrow to:
- primary asset classification
- compile strategy
- audit strategy
- snapshot/version bookkeeping

They should no longer imply different runtime entry modes.

Short-term implications:
- compile check still compiles top-level `SKILL.rhai` and `SKILL.py`
- contract check still validates the primary asset required by the current type
- layout detection can still prefer top-level files for bookkeeping

Longer-term direction:
- move from "skill type decides runtime path" to "skill assets + `SKILL.md` decide runtime behavior"
- allow testing tooling to validate Rhai and Python assets through the same skill-runtime model where feasible

## Migration Plan

### Phase 1: Add `exec_skill_script`

Create the new tool without removing `exec_local`.

This phase should:
- centralize path validation for skill-relative assets
- dispatch `.rhai` to `SkillDispatcher`
- dispatch process-backed assets to existing `exec_local` behavior
- add direct unit tests for Rhai, Python/shell, CLI, and path rejection

### Phase 2: Switch Runtime Capability Injection

Update runtime and skill cards to inject the unified script tool instead of only `exec_local`.

This phase should:
- rename `supports_local_exec` to `supports_script_exec`
- update skill card generation and tests
- keep `exec_local` registered for compatibility, but stop treating it as the canonical skill script interface

### Phase 3: Move Prompt Examples and Runtime Tests to the Unified Model

Update runtime and skill fixtures so prompt-driven skills call `exec_skill_script`.

This phase should cover:
- top-level `SKILL.rhai`
- `scripts/*.rhai`
- top-level `SKILL.py`
- `scripts/*.py`
- `bin/*`

### Phase 4: Deprecate Direct Rhai Helper

Mark the retained direct Rhai helper in `crates/agent/src/runtime.rs` as deprecated compatibility code and remove remaining mainline assumptions that Rhai is a separate executor path.

### Phase 5: Clean Up Tooling and Self-Evolution Assumptions

Update `blockcell skills test`, gateway install/generation paths, evolution prompts, and related metadata so they describe script assets instead of alternate runtime entry modes.

This phase can remain incremental after the core runtime/tool refactor lands.

## Error Handling

The unified tool should expose stable errors:
- invalid relative path
- path escapes active skill directory
- file not found
- unsupported runner or unsupported asset type
- Rhai compile error
- Rhai runtime error
- process execution failure

Errors should always include:
- requested `path`
- selected `runtime` if known
- backend-specific details such as `exit_code` or compile/runtime diagnostics

## Testing Strategy

Add or update tests in three layers.

1. Tool unit tests
- `exec_skill_script` runs top-level `SKILL.rhai`
- `exec_skill_script` runs `scripts/*.rhai`
- `exec_skill_script` runs top-level `SKILL.py`
- `exec_skill_script` runs `scripts/*.py`
- `exec_skill_script` runs `bin/*`
- parent-path escape is rejected

2. Runtime tests
- prompt skill with `SKILL.md` can call the unified script tool
- legacy top-level `SKILL.rhai` works when `SKILL.md` points to it
- CLI-style skills run through the unified tool instead of a file-type special case

3. Metadata and CLI tests
- skill cards expose `supports_script_exec`
- `list_skills` and gateway skill installation metadata remain coherent
- `blockcell skills test` no longer assumes that Rhai is validated through a separate user-facing model

## Risks and Non-Goals

Main risks:
- accidental coupling between the new tool and old `exec_local` assumptions
- overreaching into evolution/versioning before the runtime model is stable
- breaking legacy skills whose `SKILL.md` still explicitly says `exec_local`

Risk handling:
- add `exec_skill_script` first and keep `exec_local` available during migration
- preserve top-level asset compatibility
- update tests before removing or renaming prompt conventions

Non-goals for the first implementation:
- eliminating top-level `SKILL.rhai`
- removing `SkillType::Rhai`
- forbidding top-level `SKILL.py`
- forcing all scripts into `scripts/`

## Expected Outcome

After this refactor, the runtime model becomes:
- skill activation selects the skill
- `SKILL.md` instructs the model when to invoke a script asset
- the script asset path is skill-local and flexible
- backend dispatch is implementation detail

That makes Rhai, Python, shell, and CLI assets consistent without erasing their backend differences.
