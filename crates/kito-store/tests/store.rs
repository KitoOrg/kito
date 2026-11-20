//! Basic behavior of the store.

mod common;

use common::*;
use kito_core::*;
use kito_store::*;

#[test]
fn init_open_and_format_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(STORE_DIR);

    assert!(matches!(Store::open(&dir), Err(StoreError::NotAStore(_))));
    let store = Store::init(&dir).unwrap();
    assert_eq!(store.recovery().index, IndexRecovery::UpToDate);
    for sub in ["objects", "oplog", "index", "lanes"] {
        assert!(dir.join(sub).is_dir(), "{sub}");
    }
    assert!(dir.join("checkpoints").is_file());
    drop(store);

    assert!(matches!(Store::init(&dir), Err(StoreError::AlreadyExists(_))));
    Store::open(&dir).unwrap();

    std::fs::write(dir.join("format"), "kito-store 99\n").unwrap();
    assert!(matches!(Store::open(&dir), Err(StoreError::UnsupportedFormat(v)) if v == "kito-store 99"));
}

#[test]
fn only_one_writer_at_a_time() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::init(tmp.path()).unwrap();
    assert!(matches!(Store::open(tmp.path()), Err(StoreError::Locked(_))));
    drop(store);
    Store::open(tmp.path()).unwrap();
}

#[test]
fn a_lock_let_go_a_moment_later_is_waited_for() {
    // As when a process forked by another thread still shares the lock of
    // a store just closed, until it executes its program.
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::init(tmp.path()).unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        drop(store);
    });
    Store::open(tmp.path()).unwrap();
    holder.join().unwrap();
}

#[test]
fn blobs_round_trip_deduplicate_and_compress() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(tmp.path()).unwrap();

    let text = "fn main() { println!(\"hey!\"); }\n".repeat(2_000).into_bytes();
    let noise: Vec<u8> = (0..5_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let cases: Vec<Vec<u8>> = vec![vec![], b"x".to_vec(), text.clone(), noise];

    let ids: Vec<BlobId> = cases.iter().map(|c| store.put_blob(c).unwrap()).collect();
    for (id, data) in ids.iter().zip(&cases) {
        assert_eq!(*id, BlobId::of(data));
        // Readable right away, before the commit.
        assert_eq!(store.get_blob(id).unwrap().as_deref(), Some(&data[..]));
    }
    assert_eq!(store.put_blob(&text).unwrap(), ids[2], "same content, same ID");
    assert_eq!(store.stats().unwrap().blobs, 4, "the duplicate is not rewritten");
    assert_eq!(store.get_blob(&BlobId::of(b"missing")).unwrap(), None);
    store.close().unwrap();

    let pack = std::fs::metadata(tmp.path().join("objects/00000000.pack")).unwrap().len();
    assert!(pack < 10_000, "64 kB of repetitive text must compress: pack of {pack} bytes");

    let mut store = Store::open(tmp.path()).unwrap();
    for (id, data) in ids.iter().zip(&cases) {
        assert_eq!(store.get_blob(id).unwrap().as_deref(), Some(&data[..]));
    }
    assert_eq!(store.put_blob(&text).unwrap(), ids[2]);
    assert_eq!(store.stats().unwrap().blobs, 4, "deduplicated after reopening too");
}

/// A source file of about `lines` lines that does not compress to nothing.
fn source(lines: usize) -> Vec<String> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut word = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        format!("{:x}", state % 0xffff_ffff)
    };
    (0..lines).map(|i| format!("    let {} = {}({}, {i}) + {};", word(), word(), word(), word())).collect()
}

