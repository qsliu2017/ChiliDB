//! Statement boundaries around DataFusion relational plans.

use chilidb_binder::bound;
use datafusion_expr::{LogicalPlan, logical_plan::InvariantLevel};

use crate::{Modification, ModifyTable, OutputSchema, PlanError, modify_table, table_source};

#[derive(Clone, Debug)]
pub enum PlannedStatement<'sql> {
    Query {
        plan: LogicalPlan,
        /// SQL-visible names and types, in root output order. Names may repeat.
        output: OutputSchema<'sql>,
    },
    /// The root must be one ModifyTable extension, with no nested ModifyTable nodes.
    ModifyTable {
        plan: LogicalPlan,
    },
    Command(Command<'sql>),
}

impl<'sql> PlannedStatement<'sql> {
    /// Attach an optimized relational plan without losing statement metadata.
    ///
    /// Only queries and INSERT are enabled. UPDATE/DELETE require a separate
    /// ctid-preservation policy before generic optimization can be enabled.
    /// These checks protect structural contracts, not the correctness of a rule.
    pub fn with_optimized_plan(self, optimized: LogicalPlan) -> Result<Self, PlanError> {
        optimized.check_invariants(InvariantLevel::Executable)?;
        match self {
            Self::Query { plan, output } => {
                modify_table::ensure_relational_input(&optimized)?;
                if plan.schema().columns() != optimized.schema().columns()
                    || output.fields.len() != optimized.schema().fields().len()
                {
                    return Err(PlanError::InvalidBound(
                        "optimizer changed query output columns",
                    ));
                }
                for (expected, actual) in output.fields.iter().zip(optimized.schema().fields()) {
                    if table_source::arrow_type(&expected.data_type) != *actual.data_type()
                        || (!expected.nullable && actual.is_nullable())
                    {
                        return Err(PlanError::InvalidBound(
                            "optimizer changed query output types or nullability",
                        ));
                    }
                }
                Ok(Self::Query {
                    plan: optimized,
                    output,
                })
            }
            Self::ModifyTable { plan } => {
                let old = insert_root(&plan).ok_or(PlanError::InvalidBound(
                    "only INSERT modification optimization is enabled",
                ))?;
                let new = insert_root(&optimized).ok_or(PlanError::InvalidBound(
                    "optimizer removed or replaced the INSERT root",
                ))?;
                if old.target() != new.target() || plan.schema() != optimized.schema() {
                    return Err(PlanError::InvalidBound(
                        "optimizer changed the INSERT target or completion schema",
                    ));
                }
                Ok(Self::ModifyTable { plan: optimized })
            }
            Self::Command(_) => Err(PlanError::InvalidBound(
                "commands are not relational optimization inputs",
            )),
        }
    }
}

fn insert_root(plan: &LogicalPlan) -> Option<&ModifyTable> {
    let LogicalPlan::Extension(extension) = plan else {
        return None;
    };
    let node = extension.node.as_any().downcast_ref::<ModifyTable>()?;
    matches!(node.operation(), Modification::Insert { .. }).then_some(node)
}

#[derive(Clone, Debug)]
pub enum Command<'sql> {
    CreateTable(bound::CreateTable<'sql>),
    Begin,
    Commit,
    Rollback,
}
