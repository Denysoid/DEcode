use std::ops::Range;

use similar::{DiffTag, TextDiff};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchHunk {
    pub index: usize,
    pub old_lines: Range<usize>,
    pub new_lines: Range<usize>,
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchReview {
    pub path: String,
    search: String,
    replace: String,
    pub hunks: Vec<PatchHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchSelection {
    pub replacement: String,
    pub approved_hunks: usize,
    pub total_hunks: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PatchReviewError {
    #[error("patch review received {actual} hunk decisions, expected {expected}")]
    DecisionCount { expected: usize, actual: usize },
    #[error("patch diff operation referenced an invalid line range")]
    InvalidDiffRange,
}

impl PatchReview {
    #[must_use]
    pub fn new(
        path: impl Into<String>,
        search: impl Into<String>,
        replace: impl Into<String>,
    ) -> Self {
        let path = path.into();
        let search = search.into();
        let replace = replace.into();
        let old_lines = split_lines(&search);
        let new_lines = split_lines(&replace);
        let diff = TextDiff::from_lines(&search, &replace);
        let hunks = diff
            .ops()
            .iter()
            .filter(|operation| operation.tag() != DiffTag::Equal)
            .enumerate()
            .map(|(index, operation)| PatchHunk {
                index,
                old_lines: operation.old_range(),
                new_lines: operation.new_range(),
                old: join_range(&old_lines, operation.old_range()).unwrap_or_default(),
                new: join_range(&new_lines, operation.new_range()).unwrap_or_default(),
            })
            .collect();
        Self {
            path,
            search,
            replace,
            hunks,
        }
    }

    #[must_use]
    pub fn search(&self) -> &str {
        &self.search
    }

    #[must_use]
    pub fn replace(&self) -> &str {
        &self.replace
    }

    pub fn apply_decisions(&self, decisions: &[bool]) -> Result<PatchSelection, PatchReviewError> {
        if decisions.len() != self.hunks.len() {
            return Err(PatchReviewError::DecisionCount {
                expected: self.hunks.len(),
                actual: decisions.len(),
            });
        }

        let old_lines = split_lines(&self.search);
        let new_lines = split_lines(&self.replace);
        let diff = TextDiff::from_lines(&self.search, &self.replace);
        let mut replacement = String::with_capacity(self.replace.len().max(self.search.len()));
        let mut decision_index = 0_usize;
        for operation in diff.ops() {
            if operation.tag() == DiffTag::Equal {
                replacement.push_str(
                    &join_range(&new_lines, operation.new_range())
                        .ok_or(PatchReviewError::InvalidDiffRange)?,
                );
                continue;
            }

            let approved =
                decisions
                    .get(decision_index)
                    .copied()
                    .ok_or(PatchReviewError::DecisionCount {
                        expected: self.hunks.len(),
                        actual: decisions.len(),
                    })?;
            let fragment = if approved {
                join_range(&new_lines, operation.new_range())
            } else {
                join_range(&old_lines, operation.old_range())
            }
            .ok_or(PatchReviewError::InvalidDiffRange)?;
            replacement.push_str(&fragment);
            decision_index = decision_index.saturating_add(1);
        }

        Ok(PatchSelection {
            replacement,
            approved_hunks: decisions.iter().filter(|approved| **approved).count(),
            total_hunks: self.hunks.len(),
        })
    }
}

fn split_lines(value: &str) -> Vec<&str> {
    value.split_inclusive('\n').collect()
}

fn join_range(lines: &[&str], range: Range<usize>) -> Option<String> {
    lines.get(range).map(|slice| slice.concat())
}

#[cfg(test)]
mod tests {
    use super::{PatchReview, PatchReviewError};

    #[test]
    fn accepts_and_rejects_independent_hunks() -> Result<(), PatchReviewError> {
        let review = PatchReview::new(
            "src/lib.rs",
            "one\nkeep-a\ntwo\nkeep-b\nthree\n",
            "ONE\nkeep-a\ntwo\nkeep-b\nTHREE\n",
        );
        assert_eq!(review.hunks.len(), 2);
        let selection = review.apply_decisions(&[true, false])?;
        assert_eq!(selection.replacement, "ONE\nkeep-a\ntwo\nkeep-b\nthree\n");
        assert_eq!(selection.approved_hunks, 1);
        assert_eq!(selection.total_hunks, 2);
        Ok(())
    }

    #[test]
    fn preserves_crlf_and_unicode_byte_for_byte() -> Result<(), PatchReviewError> {
        let review = PatchReview::new(
            "notes.txt",
            "alpha\r\nРядок\r\nomega\r\n",
            "ALPHA\r\nРядок\r\nOMEGA\r\n",
        );
        let selection = review.apply_decisions(&[false, true])?;
        assert_eq!(selection.replacement, "alpha\r\nРядок\r\nOMEGA\r\n");
        Ok(())
    }

    #[test]
    fn wrong_decision_count_fails_closed() {
        let review = PatchReview::new("x.rs", "old\n", "new\n");
        assert_eq!(
            review.apply_decisions(&[]),
            Err(PatchReviewError::DecisionCount {
                expected: 1,
                actual: 0,
            })
        );
    }

    #[test]
    fn rejecting_every_hunk_reconstructs_original_search() -> Result<(), PatchReviewError> {
        let review = PatchReview::new("x.rs", "a\nb\nc", "A\nb\nC");
        let decisions = vec![false; review.hunks.len()];
        let selection = review.apply_decisions(&decisions)?;
        assert_eq!(selection.replacement, review.search());
        assert_eq!(selection.approved_hunks, 0);
        Ok(())
    }

    #[test]
    fn empty_inputs_have_no_hunks_and_reconstruct_empty() -> Result<(), PatchReviewError> {
        let review = PatchReview::new("x.rs", "", "");
        assert!(review.hunks.is_empty());
        assert_eq!(review.apply_decisions(&[])?.replacement, "");
        Ok(())
    }
}
