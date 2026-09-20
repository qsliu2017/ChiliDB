use std::{
    cmp::Ordering,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

use arrow_schema::{DataType, Field, Schema};
use chilidb_binder::bound;
use datafusion_common::{
    Column, DFSchema, DFSchemaRef, DataFusionError, Result, tree_node::TreeNodeRecursion,
};
use datafusion_expr::{
    Expr, LogicalPlan, UserDefinedLogicalNodeCore,
    logical_plan::{Extension, InvariantLevel},
};

use crate::table_source::{arrow_type, ctid_type};

/// The retained Arc keeps allocation identity valid for this handle's lifetime.
#[derive(Clone, Debug)]
pub struct Target {
    table: bound::Table,
}

impl Target {
    pub fn new(table: bound::Table) -> Self {
        Self { table }
    }
    pub fn table(&self) -> &bound::Table {
        &self.table
    }
    fn identity(&self) -> (u32, usize) {
        (
            self.table.relation.0,
            Arc::as_ptr(&self.table.source) as *const () as usize,
        )
    }
}
impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}
impl Eq for Target {}
impl Hash for Target {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}
impl PartialOrd for Target {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.identity().partial_cmp(&other.identity())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd)]
pub struct UpdateAssignment {
    pub target_column_index: usize,
    pub value: Column,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd)]
pub enum Modification {
    Insert {
        values: Vec<Column>,
    },
    Update {
        ctid: Column,
        assignments: Vec<UpdateAssignment>,
    },
    Delete {
        ctid: Column,
    },
}

#[derive(Clone, Debug)]
pub struct ModifyTable {
    target: Target,
    input: LogicalPlan,
    operation: Modification,
    schema: DFSchemaRef,
}

fn invalid(message: impl Into<String>) -> DataFusionError {
    DataFusionError::Plan(message.into())
}
fn completion_schema() -> Result<DFSchemaRef> {
    Ok(Arc::new(DFSchema::try_from(Schema::new(vec![
        Field::new("affected_rows", DataType::Int64, false),
    ]))?))
}

pub fn is_modify_table(plan: &LogicalPlan) -> bool {
    matches!(plan, LogicalPlan::Extension(extension) if extension.node.as_any().is::<ModifyTable>())
}

pub(crate) fn ensure_relational_input(input: &LogicalPlan) -> Result<()> {
    input.apply_with_subqueries(|plan| {
        if is_modify_table(plan)
            || matches!(
                plan,
                LogicalPlan::Dml(_)
                    | LogicalPlan::Ddl(_)
                    | LogicalPlan::Statement(_)
                    | LogicalPlan::Copy(_)
            )
        {
            return Err(invalid(
                "relational input contains a modification or command",
            ));
        }
        Ok(TreeNodeRecursion::Continue)
    })?;
    Ok(())
}

