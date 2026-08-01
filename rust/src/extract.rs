//! Tier-1 extraction: source text -> symbols + unresolved call intents.
//!
//! Mirrors the TypeScript implementation's split. Symbols are found per file;
//! call *targets* are deliberately left unresolved here, because a callee named
//! in one file is usually defined in another. Resolution happens once the whole
//! repo's symbols are known — see `resolve_edges`.
//!
//! Extraction is query-driven rather than a hand-rolled walk: a tree-sitter query
//! states which shapes are definitions, so adding a language means adding a query
//! rather than another arm in a recursive match.

use anyhow::{Context, Result};
use std::collections::HashMap;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::repo::Lang;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub signature: String,
}

/// A call site whose target is a bare name, not yet tied to a symbol row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallIntent {
    /// Index into the file's symbol list — the symbol the call appears inside.
    /// None for a top-level call, which belongs to the file itself: a module-level
    /// `register(x)` is a real dependency and dropping it loses the edge entirely.
    pub from: Option<usize>,
    pub callee: String,
    /// Receiver text for a member call (`repo` in `repo.scan()`). None for a bare
    /// call. This is what turns an ambiguous method name into one target.
    pub receiver: Option<String>,
}

/// A variable, parameter or field bound to a type name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Symbol the binding is scoped to; None at module level.
    pub owner: Option<usize>,
    pub name: String,
    pub ty: String,
}

pub struct Extracted {
    pub symbols: Vec<Symbol>,
    pub calls: Vec<CallIntent>,
    pub bindings: Vec<Binding>,
    /// Parallel to `symbols`: the enclosing class name for a method, else None.
    /// Used to resolve a receiver-typed call to one class's method.
    pub containers: Vec<Option<String>>,
    /// Parallel to `symbols`: index of the innermost enclosing symbol, else None
    /// for a top-level definition (whose parent is the file). Drives `contains`.
    pub parents: Vec<Option<usize>>,
    /// Module specifiers this file imports, verbatim (`./stats.js`, `node:fs`).
    pub imports: Vec<String>,
}

/// Definition shapes. `@name` is the identifier stored; the outer capture is the
/// span. Order matters only for readability — matches are keyed by capture name.
const TS_DEFS: &str = r#"
(function_declaration name: (identifier) @name) @def
(generator_function_declaration name: (identifier) @name) @def
(class_declaration name: (type_identifier) @name) @def
(interface_declaration name: (type_identifier) @name) @def
(type_alias_declaration name: (type_identifier) @name) @def
(enum_declaration name: (identifier) @name) @def
(method_definition name: (property_identifier) @name) @def
(public_field_definition name: (property_identifier) @name
  value: [(arrow_function) (function_expression)]) @def
(variable_declarator
  name: (identifier) @name
  value: [(arrow_function) (function_expression) (generator_function)]) @def
"#;

/// Call sites. `foo()` and `obj.foo()` both record the bare callee name, which is
/// what the repo-wide index is keyed on.
const TS_CALLS: &str = r#"
(call_expression function: (identifier) @callee)
(call_expression function: (member_expression object: (_) @recv property: (property_identifier) @callee))
(new_expression constructor: (identifier) @callee)
"#;

/// Type bindings. Each alternative names the variable and the type it carries, so
/// a later member call on that variable resolves to one class's method rather
/// than to every method in the repo sharing the name.
/// Import specifiers. `export ... from` is an import too — it pulls the module in
/// and re-exposes it, so omitting it would lose a real file-to-file dependency.
const TS_IMPORTS: &str = r#"
(import_statement source: (string) @spec)
(export_statement source: (string) @spec)
"#;

const TS_BINDINGS: &str = r#"
(variable_declarator
  name: (identifier) @name
  value: (new_expression constructor: (identifier) @ty))
(variable_declarator
  name: (identifier) @name
  type: (type_annotation (type_identifier) @ty))
(required_parameter
  pattern: (identifier) @name
  type: (type_annotation (type_identifier) @ty))
(public_field_definition
  name: (property_identifier) @field
  type: (type_annotation (type_identifier) @ty))
"#;

fn language(lang: Lang) -> tree_sitter::Language {
    match lang {
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
    }
}

/// A node's kind, mapped to the vocabulary stored in `symbols.kind`.
fn kind_of(n: &Node) -> &'static str {
    match n.kind() {
        "class_declaration" => "class",
        "interface_declaration" => "interface",
        "type_alias_declaration" => "type",
        "enum_declaration" => "enum",
        "method_definition" => "method",
        _ => "function",
    }
}

/// First line of the definition, trimmed — enough to identify a symbol without
/// storing its body. Cheap stand-in for the TypeScript implementation's richer
/// signature rendering; it is what `grep` and `skeleton` display.
fn signature_of(src: &str, n: &Node) -> String {
    let text = &src[n.byte_range()];
    let line = text.lines().next().unwrap_or("").trim();
    let line = line.strip_suffix('{').unwrap_or(line).trim_end();
    let mut s = line.to_string();
    if s.len() > 200 {
        s.truncate(200);
        s.push('…');
    }
    s
}

