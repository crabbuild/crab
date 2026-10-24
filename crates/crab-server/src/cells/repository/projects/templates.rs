use super::*;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Template {
    pub id: String,
    pub description: String,
    pub definition: Definition,
}
pub(crate) fn templates() -> Vec<Template> {
    [
        (
            "table",
            "Table",
            "Start with a flexible list of work.",
            Layout::Table,
            vec!["Todo", "In progress", "Done"],
        ),
        (
            "board",
            "Board",
            "Move work through a visual workflow.",
            Layout::Board,
            vec!["Todo", "In progress", "Done"],
        ),
        (
            "roadmap",
            "Roadmap",
            "Plan work over time with start and target dates.",
            Layout::Roadmap,
            vec!["Planned", "In progress", "Shipped"],
        ),
        (
            "team-planning",
            "Team planning",
            "Prioritize your backlog and plan the next cycle.",
            Layout::Table,
            vec!["Backlog", "Ready", "In progress", "Done"],
        ),
        (
            "kanban",
            "Kanban",
            "See what is ready, active, and complete.",
            Layout::Board,
            vec!["Backlog", "Todo", "In progress", "In review", "Done"],
        ),
        (
            "feature-release",
            "Feature release",
            "Coordinate implementation, review, and release.",
            Layout::Table,
            vec![
                "Planned",
                "Building",
                "In review",
                "Ready to release",
                "Released",
            ],
        ),
        (
            "bug-tracker",
            "Bug tracker",
            "Triage, prioritize, and resolve reported problems.",
            Layout::Board,
            vec!["Triage", "Confirmed", "In progress", "Fixed", "Won’t fix"],
        ),
        (
            "iterative-development",
            "Iterative development",
            "Plan upcoming cycles from a prioritized backlog.",
            Layout::Board,
            vec!["Backlog", "Ready", "In progress", "In review", "Done"],
        ),
        (
            "product-launch",
            "Product launch",
            "Bring engineering and launch work into one plan.",
            Layout::Table,
            vec!["Planned", "In progress", "Blocked", "Ready", "Launched"],
        ),
        (
            "retrospective",
            "Team retrospective",
            "Turn observations into concrete follow-up work.",
            Layout::Board,
            vec!["Went well", "Could improve", "Actions", "Done"],
        ),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (key, name, description, layout, statuses))| {
        let fields = match key {
            "team-planning" | "iterative-development" => vec![
                Field {
                    id: Uuid::from_u128(100),
                    name: "Cycle".into(),
                    definition: FieldKind::Text,
                },
                Field {
                    id: Uuid::from_u128(101),
                    name: "Estimate".into(),
                    definition: FieldKind::Number,
                },
            ],
            "bug-tracker" => vec![Field {
                id: Uuid::from_u128(102),
                name: "Severity".into(),
                definition: FieldKind::Select {
                    options: vec![
                        "Critical".into(),
                        "High".into(),
                        "Medium".into(),
                        "Low".into(),
                    ],
                },
            }],
            "product-launch" | "feature-release" => vec![Field {
                id: Uuid::from_u128(103),
                name: "Workstream".into(),
                definition: FieldKind::Text,
            }],
            _ => vec![],
        };
        let mut layouts = vec![(name, layout)];
        for (label, kind) in [
            ("Table", Layout::Table),
            ("Board", Layout::Board),
            ("Roadmap", Layout::Roadmap),
        ] {
            if layouts[0].1 != kind {
                layouts.push((label, kind));
            }
        }
        Template {
            id: key.into(),
            description: description.into(),
            definition: Definition {
                name: name.into(),
                description: description.into(),
                readme: String::new(),
                statuses: statuses.into_iter().map(str::to_owned).collect(),
                fields,
                views: layouts
                    .into_iter()
                    .enumerate()
                    .map(|(i, (name, layout))| View {
                        id: Uuid::from_u128(1 + index as u128 * 10 + i as u128),
                        name: name.into(),
                        layout,
                        query: String::new(),
                        status: None,
                        hide_closed: false,
                    })
                    .collect(),
                is_template: false,
            },
        }
    })
    .collect()
}
