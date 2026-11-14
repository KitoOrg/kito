//! I sei concetti del modello dati: simbolo, operazione, riferimento, lane,
//! conflitto e checkpoint.

use crate::codec::{self, CodecError};
use crate::edit::TextEdit;
use crate::hash::Hash;
use crate::hlc::Hlc;
use crate::ids::{BlobId, CheckpointId, ConflictId, IntentId, LaneId, OpId, SymbolId};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::path::PathBuf;

/// Contesti BLAKE3 `derive_key` degli hash strutturati.
const OP_CONTEXT: &str = "kito 2026 op id v1";
const INTENT_CONTEXT: &str = "kito 2026 intent id v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Lang {
    Python,
    JavaScript,
    TypeScript,
    Java,
    CSharp,
    C,
    Cpp,
    Go,
    Php,
    Rust,
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SymbolKind {
    Module,
    Class,
    Struct,
    Enum,
    Interface,
    Function,
    Method,
    Field,
    Const,
    Var,
    TypeAlias,
    /// Fallback.
    Block,
}

/// Impronta strutturale di un simbolo, per il matching tra versioni.
///
/// È uno sketch MinHash: due impronte della stessa lunghezza si confrontano
/// posizione per posizione e la frazione di posizioni uguali stima la
/// somiglianza di Jaccard tra le strutture. Uno sketch vuoto significa
/// "sconosciuta" e non somiglia a niente.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fingerprint {
    pub sketch: Vec<u32>,
}

impl Fingerprint {
    /// Somiglianza stimata, tra 0.0 e 1.0.
    pub fn similarity(&self, other: &Fingerprint) -> f32 {
        if self.sketch.is_empty() || self.sketch.len() != other.sketch.len() {
            return 0.0;
        }
        let same = self.sketch.iter().zip(&other.sketch).filter(|(a, b)| a == b).count();
        same as f32 / self.sketch.len() as f32
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub id: SymbolId,
    pub kind: SymbolKind,
    pub lang: Lang,
    pub name: String,
    /// classe → metodo
    pub parent: Option<SymbolId>,
    /// posizione attuale
    pub file: PathBuf,
    /// firma normalizzata
    pub signature: Hash,
    /// corpo normalizzato (senza spazi e commenti)
    pub body: Hash,
    /// per il matching tra versioni
    pub fingerprint: Fingerprint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AuthorKind {
    Agent,
    Human,
}

/// Chi ha prodotto un'operazione: un agente o un umano.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Author {
    pub kind: AuthorKind,
    pub name: String,
}

impl Author {
    pub fn agent(name: impl Into<String>) -> Self {
        Author { kind: AuthorKind::Agent, name: name.into() }
    }

    pub fn human(name: impl Into<String>) -> Self {
        Author { kind: AuthorKind::Human, name: name.into() }
    }
}

/// Un punto in un file: riga (da 1) e intervallo di byte.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Location {
    pub file: PathBuf,
    pub line: u32,
    pub start: u32,
    pub end: u32,
}

/// Tipo di un file nell'albero: quanto basta per esportarlo fedelmente in git.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FileMode {
    #[default]
    Regular,
    Executable,
    /// Il blob contiene il percorso di destinazione del link.
    Symlink,
}

impl FileMode {
    pub fn is_regular(&self) -> bool {
        *self == FileMode::Regular
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub id: OpId,
    /// DAG
    pub parents: SmallVec<[OpId; 2]>,
    pub lane: LaneId,
    /// agente o umano
    pub author: Author,
    /// hybrid logical clock
    pub at: Hlc,
    pub kind: OpKind,
    /// il "perché"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<IntentId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OpKind {
    AddSymbol {
        sym: Symbol,
        text: BlobId,
    },
    RemoveSymbol {
        id: SymbolId,
    },
    ModifyBody {
        id: SymbolId,
        before: BlobId,
        after: BlobId,
        edits: Vec<TextEdit>,
    },
    ModifySignature {
        id: SymbolId,
        before: BlobId,
        after: BlobId,
    },
    Rename {
        id: SymbolId,
        from: String,
        to: String,
    },
    Move {
        id: SymbolId,
        from: Location,
        to: Location,
    },
    // Fallback: file non analizzabili e testo fuori dai simboli (import, commenti)
    TextEdit {
        file: PathBuf,
        before: BlobId,
        after: BlobId,
        edits: Vec<TextEdit>,
    },
    FileAdd {
        path: PathBuf,
        blob: BlobId,
        /// Tipo di file. Assente nella codifica quando è un file normale.
        #[serde(default, skip_serializing_if = "FileMode::is_regular")]
        mode: FileMode,
    },
    FileRemove {
        path: PathBuf,
    },
    FileRename {
        from: PathBuf,
        to: PathBuf,
    },
}

impl Op {
    /// Costruisce un'operazione calcolandone l'ID.
    pub fn new(
        parents: impl IntoIterator<Item = OpId>,
        lane: LaneId,
        author: Author,
        at: Hlc,
        kind: OpKind,
        intent: Option<IntentId>,
    ) -> Result<Op, CodecError> {
        let parents: SmallVec<[OpId; 2]> = parents.into_iter().collect();
        let id = Op::compute_id(&parents, &kind, &author)?;
        Ok(Op { id, parents, lane, author, at, kind, intent })
    }

