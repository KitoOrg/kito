//! Fault injection: truncated or damaged files, disk full, failed fsyncs,
//! power loss. The rule: no confirmed op is ever lost, and damage to
//! confirmed data is never hidden.

mod common;

use common::*;
use kito_core::*;
use kito_store::testing::{FaultyFs, Unsynced};
use kito_store::*;
use proptest::prelude::*;
use std::fs;
use std::path::Path;
use std::sync::Arc;

fn repair(opts: StoreOptions) -> StoreOptions {
    StoreOptions { repair: true, ..opts }
}

/// Creates a store with `n` confirmed ops and closes it.
fn build(dir: &Path, opts: &StoreOptions, n: usize) -> Vec<Op> {
    let mut store = Store::init_with(dir, opts.clone()).unwrap();
    let mut gen = OpGen::new(3);
    let ops: Vec<Op> = (0..n).map(|_| gen.append_to(&mut store)).collect();
    store.close().unwrap();
    ops
}

fn flip_byte(path: &Path, at: usize) {
    let mut bytes = fs::read(path).unwrap();
    bytes[at] ^= 0x01;
    fs::write(path, bytes).unwrap();
}

fn truncate(path: &Path, len: usize) {
    fs::OpenOptions::new().write(true).open(path).unwrap().set_len(len as u64).unwrap();
}

/// A store in which the index was left at `first` ops while the log has
/// `first + more` of them: it is the state after a crash that came between
/// two syncs of the index. Returns the ops and the offset up to which the
/// index considered them confirmed.
fn store_with_index_behind(dir: &Path, first: usize, more: usize) -> (Vec<Op>, usize) {
    let mut ops = build(dir, &StoreOptions::default(), first);
    let index = dir.join("index/index.redb");
    let old_index = fs::read(&index).unwrap();
    let confirmed_len = fs::metadata(dir.join("oplog/00000000.seg")).unwrap().len() as usize;

    let mut store = Store::open(dir).unwrap();
    let mut gen = OpGen::resume(3, &ops);
    ops.extend((0..more).map(|_| gen.append_to(&mut store)));
    store.close().unwrap();
    fs::write(&index, old_index).unwrap();
    (ops, confirmed_len)
}

#[test]
fn a_torn_tail_is_discarded_at_every_possible_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    let (ops, confirmed_len) = store_with_index_behind(&base, 6, 3);
    let log = fs::read(base.join("oplog/00000000.seg")).unwrap();
    let ends = record_ends(&log);
    assert_eq!(ends.len(), ops.len());
    assert_eq!(ends[5], confirmed_len);

    // Every possible length from "everything that was confirmed" to "everything".
    for cut in confirmed_len..=log.len() {
        let dir = tmp.path().join("cut");
        let _ = fs::remove_dir_all(&dir);
        copy_dir(&base, &dir);
        truncate(&dir.join("oplog/00000000.seg"), cut);

        let mut store = Store::open(&dir).unwrap_or_else(|e| panic!("cut at {cut}: {e}"));
        let complete = ends.iter().filter(|&&e| e <= cut).count();
        let valid_end = ends[complete - 1];
        let recovery = store.recovery().clone();
        assert_eq!(recovery.oplog.discarded_bytes, (cut - valid_end) as u64, "cut at {cut}");
        if cut > valid_end {
            let saved = recovery.oplog.saved_to.as_ref().expect("tail saved in lost+found");
            assert_eq!(fs::read(saved).unwrap(), &log[valid_end..cut]);
        }
        assert_eq!(
            recovery.index,
            IndexRecovery::CaughtUp { ops: (complete - 6) as u64, blobs: recovery_blobs(&recovery) }
        );
        assert_holds(&store, &ops[..complete]);

        // After recovery the store is used normally (checked on the record
        // boundaries and on a sample, so as not to redo fsync at every byte).
        if !ends.contains(&cut) && cut % 61 != 0 {
            continue;
        }
        let mut gen = OpGen::resume(3, &ops[..complete]);
        let next = gen.append_to(&mut store);
        store.close().unwrap();
        let store = Store::open(&dir).unwrap();
        assert_eq!(store.recovery().discarded_bytes(), 0);
        assert_eq!(store.get_op(&next.id).unwrap(), Some(next));
    }
}