#[test]
fn a_new_version_of_a_blob_costs_what_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(tmp.path()).unwrap();
    let pack = || std::fs::metadata(tmp.path().join("objects/00000000.pack")).unwrap().len();

    // A file of some 60 kB, then sixty saves that change a line each.
    let mut lines = source(1_200);
    let first = lines.join("\n").into_bytes();
    let mut ids = vec![store.put_blob(&first).unwrap()];
    store.commit().unwrap();
    let whole = pack();
    assert!(whole > 15_000, "the file must not compress to nothing: {whole} bytes");

    let mut versions = vec![first];
    for save in 0..60 {
        lines[(save * 37) % 1_200] = format!("    let changed = save({save});");
        let content = lines.join("\n").into_bytes();
        let id = store.put_blob_like(&content, ids.last().unwrap()).unwrap();
        assert_eq!(id, BlobId::of(&content));
        // Readable right away, before the commit.
        assert_eq!(store.get_blob(&id).unwrap().as_deref(), Some(&content[..]));
        ids.push(id);
        versions.push(content);
    }
    store.commit().unwrap();
    // Sixty versions stored whole would be sixty times the first. Chains
    // are cut now and then (a version is stored whole again), so: a few.
    let grown = pack() - whole;
    assert!(grown < 4 * whole, "60 saves cost {grown} bytes, the file alone {whole}");
    assert_eq!(store.put_blob_like(&versions[30], &ids[3]).unwrap(), ids[30], "same content, same ID");
    assert_eq!(store.stats().unwrap().blobs, 61);

    // What does not resemble its supposed base, or has none, is stored whole
    // and reads the same.
    let other = source(300).join("\n").into_bytes();
    let unrelated = store.put_blob_like(&other, &ids[60]).unwrap();
    let orphan = store.put_blob_like(b"no base at all", &BlobId::of(b"never stored")).unwrap();
    let tiny = store.put_blob_like(b"tiny", &ids[60]).unwrap();
    assert_eq!(store.get_blob(&unrelated).unwrap().as_deref(), Some(&other[..]));
    assert_eq!(store.get_blob(&orphan).unwrap().as_deref(), Some(&b"no base at all"[..]));
    assert_eq!(store.get_blob(&tiny).unwrap().as_deref(), Some(&b"tiny"[..]));
    store.close().unwrap();

    // After a reopen every version is what it was, in any order, and the
    // whole store verifies.
    let store = Store::open(tmp.path()).unwrap();
    for at in [60, 0, 25, 59, 24, 1, 37] {
        assert_eq!(store.get_blob(&ids[at]).unwrap().as_deref(), Some(&versions[at][..]), "version {at}");
    }
    for (id, content) in ids.iter().zip(&versions) {
        assert_eq!(store.get_blob(id).unwrap().as_deref(), Some(&content[..]));
    }
    let report = store.verify().unwrap();
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(report.blobs, 64);
}

#[test]
fn a_store_from_before_deltas_is_opened_and_marked() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(tmp.path()).unwrap();
    let id = store.put_blob(b"written by an older version").unwrap();
    store.close().unwrap();
    std::fs::write(tmp.path().join("format"), "kito-store 1\n").unwrap();

    let store = Store::open(tmp.path()).unwrap();
    assert_eq!(store.get_blob(&id).unwrap().as_deref(), Some(&b"written by an older version"[..]));
    drop(store);
    // From now on it may hold what an older version cannot read.
    assert_eq!(std::fs::read_to_string(tmp.path().join("format")).unwrap(), "kito-store 2\n");
}

#[test]
fn ops_are_validated_before_being_logged() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(tmp.path()).unwrap();
    let author = Author::human("fra");
    let at = Hlc::new(1, 0);

    let blob = BlobId::of(b"content");
    let add = Op::new(
        [],
        LaneId::MAIN,
        author.clone(),
        at,
        OpKind::FileAdd { path: "a.txt".into(), blob, mode: FileMode::Regular },
        None,
    )
    .unwrap();
    // The blob is not in the store.
    assert!(matches!(store.append_op(&add), Err(StoreError::InvalidOp(m)) if m.contains("blob")));
    store.put_blob(b"content").unwrap();
    assert!(store.append_op(&add).unwrap());
    assert!(!store.append_op(&add).unwrap(), "the same op twice is a no-op");

    // Unknown parent.
    let orphan = Op::new(
        [OpId(Hash::of(b"?"))],
        LaneId::MAIN,
        author.clone(),
        at,
        OpKind::FileRemove { path: "a.txt".into() },
        None,
    )
    .unwrap();
    assert!(matches!(store.append_op(&orphan), Err(StoreError::InvalidOp(m)) if m.contains("parent")));

    // ID that does not match the content.
    let mut forged =
        Op::new([add.id], LaneId::MAIN, author, at, OpKind::FileRemove { path: "a.txt".into() }, None)
            .unwrap();
    forged.id = OpId(Hash::of(b"fake"));
    assert!(matches!(store.append_op(&forged), Err(StoreError::InvalidOp(m)) if m.contains("ID")));

    // Validation errors do not poison the store.
    assert!(!store.is_poisoned());
    store.commit().unwrap();
    assert_holds(&store, &[add]);
}

