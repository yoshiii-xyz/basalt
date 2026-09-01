//! Lightweight cost-based access-path selection.
//!
//! The planner deliberately keeps the physical plan small: a table scan is
//! always available, and an equality predicate on an indexed column can use
//! an index when its candidate set is cheaper than visiting every row. More
//! join and aggregate planning remains in the executor's relational pipeline.

use crate::db::Table;
use crate::sql::ast::{BinOp, Expr};
use crate::types::Value;

type RangeCandidate = (usize, Option<Value>, Option<Value>, Vec<u64>, String);

#[derive(Debug, Clone)]
pub enum AccessPath {
    TableScan,
    IndexScan {
        index_name: String,
        column: usize,
        key: Value,
        row_ids: Vec<u64>,
    },
    IndexRange {
        index_name: String,
        column: usize,
        low: Option<Value>,
        high: Option<Value>,
        row_ids: Vec<u64>,
    },
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub table: String,
    pub access: AccessPath,
    pub estimated_rows: usize,
}

/// Choose an access path for a single table's predicate. The returned path is
/// guaranteed to be semantically equivalent to a full scan; it only narrows
/// the rows that still go through normal predicate evaluation.
pub fn choose(table: &Table, relation: Option<&str>, predicate: Option<&Expr>) -> Plan {
    let full_scan_cost = table.row_count().max(1);
    let mut best: Option<(usize, AccessPath)> = None;
    let mut candidate = None;
    if let Some(predicate) = predicate {
        find_equality(table, relation, predicate, &mut candidate);
    }
    if let Some((column, key, row_ids, index_name)) = candidate {
        // An index traversal plus fetching candidates is worthwhile only when
        // the candidate set is smaller than a full table walk. Primary keys
        // and UNIQUE indexes naturally satisfy this for all but tiny tables.
        if row_ids.len().saturating_mul(2) <= full_scan_cost {
            best = Some((
                row_ids.len(),
                AccessPath::IndexScan {
                    index_name,
                    column,
                    key,
                    row_ids,
                },
            ));
        }
    }
    let mut range = None;
    if let Some(predicate) = predicate {
        find_range(table, relation, predicate, &mut range);
    }
    if let Some((column, low, high, row_ids, index_name)) = range
        && row_ids.len().saturating_mul(2) <= full_scan_cost
        && best
            .as_ref()
            .map(|(estimated, _)| row_ids.len() < *estimated)
            .unwrap_or(true)
    {
        best = Some((
            row_ids.len(),
            AccessPath::IndexRange {
                index_name,
                column,
                low,
                high,
                row_ids,
            },
        ));
    }
    if let Some((estimated_rows, access)) = best {
        return Plan {
            table: table.name.clone(),
            estimated_rows,
            access,
        };
    }
    Plan {
        table: table.name.clone(),
        estimated_rows: table.row_count(),
        access: AccessPath::TableScan,
    }
}

fn find_equality(
    table: &Table,
    relation: Option<&str>,
    expr: &Expr,
    candidate: &mut Option<(usize, Value, Vec<u64>, String)>,
) {
    match expr {
        Expr::Binary {
            left,
            op: BinOp::And,
            right,
        } => {
            find_equality(table, relation, left, candidate);
            find_equality(table, relation, right, candidate);
        }
        Expr::Binary {
            left,
            op: BinOp::Eq,
            right,
        } => {
            if let Some((column_name, value)) = column_literal(left, right, relation)
                && let Ok(column) = table.column_index(column_name)
                && let Some((index_name, row_ids)) = table.lookup_eq_index(column, value)
            {
                let is_better = candidate
                    .as_ref()
                    .map(|existing| row_ids.len() < existing.2.len())
                    .unwrap_or(true);
                if is_better {
                    *candidate = Some((column, value.clone(), row_ids, index_name));
                }
            }
        }
        _ => {}
    }
}

fn column_literal<'a>(
    left: &'a Expr,
    right: &'a Expr,
    relation: Option<&str>,
) -> Option<(&'a str, &'a Value)> {
    match (left, right) {
        (Expr::Column(column), Expr::Literal(value)) => Some((column, value)),
        (
            Expr::ColumnRef {
                relation: found,
                column,
            },
            Expr::Literal(value),
        ) if relation
            .map(|wanted| found.eq_ignore_ascii_case(wanted))
            .unwrap_or(true) =>
        {
            Some((column, value))
        }
        (Expr::Literal(value), Expr::Column(column)) => Some((column, value)),
        (
            Expr::Literal(value),
            Expr::ColumnRef {
                relation: found,
                column,
            },
        ) if relation
            .map(|wanted| found.eq_ignore_ascii_case(wanted))
            .unwrap_or(true) =>
        {
            Some((column, value))
        }
        _ => None,
    }
}

