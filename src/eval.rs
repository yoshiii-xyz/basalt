//! Expression evaluation over rows, with SQL three-valued logic.
//!
//! The evaluator works both with a single [`Table`] and with a resolved query
//! schema assembled by the planner for joins. Aggregate expressions are
//! evaluated over a group through [`eval_group`].

use crate::db::{DbError, DbErrorKind, Row, Table, dberr};
use crate::sql::ast::{BinOp, Expr, UnaryOp};
use crate::types::Value;

/// A column visible to a query, including the table names/aliases that qualify
/// it. The position in a schema slice is its position in a joined row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnBinding {
    pub name: String,
    pub relations: Vec<String>,
}

impl ColumnBinding {
    pub fn new(name: impl Into<String>, relation: impl Into<String>) -> ColumnBinding {
        ColumnBinding {
            name: name.into(),
            relations: vec![relation.into()],
        }
    }

    pub fn with_relations(name: impl Into<String>, relations: Vec<String>) -> ColumnBinding {
        ColumnBinding {
            name: name.into(),
            relations,
        }
    }
}

pub fn schema_for_table(table: &Table) -> Vec<ColumnBinding> {
    table
        .columns
        .iter()
        .map(|column| ColumnBinding::new(column.name.clone(), table.name.clone()))
        .collect()
}

/// Evaluate an expression against a single table row.
pub fn eval(table: &Table, row: &Row, expr: &Expr) -> Result<Value, DbError> {
    let schema = schema_for_table(table);
    eval_with_schema(&schema, row, expr)
}

/// Evaluate an expression against a row and a resolved, possibly joined,
/// schema. Unqualified ambiguous columns are rejected.
pub fn eval_with_schema(
    schema: &[ColumnBinding],
    row: &Row,
    expr: &Expr,
) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Column(name) => {
            let index = resolve_column(schema, None, name)?;
            Ok(row[index].clone())
        }
        Expr::QualifiedWildcard(relation) => Err(dberr(
            DbErrorKind::Syntax(format!("{relation}.* is only valid in a SELECT list")),
            format!("{relation}.* is only valid in a SELECT list"),
        )),
        Expr::ColumnRef { relation, column } => {
            let index = resolve_column(schema, Some(relation), column)?;
            Ok(row[index].clone())
        }
        Expr::Alias { expr, .. } => eval_with_schema(schema, row, expr),
        Expr::Function {
            name,
            args,
            distinct: _,
        } => eval_function(schema, row, name, args),
        Expr::IsNull { expr, negated } => {
            let value = eval_with_schema(schema, row, expr)?;
            Ok(Value::Boolean(matches!(value, Value::Null) != *negated))
        }
        Expr::Unary { op, expr } => apply_unary(op, eval_with_schema(schema, row, expr)?),
        Expr::Binary { left, op, right } => apply_binary(
            op,
            eval_with_schema(schema, row, left)?,
            eval_with_schema(schema, row, right)?,
        ),
    }
}

/// Validate names, function names, and function arity without evaluating an
/// expression.  Query execution calls this before scanning so an empty input
/// cannot hide a malformed or unresolved expression.
pub fn validate_with_schema(schema: &[ColumnBinding], expr: &Expr) -> Result<(), DbError> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column(name) => resolve_column(schema, None, name).map(|_| ()),
        Expr::QualifiedWildcard(relation) => Err(dberr(
            DbErrorKind::Syntax(format!("{relation}.* is only valid in a SELECT list")),
            format!("{relation}.* is only valid in a SELECT list"),
        )),
        Expr::ColumnRef { relation, column } => {
            resolve_column(schema, Some(relation), column).map(|_| ())
        }
        Expr::Alias { expr, .. } => validate_with_schema(schema, expr),
        Expr::Function {
            name,
            args,
            distinct,
        } => {
            let upper = name.to_ascii_uppercase();
            if is_aggregate_name(name) {
                match upper.as_str() {
                    "COUNT" if args.len() <= 1 => {}
                    "SUM" | "AVG" | "MIN" | "MAX" if args.len() == 1 => {}
                    "COUNT" => {
                        return Err(dberr(
                            DbErrorKind::Syntax("COUNT expects at most one argument".into()),
                            "COUNT expects at most one argument",
                        ));
                    }
                    _ => {
                        return Err(dberr(
                            DbErrorKind::Syntax(format!("{upper} expects one argument")),
                            format!("{upper} expects one argument"),
                        ));
                    }
                }
                for arg in args {
                    if !matches!(arg, Expr::Column(value) if value == "*") {
                        validate_with_schema(schema, arg)?;
                    }
                }
                return Ok(());
            }
            if *distinct {
                return Err(dberr(
                    DbErrorKind::Syntax("DISTINCT is only valid for aggregate functions".into()),
                    "DISTINCT is only valid for aggregate functions",
                ));
            }
            match upper.as_str() {
                "LOWER" | "UPPER" | "LENGTH" | "ABS" if args.len() == 1 => {}
                "COALESCE" => {}
                "NULLIF" if args.len() == 2 => {}
                "LOWER" | "UPPER" | "LENGTH" | "ABS" => {
                    return Err(dberr(
                        DbErrorKind::Syntax(format!("{upper} expects one argument")),
                        format!("{upper} expects one argument"),
                    ));
                }
                "NULLIF" => {
                    return Err(dberr(
                        DbErrorKind::Syntax("NULLIF expects two arguments".into()),
                        "NULLIF expects two arguments",
                    ));
                }
                _ => {
                    return Err(dberr(
                        DbErrorKind::Syntax(format!("unknown function {name}")),
                        format!("unknown function: {name}"),
                    ));
                }
            }
            for arg in args {
                validate_with_schema(schema, arg)?;
            }
            Ok(())
        }
        Expr::Binary { left, right, .. } => {
            validate_with_schema(schema, left)?;
            validate_with_schema(schema, right)
        }
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => validate_with_schema(schema, expr),
    }
}