#[test]
fn lane_and_symbol_queries_see_pending_and_committed_ops() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init_with(tmp.path(), small_segments()).unwrap();
    let mut gen = OpGen::new(3);
    let mut ops = Vec::new();

    for round in 0..4 {
        for _ in 0..25 {
            ops.push(gen.append_to(&mut store));
        }
        // On alternate rounds the queries run before the commit: the appended
        // ops must be as visible as the confirmed ones.
        if round % 2 == 0 {
            store.commit().unwrap();
        }
        for lane in (0..3).map(LaneId) {
            let expected: Vec<&Op> = ops.iter().filter(|o| o.lane == lane).collect();
            assert_eq!(store.lane_len(lane).unwrap(), expected.len() as u64);
            assert_eq!(store.lane_head(lane).unwrap(), expected.last().map(|o| o.id));
            let all = store.lane_ops(lane, 0, usize::MAX).unwrap();
            assert_eq!(all.iter().collect::<Vec<_>>(), expected);
            // Pagination straddling the index and the pending ops.
            let page = store.lane_ops(lane, 5, 20).unwrap();
            assert_eq!(page.iter().collect::<Vec<_>>(), expected[5..expected.len().min(25)].to_vec());
        }
        let summary = store.lanes().unwrap();
        assert_eq!(summary.len(), 3);
        assert_eq!(summary.iter().map(|l| l.ops).sum::<u64>(), ops.len() as u64);
    }
    assert_eq!(store.lane_len(LaneId(99)).unwrap(), 0);
    assert_eq!(store.lane_head(LaneId(99)).unwrap(), None);
    assert!(store.lane_ops(LaneId(99), 0, 10).unwrap().is_empty());

    let sym = SymbolId::new(LaneId(1), 2);
    let expected: Vec<&Op> = ops.iter().filter(|o| o.kind.symbol() == Some(sym)).collect();
    assert!(!expected.is_empty());
    assert_eq!(store.symbol_ops(sym).unwrap().iter().collect::<Vec<_>>(), expected);

    store.commit().unwrap();
    assert_eq!(store.symbol_ops(sym).unwrap().iter().collect::<Vec<_>>(), expected);
    assert_holds(&store, &ops);
}

#[test]
fn everything_survives_a_clean_reopen_across_many_segments() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gen = OpGen::new(4);
    let mut ops = Vec::new();
    for _ in 0..5 {
        let mut store = Store::open_with(tmp.path(), small_segments())
            .or_else(|_| Store::init_with(tmp.path(), small_segments()))
            .unwrap();
        assert_eq!(store.recovery().discarded_bytes(), 0);
        assert_holds(&store, &ops);
        for _ in 0..120 {
            ops.push(gen.append_to(&mut store));
        }
        store.close().unwrap();
    }
    let store = Store::open_with(tmp.path(), small_segments()).unwrap();
    assert_eq!(
        store.recovery().index,
        IndexRecovery::UpToDate,
        "after close() the index is already caught up"
    );
    let stats = store.stats().unwrap();
    assert!(stats.oplog_segments > 10, "oplog segments: {}", stats.oplog_segments);
    assert!(stats.packs > 3, "packs: {}", stats.packs);
    assert_holds(&store, &ops);

    let mut scanned = Vec::new();
    store
        .scan_ops(|_, op| {
            scanned.push(op);
            Ok(())
        })
        .unwrap();
    assert_eq!(scanned, ops, "scan_ops returns the ops in log order");
}

#[test]
fn uncommitted_work_is_lost_on_drop_but_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let mut gen = OpGen::new(2);
    let mut store = Store::init(tmp.path()).unwrap();
    let confirmed: Vec<Op> = (0..40).map(|_| gen.append_to(&mut store)).collect();
    store.commit().unwrap();
    for _ in 0..40 {
        gen.append_to(&mut store);
    }
    drop(store); // no commit: like a crash

    let store = Store::open(tmp.path()).unwrap();
    assert_holds(&store, &confirmed);
}

