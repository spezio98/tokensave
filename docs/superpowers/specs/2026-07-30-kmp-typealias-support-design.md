# KMP `actual typealias` Support — Design Spec

## Goal

The merged KMP support (`docs/superpowers/specs/2026-07-30-kmp-support-design.md`) links `expect`/`actual` declarations for functions, classes, objects, and properties — but not `typealias`. `KotlinExtractor` has no visitor for `typealias` declarations at all (a pre-existing gap, unrelated to KMP), discovered during that work's Task 1.1 grammar spike. This means the extremely common iOS KMP pattern `expect class Platform` + `actual typealias Platform = AndroidPlatform` never links, even though the underlying `expect`/`actual` linking machinery (Phase 1) is otherwise in place.

This spec adds:
1. A Kotlin extractor visitor for `typealias` declarations (`NodeKind::TypeAlias`), following the existing minimal pattern already used by `SwiftExtractor`/`TypeScriptExtractor`/`RustExtractor` (name + full-text signature, no edge to the aliased type).
2. A relaxation of the KMP resolver's kind-matching rule to accept `actual typealias` as a valid counterpart for `expect class`/`interface`/`object` (an `actual typealias` never satisfies `expect fun`/`expect val` — those always require a real `actual fun`/`actual val`).

## Grammar confirmation

Verified via a throwaway AST probe (`tree.root_node().to_sexp()`) against `tree-sitter-kotlin-sg`:

```
actual typealias Platform = AndroidPlatform
```

parses as:

```
(type_alias (modifiers (platform_modifier)) (type_identifier) (user_type (type_identifier)))
```

Two things confirmed:
- The node kind is **`type_alias`** (not `typealias_declaration` as in Swift's grammar).
- The `actual`/`expect` modifier appears as a `platform_modifier` child inside `modifiers` — the same structural shape already used by `function_declaration`/`class_declaration`/etc. `has_modifier_keyword` (`kotlin_extractor.rs:1283`) already matches on modifier text generically, confirmed already working for `data`/`sealed`/`actual` on other declaration kinds — no change needed there.

## Extractor changes (`kotlin_extractor.rs`)

Add `"type_alias" => Self::visit_type_alias(state, node),` to `visit_node`'s dispatch table.

New `visit_type_alias`, modeled directly on `SwiftExtractor::visit_typealias` (`swift_extractor.rs:961`):

```rust
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
        start_line, attrs_start_line: start_line, end_line, start_column, end_column,
        signature: Some(state.node_text(node).trim().to_string()),
        docstring: None,
        visibility,
        is_async: false,
        branches: 0, loops: 0, returns: 0, max_nesting: 0, unsafe_blocks: 0,
        unchecked_calls: 0, assertions: 0, cognitive_complexity: 0,
        distinct_operators: 0, distinct_operands: 0, total_operators: 0, total_operands: 0,
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

No edge to the aliased type (`user_type`) — matches the existing minimal convention (Swift's `visit_typealias` doesn't emit one either).

`maybe_emit_actual_for_ref` (already generic, added in the merged KMP work) needs no change — it reads `state.nodes.last()` regardless of `NodeKind`, so it works for `TypeAlias` nodes exactly as it already does for `Function`/`Class`/`Object`/`Property`.

## Resolver change (`resolver.rs`) — kind compatibility relaxation

`try_kmp_actual_match`'s current filter requires exact `NodeKind` equality between an `actual` and its candidate `expect`. This must be relaxed **only** for the type-declaration ↔ typealias case — a `typealias` never satisfies an `expect fun`/`expect val`, only an `expect class`/`interface`/`object`.

Replace the `.filter(|n| n.kind == source.kind)` line with a new helper:

```rust
/// True if `a`/`b` are the same `NodeKind`, or one is `TypeAlias` and the
/// other is a Kotlin type-declaration kind. An `actual typealias` can
/// satisfy `expect class`/`interface`/`object` (a real Kotlin/language
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
    let (alias, other) = if a == NodeKind::TypeAlias {
        (a, b)
    } else if b == NodeKind::TypeAlias {
        (b, a)
    } else {
        return false;
    };
    alias == NodeKind::TypeAlias && TYPE_DECL_KINDS.contains(&other)
}
```

Call site becomes `.filter(|n| kmp_kind_compatible(n.kind, source.kind))`.

## No other changes

- `types.rs`: `NodeKind::TypeAlias` already exists (used by 8 other language extractors) — no schema change.
- `src/extraction/kmp.rs`, `context/builder.rs`, `context/formatter.rs`: untouched — `TypeAlias` is just another `NodeKind`, already handled generically everywhere (path detection, family completion, labeling).

## Testing

- Extraction test: `actual typealias Platform = AndroidPlatform` produces one `NodeKind::TypeAlias` node named `Platform`, and emits an `ActualFor` unresolved ref (mirrors the existing `actual_fun_emits_actual_for_ref` test).
- Extraction test: plain `typealias Foo = Bar` (no modifier) produces a `TypeAlias` node but no `ActualFor` ref.
- Resolver test: `expect class Platform` (commonMain) + `actual typealias Platform = AndroidPlatform` (androidMain) resolve to the same node via `ActualFor`, despite differing `NodeKind`.
- Resolver test: `kmp_kind_compatible` does NOT allow `Function` ↔ `TypeAlias` (an actual typealias must never satisfy an expect function) — negative case.
- End-to-end pipeline test: extend the existing 3-file `commonMain`/`androidMain`/`iosMain` KMP fixture pattern with `expect class Platform` + `actual typealias Platform = ...` (one or both platforms), assert `kmp_declarations` role/source_set populated the same way as the function case.

## Out of scope

- Recording an edge from the `typealias` node to its aliased type (`user_type`) — no other language extractor in this codebase does this for typealiases either; stays consistent with existing convention.
- `expect typealias` (the less common reverse direction) — not requested; the merged Phase 1 already handles `expect`-side discovery generically via `ActualFor` edge target, so this would fall out for free if it ever came up, but isn't tested here.
