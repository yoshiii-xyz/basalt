//! SQL executor and the first query-planning layer.
//!
//! Queries are resolved into a flat row/schema representation. This keeps
//! joins, grouping, scalar expressions, and three-valued predicates on one
//! execution path while retaining the simple `State` API used by the storage
//! tests.

use std::cmp::Ordering;
use std::collections::HashSet;

use crate::db::{Column, DbError, DbErrorKind, Row, State, StatementResult, Table, dberr};
use crate::eval::{self, ColumnBinding};
use crate::planner::{self, AccessPath};
use crate::sql::ast::{ColumnDef, Expr, JoinKind, SelectItems, Statement};
use crate::types::Value;

pub(crate) const MCP_EXECUTION_WORK_LIMIT: usize = 1_000_000;

#[derive(Debug)]
pub(crate) struct ExecutionBudget {
    limit: Option<usize>,
    remaining: Option<usize>,
}

impl ExecutionBudget {
    pub(crate) fn unlimited() -> Self {
        Self {
            limit: None,
            remaining: None,
        }
    }

    pub(crate) fn bounded(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            remaining: Some(limit),
        }
    }

    fn is_unlimited(&self) -> bool {
        self.remaining.is_none()
    }

    fn consume(&mut self, units: usize, operation: &str) -> Result<(), DbError> {
        let Some(remaining) = &mut self.remaining else {
            return Ok(());
        };
        if units > *remaining {
            *remaining = 0;
            let limit = self.limit.expect("bounded budgets have a limit");
            return Err(dberr(
                DbErrorKind::Limit,
                format!(
                    "execution exceeded the {limit}-unit work limit while {operation}; narrow the query or use the CLI for larger jobs"
                ),
            ));
        }
        *remaining -= units;
        Ok(())
    }

    fn row(&mut self, row: &Row, operation: &str) -> Result<(), DbError> {
        if self.is_unlimited() {
            return Ok(());
        }
        self.consume(row_work_units(row), operation)
    }

    fn table_clone(&mut self, table: &Table, operation: &str) -> Result<(), DbError> {
        if self.is_unlimited() {
            return Ok(());
        }
        let units = table
            .scan()
            .fold(table.columns.len().max(1), |total, (_, row)| {
                total.saturating_add(row_work_units(row))
            });
        self.consume(units, operation)
    }

    pub(crate) fn state_clone(&mut self, state: &State, operation: &str) -> Result<(), DbError> {
        if self.is_unlimited() {
            return Ok(());
        }
        for table in state.tables.values() {
            self.table_clone(table, operation)?;
        }
        Ok(())
    }
}

fn value_work_units(value: &Value) -> usize {
    match value {
        Value::Text(value) => value.len().saturating_add(1023) / 1024 + 1,
        _ => 1,
    }
}

fn row_work_units(row: &Row) -> usize {
    row.iter().map(value_work_units).sum::<usize>().max(1)
}

fn joined_row_work_units(left: &Row, right: &Row) -> usize {
    row_work_units(left)
        .saturating_add(row_work_units(right))
        .saturating_add(1)
}

pub fn execute(state: &mut State, stmt: &Statement) -> Result<StatementResult, DbError> {
    let mut budget = ExecutionBudget::unlimited();
    execute_with_budget(state, stmt, &mut budget)
}

