//! Utilità condivise dai test di integrazione.
#![allow(dead_code)]

use kito_core::*;
use kito_store::{Store, StoreOptions};
use std::path::PathBuf;

/// Opzioni con segmenti piccoli, per esercitare il cambio di segmento.
pub fn small_segments() -> StoreOptions {
    StoreOptions { segment_size: 4 << 10, pack_size: 8 << 10, ..StoreOptions::default() }
}

pub fn lane_author(lane: LaneId) -> Author {
    Author::agent(format!("agent-{}", lane.0))
}

/// Genera una sequenza deterministica di op concatenate per lane.
pub struct OpGen {
    pub n: u64,
    heads: std::collections::HashMap<LaneId, OpId>,
    lanes: u64,
    /// Sposta i timestamp: cambia i byte dei record senza cambiarne ID e dimensioni.
    pub time_shift: u64,
}

impl OpGen {
    pub fn new(lanes: u64) -> Self {
        OpGen { n: 0, heads: Default::default(), lanes, time_shift: 0 }
    }

    /// Riparte dopo le op date, come se le avesse appena generate.
    pub fn resume(lanes: u64, ops: &[Op]) -> Self {
        let mut gen = OpGen::new(lanes);
        gen.n = ops.len() as u64;
        for op in ops {
            gen.heads.insert(op.lane, op.id);
        }
        gen
    }

    /// Contenuto deterministico e comprimibile per l'op `i`.
    pub fn content(i: u64) -> Vec<u8> {
        let lines = 1 + (i * 7) % 23;
        (0..lines)
            .flat_map(|l| format!("riga {l} della versione {i}: let x_{l} = {};\n", i * l).into_bytes())
            .collect()
    }

    /// Prossima op e blob che deve trovare nello store.
    pub fn next(&mut self) -> (Op, Vec<Vec<u8>>) {
        let i = self.n;
        self.n += 1;
        let lane = LaneId(i % self.lanes);
        let sym = SymbolId::new(lane, i % 7);
        let file = PathBuf::from(format!("src/f{}.ts", i % 13));
        let (kind, blobs) = match i % 5 {
            0 => {
                let c = Self::content(i);
                (OpKind::FileAdd { path: file, blob: BlobId::of(&c) }, vec![c])
            }
            1 => {
                let (a, b) = (Self::content(i - 1), Self::content(i));
                let edits = vec![TextEdit::new(0, a.len() as u32, b.clone())];
                (OpKind::TextEdit { file, before: BlobId::of(&a), after: BlobId::of(&b), edits }, vec![a, b])
            }
            2 => {
                let (a, b) = (Self::content(i - 1), Self::content(i));
                (
                    OpKind::ModifyBody {
                        id: sym,
                        before: BlobId::of(&a),
                        after: BlobId::of(&b),
                        edits: vec![],
                    },
                    vec![a, b],
                )
            }
            3 => (OpKind::Rename { id: sym, from: format!("f{i}"), to: format!("g{i}") }, vec![]),
            _ => (OpKind::FileRemove { path: file }, vec![]),
        };
        let parents: Vec<OpId> = self.heads.get(&lane).copied().into_iter().collect();
        let op = Op::new(
            parents,
            lane,
            lane_author(lane),
            Hlc::new(1_700_000_000_000 + self.time_shift + i, 0),
            kind,
            None,
        )
        .unwrap();
        self.heads.insert(lane, op.id);
        (op, blobs)
    }

    /// Genera la prossima op e la accoda allo store, blob compresi.
    pub fn append_to(&mut self, store: &mut Store) -> Op {
        let (op, blobs) = self.next();
        for b in &blobs {
            store.put_blob(b).unwrap();
        }
        assert!(store.append_op(&op).unwrap());
        op
    }
}

/// Verifica che lo store contenga esattamente le op date, con i loro blob,
/// e che il controllo di integrità non trovi problemi.
pub fn assert_holds(store: &Store, ops: &[Op]) {
    for op in ops {
        assert_eq!(store.get_op(&op.id).unwrap().as_ref(), Some(op), "op {} mancante o diversa", op.id);
        for blob in op.kind.blobs() {
            let data = store.get_blob(&blob).unwrap().unwrap_or_else(|| panic!("blob {blob} mancante"));
            assert_eq!(BlobId::of(&data), blob);
        }
    }
    assert_eq!(store.op_count().unwrap(), ops.len() as u64);
    let report = store.verify().unwrap();
    assert!(report.is_ok(), "verify: {:#?}", report.problems);
    assert_eq!(report.ops, ops.len() as u64);
}

/// Copia ricorsiva di una cartella.
pub fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

/// Offset di fine di ogni record di un file di log (header di 16 byte, poi
/// record `len:u32le payload crc:u32le`). Parser indipendente da quello dello store.
pub fn record_ends(file: &[u8]) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut off = 16;
    while off + 8 <= file.len() {
        let len = u32::from_le_bytes(file[off..off + 4].try_into().unwrap()) as usize;
        if off + 8 + len > file.len() {
            break;
        }
        off += 8 + len;
        ends.push(off);
    }
    ends
}

/// Segmenti di un log, in ordine.
pub fn segments(dir: &std::path::Path, sub: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir.join(sub)).unwrap().map(|e| e.unwrap().path()).collect();
    v.sort();
    v
}
