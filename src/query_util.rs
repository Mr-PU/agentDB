use serde_json::{json, Map, Value};
use sqlx::postgres::PgRow;
use sqlx::{Column, Row, TypeInfo};

/// Best-effort conversion of an arbitrary query row into a JSON object,
/// for the ad-hoc /query endpoint. Not exhaustive over every Postgres
/// type — extend as real query patterns surface during Phase 1 testing.
pub fn row_to_json(row: &PgRow) -> Value {
    let mut map = Map::new();

    for col in row.columns() {
        let name = col.name();
        let type_name = col.type_info().name();

        let value: Value = match type_name {
            "INT4" => row
                .try_get::<Option<i32>, _>(name)
                .ok()
                .flatten()
                .map(Value::from)
                .unwrap_or(Value::Null),
            "INT8" => row
                .try_get::<Option<i64>, _>(name)
                .ok()
                .flatten()
                .map(Value::from)
                .unwrap_or(Value::Null),
            "FLOAT4" | "FLOAT8" => row
                .try_get::<Option<f64>, _>(name)
                .ok()
                .flatten()
                .map(Value::from)
                .unwrap_or(Value::Null),
            "BOOL" => row
                .try_get::<Option<bool>, _>(name)
                .ok()
                .flatten()
                .map(Value::from)
                .unwrap_or(Value::Null),
            "UUID" => row
                .try_get::<Option<uuid::Uuid>, _>(name)
                .ok()
                .flatten()
                .map(|v| Value::String(v.to_string()))
                .unwrap_or(Value::Null),
            "TIMESTAMPTZ" | "TIMESTAMP" => row
                .try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(name)
                .ok()
                .flatten()
                .map(|v| Value::String(v.to_rfc3339()))
                .unwrap_or(Value::Null),
            "JSONB" | "JSON" => row
                .try_get::<Option<Value>, _>(name)
                .ok()
                .flatten()
                .unwrap_or(Value::Null),
            _ => row
                .try_get::<Option<String>, _>(name)
                .ok()
                .flatten()
                .map(Value::from)
                .unwrap_or(Value::Null),
        };

        map.insert(name.to_string(), value);
    }

    json!(map)
}
