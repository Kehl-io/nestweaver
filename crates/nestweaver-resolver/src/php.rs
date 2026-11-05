//! Repository-scoped PHP function and static method identities. Instance calls
//! retain type lookup; PHP function, class and method names are case-insensitive.

use std::collections::{HashMap, HashSet};

use nestweaver_parser::{RawReference, RawSymbol, ReferenceKind};
use nestweaver_schema::SymbolKind;

pub(crate) struct PhpFunctions<'a> {
    definitions: HashMap<String, Vec<(&'a str, &'a RawSymbol)>>,
    references: HashMap<&'a str, &'a [RawReference]>,
    calls: HashSet<(&'a str, usize, &'a str)>,
    incomplete: bool,
    classes: PhpClasses<'a>,
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
            classes: PhpClasses::build(files, &is_php),
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
        source: &RawSymbol,
    ) -> Option<Option<(&'a str, &'a RawSymbol)>> {
        let position = reference.scope?.position;
        if reference.kind == ReferenceKind::Call
            && self
                .classes
                .static_calls
                .contains(&(file, position, reference.name.as_str()))
        {
            return Some(self.classes.resolve(file, reference, source));
        }
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

struct PhpClass<'a> {
    file: &'a str,
    name: &'a str,
    scope: nestweaver_parser::LexicalScope,
    facts: nestweaver_parser::php::PhpClassFacts,
    symbols: &'a [RawSymbol],
}

struct PhpClasses<'a> {
    definitions: HashMap<String, Vec<PhpClass<'a>>>,
    references: HashMap<&'a str, &'a [RawReference]>,
    static_calls: HashSet<(&'a str, usize, &'a str)>,
    anonymous: Vec<(&'a str, nestweaver_parser::LexicalScope)>,
    incomplete: bool,
}

impl<'a> PhpClasses<'a> {
    fn build(
        files: &'a [(String, Vec<RawSymbol>, Vec<RawReference>)],
        is_php: &impl Fn(&str) -> bool,
    ) -> Self {
        let mut this = Self {
            definitions: HashMap::new(),
            references: HashMap::new(),
            static_calls: HashSet::new(),
            anonymous: Vec::new(),
            incomplete: false,
        };
        for (file, symbols, references) in files.iter().filter(|(file, _, _)| is_php(file)) {
            this.references.insert(file, references);
            let mut declared = HashSet::new();
            for reference in references {
                if reference.kind == ReferenceKind::PhpStaticCall {
                    if let Some(scope) = reference.scope {
                        this.static_calls.insert((
                            file.as_str(),
                            scope.position,
                            reference.name.as_str(),
                        ));
                    }
                    continue;
                }
                if reference.kind != ReferenceKind::PhpClassDefinition {
                    continue;
                }
                let Some(scope) = reference.scope else {
                    this.incomplete = true;
                    continue;
                };
                if reference.name.is_empty() {
                    this.anonymous.push((file, scope));
                    continue;
                }
                let Ok(facts) = serde_json::from_str(&reference.context) else {
                    this.incomplete = true;
                    continue;
                };
                let short = reference
                    .name
                    .rsplit('\\')
                    .next()
                    .unwrap_or(&reference.name);
                let matching: Vec<_> = symbols
                    .iter()
                    .filter(|symbol| {
                        matches!(symbol.kind, SymbolKind::Class | SymbolKind::Enum)
                            && symbol.name == short
                            && symbol.start_line == reference.start_line
                    })
                    .collect();
                if matching.len() != 1 {
                    this.incomplete = true;
                    continue;
                }
                declared.insert((matching[0].name.as_str(), matching[0].start_line));
                this.definitions
                    .entry(reference.name.to_ascii_lowercase())
                    .or_default()
                    .push(PhpClass {
                        file,
                        name: &reference.name,
                        scope,
                        facts,
                        symbols,
                    });
            }
            // Missing parser facts in graph-only stubs are unknown identity,
            // never evidence that an imported/namespaced homonym is absent.
            if symbols.iter().any(|symbol| {
                matches!(symbol.kind, SymbolKind::Class | SymbolKind::Enum)
                    && !declared.contains(&(symbol.name.as_str(), symbol.start_line))
            }) {
                this.incomplete = true;
            }
        }
        this
    }

