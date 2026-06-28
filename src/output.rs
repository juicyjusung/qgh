use crate::error::QghError;
use serde_json::{json, Value};

pub fn print_success(data: Value) {
    let envelope = json!({
        "schema_version": "qgh.v1",
        "ok": true,
        "data": data,
        "warnings": [],
        "meta": {}
    });
    println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
}

pub fn print_error(error: &QghError, json_mode: bool) {
    if json_mode {
        let envelope = json!({
            "schema_version": "qgh.v1",
            "ok": false,
            "error": error,
            "warnings": [],
            "meta": {}
        });
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    } else {
        eprintln!("{}: {}", error.code, error.message);
    }
}
