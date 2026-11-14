//! Test del contratto di serializzazione del modello dati.

use kito_core::*;
use proptest::prelude::*;
use std::path::PathBuf;

fn arb_hash() -> impl Strategy<Value = Hash> {
    any::<[u8; 32]>().prop_map(Hash::from_bytes)
}

fn arb_path() -> impl Strategy<Value = PathBuf> {
    prop::collection::vec("[a-zA-Z0-9_.àé-]{1,8}", 1..4).prop_map(|p| PathBuf::from(p.join("/")))
}

fn arb_sym_id() -> impl Strategy<Value = SymbolId> {
    (any::<u64>(), any::<u64>()).prop_map(|(l, s)| SymbolId::new(LaneId(l), s))
}

fn arb_author() -> impl Strategy<Value = Author> {
    (any::<bool>(), "[a-z0-9-]{1,12}").prop_map(
        |(agent, name)| {
            if agent {
                Author::agent(name)
            } else {
                Author::human(name)
            }
        },
    )
}

fn arb_edits() -> impl Strategy<Value = Vec<TextEdit>> {
    prop::collection::vec((any::<u32>(), any::<u32>(), prop::collection::vec(any::<u8>(), 0..24)), 0..4)
        .prop_map(|v| v.into_iter().map(|(a, b, t)| TextEdit::new(a.min(b), a.max(b), t)).collect())
}

fn arb_location() -> impl Strategy<Value = Location> {
    (arb_path(), any::<u32>(), any::<u32>(), any::<u32>()).prop_map(|(file, line, start, end)| Location {
        file,
        line,
        start,
        end,
    })
}

fn arb_symbol() -> impl Strategy<Value = Symbol> {
    let kind = prop::sample::select(vec![
        SymbolKind::Module,
        SymbolKind::Class,
        SymbolKind::Function,
        SymbolKind::Method,
        SymbolKind::Field,
        SymbolKind::Block,
    ]);
    let lang = prop::sample::select(vec![Lang::Python, Lang::TypeScript, Lang::Go, Lang::Rust, Lang::Text]);
    (
        arb_sym_id(),
        kind,
        lang,
        "[a-zA-Z_][a-zA-Z0-9_]{0,10}",
        prop::option::of(arb_sym_id()),
        arb_path(),
        arb_hash(),
        arb_hash(),
        prop::collection::vec(any::<u32>(), 0..16),
    )
        .prop_map(|(id, kind, lang, name, parent, file, signature, body, sketch)| Symbol {
            id,
            kind,
            lang,
            name,
            parent,
            file,
            signature,
            body,
            fingerprint: Fingerprint { sketch },
        })
}

fn arb_kind() -> impl Strategy<Value = OpKind> {
    let blob = || arb_hash().prop_map(BlobId);
    prop_oneof![
        (arb_symbol(), blob()).prop_map(|(sym, text)| OpKind::AddSymbol { sym, text }),
        arb_sym_id().prop_map(|id| OpKind::RemoveSymbol { id }),
        (arb_sym_id(), blob(), blob(), arb_edits())
            .prop_map(|(id, before, after, edits)| { OpKind::ModifyBody { id, before, after, edits } }),
        (arb_sym_id(), blob(), blob()).prop_map(|(id, before, after)| OpKind::ModifySignature {
            id,
            before,
            after
        }),
        (arb_sym_id(), "[a-z]{1,8}", "[a-z]{1,8}").prop_map(|(id, from, to)| OpKind::Rename { id, from, to }),
        (arb_sym_id(), arb_location(), arb_location()).prop_map(|(id, from, to)| OpKind::Move {
            id,
            from,
            to
        }),
        (arb_path(), blob(), blob(), arb_edits())
            .prop_map(|(file, before, after, edits)| { OpKind::TextEdit { file, before, after, edits } }),
        (arb_path(), blob()).prop_map(|(path, blob)| OpKind::FileAdd { path, blob, mode: FileMode::Regular }),
        arb_path().prop_map(|path| OpKind::FileRemove { path }),
        (arb_path(), arb_path()).prop_map(|(from, to)| OpKind::FileRename { from, to }),
    ]
}

fn arb_op() -> impl Strategy<Value = Op> {
    (
        prop::collection::vec(arb_hash().prop_map(OpId), 0..3),
        any::<u64>(),
        arb_author(),
        (any::<u64>(), any::<u32>()),
        arb_kind(),
        prop::option::of(arb_hash().prop_map(IntentId)),
    )
        .prop_map(|(parents, lane, author, (wall, logical), kind, intent)| {
            Op::new(parents, LaneId(lane), author, Hlc::new(wall, logical), kind, intent).unwrap()
        })
}

