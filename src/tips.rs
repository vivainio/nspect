//! Structural suggestions — soft signals for consolidation, merging, and
//! cleanup. Non-authoritative. Written to `.nspect/gen/tips.yaml` alongside
//! the other artifacts; consumed by humans, not CI.
//!
//! Today this surfaces merge candidates. Additional buckets (consolidation,
//! split candidates, orphan area roots) can land in the same file without
//! reshaping the API.

use serde::Serialize;

use crate::atlas::{Atlas, AtlasProject};

#[derive(Debug, Default, Serialize)]
pub struct TipsReport {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub merge_candidates: Vec<MergeCandidate>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cluster_candidates: Vec<ClusterCandidate>,
}

#[derive(Debug, Serialize)]
pub struct ClusterCandidate {
    /// Projects that share the same set of consumers and look like a single
    /// logical unit fragmented across multiple csprojs.
    pub projects: Vec<String>,
    /// Common consumer set — the ids that depend on every member.
    pub consumed_by: Vec<String>,
    pub total_loc: u32,
    pub confidence: Confidence,
}

#[derive(Debug, Serialize)]
pub struct MergeCandidate {
    /// Project that looks mergeable.
    pub project: String,
    /// The sole consumer it would likely merge into.
    pub into: String,
    pub reason: &'static str,
    pub confidence: Confidence,
    /// Snapshot of the weight that triggered the candidate, for sanity at
    /// read time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loc: Option<u32>,
}

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl TipsReport {
    pub fn is_empty(&self) -> bool {
        self.merge_candidates.is_empty() && self.cluster_candidates.is_empty()
    }
}

/// Build tips from an atlas snapshot. Requires `atlas` to have `weight`
/// populated (i.e. the tree-sitter source scan ran).
pub fn build(atlas: &Atlas) -> TipsReport {
    let mut out = TipsReport::default();

    // Forward adjacency: who-depends-on-whom, keyed by project id.
    // `project_refs` on each project already lists the ids it depends on.
    // Build the reverse map to find each project's unique consumer (when any).
    let mut consumers: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    for p in &atlas.projects {
        for r in &p.project_refs {
            consumers
                .entry(r.as_str())
                .or_default()
                .push(p.id.as_str());
        }
    }

    for p in &atlas.projects {
        if let Some(mc) = merge_candidate(p, &consumers) {
            out.merge_candidates.push(mc);
        }
    }
    out.merge_candidates
        .sort_by(|a, b| a.project.cmp(&b.project));

    out.cluster_candidates = cluster_candidates(atlas, &consumers);
    out
}

/// Group projects by their (sorted) set of consumers. Any group of size ≥ 2
/// where members share the *exact same* consumer set is a candidate for
/// being a single project that was over-decomposed.
///
/// Filters: skip in-cycle / test / installer members. The shared consumer
/// set must be non-empty (orphans are a different concern). Size is not a
/// filter — bigger clusters yield bigger build-graph wins on consolidation.
fn cluster_candidates(
    atlas: &Atlas,
    consumers: &std::collections::HashMap<&str, Vec<&str>>,
) -> Vec<ClusterCandidate> {
    let mut by_consumers: std::collections::BTreeMap<Vec<String>, Vec<&AtlasProject>> =
        std::collections::BTreeMap::new();
    for p in &atlas.projects {
        if p.in_cycle || looks_like_test_or_entrypoint(&p.name) {
            continue;
        }
        let Some(set) = consumers.get(p.id.as_str()) else {
            continue;
        };
        if set.is_empty() {
            continue;
        }
        let mut key: Vec<String> = set.iter().map(|s| s.to_string()).collect();
        key.sort();
        key.dedup();
        by_consumers.entry(key).or_default().push(p);
    }

    let mut out: Vec<ClusterCandidate> = Vec::new();
    for (consumer_set, members) in by_consumers {
        if members.len() < 2 {
            continue;
        }
        let total_loc: u32 = members
            .iter()
            .map(|m| m.weight.as_ref().map(|w| w.loc).unwrap_or(0))
            .sum();
        let mut projects: Vec<String> = members.iter().map(|m| m.id.clone()).collect();
        projects.sort();
        // High confidence: ≥3 members. Two-member clusters are common
        // enough to be coincidental in a large monolith and stay medium.
        let confidence = if members.len() >= 3 {
            Confidence::High
        } else {
            Confidence::Medium
        };
        out.push(ClusterCandidate {
            projects,
            consumed_by: consumer_set,
            total_loc,
            confidence,
        });
    }
    // Largest cluster first (more dlls to fold = bigger win), then by
    // ascending total LOC (smaller clusters easier to act on at the same
    // size), then by first project name for stability.
    out.sort_by(|a, b| {
        b.projects
            .len()
            .cmp(&a.projects.len())
            .then(a.total_loc.cmp(&b.total_loc))
            .then(a.projects.first().cmp(&b.projects.first()))
    });
    out
}

/// Flag a project as a single-consumer library: exactly one incoming
/// project-ref, not in a cycle, not a test / installer. Size is *not* a
/// filter — the goal is reducing csproj count to shrink the build graph,
/// and bigger absorptions yield bigger wins.
fn merge_candidate(
    p: &AtlasProject,
    consumers: &std::collections::HashMap<&str, Vec<&str>>,
) -> Option<MergeCandidate> {
    if p.in_cycle || p.fan_in != 1 {
        return None;
    }
    if looks_like_test_or_entrypoint(&p.name) {
        return None;
    }
    let into = consumers
        .get(p.id.as_str())
        .and_then(|v| v.first().copied())?
        .to_string();
    // The "only consumer is my own test project" pattern looks like a
    // candidate but isn't — it means the lib has no prod consumer, not that
    // it should be absorbed into the tests.
    if looks_like_test_or_entrypoint(&into) {
        return None;
    }
    let loc = p.weight.as_ref().map(|w| w.loc);
    Some(MergeCandidate {
        project: p.id.clone(),
        into,
        reason: "single consumer",
        confidence: Confidence::High,
        loc,
    })
}

fn looks_like_test_or_entrypoint(name: &str) -> bool {
    name.split('.').any(|seg| {
        let lower = seg.to_ascii_lowercase();
        lower.ends_with("test")
            || lower.ends_with("tests")
            || lower.ends_with("testing")
            || lower.ends_with("installer")
    })
}
