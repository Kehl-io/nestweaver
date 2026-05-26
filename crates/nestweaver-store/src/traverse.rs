use std::collections::{HashSet, VecDeque};

use crate::db::GraphStore;
use crate::error::StoreError;

/// A node returned by the impact analysis traversal.
#[derive(Debug, Clone)]
pub struct ImpactNode {
    pub uid: String,
    pub name: String,
    pub file_path: String,
    pub start_line: u32,
    pub edge_type: String,
    pub confidence: f32,
    pub depth: u32,
}

/// A row representing caller + edge metadata returned from the BFS query.
struct CallerRow {
    uid: String,
    name: String,
    file_path: String,
    start_line: u32,
    edge_type: String,
    confidence: f32,
}

impl GraphStore {
    /// Find all symbols that directly or transitively call/import/extend/implement `target_uid`.
    ///
    /// Performs iterative BFS up to `max_depth` levels following incoming edges of type
    /// CALLS, IMPORTS, EXTENDS_SYM, and IMPLEMENTS_SYM. Results with confidence below
    /// `min_confidence` are excluded.
    pub fn impact(
        &self,
        target_uid: &str,
        max_depth: u32,
        min_confidence: f32,
    ) -> Result<Vec<ImpactNode>, StoreError> {
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(target_uid.to_string());

        let mut queue: VecDeque<(String, u32)> = VecDeque::new();
        queue.push_back((target_uid.to_string(), 0));

        let mut results: Vec<ImpactNode> = Vec::new();

        while let Some((current_uid, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }

            let callers = self.direct_callers_of(&current_uid, min_confidence)?;

            for row in callers {
                if visited.contains(&row.uid) {
                    continue;
                }
                visited.insert(row.uid.clone());

                let node = ImpactNode {
                    uid: row.uid.clone(),
                    name: row.name,
                    file_path: row.file_path,
                    start_line: row.start_line,
                    edge_type: row.edge_type,
                    confidence: row.confidence,
                    depth: depth + 1,
                };
                results.push(node);
                queue.push_back((row.uid, depth + 1));
            }
        }

        Ok(results)
    }

    /// Internal: fetch all direct callers of `uid` across CALLS/IMPORTS/EXTENDS_SYM/IMPLEMENTS_SYM.
    fn direct_callers_of(
        &self,
        uid: &str,
        min_confidence: f32,
    ) -> Result<Vec<CallerRow>, StoreError> {
        let conn = self.conn()?;
        let min_conf = min_confidence as f64;

        let edge_types = [
            "CALLS",
            "IMPORTS",
            "EXTENDS_SYM",
            "IMPLEMENTS_SYM",
            "INCLUDES_SYM",
        ];
        let mut rows: Vec<CallerRow> = Vec::new();

        for edge_type in &edge_types {
            let q = format!(
                "MATCH (s:Symbol)-[r:{et}]->(t:Symbol {{uid: $uid}}) \
                 WHERE r.confidence >= $min_conf \
                 RETURN s.uid, s.name, s.file_path, s.start_line, r.confidence",
                et = edge_type,
            );

            let mut stmt = match conn.prepare(&q) {
                Ok(s) => s,
                Err(e) => {
                    tracing::trace!(
                        "impact: edge type {edge_type} skipped (table may not exist): {e}"
                    );
                    continue;
                }
            };
            let result = match conn.execute(
                &mut stmt,
                vec![
                    ("uid", lbug::Value::String(uid.to_string())),
                    ("min_conf", lbug::Value::Double(min_conf)),
                ],
            ) {
                Ok(r) => r,
                Err(e) => {
                    tracing::trace!("impact: edge type {edge_type} query failed: {e}");
                    continue;
                }
            };

            for row in result {
                use lbug::Value;
                let caller_uid = match &row[0] {
                    Value::String(s) => s.clone(),
                    _ => continue,
                };
                let name = match &row[1] {
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                let file_path = match &row[2] {
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                };
                let start_line = match &row[3] {
                    Value::Int64(n) => u32::try_from(*n).unwrap_or(0),
                    Value::Int32(n) => u32::try_from(*n).unwrap_or(0),
                    _ => 0,
                };
                let confidence = match &row[4] {
                    Value::Float(f) => *f,
                    Value::Double(f) => *f as f32,
                    _ => 0.0,
                };

                rows.push(CallerRow {
                    uid: caller_uid,
                    name,
                    file_path,
                    start_line,
                    edge_type: edge_type.to_string(),
                    confidence,
                });
            }
        }

        Ok(rows)
    }

    /// Search symbols whose name contains `query` (case-insensitive substring match).
    /// Returns up to `limit` results.
    pub fn search_symbols_by_name(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<nestweaver_schema::Symbol>, StoreError> {
        let needle = query.to_lowercase();
        let conn = self.conn()?;
        // LadybugDB's CONTAINS is case-sensitive and has no toLower().
        // Load all symbols and filter in Rust for case-insensitive matching.
        let q = format!("MATCH (s:Symbol) RETURN {}", crate::read::SYMBOL_COLUMNS,);
        let result = conn
            .query(&q)
            .map_err(|e| StoreError::Query(format!("query: {e}")))?;
        let mut matches = Vec::new();
        for row in result {
            if let Ok(sym) = crate::read::row_to_symbol(&row)
                && sym.name.to_lowercase().contains(&needle)
            {
                matches.push(sym);
                if matches.len() >= limit {
                    break;
                }
            }
        }
        Ok(matches)
    }
}