fn find_range(
    table: &Table,
    relation: Option<&str>,
    expr: &Expr,
    candidate: &mut Option<RangeCandidate>,
) {
    match expr {
        Expr::Binary {
            left,
            op: BinOp::And,
            right,
        } => {
            find_range(table, relation, left, candidate);
            find_range(table, relation, right, candidate);
        }
        Expr::Binary { left, op, right }
            if matches!(op, BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq) =>
        {
            let (column, value, reversed) =
                if let Some((column, value)) = column_literal(left, right, relation) {
                    (column, value, false)
                } else if let Some((column, value)) = column_literal(right, left, relation) {
                    (column, value, true)
                } else {
                    return;
                };
            let (low, high) = match (op, reversed) {
                (BinOp::Lt | BinOp::LtEq, false) => (None, Some(value.clone())),
                (BinOp::Gt | BinOp::GtEq, false) => (Some(value.clone()), None),
                (BinOp::Lt | BinOp::LtEq, true) => (Some(value.clone()), None),
                (BinOp::Gt | BinOp::GtEq, true) => (None, Some(value.clone())),
                _ => unreachable!(),
            };
            if let Ok(column) = table.column_index(column)
                && let Some((name, rows)) =
                    table.lookup_range_index(column, low.as_ref(), high.as_ref())
            {
                let is_better = candidate
                    .as_ref()
                    .map(|existing| rows.len() < existing.3.len())
                    .unwrap_or(true);
                if is_better {
                    *candidate = Some((column, low, high, rows, name));
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column;
    use crate::sql::ast::Statement;
    use crate::sql::parser::parse;
    use crate::types::ColumnType;

    #[test]
    fn picks_selective_index() {
        let mut table = Table::new(
            "t",
            vec![Column {
                name: "id".into(),
                ty: ColumnType::Integer,
                not_null: true,
                unique: false,
                primary_key: false,
            }],
        )
        .unwrap();
        for value in 0..100 {
            table.insert_row(vec![Value::Integer(value)]).unwrap();
        }
        table.create_index("id_idx", 0, false).unwrap();
        let Statement::Select { where_clause, .. } =
            &parse("SELECT * FROM t WHERE id = 42").unwrap()[0]
        else {
            panic!()
        };
        let plan = choose(&table, None, where_clause.as_ref());
        assert!(matches!(plan.access, AccessPath::IndexScan { .. }));
    }

    #[test]
    fn picks_range_index() {
        let mut table = Table::new(
            "t",
            vec![Column {
                name: "id".into(),
                ty: ColumnType::Integer,
                not_null: true,
                unique: false,
                primary_key: false,
            }],
        )
        .unwrap();
        for value in 0..100 {
            table.insert_row(vec![Value::Integer(value)]).unwrap();
        }
        table.create_index("id_idx", 0, false).unwrap();
        let Statement::Select { where_clause, .. } =
            &parse("SELECT * FROM t WHERE id >= 90").unwrap()[0]
        else {
            panic!()
        };
        assert!(matches!(
            choose(&table, None, where_clause.as_ref()).access,
            AccessPath::IndexRange { .. }
        ));
    }

    #[test]
    fn picks_the_most_selective_predicate() {
        let mut table = Table::new(
            "t",
            vec![
                Column {
                    name: "bucket".into(),
                    ty: ColumnType::Integer,
                    not_null: true,
                    unique: false,
                    primary_key: false,
                },
                Column {
                    name: "id".into(),
                    ty: ColumnType::Integer,
                    not_null: true,
                    unique: false,
                    primary_key: false,
                },
            ],
        )
        .unwrap();
        for id in 0..100 {
            table
                .insert_row(vec![Value::Integer(id % 10), Value::Integer(id)])
                .unwrap();
        }
        table.create_index("bucket_idx", 0, false).unwrap();
        table.create_index("id_idx", 1, false).unwrap();
        let Statement::Select { where_clause, .. } =
            &parse("SELECT * FROM t WHERE bucket = 1 AND id = 42").unwrap()[0]
        else {
            panic!()
        };
        let plan = choose(&table, None, where_clause.as_ref());
        assert!(matches!(
            plan.access,
            AccessPath::IndexScan { column: 1, .. }
        ));
    }
}
