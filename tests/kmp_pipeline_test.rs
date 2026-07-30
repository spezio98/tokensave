//! End-to-end test (#KMP Task 1.5): indexing a real KMP-shaped module wires
//! ActualFor edges and populates `kmp_declarations` from them.

use std::fs;
use tempfile::TempDir;
use tokensave::tokensave::TokenSave;
use tokensave::types::EdgeKind;

async fn setup_kmp_module() -> (TokenSave, TempDir) {
    let dir = TempDir::new().unwrap();
    let project = dir.path();

    fs::create_dir_all(project.join("shared/src/commonMain/kotlin")).unwrap();
    fs::create_dir_all(project.join("shared/src/androidMain/kotlin")).unwrap();
    fs::create_dir_all(project.join("shared/src/iosMain/kotlin")).unwrap();

    fs::write(
        project.join("shared/src/commonMain/kotlin/P.kt"),
        "package com.x\n\nexpect fun platformName(): String\n",
    )
    .unwrap();
    fs::write(
        project.join("shared/src/androidMain/kotlin/P.kt"),
        "package com.x\n\nactual fun platformName(): String = \"android\"\n",
    )
    .unwrap();
    fs::write(
        project.join("shared/src/iosMain/kotlin/P.kt"),
        "package com.x\n\nactual fun platformName(): String = \"ios\"\n",
    )
    .unwrap();

    let cg = TokenSave::init(project).await.unwrap();
    cg.index_all().await.unwrap();
    (cg, dir)
}

#[tokio::test]
async fn actual_for_edges_and_kmp_declarations_populated() {
    let (cg, _dir) = setup_kmp_module().await;

    let all_nodes = cg.db().get_all_nodes().await.unwrap();
    let all_edges = cg.db().get_all_edges().await.unwrap();

    let actual_for_edges: Vec<_> = all_edges
        .iter()
        .filter(|e| e.kind == EdgeKind::ActualFor)
        .collect();
    assert_eq!(
        actual_for_edges.len(),
        2,
        "expected 2 ActualFor edges (android->expect, ios->expect), got {:?}",
        actual_for_edges
    );

    let expect_node = all_nodes
        .iter()
        .find(|n| n.file_path.contains("commonMain") && n.name == "platformName")
        .expect("expect node not found");
    for e in &actual_for_edges {
        assert_eq!(
            e.target, expect_node.id,
            "ActualFor edge should target the commonMain expect node"
        );
    }

    let all_ids: Vec<String> = all_nodes.iter().map(|n| n.id.clone()).collect();
    let decls = cg.db().get_kmp_declarations_for(&all_ids).await.unwrap();

    assert_eq!(
        decls.iter().filter(|d| d.role == "expect").count(),
        1,
        "expected 1 expect declaration, got {decls:?}"
    );
    assert_eq!(
        decls.iter().filter(|d| d.role == "actual").count(),
        2,
        "expected 2 actual declarations, got {decls:?}"
    );
    let expect_decl = decls.iter().find(|d| d.role == "expect").unwrap();
    assert_eq!(expect_decl.source_set, "commonMain");
    assert_eq!(expect_decl.module_root, "shared");
}