fn resolve_column(
    schema: &[ColumnBinding],
    relation: Option<&str>,
    name: &str,
) -> Result<usize, DbError> {
    let mut matches = Vec::new();
    for (index, binding) in schema.iter().enumerate() {
        let name_matches = binding.name.eq_ignore_ascii_case(name);
        let relation_matches = relation
            .map(|wanted| {
                binding
                    .relations
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(wanted))
            })
            .unwrap_or(true);
        if name_matches && relation_matches {
            matches.push(index);
        }
    }
    match matches.as_slice() {
        [index] => Ok(*index),
        [] => {
            let label = relation
                .map(|value| format!("{value}.{name}"))
                .unwrap_or_else(|| name.to_string());
            Err(dberr(
                DbErrorKind::UnknownColumn,
                format!("no such column: {label}"),
            ))
        }
        _ => Err(dberr(
            DbErrorKind::UnknownColumn,
            format!("ambiguous column: {name}"),
        )),
    }
}

fn eval_function(
    schema: &[ColumnBinding],
    row: &Row,
    name: &str,
    args: &[Expr],
) -> Result<Value, DbError> {
    if is_aggregate_name(name) {
        return Err(dberr(
            DbErrorKind::TypeMismatch,
            format!("aggregate function {name} requires a query group"),
        ));
    }
    let values = args
        .iter()
        .map(|arg| eval_with_schema(schema, row, arg))
        .collect::<Result<Vec<_>, _>>()?;
    apply_scalar_function(&name.to_ascii_uppercase(), name, values)
}

fn apply_scalar_function(upper: &str, name: &str, values: Vec<Value>) -> Result<Value, DbError> {
    match upper {
        "LOWER" => match values.as_slice() {
            [Value::Text(value)] => Ok(Value::Text(value.to_lowercase())),
            [Value::Null] => Ok(Value::Null),
            _ => Err(dberr(DbErrorKind::TypeMismatch, "LOWER expects TEXT")),
        },
        "UPPER" => match values.as_slice() {
            [Value::Text(value)] => Ok(Value::Text(value.to_uppercase())),
            [Value::Null] => Ok(Value::Null),
            _ => Err(dberr(DbErrorKind::TypeMismatch, "UPPER expects TEXT")),
        },
        "LENGTH" => match values.as_slice() {
            [Value::Text(value)] => Ok(Value::Integer(value.chars().count() as i64)),
            [Value::Null] => Ok(Value::Null),
            _ => Err(dberr(DbErrorKind::TypeMismatch, "LENGTH expects TEXT")),
        },
        "ABS" => match values.as_slice() {
            [Value::Integer(value)] => value
                .checked_abs()
                .map(Value::Integer)
                .ok_or_else(|| dberr(DbErrorKind::TypeMismatch, "integer overflow")),
            [Value::Real(value)] => Ok(Value::Real(value.abs())),
            [Value::Null] => Ok(Value::Null),
            _ => Err(dberr(DbErrorKind::TypeMismatch, "ABS expects a number")),
        },
        "COALESCE" => Ok(values
            .into_iter()
            .find(|value| !matches!(value, Value::Null))
            .unwrap_or(Value::Null)),
        "NULLIF" => {
            if values.len() != 2 {
                return Err(dberr(
                    DbErrorKind::Syntax("NULLIF expects two arguments".into()),
                    "NULLIF expects two arguments",
                ));
            }
            if matches!(values[0], Value::Null) || matches!(values[1], Value::Null) {
                Ok(values[0].clone())
            } else if values[0].cmp_value(&values[1]) == std::cmp::Ordering::Equal {
                Ok(Value::Null)
            } else {
                Ok(values[0].clone())
            }
        }
        _ => Err(dberr(
            DbErrorKind::Syntax(format!("unknown function {name}")),
            format!("unknown function: {name}"),
        )),
    }
}

