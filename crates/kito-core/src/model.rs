//! The six concepts of the data model: symbol, operation, reference, lane,
//! conflict and checkpoint.

use crate::codec::{self, CodecError};
use crate::edit::TextEdit;
use crate::hash::Hash;
use crate::hlc::Hlc;
use crate::ids::{BlobId, CheckpointId, ConflictId, IntentId, LaneId, OpId, SymbolId};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::path::PathBuf;

/// BLAKE3 `derive_key` contexts of the structured hashes.
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

/// Structural fingerprint of a symbol, for matching across versions.
///
/// It is a MinHash sketch: two fingerprints of the same length are compared
/// position by position, and the fraction of equal positions estimates the
/// Jaccard similarity between the structures. An empty sketch means
/// "unknown" and is similar to nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fingerprint {
    pub sketch: Vec<u32>,
}

impl Fingerprint {
    /// Estimated similarity, between 0.0 and 1.0.
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
    /// class → method
    pub parent: Option<SymbolId>,
    /// current location
    pub file: PathBuf,
    /// normalized signature
    pub signature: Hash,
    /// normalized body (without whitespace and comments)
    pub body: Hash,
    /// for matching across versions
    pub fingerprint: Fingerprint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AuthorKind {
    Agent,
    Human,
}

/// Who produced an operation: an agent or a human.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Author {
    pub kind: AuthorKind,
    pub name: String,
    /// Email address, used to attribute the commits exported to git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl Author {
    pub fn agent(name: impl Into<String>) -> Self {
        Author { kind: AuthorKind::Agent, name: name.into(), email: None }
    }

    pub fn human(name: impl Into<String>) -> Self {
        Author { kind: AuthorKind::Human, name: name.into(), email: None }
    }

    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }
}

impl std::fmt::Display for Author {
    /// `Name <email>` if the email is known, otherwise just the name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.email {
            Some(email) => write!(f, "{} <{email}>", self.name),
            None => f.write_str(&self.name),
        }
    }
}

/// A point in a file: line (1-based) and byte range.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Location {
    pub file: PathBuf,
    pub line: u32,
    pub start: u32,
    pub end: u32,
}

/// Type of a file in the tree: just enough to export it faithfully to git.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FileMode {
    #[default]
    Regular,
    Executable,
    /// The blob contains the link's target path.
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
    /// agent or human
    pub author: Author,
    /// hybrid logical clock
    pub at: Hlc,
    pub kind: OpKind,
    /// the "why"
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
        /// The enclosing symbol after the move; `None`: top level. Absent
        /// from the encoding when `None`, as in the ops written before the
        /// field existed (which only moved top-level symbols).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<SymbolId>,
    },
    // Fallback: files that cannot be parsed and text outside symbols (imports, comments)
    TextEdit {
        file: PathBuf,
        before: BlobId,
        after: BlobId,
        edits: Vec<TextEdit>,
    },
    FileAdd {
        path: PathBuf,
        blob: BlobId,
        /// File type. Absent from the encoding when it is a regular file.
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
    /// Builds an operation, computing its ID.
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

    /// Hash of (parents, kind, payload, author).
    ///
    /// `lane`, `at` and `intent` are metadata and do not go into the ID: the
    /// same change, by the same author, on the same parents is the same
    /// operation wherever and whenever it is recorded.
    pub fn compute_id(parents: &[OpId], kind: &OpKind, author: &Author) -> Result<OpId, CodecError> {
        let bytes = codec::to_vec(&(parents, kind, author))?;
        Ok(OpId(Hash::derive(OP_CONTEXT, &bytes)))
    }

    /// `true` if the recorded ID matches the content.
    pub fn verify_id(&self) -> bool {
        matches!(Op::compute_id(&self.parents, &self.kind, &self.author), Ok(id) if id == self.id)
    }

    pub fn is_root(&self) -> bool {
        self.parents.is_empty()
    }
}

impl OpKind {
    /// Symbol touched by the operation, if it is a symbolic operation.
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

    /// Blobs the operation refers to: they must exist in the store before
    /// the operation is recorded.
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

    /// Paths of the files whose state in the tree changes as an effect of the op.
    pub fn paths(&self) -> SmallVec<[&PathBuf; 2]> {
        match self {
            OpKind::FileAdd { path, .. } | OpKind::FileRemove { path } => SmallVec::from_slice(&[path]),
            OpKind::FileRename { from, to } => SmallVec::from_slice(&[from, to]),
            OpKind::TextEdit { file, .. } => SmallVec::from_slice(&[file]),
            OpKind::AddSymbol { .. }
            | OpKind::RemoveSymbol { .. }
            | OpKind::ModifyBody { .. }
            | OpKind::ModifySignature { .. }
            | OpKind::Rename { .. }
            | OpKind::Move { .. } => SmallVec::new(),
        }
    }

