//! Phase 1d-integrate tests: lazy snapshot + delta + LRU cache on ExactIndex.

use std::collections::{HashMap, HashSet};
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use roaring::RoaringBitmap;

use bittice::core::storage::exact_index::ExactIndex;
use bittice::core::storage::exact_index_v3::{write_exact_index_v3, SnapshotReader};

fn bm(values: &[u32]) -> RoaringBitmap {
    let mut b = RoaringBitmap::new();
    for &v in values {
        b.insert(v);
    }
    b
}

fn value_key(i: usize) -> String {
    format!("value_{:04}", i)
}

fn write_v3_snapshot(path: &Path, count: usize) {
    let mut entries: Vec<(u128, Vec<(u64, RoaringBitmap)>)> = (0..count)
        .map(|i| {
            (
                ExactIndex::hash(&value_key(i)),
                vec![(i as u64 + 1, bm(&[i as u32]))],
            )
        })
        .collect();
    entries.sort_unstable_by_key(|(hash, _)| *hash);
    write_exact_index_v3(path, entries).expect("write v3 snapshot");
}

fn write_v3_entries(path: &Path, mut entries: Vec<(u128, Vec<(u64, RoaringBitmap)>)>) {
    entries.sort_unstable_by_key(|(hash, _)| *hash);
    write_exact_index_v3(path, entries).expect("write v3 entries");
}

fn hashes_present(reader: &SnapshotReader, labels: &[&str]) -> Vec<bool> {
    labels
        .iter()
        .map(|label| {
            reader
                .get(ExactIndex::hash(label))
                .expect("get")
                .is_some()
        })
        .collect()
}

#[test]
fn lazy_get_only_loads_one_bitmap() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lazy_a.idx");
    write_v3_snapshot(&path, 1000);

    let idx = ExactIndex::open(&path).expect("open");
    assert_eq!(idx.delta_len(), 0);

    let target = value_key(42);
    assert!(idx.get(&target).is_some());

    assert_eq!(idx.cache_len(), 1, "exactly one entry should be cached");
    assert_eq!(idx.delta_len(), 0, "read path must not touch delta");
}

#[test]
fn lazy_get_repeated_uses_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lazy_b.idx");
    write_v3_snapshot(&path, 1000);

    let idx = ExactIndex::open(&path).expect("open");
    let target = value_key(7);
    for _ in 0..10 {
        assert!(idx.get(&target).is_some());
    }
    assert_eq!(idx.cache_len(), 1);
}

#[test]
fn lazy_get_distinct_values_grow_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lazy_c.idx");
    write_v3_snapshot(&path, 1000);

    let idx = ExactIndex::open(&path).expect("open");
    for i in [1, 10, 100, 500, 999] {
        assert!(idx.get(&value_key(i)).is_some());
    }
    assert_eq!(idx.cache_len(), 5);
}

#[test]
fn lazy_cache_eviction_at_limit() {
    let prev = std::env::var("BITTICE_EXACT_INDEX_CACHE_PER_FIELD").ok();
    std::env::set_var("BITTICE_EXACT_INDEX_CACHE_PER_FIELD", "3");

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lazy_d.idx");
    write_v3_snapshot(&path, 100);

    let idx = ExactIndex::open(&path).expect("open");
    for i in 0..5 {
        let _ = idx.get(&value_key(i));
    }
    assert_eq!(idx.cache_len(), 3, "LRU should cap at 3 entries");

    if let Some(v) = prev {
        std::env::set_var("BITTICE_EXACT_INDEX_CACHE_PER_FIELD", v);
    } else {
        std::env::remove_var("BITTICE_EXACT_INDEX_CACHE_PER_FIELD");
    }
}

#[test]
fn lazy_insert_invalidates_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lazy_e.idx");
    write_v3_entries(&path, vec![(ExactIndex::hash("x"), vec![(1u64, bm(&[1]))])]);

    let mut idx = ExactIndex::open(&path).expect("open");
    let old = idx.get("x").expect("seed cache");
    assert_eq!(old[0].1, bm(&[1]));

    let new_bm = vec![(99u64, bm(&[42]))];
    idx.insert("x", new_bm.clone());
    let got = idx.get("x").expect("after insert");
    assert_eq!(got.as_ref(), &new_bm);
    // Delta hits are served without populating the LRU cache.
    assert_eq!(idx.cache_len(), 0);
}

