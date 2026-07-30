# KMP Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Teach tokensave Kotlin Multiplatform structure — detect source-set/target from paths, link `expect`↔`actual` declarations with a new edge kind, and surface every platform variant of a symbol together in AI context.

**Architecture:** All KMP data lives in a side table (`kmp_declarations`), populated in a post-resolution pass from `ActualFor` edges — **no field is added to `Node`, `ExtractionResult`, or `CodeBlock`** (these are struct-literal-constructed across ~50 extractor files + a positional-index DB layer, so adding fields is prohibitively invasive). `source_set`/`module_root` are derived from `file_path`; `role` is derived from `ActualFor` edge membership. The Kotlin extractor's only new job is emitting an `ActualFor` unresolved ref for `actual`-modified declarations via the existing `unresolved_refs` channel; a dedicated resolver strategy matches each `actual` to its `commonMain` `expect`.

**Tech Stack:** Rust, tree-sitter (`tree-sitter-kotlin-sg` v0.4.1), libsql/SQLite, tokio async.

## Global Constraints

- Kotlin is a "Lite"-tier (always-on) extractor — no feature flag guards Kotlin code (`src/extraction/mod.rs:262`).
- Run the full suite with `cargo test`; a single test with `cargo test <name>`; lint with `cargo clippy` and format with `cargo fmt` before every commit (`CONTRIBUTING.md:57-62`).
- Kotlin qualified names use `::` as separator (`kotlin_extractor.rs:197`), e.g. `com.example::Foo::bar`.
- Node IDs are content-addressed: `generate_node_id(file_path, &kind, name, start_line)` — `expect` and `actual` of the same declaration already get distinct IDs (different `file_path`).
- `EdgeKind` is stored in the DB as `TEXT`; every variant must appear in both `EdgeKind::as_str` and `EdgeKind::from_str` (`types.rs:266`, `:283`) or edges silently fail to round-trip.
- New KMP types (`KmpDeclaration`) go in `src/types.rs`; the path utility goes in a new `src/extraction/kmp.rs`.

---

## Phase 0 — Source-Set Detection & Side Table

### Task 0.1: Path → source-set/target utility (`src/extraction/kmp.rs`)

**Files:**
- Create: `src/extraction/kmp.rs`
- Modify: `src/extraction/mod.rs` (add `pub mod kmp;` near the other `mod` declarations, ~line 125)
- Test: inline `#[cfg(test)]` module in `src/extraction/kmp.rs`

**Interfaces:**
- Produces:
  - `pub enum KmpTarget { Common, Platform(String) }`
  - `pub struct KmpLocation { pub source_set: String, pub target: KmpTarget, pub module_root: String }`
  - `pub fn kmp_location_from_path(file_path: &str) -> Option<KmpLocation>`

- [ ] **Step 1: Write the failing tests**

In `src/extraction/kmp.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_main_is_common() {
        let loc = kmp_location_from_path("shared/src/commonMain/kotlin/com/x/Foo.kt").unwrap();
        assert_eq!(loc.source_set, "commonMain");
        assert!(matches!(loc.target, KmpTarget::Common));
        assert_eq!(loc.module_root, "shared");
    }

    #[test]
    fn android_main_is_platform() {
        let loc = kmp_location_from_path("shared/src/androidMain/kotlin/Foo.kt").unwrap();
        assert_eq!(loc.source_set, "androidMain");
        assert!(matches!(loc.target, KmpTarget::Platform(ref p) if p == "android"));
        assert_eq!(loc.module_root, "shared");
    }

    #[test]
    fn ios_test_source_set() {
        let loc = kmp_location_from_path("a/b/feature/src/iosTest/kotlin/FooTest.kt").unwrap();
        assert_eq!(loc.source_set, "iosTest");
        assert!(matches!(loc.target, KmpTarget::Platform(ref p) if p == "ios"));
        assert_eq!(loc.module_root, "a/b/feature");
    }

    #[test]
    fn non_kmp_layout_is_none() {
        assert!(kmp_location_from_path("app/src/main/kotlin/Foo.kt").is_none());
        assert!(kmp_location_from_path("src/lib.rs").is_none());
    }

    #[test]
    fn wasm_js_custom_target() {
        let loc = kmp_location_from_path("core/src/wasmJsMain/kotlin/Foo.kt").unwrap();
        assert_eq!(loc.source_set, "wasmJsMain");
        assert!(matches!(loc.target, KmpTarget::Platform(ref p) if p == "wasmJs"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib kmp::tests`
Expected: FAIL — `kmp_location_from_path` not found / module missing.

- [ ] **Step 3: Implement the utility**

In `src/extraction/kmp.rs` (above the test module):