    /// Name of the variant, for logs and output.
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
    /// Name of the language server.
    Lsp(String),
    Scip,
}

/// Reference between symbols (level 2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    /// who uses
    pub from: SymbolId,
    /// what is used
    pub to: SymbolId,
    /// where, in the file of `from`
    pub site: Location,
    /// 0.0–1.0
    pub confidence: f32,
    pub source: RefSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub id: IntentId,
    /// "add a cache for user requests"
    pub summary: String,
    /// e.g. "task #55"
    pub task: Option<String>,
    /// full prompt, optional
    pub prompt: Option<BlobId>,
}

impl Intent {
    /// Builds an intent, computing its ID from the content.
    pub fn new(
        summary: impl Into<String>,
        task: Option<String>,
        prompt: Option<BlobId>,
    ) -> Result<Intent, CodecError> {
        let summary = summary.into();
        let bytes = codec::to_vec(&(&summary, &task, &prompt))?;
        Ok(Intent { id: IntentId(Hash::derive(INTENT_CONTEXT, &bytes)), summary, task, prompt })
    }

    /// `true` if the recorded ID matches the content.
    pub fn verify_id(&self) -> bool {
        matches!(Intent::new(self.summary.clone(), self.task.clone(), self.prompt), Ok(i) if i.id == self.id)
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

/// The program an agent runs in while it works in a lane, and the model,
/// as that program or the agent said them (ADR 0010). A label for whoever
/// looks at the lane: nothing is decided by it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assistant {
    /// How the program names itself: `claude-code`, `codex`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The model it runs: `claude-opus-5`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// What is said of the assistant at work, at one moment: all of it, or
/// the part that whoever says it knows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantSaid {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A name that was worked out, not said (the mark Claude Code leaves
    /// in the environment of what it runs). It names the program only
    /// where nothing else did; where a program named itself otherwise,
    /// what comes with the guess is another program's, and is not taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guess: Option<String>,
}

/// Whether two names are of the same program: `codex` and
/// `codex-mcp-client` are, `claude-code` and `claude-ai` are not.
fn same_program(a: &str, b: &str) -> bool {
    let within = |long: &str, short: &str| long.strip_prefix(short).is_some_and(|rest| rest.starts_with('-'));
    a == b || within(a, b) || within(b, a)
}

impl AssistantSaid {
    /// The longest name, version or model that is kept.
    pub const LONGEST: usize = 64;

