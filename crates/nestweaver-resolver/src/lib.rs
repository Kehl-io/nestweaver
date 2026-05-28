// nestweaver-resolver: cross-file symbol resolution and reference linking

pub mod cross_repo;
pub mod imports;
pub mod lang;
pub mod resolve;
pub mod types;
pub mod util;
pub mod workspace;

pub use cross_repo::*;
pub use resolve::*;
pub use workspace::{
    TsconfigAlias, WorkspaceContext, WorkspacePackage, discover_workspace_context,
};
