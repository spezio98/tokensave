# Gradle KMP Source-Set Dependency Grouping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Group Gradle dependencies already extracted from KMP `sourceSets { X.dependencies { } }` blocks by source-set, using virtual `Member`s (the same pattern `parse_version_catalog` already uses) instead of touching the shared `Dep`/`Member` structs.

**Architecture:** `walk_for_dep_calls` threads an `Option<&str>` "current source-set scope" parameter through its recursion, set whenever it descends into a `<name>.dependencies { }` call. Deps are collected into a `BTreeMap<Option<String>, Vec<Dep>>` (`None` = today's flat top-level bucket) instead of a flat `Vec<Dep>`. `parse_build_file` builds one `Member` per map key instead of one `Member` total.

**Tech Stack:** Rust, tree-sitter (`tree-sitter-kotlin-sg` v0.4.1, `dekobon-tree-sitter-groovy`).

## Global Constraints

- Run `cargo test`, `cargo clippy --all-targets`, `cargo fmt` before every commit.
- Zero changes to `common.rs` (`Dep`/`Member`/`Workspace`) — confirmed 34 construction sites across 19 ecosystem parsers depend on their current shape.
- Only the Kotlin-DSL (`.gradle.kts`) type-safe accessor syntax (`commonMain.dependencies { }`) is in scope. Groovy (`.gradle`) and the legacy `getByName("x") { }` accessor are out of scope.

---

### Task 1: Confirm exact AST shape (spike)

**Files:**
- Test: temporary probe in `src/mcp/tools/handlers/dependencies/gradle.rs`'s existing `#[cfg(test)] mod tests` (kept only if useful as a regression anchor, otherwise removed once Task 2 lands)

An earlier probe already confirmed the outer shape via `to_sexp()`:

```
(call_expression (navigation_expression (simple_identifier) (navigation_suffix (simple_identifier))) (call_suffix (annotated_lambda (lambda_literal (statements ...)))))
```

This task confirms the exact **text** of the `navigation_suffix` node (does `.utf8_text()` on it return `"dependencies"` or `".dependencies"` including the dot?) and of its child (should be a clean `simple_identifier` node whose text is `"dependencies"`) — needed to write `detect_sourceset_dependencies_block` correctly in Task 2 without guessing.

- [ ] **Step 1: Write the probe**

Add temporarily to the existing test module in `gradle.rs`:

```rust
#[test]
fn probe_navigation_suffix_text() {
    let mut parser = tree_sitter::Parser::new();
    let lang = ts_provider::language("kotlin");
    parser.set_language(&lang).unwrap();
    let src = "commonMain.dependencies { implementation(\"x:y:1.0\") }";
    let tree = parser.parse(src, None).unwrap();
    let call = tree.root_node().child(0).unwrap(); // call_expression
    let nav = call.child(0).unwrap(); // navigation_expression
    let source_set_ident = nav.child(0).unwrap();
    let suffix = nav.child(1).unwrap(); // navigation_suffix
    println!("source_set_ident kind={} text={:?}", source_set_ident.kind(), source_set_ident.utf8_text(src.as_bytes()));
    println!("suffix kind={} text={:?}", suffix.kind(), suffix.utf8_text(src.as_bytes()));
    println!("suffix child count={}", suffix.child_count());
    if let Some(inner) = suffix.child(0) {
        println!("suffix.child(0) kind={} text={:?}", inner.kind(), inner.utf8_text(src.as_bytes()));
    }
}
```

- [ ] **Step 2: Run and read the output**

Run: `cargo test --lib probe_navigation_suffix_text -- --nocapture`

Record the exact `kind()`/`utf8_text()` values printed — this determines whether Task 2's `detect_sourceset_dependencies_block` reads `suffix.utf8_text()` directly (stripping a leading `.` if present) or `suffix.child(0).utf8_text()` (the clean identifier).

- [ ] **Step 3: Remove the probe**

Delete the `probe_navigation_suffix_text` test — it was only to settle the exact AST shape, not a regression to keep. Commit nothing yet; Task 2 will add the real implementation and its own tests.

---

### Task 2: `walk_for_dep_calls` scope threading + `detect_sourceset_dependencies_block`

**Files:**
- Modify: `src/mcp/tools/handlers/dependencies/gradle.rs` (`walk_for_dep_calls`, `try_extract_call`, plus a new `detect_sourceset_dependencies_block` function)
- Test: same file's `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `Node<'_>` (tree-sitter), the exact AST shape confirmed in Task 1.
- Produces:
  - `fn detect_sourceset_dependencies_block(node: Node<'_>, src: &[u8]) -> Option<String>` — returns the source-set name (e.g. `"commonMain"`) when `node` is a `<name>.dependencies { }` call, else `None`.
  - `walk_for_dep_calls` new signature: `fn walk_for_dep_calls(node: Node<'_>, src: &[u8], current_scope: Option<&str>, out: &mut std::collections::BTreeMap<Option<String>, Vec<Dep>>)`.
  - `try_extract_call` new signature: `fn try_extract_call(node: Node<'_>, src: &[u8], scope: Option<&str>, out: &mut std::collections::BTreeMap<Option<String>, Vec<Dep>>)` — pushes into `out.entry(scope.map(str::to_string)).or_default()` instead of a flat `Vec`.

- [ ] **Step 1: Write the failing test**

Add to `gradle.rs`'s test module:

```rust
#[test]
fn groups_kmp_sourceset_deps_by_source_set() {
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        "build.gradle.kts",
        r#"kotlin {
    androidTarget()
    iosX64()
    sourceSets {
        commonMain.dependencies {
            implementation("io.ktor:ktor-client-core:2.3.0")
        }
        androidMain.dependencies {
            implementation("com.google.android.material:material:1.9.0")
        }
        iosMain.dependencies {
            implementation("io.ktor:ktor-client-darwin:2.3.0")
        }
    }
}
"#,
    );
    let ws = parse(dir.path()).unwrap();

    let common = ws.members.iter().find(|m| m.path == ".::commonMain")
        .expect("commonMain virtual member missing");
    assert_eq!(common.deps.len(), 1);
    assert_eq!(common.deps[0].name, "io.ktor:ktor-client-core");

    let android = ws.members.iter().find(|m| m.path == ".::androidMain")
        .expect("androidMain virtual member missing");
    assert_eq!(android.deps.len(), 1);
    assert_eq!(android.deps[0].name, "com.google.android.material:material");

    let ios = ws.members.iter().find(|m| m.path == ".::iosMain")
        .expect("iosMain virtual member missing");
    assert_eq!(ios.deps.len(), 1);
    assert_eq!(ios.deps[0].name, "io.ktor:ktor-client-darwin");

    assert_eq!(android.name, ".::androidMain");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib groups_kmp_sourceset_deps_by_source_set`
Expected: FAIL — `parse_build_file` still returns one flat `Member`, so `ws.members` has no `.::androidMain` path (compile error too, until Step 3, since this test also doesn't compile against the old single-`Vec<Dep>` shape yet — that's fine, it's the red step).

- [ ] **Step 3: Implement `detect_sourceset_dependencies_block`**

Add near `walk_for_dep_calls` (`gradle.rs:147`), using whichever exact accessor Task 1 confirmed (this example assumes `suffix.child(0)` is the clean identifier — adjust to match Task 1's findings if different):

```rust
/// Detects a KMP type-safe source-set dependency block: `<name>.dependencies { ... }`.
/// Returns the source-set name (e.g. `"commonMain"`) if `node` matches, else `None`.
/// No whitelist of names — the identifier itself names the source-set, covering
/// any target (`commonMain`, `androidMain`, `iosMain`, `jvmMain`, custom names, ...).
fn detect_sourceset_dependencies_block(node: Node<'_>, src: &[u8]) -> Option<String> {
    if node.kind() != "call_expression" {
        return None;
    }
    let nav = node.child(0)?;
    if nav.kind() != "navigation_expression" {
        return None;
    }
    let source_set_node = nav.child(0)?;
    if !source_set_node.kind().contains("identifier") {
        return None;
    }
    let suffix = nav.child(1)?;
    let suffix_name_node = suffix.child(0).unwrap_or(suffix);
    let suffix_text = suffix_name_node.utf8_text(src).ok()?;
    if suffix_text.trim_start_matches('.') != "dependencies" {
        return None;
    }
    source_set_node.utf8_text(src).ok().map(str::to_string)
}
```

- [ ] **Step 4: Thread scope through `walk_for_dep_calls` and `try_extract_call`**

Replace `walk_for_dep_calls` (`gradle.rs:147-159`):

```rust
fn walk_for_dep_calls(
    node: Node<'_>,
    src: &[u8],
    current_scope: Option<&str>,
    out: &mut std::collections::BTreeMap<Option<String>, Vec<Dep>>,
) {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            try_extract_call(child, src, current_scope, out);
            let child_scope = detect_sourceset_dependencies_block(child, src);
            match &child_scope {
                Some(name) => walk_for_dep_calls(child, src, Some(name), out),
                None => walk_for_dep_calls(child, src, current_scope, out),
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}
```

Replace `try_extract_call` (`gradle.rs:161-178`) to take `scope` and push into the map:

```rust
fn try_extract_call(
    node: Node<'_>,
    src: &[u8],
    scope: Option<&str>,
    out: &mut std::collections::BTreeMap<Option<String>, Vec<Dep>>,
) {
    let Some(callee) = leading_identifier(node, src) else {
        return;
    };
    let Some((_, kind)) = CONFIGS.iter().find(|(name, _)| *name == callee) else {
        return;
    };
    let Some(spec) = first_string_literal(node, src) else {
        return;
    };
    if let Some(dep) = parse_coordinate(&spec, *kind) {
        out.entry(scope.map(str::to_string)).or_default().push(dep);
    }
}
```

Update `extract_deps_from_source` (`gradle.rs:130-142`) to build and return the map instead of a `Vec<Dep>`:

```rust
fn extract_deps_from_source(
    source: &str,
    language_key: &str,
) -> Option<std::collections::BTreeMap<Option<String>, Vec<Dep>>> {
    let language = if language_key == "groovy" {
        dekobon_tree_sitter_groovy::LANGUAGE.into()
    } else {
        ts_provider::language(language_key)
    };
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let mut deps = std::collections::BTreeMap::new();
    walk_for_dep_calls(tree.root_node(), source.as_bytes(), None, &mut deps);
    Some(deps)
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib gradle`
Expected: compile errors surface remaining call sites that still assume the old `Vec<Dep>` shape (`parse_build_file` — fixed in Task 3). Re-run after Task 3.

---

### Task 3: `parse_build_file` builds `Vec<Member>`; `parse` extends instead of pushing

**Files:**
- Modify: `src/mcp/tools/handlers/dependencies/gradle.rs` (`parse_build_file`, `parse`)
- Test: same file's `#[cfg(test)] mod tests` (the `groups_kmp_sourceset_deps_by_source_set` test from Task 2, plus a regression test)

**Interfaces:**
- Consumes: `extract_deps_from_source`'s new `BTreeMap<Option<String>, Vec<Dep>>` return (Task 2).
- Produces: `fn parse_build_file(_root: &Path, module_dir: &Path, rel: &str) -> Option<Vec<Member>>` (was `Option<Member>`).

- [ ] **Step 1: Write the regression test**

Add to `gradle.rs`'s test module (guards that non-KMP Gradle projects are unaffected):

```rust
#[test]
fn flat_dependencies_block_still_yields_one_member() {
    let dir = TempDir::new().unwrap();
    write(
        dir.path(),
        "build.gradle.kts",
        r#"dependencies {
    implementation("org.jetbrains.kotlin:kotlin-stdlib:2.0.0")
}
"#,
    );
    let ws = parse(dir.path()).unwrap();
    let root_members: Vec<_> = ws.members.iter().filter(|m| m.path == ".").collect();
    assert_eq!(root_members.len(), 1, "flat (non-KMP) build file must yield exactly one member");
    assert_eq!(root_members[0].deps.len(), 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib gradle`
Expected: FAIL to compile — `parse_build_file` still returns `Option<Member>`, `parse` still does `if let Some(m) = ... { members.push(m) }`.

- [ ] **Step 3: Rewrite `parse_build_file`**

Replace `gradle.rs:98-128`:

```rust
fn parse_build_file(_root: &Path, module_dir: &Path, rel: &str) -> Option<Vec<Member>> {
    let kts = module_dir.join("build.gradle.kts");
    let groovy = module_dir.join("build.gradle");
    let (path, language_key) = if kts.exists() {
        (kts, "kotlin")
    } else if groovy.exists() {
        (groovy, "groovy")
    } else {
        return None;
    };

    let raw = std::fs::read_to_string(&path).ok()?;
    let deps_by_scope = extract_deps_from_source(&raw, language_key)?;

    let module_name = if rel
        .trim_start_matches('.')
        .trim_start_matches('/')
        .is_empty()
    {
        module_dir
            .file_name()
            .map_or_else(|| "root".to_string(), |s| s.to_string_lossy().into_owned())
    } else {
        rel.to_string()
    };

    let members = deps_by_scope
        .into_iter()
        .map(|(scope, deps)| match scope {
            None => Member {
                path: rel.to_string(),
                name: module_name.clone(),
                license: None,
                deps,
            },
            Some(source_set) => Member {
                path: format!("{rel}::{source_set}"),
                name: format!("{module_name}::{source_set}"),
                license: None,
                deps,
            },
        })
        .collect();
    Some(members)
}
```

Note: if `deps_by_scope` is empty (a build file with no recognized dependency calls at all), this now returns `Some(vec![])` instead of the old behavior of still returning one empty `Member` — check Step 5's full-suite run for any existing test that assumed an empty-but-present root member and adjust if so.

- [ ] **Step 4: Update `parse`'s call sites**

In `parse` (`gradle.rs:54-96`), replace both call sites:

```rust
    if let Some(ms) = parse_build_file(root, root, ".") {
        members.extend(ms);
    }
```

and, inside the `for module in included` loop:

```rust
        if let Some(ms) = parse_build_file(root, &module_dir, &fs_rel) {
            members.extend(ms);
        }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib gradle`
Expected: PASS — `groups_kmp_sourceset_deps_by_source_set`, `flat_dependencies_block_still_yields_one_member`, and every pre-existing Gradle test (`parses_build_gradle_kts_via_kotlin_grammar`, `parses_build_gradle_via_groovy_grammar`, etc.) all green.

- [ ] **Step 6: Full suite + lint + commit**

```bash
cargo test
cargo fmt && cargo clippy --all-targets
git add src/mcp/tools/handlers/dependencies/gradle.rs
git commit -m "feat(kmp): group Gradle KMP dependencies by source set into virtual members"
```

---

## Self-Review Notes

- **Spec coverage:** AST recognition (Task 1 spike + Task 2), extraction/grouping (Task 2), Member construction (Task 3) — matches the spec's design sections exactly. "Out of scope" items (Groovy DSL, `getByName`, schema/rendering changes) have no task, intentionally.
- **Type consistency:** `detect_sourceset_dependencies_block(node: Node<'_>, src: &[u8]) -> Option<String>`, `walk_for_dep_calls(..., current_scope: Option<&str>, out: &mut BTreeMap<Option<String>, Vec<Dep>>)`, `try_extract_call(..., scope: Option<&str>, ...)`, `parse_build_file(...) -> Option<Vec<Member>>` used consistently across tasks.
- **No changes to `common.rs`** — confirmed; this plan touches only `gradle.rs` and its own tests.
- **Placeholder scan:** Task 1's exact suffix-text handling in Task 2's code sample is explicitly flagged as "adjust to match Task 1's findings if different" rather than asserted as fact — this is a deliberate flag for the implementer to verify against real probe output, not a placeholder to fill in blindly.