    /// What was said, as it is kept: short, and written with what a name,
    /// a version or a model are written with. The rest is dropped: these
    /// words are shown to people and read by other agents, and come from
    /// outside.
    pub fn clean(self) -> AssistantSaid {
        let word = |text: Option<String>, name: bool| {
            let text = text?.trim().to_owned();
            let text = match name {
                true => text.to_lowercase().replace([' ', '_'], "-"),
                false => text,
            };
            let written = |c: char| c.is_ascii_alphanumeric() || " ._:/+@()[]-".contains(c);
            (!text.is_empty() && text.len() <= Self::LONGEST && text.chars().all(written)).then_some(text)
        };
        AssistantSaid {
            name: word(self.name, true),
            version: word(self.version, false),
            model: word(self.model, false),
            guess: word(self.guess, true),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.version.is_none() && self.model.is_none() && self.guess.is_none()
    }

    /// This, completed by what is said later of the same program; another
    /// program's word stands alone.
    pub fn with(self, later: AssistantSaid) -> AssistantSaid {
        match (&self.name, &later.name) {
            (Some(known), Some(named)) if !same_program(known, named) => later,
            _ => AssistantSaid {
                name: self.name.or(later.name),
                version: later.version.or(self.version),
                model: later.model.or(self.model),
                guess: later.guess.or(self.guess),
            },
        }
    }
}

impl Assistant {
    /// Takes in what is said now. Another program replaces what was known
    /// of the one before; the same one, or none named, completes it. A
    /// model or a version alone says nothing of a lane no program was
    /// named for. A name that was only worked out names a lane nobody
    /// named; against a name that was said, it and what comes with it are
    /// left out. Answers whether anything changed.
    pub fn told(known: &mut Option<Assistant>, said: AssistantSaid) -> bool {
        let said = said.clean();
        let before = known.clone();
        let name = match (said.name, said.guess, known.as_ref()) {
            (Some(name), ..) => Some(name),
            (None, Some(guess), None) => Some(guess),
            (None, Some(guess), Some(now)) if !same_program(&now.name, &guess) => return false,
            (None, ..) => None,
        };
        match (known.as_mut(), name) {
            (Some(now), Some(name)) if !same_program(&now.name, &name) => {
                *known = Some(Assistant { name, version: said.version, model: said.model });
            }
            (Some(now), _) => {
                now.version = said.version.or(now.version.take());
                now.model = said.model.or(now.model.take());
            }
            (None, Some(name)) => {
                *known = Some(Assistant { name, version: said.version, model: said.model });
            }
            (None, None) => {}
        }
        *known != before
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lane {
    pub id: LaneId,
    pub agent: Author,
    /// The last checkpoint of the main lane below the lane: the one it
    /// started from, or was last brought up to, or the one its parent lane
    /// is filed under.
    pub base: CheckpointId,
    /// The parent of the lane's next op: its last op, or `base_head` when it
    /// has recorded none since it was last brought up to its target.
    pub head: OpId,
    pub workdir: PathBuf,
    pub state: LaneState,
    /// The op of the parent lane the lane's state starts from: the state of
    /// the lane is the state of its parent at this op, plus the lane's own
    /// ops from `since` on (ADR 0001).
    pub base_head: OpId,
    /// How many of the lane's ops were already absorbed by `base_head`
    /// (integrated, or re-expressed on top of it).
    #[serde(default)]
    pub since: u64,
    /// What the lane is for, as declared when it was opened ("task #55").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// The intent the next captured ops are recorded with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<IntentId>,
    /// The counter of the symbol IDs the lane creates never goes below
    /// this: IDs are not reused, even those of symbols that no longer exist.
    #[serde(default)]
    pub next_symbol: u64,
    /// What the lane said it would change, in the order it said so. A scope
    /// only grows: what was declared stays declared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<ScopeEntry>,
    /// The person the lane was opened for: whose agent works in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Author>,
    /// Set when the lane passed the merge and the checks at the request of
    /// somebody who may not integrate, and waits for somebody who may.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed: Option<Proposal>,
    /// The git commit, made outside Kito, whose changes this lane brings
    /// in (40 hexadecimal digits): its integration is a merge with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_tip: Option<String>,
    /// The lane this one started from. None: the main lane (ADR 0001).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<LaneId>,
    /// The lane this one lands on. None: its parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<LaneId>,
    /// Set for a permanent lane: its name (`staging`), unique and never
    /// `main`. It cannot be closed and has checkpoints of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The assistant its agent runs in, and the model, as last said
    /// (ADR 0010).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<Assistant>,
}

impl Lane {
    /// The lane it started from: the main lane unless it is stacked.
    pub fn parent_lane(&self) -> LaneId {
        self.parent.unwrap_or(LaneId::MAIN)
    }

    /// The lane it lands on: its parent unless said otherwise.
    pub fn target_lane(&self) -> LaneId {
        self.target.unwrap_or_else(|| self.parent_lane())
    }

    /// Whether the store must be marked as holding lanes on lanes.
    pub fn is_stacked(&self) -> bool {
        self.parent.is_some() || self.target.is_some() || self.name.is_some()
    }

    /// A lane that is never closed: `staging` next to `main`.
    pub fn is_permanent(&self) -> bool {
        self.name.is_some()
    }
}

/// A lane that waits to be integrated by somebody who may.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    /// Who asked.
    pub by: Author,
    /// When, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// How many ops the lane had then: with more, it changed since and
    /// what was checked is no longer what it holds.
    pub ops: u64,
}

/// Something a lane declares it will change, before it changes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeItem {
    /// A function, class or type: the file it is in and its name, qualified
    /// if needed (`Session.refresh`). `id` is the symbol it was when it was
    /// declared; `None` for one that did not exist yet.
    Symbol {
        file: PathBuf,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<SymbolId>,
    },
    /// One file, which may not exist yet.
    File(PathBuf),
    /// Everything under a directory, the files that do not exist yet
    /// included. The empty path is the whole repository.
    Dir(PathBuf),
}

/// One entry of a lane's scope, with the moment it was declared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeEntry {
    pub item: ScopeItem,
    /// Wall-clock time, in milliseconds since the Unix epoch.
    pub at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConflictKind {
    BodyBody,
    RemoveModify,
    RenameRename,
    SemanticRef,
    Text,
}

/// How a conflict is to be settled: with what the main lane has, with what
/// the lane has, or with a text written for the purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Resolution {
    Main,
    Lane,
    Text(BlobId),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: ConflictId,
    /// None = textual conflict on a file
    pub symbol: Option<SymbolId>,
    /// The heads of the two sides when the conflict was found: main lane, lane.
    pub sides: Vec<OpId>,
    pub kind: ConflictKind,
    pub assigned_to: Option<LaneId>,
    pub resolved_by: Option<OpId>,
    /// The file the conflict is in, as the main lane calls it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    /// The symbol's qualified name, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The contended text (of the symbol, or of the file) at the common
    /// starting point and on the two sides. `None`: it did not exist there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_text: Option<BlobId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_text: Option<BlobId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane_text: Option<BlobId>,
    /// The choice made, waiting to be applied (it is, as soon as every
    /// conflict of the lane has one); `resolved_by` is the op that applied it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
}

/// Result of the verifier: commands, outcome, duration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verification {
    pub commands: Vec<String>,
    pub passed: bool,
    pub duration_ms: u64,
}