fn recovery_blobs(r: &RecoveryReport) -> u64 {
    match r.index {
        IndexRecovery::CaughtUp { blobs, .. } | IndexRecovery::Rebuilt { blobs, .. } => blobs,
        IndexRecovery::UpToDate => 0,
    }
}

#[test]
fn truncation_below_the_confirmed_point_is_an_error_unless_repairing() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    let (ops, confirmed_len) = store_with_index_behind(&base, 8, 2);
    let log = fs::read(base.join("oplog/00000000.seg")).unwrap();
    let ends = record_ends(&log);

    for cut in (16..confirmed_len).step_by(37).chain([16, confirmed_len - 1]) {
        let dir = tmp.path().join("cut");
        let _ = fs::remove_dir_all(&dir);
        copy_dir(&base, &dir);
        let seg = dir.join("oplog/00000000.seg");
        truncate(&seg, cut);

        // Without repair: an error, and the file is not touched.
        match Store::open(&dir) {
            Err(StoreError::Corrupt { reason, .. }) => {
                assert!(reason.contains("repair mode"), "{reason}")
            }
            other => panic!("cut at {cut}: expected Corrupt, got {:?}", other.map(|_| ())),
        }
        assert_eq!(fs::metadata(&seg).unwrap().len(), cut as u64);
        assert!(!dir.join("lost+found").exists());

        // With repair: the intact prefix survives, the index is rebuilt.
        let complete = ends.iter().filter(|&&e| e <= cut).count();
        let store = Store::open_with(&dir, repair(StoreOptions::default())).unwrap();
        if complete > 0 {
            assert!(
                matches!(store.recovery().index, IndexRecovery::Rebuilt { .. }),
                "{:?}",
                store.recovery().index
            );
        }
        assert_holds(&store, &ops[..complete]);
    }
}

#[test]
fn confirmed_lanes_and_conflicts_are_never_silently_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    let ops = build(&base, &StoreOptions::default(), 3);
    {
        let mut store = Store::open(&base).unwrap();
        let checkpoint = Checkpoint {
            id: store.next_checkpoint_id(),
            head: ops[2].id,
            label: "base".into(),
            created_by: Author::human("fra"),
            verification: None,
            git_commit: None,
        };
        store.put_checkpoint(&checkpoint).unwrap();
        store
            .put_lane(&Lane {
                id: LaneId(1),
                agent: Author::agent("a"),
                base: checkpoint.id,
                head: ops[2].id,
                workdir: "w".into(),
                state: LaneState::Active,
                base_head: ops[2].id,
                since: 0,
                task: None,
                intent: None,
                next_symbol: 0,
            })
            .unwrap();
        store
            .put_conflict(&Conflict {
                id: ConflictId(1),
                symbol: None,
                sides: vec![ops[1].id, ops[2].id],
                kind: ConflictKind::Text,
                assigned_to: Some(LaneId(1)),
                resolved_by: None,
                file: Some("a.txt".into()),
                name: None,
                base_text: None,
                main_text: None,
                lane_text: None,
                resolution: None,
            })
            .unwrap();
        store.close().unwrap();
    }

    for (file, has_lane, has_conflict) in [("lanes/journal", false, true), ("conflicts", true, false)] {
        for missing in [false, true] {
            let dir = tmp.path().join("damaged");
            let _ = fs::remove_dir_all(&dir);
            copy_dir(&base, &dir);
            let path = dir.join(file);
            if missing {
                fs::remove_file(&path).unwrap();
            } else {
                truncate(&path, fs::metadata(&path).unwrap().len() as usize - 1);
            }
            match Store::open(&dir) {
                Err(StoreError::Corrupt { reason, .. }) => {
                    assert!(reason.contains("repair mode"), "{reason}")
                }
                other => panic!("{file}: expected Corrupt, got {:?}", other.map(|_| ())),
            }
            // Accepting the loss is an explicit choice, and costs only that record.
            let store = Store::open_with(&dir, repair(StoreOptions::default())).unwrap();
            assert_eq!(store.lane_record(LaneId(1)).is_some(), has_lane, "{file}");
            assert_eq!(store.conflict(ConflictId(1)).is_some(), has_conflict, "{file}");
            assert_holds(&store, &ops);
        }
    }
}