```rust
//! Kotlin Multiplatform (KMP) path conventions: derive the source-set, target
//! platform, and owning module from a file path. Pure path heuristics — no
//! Gradle parsing (that is out of scope; see the KMP design spec).

/// The compilation target a KMP source set belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KmpTarget {
    /// A shared source set (`commonMain`, `commonTest`).
    Common,
    /// A platform-specific target: the prefix before `Main`/`Test`
    /// (`"android"`, `"ios"`, `"jvm"`, `"js"`, `"native"`, `"wasmJs"`, ...).
    Platform(String),
}

/// Where a file sits in a KMP module layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmpLocation {
    /// The source-set directory name, e.g. `"commonMain"`, `"androidMain"`.
    pub source_set: String,
    pub target: KmpTarget,
    /// The path up to (excluding) `src/`, e.g. `"shared"`.
    pub module_root: String,
}

/// Parse a KMP location from a file path, or `None` for non-KMP layouts.
///
/// Matches the first path segment of the shape `{prefix}Main` / `{prefix}Test`
/// that sits immediately inside a `src/` segment (`.../src/{segment}/...`).
pub fn kmp_location_from_path(file_path: &str) -> Option<KmpLocation> {
    let segments: Vec<&str> = file_path.split('/').collect();
    for (i, seg) in segments.iter().enumerate() {
        if i == 0 || segments[i - 1] != "src" {
            continue;
        }
        let prefix = seg
            .strip_suffix("Main")
            .or_else(|| seg.strip_suffix("Test"))?;
        // Require a lowercase-led, alphanumeric prefix (rejects e.g. "Main").
        if prefix.is_empty() || !prefix.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
            continue;
        }
        if !prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let target = if prefix == "common" {
            KmpTarget::Common
        } else {
            KmpTarget::Platform(prefix.to_string())
        };
        let module_root = segments[..i - 1].join("/");
        return Some(KmpLocation {
            source_set: (*seg).to_string(),
            target,
            module_root,
        });
    }
    None
}
```

Add to `src/extraction/mod.rs` alongside the other module declarations:

```rust
pub mod kmp;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib kmp::tests`
Expected: PASS (5 tests).

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt && cargo clippy --lib
git add src/extraction/kmp.rs src/extraction/mod.rs
git commit -m "feat(kmp): add path-based source-set/target detection"
```

---

### Task 0.2: `kmp_declarations` side table + migration V15

**Files:**
- Modify: `src/db/migrations.rs` (bump `LATEST_VERSION`, add `15 => migrate_v15`, add `migrate_v15`)
- Modify: `src/types.rs` (add `KmpDeclaration` struct near `UnresolvedRef`, ~line 409)
- Create: `src/db/queries/kmp.rs` (insert + query helpers)
- Modify: `src/db/queries/mod.rs` (add `pub mod kmp;` / `mod kmp;` following the existing pattern)
- Test: `tests/kmp_db_test.rs`

**Interfaces:**
- Consumes: `kmp_location_from_path` (Task 0.1) — not directly here, but by callers.
- Produces:
  - `pub struct KmpDeclaration { pub node_id: String, pub source_set: String, pub module_root: String, pub role: String }`
  - `Database::insert_kmp_declarations(&self, decls: &[KmpDeclaration]) -> Result<()>`
  - `Database::get_kmp_declarations_for(&self, node_ids: &[String]) -> Result<Vec<KmpDeclaration>>`

- [ ] **Step 1: Write the failing test**

`tests/kmp_db_test.rs`:

```rust
use tempfile::TempDir;
use tokensave::db::migrations::latest_version;
use tokensave::db::Database;
use tokensave::types::KmpDeclaration;

async fn setup_db() -> (Database, TempDir) {
    let dir = TempDir::new().unwrap();
    let (db, _) = Database::initialize(&dir.path().join("test.db")).await.unwrap();
    (db, dir)
}

#[tokio::test]
async fn latest_version_is_15() {
    assert_eq!(latest_version(), 15);
}

#[tokio::test]
async fn insert_and_query_kmp_declarations() {
    let (db, _dir) = setup_db().await;
    let decls = vec![
        KmpDeclaration { node_id: "a".into(), source_set: "androidMain".into(), module_root: "shared".into(), role: "actual".into() },
        KmpDeclaration { node_id: "e".into(), source_set: "commonMain".into(), module_root: "shared".into(), role: "expect".into() },
    ];
    db.insert_kmp_declarations(&decls).await.unwrap();

    let got = db.get_kmp_declarations_for(&["a".into(), "e".into(), "missing".into()]).await.unwrap();
    assert_eq!(got.len(), 2);
    assert!(got.iter().any(|d| d.node_id == "a" && d.role == "actual" && d.source_set == "androidMain"));
    assert!(got.iter().any(|d| d.node_id == "e" && d.role == "expect"));
}