pub(crate) fn execute_with_budget(
    state: &mut State,
    stmt: &Statement,
    budget: &mut ExecutionBudget,
) -> Result<StatementResult, DbError> {
    match stmt {
        Statement::CreateTable {
            name,
            if_not_exists,
            columns,
        } => {
            if state.contains_table(name) {
                if *if_not_exists {
                    return Ok(StatementResult::CreateTable { name: name.clone() });
                }
                return Err(dberr(
                    DbErrorKind::Constraint,
                    format!("table '{name}' already exists"),
                ));
            }
            let columns: Vec<Column> = columns.iter().map(col_from_def).collect();
            state
                .tables
                .insert(name.clone(), Table::new(name, columns)?);
            Ok(StatementResult::CreateTable { name: name.clone() })
        }
        Statement::DropTable { name, if_exists } => {
            if let Some(table) = state.table(name) {
                budget.table_clone(table, "dropping a table")?;
            }
            if state.remove_table(name).is_none() {
                if *if_exists {
                    return Ok(StatementResult::DropTable { name: name.clone() });
                }
                return Err(dberr(
                    DbErrorKind::UnknownTable,
                    format!("no such table: {name}"),
                ));
            }
            Ok(StatementResult::DropTable { name: name.clone() })
        }
        Statement::CreateIndex {
            name,
            table,
            column,
            unique,
            if_not_exists,
        } => {
            if state.contains_index(name) {
                if *if_not_exists {
                    return Ok(StatementResult::CreateIndex {
                        name: name.clone(),
                        table: table.clone(),
                        column: column.clone(),
                    });
                }
                return Err(dberr(
                    DbErrorKind::Constraint,
                    format!("index '{name}' already exists"),
                ));
            }
            let table_ref = state.table(table).ok_or_else(|| {
                dberr(DbErrorKind::UnknownTable, format!("no such table: {table}"))
            })?;
            if table_ref.has_index(name) {
                if *if_not_exists {
                    return Ok(StatementResult::CreateIndex {
                        name: name.clone(),
                        table: table.clone(),
                        column: column.clone(),
                    });
                }
                return Err(dberr(
                    DbErrorKind::Constraint,
                    format!("index '{name}' already exists"),
                ));
            }
            budget.table_clone(table_ref, "building an index")?;
            let table_ref = state.table_mut(table).unwrap();
            let column_index = table_ref.column_index(column)?;
            table_ref.create_index(name, column_index, *unique)?;
            Ok(StatementResult::CreateIndex {
                name: name.clone(),
                table: table.clone(),
                column: column.clone(),
            })
        }
        Statement::DropIndex { name, if_exists } => {
            let dropped = state
                .tables
                .values_mut()
                .any(|table| table.drop_index(name));
            if !dropped && !if_exists {
                return Err(dberr(
                    DbErrorKind::Constraint,
                    format!("no such index: {name}"),
                ));
            }
            Ok(StatementResult::DropIndex { name: name.clone() })
        }
        Statement::Insert {
            table,
            columns,
            rows,
        } => exec_insert(state, table, columns, rows, budget),
        Statement::InsertSelect {
            table,
            columns,
            query,
        } => exec_insert_select(state, table, columns, query, budget),
        Statement::Select { .. } => exec_select(state, stmt, budget),
        Statement::Explain(inner) => exec_explain(state, inner, budget),
        Statement::Update {
            table,
            assignments,
            where_clause,
        } => exec_update(state, table, assignments, where_clause, budget),
        Statement::Delete {
            table,
            where_clause,
        } => exec_delete(state, table, where_clause, budget),
        Statement::Begin => Ok(StatementResult::Begin),
        Statement::Commit => Ok(StatementResult::Commit),
        Statement::Rollback => Ok(StatementResult::Rollback),
        Statement::Checkpoint => Ok(StatementResult::Checkpoint),
    }
}

fn col_from_def(definition: &ColumnDef) -> Column {
    Column {
        name: definition.name.clone(),
        ty: definition.ty.clone(),
        not_null: definition.not_null,
        unique: definition.unique,
        primary_key: definition.primary_key,
    }
}

fn eval_const(expr: &Expr) -> Result<Value, DbError> {
    // VALUES expressions are evaluated without a row.  This supports
    // constant arithmetic, NULL checks, and scalar functions while naturally
    // rejecting column references and aggregate expressions.
    let empty_row: Row = Vec::new();
    eval::eval_with_schema(&[], &empty_row, expr)
}