fn validate_value(
    target: &Target,
    input: &LogicalPlan,
    index: usize,
    value: &Column,
) -> Result<()> {
    let schema = target.table.source.schema();
    let target_column = schema
        .columns
        .get(index)
        .ok_or_else(|| invalid("modification target column index is out of range"))?;
    let field = input.schema().field(input.schema().index_of_column(value)?);
    if field.data_type() != &arrow_type(&target_column.data_type) {
        return Err(invalid(format!(
            "modification value {value} has incompatible type"
        )));
    }
    // Nullability and VARCHAR bounds are execution-time target constraints.
    Ok(())
}
fn validate_ctid(input: &LogicalPlan, ctid: &Column) -> Result<()> {
    let field = input.schema().field(input.schema().index_of_column(ctid)?);
    if field.data_type() != &ctid_type() || field.is_nullable() {
        return Err(invalid(format!(
            "modification ctid must be non-nullable {:?}",
            ctid_type()
        )));
    }
    Ok(())
}
fn rebuild_parts(
    exprs: Vec<Expr>,
    mut inputs: Vec<LogicalPlan>,
    count: usize,
) -> Result<(LogicalPlan, Vec<Column>)> {
    if inputs.len() != 1 || exprs.len() != count {
        return Err(invalid("invalid ModifyTable input or expression arity"));
    }
    let columns = exprs
        .into_iter()
        .map(|expr| match expr {
            Expr::Column(column) => Ok(column),
            _ => Err(invalid("modification expressions must be columns")),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((inputs.remove(0), columns))
}

impl ModifyTable {
    pub fn try_insert(target: Target, input: LogicalPlan, values: Vec<Column>) -> Result<Self> {
        Self::try_new(target, input, Modification::Insert { values })
    }
    pub fn try_update(
        target: Target,
        input: LogicalPlan,
        ctid: Column,
        assignments: Vec<UpdateAssignment>,
    ) -> Result<Self> {
        Self::try_new(target, input, Modification::Update { ctid, assignments })
    }
    pub fn try_delete(target: Target, input: LogicalPlan, ctid: Column) -> Result<Self> {
        Self::try_new(target, input, Modification::Delete { ctid })
    }
    fn try_new(target: Target, input: LogicalPlan, operation: Modification) -> Result<Self> {
        let node = Self {
            target,
            input,
            operation,
            schema: completion_schema()?,
        };
        node.validate()?;
        Ok(node)
    }
    pub fn target(&self) -> &Target {
        &self.target
    }
    pub fn input(&self) -> &LogicalPlan {
        &self.input
    }
    pub fn operation(&self) -> &Modification {
        &self.operation
    }
    pub fn into_plan(self) -> LogicalPlan {
        LogicalPlan::Extension(Extension {
            node: Arc::new(self),
        })
    }
    fn columns(&self) -> Vec<Column> {
        match &self.operation {
            Modification::Insert { values } => values.clone(),
            Modification::Update { ctid, assignments } => std::iter::once(ctid.clone())
                .chain(assignments.iter().map(|a| a.value.clone()))
                .collect(),
            Modification::Delete { ctid } => vec![ctid.clone()],
        }
    }
    fn validate(&self) -> Result<()> {
        ensure_relational_input(&self.input)?;
        match &self.operation {
            Modification::Insert { values } => {
                if values.len() != self.target.table.source.schema().columns.len() {
                    return Err(invalid("insert must map every target column"));
                }
                for (index, value) in values.iter().enumerate() {
                    validate_value(&self.target, &self.input, index, value)?;
                }
            }
            Modification::Update { ctid, assignments } => {
                validate_ctid(&self.input, ctid)?;
                if assignments.is_empty() {
                    return Err(invalid("update requires at least one assignment"));
                }
                let mut seen = HashSet::new();
                for assignment in assignments {
                    if !seen.insert(assignment.target_column_index) {
                        return Err(invalid("duplicate update target column"));
                    }
                    validate_value(
                        &self.target,
                        &self.input,
                        assignment.target_column_index,
                        &assignment.value,
                    )?;
                }
            }
            Modification::Delete { ctid } => validate_ctid(&self.input, ctid)?,
        }
        Ok(())
    }
    fn rebuild(&self, exprs: Vec<Expr>, inputs: Vec<LogicalPlan>) -> Result<Self> {
        match &self.operation {
            Modification::Insert { values } => {
                let (input, values) = rebuild_parts(exprs, inputs, values.len())?;
                Self::try_insert(self.target.clone(), input, values)
            }
            Modification::Update { assignments, .. } => {
                let (input, columns) = rebuild_parts(exprs, inputs, assignments.len() + 1)?;
                let ctid = columns[0].clone();
                let assignments = assignments
                    .iter()
                    .zip(columns.into_iter().skip(1))
                    .map(|(old, value)| UpdateAssignment {
                        target_column_index: old.target_column_index,
                        value,
                    })
                    .collect();
                Self::try_update(self.target.clone(), input, ctid, assignments)
            }
            Modification::Delete { .. } => {
                let (input, mut columns) = rebuild_parts(exprs, inputs, 1)?;
                Self::try_delete(self.target.clone(), input, columns.remove(0))
            }
        }
    }
}

// The completion schema is fixed, not part of independently variable node identity.
impl PartialEq for ModifyTable {
    fn eq(&self, other: &Self) -> bool {
        (&self.target, &self.input, &self.operation)
            == (&other.target, &other.input, &other.operation)
    }
}
impl Eq for ModifyTable {}
impl Hash for ModifyTable {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (&self.target, &self.input, &self.operation).hash(state);
    }
}
impl PartialOrd for ModifyTable {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        (&self.target, &self.input, &self.operation).partial_cmp(&(
            &other.target,
            &other.input,
            &other.operation,
        ))
    }
}
impl UserDefinedLogicalNodeCore for ModifyTable {
    fn name(&self) -> &str {
        "ModifyTable"
    }
    fn inputs(&self) -> Vec<&LogicalPlan> {
        vec![&self.input]
    }
    fn schema(&self) -> &DFSchemaRef {
        &self.schema
    }
    fn expressions(&self) -> Vec<Expr> {
        self.columns().into_iter().map(Expr::Column).collect()
    }
    fn check_invariants(&self, _check: InvariantLevel) -> Result<()> {
        self.validate()?;
        if self.schema != completion_schema()? {
            return Err(invalid("invalid ModifyTable completion schema"));
        }
        Ok(())
    }
    fn fmt_for_explain(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let operation = match &self.operation {
            Modification::Insert { .. } => "Insert",
            Modification::Update { .. } => "Update",
            Modification::Delete { .. } => "Delete",
        };
        write!(
            f,
            "ModifyTable({operation}): target={}, relation={}, columns={:?}",
            self.target.table.source.schema().name,
            self.target.table.relation.0,
            self.columns()
        )
    }
    fn with_exprs_and_inputs(&self, exprs: Vec<Expr>, inputs: Vec<LogicalPlan>) -> Result<Self> {
        self.rebuild(exprs, inputs)
    }
    fn necessary_children_exprs(&self, _output_columns: &[usize]) -> Option<Vec<Vec<usize>>> {
        let mut indices = self
            .columns()
            .iter()
            .map(|column| self.input.schema().index_of_column(column))
            .collect::<Result<Vec<_>>>()
            .ok()?;
        indices.sort_unstable();
        indices.dedup();
        Some(vec![indices])
    }
    fn supports_limit_pushdown(&self) -> bool {
        false
    }
}