proptest! {
    #[test]
    fn op_round_trips_through_cbor(op in arb_op()) {
        let bytes = codec::to_vec(&op).unwrap();
        let back: Op = codec::from_slice(&bytes).unwrap();
        prop_assert_eq!(&back, &op);
        prop_assert!(back.verify_id());
        // Codifica deterministica: ricodificare dà gli stessi byte.
        prop_assert_eq!(codec::to_vec(&back).unwrap(), bytes);
    }

    #[test]
    fn op_round_trips_through_json(op in arb_op()) {
        let json = serde_json::to_string(&op).unwrap();
        let back: Op = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, op);
    }

    #[test]
    fn op_id_ignores_metadata_but_not_content(op in arb_op(), other in arb_kind()) {
        let mut moved = op.clone();
        moved.lane = LaneId(op.lane.0.wrapping_add(1));
        moved.at = Hlc::new(op.at.wall_ms.wrapping_add(1), 0);
        moved.intent = None;
        prop_assert!(moved.verify_id());

        if other != op.kind {
            let changed = Op::new(op.parents.clone(), op.lane, op.author.clone(), op.at, other, op.intent).unwrap();
            prop_assert_ne!(changed.id, op.id);
        }
        let mut tampered = op.clone();
        tampered.author.name.push('x');
        prop_assert!(!tampered.verify_id());
    }

    #[test]
    fn truncated_cbor_never_decodes_as_an_op(op in arb_op(), cut in any::<prop::sample::Index>()) {
        let bytes = codec::to_vec(&op).unwrap();
        let n = cut.index(bytes.len());
        prop_assert!(codec::from_slice::<Op>(&bytes[..n]).is_err());
    }
}

fn sample_op() -> Op {
    let parent = OpId(Hash::of(b"parent"));
    Op::new(
        [parent],
        LaneId(0xb2),
        Author::agent("agent-7"),
        Hlc::new(1_794_000_000_000, 3),
        OpKind::TextEdit {
            file: PathBuf::from("src/billing.ts"),
            before: BlobId::of(b"before"),
            after: BlobId::of(b"after"),
            edits: vec![TextEdit::new(0, 6, "after")],
        },
        None,
    )
    .unwrap()
}

/// Il formato su disco e gli ID sono un contratto: se questo test fallisce, i
/// record già scritti non sono più leggibili o cambiano identità. Serve un ADR.
#[test]
fn golden_encoding_is_stable() {
    let op = sample_op();
    let bytes = codec::to_vec(&op).unwrap();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if std::env::var_os("KITO_PRINT_GOLDEN").is_some() {
        println!("GOLDEN id={} cbor={hex}", op.id);
    }
    assert_eq!(op.id.to_string(), GOLDEN_OP_ID);
    assert_eq!(hex, GOLDEN_OP_CBOR);
}

#[test]
fn hashes_are_byte_strings_in_cbor_and_hex_in_json() {
    let id = BlobId::of(b"x");
    let cbor = codec::to_vec(&id).unwrap();
    assert_eq!(cbor.len(), 34, "byte string di 32 byte: 2 byte di intestazione");
    assert_eq!(&cbor[..2], &[0x58, 32]);
    assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{id}\""));
}

#[test]
fn absent_intent_is_not_encoded() {
    // Un campo opzionale assente non occupa spazio e non altera la codifica:
    // è la proprietà che permette di aggiungere campi senza rompere i record.
    let op = sample_op();
    let json = serde_json::to_value(&op).unwrap();
    assert!(json.get("intent").is_none());
    let mut with_intent = op.clone();
    with_intent.intent = Some(IntentId(Hash::of(b"i")));
    assert!(serde_json::to_value(&with_intent).unwrap().get("intent").is_some());
}

#[test]
fn unknown_fields_are_ignored_when_reading() {
    // Un record scritto da una versione futura, con un campo in più, si legge.
    let mut v = serde_json::to_value(sample_op()).unwrap();
    v.as_object_mut().unwrap().insert("campo_futuro".into(), serde_json::json!(42));
    let back: Op = serde_json::from_value(v).unwrap();
    assert_eq!(back, sample_op());
}

