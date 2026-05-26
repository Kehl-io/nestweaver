use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeType {
    Calls,
    Imports,
    Extends,
    Implements,
    Includes,
    MemberOf,
    Contains,
    CrossRepoLink,
}

impl fmt::Display for EdgeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EdgeType::Calls => write!(f, "CALLS"),
            EdgeType::Imports => write!(f, "IMPORTS"),
            EdgeType::Extends => write!(f, "EXTENDS"),
            EdgeType::Implements => write!(f, "IMPLEMENTS"),
            EdgeType::Includes => write!(f, "INCLUDES"),
            EdgeType::MemberOf => write!(f, "MEMBER_OF"),
            EdgeType::Contains => write!(f, "CONTAINS"),
            EdgeType::CrossRepoLink => write!(f, "CROSS_REPO_LINK"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CrossRepoLinkType {
    SharedImport,
    ContractMatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedEdge {
    pub source_uid: String,
    pub target_uid: String,
    pub edge_type: EdgeType,
    pub confidence: f32,
    pub link_type: Option<CrossRepoLinkType>,
}
