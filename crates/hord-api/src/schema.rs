//! The published schema (ADR 0024): `hord.proto`'s file descriptor set, and
//! a JSON Schema of its canonical JSON mapping generated from that set.
//!
//! The JSON Schema (draft 2020-12) has one `$defs` entry per message and
//! enum, keyed by full name (`hord.v1.QueueEntry`), following the proto3
//! JSON mapping: fields under their `json_name`, 64-bit integers as decimal
//! strings (integers are accepted too), `bytes` as base64, enums by value
//! name, maps as objects. `x-hord-services` lists every RPC with its input
//! and output message, so a client can be checked against the schema ("the
//! UI calls no RPC outside `hord.proto`", ADR 0024).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use prost::Message;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{
    DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorSet, SourceCodeInfo,
};
use serde_json::{Map, Value, json};

use crate::proto;

/// Serialized `google.protobuf.FileDescriptorSet` of `hord.proto`, with
/// source info (comments).
#[must_use]
pub fn descriptor_set() -> &'static [u8] {
    include_bytes!(concat!(env!("OUT_DIR"), "/hord_descriptor.bin"))
}

/// The JSON Schema generated from [`descriptor_set`]. Built once.
#[must_use]
pub fn json_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let set = FileDescriptorSet::decode(descriptor_set())
            .expect("the build script wrote a valid descriptor set");
        generate(&set)
    })
}

/// The `GetSchema` response: [`descriptor_set`] and [`json_schema`] as text.
#[must_use]
pub fn get_schema_response() -> proto::GetSchemaResponse {
    proto::GetSchemaResponse {
        descriptor_set: descriptor_set().to_vec(),
        json_schema: json_schema().to_string(),
    }
}

/// JSON Schema for every message, enum, and service in `set`.
#[must_use]
pub fn generate(set: &FileDescriptorSet) -> Value {
    let mut defs = Map::new();
    let mut services = Map::new();
    for file in &set.file {
        let package = file.package();
        let docs = Docs::new(file.source_code_info.as_ref());
        for (i, message) in file.message_type.iter().enumerate() {
            message_defs(package, message, &docs, &[4, index(i)], &mut defs);
        }
        for (i, en) in file.enum_type.iter().enumerate() {
            let name = format!("{package}.{}", en.name());
            defs.insert(name, enum_def(en, docs.get(&[5, index(i)])));
        }
        for service in &file.service {
            let mut methods = Map::new();
            for method in &service.method {
                methods.insert(
                    method.name().to_owned(),
                    json!({
                        "input": method.input_type().trim_start_matches('.'),
                        "output": method.output_type().trim_start_matches('.'),
                        "clientStreaming": method.client_streaming(),
                        "serverStreaming": method.server_streaming(),
                    }),
                );
            }
            services.insert(
                format!("{package}.{}", service.name()),
                Value::Object(methods),
            );
        }
    }
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://hord.dev/schema/hord.v1.json",
        "title": "hord.v1",
        "description": "Canonical protobuf JSON mapping of hord.proto (ADR 0024).",
        "$defs": Value::Object(defs),
        "x-hord-services": Value::Object(services),
    })
}

