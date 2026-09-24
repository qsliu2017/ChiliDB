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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        BindContext, BindError, ColumnSchema, LogicalType, RelationBinding, Scope, TableSchema,
        TableSource, bound,
    };

    #[derive(Debug)]
    struct TestSource(Arc<TableSchema>);

    impl TableSource for TestSource {
        fn schema(&self) -> Arc<TableSchema> {
            Arc::clone(&self.0)
        }
    }

    fn relation(name: &'static str, id: u32, columns: &[&str]) -> RelationBinding<'static> {
        RelationBinding {
            name: name.into(),
            table: bound::Table {
                relation: bound::RelationId(id),
                source: Arc::new(TestSource(Arc::new(TableSchema {
                    name: name.into(),
                    columns: columns
                        .iter()
                        .map(|name| ColumnSchema {
                            name: (*name).into(),
                            data_type: LogicalType::Int32,
                            nullable: false,
                        })
                        .collect(),
                }))),
            },
        }
    }

    #[test]
    fn lookup_uses_nearest_scope_and_respects_qualifier_shadowing() {
        let context = BindContext {
            scopes: vec![
                Scope {
                    relations: vec![relation("t", 0, &["outer", "shared"])],
                },
                Scope {
                    relations: vec![relation("t", 1, &["inner", "shared"])],
                },
            ],
        };
        let resolve =
            |qualifier, name| context.resolve_column::<std::convert::Infallible>(qualifier, name);
        let bound::ExprKind::Column(column) = resolve(None, "shared").unwrap().kind else {
            panic!()
        };
        assert_eq!(column.relation, bound::RelationId(1));
        let bound::ExprKind::Column(column) = resolve(None, "outer").unwrap().kind else {
            panic!()
        };
        assert_eq!(column.relation, bound::RelationId(0));
        assert!(matches!(
            resolve(Some("t"), "outer"),
            Err(BindError::UnknownColumn { .. })
        ));
        assert!(matches!(
            resolve(Some("missing"), "inner"),
            Err(BindError::UnknownColumn { .. })
        ));
    }

    #[test]
    fn ambiguous_names_are_not_overwritten_or_resolved_by_order() {
        let context = BindContext {
            scopes: vec![Scope {
                relations: vec![relation("a", 0, &["id"]), relation("b", 1, &["id"])],
            }],
        };
        assert!(matches!(
            context.resolve_column::<()>(None, "id"),
            Err(BindError::AmbiguousColumn { .. })
        ));
        assert!(context.resolve_column::<()>(Some("b"), "id").is_ok());
    }

    #[test]
    fn scopes_retain_relation_occurrences_and_share_sources() {
        let source: Arc<dyn TableSource> = Arc::new(TestSource(Arc::new(TableSchema {
            name: "items".into(),
            columns: vec![],
        })));
        let occurrence = |name: &'static str, relation| RelationBinding {
            name: name.into(),
            table: bound::Table {
                relation: bound::RelationId(relation),
                source: Arc::clone(&source),
            },
        };
        let context: BindContext<'_> = BindContext {
            scopes: vec![
                Scope {
                    relations: vec![occurrence("a", 0), occurrence("b", 1)],
                },
                Scope {
                    relations: vec![occurrence("a", 2)],
                },
            ],
        };

        let outer = &context.scopes[0].relations;
        let inner = &context.scopes[1].relations;
        assert_eq!(outer[0].name, inner[0].name);
        assert_ne!(outer[0].table.relation, inner[0].table.relation);
        assert_ne!(outer[0].table.relation, outer[1].table.relation);
        assert!(Arc::ptr_eq(&outer[0].table.source, &inner[0].table.source));
        assert!(Arc::ptr_eq(&outer[0].table.source, &outer[1].table.source));
    }
}
