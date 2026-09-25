//! Golden vectors for RFC 8949 §4.2.1 canonical CBOR and ObjectId.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use cbor2::Value;
use hord_encoding::{ObjectId, decode, encode};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Vectors {
    integers: Vec<IntegerVector>,
    bytes: Vec<BytesVector>,
    text: Vec<TextVector>,
    arrays: Vec<NamedHex>,
    maps: Vec<NamedHex>,
    object_id_fixture: ObjectIdFixture,
}

#[derive(Debug, Deserialize)]
struct IntegerVector {
    name: String,
    #[serde(default)]
    i: Option<serde_json::Value>,
    #[serde(default)]
    u: Option<String>,
    hex: String,
}

#[derive(Debug, Deserialize)]
struct BytesVector {
    name: String,
    bytes: String,
    hex: String,
}

#[derive(Debug, Deserialize)]
struct TextVector {
    name: String,
    text: String,
    hex: String,
}

#[derive(Debug, Deserialize)]
struct NamedHex {
    name: String,
    hex: String,
}

#[derive(Debug, Deserialize)]
struct ObjectIdFixture {
    name: String,
    hex: String,
    object_id: String,
}

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn vectors() -> TestResult<Vectors> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/vectors.json");
    let json = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(serde_json::from_str(&json).map_err(|e| format!("parse testdata/vectors.json: {e}"))?)
}

fn assert_hex(name: &str, got: &[u8], expected_hex: &str) {
    let got_hex = hex::encode(got);
    assert_eq!(
        got_hex, expected_hex,
        "vector {name}: encoding mismatch\n  got  {got_hex}\n  want {expected_hex}"
    );
}

fn integer_value(v: &IntegerVector) -> TestResult<Value> {
    if let Some(u) = &v.u {
        let n: u64 = u
            .parse()
            .map_err(|e| format!("{}: parse u {u}: {e}", v.name))?;
        return Ok(Value::from(n));
    }
    let i =
        v.i.as_ref()
            .ok_or_else(|| format!("{}: missing i or u", v.name))?;
    let value = match i {
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Value::from(u)
            } else if let Some(s) = n.as_i64() {
                Value::from(s)
            } else {
                return Err(format!("{}: number {n} does not fit i64/u64", v.name).into());
            }
        }
        serde_json::Value::String(s) => {
            if let Ok(n) = s.parse::<u64>() {
                Value::from(n)
            } else if let Ok(n) = s.parse::<i64>() {
                Value::from(n)
            } else if let Ok(n) = s.parse::<i128>() {
                Value::from(n)
            } else {
                return Err(format!("{}: cannot parse integer {s}", v.name).into());
            }
        }
        other => return Err(format!("{}: unexpected integer JSON {other}", v.name).into()),
    };
    Ok(value)
}

#[test]
fn integers_preferred_serialization_boundaries() -> TestResult {
    for v in vectors()?.integers {
        let value = integer_value(&v)?;
        let bytes = encode(&value).map_err(|e| format!("{}: encode: {e}", v.name))?;
        assert_hex(&v.name, &bytes, &v.hex);
        let back: Value = decode(&bytes).map_err(|e| format!("{}: decode: {e}", v.name))?;
        assert_eq!(back, value, "vector {}", v.name);
    }
    Ok(())
}

#[test]
fn byte_strings() -> TestResult {
    for v in vectors()?.bytes {
        let payload = hex::decode(&v.bytes).map_err(|e| format!("{}: {e}", v.name))?;
        let value = Value::Bytes(payload);
        let bytes = encode(&value).map_err(|e| format!("{}: encode: {e}", v.name))?;
        assert_hex(&v.name, &bytes, &v.hex);
        let back: Value = decode(&bytes)?;
        assert_eq!(back, value, "vector {}", v.name);
    }
    Ok(())
}

#[test]
fn text_strings() -> TestResult {
    for v in vectors()?.text {
        let value = Value::Text(v.text.clone());
        let bytes = encode(&value).map_err(|e| format!("{}: encode: {e}", v.name))?;
        assert_hex(&v.name, &bytes, &v.hex);
        let back: Value = decode(&bytes)?;
        assert_eq!(back, value, "vector {}", v.name);
    }
    Ok(())
}