    fn unique_class(&self, name: &str) -> Option<&PhpClass<'a>> {
        let candidates = self.definitions.get(&name.to_ascii_lowercase())?;
        (candidates.len() == 1).then(|| &candidates[0])
    }

    fn lexical_class(&self, file: &str, position: usize) -> Option<&PhpClass<'a>> {
        if self
            .anonymous
            .iter()
            .any(|(f, scope)| *f == file && scope.start <= position && position < scope.end)
        {
            return None;
        }
        let mut candidates: Vec<_> = self
            .definitions
            .values()
            .flatten()
            .filter(|class| {
                class.file == file && class.scope.start <= position && position < class.scope.end
            })
            .collect();
        candidates.sort_by_key(|class| class.scope.end - class.scope.start);
        let first = candidates.first().copied()?;
        if candidates.get(1).is_some_and(|second| {
            second.scope.end - second.scope.start == first.scope.end - first.scope.start
        }) {
            return None;
        }
        self.unique_class(first.name)
    }

    fn class_name(&self, file: &str, position: usize, name: &str) -> Option<String> {
        if name.is_empty()
            || name.trim_start_matches('\\').split('\\').any(|part| {
                let mut chars = part.chars();
                !chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
                    || !chars.all(|c| c.is_alphanumeric() || c == '_')
            })
        {
            return None;
        }
        let references = self.references.get(file)?;
        let namespace =
            nestweaver_parser::php::namespace_for_position(references, position).unwrap_or("");
        if name.starts_with('\\') {
            return Some(name.trim_start_matches('\\').to_owned());
        }
        if name
            .get(..10)
            .is_some_and(|p| p.eq_ignore_ascii_case("namespace\\"))
        {
            return Some(qualified(namespace, &name[10..]));
        }
        let (head, tail) = name
            .split_once('\\')
            .map_or((name, None), |(h, t)| (h, Some(t)));
        let imports: Vec<_> = references
            .iter()
            .filter(|r| {
                r.kind == ReferenceKind::PhpNamespaceAlias
                    && r.name.eq_ignore_ascii_case(head)
                    && r.scope
                        .is_some_and(|scope| scope.start <= position && position < scope.end)
            })
            .collect();
        if !imports.is_empty() {
            if imports.len() != 1 {
                return None;
            }
            return Some(tail.map_or_else(
                || imports[0].context.clone(),
                |tail| qualified(&imports[0].context, tail),
            ));
        }
        // Unlike functions, PHP class names never fall back to global names.
        Some(qualified(namespace, name))
    }

    fn parent(&self, class: &PhpClass<'a>) -> Option<&PhpClass<'a>> {
        let parent = self.class_name(
            class.file,
            class.scope.position,
            class.facts.parent.as_deref()?,
        )?;
        self.unique_class(&parent)
    }

    fn is_descendant<'s>(&'s self, mut class: &'s PhpClass<'a>, parent: &PhpClass<'a>) -> bool {
        let mut seen = HashSet::new();
        for _ in 0..32 {
            if class.name.eq_ignore_ascii_case(parent.name) {
                return true;
            }
            if !seen.insert(class.name.to_ascii_lowercase()) {
                return false;
            }
            let Some(next) = self.parent(class) else {
                return false;
            };
            class = next;
        }
        false
    }

    fn method<'s>(
        &'s self,
        mut class: &'s PhpClass<'a>,
        name: &str,
    ) -> Option<(
        &'s PhpClass<'a>,
        &'s nestweaver_parser::php::PhpMethodFacts,
        &'a RawSymbol,
    )> {
        let mut seen = HashSet::new();
        for _ in 0..32 {
            if !seen.insert(class.name.to_ascii_lowercase()) {
                return None;
            }
            let methods: Vec<_> = class
                .facts
                .methods
                .iter()
                .filter(|m| m.name.eq_ignore_ascii_case(name))
                .collect();
            if !methods.is_empty() {
                if methods.len() != 1 || methods[0].is_abstract {
                    return None;
                }
                let method = methods[0];
                let symbols: Vec<_> = class
                    .symbols
                    .iter()
                    .filter(|s| {
                        s.kind == SymbolKind::Method
                            && s.name.eq_ignore_ascii_case(&method.name)
                            && s.start_line == method.start_line
                            && s.end_line == method.end_line
                            && s.parent_name.as_deref().is_some_and(|parent| {
                                parent.eq_ignore_ascii_case(
                                    class.name.rsplit('\\').next().unwrap_or(class.name),
                                )
                            })
                    })
                    .collect();
                return (symbols.len() == 1).then(|| (class, method, symbols[0]));
            }
            // Trait adaptation has its own dispatch rules. An unknown trait
            // method must not borrow an equally named parent implementation.
            if class.facts.has_traits {
                return None;
            }
            class = self.parent(class)?;
        }
        None
    }

    fn resolve(
        &self,
        file: &str,
        reference: &RawReference,
        source: &RawSymbol,
    ) -> Option<(&'a str, &'a RawSymbol)> {
        if self.incomplete {
            return None;
        }
        let position = reference.scope?.position;
        let receiver = reference.receiver.as_deref()?;
        let lexical = self.lexical_class(file, position);
        let relative = receiver.eq_ignore_ascii_case("self")
            || receiver.eq_ignore_ascii_case("parent")
            || receiver.eq_ignore_ascii_case("static");
        let class =
            if receiver.eq_ignore_ascii_case("self") || receiver.eq_ignore_ascii_case("static") {
                lexical?
            } else if receiver.eq_ignore_ascii_case("parent") {
                self.parent(lexical?)?
            } else {
                self.unique_class(&self.class_name(file, position, receiver)?)?
            };
        let (owner, method, symbol) = self.method(class, &reference.name)?;
        // Late-static binding can select an overriding descendant. Resolve it
        // only when class/method finality proves a single implementation. PHP 8
        // permits descendants to redeclare final private methods: finality has
        // no effect on them except for constructors.
        use nestweaver_schema::Visibility;
        let method_is_effectively_final = method.is_final
            && (symbol.visibility != Visibility::Private
                || method.name.eq_ignore_ascii_case("__construct"));
        if receiver.eq_ignore_ascii_case("static")
            && !class.facts.is_final
            && !method_is_effectively_final
        {
            return None;
        }
        if !method.is_static {
            let instance_context = relative
                && source.kind == SymbolKind::Method
                && lexical.is_some_and(|context| {
                    context.facts.methods.iter().any(|m| {
                        m.name == source.name
                            && m.start_line == source.start_line
                            && !m.is_static
                            && m.scope.start <= position
                            && position < m.scope.end
                    })
                });
            if !instance_context {
                return None;
            }
        }
        match symbol.visibility {
            Visibility::Private
                if !lexical.is_some_and(|c| c.name.eq_ignore_ascii_case(owner.name)) =>
            {
                return None;
            }
            Visibility::Protected if !lexical.is_some_and(|c| self.is_descendant(c, owner)) => {
                return None;
            }
            _ => {}
        }
        Some((owner.file, symbol))
    }
}