fn exec_insert(
    state: &mut State,
    table_name: &str,
    columns: &Option<Vec<String>>,
    rows: &[Vec<Expr>],
    budget: &mut ExecutionBudget,
) -> Result<StatementResult, DbError> {
    let table = state.table(table_name).ok_or_else(|| {
        dberr(
            DbErrorKind::UnknownTable,
            format!("no such table: {table_name}"),
        )
    })?;
    let column_indices = match columns {
        Some(names) => {
            let mut result = Vec::with_capacity(names.len());
            for name in names {
                result.push(table.column_index(name)?);
            }
            ensure_unique_columns(&result, "INSERT column list")?;
            result
        }
        None => (0..table.columns.len()).collect(),
    };
    if rows.is_empty() {
        return Ok(StatementResult::Insert { rows_affected: 0 });
    }
    let expected = column_indices.len();
    budget.table_clone(table, "preparing an insert")?;
    let table = state.table_mut(table_name).unwrap();
    let mut candidate = table.clone();
    for values_expr in rows {
        budget.consume(expected.max(1), "materializing inserted rows")?;
        if values_expr.len() != expected {
            return Err(dberr(
                DbErrorKind::ColumnCount,
                format!(
                    "column count mismatch: expected {expected}, got {}",
                    values_expr.len()
                ),
            ));
        }
        let mut values = vec![Value::Null; candidate.columns.len()];
        for (column, expr) in column_indices.iter().zip(values_expr) {
            values[*column] = candidate.coerce_val(&eval_const(expr)?, *column)?;
        }
        budget.row(&values, "materializing inserted rows")?;
        candidate.insert_row(values)?;
    }
    let count = rows.len();
    *table = candidate;
    Ok(StatementResult::Insert {
        rows_affected: count,
    })
}

fn exec_insert_select(
    state: &mut State,
    table_name: &str,
    columns: &Option<Vec<String>>,
    query: &Statement,
    budget: &mut ExecutionBudget,
) -> Result<StatementResult, DbError> {
    if !matches!(query, Statement::Select { .. }) {
        return Err(dberr(
            DbErrorKind::Syntax("INSERT SELECT requires a SELECT query".into()),
            "INSERT SELECT requires a SELECT query",
        ));
    }
    let selected = match exec_select(state, query, budget)? {
        StatementResult::Select { rows, .. } => rows,
        _ => unreachable!(),
    };
    let table = state.table(table_name).ok_or_else(|| {
        dberr(
            DbErrorKind::UnknownTable,
            format!("no such table: {table_name}"),
        )
    })?;
    let column_indices = match columns {
        Some(names) => {
            let result = names
                .iter()
                .map(|name| table.column_index(name))
                .collect::<Result<Vec<_>, _>>()?;
            ensure_unique_columns(&result, "INSERT column list")?;
            result
        }
        None => (0..table.columns.len()).collect(),
    };
    let expected = column_indices.len();
    budget.table_clone(table, "preparing an insert")?;
    let table = state.table_mut(table_name).unwrap();
    let mut candidate = table.clone();
    for source in &selected {
        budget.row(source, "materializing inserted rows")?;
        if source.len() != expected {
            return Err(dberr(
                DbErrorKind::ColumnCount,
                format!(
                    "column count mismatch: expected {expected}, got {}",
                    source.len()
                ),
            ));
        }
        let mut values = vec![Value::Null; candidate.columns.len()];
        for (target, value) in column_indices.iter().zip(source) {
            values[*target] = candidate.coerce_val(value, *target)?;
        }
        candidate.insert_row(values)?;
    }
    *table = candidate;
    Ok(StatementResult::Insert {
        rows_affected: selected.len(),
    })
}

fn exec_explain(
    state: &State,
    stmt: &Statement,
    budget: &mut ExecutionBudget,
) -> Result<StatementResult, DbError> {
    let Statement::Select {
        from,
        from_alias,
        where_clause,
        ..
    } = stmt
    else {
        return Ok(StatementResult::Explain(
            "EXPLAIN supports SELECT statements".into(),
        ));
    };
    if from.is_empty() {
        return Ok(StatementResult::Explain(
            "ConstantScan estimated_rows=1".into(),
        ));
    }
    let table = state
        .table(from)
        .ok_or_else(|| dberr(DbErrorKind::UnknownTable, format!("no such table: {from}")))?;
    let plan = planner::choose(
        table,
        from_alias.as_deref().or(Some(from)),
        where_clause.as_ref(),
    );
    let candidate_count = match &plan.access {
        AccessPath::TableScan => 0,
        AccessPath::IndexScan { row_ids, .. } | AccessPath::IndexRange { row_ids, .. } => {
            row_ids.len()
        }
    };
    budget.consume(candidate_count, "building an index access plan")?;
    let text = match plan.access {
        AccessPath::TableScan => format!(
            "TableScan table={from} estimated_rows={}",
            plan.estimated_rows
        ),
        AccessPath::IndexScan {
            index_name,
            column,
            key,
            row_ids,
        } => format!(
            "IndexScan index={index_name} table={from} column={column} key={key} candidates={} estimated_rows={}",
            row_ids.len(),
            plan.estimated_rows
        ),
        AccessPath::IndexRange {
            index_name,
            column,
            low,
            high,
            row_ids,
        } => format!(
            "IndexRange index={index_name} table={from} column={column} low={low:?} high={high:?} candidates={} estimated_rows={}",
            row_ids.len(),
            plan.estimated_rows
        ),
    };
    Ok(StatementResult::Explain(text))
}