pub fn extract(lang: Lang, src: &str) -> Result<Extracted> {
    let ts_lang = language(lang);
    let mut parser = Parser::new();
    parser.set_language(&ts_lang).context("set_language")?;
    let tree = parser.parse(src, None).context("parse returned no tree")?;
    let root = tree.root_node();

    let def_q = Query::new(&ts_lang, TS_DEFS).context("compile definition query")?;
    let name_idx = def_q
        .capture_index_for_name("name")
        .context("query lacks @name")?;
    let def_idx = def_q
        .capture_index_for_name("def")
        .context("query lacks @def")?;

    let mut symbols: Vec<Symbol> = Vec::new();
    // Byte span of each symbol, kept parallel to `symbols`, so a call site can be
    // attributed to the innermost definition containing it.
    let mut spans: Vec<(usize, usize)> = Vec::new();

    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&def_q, root, src.as_bytes());
    while let Some(m) = it.next() {
        let name_node = m.captures.iter().find(|c| c.index == name_idx).map(|c| c.node);
        let def_node = m.captures.iter().find(|c| c.index == def_idx).map(|c| c.node);
        let (Some(name_node), Some(def_node)) = (name_node, def_node) else {
            continue;
        };
        symbols.push(Symbol {
            name: src[name_node.byte_range()].to_string(),
            kind: kind_of(&def_node).to_string(),
            // tree-sitter rows are 0-based; every display surface is 1-based.
            start_line: def_node.start_position().row as i64 + 1,
            end_line: def_node.end_position().row as i64 + 1,
            signature: signature_of(src, &def_node),
        });
        spans.push((def_node.start_byte(), def_node.end_byte()));
    }

    // Innermost definition containing a byte offset. Narrowest wins, so a call in
    // a method is credited to the method rather than to the class around it.
    let owner_at = |at: usize| -> Option<usize> {
        spans
            .iter()
            .enumerate()
            .filter(|(_, (s, e))| *s <= at && at < *e)
            .min_by_key(|(_, (s, e))| e - s)
            .map(|(i, _)| i)
    };
    // Innermost enclosing *class*, which is what a method's container is.
    let class_at = |at: usize| -> Option<usize> {
        spans
            .iter()
            .enumerate()
            .filter(|(i, (s, e))| *s <= at && at < *e && symbols[*i].kind == "class")
            .min_by_key(|(_, (s, e))| e - s)
            .map(|(i, _)| i)
    };

    // Innermost enclosing symbol of any kind, excluding the symbol itself. This is
    // what `contains` is built from: class->method, function->nested function, and
    // (where None) file->top-level definition.
    let parents: Vec<Option<usize>> = (0..symbols.len())
        .map(|i| {
            let (ms, me) = spans[i];
            spans
                .iter()
                .enumerate()
                .filter(|(j, (s, e))| *j != i && *s <= ms && me <= *e)
                .min_by_key(|(_, (s, e))| e - s)
                .map(|(j, _)| j)
        })
        .collect();

    let containers: Vec<Option<String>> = symbols
        .iter()
        .enumerate()
        .map(|(i, sym)| {
            if sym.kind != "method" {
                return None;
            }
            // A method's own span is inside its class's span, so search from just
            // past its start to avoid matching itself.
            class_at(spans[i].0).map(|c| symbols[c].name.clone())
        })
        .collect();

    let call_q = Query::new(&ts_lang, TS_CALLS).context("compile call query")?;
    let callee_idx = call_q.capture_index_for_name("callee").context("query lacks @callee")?;
    let recv_idx = call_q.capture_index_for_name("recv").context("query lacks @recv")?;
    let mut calls = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&call_q, root, src.as_bytes());
    while let Some(m) = it.next() {
        // Per match, not per capture: a member call matches @recv and @callee
        // together, and iterating captures would record the call twice.
        let Some(callee_node) = m.captures.iter().find(|c| c.index == callee_idx).map(|c| c.node)
        else {
            continue;
        };
        calls.push(CallIntent {
            from: owner_at(callee_node.start_byte()),
            callee: src[callee_node.byte_range()].to_string(),
            receiver: m
                .captures
                .iter()
                .find(|c| c.index == recv_idx)
                .map(|c| src[c.node.byte_range()].to_string()),
        });
    }

    let bind_q = Query::new(&ts_lang, TS_BINDINGS).context("compile binding query")?;
    let b_name = bind_q.capture_index_for_name("name");
    let b_field = bind_q.capture_index_for_name("field");
    let b_ty = bind_q.capture_index_for_name("ty").context("query lacks @ty")?;
    let mut bindings = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&bind_q, root, src.as_bytes());
    while let Some(m) = it.next() {
        let Some(ty_node) = m.captures.iter().find(|c| c.index == b_ty).map(|c| c.node) else {
            continue;
        };
        let ty = src[ty_node.byte_range()].to_string();

        // A class field binds `this.<field>` and is scoped to the class, so a
        // method body's `this.repo.scan()` finds it. A plain variable or parameter
        // binds its own name in whatever definition encloses it.
        if let Some(f) = b_field.and_then(|i| m.captures.iter().find(|c| c.index == i)) {
            let at = f.node.start_byte();
            bindings.push(Binding {
                owner: class_at(at),
                name: format!("this.{}", &src[f.node.byte_range()]),
                ty,
            });
        } else if let Some(n) = b_name.and_then(|i| m.captures.iter().find(|c| c.index == i)) {
            let at = n.node.start_byte();
            bindings.push(Binding {
                owner: owner_at(at),
                name: src[n.node.byte_range()].to_string(),
                ty,
            });
        }
    }

    let imp_q = Query::new(&ts_lang, TS_IMPORTS).context("compile import query")?;
    let mut imports = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&imp_q, root, src.as_bytes());
    while let Some(m) = it.next() {
        for c in m.captures {
            // The capture is a string literal including its quotes.
            let raw = &src[c.node.byte_range()];
            let spec = raw.trim_matches(|ch| ch == '"' || ch == '\'' || ch == '`');
            if !spec.is_empty() {
                imports.push(spec.to_string());
            }
        }
    }

    Ok(Extracted { symbols, calls, bindings, containers, parents, imports })
}