#[test]
fn garbage_after_the_last_record_is_discarded_and_kept_aside() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let ops = build(dir, &StoreOptions::default(), 20);
    let mut store = Store::open(dir).unwrap();
    let ck = Checkpoint {
        id: CheckpointId(1),
        head: ops[19].id,
        label: "c".into(),
        created_by: Author::human("fra"),
        verification: None,
        git_commit: None,
    };
    store.put_checkpoint(&ck).unwrap();
    store.close().unwrap();

    let garbage: Vec<u8> = (0..301u32).map(|i| (i * 31 % 251) as u8).collect();
    let files = ["oplog/00000000.seg", "objects/00000000.pack", "checkpoints"];
    for f in files {
        let mut bytes = fs::read(dir.join(f)).unwrap();
        bytes.extend_from_slice(&garbage);
        fs::write(dir.join(f), bytes).unwrap();
    }

    let store = Store::open(dir).unwrap();
    let r = store.recovery();
    for (name, log) in [("oplog", &r.oplog), ("packs", &r.packs), ("checkpoints", &r.checkpoints)] {
        assert_eq!(log.discarded_bytes, garbage.len() as u64, "{name}");
        assert_eq!(fs::read(log.saved_to.as_ref().unwrap()).unwrap(), garbage, "{name}");
    }
    assert_eq!(r.index, IndexRecovery::UpToDate);
    assert_eq!(store.checkpoint(CheckpointId(1)), Some(&ck));
    assert_holds(&store, &ops);
    drop(store);

    // Recovery is idempotent: the second open finds nothing more.
    let store = Store::open(dir).unwrap();
    assert_eq!(store.recovery().discarded_bytes(), 0);
}

#[test]
fn damage_in_a_sealed_segment_is_reported_on_read_and_by_verify() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let ops = build(dir, &small_segments(), 200);
    let segs = segments(dir, "oplog");
    assert!(segs.len() > 4);
    let victim = &segs[1];
    let before = fs::metadata(victim).unwrap().len();
    flip_byte(victim, 16 + 40); // inside the first record of the segment

    let store = Store::open_with(dir, small_segments()).unwrap();
    assert_eq!(fs::metadata(victim).unwrap().len(), before, "a sealed segment is never truncated");

    let mut unreadable = Vec::new();
    for op in &ops {
        match store.get_op(&op.id) {
            Ok(found) => assert_eq!(found.as_ref(), Some(op)),
            Err(StoreError::Corrupt { .. }) => unreadable.push(op.id),
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    assert_eq!(unreadable.len(), 1, "only the damaged op is unreadable");
    let report = store.verify().unwrap();
    assert!(!report.is_ok());
    assert!(report.problems.iter().any(|p| p.contains("oplog")), "{:?}", report.problems);
    drop(store);

    // Without the index the whole log would have to be reread: the error surfaces right away.
    fs::remove_file(dir.join("index/index.redb")).unwrap();
    assert!(matches!(Store::open_with(dir, small_segments()), Err(StoreError::Corrupt { .. })));
}

#[test]
fn damage_to_confirmed_data_in_the_active_files_is_never_silently_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    let ops = build(&base, &StoreOptions::default(), 30);
    let seg_len = fs::metadata(base.join("oplog/00000000.seg")).unwrap().len() as usize;
    let ends = record_ends(&fs::read(base.join("oplog/00000000.seg")).unwrap());

    for (file, at) in [
        ("oplog/00000000.seg", ends[9] + 20),
        ("oplog/00000000.seg", seg_len - 1),
        ("objects/00000000.pack", 16 + 50),
    ] {
        let dir = tmp.path().join("damaged");
        let _ = fs::remove_dir_all(&dir);
        copy_dir(&base, &dir);
        flip_byte(&dir.join(file), at);
        let len_before = fs::metadata(dir.join(file)).unwrap().len();

        assert!(matches!(Store::open(&dir), Err(StoreError::Corrupt { .. })), "{file}@{at}");
        assert_eq!(fs::metadata(dir.join(file)).unwrap().len(), len_before, "{file}: no truncation");

        let store = Store::open_with(&dir, repair(StoreOptions::default())).unwrap();
        assert!(store.recovery().discarded_bytes() > 0);
        if file.starts_with("oplog") {
            let complete = ends.iter().filter(|&&e| e <= at).count();
            assert_holds(&store, &ops[..complete]);
        } else {
            // Some blobs are lost: the ops remain, and verify says which ones were left orphaned.
            assert_eq!(store.op_count().unwrap(), 30);
            let report = store.verify().unwrap();
            assert!(
                report.problems.iter().any(|p| p.contains("missing from the packs")),
                "{:?}",
                report.problems
            );
        }
    }
}

