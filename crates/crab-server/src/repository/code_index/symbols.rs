use std::{
    ops::{ControlFlow, Range},
    sync::OnceLock,
    time::{Duration, Instant},
};

use tokio_util::sync::CancellationToken;
use tree_sitter::{
    Language, ParseOptions, ParseState, Parser, Query, QueryCursor, QueryCursorOptions,
    QueryCursorState, StreamingIterator,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Syntax {
    C,
    Cpp,
    Go,
    Java,
    JavaScript,
    Python,
    Rust,
    TypeScript,
    Tsx,
}

impl Syntax {
    pub(crate) fn for_path(path: &[u8]) -> Option<Self> {
        let filename = path.rsplit(|byte| *byte == b'/').next()?;
        let dot = filename.iter().rposition(|byte| *byte == b'.')?;
        match filename[dot + 1..].to_ascii_lowercase().as_slice() {
            b"c" | b"h" => Some(Self::C),
            b"cc" | b"cp" | b"cpp" | b"cxx" | b"c++" | b"hh" | b"hpp" | b"hxx" | b"h++" => {
                Some(Self::Cpp)
            }
            b"go" => Some(Self::Go),
            b"java" => Some(Self::Java),
            b"js" | b"jsx" | b"mjs" | b"cjs" => Some(Self::JavaScript),
            b"py" | b"pyi" | b"pyw" => Some(Self::Python),
            b"rs" => Some(Self::Rust),
            b"ts" | b"mts" | b"cts" => Some(Self::TypeScript),
            b"tsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    fn grammar(self) -> Option<&'static Grammar> {
        static GRAMMARS: [OnceLock<Option<Grammar>>; 9] = [const { OnceLock::new() }; 9];
        GRAMMARS[self as usize]
            .get_or_init(|| self.compile())
            .as_ref()
    }

    fn compile(self) -> Option<Grammar> {
        let (language, source) = match self {
            Self::C => (tree_sitter_c::LANGUAGE, tree_sitter_c::TAGS_QUERY),
            Self::Cpp => (tree_sitter_cpp::LANGUAGE, tree_sitter_cpp::TAGS_QUERY),
            Self::Go => (tree_sitter_go::LANGUAGE, tree_sitter_go::TAGS_QUERY),
            Self::Java => (tree_sitter_java::LANGUAGE, tree_sitter_java::TAGS_QUERY),
            Self::JavaScript => (
                tree_sitter_javascript::LANGUAGE,
                tree_sitter_javascript::TAGS_QUERY,
            ),
            Self::Python => (tree_sitter_python::LANGUAGE, tree_sitter_python::TAGS_QUERY),
            Self::Rust => (tree_sitter_rust::LANGUAGE, tree_sitter_rust::TAGS_QUERY),
            Self::TypeScript => (
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                tree_sitter_typescript::TAGS_QUERY,
            ),
            Self::Tsx => (
                tree_sitter_typescript::LANGUAGE_TSX,
                tree_sitter_typescript::TAGS_QUERY,
            ),
        };
        let language = Language::from(language);
        // TypeScript's upstream tags add to the JavaScript tags. Use the same
        // source as the current-file Symbols worker for both TS and TSX.
        let combined;
        let source = if matches!(self, Self::TypeScript | Self::Tsx) {
            combined = format!("{}\n{source}", tree_sitter_javascript::TAGS_QUERY);
            &combined
        } else {
            source
        };
        let mut query = Query::new(&language, source).ok()?;
        for index in 0..query.pattern_count() {
            let pattern = source
                .get(query.start_byte_for_pattern(index)..query.end_byte_for_pattern(index))?;
            if !pattern.contains("@definition.") {
                query.disable_pattern(index);
            }
        }
        let name = query.capture_index_for_name("name")?;
        Some(Grammar {
            language,
            query,
            name,
        })
    }
}

struct Grammar {
    language: Language,
    query: Query,
    name: u32,
}

#[derive(Eq, PartialEq)]
pub(crate) struct Definition {
    pub(crate) line: usize,
    pub(crate) bytes: Range<usize>,
}

pub(crate) fn definitions(
    syntax: Syntax,
    source: &str,
    cancel: &CancellationToken,
) -> Option<Vec<Definition>> {
    if cancel.is_cancelled() {
        return None;
    }
    // Compiling the bundled grammar on first use is shared setup. Waiting for
    // it must not consume a file's parsing budget on a cold process.
    let grammar = syntax.grammar()?;
    // Parsing untrusted source is bounded separately from object reads.
    let deadline = Instant::now() + Duration::from_millis(150);
    let stopped = || cancel.is_cancelled() || Instant::now() >= deadline;
    let progress = || {
        if stopped() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    if stopped() {
        return None;
    }

    let mut parser = Parser::new();
    parser.set_language(&grammar.language).ok()?;
    let bytes = source.as_bytes();
    let mut check_parse = |_: &ParseState| progress();
    let tree = parser.parse_with_options(
        &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
        None,
        Some(ParseOptions::new().progress_callback(&mut check_parse)),
    )?;
    let mut cursor = QueryCursor::new();
    cursor.set_match_limit(1024);
    let captures = grammar.query.capture_names();
    let mut check_query = |_: &QueryCursorState| progress();
    let mut result = Vec::new();
    {
        let mut matches = cursor.matches_with_options(
            &grammar.query,
            tree.root_node(),
            bytes,
            QueryCursorOptions::new().progress_callback(&mut check_query),
        );
        while let Some(entry) = matches.next() {
            if stopped() {
                return None;
            }
            if !entry
                .captures()
                .iter()
                .any(|capture| captures[capture.index as usize].starts_with("definition."))
            {
                continue;
            }
            for capture in entry
                .captures()
                .iter()
                .filter(|capture| capture.index == grammar.name)
            {
                let node = capture.node;
                let range = node.byte_range();
                if node.is_missing()
                    || range.is_empty()
                    || node.start_position().row != node.end_position().row
                    || source.get(range.clone()).is_none()
                {
                    continue;
                }
                result.push(Definition {
                    line: node.start_position().row + 1,
                    bytes: range,
                });
            }
        }
    }
    if stopped() || cursor.did_exceed_match_limit() {
        return None;
    }
    result.sort_unstable_by_key(|name| (name.bytes.start, name.bytes.end));
    result.dedup();
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_grammars_return_definitions_without_value_uses() {
        let cancel = CancellationToken::new();
        for (syntax, source) in [
            (Syntax::C, "void target() {}\nvoid sample() { target(); }"),
            (Syntax::Cpp, "void target() {}\nvoid sample() { target(); }"),
            (
                Syntax::Go,
                "package main\nfunc target() {}\nfunc sample() { target() }",
            ),
            (
                Syntax::Java,
                "class App { void target() {} void sample() { target(); } }",
            ),
            (Syntax::JavaScript, "function target() {}\ntarget();"),
            (Syntax::Python, "def target():\n    pass\ntarget()"),
            (Syntax::Rust, "fn target() {}\nfn sample() { target(); }"),
            (Syntax::TypeScript, "function target(): void {}\ntarget();"),
            (
                Syntax::Tsx,
                "function target() { return <div /> }\ntarget();",
            ),
        ] {
            let symbols = definitions(syntax, source, &cancel)
                .unwrap_or_else(|| panic!("missing {syntax:?} grammar"));
            let found = symbols
                .iter()
                .filter(|entry| &source[entry.bytes.clone()] == "target")
                .count();
            assert_eq!(found, 1, "{syntax:?}");
        }
    }
}
