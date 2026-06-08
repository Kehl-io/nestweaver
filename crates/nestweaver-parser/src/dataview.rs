//! Minimal parser for Obsidian Dataview DQL queries.
//!
//! Extracts `FROM`, `WHERE`, and query type from Dataview code blocks to
//! capture implicit note relationships. Only handles DQL syntax (not inline
//! JavaScript queries).

use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone)]
pub struct DataviewQuery {
    pub query_type: String,
    pub from_source: Option<String>,
    pub where_clauses: Vec<String>,
}

static RE_FROM_QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bFROM\s+"([^"]+)""#).unwrap());

static RE_FROM_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bFROM\s+(#[\w/.\-]+)").unwrap());

static RE_WHERE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bWHERE\s+(.+?)(?:\n|$)").unwrap());

static RE_QUERY_TYPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(TABLE|LIST|TASK|CALENDAR)\b").unwrap());

pub fn parse_dataview_query(source: &str) -> Option<DataviewQuery> {
    let first_line = source.lines().next()?.trim();
    let query_type = RE_QUERY_TYPE
        .captures(first_line)?
        .get(1)?
        .as_str()
        .to_uppercase();

    let from_source = RE_FROM_QUOTED
        .captures(source)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .or_else(|| {
            RE_FROM_TAG
                .captures(source)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
        });

    let where_clauses: Vec<String> = RE_WHERE
        .captures_iter(source)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .collect();

    Some(DataviewQuery {
        query_type,
        from_source,
        where_clauses,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_table_with_from_and_where() {
        let dql = "TABLE status, priority\nFROM \"Projects\"\nWHERE status != \"done\"\nSORT priority DESC";
        let query = parse_dataview_query(dql).unwrap();
        assert_eq!(query.query_type, "TABLE");
        assert_eq!(query.from_source.as_deref(), Some("Projects"));
        assert_eq!(query.where_clauses.len(), 1);
        assert!(query.where_clauses[0].contains("status"));
    }

    #[test]
    fn parses_list_query() {
        let dql = "LIST\nFROM \"Workspaces/Projects\"\nSORT file.mtime DESC";
        let query = parse_dataview_query(dql).unwrap();
        assert_eq!(query.query_type, "LIST");
        assert_eq!(query.from_source.as_deref(), Some("Workspaces/Projects"));
    }

    #[test]
    fn parses_task_query() {
        let dql = "TASK\nFROM \"Projects\"\nWHERE !completed";
        let query = parse_dataview_query(dql).unwrap();
        assert_eq!(query.query_type, "TASK");
        assert_eq!(query.from_source.as_deref(), Some("Projects"));
    }

    #[test]
    fn parses_tag_source() {
        let dql = "LIST\nFROM #project/active";
        let query = parse_dataview_query(dql).unwrap();
        assert_eq!(query.from_source.as_deref(), Some("#project/active"));
    }

    #[test]
    fn parses_calendar_query() {
        let dql = "CALENDAR date\nFROM \"_logs\"";
        let query = parse_dataview_query(dql).unwrap();
        assert_eq!(query.query_type, "CALENDAR");
        assert_eq!(query.from_source.as_deref(), Some("_logs"));
    }

    #[test]
    fn no_from_returns_none_source() {
        let dql = "TABLE file.name, file.mtime\nWHERE file.mtime > date(today) - dur(7 days)";
        let query = parse_dataview_query(dql).unwrap();
        assert_eq!(query.query_type, "TABLE");
        assert!(query.from_source.is_none());
    }

    #[test]
    fn multiple_where_clauses() {
        let dql = "TABLE status\nFROM \"Projects\"\nWHERE status = \"active\"\nWHERE priority > 3";
        let query = parse_dataview_query(dql).unwrap();
        assert_eq!(query.where_clauses.len(), 2);
    }

    #[test]
    fn non_dql_returns_none() {
        let result = parse_dataview_query("just some text");
        assert!(result.is_none());
    }
}
