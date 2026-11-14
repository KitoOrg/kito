//! Comportamento di base dello store.

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
fn blobs_round_trip_deduplicate_and_compress() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(tmp.path()).unwrap();

    let text = "fn main() { println!(\"ciao\"); }\n".repeat(2_000).into_bytes();
    let noise: Vec<u8> = (0..5_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let cases: Vec<Vec<u8>> = vec![vec![], b"x".to_vec(), text.clone(), noise];

    let ids: Vec<BlobId> = cases.iter().map(|c| store.put_blob(c).unwrap()).collect();
    for (id, data) in ids.iter().zip(&cases) {
        assert_eq!(*id, BlobId::of(data));
        // Leggibile subito, prima del commit.
        assert_eq!(store.get_blob(id).unwrap().as_deref(), Some(&data[..]));
    }
    assert_eq!(store.put_blob(&text).unwrap(), ids[2], "stesso contenuto, stesso ID");
    assert_eq!(store.stats().unwrap().blobs, 4, "il duplicato non viene riscritto");
    assert_eq!(store.get_blob(&BlobId::of(b"assente")).unwrap(), None);
    store.close().unwrap();

    let pack = std::fs::metadata(tmp.path().join("objects/00000000.pack")).unwrap().len();
    assert!(pack < 10_000, "64 kB di testo ripetitivo devono comprimersi: pack di {pack} byte");

    let mut store = Store::open(tmp.path()).unwrap();
    for (id, data) in ids.iter().zip(&cases) {
        assert_eq!(store.get_blob(id).unwrap().as_deref(), Some(&data[..]));
    }
    assert_eq!(store.put_blob(&text).unwrap(), ids[2]);
    assert_eq!(store.stats().unwrap().blobs, 4, "deduplicato anche dopo la riapertura");
}

#[test]
fn ops_are_validated_before_being_logged() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(tmp.path()).unwrap();
    let author = Author::human("fra");
    let at = Hlc::new(1, 0);

    let blob = BlobId::of(b"contenuto");
    let add = Op::new(
        [],
        LaneId::MAIN,
        author.clone(),
        at,
        OpKind::FileAdd { path: "a.txt".into(), blob, mode: FileMode::Regular },
        None,
    )
    .unwrap();
    // Il blob non è nello store.
    assert!(matches!(store.append_op(&add), Err(StoreError::InvalidOp(m)) if m.contains("blob")));
    store.put_blob(b"contenuto").unwrap();
    assert!(store.append_op(&add).unwrap());
    assert!(!store.append_op(&add).unwrap(), "la stessa op due volte è un no-op");

    // Genitore sconosciuto.
    let orphan = Op::new(
        [OpId(Hash::of(b"?"))],
        LaneId::MAIN,
        author.clone(),
        at,
        OpKind::FileRemove { path: "a.txt".into() },
        None,
    )
    .unwrap();
    assert!(matches!(store.append_op(&orphan), Err(StoreError::InvalidOp(m)) if m.contains("genitore")));

    // ID che non corrisponde al contenuto.
    let mut forged =
        Op::new([add.id], LaneId::MAIN, author, at, OpKind::FileRemove { path: "a.txt".into() }, None)
            .unwrap();
    forged.id = OpId(Hash::of(b"falso"));
    assert!(matches!(store.append_op(&forged), Err(StoreError::InvalidOp(m)) if m.contains("ID")));

    // Gli errori di validazione non avvelenano lo store.
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
        // A round alterni si interroga prima del commit: le op accodate
        // devono essere visibili quanto quelle confermate.
        if round % 2 == 0 {
            store.commit().unwrap();
        }
        for lane in (0..3).map(LaneId) {
            let expected: Vec<&Op> = ops.iter().filter(|o| o.lane == lane).collect();
            assert_eq!(store.lane_len(lane).unwrap(), expected.len() as u64);
            assert_eq!(store.lane_head(lane).unwrap(), expected.last().map(|o| o.id));
            let all = store.lane_ops(lane, 0, usize::MAX).unwrap();
            assert_eq!(all.iter().collect::<Vec<_>>(), expected);
            // Paginazione a cavallo tra indice e op in attesa.
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
    assert_eq!(store.recovery().index, IndexRecovery::UpToDate, "dopo close() l'indice è già in pari");
    let stats = store.stats().unwrap();
    assert!(stats.oplog_segments > 10, "segmenti dell'oplog: {}", stats.oplog_segments);
    assert!(stats.packs > 3, "pack: {}", stats.packs);
    assert_holds(&store, &ops);

    let mut scanned = Vec::new();
    store
        .scan_ops(|_, op| {
            scanned.push(op);
            Ok(())
        })
        .unwrap();
    assert_eq!(scanned, ops, "scan_ops restituisce le op in ordine di log");
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
    drop(store); // nessun commit: come un crash

    let store = Store::open(tmp.path()).unwrap();
    assert_holds(&store, &confirmed);
}

#[test]
fn a_blob_larger_than_a_pack_gets_its_own_pack() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init_with(tmp.path(), small_segments()).unwrap();
    let big: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
    let small = b"piccolo".to_vec();
    let a = store.put_blob(&small).unwrap();
    let b = store.put_blob(&big).unwrap();
    let c = store.put_blob(b"dopo").unwrap();
    store.close().unwrap();
    let store = Store::open_with(tmp.path(), small_segments()).unwrap();
    assert_eq!(store.get_blob(&a).unwrap().unwrap(), small);
    assert_eq!(store.get_blob(&b).unwrap().unwrap(), big);
    assert_eq!(store.get_blob(&c).unwrap().unwrap(), b"dopo");
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
        label: "primo".into(),
        created_by: Author::human("fra"),
        verification: None,
        git_commit: None,
    };
    // put_checkpoint conferma da sé le op accodate.
    store.put_checkpoint(&ck).unwrap();
    assert_eq!(store.next_checkpoint_id(), CheckpointId(2));

    let unknown = Checkpoint { id: CheckpointId(2), head: OpId(Hash::of(b"?")), ..ck.clone() };
    assert!(matches!(store.put_checkpoint(&unknown), Err(StoreError::InvalidOp(_))));

    ck.git_commit = Some([7; 20]);
    store.put_checkpoint(&ck).unwrap();
    drop(store);

    let store = Store::open(tmp.path()).unwrap();
    assert_eq!(store.checkpoints().count(), 1);
    assert_eq!(store.checkpoint(CheckpointId(1)), Some(&ck), "vale l'ultimo record con lo stesso ID");
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

    // Lo store resta scrivibile dopo la ricostruzione.
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
    drop(store); // senza commit esplicito

    let store = Store::open(tmp.path()).unwrap();
    let survived = store.op_count().unwrap();
    assert!(survived > 150 && survived < 200, "confermate dall'auto-commit: {survived}");
    assert_holds(&store, &ops[..survived as usize]);
}

#[test]
fn intents_are_durable_and_ops_can_only_cite_known_ones() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::init(tmp.path()).unwrap();
    let intent = Intent::new("aggiungi cache sulle richieste utente", Some("task #55".into()), None).unwrap();
    let kind = OpKind::FileRemove { path: "mai-esistito".into() };

    // Un'op non può citare un intent che lo store non conosce.
    let op = Op::new([], LaneId::MAIN, Author::agent("a"), Hlc::new(1, 0), kind, Some(intent.id)).unwrap();
    assert!(matches!(store.append_op(&op), Err(StoreError::InvalidOp(m)) if m.contains("intent")));

    store.put_intent(&intent).unwrap();
    store.put_intent(&intent).unwrap(); // idempotente
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
