//! Ranked lexical retrieval over the SQLite symbol corpus.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct AskHit {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub signature: String,
    pub score: f64,
    pub in_edges: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AskResult {
    pub query: String,
    pub mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub hits: Vec<AskHit>,
}

struct Document {
    id: i64,
    name: String,
    kind: String,
    path: String,
    start_line: i64,
    end_line: i64,
    signature: String,
    in_edges: i64,
    name_terms: HashMap<String, usize>,
    path_terms: HashMap<String, usize>,
    signature_terms: HashMap<String, usize>,
    body_terms: HashMap<String, usize>,
}

pub struct AskOptions<'a> {
    pub limit: usize,
    pub scope: Option<&'a str>,
    pub source: bool,
    pub full: bool,
}

pub fn ask(
    db: &Connection,
    repo_id: i64,
    root: &Path,
    query: &str,
    options: AskOptions<'_>,
) -> Result<AskResult> {
    if let Some((subject, outgoing)) = structural_subject(query) {
        let (seeds, reached) =
            crate::index::callers_scoped(db, repo_id, subject, outgoing, 1, options.scope)?;
        if !seeds.is_empty() && !reached.is_empty() {
            let hits = reached
                .into_iter()
                .take(options.limit.max(1))
                .map(|reached| AskHit {
                    id: reached.id,
                    name: reached.name,
                    kind: reached.kind,
                    path: reached.path.clone(),
                    start_line: reached.start_line,
                    end_line: reached.end_line,
                    signature: reached.signature,
                    score: 1.0 / reached.depth as f64,
                    in_edges: reached.in_edges,
                    source: options
                        .source
                        .then(|| {
                            source_excerpt(
                                root,
                                &reached.path,
                                reached.start_line,
                                reached.end_line,
                                options.full,
                            )
                        })
                        .flatten(),
                })
                .collect();
            return Ok(AskResult {
                query: query.to_string(),
                mode: if outgoing {
                    "structural-callees"
                } else {
                    "structural-callers"
                },
                scope: options.scope.map(str::to_string),
                note: None,
                hits,
            });
        }
    }
    let query_terms = terms(query);
    if query_terms.is_empty() {
        return Ok(AskResult {
            query: query.to_string(),
            mode: "empty",
            scope: options.scope.map(str::to_string),
            note: None,
            hits: Vec::new(),
        });
    }

    let mut statement = db.prepare(
        "select s.id, s.name, s.kind, f.path, s.start_line, s.end_line,
                coalesce(s.signature,''), coalesce(s.summary,''),
                (select count(*) from edges e where e.dst_symbol_id=s.id)
           from symbols s join files f on f.id=s.file_id
          where s.repo_id=?1 and s.kind != 'module'
          order by f.path, s.start_line, s.id",
    )?;
    let rows = statement.query_map([repo_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, i64>(8)?,
        ))
    })?;

    let mut documents = Vec::new();
    for row in rows {
        let (id, name, kind, path, start_line, end_line, signature, body, in_edges) = row?;
        if let Some(scope) = options.scope
            && !in_scope(&path, scope)
        {
            continue;
        }
        documents.push(Document {
            id,
            name_terms: frequencies(&terms(&name)),
            path_terms: frequencies(&terms(&path)),
            signature_terms: frequencies(&terms(&signature)),
            body_terms: frequencies(&terms(&body)),
            name,
            kind,
            path,
            start_line,
            end_line,
            signature,
            in_edges,
        });
    }

    let mut document_frequency: HashMap<&str, usize> = HashMap::new();
    for term in &query_terms {
        let count = documents
            .iter()
            .filter(|document| document_has(document, term))
            .count();
        document_frequency.insert(term, count);
    }
    let corpus_size = documents.len().max(1) as f64;
    let query_mentions_tests = query_terms
        .iter()
        .any(|term| matches!(term.as_str(), "test" | "tests" | "fixture" | "fixtures"));

    let mut scored = Vec::new();
    for document in documents {
        let mut score = 0.0;
        let mut strong = 0.0;
        let mut possible = 0.0;
        for term in &query_terms {
            let df = *document_frequency.get(term.as_str()).unwrap_or(&0) as f64;
            let idf = ((corpus_size + 1.0) / (df + 1.0)).ln() + 1.0;
            possible += idf;
            let name = frequency(&document.name_terms, term);
            let path = frequency(&document.path_terms, term);
            let signature = frequency(&document.signature_terms, term);
            let body = frequency(&document.body_terms, term);
            if name + path + signature > 0.0 {
                strong += idf;
            }
            score += idf * (name * 5.0 + path * 2.5 + signature * 2.0 + body.min(3.0));
        }
        if score == 0.0 {
            continue;
        }
        let strong_share = if possible == 0.0 {
            0.0
        } else {
            strong / possible
        };
        if strong_share == 0.0 && score < possible * 1.5 {
            continue;
        }
        if is_test_path(&document.path) && !query_mentions_tests {
            score *= 0.65;
        }
        score *= 1.0 + (document.in_edges as f64 + 1.0).ln() * 0.06;
        scored.push((score, document));
    }

    scored.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.start_line.cmp(&right.start_line))
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut hits = Vec::new();
    for (score, document) in scored.into_iter().take(options.limit.max(1)) {
        let source = if options.source {
            source_excerpt(
                root,
                &document.path,
                document.start_line,
                document.end_line,
                options.full,
            )
        } else {
            None
        };
        hits.push(AskHit {
            id: document.id,
            name: document.name,
            kind: document.kind,
            path: document.path,
            start_line: document.start_line,
            end_line: document.end_line,
            signature: document.signature,
            score,
            in_edges: document.in_edges,
            source,
        });
    }

    Ok(AskResult {
        query: query.to_string(),
        mode: "lexical",
        scope: options.scope.map(str::to_string),
        note: structural_subject(query).map(|(subject, _)| {
            format!("no resolved graph edges for {subject:?}; showing lexical matches")
        }),
        hits,
    })
}

