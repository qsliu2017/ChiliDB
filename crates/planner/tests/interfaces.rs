use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
};

use arrow_schema::DataType;
use chilidb_binder::{ColumnSchema, LogicalType, TableSchema, TableSource, bound};
use chilidb_planner::table_source::{self, TableAdapter};
use datafusion_expr::{
    LogicalPlan, LogicalPlanBuilder, TableSource as _, UserDefinedLogicalNodeCore, col, lit,
};

#[derive(Debug)]
struct Source(Arc<TableSchema>);
impl TableSource for Source {
    fn schema(&self) -> Arc<TableSchema> {
        Arc::clone(&self.0)
    }
}

fn table() -> bound::Table {
    bound::Table {
        relation: bound::RelationId(0),
        source: Arc::new(Source(Arc::new(TableSchema {
            name: "items".into(),
            columns: vec![ColumnSchema {
                name: "value".into(),
                data_type: LogicalType::Int32,
                nullable: false,
            }],
        }))),
    }
}

fn hash(value: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[test]
fn source_clone_and_rebuilt_scan_preserve_identity_equality_and_hash() {
    let table = table();
    let adapter = TableAdapter::new(table.clone(), true);
    let clone = adapter.clone();
    assert!(Arc::ptr_eq(&adapter.table().source, &clone.table().source));
    assert_eq!(adapter.schema(), clone.schema());
    assert!(clone.with_ctid());
    assert_eq!(clone.schema().field(0).data_type(), &DataType::Int32);
    assert_eq!(
        clone.schema().field(1).data_type(),
        &DataType::FixedSizeBinary(6)
    );
    assert!(!clone.schema().field(1).is_nullable());
    assert_eq!(clone.schema().field(1).name(), "__ctid");
    assert_eq!(table_source::ctid_type(), DataType::FixedSizeBinary(6));
    let read_adapter = TableAdapter::new(table.clone(), false);
    assert!(!read_adapter.with_ctid());
    assert!(read_adapter.schema().field_with_name("__ctid").is_err());

    let scan = table_source::scan(&table, true).unwrap();
    let cloned_scan = scan.clone();
    let rebuilt_scan = table_source::scan(&table, true).unwrap();
    assert_eq!(scan, cloned_scan);
    assert_eq!(hash(&scan), hash(&cloned_scan));
    assert_eq!(scan, rebuilt_scan);
    assert_eq!(hash(&scan), hash(&rebuilt_scan));
    assert_ne!(scan, table_source::scan(&table, false).unwrap());
}

use chilidb_planner::{Ctid, Modification, ModifyTable, Target, UpdateAssignment};
use datafusion_common::{Column, ExprSchema, ScalarValue};
use datafusion_optimizer::{
    Optimizer, OptimizerContext, eliminate_filter::EliminateFilter,
    optimize_projections::OptimizeProjections, simplify_expressions::SimplifyExpressions,
};

fn payload(expressions: Vec<datafusion_expr::Expr>) -> LogicalPlan {
    LogicalPlanBuilder::empty(true)
        .project(expressions)
        .unwrap()
        .build()
        .unwrap()
}

fn insert(input: LogicalPlan) -> ModifyTable {
    ModifyTable::try_insert(
        Target::new(table()),
        input,
        vec![Column::from_name("value")],
    )
    .unwrap()
}

#[test]
fn modify_table_rebuilds_preserve_identity_and_reject_invalid_payloads() {
    let table = table();
    let input = table_source::scan(&table, true).unwrap();
    let value = table_source::column(bound::ColumnBinding {
        relation: table.relation,
        column_index: 0,
    });
    let ctid = table_source::ctid(&table);
    let target = Target::new(table.clone());
    assert_eq!(target, target.clone());
    assert_eq!(hash(&target), hash(&target.clone()));
    let other = Target::new(bound::Table {
        relation: table.relation,
        source: Arc::new(Source(table.source.schema())),
    });
    assert_ne!(
        target, other,
        "equal schemas must not merge different source handles"
    );
    let insert =
        ModifyTable::try_insert(target.clone(), input.clone(), vec![value.clone()]).unwrap();
    let assignment = UpdateAssignment {
        target_column_index: 0,
        value: value.clone(),
    };
    let update = ModifyTable::try_update(
        target.clone(),
        input.clone(),
        ctid.clone(),
        vec![assignment.clone()],
    )
    .unwrap();
    let delete = ModifyTable::try_delete(target.clone(), input.clone(), ctid.clone()).unwrap();
    for plan in [
        insert.clone().into_plan(),
        update.clone().into_plan(),
        delete.clone().into_plan(),
    ] {
        let LogicalPlan::Extension(extension) = &plan else {
            panic!()
        };
        let rebuilt = extension
            .node
            .with_exprs_and_inputs(extension.node.expressions(), vec![input.clone()])
            .unwrap();
        let rebuilt =
            LogicalPlan::Extension(datafusion_expr::logical_plan::Extension { node: rebuilt });
        assert_eq!(plan, rebuilt);
        assert_eq!(hash(&plan), hash(&rebuilt));
        assert!(
            extension
                .node
                .with_exprs_and_inputs(
                    vec![lit(1_i32); extension.node.expressions().len()],
                    vec![input.clone()]
                )
                .is_err()
        );
        assert!(
            extension
                .node
                .with_exprs_and_inputs(vec![], vec![input.clone()])
                .is_err()
        );
        assert!(
            extension
                .node
                .with_exprs_and_inputs(extension.node.expressions(), vec![])
                .is_err()
        );
        assert!(
            extension
                .node
                .with_exprs_and_inputs(
                    extension.node.expressions(),
                    vec![input.clone(), input.clone()]
                )
                .is_err()
        );
    }
    assert!(ModifyTable::try_insert(target.clone(), input.clone(), vec![]).is_err());
    assert!(
        ModifyTable::try_insert(
            target.clone(),
            input.clone(),
            vec![Column::from_name("missing")]
        )
        .is_err()
    );
    assert!(ModifyTable::try_insert(target.clone(), input.clone(), vec![ctid.clone()]).is_err());
    assert!(
        ModifyTable::try_update(
            target.clone(),
            input.clone(),
            ctid.clone(),
            vec![UpdateAssignment {
                target_column_index: 1,
                value: value.clone()
            }]
        )
        .is_err()
    );
    assert!(
        ModifyTable::try_update(
            target.clone(),
            input.clone(),
            ctid.clone(),
            vec![assignment.clone(), assignment]
        )
        .is_err()
    );
    assert!(ModifyTable::try_update(target.clone(), input.clone(), ctid.clone(), vec![]).is_err());
    assert!(
        ModifyTable::try_update(
            target.clone(),
            input.clone(),
            ctid.clone(),
            vec![UpdateAssignment {
                target_column_index: 0,
                value: ctid
            }]
        )
        .is_err()
    );
    assert!(ModifyTable::try_delete(target.clone(), input, value).is_err());
    let nullable_ctid = payload(vec![
        lit(ScalarValue::FixedSizeBinary(Ctid::BYTE_LEN as i32, None)).alias("ctid"),
    ]);
    assert!(ModifyTable::try_delete(target, nullable_ctid, Column::from_name("ctid")).is_err());
    assert!(
        insert
            .with_exprs_and_inputs(
                vec![col("value")],
                vec![payload(vec![lit(1_i64).alias("value")])]
            )
            .is_err()
    );
}

#[test]
fn update_and_delete_reject_invalid_ctid_types_and_nullability() {
    for invalid in [
        ScalarValue::FixedSizeBinary(Ctid::BYTE_LEN as i32, None),
        ScalarValue::FixedSizeBinary(5, Some(vec![1; 5])),
        ScalarValue::FixedSizeBinary(7, Some(vec![1; 7])),
        ScalarValue::Binary(Some(Ctid::new(0, 1).unwrap().to_le_bytes().to_vec())),
    ] {
        let input = payload(vec![
            lit(invalid.clone()).alias("ctid"),
            lit(1_i32).alias("value"),
        ]);
        let target = Target::new(table());
        assert!(
            ModifyTable::try_delete(target.clone(), input.clone(), Column::from_name("ctid"))
                .is_err(),
            "delete accepted {invalid:?}"
        );
        assert!(
            ModifyTable::try_update(
                target,
                input,
                Column::from_name("ctid"),
                vec![UpdateAssignment {
                    target_column_index: 0,
                    value: Column::from_name("value"),
                }],
            )
            .is_err(),
            "update accepted {invalid:?}"
        );
    }
}

#[test]
fn nested_modify_tables_are_rejected_even_behind_relational_nodes() {
    let inner = insert(payload(vec![lit(1_i32).alias("value")])).into_plan();
    let wrapped = LogicalPlanBuilder::from(inner.clone())
        .project(vec![lit(1_i32).alias("value")])
        .unwrap()
        .build()
        .unwrap();
    for input in [inner, wrapped] {
        assert!(
            ModifyTable::try_insert(
                Target::new(table()),
                input.clone(),
                vec![Column::from_name("value")]
            )
            .is_err()
        );
        assert!(
            ModifyTable::try_update(
                Target::new(table()),
                input.clone(),
                Column::from_name("ctid"),
                vec![UpdateAssignment {
                    target_column_index: 0,
                    value: Column::from_name("value")
                }]
            )
            .is_err()
        );
        assert!(
            ModifyTable::try_delete(Target::new(table()), input, Column::from_name("ctid"))
                .is_err()
        );
    }
}

fn optimize(plan: LogicalPlan) -> LogicalPlan {
    let optimizer = Optimizer::with_rules(vec![
        Arc::new(SimplifyExpressions::new()),
        Arc::new(OptimizeProjections::new()),
        Arc::new(EliminateFilter::new()),
    ]);
    let context = OptimizerContext::new().with_skip_failing_rules(false);
    optimizer.optimize(plan, &context, |_, _| {}).unwrap()
}

fn assert_completion(plan: &LogicalPlan) -> &ModifyTable {
    let LogicalPlan::Extension(extension) = plan else {
        panic!("modification root disappeared: {plan:?}")
    };
    let insert = extension
        .node
        .as_any()
        .downcast_ref::<ModifyTable>()
        .expect("ModifyTable root");
    assert_eq!(plan.schema().fields().len(), 1);
    assert_eq!(plan.schema().field(0).name(), "affected_rows");
    assert_eq!(plan.schema().field(0).data_type(), &DataType::Int64);
    assert!(!plan.schema().field(0).is_nullable());
    assert!(matches!(insert.operation(), Modification::Insert { .. }));
    insert
}

#[test]
fn optimizer_folds_expressions_and_prunes_unused_payload_under_insert() {
    let input = payload(vec![
        (lit(1_i32) + lit(2_i32)).alias("value"),
        lit(9_i32).alias("unused"),
    ]);
    let original = insert(input).into_plan();
    let optimized = optimize(original.clone());
    let node = assert_completion(&optimized);
    assert_ne!(
        original, optimized,
        "the actual optimizer must transform the child"
    );
    assert_eq!(node.input().schema().fields().len(), 1);
    let LogicalPlan::Projection(projection) = node.input() else {
        panic!("expected folded projection: {:?}", node.input())
    };
    assert_eq!(projection.expr, vec![lit(3_i32).alias("value")]);
    assert_eq!(
        node.operation(),
        &Modification::Insert {
            values: vec![Column::from_name("value")]
        }
    );
}

#[test]
fn optimizer_eliminates_false_filter_but_retains_insert_completion() {
    let input = LogicalPlanBuilder::from(payload(vec![lit(1_i32).alias("value")]))
        .filter(lit(false))
        .unwrap()
        .build()
        .unwrap();
    let optimized = optimize(insert(input).into_plan());
    let node = assert_completion(&optimized);
    let LogicalPlan::EmptyRelation(empty) = node.input() else {
        panic!("false filter should become empty: {:?}", node.input())
    };
    assert!(!empty.produce_one_row);
    assert_eq!(
        node.operation(),
        &Modification::Insert {
            values: vec![Column::from_name("value")]
        }
    );
}

#[test]
fn optimizer_folds_values_expressions_beneath_insert() {
    let input = LogicalPlanBuilder::values(vec![vec![lit(1_i32) + lit(2_i32)]])
        .unwrap()
        .build()
        .unwrap();
    let value = Column::from_name(input.schema().field(0).name());
    let original = ModifyTable::try_insert(Target::new(table()), input, vec![value.clone()])
        .unwrap()
        .into_plan();
    let optimized = optimize(original);
    let node = assert_completion(&optimized);
    let LogicalPlan::Values(values) = node.input() else {
        panic!("expected Values: {:?}", node.input())
    };
    // SimplifyExpressions preserves the original expression's display name with an alias.
    assert_eq!(
        values.values,
        vec![vec![lit(3_i32).alias("Int32(1) + Int32(2)")]]
    );
    optimized
        .check_invariants(datafusion_expr::logical_plan::InvariantLevel::Executable)
        .unwrap();
    assert!(node.input().schema().index_of_column(&value).is_ok());
    assert_eq!(
        node.operation(),
        &Modification::Insert {
            values: vec![value]
        }
    );
}

#[test]
fn modification_discriminant_participates_in_equality_and_hash() {
    let table = table();
    let input = table_source::scan(&table, true).unwrap();
    let target = Target::new(table.clone());
    let value = table_source::column(bound::ColumnBinding {
        relation: table.relation,
        column_index: 0,
    });
    let ctid = table_source::ctid(&table);
    let nodes = [
        ModifyTable::try_insert(target.clone(), input.clone(), vec![value.clone()]).unwrap(),
        ModifyTable::try_update(
            target.clone(),
            input.clone(),
            ctid.clone(),
            vec![UpdateAssignment {
                target_column_index: 0,
                value,
            }],
        )
        .unwrap(),
        ModifyTable::try_delete(target.clone(), input.clone(), ctid).unwrap(),
    ];
    for (i, node) in nodes.iter().enumerate() {
        assert_eq!(node.name(), "ModifyTable");
        let operation_name = match node.operation() {
            Modification::Insert { .. } => "Insert",
            Modification::Update { .. } => "Update",
            Modification::Delete { .. } => "Delete",
        };
        assert!(
            node.clone()
                .into_plan()
                .display_indent()
                .to_string()
                .contains(&format!("ModifyTable({operation_name})"))
        );
        assert_eq!(node.target(), &target);
        assert_eq!(node.input(), &input);
        assert_eq!(node, &node.clone());
        assert_eq!(hash(node), hash(&node.clone()));
        assert_eq!(hash(node.operation()), hash(&node.operation().clone()));
        for other in &nodes[i + 1..] {
            assert_ne!(node.operation(), other.operation());
            assert_ne!(
                node, other,
                "common target and input do not define modification identity"
            );
        }
    }
    assert_eq!(
        nodes
            .into_iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );

    // Inspect the hash input rather than assuming a collision-free hash function.
    #[derive(Default)]
    struct HashInput(Vec<u8>);
    impl Hasher for HashInput {
        fn finish(&self) -> u64 {
            0
        }
        fn write(&mut self, bytes: &[u8]) {
            self.0.extend_from_slice(bytes);
        }
    }
    let column = Column::from_name("same");
    let operations = [
        Modification::Insert {
            values: vec![column.clone()],
        },
        Modification::Update {
            ctid: column.clone(),
            assignments: vec![],
        },
        Modification::Delete { ctid: column },
    ];
    let streams: Vec<_> = operations
        .iter()
        .map(|op| {
            let mut input = HashInput::default();
            op.hash(&mut input);
            input.0
        })
        .collect();
    for i in 0..streams.len() {
        for j in i + 1..streams.len() {
            assert_ne!(streams[i], streams[j]);
        }
    }
}

#[test]
fn rebuilding_each_operation_remaps_columns_and_preserves_destinations() {
    let table = bound::Table {
        relation: bound::RelationId(0),
        source: Arc::new(Source(Arc::new(TableSchema {
            name: "pair".into(),
            columns: ["a", "b"]
                .into_iter()
                .map(|name| ColumnSchema {
                    name: name.into(),
                    data_type: LogicalType::Int32,
                    nullable: false,
                })
                .collect(),
        }))),
    };
    let target = Target::new(table);
    let make_input = |prefix: &str| {
        payload(vec![
            lit(1_i32).alias(format!("{prefix}a")),
            lit(2_i32).alias(format!("{prefix}b")),
            lit(ScalarValue::FixedSizeBinary(
                Ctid::BYTE_LEN as i32,
                Some(Ctid::new(0, 1).unwrap().to_le_bytes().to_vec()),
            ))
            .alias(format!("{prefix}ctid")),
        ])
    };
    let input = make_input("");
    let nodes = [
        ModifyTable::try_insert(
            target.clone(),
            input.clone(),
            vec![Column::from_name("b"), Column::from_name("a")],
        )
        .unwrap(),
        ModifyTable::try_update(
            target.clone(),
            input.clone(),
            Column::from_name("ctid"),
            vec![
                UpdateAssignment {
                    target_column_index: 1,
                    value: Column::from_name("a"),
                },
                UpdateAssignment {
                    target_column_index: 0,
                    value: Column::from_name("b"),
                },
            ],
        )
        .unwrap(),
        ModifyTable::try_delete(target.clone(), input, Column::from_name("ctid")).unwrap(),
    ];
    for node in nodes {
        let expressions = node
            .expressions()
            .into_iter()
            .map(|expr| {
                let datafusion_expr::Expr::Column(column) = expr else {
                    panic!()
                };
                col(format!("new_{}", column.name))
            })
            .collect();
        let rebuilt = node
            .with_exprs_and_inputs(expressions, vec![make_input("new_")])
            .unwrap();
        assert_eq!(rebuilt.target(), &target);
        match (node.operation(), rebuilt.operation()) {
            (Modification::Insert { values: old }, Modification::Insert { values: new }) => {
                assert_eq!(
                    new.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                    ["new_b", "new_a"]
                );
                assert_eq!(old.len(), new.len());
            }
            (
                Modification::Update {
                    assignments: old, ..
                },
                Modification::Update {
                    ctid,
                    assignments: new,
                },
            ) => {
                assert_eq!(ctid.name, "new_ctid");
                assert_eq!(
                    new.iter()
                        .map(|a| a.target_column_index)
                        .collect::<Vec<_>>(),
                    [1, 0]
                );
                for (old, new) in old.iter().zip(new) {
                    assert_eq!(new.value.name, format!("new_{}", old.value.name));
                }
            }
            (Modification::Delete { .. }, Modification::Delete { ctid }) => {
                assert_eq!(ctid.name, "new_ctid")
            }
            _ => panic!("rebuilding changed operation"),
        }
        let roundtrip = rebuilt
            .with_exprs_and_inputs(rebuilt.expressions(), vec![rebuilt.input().clone()])
            .unwrap();
        assert_eq!(rebuilt, roundtrip);
        assert_eq!(hash(&rebuilt), hash(&roundtrip));
    }
}

#[test]
fn actual_optimizer_preserves_required_columns_for_update_and_delete() {
    let target = Target::new(table());
    let input = payload(vec![
        (lit(1_i32) + lit(2_i32)).alias("value"),
        lit(ScalarValue::FixedSizeBinary(
            Ctid::BYTE_LEN as i32,
            Some(Ctid::new(0, 1).unwrap().to_le_bytes().to_vec()),
        ))
        .alias("ctid"),
        lit(9_i32).alias("unused"),
    ]);
    for node in [
        ModifyTable::try_update(
            target.clone(),
            input.clone(),
            Column::from_name("ctid"),
            vec![UpdateAssignment {
                target_column_index: 0,
                value: Column::from_name("value"),
            }],
        )
        .unwrap(),
        ModifyTable::try_delete(target, input, Column::from_name("ctid")).unwrap(),
    ] {
        let operation = node.operation().clone();
        let ctid_column = Column::from_name("ctid");
        let ctid_field = node
            .input()
            .schema()
            .field_from_column(&ctid_column)
            .unwrap()
            .clone();
        let LogicalPlan::Projection(original_projection) = node.input() else {
            panic!()
        };
        let ctid_expr = original_projection.expr[1].clone();
        let optimized = optimize(node.into_plan());
        let LogicalPlan::Extension(extension) = &optimized else {
            panic!("modification root disappeared")
        };
        let node = extension
            .node
            .as_any()
            .downcast_ref::<ModifyTable>()
            .unwrap();
        assert_eq!(node.operation(), &operation);
        assert_eq!(
            node.input()
                .schema()
                .field_from_column(&ctid_column)
                .unwrap(),
            &ctid_field
        );
        let LogicalPlan::Projection(projection) = node.input() else {
            panic!()
        };
        assert!(
            projection.expr.contains(&ctid_expr),
            "optimizer changed the CTID expression"
        );
        for expr in node.expressions() {
            let datafusion_expr::Expr::Column(column) = expr else {
                panic!()
            };
            assert!(node.input().schema().index_of_column(&column).is_ok());
        }
        assert!(
            node.input()
                .schema()
                .index_of_column(&Column::from_name("unused"))
                .is_err()
        );
    }
}