#[tokio::test]
async fn insert_is_idempotent() {
    let (db, _dir) = setup_db().await;
    let d = KmpDeclaration { node_id: "a".into(), source_set: "androidMain".into(), module_root: "shared".into(), role: "actual".into() };
    db.insert_kmp_declarations(&[d.clone()]).await.unwrap();
    db.insert_kmp_declarations(&[d]).await.unwrap(); // re-index: must not error/duplicate
    let got = db.get_kmp_declarations_for(&["a".into()]).await.unwrap();
    assert_eq!(got.len(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test kmp_db_test`
Expected: FAIL — `KmpDeclaration` / `insert_kmp_declarations` / `latest_version() == 15` not present.

- [ ] **Step 3a: Add the `KmpDeclaration` type**

In `src/types.rs`, after the `UnresolvedRef` struct (~line 416):

```rust
/// A row in the `kmp_declarations` side table: tags a node as an `expect` or
/// `actual` KMP declaration, with its source set and owning module. Stored off
/// to the side so no column is added to `Node`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KmpDeclaration {
    pub node_id: String,
    pub source_set: String,
    pub module_root: String,
    pub role: String, // "expect" | "actual"
}
```

- [ ] **Step 3b: Add migration V15**

In `src/db/migrations.rs`: change `const LATEST_VERSION: u32 = 14;` to `15`. Add the dispatch arm after `14 => migrate_v14(conn).await,`:

```rust
        15 => migrate_v15(conn).await,
```

Add the migration function (model on `migrate_v8`, `migrations.rs:754`):

```rust
// ---------------------------------------------------------------------------
// Migration V15: KMP declarations side table
// ---------------------------------------------------------------------------

/// Adds `kmp_declarations`, a side table tagging nodes as `expect`/`actual`
/// Kotlin Multiplatform declarations with their source set and owning module.
/// Kept separate from `nodes` so no positional column is added to the core
/// schema.
async fn migrate_v15(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kmp_declarations (
            node_id     TEXT PRIMARY KEY,
            source_set  TEXT NOT NULL,
            module_root TEXT NOT NULL,
            role        TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_kmp_declarations_role
            ON kmp_declarations(role);",
    )
    .await
    .map_err(|e| TokenSaveError::Database {
        message: format!("v15: failed to create kmp_declarations: {e}"),
        operation: "migrate_v15".to_string(),
    })?;
    Ok(())
}
```

- [ ] **Step 3c: Add the query helpers**

Create `src/db/queries/kmp.rs`:

```rust
//! Queries for the `kmp_declarations` side table.
use super::*;
use crate::types::KmpDeclaration;

impl Database {
    /// Inserts or replaces KMP declaration rows. Idempotent per `node_id`.
    pub async fn insert_kmp_declarations(&self, decls: &[KmpDeclaration]) -> Result<()> {
        for d in decls {
            self.conn()
                .execute(
                    "INSERT OR REPLACE INTO kmp_declarations
                     (node_id, source_set, module_root, role)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        d.node_id.as_str(),
                        d.source_set.as_str(),
                        d.module_root.as_str(),
                        d.role.as_str(),
                    ],
                )
                .await
                .map_err(|e| TokenSaveError::Database {
                    message: format!("failed to insert kmp declaration: {e}"),
                    operation: "insert_kmp_declarations".to_string(),
                })?;
        }
        Ok(())
    }

    /// Returns KMP declaration rows for the given node IDs (missing IDs omitted).
    pub async fn get_kmp_declarations_for(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<KmpDeclaration>> {
        let mut out = Vec::new();
        for id in node_ids {
            let mut rows = self
                .conn()
                .query(
                    "SELECT node_id, source_set, module_root, role
                     FROM kmp_declarations WHERE node_id = ?1",
                    params![id.as_str()],
                )
                .await
                .map_err(|e| TokenSaveError::Database {
                    message: format!("failed to query kmp declarations: {e}"),
                    operation: "get_kmp_declarations_for".to_string(),
                })?;
            if let Some(row) = rows.next().await.map_err(|e| TokenSaveError::Database {
                message: format!("failed to read kmp row: {e}"),
                operation: "get_kmp_declarations_for".to_string(),
            })? {
                out.push(KmpDeclaration {
                    node_id: get_string_lossy(&row, 0)?,
                    source_set: get_string_lossy(&row, 1)?,
                    module_root: get_string_lossy(&row, 2)?,
                    role: get_string_lossy(&row, 3)?,
                });
            }
        }
        Ok(out)
    }
}
```

Register the module in `src/db/queries/mod.rs` next to the existing `mod nodes;` / `mod edges;` lines:

```rust
mod kmp;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test kmp_db_test`
Expected: PASS (3 tests).

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt && cargo clippy
git add src/db/migrations.rs src/types.rs src/db/queries/kmp.rs src/db/queries/mod.rs tests/kmp_db_test.rs
git commit -m "feat(kmp): add kmp_declarations side table (migration v15)"
```

---

## Phase 1 — expect/actual Linking

### Task 1.1: Grammar verification spike (`expect`/`actual` modifiers)

**Files:**
- Test: `tests/kotlin_extraction_test.rs` (add one temporary assertion test)

This task de-risks the assumption that `tree-sitter-kotlin-sg` exposes `expect`/`actual` as modifier tokens. It is a spike: if it fails, STOP and resolve the grammar before Task 1.3.

- [ ] **Step 1: Write a probe test**

Add to `tests/kotlin_extraction_test.rs`:

```rust
#[test]
fn probe_expect_actual_parses_without_errors() {
    let src = "expect fun platformName(): String\n";
    let result = KotlinExtractor.extract("shared/src/commonMain/kotlin/P.kt", src);
    assert!(result.errors.is_empty(), "grammar errors on expect: {:?}", result.errors);
    // The declaration must still be extracted as a function node.
    assert!(
        result.nodes.iter().any(|n| n.name == "platformName"
            && matches!(n.kind, NodeKind::Function)),
        "expect fun not extracted as a Function node: {:?}",
        result.nodes.iter().map(|n| (&n.name, &n.kind)).collect::<Vec<_>>()
    );
}
```

- [ ] **Step 2: Run the probe**

Run: `cargo test --test kotlin_extraction_test probe_expect_actual_parses_without_errors`
Expected: PASS — confirms the grammar parses `expect fun` and the extractor still emits the function node.

- [ ] **Step 3: Decision gate**

If PASS: keep the test (rename mentally as a regression guard) and proceed to Task 1.2.
If FAIL (parse errors or missing node): STOP. The grammar does not handle `expect`/`actual`; a grammar upgrade/patch is required before continuing. Report this back rather than proceeding.

- [ ] **Step 4: Commit the probe**

```bash
git add tests/kotlin_extraction_test.rs
git commit -m "test(kmp): probe that grammar parses expect/actual modifiers"
```

---

### Task 1.2: `EdgeKind::ActualFor`

**Files:**
- Modify: `src/types.rs` (`EdgeKind` enum `:248`, `as_str` `:266`, `from_str` `:283`)
- Test: inline `#[cfg(test)]` in `src/types.rs` (or extend existing edge-kind tests if present)

**Interfaces:**
- Produces: `EdgeKind::ActualFor` with string form `"actual_for"`.

- [ ] **Step 1: Write the failing test**

Add to `src/types.rs` test module (create one if none exists at the bottom of the file):

```rust
#[cfg(test)]
mod edge_kind_tests {
    use super::EdgeKind;

    #[test]
    fn actual_for_round_trips() {
        assert_eq!(EdgeKind::ActualFor.as_str(), "actual_for");
        assert_eq!(EdgeKind::from_str("actual_for"), Some(EdgeKind::ActualFor));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib edge_kind_tests`
Expected: FAIL — `no variant named ActualFor`.

- [ ] **Step 3: Add the variant to all three sites**

In `src/types.rs`:
- Enum (`:248`), after `Documents,`:
  ```rust
      /// Links an `actual` KMP declaration to its `expect` counterpart (#KMP).
      ActualFor,
  ```
- `as_str` (`:266`), before the closing `}` of the match:
  ```rust
              EdgeKind::ActualFor => "actual_for",
  ```
- `from_str` (`:283`), before `_ => None`:
  ```rust
              "actual_for" => Some(EdgeKind::ActualFor),
  ```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib edge_kind_tests`
Expected: PASS.

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt && cargo clippy --lib
git add src/types.rs
git commit -m "feat(kmp): add EdgeKind::ActualFor"
```

---

### Task 1.3: Kotlin extractor emits `ActualFor` refs for `actual` declarations

**Files:**
- Modify: `src/extraction/kotlin_extractor.rs` (function decl site ~`:1005` where the `graph_node` is pushed; and the class/object/property decl sites — each already computes `id`, `qualified_name`, `start_line`, `start_column`)
- Test: `tests/kotlin_extraction_test.rs`

**Interfaces:**
- Consumes: `EdgeKind::ActualFor` (Task 1.2), existing `has_modifier_keyword` (`:1283`), existing `state.unresolved_refs`.
- Produces: one `UnresolvedRef { reference_kind: EdgeKind::ActualFor, reference_name: <qualified_name> }` per `actual`-modified declaration.

- [ ] **Step 1: Write the failing test**

Add to `tests/kotlin_extraction_test.rs`:

```rust
#[test]
fn actual_fun_emits_actual_for_ref() {
    let src = "package com.x\nactual fun platformName(): String = \"android\"\n";
    let result = KotlinExtractor.extract("shared/src/androidMain/kotlin/P.kt", src);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);

    let fun_node = result.nodes.iter().find(|n| n.name == "platformName").unwrap();
    let refs: Vec<_> = result
        .unresolved_refs
        .iter()
        .filter(|r| r.reference_kind == EdgeKind::ActualFor)
        .collect();
    assert_eq!(refs.len(), 1, "expected one ActualFor ref, got {:?}", refs);
    assert_eq!(refs[0].from_node_id, fun_node.id);
    assert_eq!(refs[0].reference_name, fun_node.qualified_name);
}

#[test]
fn expect_fun_emits_no_actual_for_ref() {
    let src = "package com.x\nexpect fun platformName(): String\n";
    let result = KotlinExtractor.extract("shared/src/commonMain/kotlin/P.kt", src);
    assert!(
        result.unresolved_refs.iter().all(|r| r.reference_kind != EdgeKind::ActualFor),
        "expect must not emit ActualFor refs"
    );
}

#[test]
fn plain_fun_emits_no_actual_for_ref() {
    let src = "package com.x\nfun plain(): String = \"x\"\n";
    let result = KotlinExtractor.extract("shared/src/commonMain/kotlin/P.kt", src);
    assert!(result.unresolved_refs.iter().all(|r| r.reference_kind != EdgeKind::ActualFor));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test kotlin_extraction_test actual_for`
Expected: FAIL — no `ActualFor` refs emitted yet.

- [ ] **Step 3: Emit the ref at each declaration site**

In `src/extraction/kotlin_extractor.rs`, in the function-declaration handler, immediately after `state.nodes.push(graph_node);` (~`:1010`), add:

```rust
        if Self::has_modifier_keyword(node, state, "actual") {
            state.unresolved_refs.push(UnresolvedRef {
                from_node_id: id.clone(),
                reference_name: graph_node_qualified_name.clone(),
                reference_kind: EdgeKind::ActualFor,
                line: start_line,
                column: start_column,
                file_path: state.file_path.clone(),
            });
        }
```

Note: `id`, `start_line`, `start_column` are already in scope. Bind the qualified name to a reusable local before it is moved into the `Node` literal — i.e. change the existing `let qualified_name = ...;` usage so a clone is available here (rename the local read above to `graph_node_qualified_name`, or simply `let qn = qualified_name.clone();` before constructing the node and reference `qn` in both the `Node { qualified_name: qn.clone(), .. }` and the ref). Apply the **same block** at the class-declaration, object-declaration, and property-declaration handlers (each already computes its own `id`/`qualified_name`/`start_line`/`start_column`). Do NOT add it to the file-node or import handlers.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test kotlin_extraction_test`
Expected: PASS (new `actual_for` tests + all existing Kotlin tests still green).

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt && cargo clippy
git add src/extraction/kotlin_extractor.rs tests/kotlin_extraction_test.rs
git commit -m "feat(kmp): emit ActualFor refs for actual-modified Kotlin decls"
```

---

### Task 1.4: Resolver strategy `try_kmp_actual_match`

**Files:**
- Modify: `src/resolution/resolver.rs` (dispatch at top of `resolve_one` `:399`; new method near `try_go_selector_match` `:599`)
- Test: `tests/resolution_test.rs`

**Interfaces:**
- Consumes: `kmp_location_from_path` (Task 0.1), `EdgeKind::ActualFor`, `qualified_name_cache` (`:271`).
- Produces: `fn try_kmp_actual_match(&self, uref: &UnresolvedRef) -> Option<ResolvedRef>`; `resolved_by` tag `"kmp-actual"`.

- [ ] **Step 1: Write the failing test**

Add to `tests/resolution_test.rs` (follow the existing `setup_db_with_nodes` pattern — build `Node` literals with all current fields, no KMP fields). Construct three nodes with the **same** `qualified_name` `com.x::platformName` across `commonMain` (expect) + `androidMain` + `iosMain` (actuals), all under module root `shared`, plus one decoy actual in a **different** module:

```rust
#[tokio::test]
async fn actual_for_links_to_common_expect() {
    let dir = TempDir::new().unwrap();
    let (db, _) = Database::initialize(&dir.path().join("t.db")).await.unwrap();

    let mk = |file: &str| Node {
        id: generate_node_id(file, &NodeKind::Function, "platformName", 1),
        kind: NodeKind::Function,
        name: "platformName".into(),
        qualified_name: "com.x::platformName".into(),
        file_path: file.into(),
        start_line: 1, attrs_start_line: 1, end_line: 2, start_column: 0, end_column: 1,
        signature: None, docstring: None, visibility: Visibility::Pub, is_async: false,
        branches: 0, loops: 0, returns: 0, max_nesting: 0, unsafe_blocks: 0,
        unchecked_calls: 0, assertions: 0, cognitive_complexity: 0,
        distinct_operators: 0, distinct_operands: 0, total_operators: 0,
        total_operands: 0, updated_at: 0, parent_id: None,
    };
    let expect = mk("shared/src/commonMain/kotlin/P.kt");
    let android = mk("shared/src/androidMain/kotlin/P.kt");
    let ios = mk("shared/src/iosMain/kotlin/P.kt");
    let decoy = mk("other/src/androidMain/kotlin/P.kt"); // different module_root

    let nodes = vec![expect.clone(), android.clone(), ios.clone(), decoy.clone()];
    let resolver = ReferenceResolver::from_nodes(&db, &nodes);

    let mk_ref = |n: &Node| UnresolvedRef {
        from_node_id: n.id.clone(),
        reference_name: "com.x::platformName".into(),
        reference_kind: EdgeKind::ActualFor,
        line: 1, column: 0, file_path: n.file_path.clone(),
    };

    // Both real actuals resolve to the commonMain expect (fan-in).
    let r_android = resolver.resolve_one(&mk_ref(&android)).expect("android resolves");
    assert_eq!(r_android.target_node_id, expect.id);
    let r_ios = resolver.resolve_one(&mk_ref(&ios)).expect("ios resolves");
    assert_eq!(r_ios.target_node_id, expect.id);

    // The decoy in a different module must NOT resolve to this expect.
    let r_decoy = resolver.resolve_one(&mk_ref(&decoy));
    assert!(
        r_decoy.as_ref().map_or(true, |r| r.target_node_id != expect.id),
        "decoy in other module linked across modules"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test resolution_test actual_for_links_to_common_expect`
Expected: FAIL — `ActualFor` currently falls through to the generic `::` qualified-match, returning an ambiguous/self match.

- [ ] **Step 3: Add dispatch + strategy**

At the very top of `resolve_one` (`resolver.rs:399`), before the `Uses`-skip block:

```rust
        if uref.reference_kind == EdgeKind::ActualFor {
            return self.try_kmp_actual_match(uref);
        }
```

Add the method near `try_go_selector_match`:

```rust
    /// KMP `expect`/`actual` resolution: link an `actual` declaration to its
    /// `expect` counterpart. The `expect` and every `actual` share the same
    /// qualified name, so the generic qualified-name strategy cannot
    /// disambiguate them — this strategy narrows by module root + node kind and
    /// picks the `Common`-source-set declaration as the `expect`.
    fn try_kmp_actual_match(&self, uref: &UnresolvedRef) -> Option<ResolvedRef> {
        use crate::extraction::kmp::{kmp_location_from_path, KmpTarget};

        let candidates = self.qualified_name_cache.get(uref.reference_name.as_str())?;
        // The source (actual) node is itself in this bucket (same qualified name).
        let source = candidates.iter().copied().find(|n| n.id == uref.from_node_id)?;
        let src_loc = kmp_location_from_path(&source.file_path)?;

        let matched: Vec<&Node> = candidates
            .iter()
            .copied()
            .filter(|n| n.id != source.id)
            .filter(|n| n.kind == source.kind)
            .filter_map(|n| kmp_location_from_path(&n.file_path).map(|loc| (n, loc)))
            .filter(|(_, loc)| loc.module_root == src_loc.module_root)
            .filter(|(_, loc)| loc.source_set != src_loc.source_set)
            .map(|(n, _)| n)
            .collect();

        // Prefer the Common-source-set declaration (the canonical `expect`);
        // fall back to the sole remaining candidate if none is Common.
        // (`expect` is a `&Node` into the shared node arena, not into `matched`.)
        let expect = matched
            .iter()
            .copied()
            .find(|n| {
                kmp_location_from_path(&n.file_path)
                    .is_some_and(|loc| loc.target == KmpTarget::Common)
            })
            .or_else(|| if matched.len() == 1 { matched.first().copied() } else { None })?;

        Some(ResolvedRef {
            original: uref.clone(),
            target_node_id: expect.id.clone(),
            confidence: 0.95,
            resolved_by: "kmp-actual".to_string(),
        })
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test resolution_test`
Expected: PASS (new test + existing resolution tests green).

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt && cargo clippy
git add src/resolution/resolver.rs tests/resolution_test.rs
git commit -m "feat(kmp): resolve actual decls to their commonMain expect"
```

---

### Task 1.5: Populate `kmp_declarations` post-resolution (`tokensave/indexing.rs`)

**Files:**
- Modify: `src/tokensave/indexing.rs` (after each resolver run: `:370`, `:695`, `:991` — factor into one helper to avoid triplication)
- Test: `tests/kmp_pipeline_test.rs`

**Interfaces:**
- Consumes: `Database::insert_kmp_declarations` (Task 0.2), `kmp_location_from_path` (Task 0.1), the resolved `ActualFor` edges + `all_nodes` in scope.
- Produces: `kmp_declarations` rows for every node touched by an `ActualFor` edge.

- [ ] **Step 1: Write the failing test**

`tests/kmp_pipeline_test.rs` — index a fixture KMP module end-to-end and assert the side table is filled. Use the project's existing end-to-end harness (mirror `tests/sync_test.rs` for how a temp project is indexed; create the fixture files under a `TempDir`). Assert:

```rust
// After indexing a shared module with:
//   commonMain/P.kt:  expect fun platformName(): String
//   androidMain/P.kt: actual fun platformName(): String = "a"
//   iosMain/P.kt:     actual fun platformName(): String = "i"
//
// 1. Two ActualFor edges exist (android->expect, ios->expect).
// 2. kmp_declarations has 3 rows: expect(commonMain), actual(androidMain), actual(iosMain),
//    all module_root == "<module dir>".
let decls = db.get_kmp_declarations_for(&all_ids).await.unwrap();
assert_eq!(decls.iter().filter(|d| d.role == "expect").count(), 1);
assert_eq!(decls.iter().filter(|d| d.role == "actual").count(), 2);
assert!(decls.iter().find(|d| d.role == "expect").unwrap().source_set == "commonMain");
```

(Write the full harness modeled on `sync_test.rs`: create the temp dir + files, run the indexing entry point that `sync_test.rs` uses, open the DB, query edges + `get_kmp_declarations_for`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test kmp_pipeline_test`
Expected: FAIL — `kmp_declarations` is empty (nothing populates it yet).

- [ ] **Step 3: Add the population helper and call it after resolution**

In `src/tokensave/indexing.rs`, add a helper method on the indexing type:

```rust
    /// Fills `kmp_declarations` from freshly-created `ActualFor` edges. Roles
    /// are derived from edge direction (source = actual, target = expect);
    /// source-set/module-root are derived from each node's path.
    async fn populate_kmp_declarations(
        &self,
        edges: &[Edge],
        nodes: &[Node],
    ) -> Result<()> {
        use crate::extraction::kmp::kmp_location_from_path;
        use std::collections::HashMap;

        let path_of: HashMap<&str, &str> =
            nodes.iter().map(|n| (n.id.as_str(), n.file_path.as_str())).collect();

        let mut decls: Vec<KmpDeclaration> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in edges.iter().filter(|e| e.kind == EdgeKind::ActualFor) {
            for (node_id, role) in [(&e.source, "actual"), (&e.target, "expect")] {
                if !seen.insert(node_id.clone()) {
                    continue;
                }
                let Some(path) = path_of.get(node_id.as_str()) else { continue };
                let Some(loc) = kmp_location_from_path(path) else { continue };
                decls.push(KmpDeclaration {
                    node_id: node_id.clone(),
                    source_set: loc.source_set,
                    module_root: loc.module_root,
                    role: role.to_string(),
                });
            }
        }
        if !decls.is_empty() {
            self.db.insert_kmp_declarations(&decls).await?;
        }
        Ok(())
    }
```

After each place the resolver produces edges and they are written (the `create_edges`/`insert` sites near `:370`, `:695`, `:991`), call:

```rust
        self.populate_kmp_declarations(&resolved_edges, &all_nodes).await?;
```

Bind `resolved_edges`/`all_nodes` to whatever those locals are named at each site (the resolved edge vec and the `all_nodes` slice already loaded for `from_nodes`). If the three sites share enough structure, extract the resolver-run block into one helper and call `populate_kmp_declarations` once inside it.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test kmp_pipeline_test`
Expected: PASS.

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt && cargo clippy
git add src/tokensave/indexing.rs tests/kmp_pipeline_test.rs
git commit -m "feat(kmp): populate kmp_declarations from ActualFor edges"
```

---

## Phase 2 — AI Context Cross-Target

### Task 2.1: `complete_kmp_families` completion pass (`context/builder.rs`)

**Files:**
- Modify: `src/context/builder.rs` (call after `expand_subgraph`'s trim/edge-recovery, ~`:687`; new method)
- Test: `tests/kmp_context_test.rs`

**Interfaces:**
- Consumes: `Database::get_kmp_declarations_for` (Task 0.2), `Database` edge queries, `Subgraph` (`types.rs:462`).
- Produces: `async fn complete_kmp_families(&self, subgraph: &mut Subgraph) -> Result<()>`.

- [ ] **Step 1: Write the failing test**

`tests/kmp_context_test.rs`: index the same 3-file KMP module (reuse the Task 1.5 fixture builder). Build context for the `androidMain` actual with a **tight** budget (`max_nodes = 2`, `traversal_depth = 1`) so plain BFS would omit the `iosMain` sibling. Assert the returned subgraph contains all three nodes (expect + both actuals):

```rust
let ctx = builder.build_context("platformName android", &opts_tight).await.unwrap();
let files: Vec<&str> = ctx.subgraph.nodes.iter().map(|n| n.file_path.as_str()).collect();
assert!(files.iter().any(|f| f.contains("commonMain")), "expect missing");
assert!(files.iter().any(|f| f.contains("androidMain")), "android missing");
assert!(files.iter().any(|f| f.contains("iosMain")), "ios sibling missing under tight budget");
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test kmp_context_test`
Expected: FAIL — under the tight budget the `iosMain` sibling is dropped.

- [ ] **Step 3: Implement the completion pass**

In `src/context/builder.rs`, add the method and call it at the end of `expand_subgraph` (after the edge-recovery `retain`, before `Ok(Subgraph { .. })` — or immediately after the `expand_subgraph` call in `build_context`, whichever holds `&mut Subgraph`):

```rust
    /// Guarantees KMP family completeness: for any node in the subgraph that is
    /// an `expect`/`actual` declaration, pull in every counterpart reachable via
    /// `ActualFor` edges, bypassing the node budget (families are tiny — one
    /// node per target).
    async fn complete_kmp_families(&self, subgraph: &mut Subgraph) -> Result<()> {
        use std::collections::HashSet;

        let ids: Vec<String> = subgraph.nodes.iter().map(|n| n.id.clone()).collect();
        let decls = self.db.get_kmp_declarations_for(&ids).await?;
        if decls.is_empty() {
            return Ok(());
        }

        let mut present: HashSet<String> = ids.into_iter().collect();
        let mut present_edges: HashSet<(String, String)> = subgraph
            .edges
            .iter()
            .map(|e| (e.source.clone(), e.target.clone()))
            .collect();

        for decl in &decls {
            // All ActualFor edges touching this node, in both directions.
            let edges = self.db.get_actual_for_edges_for(&decl.node_id).await?;
            for edge in edges {
                for counterpart in [&edge.source, &edge.target] {
                    if present.insert(counterpart.clone()) {
                        if let Some(node) = self.db.get_node_by_id(counterpart).await? {
                            subgraph.nodes.push(node);
                        }
                    }
                }
                if present_edges.insert((edge.source.clone(), edge.target.clone())) {
                    subgraph.edges.push(edge);
                }
            }
        }
        Ok(())
    }
```

Add the supporting query to `src/db/queries/kmp.rs` (or `edges.rs`):

```rust
    /// Returns all `ActualFor` edges where `node_id` is the source or target.
    pub async fn get_actual_for_edges_for(&self, node_id: &str) -> Result<Vec<Edge>> {
        let mut rows = self
            .conn()
            .query(
                "SELECT source, target, kind, line FROM edges
                 WHERE kind = 'actual_for' AND (source = ?1 OR target = ?1)",
                params![node_id],
            )
            .await
            .map_err(|e| TokenSaveError::Database {
                message: format!("failed to query actual_for edges: {e}"),
                operation: "get_actual_for_edges_for".to_string(),
            })?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| TokenSaveError::Database {
            message: format!("failed to read edge row: {e}"),
            operation: "get_actual_for_edges_for".to_string(),
        })? {
            out.push(Edge {
                source: get_string_lossy(&row, 0)?,
                target: get_string_lossy(&row, 1)?,
                kind: EdgeKind::from_str(&get_string_lossy(&row, 2)?)
                    .unwrap_or(EdgeKind::ActualFor),
                line: row.get::<i64>(3).ok().map(|l| l as u32),
            });
        }
        Ok(out)
    }
```

Call `self.complete_kmp_families(&mut subgraph).await?;` right after `expand_subgraph` returns in `build_context`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test kmp_context_test`
Expected: PASS.

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt && cargo clippy
git add src/context/builder.rs src/db/queries/kmp.rs tests/kmp_context_test.rs
git commit -m "feat(kmp): always complete expect/actual families in context"
```

---

### Task 2.2: Formatter platform labels (`context/formatter.rs`)

**Files:**
- Modify: `src/types.rs` (`TaskContext` `:622` — add `kmp_labels` field)
- Modify: `src/context/builder.rs` (populate `kmp_labels` when assembling `TaskContext`)
- Modify: `src/context/formatter.rs` (`format_context_as_markdown` `:143`, `format_context_as_json` `:173`)
- Test: `tests/kmp_context_test.rs` (extend) + formatter unit test in `src/context/formatter.rs`

**Interfaces:**
- Consumes: `kmp_location_from_path` (Task 0.1), `Database::get_kmp_declarations_for` (Task 0.2), the completed subgraph (Task 2.1).
- Produces: `TaskContext.kmp_labels: HashMap<String, (String, String)>` mapping `node_id → (role, source_set)`; markdown header `#### {name} [{role} · {source_set}] ({file}:{line})`.

- [ ] **Step 1: Write the failing test**

Add a formatter unit test in `src/context/formatter.rs` test module:

```rust
#[test]
fn markdown_shows_kmp_label() {
    let mut ctx = make_test_context(); // existing helper
    ctx.code_blocks = vec![CodeBlock {
        content: "actual fun foo() {}".into(),
        file_path: "shared/src/iosMain/kotlin/Foo.kt".into(),
        start_line: 7,
        end_line: 8,
        node_id: Some("nid".into()),
    }];
    ctx.kmp_labels.insert("nid".into(), ("actual".into(), "iosMain".into()));
    let md = format_context_as_markdown(&ctx);
    assert!(md.contains("[actual · iosMain]"), "missing kmp label:\n{md}");
}
```

Also extend `tests/kmp_context_test.rs` to assert the rendered markdown for the indexed fixture contains `[expect · commonMain]` and `[actual · androidMain]`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib markdown_shows_kmp_label && cargo test --test kmp_context_test`
Expected: FAIL — `kmp_labels` field does not exist / label not rendered.

- [ ] **Step 3a: Add the `TaskContext` field**

In `src/types.rs` `TaskContext` (`:622`), add:

```rust
    /// Maps a node_id to its `(role, source_set)` KMP label, for the formatter.
    /// Empty for non-KMP contexts. Not part of the persisted graph.
    #[serde(default)]
    pub kmp_labels: std::collections::HashMap<String, (String, String)>,
```

Update every `TaskContext { .. }` construction site (search `TaskContext {`) to initialize `kmp_labels: HashMap::new()` — except the builder site below, which fills it. Update the existing `make_test_context()` helper in `formatter.rs` tests to add `kmp_labels: HashMap::new()`.

- [ ] **Step 3b: Populate it in the builder**

In `src/context/builder.rs`, where `TaskContext` is assembled (after `complete_kmp_families`), build the map:

```rust
        let mut kmp_labels = std::collections::HashMap::new();
        let node_ids: Vec<String> = subgraph.nodes.iter().map(|n| n.id.clone()).collect();
        for decl in self.db.get_kmp_declarations_for(&node_ids).await? {
            kmp_labels.insert(decl.node_id, (decl.role, decl.source_set));
        }
```

and pass `kmp_labels` into the `TaskContext { .. }` literal.

- [ ] **Step 3c: Render the label (markdown + json)**

In `format_context_as_markdown` (`formatter.rs:143`), replace the header `writeln!`:

```rust
            let kmp = block
                .node_id
                .as_ref()
                .and_then(|id| context.kmp_labels.get(id))
                .map(|(role, ss)| format!(" [{role} · {ss}]"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "#### {}{} ({}:{})",
                label,
                kmp,
                block.file_path,
                block.start_line + 1,
            );
```

In `format_context_as_json` (`:173`), ensure `kmp_labels` is included in the serialized `TaskContext` (it is, once the field exists and derives `Serialize`) — no extra code needed beyond the struct field.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib markdown_shows_kmp_label && cargo test --test kmp_context_test`
Expected: PASS.

- [ ] **Step 5: Full suite + lint + commit**

```bash
cargo test
cargo fmt && cargo clippy
git add src/types.rs src/context/builder.rs src/context/formatter.rs tests/kmp_context_test.rs
git commit -m "feat(kmp): label expect/actual variants in context output"
```

---

## Self-Review Notes

- **Spec coverage:** Phase 0 (§ Source-Set Detection) → Tasks 0.1–0.2. Phase 1 (grammar spike, EdgeKind, extractor, resolver, post-resolution population) → Tasks 1.1–1.5. Phase 2 (traversal reuse, completion pass, formatter) → Tasks 2.1–2.2. Traversal "no change needed" is verified in the plan by Task 2.1 relying on the existing `edge_kinds: None` BFS. Phase 3 (Gradle) is out of scope per the spec — no task, intentionally.
- **Type consistency:** `KmpDeclaration { node_id, source_set, module_root, role }`, `kmp_location_from_path -> Option<KmpLocation>`, `EdgeKind::ActualFor`/`"actual_for"`, `try_kmp_actual_match`, `populate_kmp_declarations`, `complete_kmp_families`, `get_actual_for_edges_for`, `get_kmp_declarations_for`, `TaskContext.kmp_labels: HashMap<String,(String,String)>` — used consistently across tasks.
- **No `Node`/`ExtractionResult`/`CodeBlock` field added** — confirmed; the only shared-struct change is `TaskContext.kmp_labels` (a builder-owned context struct, not an extractor/DB struct), guarded with `#[serde(default)]`.
