//! Repository-scoped PHP function identities; methods/constructors stay on their
//! existing resolution path. PHP function names are case-insensitive.

use std::collections::{HashMap, HashSet};

use nestweaver_parser::{RawReference, RawSymbol, ReferenceKind};
use nestweaver_schema::SymbolKind;

pub(crate) struct PhpFunctions<'a> {
    definitions: HashMap<String, Vec<(&'a str, &'a RawSymbol)>>,
    references: HashMap<&'a str, &'a [RawReference]>,
    calls: HashSet<(&'a str, usize, &'a str)>,
    incomplete: bool,
}

impl<'a> PhpFunctions<'a> {
    pub(crate) fn build(
        files: &'a [(String, Vec<RawSymbol>, Vec<RawReference>)],
        is_php: impl Fn(&str) -> bool,
    ) -> Self {
        let mut this = Self {
            definitions: HashMap::new(),
            references: HashMap::new(),
            calls: HashSet::new(),
            incomplete: false,
        };
        for (file, symbols, references) in files.iter().filter(|(file, _, _)| is_php(file)) {
            this.references.insert(file, references);
            for reference in references {
                if reference.kind == ReferenceKind::PhpFunctionCall
                    && let Some(scope) = reference.scope
                {
                    this.calls
                        .insert((file, scope.position, reference.name.as_str()));
                }
            }
            for symbol in symbols.iter().filter(|s| s.kind == SymbolKind::Function) {
                let Some(namespace) = symbol.scope_chain.as_deref() else {
                    // A graph-only stub has lost namespace identity. Treating it
                    // as global could fabricate a fallback edge in another file.
                    this.incomplete = true;
                    continue;
                };
                let name = qualified(namespace, &symbol.name);
                this.definitions
                    .entry(name.to_ascii_lowercase())
                    .or_default()
                    .push((file, symbol));
            }
        }
        this
    }

    /// Outer None means this is not a named PHP function call; inner None is
    /// authoritative refusal, so generic basename/directory tiers cannot guess.
    pub(crate) fn resolve(
        &self,
        file: &str,
        reference: &RawReference,
    ) -> Option<Option<(&'a str, &'a RawSymbol)>> {
        let position = reference.scope?.position;
        if reference.kind != ReferenceKind::Call
            || reference.receiver.is_some()
            || !self
                .calls
                .contains(&(file, position, reference.name.as_str()))
        {
            return None;
        }
        if self.incomplete {
            return Some(None);
        }
        let references = self.references.get(file)?;
        let namespace =
            nestweaver_parser::php::namespace_for_position(references, position).unwrap_or("");
        let name = reference.name.as_str();
        let lookup = |name: String| self.definitions.get(&name.to_ascii_lowercase());
        let unique = |candidates: Option<&Vec<(&'a str, &'a RawSymbol)>>| {
            candidates.filter(|c| c.len() == 1).map(|c| c[0])
        };
        if name.starts_with('\\') {
            return Some(unique(lookup(name.trim_start_matches('\\').to_string())));
        }
        if name
            .get(..10)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("namespace\\"))
        {
            return Some(unique(lookup(qualified(namespace, &name[10..]))));
        }
        let (head, rest) = name
            .split_once('\\')
            .map_or((name, None), |(head, tail)| (head, Some(tail)));
        let import_kind = if rest.is_some() {
            ReferenceKind::PhpNamespaceAlias
        } else {
            ReferenceKind::PhpFunctionImport
        };
        let imports: Vec<_> = references
            .iter()
            .filter(|r| {
                r.kind == import_kind
                    && r.name.eq_ignore_ascii_case(head)
                    && r.scope
                        .is_some_and(|s| s.start <= position && position < s.end)
            })
            .collect();
        if !imports.is_empty() {
            return Some(if imports.len() != 1 {
                None
            } else {
                unique(lookup(rest.map_or_else(
                    || imports[0].context.clone(),
                    |tail| qualified(&imports[0].context, tail),
                )))
            });
        }
        let current = lookup(qualified(namespace, name));
        // An ambiguous namespace match must not borrow a global definition.
        if current.is_some() || rest.is_some() || namespace.is_empty() {
            return Some(unique(current));
        }
        Some(unique(lookup(name.to_string())))
    }
}

fn qualified(namespace: &str, name: &str) -> String {
    if namespace.is_empty() {
        name.to_string()
    } else {
        format!("{namespace}\\{name}")
    }
}
