# Skill Rhai Unified Script Runtime Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the remaining special-case `SKILL.rhai` runtime assumptions with one `SKILL.md`-driven script asset model, while preserving compatibility for top-level `SKILL.rhai`, `scripts/*.rhai`, top-level `SKILL.py`, `scripts/*.py`, and `bin/*`.

**Architecture:** Keep `run_skill_for_turn()` as the single runtime entry, add a unified skill-scoped script tool in `blockcell-tools`, and let `SKILL.md` select script assets by relative path. The tool dispatches `.rhai` assets through `SkillDispatcher` and external scripts or executables through the current `exec_local` mechanics. Metadata and testing move from `supports_local_exec` semantics toward broader script-execution semantics.

**Tech Stack:** Rust, tokio, serde_json, rhai, cargo test

---

### Task 1: Add the Unified Skill Script Tool

**Files:**
- Create: `crates/tools/src/exec_skill_script.rs`
- Modify: `crates/tools/src/lib.rs`
- Modify: `crates/tools/src/registry.rs`
- Modify: `crates/tools/Cargo.toml`
- Test: `crates/tools/src/exec_skill_script.rs`

**Step 1: Write the failing tests**

Add unit tests for:

```rust
#[tokio::test]
async fn test_exec_skill_script_runs_top_level_rhai() {}

#[tokio::test]
async fn test_exec_skill_script_runs_nested_rhai() {}

#[tokio::test]
async fn test_exec_skill_script_runs_process_script() {}

#[tokio::test]
async fn test_exec_skill_script_runs_cli_binary() {}

#[tokio::test]
async fn test_exec_skill_script_rejects_parent_escape() {}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p blockcell-tools exec_skill_script -- --nocapture`
Expected: FAIL because the new tool does not exist yet.

**Step 3: Write minimal implementation**

Implement `ExecSkillScriptTool` with:
- skill-scope-only execution
- validated relative `path`
- backend dispatch by path or extension
- normalized response shape

Target backend split:

```rust
match runtime_kind {
    ScriptRuntime::Rhai => run_rhai_asset(...),
    ScriptRuntime::Process => run_process_asset(...),
}
```

Add `blockcell-skills` as a `crates/tools` dependency so the tool can call `SkillDispatcher`.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p blockcell-tools exec_skill_script -- --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/tools/Cargo.toml crates/tools/src/lib.rs crates/tools/src/registry.rs crates/tools/src/exec_skill_script.rs
git commit -m "feat: add unified skill script tool"
```

### Task 2: Wire Runtime and Skill Cards to Script Capability

**Files:**
- Modify: `crates/skills/src/manager.rs`
- Modify: `crates/agent/src/runtime.rs`
- Test: `crates/skills/src/manager.rs`
- Test: `crates/agent/src/runtime.rs`

**Step 1: Write the failing tests**

Add or update tests for:

```rust
#[test]
fn test_skill_card_detects_supports_script_exec_from_rhai_asset() {}

#[test]
fn test_skill_card_detects_supports_script_exec_from_cli_asset() {}

#[tokio::test]
async fn test_prompt_skill_gets_exec_skill_script_when_script_assets_exist() {}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p blockcell-skills manager -- --nocapture`
Run: `cargo test -p blockcell-agent runtime -- --nocapture`
Expected: FAIL because runtime and skill cards still only know `supports_local_exec` and `exec_local`.

**Step 3: Write minimal implementation**

Rename and reinterpret capability fields:
- `supports_local_exec` -> `supports_script_exec`
- `skill_supports_local_exec()` -> `skill_supports_script_exec()`

Update `resolved_skill_tool_names()` so the canonical injected tool becomes `exec_skill_script`.

During this task, keep `exec_local` registered globally for compatibility, but stop treating it as the recommended skill-script interface.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p blockcell-skills manager -- --nocapture`
Run: `cargo test -p blockcell-agent runtime -- --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/skills/src/manager.rs crates/agent/src/runtime.rs
git commit -m "refactor: expose script execution as a unified skill capability"
```

### Task 3: Migrate Runtime Skill Fixtures to `SKILL.md`-Driven Script Calls

**Files:**
- Modify: `crates/agent/src/runtime.rs`
- Test: `crates/agent/src/runtime.rs`

**Step 1: Write the failing tests**

Update runtime fixture tests to cover:

```rust
#[tokio::test]
async fn test_prompt_skill_can_call_exec_skill_script_for_shell_asset() {}

#[tokio::test]
async fn test_prompt_skill_can_call_exec_skill_script_for_top_level_rhai_asset() {}

#[tokio::test]
async fn test_cli_style_skill_runs_via_exec_skill_script() {}
```

Each fixture should place the asset where `SKILL.md` says it lives, including top-level and nested paths.

**Step 2: Run tests to verify they fail**

Run: `cargo test -p blockcell-agent runtime::test_prompt_skill -- --nocapture`
Expected: FAIL because the fixtures still instruct the model to use `exec_local`.

**Step 3: Write minimal implementation**

Rewrite test fixture `SKILL.md` content and any related helper assertions so:
- prompt skills instruct `exec_skill_script`
- top-level `SKILL.rhai` is exercised as a script asset
- CLI examples use `bin/*` through the unified tool