fn index(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

/// Leading comments by source-code-info path.
struct Docs(BTreeMap<Vec<i32>, String>);

impl Docs {
    fn new(info: Option<&SourceCodeInfo>) -> Self {
        let mut out = BTreeMap::new();
        for location in info.map(|i| i.location.as_slice()).unwrap_or_default() {
            let text = location.leading_comments().trim();
            if !text.is_empty() {
                let text = text.lines().map(str::trim).collect::<Vec<_>>().join(" ");
                out.insert(location.path.clone(), text);
            }
        }
        Self(out)
    }

    fn get(&self, path: &[i32]) -> Option<&str> {
        self.0.get(path).map(String::as_str)
    }
}

fn with_description(mut schema: Value, doc: Option<&str>) -> Value {
    if let (Some(doc), Value::Object(map)) = (doc, &mut schema) {
        map.insert("description".into(), Value::String(doc.to_owned()));
    }
    schema
}

fn message_defs(
    scope: &str,
    message: &DescriptorProto,
    docs: &Docs,
    path: &[i32],
    defs: &mut Map<String, Value>,
) {
    let name = format!("{scope}.{}", message.name());
    for (i, nested) in message.nested_type.iter().enumerate() {
        let mut p = path.to_vec();
        p.extend([3, index(i)]);
        message_defs(&name, nested, docs, &p, defs);
    }
    for (i, en) in message.enum_type.iter().enumerate() {
        let mut p = path.to_vec();
        p.extend([4, index(i)]);
        defs.insert(format!("{name}.{}", en.name()), enum_def(en, docs.get(&p)));
    }
    if message.options.as_ref().is_some_and(|o| o.map_entry()) {
        return;
    }
    let mut properties = Map::new();
    for (i, field) in message.field.iter().enumerate() {
        let mut p = path.to_vec();
        p.extend([2, index(i)]);
        let schema = with_description(field_schema(field, message, &name), docs.get(&p));
        let key = field
            .json_name
            .clone()
            .unwrap_or_else(|| field.name().to_owned());
        properties.insert(key, schema);
    }
    let mut def = json!({
        "type": "object",
        "properties": Value::Object(properties),
    });
    // Members of a real oneof (not a proto3 `optional`): at most one is set.
    let real_oneofs: Vec<Vec<String>> = message
        .oneof_decl
        .iter()
        .enumerate()
        .filter_map(|(i, _)| {
            let members: Vec<String> = message
                .field
                .iter()
                .filter(|f| f.oneof_index == Some(index(i)) && !f.proto3_optional())
                .map(|f| f.json_name.clone().unwrap_or_else(|| f.name().to_owned()))
                .collect();
            (!members.is_empty()).then_some(members)
        })
        .collect();
    if let (false, Value::Object(map)) = (real_oneofs.is_empty(), &mut def) {
        let all: Vec<Value> = real_oneofs
            .iter()
            .map(|members| {
                let pairs: Vec<Value> = members
                    .iter()
                    .enumerate()
                    .flat_map(|(i, a)| {
                        members[i + 1..]
                            .iter()
                            .map(move |b| json!({ "not": { "required": [a, b] } }))
                    })
                    .collect();
                json!({ "allOf": pairs })
            })
            .collect();
        map.insert("allOf".into(), Value::Array(all));
    }
    defs.insert(name, with_description(def, docs.get(path)));
}

fn enum_def(en: &EnumDescriptorProto, doc: Option<&str>) -> Value {
    let names: Vec<Value> = en
        .value
        .iter()
        .map(|v| Value::String(v.name().to_owned()))
        .collect();
    with_description(
        json!({ "anyOf": [ { "type": "string", "enum": names }, { "type": "integer" } ] }),
        doc,
    )
}

fn reference(type_name: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{}", type_name.trim_start_matches('.')) })
}

fn scalar(field: &FieldDescriptorProto) -> Value {
    match field.r#type() {
        Type::Double | Type::Float => json!({ "type": ["number", "string"] }),
        Type::Int32 | Type::Uint32 | Type::Sint32 | Type::Fixed32 | Type::Sfixed32 => {
            json!({ "type": "integer" })
        }
        Type::Int64 | Type::Uint64 | Type::Sint64 | Type::Fixed64 | Type::Sfixed64 => {
            json!({ "type": ["string", "integer"], "pattern": "^-?[0-9]+$" })
        }
        Type::Bool => json!({ "type": "boolean" }),
        Type::String => json!({ "type": "string" }),
        Type::Bytes => json!({ "type": "string", "contentEncoding": "base64" }),
        Type::Enum | Type::Message | Type::Group => reference(field.type_name()),
    }
}

