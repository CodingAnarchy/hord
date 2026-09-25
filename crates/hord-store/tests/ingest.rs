//! Bulk ingest tests and a 10k-put microbench (M0 git-import hot path).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use hord_core::ObjectId;
use hord_store::Store;

fn temp_repo() -> std::io::Result<PathBuf> {
    static N: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hord-store-ingest-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path)?;
    Ok(path)
}

struct Guard(PathBuf);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn payload(i: u32) -> [u8; 8] {
    u64::from(i).to_le_bytes()
}

#[test]
fn many_puts_are_getable_and_pack_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let store = Store::create(&path)?;
    let n = 2_048u32;
    let mut ids = Vec::with_capacity(n as usize);
    for i in 0..n {
        ids.push(store.put(&payload(i))?);
    }
    for (i, id) in ids.iter().enumerate() {
        assert!(store.contains(*id)?, "contains {i}");
        assert_eq!(store.get(*id)?, payload(i as u32));
    }
    let packed = store.pack()?;
    assert_eq!(packed, n as usize);
    for (i, id) in ids.iter().enumerate() {
        assert!(store.contains(*id)?, "packed contains {i}");
        assert_eq!(store.get(*id)?, payload(i as u32));
    }
    assert_eq!(store.pack()?, 0);
    Ok(())
}

#[test]
fn duplicate_puts_are_idempotent_without_rewrite_errors() -> Result<(), Box<dyn std::error::Error>>
{
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let store = Store::create(&path)?;
    let bytes = b"same-blob-many-times";
    let id = store.put(bytes)?;
    for _ in 0..1_000 {
        assert_eq!(store.put(bytes)?, id);
    }
    assert_eq!(store.get(id)?, bytes);
    Ok(())
}

#[test]
fn set_ref_survives_drop_and_reopen() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let id;
    {
        let store = Store::create(&path)?;
        id = store.put(b"ref-target")?;
        store.set_ref("git/modes/example", id)?;
        store.set_ref("main", id)?;
    }
    let store = Store::open(&path)?;
    assert_eq!(store.get_ref("main")?, Some(id));
    assert_eq!(store.get_ref("git/modes/example")?, Some(id));
    Ok(())
}

#[test]
fn ingest_microbench_10k_puts() -> Result<(), Box<dyn std::error::Error>> {
    let path = temp_repo()?;
    let _g = Guard(path.clone());
    let store = Store::create(&path)?;
    let n = 10_000u32;

    let start = Instant::now();
    let mut ids = Vec::with_capacity(n as usize);
    for i in 0..n {
        ids.push(store.put(&payload(i))?);
    }
    let put_elapsed = start.elapsed();

    let start = Instant::now();
    for i in 0..n {
        let _ = store.put(&payload(i))?;
    }
    let dup_elapsed = start.elapsed();

    let start = Instant::now();
    for (i, id) in ids.iter().enumerate() {
        store.set_ref(&format!("git/modes/{i}"), *id)?;
    }
    store.append_log(ids[0])?;
    let ref_elapsed = start.elapsed();

    let start = Instant::now();
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(store.get(*id)?, payload(i as u32));
    }
    let get_elapsed = start.elapsed();

    let start = Instant::now();
    let packed = store.pack()?;
    let pack_elapsed = start.elapsed();

    let start = Instant::now();
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(store.get(*id)?, payload(i as u32));
    }
    let packed_get_elapsed = start.elapsed();

    let puts_per_s = f64::from(n) / put_elapsed.as_secs_f64();
    let refs_per_s = f64::from(n) / ref_elapsed.as_secs_f64();
    eprintln!(
        "hord-store ingest microbench n={n}: unique_put={put_elapsed:?} ({puts_per_s:.0}/s) \
         dup_put={dup_elapsed:?} set_ref+append_log={ref_elapsed:?} ({refs_per_s:.0}/s) \
         get_loose={get_elapsed:?} pack={pack_elapsed:?} packed={packed} \
         get_packed={packed_get_elapsed:?}"
    );
    assert_eq!(ids.len(), n as usize);
    assert_ne!(ids[0], ids[1]);
    assert_eq!(ObjectId::from_canonical(&payload(0)), ids[0]);
    assert_eq!(packed, n as usize);
    assert_eq!(
        store.get_ref("git/modes/0")?,
        Some(ids[0]),
        "set_ref must be readable after append_log flush"
    );
    // After ingest, an explicit pack must leave every object readable. A second
    // pack is a no-op (all remaining loose objects were included).
    assert_eq!(store.pack()?, 0);
    Ok(())
}
