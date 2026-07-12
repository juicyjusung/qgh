use std::collections::{BTreeMap, BTreeSet};

const CANDIDATE_OVERFETCH_FACTOR: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FusionPolicy {
    id: &'static str,
    rrf_k: f32,
    lexical_weight: f32,
    dense_weight: f32,
    dense_window: usize,
    protected_lexical_head: usize,
}

impl FusionPolicy {
    pub const fn new(
        id: &'static str,
        rrf_k: f32,
        lexical_weight: f32,
        dense_weight: f32,
        dense_window: usize,
        protected_lexical_head: usize,
    ) -> Self {
        Self {
            id,
            rrf_k,
            lexical_weight,
            dense_weight,
            dense_window,
            protected_lexical_head,
        }
    }

    pub const fn id(self) -> &'static str {
        self.id
    }

    pub const fn rrf_k(self) -> f32 {
        self.rrf_k
    }

    pub const fn lexical_weight(self) -> f32 {
        self.lexical_weight
    }

    pub const fn dense_weight(self) -> f32 {
        self.dense_weight
    }

    pub const fn dense_window(self) -> usize {
        self.dense_window
    }

    pub const fn protected_lexical_head(self) -> usize {
        self.protected_lexical_head
    }

    pub fn candidate_window(self, result_limit: usize) -> usize {
        result_limit
            .saturating_mul(CANDIDATE_OVERFETCH_FACTOR)
            .max(self.dense_window)
            .max(result_limit)
    }

    pub const fn dense_candidate_window(self) -> usize {
        self.dense_window
    }
}

/// Fixed hybrid policy selected for production after the public multilingual
/// qrels comparison. It treats dense retrieval as a BM25 complement: the
/// lexical top five stay stable and Qwen can add candidates below that head.
pub const LEXICAL_GUARD_V1: FusionPolicy =
    FusionPolicy::new("lexical_guard_v1", 60.0, 2.0, 1.0, 80, 5);

#[derive(Debug, Clone, PartialEq)]
pub struct FusedCandidate<K> {
    pub key: K,
    pub lexical_rank: Option<usize>,
    pub dense_rank: Option<usize>,
    pub rrf_score: f32,
    pub final_order_score: f32,
}

#[derive(Debug, Clone, Copy, Default)]
struct BranchRanks {
    lexical: Option<usize>,
    dense: Option<usize>,
}

impl BranchRanks {
    fn best(self) -> usize {
        self.lexical
            .into_iter()
            .chain(self.dense)
            .min()
            .unwrap_or(usize::MAX)
    }

    fn score(self, policy: FusionPolicy) -> f32 {
        rrf_component(self.lexical, policy.rrf_k, policy.lexical_weight)
            + rrf_component(self.dense, policy.rrf_k, policy.dense_weight)
    }
}

pub fn rrf_component(rank: Option<usize>, k: f32, weight: f32) -> f32 {
    rank.map(|rank| weight / (k + rank as f32)).unwrap_or(0.0)
}

/// Fuses already-filtered, source-level branch rankings without inspecting
/// content. Duplicate keys use their first (best) rank in each branch.
pub fn fuse_ranked<K>(
    lexical: &[K],
    dense: &[K],
    limit: usize,
    policy: FusionPolicy,
) -> Vec<FusedCandidate<K>>
where
    K: Clone + Ord,
{
    if limit == 0 {
        return Vec::new();
    }

    let mut ranks = BTreeMap::<K, BranchRanks>::new();
    for (index, key) in lexical.iter().enumerate() {
        ranks
            .entry(key.clone())
            .or_default()
            .lexical
            .get_or_insert(index + 1);
    }
    for (index, key) in dense.iter().take(policy.dense_window).enumerate() {
        ranks
            .entry(key.clone())
            .or_default()
            .dense
            .get_or_insert(index + 1);
    }

    let mut ranked = ranks
        .into_iter()
        .map(|(key, ranks)| FusedCandidate {
            key,
            lexical_rank: ranks.lexical,
            dense_rank: ranks.dense,
            rrf_score: ranks.score(policy),
            final_order_score: 0.0,
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .rrf_score
            .total_cmp(&left.rrf_score)
            .then_with(|| {
                BranchRanks {
                    lexical: left.lexical_rank,
                    dense: left.dense_rank,
                }
                .best()
                .cmp(
                    &BranchRanks {
                        lexical: right.lexical_rank,
                        dense: right.dense_rank,
                    }
                    .best(),
                )
            })
            .then_with(|| left.key.cmp(&right.key))
    });

    let mut protected_keys = BTreeSet::new();
    let mut protected = Vec::new();
    for key in lexical.iter().take(policy.protected_lexical_head) {
        if !protected_keys.insert(key.clone()) {
            continue;
        }
        if let Some(index) = ranked.iter().position(|candidate| candidate.key == *key) {
            protected.push(ranked.remove(index));
        }
    }
    protected.append(&mut ranked);
    protected.truncate(limit);
    for (index, candidate) in protected.iter_mut().enumerate() {
        candidate.final_order_score = 1.0 / (index + 1) as f32;
    }
    protected
}

#[cfg(test)]
mod tests {
    use super::{fuse_ranked, LEXICAL_GUARD_V1};

    #[test]
    fn lexical_guard_preserves_the_bm25_head_and_allows_semantic_tail_rescue() {
        let lexical = ["l1", "l2", "l3", "l4", "l5", "l6"];
        let dense = ["d1", "d2", "d3", "d4", "d5", "l6"];

        let fused = fuse_ranked(&lexical, &dense, 8, LEXICAL_GUARD_V1);
        let keys = fused
            .iter()
            .map(|candidate| candidate.key)
            .collect::<Vec<_>>();

        assert_eq!(&keys[..5], &lexical[..5]);
        assert!(keys[5..].iter().any(|key| key.starts_with('d')));
        assert!(fused
            .windows(2)
            .all(|pair| pair[0].final_order_score > pair[1].final_order_score));
    }

    #[test]
    fn lexical_guard_ignores_dense_candidates_beyond_its_fixed_window() {
        let lexical = vec!["l1".to_string()];
        let dense = (1..=81).map(|rank| format!("d{rank}")).collect::<Vec<_>>();

        let fused = fuse_ranked(&lexical, &dense, 100, LEXICAL_GUARD_V1);

        assert!(fused.iter().any(|candidate| candidate.key == "d80"));
        assert!(!fused.iter().any(|candidate| candidate.key == "d81"));
    }

    #[test]
    fn lexical_guard_owns_the_default_and_expanded_candidate_windows() {
        assert_eq!(LEXICAL_GUARD_V1.candidate_window(10), 80);
        assert_eq!(LEXICAL_GUARD_V1.candidate_window(20), 80);
        assert_eq!(LEXICAL_GUARD_V1.candidate_window(100), 400);
        assert_eq!(LEXICAL_GUARD_V1.dense_candidate_window(), 80);
    }

    #[test]
    fn lexical_guard_keeps_weighted_rrf_separate_from_final_policy_order() {
        let lexical = ["lexical-head", "lexical-second"];
        let dense = ["dense-first", "lexical-second"];

        let fused = fuse_ranked(&lexical, &dense, 3, LEXICAL_GUARD_V1);

        assert_eq!(fused[0].key, "lexical-head");
        assert!(fused[0].rrf_score < fused[1].rrf_score);
        assert!(fused[0].final_order_score > fused[1].final_order_score);
    }
}
