//! `${...}` expression parser and resolver.
//!
//! Scans a string for `${expr}` patterns, resolves each expression
//! against a [`TemplateContext`], and returns the fully-resolved
//! string. Escaped sequences (`\${...}`) are passed through as
//! literal `${...}` without resolution.
//!
//! Implementation uses a simple state machine — no regex dependency.

use serde_yaml::Value;

use crate::{Result, WorkflowError};

use super::TemplateContext;

/// Resolve all `${...}` expressions in `input` against `ctx`.
///
/// Returns the input string with every `${expr}` replaced by the
/// resolved value from the context. Escaped sequences (`\${...}`)
/// become literal `${...}`.
///
/// Returns `WorkflowError::TemplateUndefined` if any expression
/// references a key not present in the context.
pub fn substitute(input: &str, ctx: &TemplateContext) -> Result<String> {
    substitute_with(input, |normalized| ctx.resolve(normalized))
}

/// Resolve all `${...}` expressions in a command string.
///
/// Scalars keep the normal substitution behavior. Mapping values are
/// unpacked into CLI flags, matching DVC's command-only dictionary
/// unpacking convention.
pub fn substitute_cmd(input: &str, ctx: &TemplateContext) -> Result<String> {
    substitute_with(input, |normalized| {
        let value = ctx.resolve_value(normalized)?;
        command_value_to_string(&value, normalized)
    })
}

fn substitute_with<F>(input: &str, mut resolver: F) -> Result<String>
where
    F: FnMut(&str) -> Result<String>,
{
    let mut output = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Check for escaped `\${`
        if i + 2 < len && bytes[i] == b'\\' && bytes[i + 1] == b'$' && bytes[i + 2] == b'{' {
            // Emit literal `${` and skip past the escape + opening.
            output.push('$');
            output.push('{');
            i += 3;
            // Copy everything up to and including the matching `}`.
            while i < len && bytes[i] != b'}' {
                output.push(bytes[i] as char);
                i += 1;
            }
            if i < len {
                // Push the closing `}`
                output.push('}');
                i += 1;
            }
            continue;
        }

        // Check for `${` expression start.
        if i + 1 < len && bytes[i] == b'$' && bytes[i + 1] == b'{' {
            i += 2; // skip `${`

            // Find the matching `}`.
            let start = i;
            while i < len && bytes[i] != b'}' {
                i += 1;
            }

            let expr = &input[start..i];

            // Skip past the closing `}`.
            if i < len {
                i += 1;
            }

            // Normalize array-index syntax: `key.list[0]` → `key.list.0`
            let normalized = normalize_expr(expr);
            let resolved = resolver(&normalized)?;
            output.push_str(&resolved);
            continue;
        }

        // Regular character — copy through.
        output.push(bytes[i] as char);
        i += 1;
    }

    Ok(output)
}

fn command_value_to_string(value: &Value, expr: &str) -> Result<String> {
    match value {
        Value::Mapping(map) => unpack_mapping(map, ""),
        Value::Sequence(_) => Err(WorkflowError::TemplateInvalid {
            key: format!(
                "command template expression '{expr}' resolves to a sequence; only mappings can be unpacked"
            ),
            origin: "template".into(),
        }),
        Value::Tagged(tagged) => command_value_to_string(&tagged.value, expr),
        other => scalar_to_plain_string(other, expr),
    }
}

fn unpack_mapping(map: &serde_yaml::Mapping, prefix: &str) -> Result<String> {
    let mut args = Vec::new();
    push_mapping_args(map, prefix, &mut args)?;
    Ok(args.join(" "))
}

fn push_mapping_args(
    map: &serde_yaml::Mapping,
    prefix: &str,
    args: &mut Vec<String>,
) -> Result<()> {
    for (key, value) in map {
        let key = key.as_str().ok_or_else(|| WorkflowError::TemplateInvalid {
            key: "command dictionary unpacking requires string keys".to_owned(),
            origin: "template".into(),
        })?;
        let dotted = if prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{prefix}.{key}")
        };

        push_mapping_arg_value(&dotted, value, args)?;
    }
    Ok(())
}

fn push_mapping_arg_value(dotted: &str, value: &Value, args: &mut Vec<String>) -> Result<()> {
    match value {
        Value::Mapping(child) => push_mapping_args(child, dotted, args),
        Value::Sequence(seq) => {
            let values = seq
                .iter()
                .map(sequence_item_to_shell_arg)
                .collect::<Result<Vec<_>>>()?;
            if values.is_empty() {
                args.push(format!("--{dotted}"));
            } else {
                args.push(format!("--{dotted} {}", values.join(" ")));
            }
            Ok(())
        }
        Value::Bool(true) => {
            args.push(format!("--{dotted}"));
            Ok(())
        }
        Value::Tagged(tagged) => push_mapping_arg_value(dotted, &tagged.value, args),
        other => {
            args.push(format!(
                "--{dotted} {}",
                scalar_to_shell_arg(other, dotted)?
            ));
            Ok(())
        }
    }
}

fn sequence_item_to_shell_arg(value: &Value) -> Result<String> {
    match value {
        Value::Mapping(_) | Value::Sequence(_) => Err(WorkflowError::TemplateInvalid {
            key: "command dictionary list values must be scalars".to_owned(),
            origin: "template".into(),
        }),
        Value::Tagged(tagged) => sequence_item_to_shell_arg(&tagged.value),
        other => scalar_to_shell_arg(other, "sequence"),
    }
}