fn field_schema(field: &FieldDescriptorProto, message: &DescriptorProto, scope: &str) -> Value {
    if field.label() == Label::Repeated {
        // A map field is a repeated nested `*Entry` message marked map_entry.
        if field.r#type() == Type::Message {
            let entry = field.type_name().rsplit('.').next().unwrap_or_default();
            let expected = format!(".{scope}.{entry}");
            if let Some(nested) = message
                .nested_type
                .iter()
                .find(|n| n.name() == entry && n.options.as_ref().is_some_and(|o| o.map_entry()))
                && field.type_name() == expected
            {
                let value = nested
                    .field
                    .iter()
                    .find(|f| f.number() == 2)
                    .map_or(json!({}), scalar);
                return json!({ "type": "object", "additionalProperties": value });
            }
        }
        return json!({ "type": "array", "items": scalar(field) });
    }
    scalar(field)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_covers_every_message_and_rpc() -> Result<(), Box<dyn std::error::Error>> {
        let schema = json_schema();
        let defs = schema["$defs"].as_object().ok_or("$defs is an object")?;
        for name in [
            "hord.v1.QueueEntry",
            "hord.v1.ConflictReport",
            "hord.v1.Event",
            "hord.v1.EventEnvelope",
            "hord.v1.QueueStatus",
            "hord.v1.Actor",
        ] {
            assert!(defs.contains_key(name), "{name}");
        }
        let entry = &defs["hord.v1.QueueEntry"]["properties"];
        assert_eq!(entry["submittedAtMs"]["pattern"], "^-?[0-9]+$");
        assert_eq!(entry["report"]["$ref"], "#/$defs/hord.v1.ConflictReport");
        let refs = &defs["hord.v1.RefsResponse"]["properties"]["refs"];
        assert_eq!(refs["type"], "object");
        assert_eq!(refs["additionalProperties"]["type"], "string");
        assert!(
            defs["hord.v1.QueueEntry"]["description"]
                .as_str()
                .ok_or("QueueEntry has a string description")?
                .contains("lander queue")
        );
        let rpcs = schema["x-hord-services"]["hord.v1.RepoBackend"]
            .as_object()
            .ok_or("RepoBackend's services are an object")?;
        for rpc in [
            "GetObjects",
            "PutObjects",
            "Has",
            "Head",
            "Log",
            "Refs",
            "Submit",
            "Queue",
            "Arbitrate",
            "NodeHistory",
            "Edges",
            "ResolveName",
            "AttachEvidence",
            "Events",
        ] {
            assert!(rpcs.contains_key(rpc), "{rpc}");
        }
        assert_eq!(rpcs["Events"]["serverStreaming"], true);
        Ok(())
    }

    #[test]
    fn a_oneof_allows_at_most_one_member() -> Result<(), Box<dyn std::error::Error>> {
        let schema = json_schema();
        let defs = schema["$defs"].as_object().ok_or("$defs is an object")?;
        let actor = &defs["hord.v1.Actor"];
        assert_eq!(
            actor["allOf"][0]["allOf"][0]["not"]["required"],
            json!(["human", "agent"])
        );
        // A proto3 `optional` is not a oneof for this purpose.
        assert!(defs["hord.v1.HeadResponse"].get("allOf").is_none());
        Ok(())
    }

    #[test]
    fn json_mapping_matches_the_schema_names() -> Result<(), Box<dyn std::error::Error>> {
        let entry = proto::QueueEntry {
            seq: 7,
            submitted_at_ms: 1,
            status: proto::QueueStatus::Landed.into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&entry)?;
        assert_eq!(json["seq"], "7");
        assert_eq!(json["status"], "QUEUE_STATUS_LANDED");
        let schema = json_schema();
        let props = schema["$defs"]["hord.v1.QueueEntry"]["properties"]
            .as_object()
            .ok_or("QueueEntry's properties are an object")?;
        for key in json
            .as_object()
            .ok_or("a QueueEntry maps to a JSON object")?
            .keys()
        {
            assert!(props.contains_key(key), "{key} is in the schema");
        }
        let back: proto::QueueEntry = serde_json::from_value(json)?;
        assert_eq!(back, entry);
        Ok(())
    }
}
