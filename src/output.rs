use crate::error::QghError;
use serde_json::{json, Value};

pub fn success_envelope_with_meta(data: Value, meta: Value) -> Value {
    json!({
        "schema_version": "qgh.v1",
        "ok": true,
        "data": data,
        "warnings": [],
        "meta": meta
    })
}

pub fn error_envelope(error: &QghError) -> Value {
    json!({
        "schema_version": "qgh.v1",
        "ok": false,
        "error": error,
        "warnings": [],
        "meta": {}
    })
}

pub fn print_success(data: Value, meta: Value) {
    let envelope = success_envelope_with_meta(data, meta);
    println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
}

pub fn print_error(error: &QghError, json_mode: bool) {
    if json_mode {
        let envelope = error_envelope(error);
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    } else {
        eprintln!("{}: {}", error.code, error.message);
        if let Some(hint) = &error.hint {
            eprintln!("hint: {hint}");
        }
    }
}