#[derive(Clone)]
struct QueryRow {
    values: Row,
}

fn relation_schema(table: &Table, alias: Option<&str>) -> Vec<ColumnBinding> {
    let mut relations = vec![table.name.clone()];
    if let Some(alias) = alias
        && !relations
            .iter()
            .any(|value| value.eq_ignore_ascii_case(alias))
    {
        relations.push(alias.to_string());
    }
    table
        .columns
        .iter()
        .map(|column| ColumnBinding::with_relations(column.name.clone(), relations.clone()))
        .collect()
}

fn exec_select(
    state: &State,
    stmt: &Statement,
    budget: &mut ExecutionBudget,
) -> Result<StatementResult, DbError> {
    let Statement::Select {
        distinct,
        columns,
        from,
        from_alias,
        joins,
        where_clause,
        group_by,
        having,
        order_by,
        order_by_exprs,
        limit,
        offset,
    } = stmt
    else {
        unreachable!()
    };

    let (mut schema, mut input): (Vec<ColumnBinding>, Vec<QueryRow>) = if from.is_empty() {
        if !joins.is_empty() {
            return Err(dberr(
                DbErrorKind::Syntax("JOIN requires a FROM table".into()),
                "JOIN requires a FROM table",
            ));
        }
        (Vec::new(), vec![QueryRow { values: Vec::new() }])
    } else {
        let base = state
            .table(from)
            .ok_or_else(|| dberr(DbErrorKind::UnknownTable, format!("no such table: {from}")))?;
        let schema = relation_schema(base, from_alias.as_deref());
        let base_plan = planner::choose(
            base,
            from_alias.as_deref().or(Some(from)),
            where_clause.as_ref(),
        );
        let mut base_rows = Vec::new();
        match base_plan.access {
            AccessPath::TableScan => {
                for (_, row) in base.scan() {
                    budget.row(row, "scanning the base table")?;
                    base_rows.push(row.clone());
                }
            }
            AccessPath::IndexScan { row_ids, .. } | AccessPath::IndexRange { row_ids, .. } => {
                for rid in row_ids {
                    if let Some(row) = base.get_row(rid) {
                        budget.row(row, "materializing index matches")?;
                        base_rows.push(row.clone());
                    }
                }
            }
        }
        let input = base_rows
            .into_iter()
            .map(|values| QueryRow { values })
            .collect();
        (schema, input)
    };

    for join in joins {
        let right = state.table(&join.table).ok_or_else(|| {
            dberr(
                DbErrorKind::UnknownTable,
                format!("no such table: {}", join.table),
            )
        })?;
        let right_schema = relation_schema(right, join.alias.as_deref());
        let mut joined_schema = schema.clone();
        joined_schema.extend(right_schema.iter().cloned());
        if let Some(on) = &join.on {
            reject_aggregate(on, "JOIN ON")?;
            eval::validate_with_schema(&joined_schema, on)?;
        }
        let mut right_rows = Vec::new();
        for (_, row) in right.scan() {
            budget.row(row, "scanning a joined table")?;
            right_rows.push(row.clone());
        }
        budget.consume(right_rows.len(), "tracking join matches")?;
        let mut next = Vec::new();
        let mut matched_right = vec![false; right_rows.len()];
        if join.kind == JoinKind::Right {
            for (right_index, right_row) in right_rows.iter().enumerate() {
                let mut matched = false;
                for left in &input {
                    budget.consume(
                        joined_row_work_units(&left.values, right_row),
                        "materializing join candidates",
                    )?;
                    let mut values = left.values.clone();
                    values.extend(right_row.iter().cloned());
                    if join_passes(&join.kind, &join.on, &joined_schema, &values)? {
                        matched = true;
                        next.push(QueryRow { values });
                    }
                }
                if !matched {
                    budget.consume(
                        input_schema_width(&schema)
                            .saturating_add(row_work_units(right_row))
                            .saturating_add(1),
                        "materializing an unmatched join row",
                    )?;
                    let mut values = vec![Value::Null; input_schema_width(&schema)];
                    values.extend(right_row.iter().cloned());
                    next.push(QueryRow { values });
                }
                matched_right[right_index] = matched;
            }
        } else {
            for left in &input {
                let mut matched = false;
                for (right_index, right_row) in right_rows.iter().enumerate() {
                    budget.consume(
                        joined_row_work_units(&left.values, right_row),
                        "materializing join candidates",
                    )?;
                    let mut values = left.values.clone();
                    values.extend(right_row.iter().cloned());
                    if join_passes(&join.kind, &join.on, &joined_schema, &values)? {
                        matched = true;
                        matched_right[right_index] = true;
                        next.push(QueryRow { values });
                    }
                }
                if (join.kind == JoinKind::Left || join.kind == JoinKind::Full) && !matched {
                    budget.consume(
                        row_work_units(&left.values)
                            .saturating_add(right.columns.len())
                            .saturating_add(1),
                        "materializing an unmatched join row",
                    )?;
                    let mut values = left.values.clone();
                    values.extend(std::iter::repeat_n(Value::Null, right.columns.len()));
                    next.push(QueryRow { values });
                }
            }
            if join.kind == JoinKind::Full {
                for (right_index, right_row) in right_rows.iter().enumerate() {
                    if !matched_right[right_index] {
                        budget.consume(
                            input_schema_width(&schema)
                                .saturating_add(row_work_units(right_row))
                                .saturating_add(1),
                            "materializing an unmatched join row",
                        )?;
                        let mut values = vec![Value::Null; input_schema_width(&schema)];
                        values.extend(right_row.iter().cloned());
                        next.push(QueryRow { values });
                    }
                }
            }
        }
        schema = joined_schema;
        input = next;
    }

    let expanded_items = match columns {
        SelectItems::Star => Vec::new(),
        SelectItems::List(items) => expand_select_items(&schema, items)?,
    };
    let effective_order: Vec<(Expr, bool)> = if !order_by_exprs.is_empty() {
        order_by_exprs
            .iter()
            .map(|(expr, ascending)| {
                (
                    resolve_order_alias(expr.clone(), &expanded_items),
                    *ascending,
                )
            })
            .collect()
    } else {
        order_by
            .iter()
            .map(|(name, ascending)| (order_name_expr(name), *ascending))
            .collect()
    };
    if let Some(predicate) = where_clause {
        reject_aggregate(predicate, "WHERE")?;
        eval::validate_with_schema(&schema, predicate)?;
    }
    for expr in group_by {
        reject_aggregate(expr, "GROUP BY")?;
        eval::validate_with_schema(&schema, expr)?;
    }
    if let Some(predicate) = having {
        eval::validate_with_schema(&schema, predicate)?;
    }
    for expr in &expanded_items {
        eval::validate_with_schema(&schema, expr)?;
    }
    for (expr, _) in &effective_order {
        eval::validate_with_schema(&schema, expr)?;
    }

    let mut filtered = Vec::with_capacity(input.len());
    for row in input {
        budget.row(&row.values, "evaluating a filter")?;
        if where_clause
            .as_ref()
            .map(|predicate| eval::where_matches_with_schema(&schema, &row.values, predicate))
            .transpose()?
            .unwrap_or(true)
        {
            filtered.push(row.values);
        }
    }

    let has_aggregate = match columns {
        SelectItems::Star => false,
        SelectItems::List(items) => items.iter().any(eval::contains_aggregate),
    } || having.as_ref().is_some_and(eval::contains_aggregate);
    let grouped = has_aggregate || !group_by.is_empty() || having.is_some();
    let groups = make_groups(&schema, filtered, group_by, grouped, budget)?;

    let output_names = select_output_names(&schema, columns, &expanded_items);
    let mut projected: Vec<(Row, Vec<Value>)> = Vec::new();
    for group in groups {
        let group_work = group.iter().fold(1usize, |total, row| {
            total.saturating_add(row_work_units(row))
        });
        budget.consume(group_work, "evaluating a result group")?;
        if let Some(predicate) = having
            && !eval::eval_group(&schema, &group, predicate)?
                .is_truthy()
                .unwrap_or(false)
        {
            continue;
        }
        let row = match columns {
            SelectItems::Star => group.first().cloned().unwrap_or_default(),
            SelectItems::List(_) => expanded_items
                .iter()
                .map(|expr| eval::eval_group(&schema, &group, expr))
                .collect::<Result<Vec<_>, _>>()?,
        };
        let mut keys = Vec::with_capacity(effective_order.len());
        for (expression, _) in &effective_order {
            keys.push(eval::eval_group(&schema, &group, expression)?);
        }
        budget.consume(
            row_work_units(&row)
                .saturating_add(keys.iter().map(value_work_units).sum())
                .saturating_add(1),
            "materializing a result row",
        )?;
        projected.push((row, keys));
    }

    if !effective_order.is_empty() {
        budget.consume(
            projected.len().saturating_mul(effective_order.len().max(1)),
            "sorting result rows",
        )?;
        projected.sort_by(|left, right| {
            for (index, (_, ascending)) in effective_order.iter().enumerate() {
                let mut ordering = left.1[index].cmp_value(&right.1[index]);
                if !*ascending {
                    ordering = ordering.reverse();
                }
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            Ordering::Equal
        });
    }
    let mut rows: Vec<Row> = projected.into_iter().map(|(row, _)| row).collect();
    if *distinct {
        let mut seen = Vec::new();
        let mut distinct_rows = Vec::new();
        for row in rows {
            budget.consume(
                seen.len()
                    .saturating_add(row_work_units(&row))
                    .saturating_add(1),
                "deduplicating result rows",
            )?;
            if !seen.contains(&row) {
                seen.push(row.clone());
                distinct_rows.push(row);
            }
        }
        rows = distinct_rows;
    }
    if let Some(offset) = offset {
        if *offset >= rows.len() as u64 {
            rows.clear();
        } else {
            rows = rows.split_off(*offset as usize);
        }
    }
    if let Some(limit) = limit {
        rows.truncate(*limit as usize);
    }
    Ok(StatementResult::Select {
        columns: output_names,
        rows,
    })
}

fn input_schema_width(schema: &[ColumnBinding]) -> usize {
    schema.len()
}

fn join_passes(
    kind: &JoinKind,
    on: &Option<Expr>,
    schema: &[ColumnBinding],
    row: &Row,
) -> Result<bool, DbError> {
    match (kind, on) {
        (JoinKind::Cross, _) => Ok(true),
        (_, Some(on)) => eval::where_matches_with_schema(schema, row, on),
        (_, None) => Ok(true),
    }
}

fn make_groups(
    schema: &[ColumnBinding],
    rows: Vec<Row>,
    group_by: &[Expr],
    grouped: bool,
    budget: &mut ExecutionBudget,
) -> Result<Vec<Vec<Row>>, DbError> {
    if !grouped {
        return Ok(rows.into_iter().map(|row| vec![row]).collect());
    }
    if group_by.is_empty() {
        return Ok(vec![rows]);
    }
    let mut groups: Vec<(Vec<Value>, Vec<Row>)> = Vec::new();
    for row in rows {
        budget.row(&row, "evaluating group keys")?;
        let key = group_by
            .iter()
            .map(|expr| eval::eval_with_schema(schema, &row, expr))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some((_, values)) = groups.iter_mut().find(|(existing, _)| {
            existing.len() == key.len()
                && existing
                    .iter()
                    .zip(&key)
                    .all(|(left, right)| left.cmp_value(right) == Ordering::Equal)
        }) {
            values.push(row);
        } else {
            groups.push((key, vec![row]));
        }
    }
    Ok(groups.into_iter().map(|(_, rows)| rows).collect())
}

fn order_name_expr(name: &str) -> Expr {
    if let Some((relation, column)) = name.split_once('.') {
        Expr::ColumnRef {
            relation: relation.to_string(),
            column: column.to_string(),
        }
    } else {
        Expr::Column(name.to_string())
    }
}

fn resolve_order_alias(expr: Expr, items: &[Expr]) -> Expr {
    let Expr::Column(name) = &expr else {
        return expr;
    };
    for item in items {
        if let Expr::Alias { expr: inner, alias } = item
            && alias.eq_ignore_ascii_case(name)
        {
            return *inner.clone();
        }
    }
    expr
}

fn expand_select_items(schema: &[ColumnBinding], items: &[Expr]) -> Result<Vec<Expr>, DbError> {
    let mut expanded = Vec::new();
    for item in items {
        if let Expr::QualifiedWildcard(relation) = item {
            let mut found = false;
            for column in schema {
                if column
                    .relations
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(relation))
                {
                    found = true;
                    expanded.push(Expr::ColumnRef {
                        relation: relation.clone(),
                        column: column.name.clone(),
                    });
                }
            }
            if !found {
                return Err(dberr(
                    DbErrorKind::UnknownTable,
                    format!("no such table: {relation}"),
                ));
            }
        } else {
            expanded.push(item.clone());
        }
    }
    Ok(expanded)
}