/// Resolve bare callee names against the whole-repo symbol index.
///
/// Ambiguity is dropped, not guessed: a name defined in more than one file cannot
/// be attributed to one of them from a bare call site, and inventing an edge is
/// worse than omitting one — `callers` is used to decide what a change breaks.
pub fn resolve(
    by_name: &HashMap<String, Vec<i64>>,
    from_id: i64,
    callee: &str,
) -> Option<i64> {
    match by_name.get(callee)?.as_slice() {
        [only] => Some(*only),
        // Self-recursion resolves even when the name is ambiguous repo-wide.
        many if many.contains(&from_id) => Some(from_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(src: &str) -> Vec<(String, String)> {
        extract(Lang::TypeScript, src)
            .unwrap()
            .symbols
            .into_iter()
            .map(|s| (s.name, s.kind))
            .collect()
    }

    #[test]
    fn finds_the_definition_shapes_typescript_actually_uses() {
        let src = r#"
export function greet(name: string): string { return `hi ${name}`; }
export const shout = (s: string) => s.toUpperCase();
export class Repo {
  scan(): void {}
}
export interface Node { id: string }
export type Id = string;
enum Color { Red }
"#;
        let got = names(src);
        for want in [
            ("greet", "function"),
            ("shout", "function"),
            ("Repo", "class"),
            ("scan", "method"),
            ("Node", "interface"),
            ("Id", "type"),
            ("Color", "enum"),
        ] {
            assert!(
                got.contains(&(want.0.to_string(), want.1.to_string())),
                "missing {want:?} in {got:?}"
            );
        }
    }

    #[test]
    fn a_plain_const_is_not_a_symbol() {
        // Only function-valued declarators are definitions; indexing every const
        // would bury real symbols under configuration and string literals.
        let got = names("const MAX = 10; const cfg = { a: 1 };");
        assert!(got.is_empty(), "expected no symbols, got {got:?}");
    }

    #[test]
    fn lines_are_one_based() {
        let e = extract(Lang::TypeScript, "\n\nfunction f() {}\n").unwrap();
        assert_eq!(e.symbols[0].start_line, 3, "tree-sitter rows are 0-based; storage is 1-based");
    }

    #[test]
    fn calls_are_credited_to_the_innermost_definition() {
        let src = r#"
class Service {
  run(): void { helper(); }
}
function helper(): void {}
"#;
        let e = extract(Lang::TypeScript, src).unwrap();
        let run = e.symbols.iter().position(|s| s.name == "run").unwrap();
        let call = e.calls.iter().find(|c| c.callee == "helper").expect("call not found");
        assert_eq!(
            call.from, Some(run),
            "the call belongs to run(), not to the enclosing class"
        );
    }

    #[test]
    fn method_calls_record_the_bare_property_name() {
        let e = extract(Lang::TypeScript, "function f() { repo.scan(); }").unwrap();
        assert!(e.calls.iter().any(|c| c.callee == "scan"), "{:?}", e.calls);
    }

    #[test]
    fn resolve_refuses_to_guess_between_duplicates() {
        let mut idx = HashMap::new();
        idx.insert("unique".to_string(), vec![7]);
        idx.insert("dup".to_string(), vec![1, 2]);
        assert_eq!(resolve(&idx, 99, "unique"), Some(7));
        assert_eq!(resolve(&idx, 99, "dup"), None, "an invented edge is worse than a missing one");
        assert_eq!(resolve(&idx, 99, "absent"), None);
        // …except recursion, where the caller is itself a candidate.
        assert_eq!(resolve(&idx, 2, "dup"), Some(2));
    }

    #[test]
    fn tsx_parses_as_its_own_dialect() {
        let e = extract(Lang::Tsx, "const App = () => <div>hi</div>;").unwrap();
        assert_eq!(e.symbols.len(), 1);
        assert_eq!(e.symbols[0].name, "App");
    }
}
