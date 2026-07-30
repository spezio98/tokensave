# KMP Support — Validation & Dogfooding Guide

Companion to `2026-07-30-kmp-support.md` (implementation plan) and
`2026-07-30-kmp-support-design.md` (spec). Run this **after all Phase 0–2 tasks
are implemented and committed**, to confirm the feature works on real Kotlin
Multiplatform code — not just synthetic fixtures.

Four levels, cheapest first. Levels 1 + 2 are mandatory; level 3 (dogfooding) is
the real proof the feature helps the AI; level 4 covers the edge cases the
heuristic can break on.

---

## Level 1 — Automated suite (fast, isolated)

```bash
cd /Users/andrea.speziale/OtherProjects/tokensave

cargo test                                        # full suite, default features
cargo test --no-default-features --features lite  # Kotlin is Lite-tier — must pass here too
cargo fmt --check
cargo clippy -- -D warnings
```

Green means the logic is correct in isolation: `kmp_location_from_path`,
migration V15, `ActualFor` emission, resolver strategy, post-resolution
population, context completion pass, formatter labels.

**Not sufficient** — fixtures use flat `commonMain`/`androidMain`/`iosMain`
only. Continue to level 2.

---

## Level 2 — Real KMP repo, data checks (SQL)

Index a real multiplatform app (the ones with SwiftUI + Jetpack Compose UIs are
ideal — heavy `expect`/`actual`).

```bash
# Build the dev binary from this branch
cargo build --release
BIN=/Users/andrea.speziale/OtherProjects/tokensave/target/release/tokensave

# Index a real KMP project
cd /path/to/your-kmp-app
"$BIN" sync .
```

Then inspect the DB it produced (the sync output prints the DB path; typically
under the project's `.tokensave/` dir). Open it with `sqlite3`:

**a) `ActualFor` edges exist**
```sql
SELECT source, target FROM edges WHERE kind = 'actual_for';
-- expect: one row per actual, each pointing at its commonMain expect
```

**b) `kmp_declarations` populated with correct roles/source-sets**
```sql
SELECT role, source_set, COUNT(*) FROM kmp_declarations GROUP BY role, source_set;
-- expect: 1 'expect' per family (commonMain), N 'actual' across platform source sets
```

**c) Fan-in is correct** — pick a known `expect fun`/`expect class` with 2+
platform implementations, count incoming edges:
```sql
SELECT COUNT(*) FROM edges e
JOIN kmp_declarations d ON d.node_id = e.target
WHERE e.kind = 'actual_for' AND d.role = 'expect'
  AND d.node_id = '<expect-node-id>';
-- expect: == number of platforms implementing it
```

**d) No cross-module false matches** — if two separate Gradle modules define a
same-named/same-package declaration, confirm no `actual_for` edge links across
them (join `kmp_declarations` on both ends, compare `module_root`).

---

## Level 3 — Dogfooding: install as a dev MCP and use Claude normally

This is the real test: does the feature actually help the AI? Register the dev
build **under a distinct name** so the globally-installed `tokensave` is never
touched and rollback is trivial.

**1. Save the current Claude MCP config (for reference)**
```bash
claude mcp list
claude mcp get tokensave     # note it, in case you want to compare
```

**2. Register the dev build as a separate server**
```bash
claude mcp add tokensave-dev -- \
  /Users/andrea.speziale/OtherProjects/tokensave/target/release/tokensave serve
```
Distinct name `tokensave-dev` ⇒ coexists with the original, no clobber.

**3. Index the real KMP app** (if not already done in level 2)
```bash
cd /path/to/your-kmp-app
/Users/andrea.speziale/OtherProjects/tokensave/target/release/tokensave sync .
```

**4. Open that repo in Claude and use it normally.** Concrete probes:
- "Show me the implementation of `<an expect function>` on every platform" →
  context must surface commonMain + all platform actuals, not just one file.
- "How is `<X>` implemented on iOS vs Android?" → both actuals present, each
  tagged `[actual · iosMain]` / `[actual · androidMain]`, plus the
  `[expect · commonMain]`.
- Start a task editing one `actual` → Claude should receive the `expect` and the
  sibling actuals automatically, without hunting for them.

**Pass signal:** Claude sees the whole platform family (where before it saw one
variant), and the labels make it obvious which platform each block is.

**5. Roll back when done**
```bash
claude mcp remove tokensave-dev
```
The original install was never modified.

> Optional — replace the real install instead of running side-by-side:
> `cargo install --path .` overwrites the on-PATH `tokensave` with this branch's
> build. Reversible with `cargo install tokensave` (reinstalls the published
> release) or `brew reinstall tokensave`. Prefer the side-by-side `tokensave-dev`
> route above unless you specifically want the default agent to use it.

---

## Level 4 — Edge cases the heuristic can break on

The automated fixtures don't cover these; test them explicitly on real code (or
add fixtures). These are the highest-risk spots in the path-heuristic design:

- **Hierarchical / intermediate source sets** (`appleMain` shared by
  `iosMain` + `macosMain`, `nativeMain`, `jvmAndroidMain`). An `expect` may live
  in `appleMain`, not `commonMain`. The resolver's "prefer `Common`
  source-set" rule will NOT find it there. **Risk #1** if your apps use
  hierarchical source sets — verify these link, and if they don't, the resolver
  strategy needs a "nearest common ancestor source set" rule rather than
  strict `commonMain`.
- **`actual typealias`** — an `actual` can be a typealias, not a function/class.
  Confirm the resolver's `kind == source.kind` filter doesn't wrongly reject it
  (a `typealias` actual pointing at a platform type differs in kind from the
  `expect class`).
- **Test source sets** — `commonTest`/`androidTest` expect/actual pairs.
- **`expect`/`actual` properties and objects**, not just functions — the
  extractor emits `ActualFor` at class/object/property sites too; confirm real
  ones link.
- **Files with tree-sitter parse errors** — a malformed `.kt` must not crash
  indexing; the file is skipped, others still index.
- **Regression on non-KMP repos** — re-index a single-platform Android app
  (`src/main/…`) or a Rust project: `kmp_declarations` and `actual_for` must be
  **empty**, and context output unchanged. Confirms the path heuristic has no
  false positives.

---

## Priority

Level 1 + 2 gate correctness. Level 3 proves value to the AI (the actual point of
the feature). Level 4's hierarchical-source-set case (`appleMain`) is where the
current design is most likely to fall short — test it first among the edge cases
if your real apps use hierarchical KMP source sets.
