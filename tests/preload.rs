//! Tests for the index preload / lock API.
//!
//! These only assert the parts that are deterministic: what counts as "the index", that
//! preloading reports the right byte count, and that lookups are unaffected. Whether
//! pages are actually resident afterwards is a kernel scheduling matter we cannot
//! observe portably, and `mlock` depends on `RLIMIT_MEMLOCK`, so a failure there is
//! tolerated rather than asserted.

use std::path::{Path, PathBuf};

use erigon_seg::{DomainOptions, DomainWriter, KvReader, Salt, memlock_limit, raise_memlock_limit};

fn scratch(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("erigon_seg_preload_{}_{tag}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p.join("v1.1-test.0-1.kv")
}

fn cleanup(kv: &Path) {
    for ext in ["kv", "bt", "kvei"] {
        let _ = std::fs::remove_file(kv.with_extension(ext));
    }
    if let Some(d) = kv.parent() {
        let _ = std::fs::remove_dir(d);
    }
}

/// Build a domain of `n` keys, with a `.kvei` when `salt` is set.
fn build(tag: &str, n: u32, salt: Option<u32>) -> (PathBuf, Vec<Vec<u8>>) {
    let kv = scratch(tag);
    let keys: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let mut k = vec![0u8; 20];
            k[..4].copy_from_slice(&i.to_be_bytes());
            k
        })
        .collect();
    let mut w = DomainWriter::create(
        &kv,
        DomainOptions {
            bt: Default::default(),
            salt,
            compress: true,
        },
    )
    .unwrap();
    for k in &keys {
        w.add(k, b"v").unwrap();
    }
    w.finish().unwrap();
    (kv, keys)
}

#[test]
fn preload_reports_index_size_and_preserves_lookups() {
    let (kv, keys) = build("basic", 2000, None);
    let r = KvReader::open(&kv).unwrap();

    let bt_len = std::fs::metadata(kv.with_extension("bt")).unwrap().len();
    assert_eq!(
        r.index_bytes(),
        bt_len,
        "with no active bloom the index is just the .bt"
    );
    assert_eq!(r.preload_index(), bt_len, "preload reports bytes loaded");

    // Preloading must not disturb results, and must be safe to repeat.
    r.preload_index();
    for k in keys.iter().step_by(13) {
        assert_eq!(r.get(k).unwrap().as_deref(), Some(&b"v"[..]));
    }
    assert_eq!(r.get(&[0xffu8; 20]).unwrap(), None);
    cleanup(&kv);
}

#[test]
fn kvei_counts_only_once_the_bloom_is_active() {
    let salt = 1234u32;
    let (kv, keys) = build("bloom", 2000, Some(salt));
    let mut r = KvReader::open(&kv).unwrap();
    let bt_len = std::fs::metadata(kv.with_extension("bt")).unwrap().len();
    let kvei_len = std::fs::metadata(kv.with_extension("kvei")).unwrap().len();
    assert!(kvei_len > 0, "a .kvei should have been written");

    // Bloom not yet enabled: the .kvei is never read, so it is not worth preloading.
    assert_eq!(r.index_bytes(), bt_len);
    assert_eq!(r.preload_index(), bt_len);

    assert!(r.enable_bloom(Salt::Known(salt)), "bloom should validate");
    assert_eq!(r.index_bytes(), bt_len + kvei_len);
    assert_eq!(r.preload_index(), bt_len + kvei_len);

    // Lookups still correct with the bloom in play.
    for k in keys.iter().step_by(13) {
        assert_eq!(r.get(k).unwrap().as_deref(), Some(&b"v"[..]));
    }
    for i in 0..200u32 {
        let mut miss = vec![0u8; 20];
        miss[..4].copy_from_slice(&i.to_be_bytes());
        miss[19] = 0xff;
        assert_eq!(r.get(&miss).unwrap(), None);
    }
    cleanup(&kv);
}

#[test]
fn preload_all_covers_the_data_file_too() {
    let (kv, keys) = build("all", 2000, None);
    let r = KvReader::open(&kv).unwrap();

    let kv_len = std::fs::metadata(&kv).unwrap().len();
    let bt_len = std::fs::metadata(kv.with_extension("bt")).unwrap().len();
    assert_eq!(
        r.total_bytes(),
        kv_len + bt_len,
        "no .kvei was written here"
    );
    assert!(
        r.total_bytes() > r.index_bytes(),
        "total must include the .kv, which index_bytes does not"
    );
    assert_eq!(
        r.preload_all(),
        kv_len + bt_len,
        "preload_all reports .kv + index"
    );

    // Repeatable, and lookups are unaffected.
    r.preload_all();
    for k in keys.iter().step_by(13) {
        assert_eq!(r.get(k).unwrap().as_deref(), Some(&b"v"[..]));
    }
    assert_eq!(r.get(&[0xffu8; 20]).unwrap(), None);
    cleanup(&kv);
}