fn select_output_names(
    schema: &[ColumnBinding],
    columns: &SelectItems,
    expanded: &[Expr],
) -> Vec<String> {
    match columns {
        SelectItems::Star => schema.iter().map(|column| column.name.clone()).collect(),
        SelectItems::List(items) => {
            let mut names = Vec::new();
            let mut expanded_index = 0usize;
            for item in items {
                if let Expr::QualifiedWildcard(relation) = item {
                    names.extend(
                        schema
                            .iter()
                            .filter(|column| {
                                column
                                    .relations
                                    .iter()
                                    .any(|value| value.eq_ignore_ascii_case(relation))
                            })
                            .map(|column| column.name.clone()),
                    );
                    expanded_index += names.len().saturating_sub(expanded_index);
                } else if let Some(expr) = expanded.get(expanded_index) {
                    names.push(expr_label(expr));
                    expanded_index += 1;
                }
            }
            names
        }
    }
}

fn expr_label(expr: &Expr) -> String {
    match expr {
        Expr::Column(name) => name.clone(),
        Expr::ColumnRef { relation, column } => format!("{relation}.{column}"),
        Expr::QualifiedWildcard(relation) => format!("{relation}.*"),
        Expr::Alias { alias, .. } => alias.clone(),
        Expr::Function { name, .. } => name.clone(),
        Expr::Literal(value) => value.to_string(),
        _ => "expr".into(),
    }
}