#[test]
fn lazy_merge_segment_invalidates_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lazy_f.idx");
    write_v3_entries(&path, vec![(ExactIndex::hash("x"), vec![(1u64, bm(&[1]))])]);

    let mut idx = ExactIndex::open(&path).expect("open");
    let cached = idx.get("x").expect("load cache");
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].0, 1);

    let mut seg2 = HashMap::new();
    seg2.insert("x".to_string(), bm(&[2]));
    idx.merge_segment(2, seg2);

    let merged = idx.get("x").expect("merged");
    assert_eq!(merged.len(), 2);
    let seg_ids: Vec<u64> = merged.iter().map(|(s, _)| *s).collect();
    assert!(seg_ids.contains(&1));
    assert!(seg_ids.contains(&2));
}

#[test]
fn lazy_purge_invalidates_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lazy_g.idx");
    write_v3_entries(
        &path,
        vec![
            (ExactIndex::hash("x"), vec![(1u64, bm(&[1]))]),
            (ExactIndex::hash("y"), vec![(1u64, bm(&[2]))]),
        ],
    );

    let mut idx = ExactIndex::open(&path).expect("open");
    assert!(idx.get("x").is_some());
    assert!(idx.get("y").is_some());
    assert_eq!(idx.cache_len(), 2);

    let removed: HashSet<u64> = [1u64].into_iter().collect();
    idx.purge_segments(&removed);

    assert!(idx.get("x").is_none());
    assert!(idx.get("y").is_none());
    assert_eq!(idx.cache_len(), 0);
}

#[test]
fn lazy_save_streaming_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lazy_h.idx");

    write_v3_entries(
        &path,
        vec![
            (ExactIndex::hash("a"), vec![(10, bm(&[1]))]),
            (ExactIndex::hash("c"), vec![(10, bm(&[10]))]),
            (ExactIndex::hash("e"), vec![(20, bm(&[100]))]),
            (ExactIndex::hash("g"), vec![(10, bm(&[1000]))]),
        ],
    );

    let mut idx = ExactIndex::open(&path).expect("open");
    idx.insert("c", vec![(2, bm(&[20]))]);
    idx.insert("d", vec![(3, bm(&[30]))]);
    let removed: HashSet<u64> = [20].into_iter().collect();
    idx.purge_segments(&removed);

    idx.save(Some(&path)).expect("save merged v3");

    let reader = SnapshotReader::open(&path).expect("reopen snapshot");
    assert_eq!(reader.num_entries(), 4);

    let present = hashes_present(&reader, &["a", "c", "d", "e", "g"]);
    assert_eq!(present, vec![true, true, true, false, true]);

    let c_bm = reader
        .get(ExactIndex::hash("c"))
        .expect("get c")
        .expect("c exists");
    assert_eq!(c_bm.len(), 1);
    assert_eq!(c_bm[0].0, 2);
    assert!(c_bm[0].1.contains(20));
}

#[test]
fn lazy_load_v1_to_lazy() {
    use std::fs;
    use std::io::Write;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lazy_i.idx");

    let n = 5usize;
    let mut raw: HashMap<String, Vec<(u64, RoaringBitmap)>> = HashMap::new();
    for i in 0..n {
        raw.insert(
            value_key(i),
            vec![((i + 1) as u64, bm(&[i as u32]))],
        );
    }
    {
        let mut file = BufWriter::new(fs::File::create(&path).unwrap());
        file.write_all(b"BTXI").unwrap();
        file.write_all(&[1u8, 0, 0, 0]).unwrap();
        bincode::serialize_into(&mut file, &raw).unwrap();
        file.flush().unwrap();
    }

    let mut idx = ExactIndex::open(&path).expect("open v1");
    assert_eq!(idx.len(), n);
    assert_eq!(idx.delta_len(), n, "v1 load should populate delta only");

    idx.save(Some(&path)).expect("save v3");

    let reopened = ExactIndex::open(&path).expect("reopen v3");
    assert_eq!(reopened.delta_len(), 0);
    assert_eq!(reopened.len(), n);
    for i in 0..n {
        assert!(reopened.get(&value_key(i)).is_some());
    }
}

#[test]
fn lazy_concurrent_gets() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lazy_j.idx");
    write_v3_snapshot(&path, 100);

    let idx = Arc::new(ExactIndex::open(&path).expect("open"));
    let mut handles = Vec::new();
    for t in 0..10 {
        let idx = Arc::clone(&idx);
        handles.push(thread::spawn(move || {
            for i in 0..20 {
                let key = value_key((t * 7 + i) % 100);
                let got = idx.get(&key);
                assert!(got.is_some(), "missing {}", key);
            }
        }));
    }
    for h in handles {
        h.join().expect("thread join");
    }
}

#[test]
fn lazy_empty_snapshot_save_and_query() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("lazy_k.idx");

    let mut idx = ExactIndex::open(&path).expect("open missing file binds path");
    idx.save(Some(&path)).expect("save empty");

    let reopened = ExactIndex::open(&path).expect("reopen");
    assert!(reopened.get("anything").is_none());
}