#[test]
fn checkpoint_and_friends_round_trip() {
    let ck = Checkpoint {
        id: CheckpointId(119),
        head: OpId(Hash::of(b"h")),
        label: "integra lane/b2".into(),
        created_by: Author::human("francesco"),
        verification: Some(Verification {
            commands: vec!["tsc".into(), "npm test".into()],
            passed: true,
            duration_ms: 8200,
        }),
        git_commit: Some([0xab; 20]),
    };
    let back: Checkpoint = codec::from_slice(&codec::to_vec(&ck).unwrap()).unwrap();
    assert_eq!(back, ck);
    let json = serde_json::to_value(&ck).unwrap();
    assert_eq!(json["git_commit"], serde_json::json!("ab".repeat(20)));
    assert_eq!(serde_json::from_value::<Checkpoint>(json).unwrap(), ck);

    let mut none = ck.clone();
    none.git_commit = None;
    assert_eq!(codec::from_slice::<Checkpoint>(&codec::to_vec(&none).unwrap()).unwrap(), none);

    let lane = Lane {
        id: LaneId(2),
        agent: Author::agent("a"),
        base: CheckpointId(1),
        head: OpId(Hash::of(b"h")),
        workdir: PathBuf::from(".kito/lanes/2/work"),
        state: LaneState::Blocked(ConflictId(4)),
    };
    assert_eq!(codec::from_slice::<Lane>(&codec::to_vec(&lane).unwrap()).unwrap(), lane);

    let conflict = Conflict {
        id: ConflictId(4),
        symbol: Some(SymbolId::new(LaneId(2), 9)),
        sides: vec![OpId(Hash::of(b"a")), OpId(Hash::of(b"b"))],
        kind: ConflictKind::BodyBody,
        assigned_to: Some(LaneId(2)),
        resolved_by: None,
    };
    assert_eq!(codec::from_slice::<Conflict>(&codec::to_vec(&conflict).unwrap()).unwrap(), conflict);

    let reference = Reference {
        from: SymbolId::new(LaneId(0), 1),
        to: SymbolId::new(LaneId(0), 2),
        site: Location { file: PathBuf::from("a.ts"), line: 41, start: 10, end: 17 },
        confidence: 0.9,
        source: RefSource::Lsp("typescript-language-server".into()),
    };
    assert_eq!(codec::from_slice::<Reference>(&codec::to_vec(&reference).unwrap()).unwrap(), reference);

    let intent = Intent::new("aggiungi cache sulle richieste utente", Some("task #55".into()), None).unwrap();
    assert_eq!(codec::from_slice::<Intent>(&codec::to_vec(&intent).unwrap()).unwrap(), intent);
    assert_ne!(intent.id, Intent::new("altro", None, None).unwrap().id);
}

#[test]
fn text_edit_bytes_survive_json_even_if_not_utf8() {
    let e = TextEdit::new(1, 2, vec![0xff, 0xfe, 0x00]);
    let back: TextEdit = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    assert_eq!(back, e);
    let e = TextEdit::new(1, 2, "ciao è");
    assert_eq!(serde_json::to_value(&e).unwrap()["text"], serde_json::json!("ciao è"));
    let back: TextEdit = codec::from_slice(&codec::to_vec(&e).unwrap()).unwrap();
    assert_eq!(back, e);
}

#[test]
fn fingerprint_similarity() {
    let a = Fingerprint { sketch: vec![1, 2, 3, 4, 5] };
    let b = Fingerprint { sketch: vec![1, 2, 3, 4, 9] };
    assert!((a.similarity(&b) - 0.8).abs() < 1e-6);
    assert_eq!(a.similarity(&Fingerprint::default()), 0.0);
    assert_eq!(Fingerprint::default().similarity(&Fingerprint::default()), 0.0);
}

#[test]
fn op_kind_reports_symbol_and_blobs() {
    let id = SymbolId::new(LaneId(1), 2);
    let k = OpKind::ModifyBody { id, before: BlobId::of(b"a"), after: BlobId::of(b"b"), edits: vec![] };
    assert_eq!(k.symbol(), Some(id));
    assert_eq!(k.blobs().as_slice(), &[BlobId::of(b"a"), BlobId::of(b"b")]);
    let k = OpKind::FileRemove { path: "x".into() };
    assert_eq!(k.symbol(), None);
    assert!(k.blobs().is_empty());
    assert_eq!(k.name(), "FileRemove");
}

const GOLDEN_OP_ID: &str = "7614020c4e17fe74dda9802047303ed7fba5243086e7c47ec6818c584e41dcc1";
const GOLDEN_OP_CBOR: &str = concat!(
    "a662696458207614020c4e17fe74dda9802047303ed7fba5243086e7c47ec6818c584e41dcc167706172656e74738158",
    "20ebdea6058df2230dc25b7a7c7b487b470c508c2e0a5119c96893c443de3a9e79646c616e6518b266617574686f72a2",
    "646b696e64654167656e74646e616d65676167656e742d37626174a26777616c6c5f6d731b000001a1b2bb9400676c6f",
    "676963616c03646b696e64a1685465787445646974a46466696c656e7372632f62696c6c696e672e7473666265666f72",
    "65582018c52a7ed981fbffa12fbf611b593c451f6a5700af2206c612b5c3dd2888909b656166746572582045df22c72c",
    "6e776f178bdbb8103c5cac32795afaf8d736fb12c848264ae87d1165656469747381a36573746172740063656e640664",
    "74657874456166746572",
);
