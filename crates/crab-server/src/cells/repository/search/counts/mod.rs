use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
enum Metric {
    Comments,
    Reactions,
    Interactions,
}

#[derive(Clone, Debug, Serialize)]
enum Comparison {
    Equal(i64),
    Greater(i64),
    AtLeast(i64),
    Less(i64),
    AtMost(i64),
    Between(i64, i64),
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Filter {
    metric: Metric,
    comparison: Comparison,
}

impl Filter {
    pub(super) fn parse(qualifier: &str, value: &str) -> Result<Self, &'static str> {
        let metric = match qualifier {
            "comments" => Metric::Comments,
            "reactions" => Metric::Reactions,
            "interactions" => Metric::Interactions,
            _ => return Err("Unsupported count qualifier"),
        };
        let comparison = if let Some((min, max)) = value.split_once("..") {
            match (min, max) {
                ("*", "*") => return Err("Count ranges need at least one numeric bound"),
                ("*", max) => Comparison::AtMost(number(max)?),
                (min, "*") => Comparison::AtLeast(number(min)?),
                (min, max) => {
                    let (min, max) = (number(min)?, number(max)?);
                    if min > max {
                        return Err("Count range lower bound must not exceed its upper bound");
                    }
                    Comparison::Between(min, max)
                }
            }
        } else if let Some(value) = value.strip_prefix(">=") {
            Comparison::AtLeast(number(value)?)
        } else if let Some(value) = value.strip_prefix("<=") {
            Comparison::AtMost(number(value)?)
        } else if let Some(value) = value.strip_prefix('>') {
            Comparison::Greater(number(value)?)
        } else if let Some(value) = value.strip_prefix('<') {
            Comparison::Less(number(value)?)
        } else {
            Comparison::Equal(number(value)?)
        };
        Ok(Self { metric, comparison })
    }

    pub(super) fn sql(&self, mut bind: impl FnMut(i64) -> String) -> String {
        let field = match self.metric {
            Metric::Comments => "d.comments",
            Metric::Reactions => "d.reactions",
            // Each count is bounded by MAX_NUMBER, so this sum fits SQLite's
            // signed integer exactly; converting to REAL would lose boundaries.
            Metric::Interactions => "(d.comments + d.reactions)",
        };
        let (operator, value) = match self.comparison {
            Comparison::Equal(value) => ("=", value),
            Comparison::Greater(value) => (">", value),
            Comparison::AtLeast(value) => (">=", value),
            Comparison::Less(value) => ("<", value),
            Comparison::AtMost(value) => ("<=", value),
            Comparison::Between(min, max) => {
                return format!("{field} BETWEEN {} AND {}", bind(min), bind(max));
            }
        };
        format!("{field} {operator} {}", bind(value))
    }
}

fn number(value: &str) -> Result<i64, &'static str> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Counts require a nonnegative integer, comparison, or inclusive range");
    }
    value
        .parse()
        .map_err(|_| "Count bound exceeds the maximum integer (9223372036854775807)")
}

#[cfg(test)]
mod tests;
