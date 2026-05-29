use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeType {
    Calls,
    Imports,
    Extends,
    Implements,
    Includes,
    Uses,
    Accesses,
    MemberOf,
    Contains,
    CrossRepoLink,
    ProjectIncludesSymbol,
    ProjectIncludesNote,
    ProjectHasComponent,
    ProjectHasParent,
    /// A code handler (Symbol) implements an API [`crate::nodes::Contract`].
    /// Confidence records match quality: 1.0 for an exact verb+path match,
    /// 0.8 for a base-path-inferred match.
    ImplementsContract,
    /// F11 memory-bank typed relationships over the markdown vault (Note→Note).
    ///
    /// These promote a generic [`Self::Calls`]-style wikilink into an explicit,
    /// semantically-typed knowledge edge derived from frontmatter keys or
    /// heading-grouped wikilinks. Each maps to a well-known vocabulary term so
    /// the vault graph can interoperate with PROV-O / SKOS tooling:
    ///
    /// | EdgeType            | Vocabulary term                       |
    /// |---------------------|---------------------------------------|
    /// | [`Self::Supersedes`] | `prov:wasRevisionOf` (this revises the target) |
    /// | [`Self::DependsOn`]  | `prov:wasInformedBy` / `dependsOn`    |
    /// | [`Self::CausedBy`]   | `prov:wasDerivedFrom`                 |
    /// | [`Self::RelatesTo`]  | `skos:related`                        |
    ///
    /// `Supersedes` is directional A→B meaning "A supersedes (replaces) B".
    Supersedes,
    /// F11: `A DependsOn B` — A relies on B. Maps to `prov:wasInformedBy`.
    DependsOn,
    /// F11: `A CausedBy B` — A was derived from / caused by B. Maps to
    /// `prov:wasDerivedFrom`.
    CausedBy,
    /// F11: `A RelatesTo B` — a soft associative link. Maps to `skos:related`.
    RelatesTo,
}

impl EdgeType {
    /// Return the Cypher relationship table name used in the graph store.
    ///
    /// This may differ from the `Display` representation: `Extends`,
    /// `Implements`, and `Includes` use a `_SYM` suffix in the DB to
    /// disambiguate from note-level edge tables.
    pub fn rel_table_name(&self) -> &'static str {
        match self {
            EdgeType::Calls => "CALLS",
            EdgeType::Imports => "IMPORTS",
            EdgeType::Extends => "EXTENDS_SYM",
            EdgeType::Implements => "IMPLEMENTS_SYM",
            EdgeType::Includes => "INCLUDES_SYM",
            EdgeType::Uses => "USES",
            EdgeType::Accesses => "ACCESSES",
            EdgeType::MemberOf => "MEMBER_OF",
            EdgeType::Contains => "CONTAINS",
            EdgeType::CrossRepoLink => "CROSS_REPO_LINK",
            EdgeType::ProjectIncludesSymbol => "PROJECT_INCLUDES_SYMBOL",
            EdgeType::ProjectIncludesNote => "PROJECT_INCLUDES_NOTE",
            EdgeType::ProjectHasComponent => "PROJECT_HAS_COMPONENT",
            EdgeType::ProjectHasParent => "PROJECT_HAS_PARENT",
            EdgeType::ImplementsContract => "IMPLEMENTS_CONTRACT",
            EdgeType::Supersedes => "SUPERSEDES",
            EdgeType::DependsOn => "DEPENDS_ON",
            EdgeType::CausedBy => "CAUSED_BY",
            EdgeType::RelatesTo => "RELATES_TO",
        }
    }
}

impl fmt::Display for EdgeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EdgeType::Calls => write!(f, "CALLS"),
            EdgeType::Imports => write!(f, "IMPORTS"),
            EdgeType::Extends => write!(f, "EXTENDS"),
            EdgeType::Implements => write!(f, "IMPLEMENTS"),
            EdgeType::Includes => write!(f, "INCLUDES"),
            EdgeType::Uses => write!(f, "USES"),
            EdgeType::Accesses => write!(f, "ACCESSES"),
            EdgeType::MemberOf => write!(f, "MEMBER_OF"),
            EdgeType::Contains => write!(f, "CONTAINS"),
            EdgeType::CrossRepoLink => write!(f, "CROSS_REPO_LINK"),
            EdgeType::ProjectIncludesSymbol => write!(f, "PROJECT_INCLUDES_SYMBOL"),
            EdgeType::ProjectIncludesNote => write!(f, "PROJECT_INCLUDES_NOTE"),
            EdgeType::ProjectHasComponent => write!(f, "PROJECT_HAS_COMPONENT"),
            EdgeType::ProjectHasParent => write!(f, "PROJECT_HAS_PARENT"),
            EdgeType::ImplementsContract => write!(f, "IMPLEMENTS_CONTRACT"),
            EdgeType::Supersedes => write!(f, "SUPERSEDES"),
            EdgeType::DependsOn => write!(f, "DEPENDS_ON"),
            EdgeType::CausedBy => write!(f, "CAUSED_BY"),
            EdgeType::RelatesTo => write!(f, "RELATES_TO"),
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