fn apply_unary(op: &UnaryOp, value: Value) -> Result<Value, DbError> {
    match op {
        UnaryOp::Neg => match value {
            Value::Integer(value) => value
                .checked_neg()
                .map(Value::Integer)
                .ok_or_else(|| dberr(DbErrorKind::TypeMismatch, "integer overflow")),
            Value::Real(value) => Ok(Value::Real(-value)),
            Value::Null => Ok(Value::Null),
            _ => Err(dberr(
                DbErrorKind::TypeMismatch,
                "cannot negate non-numeric value",
            )),
        },
        UnaryOp::Not => match value.is_truthy() {
            Some(value) => Ok(Value::Boolean(!value)),
            None => Ok(Value::Null),
        },
    }
}

pub(crate) fn apply_binary(op: &BinOp, left: Value, right: Value) -> Result<Value, DbError> {
    use BinOp::*;
    match op {
        And => {
            let left = left.is_truthy();
            let right = right.is_truthy();
            if left == Some(false) || right == Some(false) {
                Ok(Value::Boolean(false))
            } else if left.is_none() || right.is_none() {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(true))
            }
        }
        Or => {
            let left = left.is_truthy();
            let right = right.is_truthy();
            if left == Some(true) || right == Some(true) {
                Ok(Value::Boolean(true))
            } else if left.is_none() || right.is_none() {
                Ok(Value::Null)
            } else {
                Ok(Value::Boolean(false))
            }
        }
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            if matches!(left, Value::Null) || matches!(right, Value::Null) {
                return Ok(Value::Null);
            }
            let ordering = left.cmp_value(&right);
            let value = match op {
                Eq => ordering == std::cmp::Ordering::Equal,
                NotEq => ordering != std::cmp::Ordering::Equal,
                Lt => ordering == std::cmp::Ordering::Less,
                LtEq => ordering != std::cmp::Ordering::Greater,
                Gt => ordering == std::cmp::Ordering::Greater,
                GtEq => ordering != std::cmp::Ordering::Less,
                _ => unreachable!(),
            };
            Ok(Value::Boolean(value))
        }
        Add | Sub | Mul | Div | Mod => apply_arithmetic(op, left, right),
    }
}

fn apply_arithmetic(op: &BinOp, left: Value, right: Value) -> Result<Value, DbError> {
    use BinOp::*;
    use Value::*;
    match (left, right) {
        (Null, _) | (_, Null) => Ok(Null),
        (Integer(a), Integer(b)) => match op {
            Add => a
                .checked_add(b)
                .map(Integer)
                .ok_or_else(|| dberr(DbErrorKind::TypeMismatch, "integer overflow")),
            Sub => a
                .checked_sub(b)
                .map(Integer)
                .ok_or_else(|| dberr(DbErrorKind::TypeMismatch, "integer overflow")),
            Mul => a
                .checked_mul(b)
                .map(Integer)
                .ok_or_else(|| dberr(DbErrorKind::TypeMismatch, "integer overflow")),
            Div => {
                if b == 0 {
                    Ok(Null)
                } else {
                    a.checked_div(b)
                        .map(Integer)
                        .ok_or_else(|| dberr(DbErrorKind::TypeMismatch, "integer overflow"))
                }
            }
            Mod => {
                if b == 0 {
                    Ok(Null)
                } else {
                    a.checked_rem(b)
                        .map(Integer)
                        .ok_or_else(|| dberr(DbErrorKind::TypeMismatch, "integer overflow"))
                }
            }
            _ => unreachable!(),
        },
        (Integer(a), Real(b)) => float_arithmetic(op, a as f64, b),
        (Real(a), Integer(b)) => float_arithmetic(op, a, b as f64),
        (Real(a), Real(b)) => float_arithmetic(op, a, b),
        _ => Err(dberr(
            DbErrorKind::TypeMismatch,
            "arithmetic on TEXT/BOOLEAN values is not supported",
        )),
    }
}

