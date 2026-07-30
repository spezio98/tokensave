# KMP `actual typealias` Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the merged KMP support so `expect class` + `actual typealias` (the common iOS KMP pattern) links, by adding a Kotlin `typealias` extractor visitor and relaxing the resolver's kind-match rule.

**Architecture:** Add `visit_type_alias` to `kotlin_extractor.rs` (modeled on `SwiftExtractor::visit_typealias`), producing `NodeKind::TypeAlias` nodes; the existing generic `maybe_emit_actual_for_ref` needs no change. Relax `try_kmp_actual_match`'s exact-kind filter with a new `kmp_kind_compatible` helper that allows `TypeAlias` ↔ any Kotlin type-declaration kind, but never `TypeAlias` ↔ `Function`/`Property`.

**Tech Stack:** Rust, tree-sitter (`tree-sitter-kotlin-sg` v0.4.1), libsql/SQLite, tokio async.

## Global Constraints

- Run `cargo test` before every commit; `cargo clippy --all-targets` and `cargo fmt` clean.
- Grammar node kind for Kotlin typealias is `type_alias` (confirmed via AST probe — see spec).
- `NodeKind::TypeAlias` already exists in `src/types.rs` — no schema change needed anywhere in this plan.

---

### Task 1: Kotlin extractor `visit_type_alias`

**Files:**
- Modify: `src/extraction/kotlin_extractor.rs` (dispatch table in `visit_node`, ~line 166-182; new function near `visit_property`)
- Test: `tests/kotlin_extraction_test.rs`

**Interfaces:**
- Consumes: `find_child_by_kind`, `state.qualified_prefix()`, `generate_node_id`, `Self::maybe_emit_actual_for_ref` (existing, unchanged).
- Produces: a `NodeKind::TypeAlias` node per `typealias` declaration.

- [ ] **Step 1: Write the failing tests**

Add to `tests/kotlin_extraction_test.rs`:

```rust
#[test]
fn plain_typealias_extracted_no_actual_for_ref() {
    let src = "typealias Foo = Bar\n";
    let result = KotlinExtractor.extract("test.kt", src);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
    let alias = result
        .nodes
        .iter()
        .find(|n| n.name == "Foo" && matches!(n.kind, NodeKind::TypeAlias));
    assert!(alias.is_some(), "no TypeAlias node: {:?}", result.nodes.iter().map(|n| (&n.name, &n.kind)).collect::<Vec<_>>());
    assert!(result.unresolved_refs.iter().all(|r| r.reference_kind != EdgeKind::ActualFor));
}

#[test]
fn actual_typealias_emits_actual_for_ref() {
    let src = "actual typealias Platform = AndroidPlatform\n";
    let result = KotlinExtractor.extract("shared/src/androidMain/kotlin/P.kt", src);
    assert!(result.errors.is_empty(), "errors: {:?}", result.errors);

    let alias_node = result
        .nodes
        .iter()
        .find(|n| n.name == "Platform" && matches!(n.kind, NodeKind::TypeAlias))
        .expect("typealias node not found");
    let refs: Vec<_> = result
        .unresolved_refs
        .iter()
        .filter(|r| r.reference_kind == EdgeKind::ActualFor)
        .collect();
    assert_eq!(refs.len(), 1, "expected one ActualFor ref, got {:?}", refs);
    assert_eq!(refs[0].from_node_id, alias_node.id);
    assert_eq!(refs[0].reference_name, alias_node.name);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test kotlin_extraction_test typealias`
Expected: FAIL — no `TypeAlias` node produced (no visitor exists yet).

- [ ] **Step 3: Add the dispatch entry and visitor**

In `src/extraction/kotlin_extractor.rs`, find the `visit_node` dispatch match (the fixed whitelist of top-level node kinds — currently `package_header`, `import_list`, `import_header`, `function_declaration`, `class_declaration`, `object_declaration`, `companion_object`, `property_declaration`, `secondary_constructor`) and add:

```rust
            "type_alias" => Self::visit_type_alias(state, node),
```

Add the new function near `visit_property` (~line 1035, before the "Secondary Constructor" section comment):

