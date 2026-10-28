// nestweaver-resolver: cross-file symbol resolution and reference linking

pub mod cross_repo;
pub mod imports;
pub mod lang;
mod php;
pub mod resolve;
pub mod type_extractors;
pub mod types;
pub mod util;
pub mod workspace;

pub use cross_repo::*;
pub use resolve::*;
pub use workspace::{
    GoModule, TsconfigAlias, WorkspaceContext, WorkspacePackage, discover_workspace_context,
    discover_workspace_context_with, discover_workspace_context_with_files,
};

#[cfg(test)]
mod import_resolution_tests;
