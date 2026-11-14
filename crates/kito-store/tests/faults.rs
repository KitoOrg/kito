//! Fault injection: file troncati o danneggiati, disco pieno, fsync falliti,
//! perdita di corrente. La regola: nessuna op confermata va persa, e un danno
//! a dati confermati non viene mai nascosto.

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

/// Crea uno store con `n` op confermate e lo chiude.
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

/// Uno store in cui l'indice è rimasto a `first` op mentre il log ne ha
/// `first + more`: è lo stato dopo un crash arrivato tra due sincronizzazioni
/// dell'indice. Restituisce le op e l'offset fin dove l'indice le dava per
/// confermate.
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

    // Ogni lunghezza possibile da "tutto ciò che era confermato" a "tutto".
    for cut in confirmed_len..=log.len() {
        let dir = tmp.path().join("cut");
        let _ = fs::remove_dir_all(&dir);
        copy_dir(&base, &dir);
        truncate(&dir.join("oplog/00000000.seg"), cut);

        let mut store = Store::open(&dir).unwrap_or_else(|e| panic!("taglio a {cut}: {e}"));
        let complete = ends.iter().filter(|&&e| e <= cut).count();
        let valid_end = ends[complete - 1];
        let recovery = store.recovery().clone();
        assert_eq!(recovery.oplog.discarded_bytes, (cut - valid_end) as u64, "taglio a {cut}");
        if cut > valid_end {
            let saved = recovery.oplog.saved_to.as_ref().expect("coda salvata in lost+found");
            assert_eq!(fs::read(saved).unwrap(), &log[valid_end..cut]);
        }
        assert_eq!(
            recovery.index,
            IndexRecovery::CaughtUp { ops: (complete - 6) as u64, blobs: recovery_blobs(&recovery) }
        );
        assert_holds(&store, &ops[..complete]);

        // Dopo il recupero lo store si usa normalmente (controllato sui
        // confini dei record e a campione, per non rifare fsync a ogni byte).
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

        // Senza riparazione: errore, e il file non viene toccato.
        match Store::open(&dir) {
            Err(StoreError::Corrupt { reason, .. }) => {
                assert!(reason.contains("riparazione"), "{reason}")
            }
            other => panic!("taglio a {cut}: atteso Corrupt, ottenuto {:?}", other.map(|_| ())),
        }
        assert_eq!(fs::metadata(&seg).unwrap().len(), cut as u64);
        assert!(!dir.join("lost+found").exists());

        // Con riparazione: sopravvive il prefisso integro, l'indice è rifatto.
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

    // Il recupero è idempotente: la seconda apertura non trova più nulla.
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
    flip_byte(victim, 16 + 40); // dentro il primo record del segmento

    let store = Store::open_with(dir, small_segments()).unwrap();
    assert_eq!(fs::metadata(victim).unwrap().len(), before, "un segmento sigillato non viene mai troncato");

    let mut unreadable = Vec::new();
    for op in &ops {
        match store.get_op(&op.id) {
            Ok(found) => assert_eq!(found.as_ref(), Some(op)),
            Err(StoreError::Corrupt { .. }) => unreadable.push(op.id),
            Err(e) => panic!("errore inatteso: {e}"),
        }
    }
    assert_eq!(unreadable.len(), 1, "solo l'op danneggiata è illeggibile");
    let report = store.verify().unwrap();
    assert!(!report.is_ok());
    assert!(report.problems.iter().any(|p| p.contains("oplog")), "{:?}", report.problems);
    drop(store);

    // Senza indice bisognerebbe rileggere tutto il log: l'errore emerge subito.
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
        assert_eq!(fs::metadata(dir.join(file)).unwrap().len(), len_before, "{file}: nessun troncamento");

        let store = Store::open_with(&dir, repair(StoreOptions::default())).unwrap();
        assert!(store.recovery().discarded_bytes() > 0);
        if file.starts_with("oplog") {
            let complete = ends.iter().filter(|&&e| e <= at).count();
            assert_holds(&store, &ops[..complete]);
        } else {
            // Persi dei blob: le op restano, e verify dice quali sono rimaste orfane.
            assert_eq!(store.op_count().unwrap(), 30);
            let report = store.verify().unwrap();
            assert!(report.problems.iter().any(|p| p.contains("assente dai pack")), "{:?}", report.problems);
        }
    }
}

