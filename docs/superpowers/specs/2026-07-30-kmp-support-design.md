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

`kmp_location_from_path` scans path segments for one matching `^[a-z][a-zA-Z0-9]*(Main|Test)$` immediately inside a `src/` segment (i.e. `.../src/{segment}/...`). The prefix before `Main`/`Test` becomes the target: `"common"` → `KmpTarget::Common`, anything else → `KmpTarget::Platform(prefix)`. `module_root` is the path up to (excluding) `src/`. Returns `None` for non-KMP-shaped paths (e.g. plain `src/main/kotlin/...` single-platform Android/JVM layout).

This is purely path-convention based — no Gradle parsing required (that's Phase 3). Because source-set and module-root are a **pure function of `file_path`**, they are never stored on the node — any consumer (resolver, formatter, context builder) derives them on demand by calling `kmp_location_from_path` on the node's existing `file_path`. No `Node` column or field is added.

### Schema — side table only (deliberate: avoids struct blast radius)

**Design constraint that drove this.** Adding a field to `Node` (`types.rs:335`) or to `ExtractionResult` (`types.rs:420`) is prohibitively invasive: `Node` is built via explicit struct literals in ~50 extractor files (hundreds of sites, every one must init every field or it won't compile), the DB layer is positional-column-index based (3 `INSERT` sites in `db/queries/nodes.rs`, 18 identical SELECT lists, `row_to_node` at `mod.rs:76`), and `ExtractionResult` is likewise literal-constructed per extractor. So KMP data lives **entirely off to the side** — no shared struct changes at all.

New migration `V15` (`db/migrations.rs`, current `LATEST_VERSION = 14` → bump to `15`) creates one table:

```sql
CREATE TABLE IF NOT EXISTS kmp_declarations (
    node_id     TEXT PRIMARY KEY,
    source_set  TEXT NOT NULL,   -- e.g. "commonMain", "androidMain", "iosMain"
    module_root TEXT NOT NULL,   -- dir containing src/, e.g. "shared"
    role        TEXT NOT NULL    -- 'expect' | 'actual'
);
CREATE INDEX IF NOT EXISTS idx_kmp_declarations_role ON kmp_declarations(role);
```

Follows the migration pattern of `migrate_v8` (`CREATE TABLE IF NOT EXISTS` + index, `migrations.rs:754`). Add `14 => migrate_v14` sibling `15 => migrate_v15` to the dispatch match (`migrations.rs:422`).

**This table is populated in a post-resolution pass, not by the extractor** (see Phase 1) — so nothing threads through `Node`/`ExtractionResult`. `role` and `source_set` are both *derived*: `role` from `ActualFor` edge membership, `source_set`/`module_root` from `file_path`.

## Phase 1 — expect/actual Linking

### Pre-work: grammar verification (spike)

Before writing extraction logic, confirm `tree-sitter-kotlin-sg` v0.4.1 actually parses `expect fun foo()` / `actual fun foo()` with `expect`/`actual` appearing as tokens inside the declaration's `modifiers` node (structurally like `data`/`sealed` today). Write a throwaway fixture and inspect the parse tree. If the grammar mis-parses or doesn't expose these tokens, this phase is blocked on a grammar fix/upgrade first — resolve that before continuing.

### Schema

No new schema in Phase 1 — the `kmp_declarations` table (Phase 0) holds everything. Phase 1 fills it.

### Role handling — derived, never stored on the node

There is no `kmp_role` field anywhere on `Node`. Roles are derived:

- A node is an **`actual`** if it is the *source* of an `ActualFor` edge.
- A node is an **`expect`** if it is the *target* of an `ActualFor` edge.

The Kotlin extractor's only job is to emit an `ActualFor` unresolved ref for each `actual`-modified declaration (below). `expect` nodes need no marking at extraction time — they are discovered as `ActualFor` targets after resolution. (A standalone `expect` with no `actual` is invalid KMP — every `expect` requires an `actual` per target — so edge-derived roles cover all valid code.)

### Extractor changes (`kotlin_extractor.rs`)

Reuse the existing `has_modifier_keyword(node, state, keyword)` helper (`kotlin_extractor.rs:1283`), calling it with `"actual"` at the same call sites already using `"data"`/`"sealed"` (function `:356`, plus class/object/property/interface). When a declaration has the `actual` modifier, push an `UnresolvedRef` onto `state.unresolved_refs` — the exact same channel the extractor already uses for `Uses`/`Extends`/`Annotates` refs (`kotlin_extractor.rs:336`), so **no struct or pipeline change**:

```rust
state.unresolved_refs.push(UnresolvedRef {
    from_node_id: id.clone(),
    reference_name: name.clone(),
    reference_kind: EdgeKind::ActualFor,
    line: start_line,
    column: start_column,
    file_path: state.file_path.clone(),
});
```

**`reference_name` is the bare `name` (e.g. `"platformName"`), not `qualified_name`.** This was wrong in an earlier draft of this spec, caught by the Task 1.5 end-to-end test: `KotlinExtractor`'s `qualified_name` is **file-scoped** — `qualified_prefix()` (`kotlin_extractor.rs:57`) prepends `file_path`, and the per-file node stack it walks always starts with a `(file_path, _)` entry, so a top-level declaration's `qualified_name` is literally `"{file_path}::{file_path}::{name}"`. An `expect` and its `actual`s always live in **different files**, so they never share a `qualified_name` — only the bare `name`, which Kotlin's language rules guarantee is identical. See the corresponding fix in the resolver section below.

`expect` modifier is *not* checked at extraction time. This reuses the existing generic `unresolved_refs → resolve_all → create_edges` pipeline unchanged.

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

**Dispatch placement is critical.** `resolve_one` (`resolver.rs:399`) dispatches by the *shape* of `reference_name` (contains `::` → qualified match; contains `.` → dotted-receiver; else exact name), **not** by `reference_kind`. If the KMP ref's `reference_name` contained `::`, it would fall into Strategy 1 (qualified match) instead of the dedicated strategy below.

Therefore the KMP strategy must be dispatched **on `reference_kind`, at the very top of `resolve_one`, before the `Uses`-skip block and before Strategy 1**:

```rust
if uref.reference_kind == EdgeKind::ActualFor {
    return self.try_kmp_actual_match(uref);
}
```

**Cache choice, corrected.** `qualified_name_cache` is keyed on the file-scoped `qualified_name` — since (per the extractor section above) an `expect` and its `actual`s never share that string, looking them up there returns only the single node itself, not its counterparts. The strategy instead uses `name_cache` (keyed on the bare `name`, which Kotlin guarantees is identical across an `expect`/`actual` pair), plus a new `kmp_logical_path(node)` helper that strips the file-scoping prefix from `qualified_name` (splits on `::`, keeps everything after the first two segments) to recover just the nesting path (e.g. `"Platform::name"` or `"platformName"`) — this disambiguates same-named declarations nested in different classes within the same module, which bare-name matching alone couldn't.

Inside `try_kmp_actual_match` (all fields derived on the fly — nothing is read from a stored KMP column):

1. Look up the source (`actual`) node by `uref.from_node_id` in the resolver's node cache (holds full `&Node` refs, `resolver.rs:269`). Compute its location: `let src = kmp_location_from_path(&source_node.file_path)?;` and `let src_path = kmp_logical_path(source_node);`.
2. Get candidates from `name_cache[uref.reference_name]` (the bare name — `expect` and all sibling `actual`s share it).
3. Keep only candidates where: (a) `kmp_logical_path(candidate) == src_path` (same nesting path, not just same bare name), (b) `kmp_location_from_path(candidate.file_path)` yields the **same `module_root`** as `src` (prevents cross-module false matches), (c) `candidate.kind == source_node.kind` (fun↔fun, class↔class), and (d) the candidate is the **`expect`** — identified structurally as the one whose `source_set` target is `Common` (KMP requires `expect` to live in a common source set), or, if none is `Common`, the single candidate in a source set that differs from every `actual`'s. Exclude the source node itself.
4. Return a `ResolvedRef` to that node. This bypasses the generic scorer entirely, like `try_go_selector_match` — no cross-language penalty involved.

Fan-out across platforms falls out for free: each `actual` node emits its own `UnresolvedRef`, independently resolved to the same `expect` target, producing N `ActualFor` edges converging on one node.

### Post-resolution population of `kmp_declarations` (`tokensave/indexing.rs`)

After `resolve_all` + `create_edges` (the resolver runs at `indexing.rs:370`/`:695`/`:991`), add a pass that fills the side table purely from the freshly created `ActualFor` edges — this is what makes role/source-set queryable without any struct threading:

```
for each ActualFor edge (source = actual_id, target = expect_id):
    for (node_id, role) in [(actual_id, "actual"), (expect_id, "expect")]:
        let loc = kmp_location_from_path(file_path_of(node_id))  // via node map
        upsert kmp_declarations(node_id, loc.source_set, loc.module_root, role)
```

`file_path_of` comes from the `all_nodes` slice already in scope for the resolver. Use `INSERT OR REPLACE` so re-indexing a file is idempotent. A new `Database::insert_kmp_declarations(&[KmpDeclaration])` wraps the write (mirrors `insert_unresolved_refs`, `indexing.rs:660`).

## Phase 2 — AI Context Cross-Target

### Traversal (no change needed)

`context/builder.rs::expand_subgraph` already calls `GraphTraverser::traverse_bfs` with `edge_kinds: None`, `direction: TraversalDirection::Both` — any new `EdgeKind` (including `ActualFor`) is included automatically. No traversal code changes required.

### Guaranteed family completeness

Default BFS is proximity-bound (`traversal_depth`, `max_nodes`), but a sibling `actual` reachable only via `actual-A → expect → actual-B` is 2 hops away and can be dropped by depth/node-count limits or truncation even though it's always relevant. Add a dedicated completion pass:

```rust
async fn complete_kmp_families(&self, subgraph: &mut Subgraph) -> Result<Vec<Node>>
```

Called after `expand_subgraph`'s existing trim/edge-recovery step, from both `build_context` and `find_relevant_context`. It queries `kmp_declarations` for the subgraph's node IDs to find which nodes are KMP declarations, then walks `ActualFor` edges outward, adding missing counterpart nodes/edges — bypassing `max_nodes` for this addition (bounded in practice to the number of KMP targets, typically 2–5). Because role lives in `kmp_declarations` (not on `Node`), this pass needs no node-struct field.

**Must be a fixed-point walk, not a single pass over the seed nodes.** An earlier version queried `ActualFor` edges only for the nodes already in the subgraph (e.g. one `actual`), found the `expect`, and stopped — missing sibling `actual`s that are only discoverable via *the expect's own* incoming edges (the fan-in from other platforms). A queue of node IDs seeded from the subgraph and grown with every newly-discovered node — draining until nothing new turns up — is what actually reaches the full family: discovering the `expect` re-queues it, and processing it surfaces every other platform's `actual`.

**Code blocks must also cover the completed family, not just entry points.** `extract_code_blocks` (called from `build_context`) iterates `entry_points`, not `subgraph.nodes` — a sibling pulled in by `complete_kmp_families` is usually not itself an entry point, so without change it would appear in the "Related Symbols" file:line list but its actual code would never be read. `complete_kmp_families` returns the newly-added nodes; `build_context` feeds `entry_points` chained with that list into `extract_code_blocks`, so the AI reads the sibling's implementation, not just its location.

### Formatter (`context/formatter.rs`)

Target header format (current code-block header is `#### {label} ({file}:{line})`, `formatter.rs:143`), with a KMP label inserted when the block's node is a KMP declaration:

```
#### foo [expect · commonMain] (shared/src/commonMain/kotlin/Foo.kt:12)
#### foo [actual · iosMain] (shared/src/iosMain/kotlin/Foo.kt:8)
```

**Where the label data comes from (no `Node`/`ExtractionResult` change).** The `source_set` is always derivable from `block.file_path` via `kmp_location_from_path`. The `role` comes from `kmp_declarations`. The current header `label` is resolved by looking `block.node_id` up **only against `context.entry_points`** — a sibling `actual` pulled in by the completion pass usually isn't an entry point, so that lookup misses it. Fix by threading a small lookup the builder already has the data for:

- Extend the in-memory `TaskContext` the builder passes to the formatter with a `kmp_labels: HashMap<String /*node_id*/, (String /*role*/, String /*source_set*/)>`, populated in `context/builder.rs` from the `kmp_declarations` rows fetched by `complete_kmp_families` (role) plus `kmp_location_from_path` on each block's path (source_set). The formatter reads `context.kmp_labels.get(node_id)` when emitting the header. This touches only the context-layer `TaskContext` type (a builder-owned struct, not the shared `Node`/`ExtractionResult`/`CodeBlock` extractor structs) and the formatter — no extractor or DB-schema churn.

Also emit the same `role`/`source_set` in **`format_context_as_json`** (`formatter.rs:173`) so the two output formats agree on what they expose.

## Testing

Following existing project conventions (`docs/MORE-LANGUAGES-SUPPORT.md` fixture + extraction test pattern):

- **Phase 0:** unit tests for `kmp_location_from_path` covering standard source sets (`commonMain`, `androidMain`, `iosMain`, `jvmMain`, `commonTest`, etc.), non-KMP layouts (returns `None`), and module-root extraction with nested paths.
- **Phase 1:** fixture project with a shared module containing `expect fun`/`actual fun`, `expect class`/`actual class` across `commonMain` + 2 platform source sets. Extraction test asserts each `actual` decl emits an `ActualFor` unresolved ref. Resolver test asserts `ActualFor` edges are created (source = actual, target = the `commonMain` expect) with correct fan-in (N actuals → 1 expect), and that same-named declarations in an unrelated module (different `module_root`) do NOT get linked. Post-resolution test asserts `kmp_declarations` rows exist with correct `role`/`source_set`/`module_root`.
- **Phase 2:** context-builder test asserts that querying context for one `actual` pulls in the `expect` and sibling `actual`s even under a tight `max_nodes`/`traversal_depth`; formatter test asserts the `[role · source_set]` label appears.

## Out of Scope

- **Phase 3 (deferred): Gradle KMP source-set-aware dependency parsing.** `gradle.rs` today only recognizes flat `dependencies { implementation(...) }` blocks; KMP idiomatically nests dependencies inside `kotlin { sourceSets { commonMain.dependencies { ... } } }`, which isn't currently matched. Extending the tree-sitter visitor to recognize this nested shape and tag dependencies per source-set is independent of Phases 0–2 (which rely only on path heuristics, not Gradle parsing) and can be spec'd separately when prioritized.
- Linking Kotlin `expect`/`actual` declarations to their Swift-side consumers via Kotlin/Native-generated Objective-C/Swift interop headers.
- Any change to the generic (non-KMP) resolver scoring/matching behavior for other languages.
- Windows/other non-standard KMP target layouts beyond the common/android/ios/jvm/js/native/wasm convention family.
