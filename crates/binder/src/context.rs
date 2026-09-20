//! Query-local namespaces, independent of the catalog implementation.

use std::borrow::Cow;

use crate::{BindError, bound};

/// Scope state for one query, separate from the catalog-backed [`crate::Binder`].
///
/// Scopes are ordered outermost to innermost; the last entry is active. Relation
/// IDs identify occurrences across the query, not positions in this stack.
#[derive(Clone, Debug)]
pub struct BindContext<'sql> {
    /// The active scope stack; empty before a query scope is established.
    pub scopes: Vec<Scope<'sql>>,
}

impl<'sql> BindContext<'sql> {
    /// Resolve a normalized column name in the nearest matching scope.
    ///
    /// Qualified names stop at a scope containing that relation name, even if
    /// the column is missing. Unqualified names search outward only when no
    /// column in the current scope matches.
    ///
    /// # Errors
    /// Returns an unknown-column or ambiguous-column error when resolution does
    /// not identify exactly one visible column.
    pub fn resolve_column<E>(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<bound::Expr<'sql>, BindError<E>> {
        let unknown = || BindError::UnknownColumn {
            qualifier: qualifier.map(str::to_owned),
            name: name.to_owned(),
        };
        let ambiguous = || BindError::AmbiguousColumn {
            qualifier: qualifier.map(str::to_owned),
            name: name.to_owned(),
        };
        for scope in self.scopes.iter().rev() {
            let mut found = None;
            let mut qualified_relations = 0;
            for relation in &scope.relations {
                if let Some(qualifier) = qualifier {
                    if relation.name != qualifier {
                        continue;
                    }
                    qualified_relations += 1;
                    if qualified_relations > 1 {
                        return Err(ambiguous());
                    }
                }
                let schema = relation.table.source.schema();
                for (column_index, column) in schema.columns.iter().enumerate() {
                    if column.name == name {
                        if found.is_some() {
                            return Err(ambiguous());
                        }
                        found = Some(bound::Expr {
                            data_type: column.data_type.clone(),
                            nullable: column.nullable,
                            kind: bound::ExprKind::Column(bound::ColumnBinding {
                                relation: relation.table.relation,
                                column_index,
                            }),
                        });
                    }
                }
            }
            if let Some(found) = found {
                return Ok(found);
            }
            if qualified_relations > 0 {
                return Err(unknown());
            }
        }
        Err(unknown())
    }
}

/// Relations visible within one lexical query scope.
///
/// Relation order follows FROM-clause order. Names are not map keys: retaining
/// separate occurrences allows ambiguity checks rather than overwriting entries.
#[derive(Clone, Debug)]
pub struct Scope<'sql> {
    /// Relation occurrences introduced by this scope.
    pub relations: Vec<RelationBinding<'sql>>,
}

/// A visible relation name paired with a resolved table occurrence.
#[derive(Clone, Debug)]
pub struct RelationBinding<'sql> {
    /// The normalized table name or alias used for qualified column lookup.
    pub name: Cow<'sql, str>,
    /// Query-local identity and shared table source.
    pub table: bound::Table,
}