fn exec_update(
    state: &mut State,
    table_name: &str,
    assignments: &[(String, Expr)],
    where_clause: &Option<Expr>,
    budget: &mut ExecutionBudget,
) -> Result<StatementResult, DbError> {
    let targets = {
        let table = state.table(table_name).ok_or_else(|| {
            dberr(
                DbErrorKind::UnknownTable,
                format!("no such table: {table_name}"),
            )
        })?;
        let targets = assignments
            .iter()
            .map(|(name, _)| table.column_index(name))
            .collect::<Result<Vec<_>, _>>()?;
        ensure_unique_columns(&targets, "UPDATE assignment list")?;
        let schema = eval::schema_for_table(table);
        for (_, expr) in assignments {
            reject_aggregate(expr, "UPDATE")?;
            eval::validate_with_schema(&schema, expr)?;
        }
        if let Some(predicate) = where_clause {
            reject_aggregate(predicate, "WHERE")?;
            eval::validate_with_schema(&schema, predicate)?;
        }
        targets
    };
    let table = state.table(table_name).unwrap();
    budget.table_clone(table, "preparing an update")?;
    let mut plan = Vec::new();
    {
        let table = state.table(table_name).unwrap();
        for (rid, row) in table.scan() {
            budget.row(row, "scanning rows for update")?;
            if where_clause
                .as_ref()
                .map(|predicate| eval::where_matches(table, row, predicate))
                .transpose()?
                .unwrap_or(true)
            {
                budget.row(row, "materializing updated rows")?;
                let mut new_row = row.clone();
                for (index, (_, expr)) in assignments.iter().enumerate() {
                    let value = eval::eval(table, row, expr)?;
                    new_row[targets[index]] = table.coerce_val(&value, targets[index])?;
                }
                plan.push((rid, new_row));
            }
        }
    }
    let table = state.table_mut(table_name).unwrap();
    let mut candidate = table.clone();
    let affected = plan.len();
    for (rid, row) in plan {
        candidate.replace_row(rid, row)?;
    }
    *table = candidate;
    Ok(StatementResult::Update {
        rows_affected: affected,
    })
}

