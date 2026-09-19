//! Bounded, persisted relationships among an explicitly selected node set.
use std::collections::HashSet;

use lbug::Value;
use serde::Serialize;

use crate::read::{VAULT_CODE_BRIDGE_RELATIONS, VAULT_RELATIONS, extract_string};
use crate::{GraphStore, StoreError};

pub const CONTEXT_NODE_LIMIT: usize = 500;
pub const CONTEXT_EDGE_LIMIT: usize = 2_000;

#[derive(Debug, Clone, Serialize)]
pub struct ContextEdge {
    pub source: String,
    pub target: String,
    pub edge_type: String,
    pub confidence: Option<f64>,
    pub evidence: Option<String>,
}

pub struct ContextEdges {
    pub edges: Vec<ContextEdge>,
    pub total: usize,
    pub total_exact: bool,
}

struct Relation {
    name: &'static str,
    from: &'static str,
    to: &'static str,
    scored: bool,
    evidence: bool,
}

fn relations() -> Vec<Relation> {
    let mut result: Vec<_> = nestweaver_schema::ALL_SYMBOL_EDGE_TYPES
        .iter()
        .map(|kind| Relation {
            name: kind.rel_table_name(),
            from: "Symbol",
            to: "Symbol",
            scored: true,
            evidence: true,
        })
        .collect();
    result.extend(
        VAULT_RELATIONS
            .iter()
            .chain(VAULT_CODE_BRIDGE_RELATIONS)
            .map(|relation| Relation {
                name: relation.rel,
                from: relation.from,
                to: relation.to,
                scored: relation.scored,
                evidence: false,
            }),
    );
    for (name, from, to, scored, evidence) in [
        ("CROSS_REPO_LINK", "Symbol", "Symbol", true, true),
        ("REPO_HAS_FILE", "Repo", "File", false, false),
        ("FILE_HAS_SYMBOL", "File", "Symbol", false, false),
        ("SERVICE_HAS_SYMBOL", "Service", "Symbol", false, false),
        ("PROJECT_INCLUDES_NOTE", "Project", "Note", true, false),
        ("PROJECT_INCLUDES_SYMBOL", "Project", "Symbol", true, false),
        ("PROJECT_HAS_COMPONENT", "Project", "Project", true, false),
        ("PROJECT_HAS_PARENT", "Project", "Project", true, false),
        ("IMPLEMENTS_CONTRACT", "Symbol", "Contract", true, true),
    ] {
        result.push(Relation {
            name,
            from,
            to,
            scored,
            evidence,
        });
    }
    result.sort_by_key(|relation| relation.name);
    result
}

impl GraphStore {
    /// Both endpoints are parameterized and bounded. No whole-graph edge load
    /// and no neighbor request per node. Counts describe returned-node scope;
    /// when a per-relation cap is reached, `total` is a disclosed lower bound.
    pub fn context_edges(&self, selected: &[String]) -> Result<ContextEdges, StoreError> {
        let selected: HashSet<_> = selected.iter().cloned().collect();
        if selected.len() > CONTEXT_NODE_LIMIT {
            return Err(StoreError::Query(
                "context graph node limit exceeded".into(),
            ));
        }
        if selected.is_empty() {
            return Ok(ContextEdges {
                edges: Vec::new(),
                total: 0,
                total_exact: true,
            });
        }
        let mut uids: Vec<_> = selected.iter().cloned().collect();
        uids.sort();
        let values = Value::List(
            lbug::LogicalType::String,
            uids.into_iter().map(Value::String).collect(),
        );
        let conn = self.conn()?;
        let mut edges = Vec::new();
        let mut total_exact = true;
        for relation in relations() {
            Self::check_read_deadline()?;
            let confidence = if relation.scored {
                "r.confidence"
            } else {
                "NULL"
            };
            let evidence = if relation.evidence {
                "r.evidence"
            } else {
                "NULL"
            };
            // Names come exclusively from the fixed schema inventory above.
            let query = format!(
                "MATCH (a:{})-[r:{}]->(b:{}) WHERE a.uid IN $uids AND b.uid IN $uids \
                 RETURN DISTINCT a.uid AS source, b.uid AS target, {confidence} AS confidence, {evidence} AS evidence \
                 ORDER BY source, target, confidence, evidence LIMIT {}",
                relation.from,
                relation.name,
                relation.to,
                CONTEXT_EDGE_LIMIT + 1,
            );
            let mut statement = conn.prepare(&query).map_err(|error| {
                StoreError::Query(format!("context edges {} prepare: {error}", relation.name))
            })?;
            let rows = conn
                .execute(&mut statement, vec![("uids", values.clone())])
                .map_err(|error| {
                    StoreError::Query(format!("context edges {}: {error}", relation.name))
                })?;
            let mut count = 0;
            for row in rows {
                Self::check_read_deadline()?;
                let source = extract_string(&row, 0)?;
                let target = extract_string(&row, 1)?;
                if !selected.contains(&source) || !selected.contains(&target) {
                    return Err(StoreError::Query(
                        "context edge escaped selected scope".into(),
                    ));
                }
                let confidence = match row.get(2) {
                    Some(Value::Float(value)) => Some(f64::from(*value)),
                    Some(Value::Double(value)) => Some(*value),
                    Some(Value::Null(_)) => None,
                    _ => return Err(StoreError::Query("invalid context edge confidence".into())),
                };
                if confidence.is_some_and(|value| !value.is_finite()) {
                    return Err(StoreError::Query(
                        "non-finite context edge confidence".into(),
                    ));
                }
                let evidence = match row.get(3) {
                    Some(Value::Null(_)) => None,
                    _ => Some(extract_string(&row, 3)?),
                };
                edges.push(ContextEdge {
                    source,
                    target,
                    edge_type: relation.name.into(),
                    confidence,
                    evidence,
                });
                count += 1;
            }
            if count > CONTEXT_EDGE_LIMIT {
                total_exact = false;
            }
        }
        // Stable cross-type ordering keeps graph/table/matrix identical.
        edges.sort_by(|a, b| {
            a.edge_type
                .cmp(&b.edge_type)
                .then_with(|| a.source.cmp(&b.source))
                .then_with(|| a.target.cmp(&b.target))
                .then_with(|| {
                    a.confidence
                        .unwrap_or(-1.0)
                        .total_cmp(&b.confidence.unwrap_or(-1.0))
                })
                .then_with(|| a.evidence.cmp(&b.evidence))
        });
        let total = edges.len();
        edges.truncate(CONTEXT_EDGE_LIMIT);
        Ok(ContextEdges {
            edges,
            total,
            total_exact,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn inventory_covers_persisted_typed_relationships() {
        let names: std::collections::HashSet<_> = super::relations()
            .iter()
            .map(|relation| relation.name)
            .collect();
        for kind in nestweaver_schema::ALL_EDGE_TYPES {
            // CONTAINS is the artifact vocabulary for physical containment
            // tables; the store has no CONTAINS relation table.
            if *kind != nestweaver_schema::EdgeType::Contains {
                assert!(names.contains(kind.rel_table_name()), "{kind:?}");
            }
        }
        for name in ["REPO_HAS_FILE", "FILE_HAS_SYMBOL", "SERVICE_HAS_SYMBOL"] {
            assert!(names.contains(name));
        }
    }
}
