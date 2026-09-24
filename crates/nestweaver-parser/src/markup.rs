//! Identifiers a component's MARKUP uses, for the regex-based single-file
//! component parsers (`astro`, `svelte`, `vue`).
//!
//! nw-453. Those parsers scan only the `<script>` block (or Astro's
//! frontmatter) for references, so a function used only by the template --
//! `on:submit={handleSubmit}`, `@click="handleClick"`, `{formatDate(d)}` --
//! has no caller in the graph. Before nw-453 the blanket `/routes/`/`/pages/`
//! entry-point rule hid that on route pages by rooting every symbol there;
//! narrowing that rule to the frameworks' real exports made nearly every
//! interactive page's handlers look dead. Outside route directories the blind
//! spot was always there.
//!
//! The fix is rooting, not reference capture: a symbol in a component file
//! whose name the markup uses is an entry point, because the framework (not
//! any code in the graph) is what calls it. Emitting template references
//! instead would need a SOURCE symbol whose span contains the markup, and a
//! classic Vue SFC's component symbol spans only `export default { ... }`, so
//! the resolver would attribute template references to nothing or to a wrong
//! neighbour -- a confidently wrong edge is worse than a declared root.
//!
//! Only EXPRESSION positions count: `{...}` (Svelte/Astro text, attributes and
//! directives, Vue's `{{ ... }}`), and for Vue the values of `@x`, `:x`, `v-*`
//! and `#slot` attributes. A word in static text such as `<p>total</p>` is not
//! a use. A member access (`item.name`) is not a use of `name`.

use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

/// `<script ...> ... </script>` and `<style ...> ... </style>` blocks.
static RE_SCRIPT_OR_STYLE_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<script\b[^>]*>.*?</script\s*>|<style\b[^>]*>.*?</style\s*>").unwrap()
});

/// A Vue directive attribute with a quoted expression value:
/// `@click="h"`, `:prop="x"`, `v-on:submit.prevent="h"`, `v-model="x"`,
/// `#default="{ item }"`.
static RE_VUE_DIRECTIVE_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:^|\s)(?:@|:|v-|#)[\w\-:.\[\]]*\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap()
});

/// An identifier that is not a member access (not preceded by `.`).
static RE_IDENTIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[^.\w$])([A-Za-z_$][\w$]*)").unwrap());

/// Which markup dialect's expression positions to read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// Svelte and Astro: every expression is inside `{...}`.
    Braces,
    /// Vue: `{{ ... }}` plus directive attribute values.
    Vue,
}

/// Every identifier used in an expression position of `markup`, where
/// `markup` is the component's template with its script (and, for Astro, its
/// frontmatter) already removed. `<script>`/`<style>` blocks still inside it
/// are dropped here: an Astro page's client `<script>` is a separate scope.
pub(crate) fn markup_identifiers(markup: &str, dialect: Dialect) -> HashSet<String> {
    let markup = RE_SCRIPT_OR_STYLE_BLOCK.replace_all(markup, "");
    let mut expressions = brace_expressions(&markup);
    if dialect == Dialect::Vue {
        for cap in RE_VUE_DIRECTIVE_VALUE.captures_iter(&markup) {
            if let Some(value) = cap.get(1).or_else(|| cap.get(2)) {
                expressions.push(value.as_str().to_string());
            }
        }
    }
    expressions
        .iter()
        .flat_map(|expression| {
            RE_IDENTIFIER
                .captures_iter(expression)
                .map(|cap| cap[1].to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The text inside every top-level `{...}` span (nested braces included).
/// An unbalanced trailing `{` contributes what follows it, which can only
/// over-root, never hide a use.
fn brace_expressions(markup: &str) -> Vec<String> {
    let mut expressions = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in markup.chars() {
        match ch {
            '{' => {
                if depth > 0 {
                    current.push(ch);
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    expressions.push(std::mem::take(&mut current));
                } else {
                    current.push(ch);
                }
            }
            _ if depth > 0 => current.push(ch),
            _ => {}
        }
    }
    if !current.is_empty() {
        expressions.push(current);
    }
    expressions
}

/// Root every non-component symbol whose name the markup uses.
///
/// The component itself is already an entry point. `EventListener` is the
/// existing overload for UI-bound code (see `detect_js_ts`'s React notes):
/// a template-bound handler is exactly that. A symbol already rooted keeps
/// its more specific kind.
pub(crate) fn root_markup_used_symbols(
    symbols: &mut [crate::parse::RawSymbol],
    used: &HashSet<String>,
) {
    for symbol in symbols.iter_mut() {
        if !symbol.is_entry_point && used.contains(&symbol.name) {
            symbol.is_entry_point = true;
            symbol.entry_point_kind = Some(nestweaver_schema::EntryPointKind::EventListener);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svelte_expressions_and_directives_are_uses() {
        let used = markup_identifiers(
            "<form on:submit|preventDefault={handleSubmit}>\n  <input bind:value={name} />\n  <p>{computeTotal(items)}</p>\n  {#if open}<Modal />{/if}\n</form>",
            Dialect::Braces,
        );
        for name in ["handleSubmit", "name", "computeTotal", "items", "open"] {
            assert!(used.contains(name), "{name} is used; got {used:?}");
        }
    }

    #[test]
    fn vue_directive_values_and_mustaches_are_uses() {
        let used = markup_identifiers(
            "<button @click=\"handleClick\" :disabled=\"isBusy(state)\" v-on:keyup.enter='onEnter'>{{ label }}</button>\n<li v-for=\"row in rows\" #item=\"{ entry }\"></li>",
            Dialect::Vue,
        );
        for name in [
            "handleClick",
            "isBusy",
            "state",
            "onEnter",
            "label",
            "rows",
            "entry",
        ] {
            assert!(used.contains(name), "{name} is used; got {used:?}");
        }
    }

    /// COUNTERWEIGHT. Static text, plain attribute values, member names and
    /// client `<script>` blocks are not uses, or every symbol whose name is an
    /// ordinary word would be rooted.
    #[test]
    fn static_text_members_and_scripts_are_not_uses() {
        let used = markup_identifiers(
            "<p class=\"summary\">summary</p>\n<p>{item.total}</p>\n<script>unusedHelper()</script>\n<style>.x { color: red }</style>",
            Dialect::Braces,
        );
        assert!(!used.contains("summary"), "{used:?}");
        assert!(!used.contains("total"), "{used:?}");
        assert!(!used.contains("unusedHelper"), "{used:?}");
        assert!(!used.contains("color"), "{used:?}");
        assert!(used.contains("item"), "{used:?}");

        let vue = markup_identifiers("<p title=\"helper\">helper</p>", Dialect::Vue);
        assert!(!vue.contains("helper"), "{vue:?}");
    }
}
