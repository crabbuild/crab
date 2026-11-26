use regex::bytes::{RegexSet, RegexSetBuilder};

const MAX_RULES: usize = 4_096;
const MAX_PATTERN_BYTES: usize = 1_024;
const MAX_OWNERS_PER_RULE: usize = 128;

pub(super) struct Rule {
    pub line: usize,
    pub owners: Vec<String>,
}

pub(super) struct Rules {
    pub rules: Vec<Rule>,
    pub invalid_lines: usize,
    matcher: RegexSet,
}

impl Rules {
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        let mut rules = Vec::new();
        let mut expressions = Vec::new();
        let mut invalid_lines = 0;
        for (index, line) in text.lines().enumerate() {
            let line = line.trim_start();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (pattern, rest) = line.split_at(pattern_end(line));
            let Some(expression) = pattern_expression(pattern) else {
                invalid_lines += 1;
                continue;
            };
            let mut owners: Vec<_> = rest
                .split_ascii_whitespace()
                .take_while(|token| !token.starts_with('#'))
                .take(MAX_OWNERS_PER_RULE + 1)
                .map(str::to_ascii_lowercase)
                .collect();
            if rules.len() == MAX_RULES || owners.len() > MAX_OWNERS_PER_RULE {
                return Err("CODEOWNERS exceeds the ownership rule limit.");
            }
            owners.sort_unstable();
            owners.dedup();
            rules.push(Rule {
                line: index + 1,
                owners,
            });
            expressions.push(expression);
        }
        let matcher = RegexSetBuilder::new(expressions)
            .unicode(false)
            .dot_matches_new_line(true)
            .size_limit(16 * 1024 * 1024)
            .build()
            .map_err(|_| "CODEOWNERS patterns exceed the matching budget.")?;
        Ok(Self {
            rules,
            invalid_lines,
            matcher,
        })
    }

    pub fn matching(&self, path: &[u8]) -> Option<usize> {
        self.matcher.matches(path).iter().next_back()
    }
}

fn pattern_end(line: &str) -> usize {
    let mut escaped = false;
    for (index, byte) in line.bytes().enumerate() {
        if !escaped && byte.is_ascii_whitespace() {
            return index;
        }
        escaped = !escaped && byte == b'\\';
    }
    line.len()
}

fn pattern_expression(pattern: &str) -> Option<String> {
    if pattern.len() > MAX_PATTERN_BYTES
        || pattern.starts_with(['!', '#'])
        || pattern.starts_with("\\#")
        || pattern.contains(['[', ']', '\0'])
    {
        return None;
    }
    let absolute = pattern.starts_with('/');
    let directory = pattern.ends_with('/');
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    let pattern = pattern.strip_suffix('/').unwrap_or(pattern);
    if pattern
        .split('/')
        .any(|part| matches!(part, "" | "." | ".."))
    {
        return None;
    }
    let mut expression = String::from("^");
    if !absolute && !pattern.contains('/') {
        expression.push_str("(?:.*/)?");
    }
    let mut parts = pattern.split('/').peekable();
    let mut last_literal = true;
    while let Some(part) = parts.next() {
        last_literal = true;
        if part == "**" {
            expression.push_str(if parts.peek().is_some() {
                "(?:[^/]+/)*"
            } else {
                ".*"
            });
            last_literal = false;
            continue;
        }
        let mut bytes = part.bytes().peekable();
        while let Some(byte) = bytes.next() {
            match byte {
                b'*' => {
                    while bytes.next_if_eq(&b'*').is_some() {}
                    expression.push_str("[^/]*");
                    last_literal = false;
                }
                b'?' => {
                    expression.push_str("[^/]");
                    last_literal = false;
                }
                b'\\' => literal(&mut expression, bytes.next()?),
                byte => literal(&mut expression, byte),
            }
        }
        if parts.peek().is_some() {
            expression.push('/');
        }
    }
    if directory {
        expression.push_str("/.+");
    } else if last_literal {
        expression.push_str("(?:/.+)?");
    }
    expression.push('$');
    Some(expression)
}

fn literal(expression: &mut String, byte: u8) {
    use std::fmt::Write as _;
    let _ = write!(expression, "\\x{byte:02x}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_patterns_follow_last_matching_case_sensitive_git_path_rule() {
        let rules = Rules::parse(
            "* @all\n*.js @javascript\n/docs/* @docs\napps/ @apps\n/apps/github\n**/logs @operations\n/vendor/**/third @dependencies\n/with\\ space @space\n/文 @text\n![a-b] @invalid\n",
        ).unwrap();
        for (path, line) in [
            ("docs/top.js", 3),
            ("docs/intermediate/top.js", 2),
            ("docs/top.JS", 3),
            ("Docs/top.JS", 1),
            ("packages/front/apps/main", 4),
            ("apps/github/workflow", 5),
            ("logs/old", 6),
            ("deeply/nested/logs/archive", 6),
            ("vendor/third", 7),
            ("vendor/deep/third", 7),
            ("with space", 8),
            ("文/article", 9),
        ] {
            let matching = rules.matching(path.as_bytes()).unwrap();
            assert_eq!(rules.rules[matching].line, line, "{path}");
        }
        assert_eq!(rules.invalid_lines, 1);
    }
}
