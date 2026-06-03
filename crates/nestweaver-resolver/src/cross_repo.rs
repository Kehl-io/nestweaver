use nestweaver_schema::{
    CrossRepoLinkType, EdgeType, Language, MatchType, ResolvedEdge, confidence_score,
};

/// Per-repo data for cross-repo link detection.
pub struct RepoSymbols {
    pub repo_uid: String,
    pub language: Language,
    /// (package_name, [symbol_names])
    pub package_exports: Vec<(String, Vec<String>)>,
    /// imported package names
    pub package_imports: Vec<String>,
    /// (symbol_uid, contract_value like "/api/v1/users")
    pub contract_strings: Vec<(String, String)>,
}

/// Find cross-repo links using two signals:
///
/// 1. **Shared package imports**: Repo A imports package X, Repo B exports package X
///    → CROSS_REPO_LINK with SharedImport confidence
///
/// 2. **Contract matching**: matching API route strings across repos
///    → CROSS_REPO_LINK with ContractMatch, lower confidence
pub fn find_cross_repo_links(repos: &[RepoSymbols]) -> Vec<ResolvedEdge> {
    let mut edges: Vec<ResolvedEdge> = Vec::new();

    // Signal 1: Shared package imports
    // For each repo that imports a package, find repos that export that package
    for importer in repos {
        for imported_pkg in &importer.package_imports {
            for exporter in repos {
                if exporter.repo_uid == importer.repo_uid {
                    continue;
                }
                if exporter
                    .package_exports
                    .iter()
                    .any(|(pkg, _)| pkg == imported_pkg)
                {
                    // Repo importer depends on repo exporter via shared package
                    let confidence = confidence_score(MatchType::ImportResolved, importer.language);
                    edges.push(ResolvedEdge {
                        source_uid: importer.repo_uid.clone(),
                        target_uid: exporter.repo_uid.clone(),
                        edge_type: EdgeType::CrossRepoLink,
                        confidence,
                        link_type: Some(CrossRepoLinkType::SharedImport),
                        evidence: Vec::new(),
                    });
                }
            }
        }
    }

    // Signal 2: Contract matching — matching API route strings across repos
    // Build a map: contract_value → [(repo_uid, symbol_uid, language)]
    let mut contract_map: std::collections::HashMap<String, Vec<(String, String, Language)>> =
        std::collections::HashMap::new();
    for repo in repos {
        for (sym_uid, contract_val) in &repo.contract_strings {
            contract_map.entry(contract_val.clone()).or_default().push((
                repo.repo_uid.clone(),
                sym_uid.clone(),
                repo.language,
            ));
        }
    }

    // For each contract value shared across multiple repos, emit edges
    for repo_syms in contract_map.values() {
        if repo_syms.len() < 2 {
            continue;
        }
        // Emit edges between all pairs
        for i in 0..repo_syms.len() {
            for j in (i + 1)..repo_syms.len() {
                let (repo_a, sym_a, lang_a) = &repo_syms[i];
                let (repo_b, sym_b, _lang_b) = &repo_syms[j];
                if repo_a == repo_b {
                    continue;
                }
                // Lower confidence for contract matching (use SamePackageFallback as proxy)
                let confidence = confidence_score(MatchType::SamePackageFallback, *lang_a);
                edges.push(ResolvedEdge {
                    source_uid: sym_a.clone(),
                    target_uid: sym_b.clone(),
                    edge_type: EdgeType::CrossRepoLink,
                    confidence,
                    link_type: Some(CrossRepoLinkType::ContractMatch),
                    evidence: Vec::new(),
                });
            }
        }
    }

    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_shared_package_import() {
        let repos = vec![
            RepoSymbols {
                repo_uid: "repo:a".to_string(),
                language: nestweaver_schema::Language::Go,
                package_exports: vec![("my-lib".to_string(), vec!["FooApi".to_string()])],
                package_imports: vec![],
                contract_strings: vec![],
            },
            RepoSymbols {
                repo_uid: "repo:b".to_string(),
                language: nestweaver_schema::Language::Go,
                package_exports: vec![],
                package_imports: vec!["my-lib".to_string()],
                contract_strings: vec![],
            },
        ];

        let edges = find_cross_repo_links(&repos);
        let shared = edges
            .iter()
            .find(|e| e.link_type == Some(CrossRepoLinkType::SharedImport));
        assert!(
            shared.is_some(),
            "should detect shared package import; edges: {edges:?}"
        );
        let edge = shared.unwrap();
        assert_eq!(edge.source_uid, "repo:b");
        assert_eq!(edge.target_uid, "repo:a");
        assert_eq!(edge.edge_type, EdgeType::CrossRepoLink);
        assert!(edge.confidence > 0.0);
    }

    #[test]
    fn detects_contract_match() {
        let repos = vec![
            RepoSymbols {
                repo_uid: "repo:api".to_string(),
                language: nestweaver_schema::Language::Go,
                package_exports: vec![],
                package_imports: vec![],
                contract_strings: vec![(
                    "sym:repo:api:users:1".to_string(),
                    "/api/v1/users".to_string(),
                )],
            },
            RepoSymbols {
                repo_uid: "repo:client".to_string(),
                language: nestweaver_schema::Language::Go,
                package_exports: vec![],
                package_imports: vec![],
                contract_strings: vec![(
                    "sym:repo:client:fetch:10".to_string(),
                    "/api/v1/users".to_string(),
                )],
            },
        ];

        let edges = find_cross_repo_links(&repos);
        let contract = edges
            .iter()
            .find(|e| e.link_type == Some(CrossRepoLinkType::ContractMatch));
        assert!(
            contract.is_some(),
            "should detect contract match; edges: {edges:?}"
        );
        let edge = contract.unwrap();
        assert_eq!(edge.edge_type, EdgeType::CrossRepoLink);
        assert!(edge.confidence > 0.0);
        // ContractMatch confidence should be lower than SharedImport confidence
        let shared_conf = nestweaver_schema::confidence_score(
            nestweaver_schema::MatchType::ImportResolved,
            nestweaver_schema::Language::Go,
        );
        assert!(
            edge.confidence < shared_conf,
            "contract match confidence ({}) should be lower than shared import ({})",
            edge.confidence,
            shared_conf
        );
    }

    #[test]
    fn no_self_links() {
        let repos = vec![RepoSymbols {
            repo_uid: "repo:a".to_string(),
            language: nestweaver_schema::Language::Go,
            package_exports: vec![("my-lib".to_string(), vec![])],
            package_imports: vec!["my-lib".to_string()],
            contract_strings: vec![],
        }];

        let edges = find_cross_repo_links(&repos);
        for edge in &edges {
            assert_ne!(
                edge.source_uid, edge.target_uid,
                "should not produce self-links"
            );
        }
    }

    #[test]
    fn no_cross_repo_links_when_no_overlap() {
        let repos = vec![
            RepoSymbols {
                repo_uid: "repo:a".to_string(),
                language: nestweaver_schema::Language::Go,
                package_exports: vec![("lib-a".to_string(), vec![])],
                package_imports: vec!["lib-b".to_string()],
                contract_strings: vec![],
            },
            RepoSymbols {
                repo_uid: "repo:b".to_string(),
                language: nestweaver_schema::Language::Go,
                package_exports: vec![("lib-b-other".to_string(), vec![])],
                package_imports: vec!["lib-c".to_string()],
                contract_strings: vec![],
            },
        ];

        let edges = find_cross_repo_links(&repos);
        let shared = edges
            .iter()
            .filter(|e| e.link_type == Some(CrossRepoLinkType::SharedImport))
            .count();
        assert_eq!(shared, 0, "no shared package imports");
    }
}
