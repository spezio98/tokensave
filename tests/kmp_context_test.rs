//! End-to-end test (#KMP Task 2.1): context built around one KMP `actual`
//! declaration always includes its `expect` and every sibling `actual`, even
//! under a traversal budget too tight for plain BFS to reach them all.

use std::fs;
use tempfile::TempDir;
use tokensave::context::ContextBuilder;
use tokensave::tokensave::TokenSave;
use tokensave::types::BuildContextOptions;

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
async fn context_completes_kmp_family_under_tight_budget() {
    let (cg, dir) = setup_kmp_module().await;
    let builder = ContextBuilder::new(cg.db(), dir.path());

    // Restrict entry points to the androidMain file only, and starve the BFS
    // (max_nodes=1, traversal_depth=1) so plain traversal could reach at most
    // the expect (1 hop) -- never the iOS sibling, which sits 2 hops away
    // (android -> expect -> ios). complete_kmp_families must add it anyway.
    let options = BuildContextOptions {
        max_nodes: 1,
        traversal_depth: 1,
        path_include: vec!["androidMain".to_string()],
        ..Default::default()
    };

    let ctx = builder
        .build_context("platformName", &options)
        .await
        .unwrap();

    let files: Vec<&str> = ctx
        .subgraph
        .nodes
        .iter()
        .map(|n| n.file_path.as_str())
        .collect();
    assert!(
        files.iter().any(|f| f.contains("commonMain")),
        "expect missing from subgraph: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.contains("androidMain")),
        "android actual missing from subgraph: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.contains("iosMain")),
        "ios sibling missing from subgraph under tight budget: {files:?}"
    );

    // Code blocks must also cover the completed family, not just entry points.
    let code_files: Vec<&str> = ctx
        .code_blocks
        .iter()
        .map(|b| b.file_path.as_str())
        .collect();
    assert!(
        code_files.iter().any(|f| f.contains("iosMain")),
        "ios sibling's code missing from code_blocks: {code_files:?}"
    );
}