#[test]
fn a_damaged_base_is_reported_when_a_delta_is_read() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(tmp.path()).unwrap();
    let lines: Vec<String> =
        (0..600).map(|i| format!("line {i}: {:x}", (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15))).collect();
    let first = lines.join("\n").into_bytes();
    let second = format!("{}\none more line", lines.join("\n")).into_bytes();
    let base = store.put_blob(&first).unwrap();
    let delta = store.put_blob_like(&second, &base).unwrap();
    store.close().unwrap();
    let pack = tmp.path().join("objects/00000000.pack");
    let whole_then_delta = std::fs::metadata(&pack).unwrap().len() as usize;
    assert!(whole_then_delta < first.len(), "the second version is stored as a delta");

    // A byte of the first record goes wrong: neither blob can be trusted,
    // and neither is returned.
    flip_byte(&pack, 200);
    let store = Store::open_with(tmp.path(), repair(StoreOptions::default())).unwrap();
    assert!(matches!(store.get_blob(&base), Err(StoreError::Corrupt { .. }) | Ok(None)), "the base");
    assert!(matches!(store.get_blob(&delta), Err(StoreError::Corrupt { .. }) | Ok(None)), "its delta");
}

#[test]
fn missing_segments_are_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    let ops = build(&base, &small_segments(), 200);
    let all = segments(&base, "oplog");

    // A segment in the middle is missing.
    let dir = tmp.path().join("hole");
    copy_dir(&base, &dir);
    fs::remove_file(segments(&dir, "oplog")[2].clone()).unwrap();
    match Store::open_with(&dir, small_segments()) {
        Err(StoreError::Corrupt { reason, .. }) => {
            assert!(reason.contains("segment 2 is missing"), "{reason}")
        }
        other => panic!("expected Corrupt, got {:?}", other.map(|_| ())),
    }

    // The last segments are missing: confirmed data has vanished.
    let dir = tmp.path().join("short");
    copy_dir(&base, &dir);
    for seg in &segments(&dir, "oplog")[all.len() - 2..] {
        fs::remove_file(seg).unwrap();
    }
    assert!(matches!(Store::open_with(&dir, small_segments()), Err(StoreError::Corrupt { .. })));
    let store = Store::open_with(&dir, repair(small_segments())).unwrap();
    let survived = store.op_count().unwrap() as usize;
    assert!(survived > 0 && survived < ops.len());
    assert_holds(&store, &ops[..survived]);
}