fn structural_subject(query: &str) -> Option<(&str, bool)> {
    let query = query.trim().trim_end_matches(['?', '.']);
    let lower = query.to_ascii_lowercase();
    for prefix in ["who calls ", "what calls "] {
        if lower.starts_with(prefix) {
            return Some((query[prefix.len()..].trim(), false));
        }
    }
    for prefix in ["what does ", "what do "] {
        if lower.starts_with(prefix) && lower.ends_with(" call") {
            return Some((query[prefix.len()..query.len() - 5].trim(), true));
        }
    }
    None
}

fn in_scope(path: &str, scope: &str) -> bool {
    let scope = scope.trim_matches('/');
    path == scope || path.starts_with(&format!("{scope}/"))
}

fn is_test_path(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    path.split('/')
        .any(|part| matches!(part, "test" | "tests" | "__tests__"))
        || file.starts_with("test_")
        || file == "conftest.py"
        || file.contains(".test.")
        || file.contains("_test.")
}

fn source_excerpt(root: &Path, path: &str, start: i64, end: i64, full: bool) -> Option<String> {
    let text = std::fs::read_to_string(root.join(path)).ok()?;
    let start = start.max(1) as usize;
    let mut end = end.max(start as i64) as usize;
    if !full {
        end = end.min(start + 7);
    }
    Some(
        text.lines()
            .enumerate()
            .filter(|(index, _)| start <= index + 1 && *index < end)
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn document_has(document: &Document, term: &str) -> bool {
    document.name_terms.contains_key(term)
        || document.path_terms.contains_key(term)
        || document.signature_terms.contains_key(term)
        || document.body_terms.contains_key(term)
}

fn frequency(frequencies: &HashMap<String, usize>, term: &str) -> f64 {
    frequencies.get(term).copied().unwrap_or(0) as f64
}

fn frequencies(terms: &[String]) -> HashMap<String, usize> {
    let mut frequencies = HashMap::new();
    for term in terms {
        *frequencies.entry(term.clone()).or_insert(0) += 1;
    }
    frequencies
}

fn terms(text: &str) -> Vec<String> {
    let mut expanded = String::with_capacity(text.len() * 2);
    let mut previous_lower = false;
    for ch in text.chars() {
        if ch.is_uppercase() && previous_lower {
            expanded.push(' ');
        }
        if ch.is_alphanumeric() || ch == '_' {
            expanded.extend(ch.to_lowercase());
            previous_lower = ch.is_lowercase();
        } else {
            expanded.push(' ');
            previous_lower = false;
        }
    }
    let stopwords: HashSet<&'static str> = [
        "a", "an", "and", "are", "as", "at", "be", "by", "code", "does", "for", "from", "how",
        "in", "is", "it", "of", "on", "or", "that", "the", "this", "to", "what", "where", "which",
        "with",
    ]
    .into_iter()
    .collect();
    expanded
        .split(|ch: char| ch == '_' || ch.is_whitespace())
        .filter(|term| term.len() > 1 && !stopwords.contains(term))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenization_splits_camel_case_and_drops_junk() {
        assert_eq!(
            terms("How does serverEntry work?"),
            ["server", "entry", "work"]
        );
    }

    #[test]
    fn scope_is_segment_aware() {
        assert!(in_scope("src/graph/build.rs", "src/graph"));
        assert!(!in_scope("src/graphical/a.rs", "src/graph"));
    }

    #[test]
    fn structural_prompts_extract_the_subject_and_direction() {
        assert_eq!(
            structural_subject("Who calls Cache.get?"),
            Some(("Cache.get", false))
        );
        assert_eq!(
            structural_subject("what does Cache.get call?"),
            Some(("Cache.get", true))
        );
        assert_eq!(structural_subject("cache lookup"), None);
    }

    #[test]
    fn ranking_is_stable_de_ranks_tests_and_searches_module_bodies() {
        use crate::{db, index};
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "graft-ask-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(
            root.join("src/gateway.ts"),
            "const moduleMarker = 'only_at_module_scope';\nexport function processPayment() { return moduleMarker; }\nexport function main() { return processPayment(); }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tests/gateway.test.ts"),
            "export function processPaymentTest() { return 'payment'; }\n",
        )
        .unwrap();
        let store = root.join("store.db");
        let mut conn = db::open(&store).unwrap();
        index::build(&mut conn, &root).unwrap();
        let repo_id = index::repo_id_of(&conn, &root).unwrap().unwrap();
        let first = ask(
            &conn,
            repo_id,
            &root,
            "process payment",
            AskOptions {
                limit: 8,
                scope: None,
                source: false,
                full: false,
            },
        )
        .unwrap();
        assert_eq!(first.hits[0].path, "src/gateway.ts");
        let ids: Vec<_> = first.hits.iter().map(|hit| hit.id).collect();
        index::build(&mut conn, &root).unwrap();
        let second = ask(
            &conn,
            repo_id,
            &root,
            "process payment",
            AskOptions {
                limit: 8,
                scope: None,
                source: false,
                full: false,
            },
        )
        .unwrap();
        assert_eq!(
            ids,
            second.hits.iter().map(|hit| hit.id).collect::<Vec<_>>()
        );
        let module = ask(
            &conn,
            repo_id,
            &root,
            "only_at_module_scope",
            AskOptions {
                limit: 8,
                scope: None,
                source: false,
                full: false,
            },
        )
        .unwrap();
        assert!(module.hits.iter().any(|hit| hit.path == "src/gateway.ts"));
        let structural = ask(
            &conn,
            repo_id,
            &root,
            "who calls processPayment?",
            AskOptions {
                limit: 8,
                scope: None,
                source: false,
                full: false,
            },
        )
        .unwrap();
        assert_eq!(structural.mode, "structural-callers");
        assert!(structural.hits.iter().any(|hit| hit.name == "main"));
        drop(conn);
        let _ = std::fs::remove_dir_all(root);
    }
}
