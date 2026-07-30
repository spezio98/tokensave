//! Queries for the `kmp_declarations` side table.
use super::*;

impl Database {
    /// Inserts or replaces KMP declaration rows. Idempotent per `node_id`.
    pub async fn insert_kmp_declarations(&self, decls: &[KmpDeclaration]) -> Result<()> {
        if decls.is_empty() {
            return Ok(());
        }

        self.conn()
            .execute("BEGIN", ())
            .await
            .map_err(|e| TokenSaveError::Database {
                message: format!("failed to begin: {e}"),
                operation: "insert_kmp_declarations".to_string(),
            })?;

        let stmt = self
            .conn()
            .prepare(
                "INSERT OR REPLACE INTO kmp_declarations
                 (node_id, source_set, module_root, role)
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .await
            .map_err(|e| TokenSaveError::Database {
                message: format!("failed to prepare: {e}"),
                operation: "insert_kmp_declarations".to_string(),
            })?;

        for d in decls {
            stmt.execute(params![
                d.node_id.as_str(),
                d.source_set.as_str(),
                d.module_root.as_str(),
                d.role.as_str(),
            ])
            .await
            .map_err(|e| TokenSaveError::Database {
                message: format!("failed to insert kmp declaration: {e}"),
                operation: "insert_kmp_declarations".to_string(),
            })?;
            stmt.reset();
        }

        self.conn()
            .execute("COMMIT", ())
            .await
            .map_err(|e| TokenSaveError::Database {
                message: format!("failed to commit: {e}"),
                operation: "insert_kmp_declarations".to_string(),
            })?;
        Ok(())
    }

    /// Returns KMP declaration rows for the given node IDs (missing IDs omitted).
    pub async fn get_kmp_declarations_for(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<KmpDeclaration>> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }

        let qmarks = build_qmark_placeholders(node_ids.len());
        let sql = format!(
            "SELECT node_id, source_set, module_root, role
             FROM kmp_declarations WHERE node_id IN ({qmarks})"
        );
        let params: Vec<libsql::Value> = node_ids
            .iter()
            .map(|id| libsql::Value::Text(id.clone()))
            .collect();

        let mut rows =
            self.conn()
                .query(&sql, params)
                .await
                .map_err(|e| TokenSaveError::Database {
                    message: format!("failed to query kmp declarations: {e}"),
                    operation: "get_kmp_declarations_for".to_string(),
                })?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| TokenSaveError::Database {
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
        Ok(out)
    }

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
        collect_rows(&mut rows, row_to_edge, "get_actual_for_edges_for").await
    }
}
