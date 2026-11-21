//! Verdicts on a lane (ADR 0004): approved, or changes requested, by
//! whoever reviewed it, and what integration asks of them.
//!
//! A verdict is about the lane as it was: an approval given before the
//! lane changed is stale, and does not count unless the repository says
//! approvals are kept. The owner of a lane may ask it to change, and that
//! stands; their approval does not count: a review is somebody else's. Approving writes nothing to the target: bringing
//! the lane in stays `integrate`, for whoever may.

use crate::error::{Error, Result};
use crate::Repo;
use kito_core::{Author, AuthorKind, LaneId, Verdict, VerdictKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A verdict as the clients see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictInfo {
    pub by: String,
    #[serde(rename = "for", default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
    /// `approved` or `changes_requested`.
    pub verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub at_ms: u64,
    /// The lane changed since: an approval of what it was.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// Whether it counts for the integration of the lane.
    pub counts: bool,
    /// Why it does not count, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub because: Option<String>,
}

/// Where the review of a lane stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewInfo {
    pub lane: String,
    /// Approvals the lane needs before it is integrated.
    pub required: u32,
    /// The people whose approval counts.
    pub approved_by: Vec<String>,
    /// The people whose request for changes stands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes_requested_by: Vec<String>,
    /// What the lane lacks to be integrated, in words. None: nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
    /// The verdict each person gave last, oldest first.
    pub verdicts: Vec<VerdictInfo>,
    /// Comments that wait on the lane.
    pub comments: usize,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Whose word a verdict is: the person an agent works for, or who gave it.
fn reviewer(verdict: &Verdict) -> &Author {
    verdict.person.as_ref().unwrap_or(&verdict.by)
}

fn kind_word(kind: VerdictKind) -> &'static str {
    match kind {
        VerdictKind::Approved => "approved",
        VerdictKind::ChangesRequested => "changes_requested",
        VerdictKind::Withdrawn => "withdrawn",
    }
}

impl Repo {
    /// Approvals a lane needs before it is integrated, when its target
    /// does not say otherwise.
    pub fn required_approvals(&self) -> u32 {
        self.config.review.required
    }

    /// Sets it, in the configuration.
    pub fn set_required_approvals(&mut self, required: u32) -> Result<()> {
        let store = self.root.join(kito_store::STORE_DIR);
        crate::config::write_value(&store, "review", "required", Some(i64::from(required).into()))
            .map_err(Error::Config)?;
        self.config.review.required = required;
        Ok(())
    }

    /// Approvals a lane needs: what its target says (`[lanes.<name>]`
    /// `required`), else `[review] required`.
    fn approvals_required(&self, lane: LaneId) -> u32 {
        let target = self.store.lane_record(lane).map(|lane| lane.target_lane());
        let named =
            target.and_then(|target| self.store.lane_record(target)).and_then(|lane| lane.name.as_ref());
        named
            .and_then(|name| self.config.lanes.permanent.get(name))
            .and_then(|permanent| permanent.required)
            .unwrap_or(self.config.review.required)
    }

    /// The verdict each person gave last on a lane, oldest first, each
    /// with whether it counts and why not.
    fn standing(&self, lane: LaneId) -> Vec<(&Verdict, bool, Option<&'static str>)> {
        let mut last: BTreeMap<&str, &Verdict> = BTreeMap::new();
        for verdict in self.store.verdicts().filter(|verdict| verdict.lane == lane) {
            last.insert(reviewer(verdict).name.as_str(), verdict);
        }
        let mut standing: Vec<&Verdict> =
            last.into_values().filter(|verdict| verdict.kind != VerdictKind::Withdrawn).collect();
        standing.sort_by_key(|verdict| verdict.id);
        let record = self.store.lane_record(lane);
        let owner = record.and_then(|lane| lane.owner.as_ref()).map(|owner| owner.name.as_str());
        let ops = self.lane_ops(lane).ok();
        standing
            .into_iter()
            .map(|verdict| {
                let stale =
                    verdict.kind == VerdictKind::Approved && ops.is_some_and(|ops| ops != verdict.at_ops);
                // The owner may say that their lane must change, and that
                // stands; their approval is not the review of somebody else.
                let own = Some(reviewer(verdict).name.as_str()) == owner;
                let because = if own && verdict.kind == VerdictKind::Approved {
                    Some("the lane is theirs")
                } else if verdict.by.kind == AuthorKind::Agent && !self.config.review.agents {
                    Some("given by an agent")
                } else if stale && !self.config.review.keep {
                    Some("the lane changed since")
                } else {
                    None
                };
                (verdict, because.is_none(), because)
            })
            .collect()
    }