#[test]
fn will_need_is_a_harmless_hint() {
    let (kv, keys) = build("willneed", 1500, None);
    let r = KvReader::open(&kv).unwrap();
    // It may do nothing at all (see the method docs), but it must never fail or change
    // results, in any order relative to the other advice calls.
    r.advise_will_need().unwrap();
    r.advise_random().unwrap();
    r.advise_will_need().unwrap();
    for k in keys.iter().step_by(11) {
        assert_eq!(r.get(k).unwrap().as_deref(), Some(&b"v"[..]));
    }
    cleanup(&kv);
}

#[test]
fn lock_and_unlock_round_trip() {
    let (kv, keys) = build("lock", 1000, None);
    let r = KvReader::open(&kv).unwrap();

    // RLIMIT_MEMLOCK is commonly small, so a failure here is expected on some hosts and
    // must simply leave the reader usable.
    match r.lock_index() {
        Ok(()) => {
            for k in keys.iter().step_by(7) {
                assert_eq!(r.get(k).unwrap().as_deref(), Some(&b"v"[..]));
            }
            r.unlock_index().expect("unlock after a successful lock");
        }
        Err(e) => eprintln!("lock_index unavailable here ({e}); skipping the locked checks"),
    }

    // Either way, lookups keep working.
    for k in keys.iter().step_by(7) {
        assert_eq!(r.get(k).unwrap().as_deref(), Some(&b"v"[..]));
    }
    cleanup(&kv);
}

#[test]
fn lock_all_pins_everything_or_degrades_to_preload() {
    let (kv, keys) = build("lockall", 2000, None);
    let r = KvReader::open(&kv).unwrap();

    // Over RLIMIT_MEMLOCK this fails, and must leave the reader working and the data
    // merely preloaded rather than half-configured.
    match r.lock_all() {
        Ok(()) => {
            for k in keys.iter().step_by(9) {
                assert_eq!(r.get(k).unwrap().as_deref(), Some(&b"v"[..]));
            }
            r.unlock_all().expect("unlock after a successful lock");
            // Unlocking must not disturb results either.
            for k in keys.iter().step_by(9) {
                assert_eq!(r.get(k).unwrap().as_deref(), Some(&b"v"[..]));
            }
        }
        Err(e) => eprintln!("lock_all unavailable here ({e}); skipping the locked checks"),
    }

    for k in keys.iter().step_by(9) {
        assert_eq!(r.get(k).unwrap().as_deref(), Some(&b"v"[..]));
    }
    assert_eq!(r.get(&[0xffu8; 20]).unwrap(), None);
    cleanup(&kv);
}

#[test]
fn memlock_limit_is_readable_and_raisable() {
    let Some((soft, hard)) = memlock_limit() else {
        eprintln!("no RLIMIT_MEMLOCK on this platform; skipping");
        return;
    };
    assert!(soft <= hard, "soft limit must not exceed the hard limit");

    // Raising to the hard limit needs no privilege and must never lower anything.
    let (soft2, hard2) = raise_memlock_limit(u64::MAX).expect("raising should not error");
    assert!(
        soft2 >= soft,
        "soft limit must not go down: {soft} -> {soft2}"
    );
    assert!(
        hard2 >= hard,
        "hard limit must not go down: {hard} -> {hard2}"
    );
    assert_eq!(
        soft2, hard2,
        "soft should have been raised to the hard limit"
    );

    // Idempotent.
    let (soft3, hard3) = raise_memlock_limit(u64::MAX).unwrap();
    assert_eq!((soft2, hard2), (soft3, hard3));
}

#[test]
fn advice_calls_are_harmless_in_any_order() {
    let (kv, keys) = build("advice", 1500, None);
    let r = KvReader::open(&kv).unwrap();

    // preload_index restores random advice, so the two can be combined either way round.
    r.advise_random().unwrap();
    r.preload_index();
    r.advise_sequential().unwrap();
    r.preload_index();
    r.advise_random().unwrap();

    for k in keys.iter().step_by(11) {
        assert_eq!(r.get(k).unwrap().as_deref(), Some(&b"v"[..]));
    }
    cleanup(&kv);
}