```rust
    /// Extract a typealias declaration.
    fn visit_type_alias(state: &mut ExtractionState, node: TsNode<'_>) {
        let name = find_child_by_kind(node, "type_identifier")
            .map_or_else(|| "<anonymous>".to_string(), |n| state.node_text(n));
        let visibility = Self::extract_visibility(node, state);
        let start_line = node.start_position().row as u32;
        let end_line = node.end_position().row as u32;
        let start_column = node.start_position().column as u32;
        let end_column = node.end_position().column as u32;
        let qualified_name = format!("{}::{}", state.qualified_prefix(), name);
        let id = generate_node_id(&state.file_path, &NodeKind::TypeAlias, &name, start_line);

        let graph_node = Node {
            id: id.clone(),
            kind: NodeKind::TypeAlias,
            name: name.clone(),
            qualified_name,
            file_path: state.file_path.clone(),
            start_line,
            attrs_start_line: start_line,
            end_line,
            start_column,
            end_column,
            signature: Some(state.node_text(node).trim().to_string()),
            docstring: None,
            visibility,
            is_async: false,
            branches: 0,
            loops: 0,
            returns: 0,
            max_nesting: 0,
            unsafe_blocks: 0,
            unchecked_calls: 0,
            assertions: 0,
            cognitive_complexity: 0,
            distinct_operators: 0,
            distinct_operands: 0,
            total_operators: 0,
            total_operands: 0,
            updated_at: state.timestamp,
            parent_id: None,
        };
        state.nodes.push(graph_node);
        Self::maybe_emit_actual_for_ref(state, node);

        if let Some(parent_id) = state.parent_node_id() {
            state.edges.push(Edge {
                source: parent_id.to_string(),
                target: id,
                kind: EdgeKind::Contains,
                line: Some(start_line),
            });
        }
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test kotlin_extraction_test`
Expected: PASS — new tests plus all existing Kotlin tests green (no regressions).

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt && cargo clippy --all-targets
git add src/extraction/kotlin_extractor.rs tests/kotlin_extraction_test.rs
git commit -m "feat(kmp): extract Kotlin typealias declarations"
```

---

### Task 2: Resolver kind-compatibility relaxation

**Files:**
- Modify: `src/resolution/resolver.rs` (`try_kmp_actual_match`, the `.filter(|n| n.kind == source.kind)` line; new `kmp_kind_compatible` helper near it)
- Test: `tests/resolution_test.rs`

**Interfaces:**
- Consumes: `NodeKind` (from `crate::types`).
- Produces: `fn kmp_kind_compatible(a: NodeKind, b: NodeKind) -> bool`.

- [ ] **Step 1: Write the failing tests**

Add to `tests/resolution_test.rs` (follow the existing `actual_for_links_to_common_expect` pattern — a `mk` closure building nodes with file-scoped qualified names):

```rust
#[tokio::test]
async fn actual_typealias_links_to_expect_class() {
    let dir = TempDir::new().unwrap();
    let (db, _) = Database::initialize(&dir.path().join("t.db")).await.unwrap();

    let mk = |file: &str, kind: NodeKind| Node {
        id: generate_node_id(file, &kind, "Platform", 1),
        kind,
        name: "Platform".into(),
        qualified_name: format!("{file}::{file}::Platform"),
        file_path: file.into(),
        start_line: 1, attrs_start_line: 1, end_line: 2, start_column: 0, end_column: 1,
        signature: None, docstring: None, visibility: Visibility::Pub, is_async: false,
        branches: 0, loops: 0, returns: 0, max_nesting: 0, unsafe_blocks: 0,
        unchecked_calls: 0, assertions: 0, cognitive_complexity: 0,
        distinct_operators: 0, distinct_operands: 0, total_operators: 0, total_operands: 0,
        updated_at: 0, parent_id: None,
    };
    let expect_class = mk("shared/src/commonMain/kotlin/P.kt", NodeKind::Class);
    let actual_alias = mk("shared/src/androidMain/kotlin/P.kt", NodeKind::TypeAlias);

    let nodes = vec![expect_class.clone(), actual_alias.clone()];
    let resolver = ReferenceResolver::from_nodes(&db, &nodes);

    let uref = UnresolvedRef {
        from_node_id: actual_alias.id.clone(),
        reference_name: "Platform".into(),
        reference_kind: EdgeKind::ActualFor,
        line: 1, column: 0,
        file_path: actual_alias.file_path.clone(),
    };
    let resolved = resolver.resolve_one(&uref).expect("actual typealias should resolve to expect class");
    assert_eq!(resolved.target_node_id, expect_class.id);
}

