use regex_syntax::hir::{Class, Hir, HirKind};

use super::Mask;

impl Mask {
    // Every bit must be present in any ASCII match. Non-ASCII candidates
    // remain eligible because inline case flags can fold Unicode to ASCII.
    pub(crate) fn from_regex(hir: &Hir) -> Option<Self> {
        let required = requirements(hir);
        required
            .bits
            .iter()
            .any(|&byte| byte != 0)
            .then_some(required)
    }
}

fn requirements(hir: &Hir) -> Mask {
    let mut required = Mask::from_text("");
    let mut run = String::new();
    collect(hir, &mut run, &mut required);
    required.insert(&run);
    required
}

fn collect(hir: &Hir, run: &mut String, required: &mut Mask) {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => (),
        HirKind::Capture(capture) => collect(&capture.sub, run, required),
        HirKind::Concat(parts) => {
            for part in parts {
                collect(part, run, required);
            }
        }
        HirKind::Literal(literal) if literal.0.is_ascii() => {
            run.extend(literal.0.iter().map(|&byte| char::from(byte)));
        }
        HirKind::Class(class) => match folded_ascii(class) {
            Some(byte) => run.push(char::from(byte)),
            None => flush(run, required),
        },
        HirKind::Alternation(branches) => {
            flush(run, required);
            let mut masks = branches.iter().map(requirements);
            if let Some(mut shared) = masks.next() {
                // Each branch can demand different grams. Only their shared
                // bits are mandatory regardless of which branch matches.
                for mask in masks {
                    for (common, other) in shared.bits.iter_mut().zip(mask.bits) {
                        *common &= other;
                    }
                }
                include(required, shared);
            }
        }
        HirKind::Repetition(repetition) if repetition.min > 0 => {
            flush(run, required);
            include(required, requirements(&repetition.sub));
        }
        HirKind::Literal(_) | HirKind::Repetition(_) => flush(run, required),
    }
}

fn folded_ascii(class: &Class) -> Option<u8> {
    match class {
        Class::Unicode(ranges) => folded_ranges(
            ranges
                .iter()
                .map(|range| (range.start() as u32, range.end() as u32)),
        ),
        Class::Bytes(ranges) => folded_ranges(
            ranges
                .iter()
                .map(|range| (u32::from(range.start()), u32::from(range.end()))),
        ),
    }
}

fn folded_ranges(ranges: impl Iterator<Item = (u32, u32)>) -> Option<u8> {
    let mut folded = None;
    for (start, end) in ranges.take_while(|(start, _)| *start <= 127) {
        if start != end.min(127) {
            return None;
        }
        let byte = (start as u8).to_ascii_lowercase();
        if folded.is_some_and(|prior| prior != byte) {
            return None;
        }
        folded = Some(byte);
    }
    folded
}

fn flush(run: &mut String, required: &mut Mask) {
    required.insert(run);
    run.clear();
}

fn include(required: &mut Mask, additional: Mask) {
    for (bit, extra) in required.bits.iter_mut().zip(additional.bits) {
        *bit |= extra;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::RegexBuilder;
    use regex_syntax::ParserBuilder;

    #[test]
    fn required_trigrams_keep_matches_through_classes_alternation_and_repetition() {
        for (pattern, matches, absent) in [
            (
                r"(?:let|const)\s+SearchTarget\s*=",
                "const sEARCHtARGET =",
                "const search_value",
            ),
            (
                r"^stable[0-9]+(?:needle|needlepoint)$",
                "stable32needlepoint",
                "stable32point",
            ),
            (r"(?:3needle)+[-_]?(?:again)?", "3NEEDLE-", "3need_"),
            (r"^word\b", "WORD", "odd"),
            (r"(?-u:[A-Z])(?i:connection)", "XCoNnECTIon", "Rconnect"),
        ] {
            let parsed = ParserBuilder::new()
                .case_insensitive(true)
                .build()
                .parse(pattern)
                .unwrap();
            let required = Mask::from_regex(&parsed).unwrap();
            assert!(
                RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .build()
                    .unwrap()
                    .is_match(matches)
            );
            assert!(
                Mask::from_text(matches).may_contain(&required, false),
                "{pattern}"
            );
            assert!(
                !Mask::from_text(absent).may_contain(&required, false),
                "{pattern}"
            );
        }
    }

    #[test]
    fn optional_and_unbounded_matches_cannot_require_grams() {
        for pattern in [r"(?:needle)?", r"a|needle", r"\b\w+\b", r"(?i:needle)*"] {
            let parsed = regex_syntax::parse(pattern).unwrap();
            assert!(Mask::from_regex(&parsed).is_none(), "{pattern}");
        }
    }
}