#[test]
fn arrays() -> TestResult {
    let vectors = vectors()?;
    let empty = Value::Array(vec![]);
    let one_two_three = Value::Array(vec![
        Value::from(1u64),
        Value::from(2u64),
        Value::from(3u64),
    ]);
    let nested = Value::Array(vec![
        Value::from(1u64),
        Value::Array(vec![Value::from(2u64), Value::from(3u64)]),
        Value::Array(vec![Value::from(4u64), Value::from(5u64)]),
    ]);
    let cases = [("empty", empty), ("123", one_two_three), ("nested", nested)];
    for expected in &vectors.arrays {
        let (_, value) = cases
            .iter()
            .find(|(n, _)| *n == expected.name)
            .ok_or_else(|| format!("missing array constructor {}", expected.name))?;
        let bytes = encode(value)?;
        assert_hex(&expected.name, &bytes, &expected.hex);
        assert_eq!(decode::<Value>(&bytes)?, *value);
    }
    Ok(())
}

#[test]
fn unsorted_maps_encode_with_bytewise_sorted_keys() -> TestResult {
    let vectors = vectors()?;

    let empty = Value::Map(vec![]);

    // Insertion order z, aa, b — canonical order is b, z, aa.
    let unsorted_text = Value::Map(vec![
        (Value::from("z"), Value::from(1u64)),
        (Value::from("aa"), Value::from(2u64)),
        (Value::from("b"), Value::from(3u64)),
    ]);

    // RFC 8949 §4.2.1 example keys, inserted in reverse of canonical order.
    let rfc_keys = Value::Map(vec![
        (Value::Bool(false), Value::from(0u64)),
        (Value::Array(vec![Value::from(-1i64)]), Value::from(0u64)),
        (Value::Array(vec![Value::from(100u64)]), Value::from(0u64)),
        (Value::from("aa"), Value::from(0u64)),
        (Value::from("z"), Value::from(0u64)),
        (Value::from(-1i64), Value::from(0u64)),
        (Value::from(100u64), Value::from(0u64)),
        (Value::from(10u64), Value::from(0u64)),
    ]);

    // Outer inserted b then a; inner inserted z then aa.
    let nested = Value::Map(vec![
        (
            Value::from("b"),
            Value::Map(vec![
                (Value::from("z"), Value::from(1u64)),
                (Value::from("aa"), Value::from(2u64)),
            ]),
        ),
        (Value::from("a"), Value::from(0u64)),
    ]);

    let cases = [
        ("empty", empty),
        ("unsorted-text-keys", unsorted_text),
        ("rfc8949-4.2.1-key-order", rfc_keys),
        ("nested-maps", nested),
    ];
    for expected in &vectors.maps {
        let (_, value) = cases
            .iter()
            .find(|(n, _)| *n == expected.name)
            .ok_or_else(|| format!("missing map constructor {}", expected.name))?;
        let bytes = encode(value)?;
        assert_hex(&expected.name, &bytes, &expected.hex);
        let back: Value = decode(&bytes)?;
        assert_eq!(encode(&back)?, bytes, "re-encode {}", expected.name);
    }
    Ok(())
}

#[test]
fn hashmap_and_value_agree_on_unsorted_text_keys() -> TestResult {
    let mut map = HashMap::new();
    map.insert("z", 1i64);
    map.insert("aa", 2);
    map.insert("b", 3);
    assert_eq!(hex::encode(encode(&map)?), "a3616203617a0162616102");
    Ok(())
}

#[test]
fn object_id_of_known_fixture() -> TestResult {
    let fixture = vectors()?.object_id_fixture;
    let value = Value::Map(vec![
        (Value::from("kind"), Value::from("blob")),
        (Value::from("bytes"), Value::Bytes(hex::decode("deadbeef")?)),
    ]);
    let bytes = encode(&value)?;
    assert_hex(&fixture.name, &bytes, &fixture.hex);
    let id = ObjectId::of(&value)?;
    assert_eq!(id, ObjectId::from_canonical(&bytes));
    assert_eq!(
        id.to_hex(),
        fixture.object_id,
        "ObjectId of fixture {} changed",
        fixture.name
    );
    let parsed: ObjectId = fixture.object_id.parse()?;
    assert_eq!(id, parsed);
    Ok(())
}