fn float_arithmetic(op: &BinOp, left: f64, right: f64) -> Result<Value, DbError> {
    use BinOp::*;
    if matches!(op, Div | Mod) && right == 0.0 {
        return Ok(Value::Null);
    }
    Ok(match op {
        Add => Value::Real(left + right),
        Sub => Value::Real(left - right),
        Mul => Value::Real(left * right),
        Div => Value::Real(left / right),
        Mod => Value::Real(left % right),
        _ => unreachable!(),
    })
}

pub fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function { name, args, .. } => {
            is_aggregate_name(name) || args.iter().any(contains_aggregate)
        }
        Expr::Binary { left, right, .. } => contains_aggregate(left) || contains_aggregate(right),
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => contains_aggregate(expr),
        Expr::Literal(_)
        | Expr::Column(_)
        | Expr::QualifiedWildcard(_)
        | Expr::ColumnRef { .. } => false,
        Expr::Alias { expr, .. } => contains_aggregate(expr),
    }
}

fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
    )
}

/// Evaluate an expression over a group of rows. Non-aggregate expressions use
/// the first row; aggregate expressions combine every row.
pub fn eval_group(schema: &[ColumnBinding], rows: &[Row], expr: &Expr) -> Result<Value, DbError> {
    match expr {
        Expr::Function {
            name,
            args,
            distinct,
        } if is_aggregate_name(name) => eval_aggregate(schema, rows, name, args, *distinct),
        Expr::Binary { left, op, right } => apply_binary(
            op,
            eval_group(schema, rows, left)?,
            eval_group(schema, rows, right)?,
        ),
        Expr::Unary { op, expr } => apply_unary(op, eval_group(schema, rows, expr)?),
        Expr::IsNull { expr, negated } => {
            let value = eval_group(schema, rows, expr)?;
            Ok(Value::Boolean(matches!(value, Value::Null) != *negated))
        }
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Column(_) | Expr::ColumnRef { .. } => rows
            .first()
            .map(|row| eval_with_schema(schema, row, expr))
            .unwrap_or_else(|| Ok(Value::Null)),
        Expr::QualifiedWildcard(relation) => Err(dberr(
            DbErrorKind::Syntax(format!("{relation}.* is only valid in a SELECT list")),
            format!("{relation}.* is only valid in a SELECT list"),
        )),
        Expr::Alias { expr, .. } => eval_group(schema, rows, expr),
        Expr::Function { .. } => rows
            .first()
            .map(|row| eval_with_schema(schema, row, expr))
            .unwrap_or_else(|| Ok(Value::Null)),
    }
}

fn eval_aggregate(
    schema: &[ColumnBinding],
    rows: &[Row],
    name: &str,
    args: &[Expr],
    distinct: bool,
) -> Result<Value, DbError> {
    let upper = name.to_ascii_uppercase();
    if upper == "COUNT" {
        if args.is_empty() {
            return Ok(Value::Integer(rows.len() as i64));
        }
        if args.len() != 1 {
            return Err(dberr(
                DbErrorKind::Syntax("COUNT expects at most one argument".into()),
                "COUNT expects at most one argument",
            ));
        }
        if matches!(args.first(), Some(Expr::Column(value)) if value == "*") {
            return Ok(Value::Integer(rows.len() as i64));
        }
        let values = distinct_values(group_values(schema, rows, args.first().unwrap())?, distinct);
        return Ok(Value::Integer(
            values
                .into_iter()
                .filter(|value| !matches!(value, Value::Null))
                .count() as i64,
        ));
    }
    if args.len() != 1 {
        return Err(dberr(
            DbErrorKind::Syntax(format!("{upper} expects one argument")),
            format!("{upper} expects one argument"),
        ));
    }
    let argument = &args[0];
    let values = distinct_values(group_values(schema, rows, argument)?, distinct)
        .into_iter()
        .filter(|value| !matches!(value, Value::Null))
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Ok(Value::Null);
    }
    match upper.as_str() {
        "SUM" => sum_values(values),
        "AVG" => {
            let mut total = 0.0;
            let mut count = 0usize;
            for value in values {
                total += match value {
                    Value::Integer(value) => value as f64,
                    Value::Real(value) => value,
                    _ => return Err(dberr(DbErrorKind::TypeMismatch, "AVG expects numbers")),
                };
                count += 1;
            }
            Ok(Value::Real(total / count as f64))
        }
        "MIN" | "MAX" => {
            let mut result = values[0].clone();
            for value in values.into_iter().skip(1) {
                let ordering = value.cmp_value(&result);
                if (upper == "MIN" && ordering == std::cmp::Ordering::Less)
                    || (upper == "MAX" && ordering == std::cmp::Ordering::Greater)
                {
                    result = value;
                }
            }
            Ok(result)
        }
        _ => unreachable!(),
    }
}