#[test]
fn a_blob_larger_than_a_pack_gets_its_own_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init_with(tmp.path(), small_segments()).unwrap();
    let big: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
    let small = b"small".to_vec();
    let a = store.put_blob(&small).unwrap();
    let b = store.put_blob(&big).unwrap();
    let c = store.put_blob(b"after").unwrap();
    store.close().unwrap();
    let store = Store::open_with(tmp.path(), small_segments()).unwrap();
    assert_eq!(store.get_blob(&a).unwrap().unwrap(), small);
    assert_eq!(store.get_blob(&b).unwrap().unwrap(), big);
    assert_eq!(store.get_blob(&c).unwrap().unwrap(), b"after");
    assert!(store.verify().unwrap().is_ok());
}

#[test]
fn checkpoints_are_durable_and_updatable() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(tmp.path()).unwrap();
    let mut gen = OpGen::new(1);
    let first = gen.append_to(&mut store);
    assert_eq!(store.next_checkpoint_id(), CheckpointId(1));

    let mut ck = Checkpoint {
        id: store.next_checkpoint_id(),
        head: first.id,
        label: "first".into(),
        created_by: Author::human("fra"),
        verification: None,
        git_commit: None,
        approved_by: None,
        git_merge: None,
    };
    // put_checkpoint confirms the appended ops on its own.
    store.put_checkpoint(&ck).unwrap();
    assert_eq!(store.next_checkpoint_id(), CheckpointId(2));

    let unknown = Checkpoint { id: CheckpointId(2), head: OpId(Hash::of(b"?")), ..ck.clone() };
    assert!(matches!(store.put_checkpoint(&unknown), Err(StoreError::InvalidOp(_))));

    ck.git_commit = Some([7; 20]);
    store.put_checkpoint(&ck).unwrap();
    drop(store);

    let store = Store::open(tmp.path()).unwrap();
    assert_eq!(store.checkpoints().count(), 1);
    assert_eq!(store.checkpoint(CheckpointId(1)), Some(&ck), "the last record with the same ID wins");
    assert_eq!(store.next_checkpoint_id(), CheckpointId(2));
    assert_holds(&store, &[first]);
}

#[test]
fn rebuilt_index_answers_like_the_incremental_one() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init_with(tmp.path(), small_segments()).unwrap();
    let mut gen = OpGen::new(3);
    let ops: Vec<Op> = (0..300).map(|_| gen.append_to(&mut store)).collect();
    store.commit().unwrap();

    let snapshot = |s: &Store| {
        let lanes = s.lanes().unwrap();
        let per_lane: Vec<Vec<Op>> =
            lanes.iter().map(|l| s.lane_ops(l.lane, 0, usize::MAX).unwrap()).collect();
        let syms: Vec<Vec<Op>> = (0..7).map(|q| s.symbol_ops(SymbolId::new(LaneId(1), q)).unwrap()).collect();
        (lanes, per_lane, syms, s.stats().unwrap())
    };
    let before = snapshot(&store);
    store.rebuild_index().unwrap();
    assert_eq!(snapshot(&store), before);
    assert_holds(&store, &ops);

    // The store stays writable after the rebuild.
    let more = gen.append_to(&mut store);
    store.close().unwrap();
    let store = Store::open_with(tmp.path(), small_segments()).unwrap();
    assert_eq!(store.get_op(&more.id).unwrap(), Some(more));
}

#[test]
fn append_commits_on_its_own_when_the_buffer_fills() {
    let tmp = tempfile::tempdir().unwrap();
    let opts = StoreOptions { max_buffered: 2_000, ..StoreOptions::default() };
    let mut store = Store::init_with(tmp.path(), opts).unwrap();
    let mut gen = OpGen::new(1);
    let ops: Vec<Op> = (0..200).map(|_| gen.append_to(&mut store)).collect();
    drop(store); // without an explicit commit

    let store = Store::open(tmp.path()).unwrap();
    let survived = store.op_count().unwrap();
    assert!(survived > 150 && survived < 200, "confirmed by the auto-commit: {survived}");
    assert_holds(&store, &ops[..survived as usize]);
}

