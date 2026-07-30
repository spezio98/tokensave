use tempfile::TempDir;
use tokensave::db::migrations::latest_version;
use tokensave::db::Database;
use tokensave::types::KmpDeclaration;

async fn setup_db() -> (Database, TempDir) {
    let dir = TempDir::new().unwrap();
    let (db, _) = Database::initialize(&dir.path().join("test.db"))
        .await
        .unwrap();
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
        KmpDeclaration {
            node_id: "a".into(),
            source_set: "androidMain".into(),
            module_root: "shared".into(),
            role: "actual".into(),
        },
        KmpDeclaration {
            node_id: "e".into(),
            source_set: "commonMain".into(),
            module_root: "shared".into(),
            role: "expect".into(),
        },
    ];
    db.insert_kmp_declarations(&decls).await.unwrap();

    let got = db
        .get_kmp_declarations_for(&["a".to_string(), "e".to_string(), "missing".to_string()])
        .await
        .unwrap();
    assert_eq!(got.len(), 2);
    assert!(got
        .iter()
        .any(|d| d.node_id == "a" && d.role == "actual" && d.source_set == "androidMain"));
    assert!(got.iter().any(|d| d.node_id == "e" && d.role == "expect"));
}

#[tokio::test]
async fn insert_is_idempotent() {
    let (db, _dir) = setup_db().await;
    let d = KmpDeclaration {
        node_id: "a".into(),
        source_set: "androidMain".into(),
        module_root: "shared".into(),
        role: "actual".into(),
    };
    db.insert_kmp_declarations(&[d.clone()]).await.unwrap();
    db.insert_kmp_declarations(&[d]).await.unwrap();
    let got = db
        .get_kmp_declarations_for(&["a".to_string()])
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
}