#[test]
fn the_index_can_always_be_rebuilt_from_the_logs() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    let ops = build(&base, &small_segments(), 150);
    let index = "index/index.redb";

    let reopen = |name: &str, damage: &dyn Fn(&Path)| {
        let dir = tmp.path().join(name);
        copy_dir(&base, &dir);
        damage(&dir.join(index));
        let store = Store::open_with(&dir, small_segments()).unwrap();
        assert_holds(&store, &ops);
        store.recovery().index.clone()
    };

    let rebuilt = |r: IndexRecovery| matches!(r, IndexRecovery::Rebuilt { ops: 150, .. });
    assert!(rebuilt(reopen("missing", &|p| fs::remove_file(p).unwrap())));
    assert!(rebuilt(reopen("garbage", &|p| fs::write(p, vec![0xa5; 9000]).unwrap())));
    assert!(rebuilt(reopen("empty", &|p| fs::write(p, b"").unwrap())));
    assert!(rebuilt(reopen("truncated", &|p| {
        let len = fs::metadata(p).unwrap().len();
        truncate(p, len as usize / 2);
    })));

    // Index of another store with logs of the same shape: the positions
    // would be plausible, but the last indexed op does not match.
    let other = tmp.path().join("other");
    {
        let mut store = Store::init_with(&other, small_segments()).unwrap();
        let mut gen = OpGen::new(3);
        gen.time_shift = 1;
        for _ in 0..150 {
            gen.append_to(&mut store);
        }
        store.close().unwrap();
    }
    let foreign = fs::read(other.join(index)).unwrap();
    let r = reopen("foreign", &|p| fs::write(p, &foreign).unwrap());
    assert!(
        matches!(&r, IndexRecovery::Rebuilt { reason, .. } if reason.contains("is no longer in the log")),
        "{r:?}"
    );
}

/// Tries to append and confirm ops until a write fails.
/// Returns the confirmed ops and the attempted ones.
fn write_until_failure(
    store: &mut Store,
    gen: &mut OpGen,
    batches: usize,
) -> (Vec<Op>, Vec<Op>, Option<StoreError>) {
    let (mut confirmed, mut attempted) = (Vec::new(), Vec::new());
    for _ in 0..batches {
        let mut batch = Vec::new();
        for _ in 0..5 {
            let (op, blobs) = gen.next();
            let result = blobs
                .iter()
                .try_for_each(|b| store.put_blob(b).map(drop))
                .and_then(|()| store.append_op(&op));
            attempted.push(op.clone());
            match result {
                Ok(_) => batch.push(op),
                Err(e) => return (confirmed, attempted, Some(e)),
            }
        }
        match store.commit() {
            Ok(()) => confirmed.extend(batch),
            Err(e) => return (confirmed, attempted, Some(e)),
        }
    }
    (confirmed, attempted, None)
}

#[test]
fn a_full_disk_never_loses_confirmed_ops() {
    let tmp = tempfile::tempdir().unwrap();
    let mut failures = 0;
    // The space runs out at different points: in the middle of a pack, of an
    // oplog segment, or at the creation of a new segment.
    for space in (0..6_000).step_by(211) {
        let dir = tmp.path().join(format!("s{space}"));
        let faulty = FaultyFs::new();
        let opts = StoreOptions { fs: Arc::new(faulty.clone()), ..small_segments() };
        let mut store = Store::init_with(&dir, opts).unwrap();
        let mut gen = OpGen::new(3);
        let (mut confirmed, _, err) = write_until_failure(&mut store, &mut gen, 4);
        assert!(err.is_none());

        faulty.set_space(space);
        let (more, attempted, err) = write_until_failure(&mut store, &mut gen, 8);
        confirmed.extend(more);
        if let Some(err) = err {
            failures += 1;
            assert!(err.is_storage_full(), "space {space}: {err}");
            assert!(store.is_poisoned());
            assert!(matches!(store.commit(), Err(StoreError::Poisoned)));
            assert!(matches!(store.put_blob(b"x"), Err(StoreError::Poisoned)));
            // Reads of what was confirmed keep working.
            for op in &confirmed {
                assert_eq!(store.get_op(&op.id).unwrap().as_ref(), Some(op));
            }
        }
        drop(store);

        // Once the space is freed, the store reopens with all the confirmed ops.
        let mut store = Store::open_with(&dir, small_segments()).unwrap();
        let survived = store.op_count().unwrap() as usize;
        assert!(survived >= confirmed.len(), "space {space}: {survived} < {}", confirmed.len());
        let mut regen = OpGen::new(3);
        let all: Vec<Op> = (0..20 + attempted.len()).map(|_| regen.next().0).collect();
        assert_holds(&store, &all[..survived]);

        let mut gen = OpGen::resume(3, &all[..survived]);
        let next = gen.append_to(&mut store);
        store.close().unwrap();
        assert_eq!(Store::open_with(&dir, small_segments()).unwrap().get_op(&next.id).unwrap(), Some(next));
    }
    assert!(failures > 20, "the disk must fill up in most cases: {failures}");
}