    /// Where the review of a lane stands.
    pub fn review(&self, lane: LaneId) -> Result<ReviewInfo> {
        if lane.is_main() {
            return Err(Error::Other(
                "the main lane is not reviewed: lanes are, before they land on it".into(),
            ));
        }
        if self.store.lane_record(lane).is_none() {
            return Err(Error::NoSuchLane(lane));
        }
        let standing = self.standing(lane);
        let ops = self.lane_ops(lane).ok();
        let names = |kind: VerdictKind| -> Vec<String> {
            standing
                .iter()
                .filter(|(verdict, counts, _)| *counts && verdict.kind == kind)
                .map(|(verdict, ..)| reviewer(verdict).name.clone())
                .collect()
        };
        let (approved_by, changes_requested_by) =
            (names(VerdictKind::Approved), names(VerdictKind::ChangesRequested));
        let required = self.approvals_required(lane);
        let mut missing = Vec::new();
        if (approved_by.len() as u32) < required {
            let s = if required == 1 { "" } else { "s" };
            missing.push(format!("{} of {required} approval{s}", approved_by.len()));
        }
        if !changes_requested_by.is_empty() {
            missing.push(format!("changes requested by {}", changes_requested_by.join(", ")));
        }
        let verdicts = standing
            .iter()
            .map(|(verdict, counts, because)| VerdictInfo {
                by: verdict.by.name.clone(),
                person: verdict.person.as_ref().map(|person| person.name.clone()),
                verdict: kind_word(verdict.kind).to_owned(),
                text: verdict.text.clone(),
                at_ms: verdict.at_ms,
                stale: verdict.kind == VerdictKind::Approved && ops.is_some_and(|ops| ops != verdict.at_ops),
                counts: *counts,
                because: because.map(str::to_owned),
            })
            .collect();
        Ok(ReviewInfo {
            lane: lane.to_string(),
            required,
            approved_by,
            changes_requested_by,
            missing: (!missing.is_empty()).then(|| missing.join("; ")),
            verdicts,
            comments: self
                .comments(lane, false)
                .map_or(0, |comments| comments.iter().filter(|comment| comment.reply_to.is_none()).count()),
        })
    }

    /// What a lane lacks of its review to be integrated, and whose
    /// approvals count for it.
    pub(crate) fn review_gate(&self, lane: LaneId) -> (Option<String>, Vec<Author>) {
        let Ok(review) = self.review(lane) else { return (None, Vec::new()) };
        let approvers = self
            .standing(lane)
            .into_iter()
            .filter(|(verdict, counts, _)| *counts && verdict.kind == VerdictKind::Approved)
            .map(|(verdict, ..)| reviewer(verdict).clone())
            .collect();
        (review.missing, approvers)
    }

    /// Gives a verdict on a lane as it is now, in the place of the one
    /// the same person gave before. `by`: who gives it (default: whoever
    /// runs the repository); `person`: whom an agent works for.
    pub fn give_verdict(
        &mut self,
        lane: LaneId,
        kind: VerdictKind,
        text: Option<&str>,
        by: Option<Author>,
        person: Option<Author>,
    ) -> Result<ReviewInfo> {
        if lane.is_main() || !self.lanes.contains_key(&lane) {
            return Err(Error::Other(format!("{lane} is not an open lane: there is nothing to review")));
        }
        let text = text.map(str::trim).filter(|text| !text.is_empty());
        if kind == VerdictKind::ChangesRequested && text.is_none() {
            return Err(Error::Other(
                "say what should change: a request for changes needs its words (comments say where)".into(),
            ));
        }
        let by = by.unwrap_or_else(|| self.identity.author());
        let whose = person.as_ref().unwrap_or(&by).name.clone();
        if kind != VerdictKind::Withdrawn {
            if by.kind == AuthorKind::Agent && !self.config.review.agents {
                return Err(Error::Refused(
                    "a verdict is a person's word here: agents comment, and people approve (`[review] agents \
                     = true` lets an agent's verdict count as its person's)"
                        .into(),
                ));
            }
        } else if !self.standing(lane).iter().any(|(verdict, ..)| reviewer(verdict).name == whose) {
            return Err(Error::Other(format!("{whose} gave no verdict on {lane}")));
        }
        // The verdict is on the lane as its agent just left it.
        self.scan_lane(lane)?;
        let verdict = Verdict {
            id: self.store.next_verdict_id(),
            lane,
            by,
            person,
            kind,
            text: text.map(str::to_owned),
            at_ms: now_ms(),
            at_ops: self.lane_ops(lane)?,
        };
        self.store.put_verdict(&verdict)?;
        self.review(lane)
    }
}
