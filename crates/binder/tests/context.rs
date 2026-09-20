use std::sync::Arc;

use chilidb_binder::{
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