fn ensure_unique_columns(indices: &[usize], label: &str) -> Result<(), DbError> {
    let mut seen = HashSet::with_capacity(indices.len());
    if indices.iter().any(|index| !seen.insert(*index)) {
        return Err(dberr(
            DbErrorKind::Constraint,
            format!("{label} contains a duplicate column"),
        ));
    }
    Ok(())
}

fn reject_aggregate(expr: &Expr, context: &str) -> Result<(), DbError> {
    if eval::contains_aggregate(expr) {
        return Err(dberr(
            DbErrorKind::Syntax(format!("aggregate functions are not allowed in {context}")),
            format!("aggregate functions are not allowed in {context}"),
        ));
    }
    Ok(())
}

fn exec_delete(
    state: &mut State,
    table_name: &str,
    where_clause: &Option<Expr>,
    budget: &mut ExecutionBudget,
) -> Result<StatementResult, DbError> {
    let mut ids = Vec::new();
    {
        let table = state.table(table_name).ok_or_else(|| {
            dberr(
                DbErrorKind::UnknownTable,
                format!("no such table: {table_name}"),
            )
        })?;
        let schema = eval::schema_for_table(table);
        if let Some(predicate) = where_clause {
            reject_aggregate(predicate, "WHERE")?;
            eval::validate_with_schema(&schema, predicate)?;
        }
        for (rid, row) in table.scan() {
            budget.row(row, "scanning rows for delete")?;
            if where_clause
                .as_ref()
                .map(|predicate| eval::where_matches(table, row, predicate))
                .transpose()?
                .unwrap_or(true)
            {
                ids.push(rid);
            }
        }
    }
    let table = state.table(table_name).unwrap();
    budget.table_clone(table, "preparing a delete")?;
    let table = state.table_mut(table_name).unwrap();
    let mut candidate = table.clone();
    for rid in &ids {
        candidate.delete_row(*rid)?;
    }
    *table = candidate;
    Ok(StatementResult::Delete {
        rows_affected: ids.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::parser::parse;

    fn state_with_rows(count: i64) -> State {
        let mut state = State::empty();
        let mut table = Table::new(
            "items",
            vec![Column {
                name: "id".into(),
                ty: crate::types::ColumnType::Integer,
                not_null: false,
                unique: false,
                primary_key: false,
            }],
        )
        .unwrap();
        for id in 0..count {
            table.insert_row(vec![Value::Integer(id)]).unwrap();
        }
        state.tables.insert("items".into(), table);
        state
    }

    #[test]
    fn bounded_execution_rejects_materialization_before_result_conversion() {
        let mut state = state_with_rows(10);
        let statement = parse("SELECT * FROM items").unwrap().remove(0);
        let mut budget = ExecutionBudget::bounded(5);

        let error = execute_with_budget(&mut state, &statement, &mut budget).unwrap_err();

        assert_eq!(error.kind, DbErrorKind::Limit);
        assert!(error.message.contains("work limit"));
    }

    #[test]
    fn bounded_mutation_does_not_publish_partial_state() {
        let mut state = state_with_rows(4);
        let statement = parse("UPDATE items SET id = id + 1").unwrap().remove(0);
        let mut budget = ExecutionBudget::bounded(10);

        let error = execute_with_budget(&mut state, &statement, &mut budget).unwrap_err();

        assert_eq!(error.kind, DbErrorKind::Limit);
        let query = parse("SELECT id FROM items ORDER BY id").unwrap().remove(0);
        let StatementResult::Select { rows, .. } = execute(&mut state, &query).unwrap() else {
            panic!("expected select result");
        };
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(0)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)],
                vec![Value::Integer(3)],
            ]
        );
    }
}