#[test]
fn intents_are_durable_and_ops_can_only_cite_known_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(tmp.path()).unwrap();
    let intent = Intent::new("add a cache on user requests", Some("task #55".into()), None).unwrap();
    let kind = OpKind::FileRemove { path: "never-existed".into() };

    // An op cannot cite an intent that the store does not know.
    let op = Op::new([], LaneId::MAIN, Author::agent("a"), Hlc::new(1, 0), kind, Some(intent.id)).unwrap();
    assert!(matches!(store.append_op(&op), Err(StoreError::InvalidOp(m)) if m.contains("intent")));

    store.put_intent(&intent).unwrap();
    store.put_intent(&intent).unwrap(); // idempotent
    assert!(store.append_op(&op).unwrap());
    let mut forged = intent.clone();
    forged.summary.push('!');
    assert!(matches!(store.put_intent(&forged), Err(StoreError::InvalidOp(_))));
    store.close().unwrap();

    let store = Store::open(tmp.path()).unwrap();
    assert_eq!(store.intent(&intent.id), Some(&intent));
    assert_eq!(store.get_op(&op.id).unwrap().unwrap().intent, Some(intent.id));
    assert!(store.verify().unwrap().is_ok());
    assert!(tmp.path().join("intents").is_file());
}

#[test]
fn lanes_and_conflicts_are_durable_and_updatable() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(tmp.path()).unwrap();
    let mut gen = OpGen::new(1);
    let first = gen.append_to(&mut store);
    let checkpoint = Checkpoint {
        id: store.next_checkpoint_id(),
        head: first.id,
        label: "base".into(),
        created_by: Author::human("fra"),
        verification: None,
        git_commit: None,
        approved_by: None,
        git_merge: None,
    };
    store.put_checkpoint(&checkpoint).unwrap();

    assert_eq!(store.next_lane_id(), LaneId(1), "the main lane is lane 0 and has no record");
    let mut lane = Lane {
        id: store.next_lane_id(),
        agent: Author::agent("builder"),
        base: checkpoint.id,
        head: first.id,
        workdir: ".kito/lanes/1/work".into(),
        state: LaneState::Active,
        base_head: first.id,
        since: 0,
        task: Some("task #55".into()),
        intent: None,
        next_symbol: 0,
        scope: Vec::new(),
        owner: None,
        proposed: None,
        git_tip: None,
        parent: None,
        target: None,
    };
    store.put_lane(&lane).unwrap();
    assert_eq!(store.next_lane_id(), LaneId(2));
    // A lane cannot point to ops or checkpoints the store does not have.
    let unknown = OpId(Hash::of(b"?"));
    for bad in [
        Lane { id: LaneId(2), head: unknown, ..lane.clone() },
        Lane { id: LaneId(2), base_head: unknown, ..lane.clone() },
        Lane { id: LaneId(2), base: CheckpointId(9), ..lane.clone() },
    ] {
        assert!(matches!(store.put_lane(&bad), Err(StoreError::InvalidOp(_))));
    }

    // put_lane confirms the appended ops on its own: the head is durable.
    let second = gen.append_to(&mut store);
    lane.head = second.id;
    lane.state = LaneState::Blocked(ConflictId(1));
    store.put_lane(&lane).unwrap();

    assert_eq!(store.next_conflict_id(), ConflictId(1));
    let mut conflict = Conflict {
        id: store.next_conflict_id(),
        symbol: Some(SymbolId::new(LaneId(1), 0)),
        sides: vec![first.id, second.id],
        kind: ConflictKind::BodyBody,
        assigned_to: Some(lane.id),
        resolved_by: None,
        file: Some("src/auth.ts".into()),
        name: Some("getUser".into()),
        base_text: Some(store.put_blob(b"base").unwrap()),
        main_text: Some(store.put_blob(b"main").unwrap()),
        lane_text: Some(store.put_blob(b"lane").unwrap()),
        resolution: None,
    };
    store.put_conflict(&conflict).unwrap();
    conflict.resolution = Some(Resolution::Lane);
    conflict.resolved_by = Some(second.id);
    store.put_conflict(&conflict).unwrap();
    drop(store);

    let store = Store::open(tmp.path()).unwrap();
    assert_eq!(store.lane_records().collect::<Vec<_>>(), [&lane], "the last record of a lane wins");
    assert_eq!(store.lane_record(LaneId(1)), Some(&lane));
    assert_eq!(store.lane_record(LaneId::MAIN), None);
    assert_eq!(store.conflicts().collect::<Vec<_>>(), [&conflict]);
    assert_eq!(store.conflict(ConflictId(1)), Some(&conflict));
    assert_eq!((store.next_lane_id(), store.next_conflict_id()), (LaneId(2), ConflictId(2)));
    // The blobs the conflict cites were confirmed with it.
    assert_eq!(store.get_blob(&conflict.lane_text.unwrap()).unwrap().as_deref(), Some(&b"lane"[..]));
    assert_holds(&store, &[first, second]);
}
