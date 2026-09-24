//! Golden canonical CBOR and ObjectId fixtures for Blob and Snapshot.

use std::fs;
use std::path::PathBuf;

use hord_core::{Blob, Bytes, IndexPointers, ObjectId, Snapshot, SnapshotMetadata};
use hord_encoding::{decode, encode};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Vectors {
    blob: Fixture,
    snapshot: Fixture,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    name: String,
    hex: String,
    object_id: String,
}

fn vectors() -> Result<Vectors, Box<dyn std::error::Error>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/vectors.json");
    let json = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(serde_json::from_str(&json).map_err(|e| format!("parse testdata/vectors.json: {e}"))?)
}

fn sample_blob() -> Blob {
    Blob::new(Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]))
}

fn sample_snapshot() -> Snapshot {
    Snapshot {
        tree: ObjectId::from_bytes([0x11; ObjectId::LEN]),
        metadata: SnapshotMetadata { toolchain: None },
        index: IndexPointers {
            identity: None,
            edges: None,
        },
    }
}

fn assert_fixture<T>(
    name: &str,
    value: &T,
    fixture: &Fixture,
) -> Result<(), Box<dyn std::error::Error>>
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let bytes = encode(value).map_err(|e| format!("{name}: encode: {e}"))?;
    let got_hex = hex::encode(&bytes);
    assert_eq!(
        got_hex, fixture.hex,
        "vector {name} ({}) encoding mismatch\n  got  {got_hex}\n  want {}",
        fixture.name, fixture.hex
    );
    let id = ObjectId::of(value)?;
    assert_eq!(id, ObjectId::from_canonical(&bytes));
    assert_eq!(
        id.to_hex(),
        fixture.object_id,
        "ObjectId of fixture {} changed",
        fixture.name
    );
    let parsed: ObjectId = fixture.object_id.parse()?;
    assert_eq!(id, parsed);
    let back: T = decode(&bytes)?;
    assert_eq!(&back, value);
    Ok(())
}

#[test]
fn blob_golden() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = vectors()?.blob;
    let blob = sample_blob();
    assert_fixture("blob", &blob, &fixture)?;
    let encoded = encode(&blob)?;
    assert_eq!(encoded[0], 0xa1, "one-element map");
    let hex = hex::encode(&encoded);
    assert!(
        hex.ends_with("44deadbeef"),
        "payload must be CBOR bstr of deadbeef, got {hex}"
    );
    Ok(())
}

#[test]
fn snapshot_golden() -> Result<(), Box<dyn std::error::Error>> {
    assert_fixture("snapshot", &sample_snapshot(), &vectors()?.snapshot)
}
