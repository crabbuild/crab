use serde::{Deserialize, Serialize};

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct Header {
    name: String,
    about: String,
    description: String,
    pub title: String,
    labels: Names,
    assignees: Names,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Names {
    Line(String),
    List(Vec<String>),
}

impl Default for Names {
    fn default() -> Self {
        Self::List(vec![])
    }
}

impl Names {
    fn values(&self, limit: usize) -> Vec<&str> {
        let items: Vec<_> = match self {
            Self::Line(line) => line.split(',').collect(),
            Self::List(list) => list.iter().map(String::as_str).collect(),
        };
        items
            .into_iter()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .take(limit)
            .collect()
    }
}

#[derive(Deserialize)]
struct Form {
    #[serde(flatten)]
    header: Header,
    body: Vec<Field>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Field {
    Markdown {
        attributes: Markdown,
    },
    Input {
        attributes: Text,
        #[serde(default)]
        validations: Validations,
    },
    Textarea {
        attributes: Text,
        #[serde(default)]
        validations: Validations,
    },
    Dropdown {
        attributes: Dropdown,
        #[serde(default)]
        validations: Validations,
    },
    Checkboxes {
        attributes: Checkboxes,
        #[serde(default)]
        validations: Validations,
    },
}

#[derive(Deserialize, Serialize)]
pub(super) struct Markdown {
    value: String,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Prompt {
    label: String,
    description: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Text {
    #[serde(flatten)]
    prompt: Prompt,
    placeholder: Option<String>,
    value: Option<String>,
    render: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Dropdown {
    #[serde(flatten)]
    prompt: Prompt,
    options: Vec<String>,
    #[serde(default)]
    multiple: bool,
    default: Option<usize>,
}

#[derive(Deserialize, Serialize)]
pub(super) struct Checkboxes {
    #[serde(flatten)]
    prompt: Prompt,
    options: Vec<Check>,
}

#[derive(Deserialize, Serialize)]
struct Check {
    label: String,
    #[serde(default)]
    required: bool,
}

#[derive(Default, Deserialize, Serialize)]
pub(super) struct Validations {
    #[serde(default)]
    required: bool,
}

#[derive(Serialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub(super) enum Content {
    Markdown { body: String },
    Form { fields: Vec<Field> },
}

pub(super) struct Document {
    pub header: Header,
    pub content: Content,
}

impl Header {
    pub fn name(&self) -> &str {
        self.name.trim()
    }
    pub fn about(&self) -> &str {
        let text = if self.description.is_empty() {
            &self.about
        } else {
            &self.description
        };
        text.trim()
    }
    pub fn labels(&self) -> Vec<&str> {
        self.labels.values(20)
    }
    pub fn assignees(&self) -> Vec<&str> {
        self.assignees.values(10)
    }
}

impl Document {
    pub fn parse(name: &str, source: &str) -> Option<Self> {
        let source = source.trim_start_matches('\u{feff}');
        let doc = if name.ends_with(".md") {
            let (_, mut rest) = source
                .split_once('\n')
                .filter(|(line, _)| line.trim_end() == "---")?;
            let yaml = rest;
            loop {
                let (line, next) = rest.split_once('\n').unwrap_or((rest, ""));
                if line.trim_end() == "---" {
                    let header = serde_yaml::from_str(&yaml[..yaml.len() - rest.len()]).ok()?;
                    break Self {
                        header,
                        content: Content::Markdown {
                            body: next.trim_start_matches(['\r', '\n']).into(),
                        },
                    };
                }
                if next.is_empty() {
                    return None;
                }
                rest = next;
            }
        } else {
            let Form { header, body } = serde_yaml::from_str(source).ok()?;
            if body.is_empty()
                || body.len() > 30
                || !body.iter().all(Field::valid)
                || body
                    .iter()
                    .all(|field| matches!(field, Field::Markdown { .. }))
            {
                return None;
            }
            Self {
                header,
                content: Content::Form { fields: body },
            }
        };
        (line(doc.header.name(), 128)
            && doc.header.about().len() <= 1024
            && doc.header.title.chars().count() <= 256
            && !doc.header.title.chars().any(char::is_control))
        .then_some(doc)
    }
}

impl Field {
    fn valid(&self) -> bool {
        match self {
            Self::Markdown { .. } => true,
            Self::Input { attributes, .. } | Self::Textarea { attributes, .. } => {
                line(&attributes.prompt.label, 128)
                    && attributes.render.as_ref().is_none_or(|language| {
                        language.len() <= 40
                            && language
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b))
                    })
            }
            Self::Dropdown { attributes, .. } => {
                line(&attributes.prompt.label, 128)
                    && (1..=30).contains(&attributes.options.len())
                    && attributes.options.iter().all(|option| line(option, 256))
                    && attributes
                        .default
                        .is_none_or(|index| index < attributes.options.len())
            }
            Self::Checkboxes { attributes, .. } => {
                line(&attributes.prompt.label, 128)
                    && (1..=30).contains(&attributes.options.len())
                    && attributes
                        .options
                        .iter()
                        .all(|option| line(&option.label, 512))
            }
        }
    }
}

pub(super) fn line(value: &str, limit: usize) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= limit
        && !value.chars().any(char::is_control)
}
