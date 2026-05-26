use thiserror::Error;

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(String),
    #[error("query error: {0}")]
    Query(String),
    #[error("not found")]
    NotFound,
}

impl StoreError {
    pub fn is_duplicate(&self) -> bool {
        match self {
            StoreError::Database(msg) | StoreError::Query(msg) => {
                let lower = msg.to_lowercase();
                lower.contains("already exist")
                    || lower.contains("duplicate")
                    || lower.contains("unique")
                    || lower.contains("constraint")
            }
            StoreError::NotFound => false,
        }
    }
}

impl From<lbug::Error> for StoreError {
    fn from(e: lbug::Error) -> Self {
        StoreError::Database(e.to_string())
    }
}
