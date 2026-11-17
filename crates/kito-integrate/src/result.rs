//! What an agent gets back from an integration.

use serde::{Deserialize, Serialize};

/// How an integration ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The lane's work is on the main lane, under a new checkpoint.
    Integrated,
    /// The lane had nothing the main lane does not already have.
    NothingToIntegrate,
    /// Some changes cannot be composed: each conflict waits for a decision.
    Conflicts,
    /// The merge composed, but the verification commands failed on it.
    VerificationFailed,
    /// The lane changed things outside the scope it declared: nothing was
    /// merged (see `out_of_scope`).
    OutOfScope,
    /// The merge composed and the checks passed, but who asked may not
    /// integrate: the lane waits for somebody who may.
    AwaitingApproval,
    /// The integration could not be carried out (see `message`).
    Failed,
}

/// Something the integration settled by itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoResolution {
    /// `body_merge`: both sides changed the symbol and the texts merged.
    pub kind: String,
    /// `ts/auth.getUser#42`: language, file, name and ID of the symbol.
    pub symbol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
    /// `src/billing.ts:41`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<String>,
}

/// A conflict as the clients see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictInfo {
    /// `conflict#7`.
    pub id: String,
    /// `body_body`, `remove_modify`, `rename_rename` or `text`.
    pub kind: String,
    /// The lane that has to decide.
    pub lane: String,
    pub file: Option<String>,
    /// The symbol's name; `None` for a conflict on the text of a file.
    pub symbol: Option<String>,
    pub symbol_id: Option<String>,
    /// The contended text at the common starting point and on the two
    /// sides. `None`: it does not exist there.
    pub base: Option<String>,
    pub main: Option<String>,
    pub lane_text: Option<String>,
    /// The decision taken, if any: `main`, `lane` or `text`. It is applied
    /// by the next integration.
    pub resolution: Option<String>,
    /// `true` once an integration has applied the decision.
    pub resolved: bool,
}

/// What the verifier did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReport {
    /// The commands that ran, in order.
    pub commands: Vec<String>,
    pub passed: bool,
    pub duration_ms: u64,
    /// The command that failed, with the end of what it printed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The commands ran confined: they could write only in their own copy
    /// of the files.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confined: bool,
    /// The lanes whose merged results were checked together in this run;
    /// empty when the lane was checked by itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub batch: Vec<String>,
}

/// The structured result of an integration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationResult {
    pub lane: String,
    pub result: Outcome,
    /// The checkpoint created on the main lane.
    pub checkpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    pub auto_resolutions: Vec<AutoResolution>,
    pub conflicts: Vec<ConflictInfo>,
    pub verification: Option<VerificationReport>,
    /// The checkpoint the lane continues from.
    pub your_next_base: Option<String>,
    /// What went wrong, or what there was not to do, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// What the lane changed outside the scope it declared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_scope: Vec<String>,
}

impl IntegrationResult {
    /// A result with nothing in it but how it ended.
    pub fn new(lane: impl Into<String>, result: Outcome) -> IntegrationResult {
        IntegrationResult {
            lane: lane.into(),
            result,
            checkpoint: None,
            git_commit: None,
            auto_resolutions: Vec::new(),
            conflicts: Vec::new(),
            verification: None,
            your_next_base: None,
            message: None,
            out_of_scope: Vec::new(),
        }
    }

    pub fn failed(lane: impl Into<String>, message: impl Into<String>) -> IntegrationResult {
        IntegrationResult { message: Some(message.into()), ..IntegrationResult::new(lane, Outcome::Failed) }
    }
}
