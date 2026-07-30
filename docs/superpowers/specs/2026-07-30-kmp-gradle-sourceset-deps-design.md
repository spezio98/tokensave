# Gradle KMP Source-Set Dependency Grouping — Design Spec

## Goal

`tokensave_dependencies`' Gradle parser (`src/mcp/tools/handlers/dependencies/gradle.rs`) walks the whole build-file AST recursively looking for `implementation(...)`/`api(...)`/etc. calls, regardless of nesting. A probe confirmed this **already extracts** dependencies declared inside KMP's per-source-set blocks:

```kotlin
kotlin {
    sourceSets {
        commonMain.dependencies { implementation("io.ktor:ktor-client-core:2.3.0") }
        androidMain.dependencies { implementation("com.google.android.material:material:1.9.0") }
        iosMain.dependencies { implementation("io.ktor:ktor-client-darwin:2.3.0") }
    }
}
```

— all three deps come back correctly (name, version, kind). **The real gap is narrower than originally assumed**: everything lands in one flat `Member.deps` list, so there's no way to tell that `material` is Android-only and `ktor-client-darwin` is iOS-only. This spec adds that grouping.

## Design constraint

`Dep`/`Member` (`common.rs`) are shared structs, constructed via explicit struct literal at 34 call sites across 19 ecosystem parsers (npm, cargo, pip, go, etc.). Adding a field to either would force updating all 34 sites for a Gradle/KMP-only concern — the same blast-radius problem the core KMP linking work already solved once (there, by keeping data in a side table instead of touching `Node`). Here the equivalent move is: **don't touch `Dep`/`Member` at all** — represent per-source-set grouping as additional **virtual `Member`s**, the same pattern `parse_version_catalog` (`gradle.rs:341`) already uses to expose the version catalog as a pseudo-module.

## Design

### AST recognition

In the existing recursive walk (`walk_for_dep_calls`, `gradle.rs:147`), a KMP source-set scope is a `call_expression` whose callee is a `navigation_expression` shaped `<identifier>.dependencies` (confirmed via AST probe: `(call_expression (navigation_expression (simple_identifier) (navigation_suffix (simple_identifier "dependencies"))) (call_suffix (annotated_lambda ...)))`). No name whitelist needed — the identifier itself *is* the source-set name (`commonMain`, `androidMain`, `iosMain`, `jvmMain`, ...), matching whatever the project actually declares.

Only the modern type-safe accessor syntax (`commonMain.dependencies { }`) is recognized. The legacy `getByName("commonMain") { }` form is out of scope (rare in current KMP projects, adds parsing complexity for a shrinking pattern).

### Extraction change

`walk_for_dep_calls` gains a `current_source_set: Option<&str>` parameter, threaded through recursive calls. When recursing into a node whose callee matches `<name>.dependencies`, set `current_source_set = Some(name)` for that subtree only. Every extracted `Dep` is tagged with whichever `current_source_set` was active at its call site — collected into `BTreeMap<Option<String>, Vec<Dep>>` (`None` = the flat top-level `dependencies { }` block, unaffected by this change).

### Member construction

`parse_build_file` (`gradle.rs:98`) changes from returning `Option<Member>` (one member) to building one `Member` per map key:

- `None` key → the existing single `Member` exactly as today (`path: rel`, `name: <module or "root">`) — **zero behavior change for non-KMP modules**, since they only ever populate the `None` bucket.
- Each `Some(source_set)` key → a virtual `Member` with `path: format!("{rel}::{source_set}")`, `name: format!("{module_name}::{source_set}")`, `license: None`, `deps` = that bucket's deps. Colon-separated (not space/parens) so `tokensave_dependencies --member "shared::androidMain"` is easy to type and matches `render_member`'s exact-match lookup (`m.name == name || m.path == name`, `mod.rs:298`) cleanly.

`parse` (`gradle.rs:54`) already does `if let Some(m) = parse_build_file(...) { members.push(m) }` for a single member; changes to `if let Some(ms) = parse_build_file(...) { members.extend(ms) }` for a `Vec<Member>`.

### No other changes

- `common.rs` (`Dep`/`Member`/`Workspace`): untouched.
- Every other ecosystem parser (npm, cargo, pip, go, ...): untouched, unaffected — this is Gradle-only logic.
- `mcp/tools/handlers/dependencies/mod.rs` (summary/crate/member rendering): untouched — virtual per-source-set members flow through the exact same `Vec<Member>` rendering path as any other member, no special-casing needed.

## Testing

- Unit test in `gradle.rs`'s existing `#[cfg(test)] mod tests`: the KMP fixture above (3 source sets) produces 4 members total for the root module (`.` with 0 deps if no flat block, or the flat-block deps; `.::commonMain` with `ktor-client-core`; `.::androidMain` with `material`; `.::iosMain` with `ktor-client-darwin`).
- Regression test: the existing `parses_build_gradle_kts_via_kotlin_grammar` test (flat `dependencies { }`, no `sourceSets`) must still produce exactly one `Member` with all three deps in the `None` bucket — confirms non-KMP Gradle projects are completely unaffected.
- Groovy DSL (`.gradle`, not `.gradle.kts`): out of scope for this spec — KMP's `sourceSets { X.dependencies { } }` type-safe accessor syntax is Kotlin-DSL-only (Groovy KMP builds use `getByName("X").dependencies { }`, the legacy form already excluded above), so no Groovy-side change is needed or tested.

## Out of scope

- Legacy `getByName("sourceSetName") { }` accessor syntax.
- Attaching source-set metadata to non-Gradle ecosystems.
- Any change to `tokensave_dependencies`' output schema/rendering — virtual members are indistinguishable from real ones to consumers, by design.
