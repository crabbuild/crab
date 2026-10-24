#[derive(Clone, Copy, Debug, serde::Serialize)]
pub(super) struct Fields(u8);
impl Fields {
    const TITLE: u8 = 1;
    const BODY: u8 = 2;
    const COMMENTS: u8 = 4;
    pub(super) const ALL: Self = Self(Self::TITLE | Self::BODY | Self::COMMENTS);

    pub(super) fn parse(value: &str) -> Result<Self, &'static str> {
        let mut selected = 0;
        for field in value.to_ascii_lowercase().split(',') {
            selected |= match field {
                "title" => Self::TITLE,
                "body" => Self::BODY,
                "comments" => Self::COMMENTS,
                _ => {
                    return Err(
                        "Search in qualifier accepts title, body, comments, or a comma-separated combination",
                    );
                }
            };
        }
        Ok(Self(selected))
    }
    pub(super) fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    fn header(self) -> Vec<&'static str> {
        [(Self::TITLE, "title"), (Self::BODY, "body")]
            .into_iter()
            .filter_map(|(flag, name)| (self.0 & flag != 0).then_some(name))
            .collect()
    }
    fn comments(self) -> bool {
        self.0 & Self::COMMENTS != 0
    }
    pub(super) fn source(self) -> String {
        let mut fields = self.header();
        if self.comments() {
            fields.push("comments");
        }
        fields.join(",")
    }
    fn header_query(self, text: &str) -> Option<String> {
        let fields = self.header();
        (!fields.is_empty()).then(|| format!("{{{}}} : {}", fields.join(" "), quote(text)))
    }
}

pub(super) fn predicate(
    text: &str,
    fields: Fields,
    mut bind: impl FnMut(&str) -> String,
) -> String {
    if text.chars().count() < 3 {
        // Trigram MATCH cannot represent one or two Unicode scalars. Keep
        // literal substring semantics, including non-ASCII comment bodies.
        let value = bind(text);
        let mut clauses = fields
            .header()
            .into_iter()
            .map(|field| format!("instr(d.{field},{value}) > 0"))
            .collect::<Vec<_>>();
        if fields.comments() {
            clauses.push(format!("EXISTS (SELECT 1 FROM discussion_index_comment_text c WHERE c.kind = d.kind AND c.discussion = d.number AND instr(c.body,{value}) > 0)"));
        }
        return format!("({})", clauses.join(" OR "));
    }
    let mut sources = Vec::new();
    if let Some(query) = fields.header_query(text) {
        let value = bind(&query);
        sources.push(format!(
            "SELECT rowid FROM discussion_fts WHERE discussion_fts MATCH {value}"
        ));
    }
    if fields.comments() {
        let value = bind(&quote(text));
        sources.push(format!("SELECT c.discussion * 2 + c.kind FROM discussion_comment_fts JOIN discussion_index_comment_text c ON c.id = discussion_comment_fts.rowid WHERE discussion_comment_fts MATCH {value}"));
    }
    format!("d.rowid IN ({})", sources.join(" UNION "))
}

pub(super) fn relevance(
    text: &str,
    fields: Fields,
    mut bind: impl FnMut(&str) -> String,
) -> String {
    if text.chars().count() < 3 {
        return "0.0".into();
    }
    let mut scores = Vec::new();
    if let Some(query) = fields.header_query(text) {
        let value = bind(&query);
        scores.push(format!("COALESCE((SELECT -bm25(discussion_fts) FROM discussion_fts WHERE discussion_fts MATCH {value} AND rowid = d.rowid),0.0)"));
    }
    if fields.comments() {
        let value = bind(&quote(text));
        // Use the best matching contribution so long threads cannot dominate
        // solely by repeating the same word across many comments.
        scores.push(format!("COALESCE((SELECT -discussion_comment_fts.rank FROM discussion_comment_fts JOIN discussion_index_comment_text c ON c.id = discussion_comment_fts.rowid WHERE discussion_comment_fts MATCH {value} AND c.kind = d.kind AND c.discussion = d.number ORDER BY discussion_comment_fts.rank LIMIT 1),0.0)"));
    }
    format!("({})", scores.join(" + "))
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

#[cfg(test)]
mod tests;