Keep one compatibility test proving that an older prompt mentioning `exec_local` still works if the tool is explicitly available.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p blockcell-agent runtime -- --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/agent/src/runtime.rs
git commit -m "test: move runtime skill fixtures to unified script tool"
```

### Task 4: Preserve Rhai Compatibility While Deprecating Direct Runtime Use

**Files:**
- Modify: `crates/agent/src/runtime.rs`
- Modify: `crates/skills/src/service.rs`
- Modify: `crates/tools/src/skills.rs`
- Test: `crates/skills/src/service.rs`

**Step 1: Write the failing tests**

Add tests for:

```rust
#[test]
fn test_detect_skill_layout_keeps_top_level_rhai_compatibility() {}

#[test]
fn test_list_skills_metadata_reports_rhai_python_md_presence_consistently() {}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p blockcell-skills service -- --nocapture`
Run: `cargo test -p blockcell-tools skills -- --nocapture`
Expected: FAIL if metadata still assumes top-level Rhai implies a separate runtime mode.

**Step 3: Write minimal implementation**

Keep top-level `SKILL.rhai` compatibility in layout detection and skill metadata, but remove or de-emphasize code comments and helper naming that imply it is a separate mainline executor.

For `run_rhai_script_with_context()`:
- mark it explicitly as deprecated compatibility code
- ensure no new mainline path is added to it

**Step 4: Run tests to verify they pass**

Run: `cargo test -p blockcell-skills service -- --nocapture`
Run: `cargo test -p blockcell-tools skills -- --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/agent/src/runtime.rs crates/skills/src/service.rs crates/tools/src/skills.rs
git commit -m "refactor: preserve rhai compatibility as script asset metadata"
```

### Task 5: Narrow Evolution and Contract Semantics Without Rewriting Them

**Files:**
- Modify: `crates/skills/src/evolution.rs`
- Modify: `bin/blockcell/src/commands/gateway/skills_install.rs`
- Test: `crates/skills/src/evolution.rs`

**Step 1: Write the failing tests**

Add tests that express the new interpretation:

```rust
#[test]
fn test_contract_check_treats_skill_type_as_primary_asset_bookkeeping() {}

#[test]
fn test_compile_check_keeps_top_level_rhai_as_primary_asset_only() {}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p blockcell-skills evolution -- --nocapture`
Expected: FAIL because comments, prompts, or checks still frame Rhai/Python as alternate runtime entry modes.

**Step 3: Write minimal implementation**

Update wording, helper comments, and generation templates so:
- `SkillType::Rhai` / `SkillType::Python` remain valid bookkeeping
- compile and contract checks still target primary assets
- generated prompts stop implying that file type decides runtime entry behavior

Keep this task intentionally narrow. Do not redesign the full evolution system here.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p blockcell-skills evolution -- --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/skills/src/evolution.rs bin/blockcell/src/commands/gateway/skills_install.rs
git commit -m "refactor: narrow skill type semantics to asset bookkeeping"
```

### Task 6: Unify CLI Skill Testing and Documentation

**Files:**
- Modify: `bin/blockcell/src/commands/skills.rs`
- Modify: `docs/04_skill_system.md`
- Modify: `docs/en/04_skill_system.md`
- Test: `bin/blockcell/src/commands/skills.rs`

**Step 1: Write the failing tests**

Add or update tests for:

```rust
#[test]
fn test_skills_test_describes_rhai_as_skill_md_driven_asset() {}
```

If the CLI command is not covered by tests today, add a narrow parser or output-format unit test instead of a full end-to-end harness.

**Step 2: Run tests to verify they fail**

Run: `cargo test -p blockcell skills -- --nocapture`
Expected: FAIL because CLI test/help text still describes Rhai through its old standalone model.

**Step 3: Write minimal implementation**

Update:
- `blockcell skills test` wording and assumptions
- skill system docs in both languages
- examples to prefer `exec_skill_script`

Document clearly that:
- `SKILL.md` decides when assets run
- path comes from the skill prompt, not from a hardcoded folder rule
- top-level compatibility remains
- CLI assets are first-class

**Step 4: Run tests to verify they pass**

Run: `cargo test -p blockcell skills -- --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add bin/blockcell/src/commands/skills.rs docs/04_skill_system.md docs/en/04_skill_system.md
git commit -m "docs: describe unified skill script runtime"
```

### Task 7: Final Verification

**Files:**
- Modify: none unless verification exposes a real bug

**Step 1: Run focused crate tests**

Run:

```bash
cargo test -p blockcell-tools exec_skill_script -- --nocapture
cargo test -p blockcell-skills manager service evolution -- --nocapture
cargo test -p blockcell-agent runtime -- --nocapture
cargo test -p blockcell skills -- --nocapture
```

Expected: PASS

**Step 2: Run targeted compile check**

Run:

```bash
cargo check -p blockcell-tools -p blockcell-skills -p blockcell-agent -p blockcell
```

Expected: PASS

**Step 3: Review for prompt and naming drift**

Verify that:
- new code says `supports_script_exec`, not `supports_local_exec`
- new prompt fixtures prefer `exec_skill_script`
- no new runtime path bypasses `run_skill_for_turn()`

**Step 4: Commit verification fixes if needed**

```bash
git add <changed-files>
git commit -m "test: verify unified skill script runtime"
```
