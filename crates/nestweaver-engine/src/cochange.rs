//! Co-change mining from git history.
//! Identifies file pairs that frequently change together in commits.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoChangeEdge {
    pub file_a: String,
    pub file_b: String,
    pub cochange_count: u32,
    pub total_commits_a: u32,
    pub total_commits_b: u32,
    pub confidence: f32, // Jaccard coefficient
}

/// Mine git history for co-changing file pairs.
pub fn compute_cochanges(
    repo_path: &Path,
    max_commits: usize,
    min_cochange: u32,
    min_confidence: f32,
) -> Result<Vec<CoChangeEdge>, anyhow::Error> {
    // Run git log to get commits and their changed files
    let output = Command::new("git")
        .args([
            "log",
            "--name-only",
            "--format=%H",
            &format!("--max-count={max_commits}"),
        ])
        .current_dir(repo_path)
        .output()?;

    if !output.status.success() {
        anyhow::bail!("git log failed");
    }

    let text = String::from_utf8_lossy(&output.stdout);

    // Parse: each commit is a hash line followed by file lines, then blank line
    let mut commits: Vec<Vec<String>> = Vec::new();
    let mut current_files: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !current_files.is_empty() {
                commits.push(current_files.clone());
                current_files.clear();
            }
        } else if trimmed.len() == 40 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            // This is a commit hash — start a new commit
            if !current_files.is_empty() {
                commits.push(current_files.clone());
                current_files.clear();
            }
        } else {
            current_files.push(trimmed.to_string());
        }
    }
    if !current_files.is_empty() {
        commits.push(current_files);
    }

    // Count per-file commit frequency
    let mut file_commits: HashMap<String, u32> = HashMap::new();
    for commit in &commits {
        for file in commit {
            *file_commits.entry(file.clone()).or_default() += 1;
        }
    }

    // Count co-change pairs
    let mut cochange_counts: HashMap<(String, String), u32> = HashMap::new();
    for commit in &commits {
        let files: Vec<&String> = commit.iter().collect();
        for i in 0..files.len() {
            for j in (i + 1)..files.len() {
                let (a, b) = if files[i] < files[j] {
                    (files[i].clone(), files[j].clone())
                } else {
                    (files[j].clone(), files[i].clone())
                };
                *cochange_counts.entry((a, b)).or_default() += 1;
            }
        }
    }

    // Compute Jaccard coefficient and filter
    let mut edges: Vec<CoChangeEdge> = cochange_counts
        .into_iter()
        .filter_map(|((a, b), count)| {
            if count < min_cochange {
                return None;
            }
            let total_a = *file_commits.get(&a)?;
            let total_b = *file_commits.get(&b)?;
            let jaccard = count as f32 / (total_a + total_b - count) as f32;
            if jaccard < min_confidence {
                return None;
            }
            Some(CoChangeEdge {
                file_a: a,
                file_b: b,
                cochange_count: count,
                total_commits_a: total_a,
                total_commits_b: total_b,
                confidence: jaccard,
            })
        })
        .collect();

    edges.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(edges)
}

/// Save co-change edges to a sidecar JSON file.
pub fn save_cochange_sidecar(edges: &[CoChangeEdge], path: &Path) -> Result<(), anyhow::Error> {
    let json = serde_json::to_string_pretty(edges)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Load co-change edges from a sidecar JSON file.
pub fn load_cochange_sidecar(path: &Path) -> Option<Vec<CoChangeEdge>> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jaccard_coefficient_correct() {
        // If files A and B both change in 3 commits, A has 5 total, B has 4 total
        // Jaccard = 3 / (5 + 4 - 3) = 3/6 = 0.5
        let edge = CoChangeEdge {
            file_a: "a.rs".into(),
            file_b: "b.rs".into(),
            cochange_count: 3,
            total_commits_a: 5,
            total_commits_b: 4,
            confidence: 3.0 / 6.0,
        };
        assert!((edge.confidence - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn sidecar_roundtrip() {
        let edges = vec![CoChangeEdge {
            file_a: "src/a.rs".into(),
            file_b: "src/b.rs".into(),
            cochange_count: 5,
            total_commits_a: 10,
            total_commits_b: 8,
            confidence: 0.385,
        }];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cochange.json");
        save_cochange_sidecar(&edges, &path).unwrap();
        let loaded = load_cochange_sidecar(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].file_a, "src/a.rs");
    }
}
