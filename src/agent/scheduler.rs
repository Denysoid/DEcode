use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

const MAX_DEPENDENCIES: usize = 32;
const MAX_FILE_CLAIMS: usize = 64;
const MAX_FILE_CLAIM_BYTES: usize = 512;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    #[error("an agent cannot depend on itself ({0})")]
    SelfDependency(u64),
    #[error("unknown dependency agent {0}")]
    UnknownDependency(u64),
    #[error("dependency graph contains a cycle through agent {0}")]
    Cycle(u64),
    #[error("task declares {actual} dependencies; maximum is {limit}")]
    TooManyDependencies { actual: usize, limit: usize },
    #[error("task declares {actual} file claims; maximum is {limit}")]
    TooManyFileClaims { actual: usize, limit: usize },
    #[error("file claim is empty")]
    EmptyFileClaim,
    #[error("file claim {0:?} is absolute or contains traversal")]
    UnsafeFileClaim(String),
    #[error("file claim is {actual_bytes} bytes; maximum is {limit_bytes}")]
    FileClaimTooLarge {
        actual_bytes: usize,
        limit_bytes: usize,
    },
    #[error("read-only agents cannot reserve writable file claims")]
    ReadOnlyFileClaims,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyState {
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyDecision {
    Ready,
    Waiting(Vec<u64>),
    Failed(Vec<u64>),
}

pub fn normalize_dependencies(
    new_id: u64,
    dependencies: &[u64],
    graph: &BTreeMap<u64, Vec<u64>>,
) -> Result<Vec<u64>, ScheduleError> {
    if dependencies.len() > MAX_DEPENDENCIES {
        return Err(ScheduleError::TooManyDependencies {
            actual: dependencies.len(),
            limit: MAX_DEPENDENCIES,
        });
    }
    let dependencies = dependencies.iter().copied().collect::<BTreeSet<_>>();
    if dependencies.contains(&new_id) {
        return Err(ScheduleError::SelfDependency(new_id));
    }
    if let Some(unknown) = dependencies
        .iter()
        .find(|dependency| !graph.contains_key(dependency))
    {
        return Err(ScheduleError::UnknownDependency(*unknown));
    }

    let dependencies = dependencies.into_iter().collect::<Vec<_>>();
    let mut prospective = graph.clone();
    prospective.insert(new_id, dependencies.clone());
    validate_dependency_graph(&prospective)?;
    Ok(dependencies)
}

pub fn validate_dependency_graph(graph: &BTreeMap<u64, Vec<u64>>) -> Result<(), ScheduleError> {
    for (id, dependencies) in graph {
        if dependencies.len() > MAX_DEPENDENCIES {
            return Err(ScheduleError::TooManyDependencies {
                actual: dependencies.len(),
                limit: MAX_DEPENDENCIES,
            });
        }
        if dependencies.iter().any(|dependency| dependency == id) {
            return Err(ScheduleError::SelfDependency(*id));
        }
        if let Some(unknown) = dependencies
            .iter()
            .find(|dependency| !graph.contains_key(dependency))
        {
            return Err(ScheduleError::UnknownDependency(*unknown));
        }
    }
    ensure_acyclic(graph)
}

pub fn dependency_decision(
    dependencies: &[u64],
    states: &BTreeMap<u64, DependencyState>,
) -> Result<DependencyDecision, ScheduleError> {
    let mut waiting = Vec::new();
    let mut failed = Vec::new();
    for dependency in dependencies {
        match states
            .get(dependency)
            .copied()
            .ok_or(ScheduleError::UnknownDependency(*dependency))?
        {
            DependencyState::Succeeded => {}
            DependencyState::Pending => waiting.push(*dependency),
            DependencyState::Failed => failed.push(*dependency),
        }
    }
    if !failed.is_empty() {
        Ok(DependencyDecision::Failed(failed))
    } else if !waiting.is_empty() {
        Ok(DependencyDecision::Waiting(waiting))
    } else {
        Ok(DependencyDecision::Ready)
    }
}

pub fn normalize_file_claims(claims: &[String]) -> Result<Vec<String>, ScheduleError> {
    if claims.len() > MAX_FILE_CLAIMS {
        return Err(ScheduleError::TooManyFileClaims {
            actual: claims.len(),
            limit: MAX_FILE_CLAIMS,
        });
    }
    let mut normalized = BTreeSet::new();
    for claim in claims {
        if claim.len() > MAX_FILE_CLAIM_BYTES {
            return Err(ScheduleError::FileClaimTooLarge {
                actual_bytes: claim.len(),
                limit_bytes: MAX_FILE_CLAIM_BYTES,
            });
        }
        if claim.contains('\0') {
            return Err(ScheduleError::UnsafeFileClaim(claim.clone()));
        }
        let slash = claim.replace('\\', "/");
        if slash.trim().is_empty() {
            return Err(ScheduleError::EmptyFileClaim);
        }
        if slash.starts_with('/')
            || slash.starts_with("//")
            || slash
                .split('/')
                .next()
                .is_some_and(|segment| segment.contains(':'))
        {
            return Err(ScheduleError::UnsafeFileClaim(claim.clone()));
        }
        let mut components = Vec::new();
        for component in slash.split('/') {
            match component {
                "" | "." => {}
                ".." => return Err(ScheduleError::UnsafeFileClaim(claim.clone())),
                value => components.push(value),
            }
        }
        if components.is_empty() {
            return Err(ScheduleError::EmptyFileClaim);
        }
        normalized.insert(components.join("/"));
    }
    Ok(normalized.into_iter().collect())
}

#[must_use]
pub fn writer_claims_conflict(left: &[String], right: &[String]) -> bool {
    if left.is_empty() || right.is_empty() {
        return true;
    }
    left.iter()
        .any(|left| right.iter().any(|right| path_claims_overlap(left, right)))
}

#[must_use]
pub fn file_claims_cover_path(claims: &[String], path: &str) -> bool {
    if claims.is_empty() {
        return true;
    }
    let Ok(normalized) = normalize_file_claims(&[path.to_owned()]) else {
        return false;
    };
    let Some(path) = normalized.first() else {
        return false;
    };
    let path = path.to_lowercase();
    claims.iter().any(|claim| {
        let claim = claim.to_lowercase();
        path == claim
            || path
                .strip_prefix(&claim)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

fn path_claims_overlap(left: &str, right: &str) -> bool {
    let left = left.to_lowercase();
    let right = right.to_lowercase();
    left == right
        || left
            .strip_prefix(&right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(&left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn ensure_acyclic(graph: &BTreeMap<u64, Vec<u64>>) -> Result<(), ScheduleError> {
    let mut visited = BTreeSet::new();
    let mut visiting = BTreeSet::new();
    for node in graph.keys().copied() {
        visit(node, graph, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn visit(
    node: u64,
    graph: &BTreeMap<u64, Vec<u64>>,
    visiting: &mut BTreeSet<u64>,
    visited: &mut BTreeSet<u64>,
) -> Result<(), ScheduleError> {
    if visited.contains(&node) {
        return Ok(());
    }
    if !visiting.insert(node) {
        return Err(ScheduleError::Cycle(node));
    }
    let dependencies = graph
        .get(&node)
        .ok_or(ScheduleError::UnknownDependency(node))?;
    for dependency in dependencies {
        if !graph.contains_key(dependency) {
            return Err(ScheduleError::UnknownDependency(*dependency));
        }
        visit(*dependency, graph, visiting, visited)?;
    }
    visiting.remove(&node);
    visited.insert(node);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        DependencyDecision, DependencyState, ScheduleError, dependency_decision,
        file_claims_cover_path, normalize_dependencies, normalize_file_claims,
        validate_dependency_graph, writer_claims_conflict,
    };

    #[test]
    fn dependency_validation_deduplicates_and_rejects_cycles() {
        let graph = BTreeMap::from([(1, Vec::new()), (2, vec![1])]);
        assert_eq!(
            normalize_dependencies(3, &[2, 1, 2], &graph),
            Ok(vec![1, 2])
        );
        assert!(matches!(
            normalize_dependencies(3, &[4], &graph),
            Err(ScheduleError::UnknownDependency(4))
        ));

        let cyclic = BTreeMap::from([(1, vec![3]), (2, vec![1])]);
        assert!(matches!(
            normalize_dependencies(3, &[2], &cyclic),
            Err(ScheduleError::Cycle(_))
        ));
        assert!(matches!(
            validate_dependency_graph(&BTreeMap::from([(1, vec![2])])),
            Err(ScheduleError::UnknownDependency(2))
        ));
        assert!(matches!(
            validate_dependency_graph(&BTreeMap::from([(1, vec![1])])),
            Err(ScheduleError::SelfDependency(1))
        ));
    }

    #[test]
    fn failed_dependencies_take_precedence_over_waiting() {
        let states = BTreeMap::from([
            (1, DependencyState::Pending),
            (2, DependencyState::Failed),
            (3, DependencyState::Succeeded),
        ]);
        assert_eq!(
            dependency_decision(&[1, 2, 3], &states),
            Ok(DependencyDecision::Failed(vec![2]))
        );
        assert_eq!(
            dependency_decision(&[1, 3], &states),
            Ok(DependencyDecision::Waiting(vec![1]))
        );
        assert_eq!(
            dependency_decision(&[3], &states),
            Ok(DependencyDecision::Ready)
        );
    }

    #[test]
    fn file_claims_are_normalized_component_wise_and_fail_closed() {
        assert_eq!(
            normalize_file_claims(&[
                "src\\lib.rs".to_owned(),
                "./src/lib.rs".to_owned(),
                "tests".to_owned(),
            ]),
            Ok(vec!["src/lib.rs".to_owned(), "tests".to_owned()])
        );
        assert!(normalize_file_claims(&["../outside".to_owned()]).is_err());
        assert!(normalize_file_claims(&["C:\\outside".to_owned()]).is_err());
        assert!(normalize_file_claims(&["/outside".to_owned()]).is_err());
    }

    #[test]
    fn unknown_claims_are_exclusive_and_prefixes_match_components() {
        assert!(writer_claims_conflict(&[], &["src/lib.rs".to_owned()]));
        assert!(writer_claims_conflict(
            &["src".to_owned()],
            &["src/lib.rs".to_owned()]
        ));
        assert!(writer_claims_conflict(
            &["SRC/Parser.rs".to_owned()],
            &["src/parser.rs".to_owned()]
        ));
        assert!(!writer_claims_conflict(
            &["src".to_owned()],
            &["src2/lib.rs".to_owned()]
        ));
        assert!(!writer_claims_conflict(
            &["src/a.rs".to_owned()],
            &["src/b.rs".to_owned()]
        ));
        assert!(file_claims_cover_path(&["src".to_owned()], "SRC/parser.rs"));
        assert!(file_claims_cover_path(
            &["src/parser.rs".to_owned()],
            "src/parser.rs"
        ));
        assert!(!file_claims_cover_path(
            &["src/parser.rs".to_owned()],
            "src/parser_tests.rs"
        ));
        assert!(!file_claims_cover_path(
            &["src".to_owned()],
            "../src/parser.rs"
        ));
    }

    #[test]
    fn claim_matching_does_not_trim_real_path_components() {
        assert_eq!(
            normalize_file_claims(&[" src/lib.rs".to_owned()]),
            Ok(vec![" src/lib.rs".to_owned()])
        );
        assert!(!file_claims_cover_path(
            &["src/lib.rs".to_owned()],
            " src/lib.rs"
        ));
    }

    #[test]
    fn claim_matching_folds_non_ascii_case() {
        assert!(writer_claims_conflict(
            &["src/Ä.rs".to_owned()],
            &["SRC/ä.rs".to_owned()]
        ));
        assert!(file_claims_cover_path(&["src/Ä.rs".to_owned()], "src/ä.rs"));
    }
}