#[test]
fn a_failed_fsync_poisons_the_store_and_reopening_recovers() {
    for syncs_before_failure in 0..4 {
        let tmp = tempfile::tempdir().unwrap();
        let faulty = FaultyFs::new();
        let opts = StoreOptions { fs: Arc::new(faulty.clone()), ..StoreOptions::default() };
        let mut store = Store::init_with(tmp.path(), opts).unwrap();
        let mut gen = OpGen::new(2);
        let (confirmed, _, err) = write_until_failure(&mut store, &mut gen, 3);
        assert!(err.is_none());

        faulty.fail_sync_after(syncs_before_failure);
        let (more, attempted, err) = write_until_failure(&mut store, &mut gen, 4);
        let err = err.expect("an fsync must fail");
        assert!(matches!(err, StoreError::Io { .. }), "{err}");
        assert!(store.is_poisoned());
        drop(store);

        let store = Store::open(tmp.path()).unwrap();
        let survived = store.op_count().unwrap() as usize;
        let confirmed_n = confirmed.len() + more.len();
        assert!(survived >= confirmed_n && survived <= confirmed.len() + attempted.len());
        let mut regen = OpGen::new(2);
        let all: Vec<Op> = (0..survived).map(|_| regen.next().0).collect();
        assert_holds(&store, &all);
    }
}

#[test]
fn a_partial_write_that_cannot_be_undone_is_discarded_at_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let faulty = FaultyFs::new();
    let opts = StoreOptions { fs: Arc::new(faulty.clone()), ..StoreOptions::default() };
    let mut store = Store::init_with(tmp.path(), opts).unwrap();
    let mut gen = OpGen::new(2);
    let (confirmed, _, _) = write_until_failure(&mut store, &mut gen, 3);

    // The space runs out in the middle of a write and not even the truncate
    // succeeds: a half-written record is left in the files.
    faulty.set_space(150);
    faulty.fail_truncate(true);
    let (more, _, err) = write_until_failure(&mut store, &mut gen, 4);
    assert!(more.is_empty() && err.unwrap().is_storage_full());
    drop(store);

    let store = Store::open(tmp.path()).unwrap();
    assert_eq!(store.recovery().discarded_bytes(), 150, "{:?}", store.recovery());
    assert_holds(&store, &confirmed);
}

#[derive(Clone, Debug)]
enum Action {
    Append(usize),
    Commit,
    /// Clean reopen.
    Reopen,
    /// The process dies: what had not been written to the files is lost.
    Kill,
    /// The power goes out: the bytes written but not synced are at risk too.
    PowerLoss {
        fates: Vec<(u8, u64)>,
        lose_index: bool,
    },
    /// An fsync fails during a commit, then the power goes out.
    FailedCommit {
        syncs_before_failure: u64,
        fates: Vec<(u8, u64)>,
    },
}

fn arb_fates() -> impl Strategy<Value = Vec<(u8, u64)>> {
    prop::collection::vec((0u8..5, any::<u64>()), 4)
}