fn group_values(
    schema: &[ColumnBinding],
    rows: &[Row],
    expr: &Expr,
) -> Result<Vec<Value>, DbError> {
    rows.iter()
        .map(|row| eval_with_schema(schema, row, expr))
        .collect()
}

fn distinct_values(values: Vec<Value>, distinct: bool) -> Vec<Value> {
    if !distinct {
        return values;
    }
    let mut result: Vec<Value> = Vec::new();
    for value in values {
        if !result
            .iter()
            .any(|existing| existing.cmp_value(&value) == std::cmp::Ordering::Equal)
        {
            result.push(value);
        }
    }
    result
}

fn sum_values(values: Vec<Value>) -> Result<Value, DbError> {
    let has_real = values.iter().any(|value| matches!(value, Value::Real(_)));
    if has_real {
        let mut total = 0.0;
        for value in values {
            total += match value {
                Value::Integer(value) => value as f64,
                Value::Real(value) => value,
                _ => return Err(dberr(DbErrorKind::TypeMismatch, "SUM expects numbers")),
            };
        }
        Ok(Value::Real(total))
    } else {
        let mut total = 0i64;
        for value in values {
            let Value::Integer(value) = value else {
                return Err(dberr(DbErrorKind::TypeMismatch, "SUM expects numbers"));
            };
            total = total
                .checked_add(value)
                .ok_or_else(|| dberr(DbErrorKind::TypeMismatch, "integer overflow"))?;
        }
        Ok(Value::Integer(total))
    }
}

/// WHERE filter: a row matches only when the predicate is exactly TRUE.
pub fn where_matches(table: &Table, row: &Row, expr: &Expr) -> Result<bool, DbError> {
    Ok(eval(table, row, expr)?.is_truthy() == Some(true))
}

pub fn where_matches_with_schema(
    schema: &[ColumnBinding],
    row: &Row,
    expr: &Expr,
) -> Result<bool, DbError> {
    Ok(eval_with_schema(schema, row, expr)?.is_truthy() == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Column;
    use crate::sql::parser::parse;
    use crate::types::ColumnType as CT;

    fn table() -> Table {
        Table::new(
            "t",
            vec![
                Column {
                    name: "id".into(),
                    ty: CT::Integer,
                    not_null: true,
                    unique: false,
                    primary_key: true,
                },
                Column {
                    name: "score".into(),
                    ty: CT::Real,
                    not_null: false,
                    unique: false,
                    primary_key: false,
                },
                Column {
                    name: "name".into(),
                    ty: CT::Text,
                    not_null: true,
                    unique: false,
                    primary_key: false,
                },
            ],
        )
        .unwrap()
    }

    fn expr(sql: &str) -> Expr {
        match parse(sql).unwrap().into_iter().next().unwrap() {
            crate::sql::ast::Statement::Select {
                where_clause: Some(expr),
                ..
            } => expr,
            other => panic!("need WHERE: {other:?}"),
        }
    }

    fn row() -> Row {
        vec![Value::Integer(1), Value::Null, Value::Text("a".into())]
    }

    #[test]
    fn null_logic_and_comparisons() {
        let table = table();
        assert_eq!(
            eval(&table, &row(), &expr("SELECT * FROM t WHERE score > 5")).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval(
                &table,
                &row(),
                &expr("SELECT * FROM t WHERE id = 2 AND score > 1")
            )
            .unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval(
                &table,
                &row(),
                &expr("SELECT * FROM t WHERE id = 1 OR score > 1")
            )
            .unwrap(),
            Value::Boolean(true)
        );
    }

    #[test]
    fn aggregate_group() {
        let table = table();
        let schema = schema_for_table(&table);
        let rows = vec![
            vec![Value::Integer(1), Value::Real(2.0), Value::Text("a".into())],
            vec![Value::Integer(2), Value::Real(3.0), Value::Text("b".into())],
        ];
        let parsed = parse("SELECT SUM(score) FROM t").unwrap();
        let crate::sql::ast::Statement::Select {
            columns: crate::sql::ast::SelectItems::List(items),
            ..
        } = &parsed[0]
        else {
            panic!()
        };
        assert_eq!(
            eval_group(&schema, &rows, &items[0]).unwrap(),
            Value::Real(5.0)
        );
    }
}
