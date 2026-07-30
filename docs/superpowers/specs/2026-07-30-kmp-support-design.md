# Kotlin Multiplatform (KMP) Support Design Spec

## Goal

tokensave already extracts Kotlin (`src/extraction/kotlin_extractor.rs`) and Swift (`src/extraction/swift_extractor.rs`) as independent languages, but has zero awareness of Kotlin Multiplatform structure: `expect`/`actual` declarations are indistinguishable from ordinary declarations, there is no concept of source-set/target (`commonMain`, `androidMain`, `iosMain`, `jvmMain`, ...), and Gradle KMP's nested `sourceSets { }` dependency DSL isn't parsed. For teams whose apps share Kotlin business logic across SwiftUI and Jetpack Compose UIs with heavy `expect`/`actual` usage, this means the code graph and AI context output treat platform-specific implementations of the same logical declaration as unrelated code.

This spec covers three phases. Phases 0–2 are in scope for this design; Phase 3 is deferred (see [Out of Scope](#out-of-scope)).

- **Phase 0:** Detect source-set/target from file path, store on `Node`.
- **Phase 1:** Detect `expect`/`actual` declarations, link them with a new edge kind.
- **Phase 2:** Make AI-generated context automatically surface all platform variants of a symbol together.

## Phase 0 — Source-Set / Target Detection

### Detection logic

New shared utility, `src/extraction/kmp.rs`:

```rust
pub enum KmpTarget {
    Common,
    Platform(String), // "android", "ios", "jvm", "js", "native", "wasmJs", ...
}

pub struct KmpLocation {
    pub source_set: String,      // e.g. "commonMain", "androidMain", "iosTest"
    pub target: KmpTarget,
    pub module_root: String,     // dir containing `src/`, e.g. "shared"
}

pub fn kmp_location_from_path(file_path: &str) -> Option<KmpLocation>;
```

`kmp_location_from_path` scans path segments for one matching `^[a-z][a-zA-Z0-9]*(Main|Test)$` immediately inside a `src/` segment (i.e. `.../src/{segment}/...`). The prefix before `Main`/`Test` becomes the target: `"common"` → `KmpTarget::Common`, anything else → `KmpTarget::Platform(prefix)`. `module_root` is the path up to (excluding) `src/`. Returns `None` for non-KMP-shaped paths (e.g. plain `src/main/kotlin/...` single-platform Android/JVM layout) — those nodes get `source_set = NULL`.

This is purely path-convention based — no Gradle parsing required (that's Phase 3).

### Schema

New migration `V15` (`db/migrations.rs`, current `LATEST_VERSION = 14` → bump to `15`). All three KMP columns land in this single migration (Phase 0's `source_set` + `kmp_module_root`, plus Phase 1's `kmp_role`), since the same extractor code path populates them together:

```sql
ALTER TABLE nodes ADD COLUMN source_set TEXT NULL;
ALTER TABLE nodes ADD COLUMN kmp_module_root TEXT NULL;
ALTER TABLE nodes ADD COLUMN kmp_role TEXT NULL; -- 'expect' | 'actual' | NULL (Phase 1)
```

Populated by extractors at insert time. Any extractor can call `kmp_location_from_path(file_path)` when building a `Node` — initially wired into `KotlinExtractor` only (Swift/other KMP-relevant files can adopt it later without a schema change).

**Schema blast radius (do not underestimate).** The DB layer is **positional, column-index based** — this is not a `visibility`/`is_async`-scale change. Appending columns lands them at indices 28–30, and every one of these sites must be updated in lockstep:

- **`row_to_node` (`db/queries/mod.rs:76`)** — add tolerant reads for the three new columns (`row.get(28).ok()...` pattern, mirroring the issue-#150 health columns at indices 23–27 which read via `unwrap_or`), so older SELECT lists that omit them don't error.
- **All three `INSERT OR REPLACE INTO nodes` statements** (`db/queries/nodes.rs:13`, `:118`, `:274`) — each lists columns and positional bind params; all three must add the three columns + bindings.
- **SELECT lists across `db/queries/`** — tolerant reads mean an omitting SELECT silently yields `NULL`. That's the trap: **the SELECT that loads nodes for the resolver (Phase 1) MUST include the three new columns**, or `row_to_node` reads `None` and KMP matching fails silently with no error. Audit every SELECT feeding `row_to_node` and confirm the resolver-loading one is updated.
- **`Node` struct (`types.rs:335`)** — add `source_set: Option<String>`, `kmp_module_root: Option<String>`, `kmp_role: Option<String>` fields.

## Phase 1 — expect/actual Linking

### Pre-work: grammar verification (spike)

Before writing extraction logic, confirm `tree-sitter-kotlin-sg` v0.4.1 actually parses `expect fun foo()` / `actual fun foo()` with `expect`/`actual` appearing as tokens inside the declaration's `modifiers` node (structurally like `data`/`sealed` today). Write a throwaway fixture and inspect the parse tree. If the grammar mis-parses or doesn't expose these tokens, this phase is blocked on a grammar fix/upgrade first — resolve that before continuing.

### Schema

The `kmp_role` column ships in the same `V15` migration described in Phase 0 (all three columns together) — see the Phase 0 schema section for the full column list and the blast-radius checklist that applies here too.

### Extractor changes (`kotlin_extractor.rs`)

Reuse the existing `has_modifier_keyword(node, state, keyword)` helper, calling it with `"expect"` and `"actual"` at the same call sites already used for `"data"`/`"sealed"` (function, class, object, property, interface declarations). Set `Node.kmp_role` accordingly.

For every node where `kmp_role == Some("actual")`, additionally emit an `UnresolvedRef`. Note the actual field names of the struct (`types.rs:409`) — `from_node_id` / `reference_name` / `reference_kind`, plus the required `column` and `file_path`:

```rust
UnresolvedRef {
    from_node_id: node.id.clone(),
    reference_name: node.qualified_name.clone(),
    reference_kind: EdgeKind::ActualFor,
    line: node.start_line,
    column: node.start_column,
    file_path: node.file_path.clone(),
}
```

This reuses the existing generic `unresolved_refs → resolve_all → create_edges` pipeline unchanged — no new collection/dispatch machinery.

### `types.rs`

New variant: `EdgeKind::ActualFor` (source = `actual` node, target = `expect` node). Direction chosen so an `expect` naturally has fan-in from N `actual`s (one per platform), rather than modeling fan-out from a single node.

Adding the variant is not just the enum — both `EdgeKind` match arms are exhaustive and must gain the new case, or edges won't round-trip through the DB (stored as `TEXT`):

- `EdgeKind::as_str` (`types.rs:266`) → `EdgeKind::ActualFor => "actual_for"`
- `EdgeKind::from_str` (`types.rs:283`) → `"actual_for" => Some(EdgeKind::ActualFor)`

### Resolver changes (`resolver.rs`)

New dedicated matching strategy, following the existing `try_go_selector_match` pattern (a targeted branch alongside the generic scorer, not a modification to it):

```rust
fn try_kmp_actual_match(&self, uref: &UnresolvedRef) -> Option<ResolvedRef>
```

**Dispatch placement is critical.** `resolve_one` (`resolver.rs:399`) dispatches by the *shape* of `reference_name` (contains `::` → qualified match; contains `.` → dotted-receiver; else exact name), **not** by `reference_kind`. Kotlin qualified names use `::` (`kotlin_extractor.rs:197`), so an `ActualFor` ref whose `reference_name` is the actual's own qualified name would fall into Strategy 1 (qualified match). That is wrong: the `expect` and **all** its `actual`s share the *same* qualified name, so `qualified_name_cache` returns the whole family — the generic strategy would match ambiguously, and could even match the actual to itself.

Therefore the KMP strategy must be dispatched **on `reference_kind`, at the very top of `resolve_one`, before the `Uses`-skip block and before Strategy 1**:

```rust
if uref.reference_kind == EdgeKind::ActualFor {
    return self.try_kmp_actual_match(uref);
}
```

Inside `try_kmp_actual_match`: look up `qualified_name_cache` for `uref.reference_name`, then restrict candidates to nodes where `kmp_role == Some("expect")` **and** `kmp_module_root` equals the source node's `kmp_module_root` (prevents cross-module false matches for same-named declarations) **and** `NodeKind` equals the source node's kind (fun↔fun, class↔class — `expect`/`actual` pairs are always the same declaration shape). No cross-language penalty is involved — this strategy bypasses the generic scorer entirely, like `try_go_selector_match`. To read the source node's `kmp_module_root`/kind, look it up by `uref.from_node_id` (the resolver's node cache holds full `&Node` refs, `resolver.rs:269`).

Fan-out across platforms falls out for free: each `actual` node emits its own `UnresolvedRef`, independently resolved to the same `expect` target, producing N `ActualFor` edges converging on one node.

## Phase 2 — AI Context Cross-Target

### Traversal (no change needed)

`context/builder.rs::expand_subgraph` already calls `GraphTraverser::traverse_bfs` with `edge_kinds: None`, `direction: TraversalDirection::Both` — any new `EdgeKind` (including `ActualFor`) is included automatically. No traversal code changes required.

### Guaranteed family completeness

Default BFS is proximity-bound (`traversal_depth`, `max_nodes`), but a sibling `actual` reachable only via `actual-A → expect → actual-B` is 2 hops away and can be dropped by depth/node-count limits or truncation even though it's always relevant. Add a dedicated completion pass:

```rust
fn complete_kmp_families(subgraph: &mut Subgraph, db: &Database) -> Result<()>
```

Called after `expand_subgraph`'s existing trim/edge-recovery step. For every node in the subgraph with `kmp_role.is_some()`, query the DB for all `ActualFor` edges where the node is source or target, and add any missing counterpart nodes/edges — bypassing `max_nodes` for this addition (bounded in practice to the number of KMP targets in the project, typically 2–5).

### Formatter (`context/formatter.rs`)

Target header format (current code-block header is `#### {label} ({file}:{line})`, `formatter.rs:143`), with a KMP label inserted when the node has a `kmp_role`:

```
#### foo [expect · commonMain] (shared/src/commonMain/kotlin/Foo.kt:12)
#### foo [actual · iosMain] (shared/src/iosMain/kotlin/Foo.kt:8)
```

**Structural gap to resolve first.** The formatter cannot see `kmp_role`/`source_set` today:

- `CodeBlock` (`types.rs:635`) carries only `node_id: Option<String>` — not the node's fields.
- The header `label` is currently resolved by looking `block.node_id` up **only against `context.entry_points`** (falling back to the raw node_id). A sibling `actual` pulled in by the Phase 2 completion pass is usually **not** an entry point, so this lookup wouldn't find it at all.

Pick one of two mechanisms (decide in the implementation plan):

1. **Extend `CodeBlock`** with `kmp_role: Option<String>` and `source_set: Option<String>`, populated where code blocks are built (`context/builder.rs`), so the formatter reads them directly. Simpler formatter, small struct/serialization change.
2. **Pass a `node_id → &Node` map covering the whole subgraph** (not just entry_points) into `format_context_as_markdown`, and look up role/source_set there. No struct change, but a new formatter parameter.

Recommendation: option 1 (keeps the formatter a pure function of its inputs; the builder already has the `Node` in hand when it creates each block).

Also update **`format_context_as_json`** (`formatter.rs:173`) so the same `kmp_role`/`source_set` surface in JSON output — otherwise the two output formats disagree on what they expose.

## Testing

Following existing project conventions (`docs/MORE-LANGUAGES-SUPPORT.md` fixture + extraction test pattern):

- **Phase 0:** unit tests for `kmp_location_from_path` covering standard source sets (`commonMain`, `androidMain`, `iosMain`, `jvmMain`, `commonTest`, etc.), non-KMP layouts (returns `None`), and module-root extraction with nested paths.
- **Phase 1:** fixture project with a shared module containing `expect fun`/`actual fun`, `expect class`/`actual class` across `commonMain` + 2 platform source sets; extraction test asserts `kmp_role` is set correctly; resolver test asserts `ActualFor` edges are created with correct fan-in, and that same-named declarations in an unrelated module (different `kmp_module_root`) do NOT get linked.
- **Phase 2:** context-builder test asserts that querying context for one `actual` pulls in the `expect` and sibling `actual`s even under a tight `max_nodes`/`traversal_depth`; formatter test asserts the `[role · source_set]` label appears.

## Out of Scope

- **Phase 3 (deferred): Gradle KMP source-set-aware dependency parsing.** `gradle.rs` today only recognizes flat `dependencies { implementation(...) }` blocks; KMP idiomatically nests dependencies inside `kotlin { sourceSets { commonMain.dependencies { ... } } }`, which isn't currently matched. Extending the tree-sitter visitor to recognize this nested shape and tag dependencies per source-set is independent of Phases 0–2 (which rely only on path heuristics, not Gradle parsing) and can be spec'd separately when prioritized.
- Linking Kotlin `expect`/`actual` declarations to their Swift-side consumers via Kotlin/Native-generated Objective-C/Swift interop headers.
- Any change to the generic (non-KMP) resolver scoring/matching behavior for other languages.
- Windows/other non-standard KMP target layouts beyond the common/android/ios/jvm/js/native/wasm convention family.