fn arb_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        4 => (1usize..40).prop_map(Action::Append),
        3 => Just(Action::Commit),
        1 => Just(Action::Reopen),
        1 => Just(Action::Kill),
        2 => (arb_fates(), any::<bool>()).prop_map(|(fates, lose_index)| Action::PowerLoss { fates, lose_index }),
        1 => (0u64..3, arb_fates()).prop_map(|(syncs_before_failure, fates)| Action::FailedCommit { syncs_before_failure, fates }),
    ]
}

fn fate_of(fates: &[(u8, u64)], i: usize) -> (Unsynced, u64) {
    let (kind, salt) = fates[i % fates.len()];
    let how = match kind {
        0 => Unsynced::Lost,
        1 => Unsynced::Partial,
        2 => Unsynced::Kept,
        3 => Unsynced::Zeroed,
        _ => Unsynced::Garbage,
    };
    (how, salt)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// Model: any sequence of writes, commits, crashes and power losses
    /// leaves in the store a prefix of the history that contains at least
    /// all the confirmed ops, with consistent blobs and indexes.
    #[test]
    fn confirmed_ops_survive_any_crash(
        actions in prop::collection::vec(arb_action(), 1..25),
        durable_index in any::<bool>(),
        max_buffered in prop::sample::select(vec![600usize, 4 << 20]),
    ) {
        const LANES: u64 = 3;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let options = |fs: &FaultyFs| StoreOptions {
            fs: Arc::new(fs.clone()),
            max_buffered,
            index_sync_interval: if durable_index { std::time::Duration::ZERO } else { std::time::Duration::from_secs(3600) },
            ..small_segments()
        };

        let mut faulty = FaultyFs::new();
        let mut store = Some(Store::init_with(dir, options(&faulty)).unwrap());
        // `history`: all the appended ops; the first `confirmed` are confirmed.
        let mut history: Vec<Op> = Vec::new();
        let mut confirmed = 0usize;
        let mut gen = OpGen::new(LANES);

        for action in actions {
            let live = store.as_mut().unwrap();
            let crashed = match action {
                Action::Append(n) => {
                    for _ in 0..n {
                        history.push(gen.append_to(live));
                    }
                    false
                }
                Action::Commit => {
                    live.commit().unwrap();
                    confirmed = history.len();
                    false
                }
                Action::Reopen => {
                    store.take().unwrap().close().unwrap();
                    confirmed = history.len();
                    true
                }
                Action::Kill => true,
                Action::PowerLoss { fates, lose_index } => {
                    drop(store.take());
                    let mut i = 0;
                    faulty.power_loss(|_, _| { i += 1; fate_of(&fates, i) }).unwrap();
                    if lose_index {
                        let _ = fs::remove_file(dir.join("index/index.redb"));
                    }
                    true
                }
                Action::FailedCommit { syncs_before_failure, fates } => {
                    history.push(gen.append_to(live));
                    faulty.fail_sync_after(syncs_before_failure);
                    // If the commit needs fewer fsyncs than those granted (for
                    // example an op without blobs), it succeeds: it is a normal commit.
                    match live.commit() {
                        Ok(()) => confirmed = history.len(),
                        Err(_) => prop_assert!(live.is_poisoned()),
                    }
                    drop(store.take());
                    let mut i = 0;
                    faulty.power_loss(|_, _| { i += 1; fate_of(&fates, i) }).unwrap();
                    true
                }
            };
            if crashed {
                drop(store.take());
                faulty = FaultyFs::new();
                let reopened = Store::open_with(dir, options(&faulty)).unwrap();
                let survived = reopened.op_count().unwrap() as usize;
                prop_assert!(survived >= confirmed, "confirmed ops lost: {} < {}", survived, confirmed);
                prop_assert!(survived <= history.len());
                history.truncate(survived);
                assert_holds(&reopened, &history);
                // Everything that survived the reopen is now durable.
                confirmed = survived;
                gen = OpGen::resume(LANES, &history);
                store = Some(reopened);
            }
        }
    }
}
