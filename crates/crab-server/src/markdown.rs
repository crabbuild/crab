use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

pub(crate) fn prose_ranges(body: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut ignored = 0usize;
    Parser::new_ext(body, Options::ENABLE_GFM)
        .into_offset_iter()
        .filter_map(move |(event, range)| {
            match event {
                Event::Start(
                    Tag::CodeBlock(_) | Tag::BlockQuote(_) | Tag::Link { .. } | Tag::Image { .. },
                ) => ignored += 1,
                Event::End(
                    TagEnd::CodeBlock | TagEnd::BlockQuote(_) | TagEnd::Link | TagEnd::Image,
                ) => ignored = ignored.saturating_sub(1),
                Event::Text(_) if ignored == 0 => return Some(range),
                _ => {}
            }
            None
        })
}
