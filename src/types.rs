/// Basalt's scalar value system.
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Boolean(bool),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Integer(_) => "INTEGER",
            Value::Real(_) => "REAL",
            Value::Text(_) => "TEXT",
            Value::Boolean(_) => "BOOLEAN",
        }
    }

    /// Total ordering used by comparisons and B-trees. NULL sorts lowest.
    pub fn cmp_value(&self, other: &Value) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        fn rank(v: &Value) -> u8 {
            match v {
                Value::Null => 0,
                Value::Boolean(_) => 1,
                Value::Integer(_) | Value::Real(_) => 2,
                Value::Text(_) => 3,
            }
        }
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
            (Value::Integer(a), Value::Integer(b)) => a.cmp(b),
            (Value::Real(a), Value::Real(b)) => cmp_real(*a, *b),
            (Value::Integer(a), Value::Real(b)) => cmp_real(*a as f64, *b),
            (Value::Real(a), Value::Integer(b)) => cmp_real(*a, *b as f64),
            (Value::Text(a), Value::Text(b)) => a.cmp(b),
            _ => rank(self).cmp(&rank(other)),
        }
    }

    /// Truthiness per SQL-ish rules: nonzero numbers and their own values.
    pub fn is_truthy(&self) -> Option<bool> {
        match self {
            Value::Null => None,
            Value::Boolean(b) => Some(*b),
            Value::Integer(i) => Some(*i != 0),
            Value::Real(f) => Some(*f != 0.0),
            Value::Text(_) => None,
        }
    }

    pub fn coerce_to(&self, ty: &ColumnType) -> Result<Value, String> {
        match (ty, self) {
            (ColumnType::Null | ColumnType::Any, _) => Ok(self.clone()),
            (ColumnType::Integer, Value::Integer(_)) | (ColumnType::Integer, Value::Null) => {
                Ok(self.clone())
            }
            (ColumnType::Integer, Value::Boolean(b)) => Ok(Value::Integer(*b as i64)),
            (ColumnType::Integer, Value::Real(f)) => {
                if !f.is_finite() || *f < i64::MIN as f64 || *f >= i64::MAX as f64 {
                    Err("cannot convert non-finite or out-of-range REAL to INTEGER".to_string())
                } else {
                    Ok(Value::Integer(*f as i64))
                }
            }
            (ColumnType::Integer, Value::Text(t)) => t
                .parse::<i64>()
                .map(Value::Integer)
                .map_err(|_| format!("cannot convert {t:?} to INTEGER")),
            (ColumnType::Real, Value::Real(f)) if f.is_finite() => Ok(self.clone()),
            (ColumnType::Real, Value::Real(_)) => Err("cannot store a non-finite REAL".to_string()),
            (ColumnType::Real, Value::Null) => Ok(self.clone()),
            (ColumnType::Real, Value::Integer(i)) => Ok(Value::Real(*i as f64)),
            (ColumnType::Real, Value::Boolean(_)) => {
                Err("cannot convert BOOLEAN to REAL".to_string())
            }
            (ColumnType::Real, Value::Text(t)) => {
                let value = t
                    .parse::<f64>()
                    .map_err(|_| format!("cannot convert {t:?} to REAL"))?;
                if value.is_finite() {
                    Ok(Value::Real(value))
                } else {
                    Err(format!("cannot convert {t:?} to REAL"))
                }
            }
            (ColumnType::Text, Value::Text(_)) | (ColumnType::Text, Value::Null) => {
                Ok(self.clone())
            }
            (ColumnType::Text, v) => Ok(Value::Text(v.to_string())),
            (ColumnType::Boolean, Value::Boolean(_)) | (ColumnType::Boolean, Value::Null) => {
                Ok(self.clone())
            }
            (ColumnType::Boolean, Value::Integer(i)) => Ok(Value::Boolean(*i != 0)),
            (ColumnType::Boolean, Value::Real(_)) | (ColumnType::Boolean, Value::Text(_)) => {
                Err(format!("cannot convert {} to BOOLEAN", self.type_name()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_finite_and_out_of_range_real_conversions() {
        assert!(
            Value::Real(f64::INFINITY)
                .coerce_to(&ColumnType::Real)
                .is_err()
        );
        assert!(
            Value::Text("NaN".into())
                .coerce_to(&ColumnType::Real)
                .is_err()
        );
        assert!(
            Value::Real(i64::MAX as f64)
                .coerce_to(&ColumnType::Integer)
                .is_err()
        );
        assert_eq!(
            Value::Real(i64::MIN as f64)
                .coerce_to(&ColumnType::Integer)
                .unwrap(),
            Value::Integer(i64::MIN)
        );
    }
}

fn cmp_real(left: f64, right: f64) -> std::cmp::Ordering {
    left.partial_cmp(&right)
        .unwrap_or_else(|| left.total_cmp(&right))
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Integer(i) => write!(f, "{i}"),
            Value::Real(x) => write!(f, "{x}"),
            Value::Text(s) => write!(f, "'{s}'"),
            Value::Boolean(b) => write!(f, "{b}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnType {
    Integer,
    Real,
    Text,
    Boolean,
    /// Untyped (expression results)
    Any,
    /// Internal: NULL literal type
    Null,
}

impl ColumnType {
    pub fn parse(name: &str) -> Option<ColumnType> {
        match name.to_uppercase().as_str() {
            "INTEGER" | "INT" | "BIGINT" | "SMALLINT" => Some(ColumnType::Integer),
            "REAL" | "FLOAT" | "DOUBLE" => Some(ColumnType::Real),
            "TEXT" | "VARCHAR" | "CHAR" | "STRING" => Some(ColumnType::Text),
            "BOOLEAN" | "BOOL" => Some(ColumnType::Boolean),
            _ => None,
        }
    }
}