/// A short text attached to code without changing it: a reason that
/// belongs to no change ("called by the nightly billing job: do not
/// rename"). ADR 0003.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    pub id: crate::NoteId,
    pub on: NoteTarget,
    pub text: String,
    /// Who wrote it: a person, or an agent.
    pub by: Author,
    /// Whom the agent works for, when it is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person: Option<Author>,
    /// Wall-clock time, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The lane it was written in, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<LaneId>,
    /// Who removed it, and when: it is kept, and no longer shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<(Author, u64)>,
}

/// What a note is on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoteTarget {
    /// A function or a class: the note follows it through renames and
    /// moves. `file` and `name` are where it was and what it was called
    /// when the note was written.
    Symbol { id: SymbolId, file: PathBuf, name: String },
    /// A file, or a directory and everything under it.
    Path(PathBuf),
}

/// Something said about a lane, or about code in it, by whoever reviews
/// it; or an answer to that. ADR 0004.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: crate::CommentId,
    pub lane: LaneId,
    /// What it is on; none: the lane as a whole, or what it answers is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<CommentTarget>,
    pub text: String,
    /// Who wrote it: a person, or an agent.
    pub by: Author,
    /// Whom the agent works for, when it is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person: Option<Author>,
    /// Wall-clock time, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// How many ops the lane had then: what was looked at.
    pub at_ops: u64,
    /// The comment it answers: the one that started the exchange.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<crate::CommentId>,
    /// Who resolved it, and when: it is kept, and no longer told.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<(Author, u64)>,
}

/// The code a comment is on, as the lane had it then.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentTarget {
    pub path: PathBuf,
    /// The function it is on, or the one its lines are in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<SymbolId>,
    /// The lines, from 1, in the lane's text then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<(u32, u32)>,
    /// What those lines said.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub quoted: String,
}

/// What somebody who reviewed a lane says of it, as it was then. The last
/// one a person gave on a lane stands. ADR 0004.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub id: crate::VerdictId,
    pub lane: LaneId,
    /// Who gave it: a person, or an agent.
    pub by: Author,
    /// Whom the agent works for, when it is known: the verdict is theirs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person: Option<Author>,
    pub kind: VerdictKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Wall-clock time, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// How many ops the lane had then: what was looked at.
    pub at_ops: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerdictKind {
    Approved,
    ChangesRequested,
    /// The verdict given before is taken back.
    Withdrawn,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub head: OpId,
    pub label: String,
    pub created_by: Author,
    pub verification: Option<Verification>,
    /// after the export
    #[serde(with = "crate::serde_helpers::opt_oid20")]
    pub git_commit: Option<[u8; 20]>,
    /// The person who let an integration in, when a server knew who asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_by: Option<Author>,
    /// A git commit made outside Kito that this checkpoint brings in: the
    /// commit exported for it is a merge, with that commit as its second
    /// parent.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::serde_helpers::opt_oid20")]
    pub git_merge: Option<[u8; 20]>,
    /// The permanent lane this is a checkpoint of. None: the main lane
    /// (ADR 0001).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<LaneId>,
    /// The people whose approval of the lane counted when it was
    /// integrated (ADR 0004).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewed_by: Vec<Author>,
}

impl Checkpoint {
    /// The lane it is a checkpoint of.
    pub fn lane_of(&self) -> LaneId {
        self.lane.unwrap_or(LaneId::MAIN)
    }
}