#[tokio::test]
async fn actual_typealias_does_not_link_to_expect_function() {
    let dir = TempDir::new().unwrap();
    let (db, _) = Database::initialize(&dir.path().join("t.db")).await.unwrap();

    let mk = |file: &str, kind: NodeKind| Node {
        id: generate_node_id(file, &kind, "Platform", 1),
        kind,
        name: "Platform".into(),
        qualified_name: format!("{file}::{file}::Platform"),
        file_path: file.into(),
        start_line: 1, attrs_start_line: 1, end_line: 2, start_column: 0, end_column: 1,
        signature: None, docstring: None, visibility: Visibility::Pub, is_async: false,
        branches: 0, loops: 0, returns: 0, max_nesting: 0, unsafe_blocks: 0,
        unchecked_calls: 0, assertions: 0, cognitive_complexity: 0,
        distinct_operators: 0, distinct_operands: 0, total_operators: 0, total_operands: 0,
        updated_at: 0, parent_id: None,
    };
    // A same-named `expect fun` must NOT be treated as this typealias's counterpart.
    let expect_fn = mk("shared/src/commonMain/kotlin/P.kt", NodeKind::Function);
    let actual_alias = mk("shared/src/androidMain/kotlin/P.kt", NodeKind::TypeAlias);

    let nodes = vec![expect_fn.clone(), actual_alias.clone()];
    let resolver = ReferenceResolver::from_nodes(&db, &nodes);

    let uref = UnresolvedRef {
        from_node_id: actual_alias.id.clone(),
        reference_name: "Platform".into(),
        reference_kind: EdgeKind::ActualFor,
        line: 1, column: 0,
        file_path: actual_alias.file_path.clone(),
    };
    assert!(
        resolver.resolve_one(&uref).is_none(),
        "actual typealias must not link to an expect function"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --test resolution_test typealias`
Expected: FAIL — `actual_typealias_links_to_expect_class` fails because the current `.filter(|n| n.kind == source.kind)` rejects `Class` vs `TypeAlias` (different kinds). The negative test passes trivially already (not useful yet as a red/green signal, but keep it — it must still pass after Step 3).

- [ ] **Step 3: Add `kmp_kind_compatible` and use it**

In `src/resolution/resolver.rs`, near `kmp_logical_path` (added in the merged KMP work):

```rust
/// True if `a`/`b` are the same `NodeKind`, or one is `TypeAlias` and the
/// other is a Kotlin type-declaration kind. An `actual typealias` can
/// satisfy `expect class`/`interface`/`object` (a real Kotlin language
/// rule), but never `expect fun`/`expect val` — those always require a
/// real `actual fun`/`actual val`.
fn kmp_kind_compatible(a: NodeKind, b: NodeKind) -> bool {
    if a == b {
        return true;
    }
    const TYPE_DECL_KINDS: &[NodeKind] = &[
        NodeKind::Class,
        NodeKind::InnerClass,
        NodeKind::SealedClass,
        NodeKind::DataClass,
        NodeKind::Trait, // Kotlin `interface` extracts as Trait
        NodeKind::KotlinObject,
        NodeKind::CompanionObject,
        NodeKind::Enum,
    ];
    let other = if a == NodeKind::TypeAlias {
        b
    } else if b == NodeKind::TypeAlias {
        a
    } else {
        return false;
    };
    TYPE_DECL_KINDS.contains(&other)
}
```

In `try_kmp_actual_match`, change:

```rust
            .filter(|n| n.kind == source.kind)
```

to:

```rust
            .filter(|n| kmp_kind_compatible(n.kind, source.kind))
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test resolution_test`
Expected: PASS — both new tests, plus the existing `actual_for_links_to_common_expect` (same-kind case still works — `kmp_kind_compatible` returns `true` on the `a == b` fast path).

- [ ] **Step 5: Lint + commit**

```bash
cargo fmt && cargo clippy --all-targets
git add src/resolution/resolver.rs tests/resolution_test.rs
git commit -m "feat(kmp): allow actual typealias to satisfy expect class/interface/object"
```

---

### Task 3: End-to-end pipeline test

**Files:**
- Test: `tests/kmp_pipeline_test.rs` (extend the existing 3-file fixture pattern with a second scenario)

**Interfaces:**
- Consumes: `TokenSave::init`/`index_all` (existing), `Database::get_kmp_declarations_for` (existing).

- [ ] **Step 1: Write the failing test**

Add to `tests/kmp_pipeline_test.rs`:

```rust
async fn setup_kmp_typealias_module() -> (TokenSave, TempDir) {
    let dir = TempDir::new().unwrap();
    let project = dir.path();

    fs::create_dir_all(project.join("shared/src/commonMain/kotlin")).unwrap();
    fs::create_dir_all(project.join("shared/src/androidMain/kotlin")).unwrap();

    fs::write(
        project.join("shared/src/commonMain/kotlin/Platform.kt"),
        "package com.x\n\nexpect class Platform {\n    val name: String\n}\n",
    )
    .unwrap();
    fs::write(
        project.join("shared/src/androidMain/kotlin/Platform.kt"),
        "package com.x\n\nactual typealias Platform = AndroidPlatform\n",
    )
    .unwrap();

    let cg = TokenSave::init(project).await.unwrap();
    cg.index_all().await.unwrap();
    (cg, dir)
}

#[tokio::test]
async fn actual_typealias_links_to_expect_class_end_to_end() {
    let (cg, _dir) = setup_kmp_typealias_module().await;

    let all_nodes = cg.db().get_all_nodes().await.unwrap();
    let all_edges = cg.db().get_all_edges().await.unwrap();

    let actual_for_edges: Vec<_> = all_edges
        .iter()
        .filter(|e| e.kind == EdgeKind::ActualFor)
        .collect();
    assert_eq!(
        actual_for_edges.len(),
        1,
        "expected 1 ActualFor edge (androidMain typealias -> commonMain expect class), got {:?}",
        actual_for_edges
    );

    let expect_node = all_nodes
        .iter()
        .find(|n| n.file_path.contains("commonMain") && n.name == "Platform")
        .expect("expect class node not found");
    assert_eq!(actual_for_edges[0].target, expect_node.id);

    let all_ids: Vec<String> = all_nodes.iter().map(|n| n.id.clone()).collect();
    let decls = cg.db().get_kmp_declarations_for(&all_ids).await.unwrap();
    assert_eq!(decls.iter().filter(|d| d.role == "expect").count(), 1);
    assert_eq!(decls.iter().filter(|d| d.role == "actual").count(), 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test kmp_pipeline_test actual_typealias`
Expected: FAIL before Tasks 1-2 land; should already PASS at this point since this task runs after them — run it anyway to confirm the full pipeline (extraction → resolution → population) works together, not just the unit-level pieces.

- [ ] **Step 3: (No implementation step — this test exercises Tasks 1+2 together)**

If it fails here despite Tasks 1-2 being green individually, that indicates an integration gap (e.g. a wiring issue between the extractor and resolver in the real `sync`/`index_all` pipeline) — stop and investigate rather than adding new code blindly.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --test kmp_pipeline_test`
Expected: PASS (existing `actual_for_edges_and_kmp_declarations_populated` test plus the new one).

- [ ] **Step 5: Full suite + lint + commit**

```bash
cargo test
cargo fmt && cargo clippy --all-targets
git add tests/kmp_pipeline_test.rs
git commit -m "test(kmp): end-to-end coverage for expect class + actual typealias"
```

---

## Self-Review Notes

- **Spec coverage:** extractor visitor (Task 1), resolver kind relaxation (Task 2), end-to-end proof (Task 3) — matches the spec's three change points exactly. "Out of scope" items (no edge to aliased type, `expect typealias`) have no corresponding task, intentionally.
- **Type consistency:** `visit_type_alias`, `kmp_kind_compatible(a: NodeKind, b: NodeKind) -> bool`, `TYPE_DECL_KINDS` used consistently; negative test (`Function` vs `TypeAlias`) guards against over-relaxing the rule.
- **No schema/DB changes** — confirmed, `NodeKind::TypeAlias` pre-exists; this plan touches only two files' logic (`kotlin_extractor.rs`, `resolver.rs`) plus tests.
