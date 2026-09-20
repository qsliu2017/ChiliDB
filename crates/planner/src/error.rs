use chilidb_binder::bound::ColumnBinding;
use datafusion_common::DataFusionError;

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("invalid bound statement: {0}")]
    InvalidBound(&'static str),
    #[error("column is unavailable in this evaluation phase: {0:?}")]
    UnknownColumn(ColumnBinding),
    #[error("expression nesting exceeds 256 nodes")]
    ExpressionTooDeep,
    #[error(transparent)]
    DataFusion(#[from] DataFusionError),
}
