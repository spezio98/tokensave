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

New migration (`db/migrations.rs`, next `LATEST_VERSION`), adding to `nodes`:

```sql
ALTER TABLE nodes ADD COLUMN source_set TEXT NULL;
ALTER TABLE nodes ADD COLUMN kmp_module_root TEXT NULL;
```

Populated by extractors at insert time (like `visibility`/`is_async` today), not computed at query time. Any extractor can call `kmp_location_from_path(file_path)` when building a `Node` — initially wired into `KotlinExtractor` only (Swift/other KMP-relevant files can adopt it later without a schema change).

## Phase 1 — expect/actual Linking

### Pre-work: grammar verification (spike)

Before writing extraction logic, confirm `tree-sitter-kotlin-sg` v0.4.1 actually parses `expect fun foo()` / `actual fun foo()` with `expect`/`actual` appearing as tokens inside the declaration's `modifiers` node (structurally like `data`/`sealed` today). Write a throwaway fixture and inspect the parse tree. If the grammar mis-parses or doesn't expose these tokens, this phase is blocked on a grammar fix/upgrade first — resolve that before continuing.

### Schema

Same migration as Phase 0 (single migration bump adds all three `nodes` columns together — `source_set`, `kmp_module_root`, `kmp_role` — since they're always populated together by the same extractor code path):

```sql
ALTER TABLE nodes ADD COLUMN kmp_role TEXT NULL; -- 'expect' | 'actual' | NULL
```

### Extractor changes (`kotlin_extractor.rs`)

Reuse the existing `has_modifier_keyword(node, state, keyword)` helper, calling it with `"expect"` and `"actual"` at the same call sites already used for `"data"`/`"sealed"` (function, class, object, property, interface declarations). Set `Node.kmp_role` accordingly.

For every node where `kmp_role == Some("actual")`, additionally emit an `UnresolvedRef`:

```rust
UnresolvedRef {
    source: node.id.clone(),
    target_name: node.qualified_name.clone(),
    kind: EdgeKind::ActualFor,
    line: node.start_line,
}
```

This reuses the existing generic `unresolved_refs → resolve_all → create_edges` pipeline unchanged — no new collection/dispatch machinery.

### `types.rs`

New variant: `EdgeKind::ActualFor` (source = `actual` node, target = `expect` node). Direction chosen so an `expect` naturally has fan-in from N `actual`s (one per platform), rather than modeling fan-out from a single node.

### Resolver changes (`resolver.rs`)

New dedicated matching strategy, following the existing `try_go_selector_match` pattern (a targeted branch alongside the generic scorer, not a modification to it):

```rust
fn try_kmp_actual_match(&self, r: &UnresolvedRef) -> Option<ResolvedRef>
```

When `r.kind == EdgeKind::ActualFor`: restrict candidates to nodes with `kmp_role == Some("expect")`, matching `kmp_module_root`, and exact `qualified_name` match. No cross-language penalty applies (always same language). `kind_compatible` requires the target's `NodeKind` to equal the source's `NodeKind` (fun↔fun, class↔class, etc.) — `expect`/`actual` pairs are always the same declaration shape.

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

In `format_context_as_markdown`, when rendering a code block's header for a node with `kmp_role.is_some()`, append a label derived from the node's `kmp_role` and `source_set`:

```
#### foo [expect · commonMain] (shared/src/commonMain/kotlin/Foo.kt:12)
#### foo [actual · iosMain] (shared/src/iosMain/kotlin/Foo.kt:8)
```

Makes the platform variant explicit without requiring the reader (human or AI) to infer it from the file path.

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
