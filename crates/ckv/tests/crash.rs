use keel_ckv::{KvError, PagedKv};
use keel_faultfs::{FaultConfig, FaultDisk};
use std::sync::Arc;

fn self_bucket_of(k: u64, buckets: u32) -> u32 {
    (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as u32 % buckets
}

const BUCKETS: u32 = 8;
const FRAMES: usize = 3;
const KEYS: u64 = 120;
const EXTRA: u64 = 40;
const BASE: u64 = 1_000;
const NEWER: u64 = 2_000;

#[test]
fn checkpoint_barrier_and_torn_detection_under_crash() {
    let mut total_pending = 0usize;
    let mut total_torn = 0usize;
    let mut seeds_with_full_survival = 0usize;

    for seed in 0..24u64 {
        let disk = FaultDisk::new(FaultConfig::default(), seed);

        {
            let kv = PagedKv::create(Arc::new(disk.handle()), BUCKETS, FRAMES).unwrap();
            for k in 0..KEYS {
                kv.put(k, BASE).unwrap();
            }
            kv.checkpoint().unwrap();

            for k in 0..KEYS {
                kv.put(k, NEWER).unwrap();
            }
            for k in KEYS..KEYS + EXTRA {
                kv.put(k, NEWER).unwrap();
            }
        }

        let report = disk.crash();
        total_pending += report.pending_ops;

        let disk2 = FaultDisk::from_image(FaultConfig::benign(), seed, disk.durable_image());
        let kv = PagedKv::open(Arc::new(disk2.handle()), BUCKETS, FRAMES);

        let mut all_survived = true;
        for bkt in 0..BUCKETS {
            match kv.bucket_entries(bkt) {
                Ok(entries) => {
                    for (k, v) in entries {
                        assert!(
                            k < KEYS + EXTRA,
                            "seed {seed}: bucket {bkt} garbage key {k}"
                        );
                        assert_eq!(
                            self_bucket_of(k, BUCKETS),
                            bkt,
                            "seed {seed}: bucket {bkt} holds key {k} that hashes elsewhere \
                             — a torn or cross-page image was read as valid"
                        );
                        assert!(
                            v == BASE || v == NEWER,
                            "seed {seed}: bucket {bkt} key {k} has value {v} \
                             — neither the checkpointed nor the newer value"
                        );
                    }
                }
                Err(KvError::Corrupt(_)) => {
                    total_torn += 1;
                    all_survived = false;
                }
                Err(e) => panic!("seed {seed}: unexpected {e}"),
            }
        }
        if all_survived {
            seeds_with_full_survival += 1;
        }
    }

    assert!(
        total_pending > 0,
        "no un-synced writes were in flight — the crash never exercised the adversary"
    );
    assert!(
        seeds_with_full_survival > 0,
        "no seed left every bucket intact — the checkpoint barrier is suspiciously weak"
    );
    let _ = total_torn;
}