#[test]
fn missing_segments_are_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("base");
    let ops = build(&base, &small_segments(), 200);
    let all = segments(&base, "oplog");

    // Manca un segmento in mezzo.
    let dir = tmp.path().join("hole");
    copy_dir(&base, &dir);
    fs::remove_file(segments(&dir, "oplog")[2].clone()).unwrap();
    match Store::open_with(&dir, small_segments()) {
        Err(StoreError::Corrupt { reason, .. }) => {
            assert!(reason.contains("manca il segmento 2"), "{reason}")
        }
        other => panic!("atteso Corrupt, ottenuto {:?}", other.map(|_| ())),
    }

    // Mancano gli ultimi segmenti: dati confermati spariti.
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

    // Indice di un altro store con log della stessa forma: le posizioni
    // sarebbero plausibili, ma l'ultima op indicizzata non corrisponde.
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
        matches!(&r, IndexRecovery::Rebuilt { reason, .. } if reason.contains("non è più nel log")),
        "{r:?}"
    );
}

/// Prova ad accodare e confermare op finché una scrittura fallisce.
/// Restituisce le op confermate e quelle tentate.
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
    // Lo spazio finisce in punti diversi: a metà di un pack, di un segmento
    // dell'oplog, o alla creazione di un nuovo segmento.
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
            assert!(err.is_storage_full(), "spazio {space}: {err}");
            assert!(store.is_poisoned());
            assert!(matches!(store.commit(), Err(StoreError::Poisoned)));
            assert!(matches!(store.put_blob(b"x"), Err(StoreError::Poisoned)));
            // Le letture di ciò che era confermato continuano a funzionare.
            for op in &confirmed {
                assert_eq!(store.get_op(&op.id).unwrap().as_ref(), Some(op));
            }
        }
        drop(store);

        // Liberato lo spazio, lo store si riapre con tutte le op confermate.
        let mut store = Store::open_with(&dir, small_segments()).unwrap();
        let survived = store.op_count().unwrap() as usize;
        assert!(survived >= confirmed.len(), "spazio {space}: {survived} < {}", confirmed.len());
        let mut regen = OpGen::new(3);
        let all: Vec<Op> = (0..20 + attempted.len()).map(|_| regen.next().0).collect();
        assert_holds(&store, &all[..survived]);

        let mut gen = OpGen::resume(3, &all[..survived]);
        let next = gen.append_to(&mut store);
        store.close().unwrap();
        assert_eq!(Store::open_with(&dir, small_segments()).unwrap().get_op(&next.id).unwrap(), Some(next));
    }
    assert!(failures > 20, "il disco deve riempirsi nella maggior parte dei casi: {failures}");
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
        let err = err.expect("un fsync deve fallire");
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

    // Lo spazio finisce a metà scrittura e nemmeno il truncate riesce: nei
    // file resta un record a metà.
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
    /// Riapertura pulita.
    Reopen,
    /// Il processo muore: ciò che non era stato scritto nei file è perso.
    Kill,
    /// Salta la corrente: anche i byte scritti ma non sincronizzati sono a rischio.
    PowerLoss {
        fates: Vec<(u8, u64)>,
        lose_index: bool,
    },
    /// Un fsync fallisce durante un commit, poi salta la corrente.
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

    /// Modello: qualunque sequenza di scritture, commit, crash e perdite di
    /// corrente lascia nello store un prefisso della storia che contiene
    /// almeno tutte le op confermate, con blob e indici coerenti.
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
        // `history`: tutte le op accodate; le prime `confirmed` sono confermate.
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
                    // Se il commit ha bisogno di meno fsync di quelli concessi
                    // (per esempio un'op senza blob), riesce: è un commit normale.
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
                prop_assert!(survived >= confirmed, "perse op confermate: {} < {}", survived, confirmed);
                prop_assert!(survived <= history.len());
                history.truncate(survived);
                assert_holds(&reopened, &history);
                // Tutto ciò che è sopravvissuto alla riapertura è ora durevole.
                confirmed = survived;
                gen = OpGen::resume(LANES, &history);
                store = Some(reopened);
            }
        }
    }
}