fn scalar_to_plain_string(value: &Value, expr: &str) -> Result<String> {
    match value {
        Value::Null => Ok("null".to_owned()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => Ok(s.clone()),
        Value::Tagged(tagged) => scalar_to_plain_string(&tagged.value, expr),
        Value::Mapping(_) | Value::Sequence(_) => Err(WorkflowError::TemplateInvalid {
            key: format!(
                "template expression '{expr}' resolves to a complex value (mapping or sequence), not a scalar"
            ),
            origin: "template".into(),
        }),
    }
}

fn scalar_to_shell_arg(value: &Value, expr: &str) -> Result<String> {
    match value {
        Value::String(s) => Ok(shell_quote(s)),
        other => scalar_to_plain_string(other, expr),
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }

    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Normalize bracket-index syntax to dot-separated paths.
///
/// Converts `key.list[0]` to `key.list.0` so the context resolver
/// can handle it uniformly with dot-path traversal.
fn normalize_expr(expr: &str) -> String {
    if !expr.contains('[') {
        return expr.to_owned();
    }

    let mut result = String::with_capacity(expr.len());
    for ch in expr.chars() {
        match ch {
            '[' => result.push('.'),
            ']' => {} // drop closing bracket
            _ => result.push(ch),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use serde_yaml::Value;

    use super::*;

    fn make_ctx(vars_yaml: &str, params_yaml: &str) -> TemplateContext {
        let vars: Value = if vars_yaml.is_empty() {
            Value::Mapping(serde_yaml::Mapping::new())
        } else {
            serde_yaml::from_str(vars_yaml).unwrap()
        };
        let params: Value = if params_yaml.is_empty() {
            Value::Mapping(serde_yaml::Mapping::new())
        } else {
            serde_yaml::from_str(params_yaml).unwrap()
        };
        TemplateContext::new(vars, params, false)
    }

    #[test]
    fn no_expressions_passes_through() {
        let ctx = TemplateContext::empty();
        assert_eq!(substitute("hello world", &ctx).unwrap(), "hello world");
    }

    #[test]
    fn simple_substitution() {
        let ctx = make_ctx("codedir: src", "");
        assert_eq!(
            substitute("python ${codedir}/train.py", &ctx).unwrap(),
            "python src/train.py"
        );
    }

    #[test]
    fn nested_path_substitution() {
        let ctx = make_ctx("", "model:\n  lr: 0.001\n  epochs: 10");
        assert_eq!(
            substitute("--lr ${model.lr} --epochs ${model.epochs}", &ctx).unwrap(),
            "--lr 0.001 --epochs 10"
        );
    }

    #[test]
    fn multiple_expressions() {
        let ctx = make_ctx("src: code\ndata: datasets", "");
        assert_eq!(
            substitute("${src}/train.py ${data}/input.csv", &ctx).unwrap(),
            "code/train.py datasets/input.csv"
        );
    }

    #[test]
    fn escaped_expression_not_resolved() {
        let ctx = TemplateContext::empty();
        let result = substitute("echo \\${HOME}", &ctx).unwrap();
        assert_eq!(result, "echo ${HOME}");
    }

    #[test]
    fn array_index_syntax() {
        let ctx = make_ctx("", "widths:\n  - 64\n  - 128\n  - 256");
        assert_eq!(substitute("${widths[0]}", &ctx).unwrap(), "64");
        assert_eq!(substitute("${widths[2]}", &ctx).unwrap(), "256");
    }

    #[test]
    fn undefined_key_returns_error() {
        let ctx = TemplateContext::empty();
        let err = substitute("${nonexistent}", &ctx).unwrap_err();
        assert!(matches!(
            err,
            crate::WorkflowError::TemplateUndefined { .. }
        ));
    }

    #[test]
    fn mixed_literal_and_expressions() {
        let ctx = make_ctx("name: world", "");
        assert_eq!(substitute("hello ${name}!", &ctx).unwrap(), "hello world!");
    }

    #[test]
    fn empty_input() {
        let ctx = TemplateContext::empty();
        assert_eq!(substitute("", &ctx).unwrap(), "");
    }

    #[test]
    fn expression_at_start_and_end() {
        let ctx = make_ctx("a: alpha\nb: beta", "");
        assert_eq!(substitute("${a}${b}", &ctx).unwrap(), "alphabeta");
    }

    #[test]
    fn dollar_without_brace_passes_through() {
        let ctx = TemplateContext::empty();
        assert_eq!(substitute("cost is $5", &ctx).unwrap(), "cost is $5");
    }

    #[test]
    fn normalize_bracket_to_dot() {
        assert_eq!(normalize_expr("widths[0]"), "widths.0");
        assert_eq!(normalize_expr("a.b[1].c[2]"), "a.b.1.c.2");
        assert_eq!(normalize_expr("simple"), "simple");
    }

    #[test]
    fn command_substitution_unpacks_dictionary() {
        let ctx = make_ctx(
            "",
            "mydict:\n  foo: foo\n  bar: 1\n  bool: true\n  nested:\n    baz: bar\n  list: [2, 3, 'qux']\n",
        );

        let resolved = substitute_cmd("R train.r ${mydict}", &ctx).unwrap();

        assert_eq!(
            resolved,
            "R train.r --foo 'foo' --bar 1 --bool --nested.baz 'bar' --list 2 3 'qux'"
        );
    }

    #[test]
    fn regular_substitution_rejects_dictionary() {
        let ctx = make_ctx("", "mydict:\n  foo: foo\n");

        let err = substitute("${mydict}", &ctx).unwrap_err();

        assert!(matches!(err, WorkflowError::TemplateInvalid { .. }));
    }
}