    /// Hash di (parents, kind, payload, author).
    ///
    /// `lane`, `at` e `intent` sono metadati e non entrano nell'ID: la stessa
    /// modifica, dello stesso autore, sugli stessi genitori è la stessa
    /// operazione ovunque e in qualunque momento venga registrata.
    pub fn compute_id(parents: &[OpId], kind: &OpKind, author: &Author) -> Result<OpId, CodecError> {
        let bytes = codec::to_vec(&(parents, kind, author))?;
        Ok(OpId(Hash::derive(OP_CONTEXT, &bytes)))
    }

    /// `true` se l'ID registrato corrisponde al contenuto.
    pub fn verify_id(&self) -> bool {
        matches!(Op::compute_id(&self.parents, &self.kind, &self.author), Ok(id) if id == self.id)
    }

    pub fn is_root(&self) -> bool {
        self.parents.is_empty()
    }
}

impl OpKind {
    /// Simbolo toccato dall'operazione, se è un'operazione simbolica.
    pub fn symbol(&self) -> Option<SymbolId> {
        match self {
            OpKind::AddSymbol { sym, .. } => Some(sym.id),
            OpKind::RemoveSymbol { id }
            | OpKind::ModifyBody { id, .. }
            | OpKind::ModifySignature { id, .. }
            | OpKind::Rename { id, .. }
            | OpKind::Move { id, .. } => Some(*id),
            OpKind::TextEdit { .. }
            | OpKind::FileAdd { .. }
            | OpKind::FileRemove { .. }
            | OpKind::FileRename { .. } => None,
        }
    }

    /// Blob a cui l'operazione fa riferimento: devono esistere nello store
    /// prima che l'operazione venga registrata.
    pub fn blobs(&self) -> SmallVec<[BlobId; 2]> {
        match self {
            OpKind::AddSymbol { text, .. } => SmallVec::from_slice(&[*text]),
            OpKind::ModifyBody { before, after, .. }
            | OpKind::ModifySignature { before, after, .. }
            | OpKind::TextEdit { before, after, .. } => SmallVec::from_slice(&[*before, *after]),
            OpKind::FileAdd { blob, .. } => SmallVec::from_slice(&[*blob]),
            OpKind::RemoveSymbol { .. }
            | OpKind::Rename { .. }
            | OpKind::Move { .. }
            | OpKind::FileRemove { .. }
            | OpKind::FileRename { .. } => SmallVec::new(),
        }
    }

    /// Nome della variante, per log e output.
    pub fn name(&self) -> &'static str {
        match self {
            OpKind::AddSymbol { .. } => "AddSymbol",
            OpKind::RemoveSymbol { .. } => "RemoveSymbol",
            OpKind::ModifyBody { .. } => "ModifyBody",
            OpKind::ModifySignature { .. } => "ModifySignature",
            OpKind::Rename { .. } => "Rename",
            OpKind::Move { .. } => "Move",
            OpKind::TextEdit { .. } => "TextEdit",
            OpKind::FileAdd { .. } => "FileAdd",
            OpKind::FileRemove { .. } => "FileRemove",
            OpKind::FileRename { .. } => "FileRename",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RefSource {
    TreeSitterHeuristic,
    /// Nome del language server.
    Lsp(String),
    Scip,
}

/// Riferimento tra simboli (livello 2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    /// chi usa
    pub from: SymbolId,
    /// cosa è usato
    pub to: SymbolId,
    /// dove, nel file di `from`
    pub site: Location,
    /// 0.0–1.0
    pub confidence: f32,
    pub source: RefSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub id: IntentId,
    /// "aggiungi cache sulle richieste utente"
    pub summary: String,
    /// es. "task #55"
    pub task: Option<String>,
    /// prompt completo, opzionale
    pub prompt: Option<BlobId>,
}

impl Intent {
    /// Costruisce un intent calcolandone l'ID dal contenuto.
    pub fn new(
        summary: impl Into<String>,
        task: Option<String>,
        prompt: Option<BlobId>,
    ) -> Result<Intent, CodecError> {
        let summary = summary.into();
        let bytes = codec::to_vec(&(&summary, &task, &prompt))?;
        Ok(Intent { id: IntentId(Hash::derive(INTENT_CONTEXT, &bytes)), summary, task, prompt })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LaneState {
    Active,
    Ready,
    Integrating,
    Blocked(ConflictId),
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lane {
    pub id: LaneId,
    pub agent: Author,
    pub base: CheckpointId,
    pub head: OpId,
    pub workdir: PathBuf,
    pub state: LaneState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConflictKind {
    BodyBody,
    RemoveModify,
    RenameRename,
    SemanticRef,
    Text,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: ConflictId,
    /// None = conflitto testuale su file
    pub symbol: Option<SymbolId>,
    pub sides: Vec<OpId>,
    pub kind: ConflictKind,
    pub assigned_to: Option<LaneId>,
    pub resolved_by: Option<OpId>,
}

/// Esito del verifier: comandi, esito, durata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub commands: Vec<String>,
    pub passed: bool,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub head: OpId,
    pub label: String,
    pub created_by: Author,
    pub verification: Option<Verification>,
    /// dopo l'export
    #[serde(with = "crate::serde_helpers::opt_oid20")]
    pub git_commit: Option<[u8; 20]>,
}
