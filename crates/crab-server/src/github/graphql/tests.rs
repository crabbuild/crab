use std::sync::LazyLock;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::{FieldError, FieldRequest, Object, Request, Resolved, Schema, execute};

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::parse(
        r#"
        type Query {
          viewer: User!
          repository(owner: String!, name: String!): Repository
          node(id: ID!): Node
          search(query: String!): [SearchResultItem!]!
        }
        type Mutation { closeIssue(number: Int!): Issue }
        interface Node { id: ID! }
        type User implements Node { id: ID! login: String! }
        type Repository implements Node {
          id: ID!
          name: String!
          issues(first: Int, states: [IssueState!]): IssueConnection!
          missing: String
          required: String!
        }
        type IssueConnection { totalCount: Int! nodes: [Issue] }
        type Issue implements Node { id: ID! number: Int! state: IssueState! }
        union SearchResultItem = Issue | User
        enum IssueState { OPEN CLOSED }
        "#,
    )
    .unwrap()
});

struct Context;

struct Root;
struct User;
struct Repository;
struct Connection(Vec<i64>);
struct Issue(i64);

#[async_trait]
impl Object<Context> for Root {
    fn typename(&self) -> &str {
        "Query"
    }

    async fn resolve(
        &self,
        _: &Context,
        field: &FieldRequest<'_>,
    ) -> Result<Resolved<Context>, FieldError> {
        Ok(match field.name {
            "viewer" => Resolved::object(User),
            "repository" if field.string("name") == Some("missing") => {
                return Err(FieldError::not_found(
                    "Could not resolve to a Repository with the name 'team/missing'.",
                ));
            }
            "repository" => Resolved::object(Repository),
            "node" => Resolved::object(Issue(7)),
            "search" => Resolved::List(vec![Resolved::object(Issue(1)), Resolved::object(User)]),
            _ => Resolved::null(),
        })
    }
}

struct Mutation;

#[async_trait]
impl Object<Context> for Mutation {
    fn typename(&self) -> &str {
        "Mutation"
    }

    async fn resolve(
        &self,
        _: &Context,
        field: &FieldRequest<'_>,
    ) -> Result<Resolved<Context>, FieldError> {
        Ok(Resolved::object(Issue(field.int("number").unwrap_or(0))))
    }
}

#[async_trait]
impl Object<Context> for User {
    fn typename(&self) -> &str {
        "User"
    }

    async fn resolve(
        &self,
        _: &Context,
        field: &FieldRequest<'_>,
    ) -> Result<Resolved<Context>, FieldError> {
        Ok(Resolved::leaf(match field.name {
            "id" => "U_1",
            _ => "alice",
        }))
    }
}

#[async_trait]
impl Object<Context> for Repository {
    fn typename(&self) -> &str {
        "Repository"
    }

    async fn resolve(
        &self,
        _: &Context,
        field: &FieldRequest<'_>,
    ) -> Result<Resolved<Context>, FieldError> {
        match field.name {
            "id" => Ok(Resolved::leaf("R_1")),
            "name" => Ok(Resolved::leaf("private")),
            "issues" => {
                let open = field.argument("states").is_none_or(|states| {
                    states
                        .as_array()
                        .is_some_and(|states| states.contains(&json!("OPEN")))
                });
                let first = field.int("first").unwrap_or(10) as usize;
                let numbers: Vec<i64> = if open { vec![3, 2, 1] } else { vec![] };
                Ok(Resolved::object(Connection(
                    numbers.into_iter().take(first).collect(),
                )))
            }
            "missing" => Err(FieldError::forbidden(
                "Resource not accessible by integration",
            )),
            _ => Ok(Resolved::null()),
        }
    }
}

#[async_trait]
impl Object<Context> for Connection {
    fn typename(&self) -> &str {
        "IssueConnection"
    }

    async fn resolve(
        &self,
        _: &Context,
        field: &FieldRequest<'_>,
    ) -> Result<Resolved<Context>, FieldError> {
        Ok(match field.name {
            "totalCount" => Resolved::leaf(self.0.len()),
            _ => Resolved::List(
                self.0
                    .iter()
                    .map(|number| Resolved::object(Issue(*number)))
                    .collect(),
            ),
        })
    }
}

#[async_trait]
impl Object<Context> for Issue {
    fn typename(&self) -> &str {
        "Issue"
    }

    async fn resolve(
        &self,
        _: &Context,
        field: &FieldRequest<'_>,
    ) -> Result<Resolved<Context>, FieldError> {
        Ok(match field.name {
            "id" => Resolved::leaf(format!("I_{}", self.0)),
            "number" => Resolved::leaf(self.0),
            _ => Resolved::leaf("OPEN"),
        })
    }
}

async fn run(query: &str, variables: Value) -> Value {
    let Value::Object(variables) = variables else {
        panic!("variables must be an object");
    };
    run_named(query, None, variables).await
}

async fn run_named(query: &str, operation: Option<&str>, variables: Map<String, Value>) -> Value {
    execute(
        &SCHEMA,
        &Context,
        &Root,
        &Mutation,
        &Request {
            query: query.into(),
            operation_name: operation.map(str::to_owned),
            variables,
        },
    )
    .await
}

#[tokio::test]
async fn executes_gh_style_fragments_aliases_variables_and_unions() {
    let response = run(
        r#"
        fragment issue on Issue { number state }
        query IssueList($owner: String!, $repo: String!, $limit: Int, $states: [IssueState!] = OPEN) {
          viewer { login }
          repository(owner: $owner, name: $repo) {
            name
            issues(first: $limit, states: $states) { totalCount nodes { ...issue id } }
          }
          search(query: "is:open") { __typename ...on Issue { number } ...on User { login } }
        }
        "#,
        json!({"owner": "team", "repo": "private", "limit": 2}),
    )
    .await;
    assert_eq!(
        response,
        json!({"data": {
            "viewer": {"login": "alice"},
            "repository": {"name": "private", "issues": {"totalCount": 2, "nodes": [
                {"number": 3, "state": "OPEN", "id": "I_3"},
                {"number": 2, "state": "OPEN", "id": "I_2"},
            ]}},
            "search": [{"__typename": "Issue", "number": 1}, {"__typename": "User", "login": "alice"}],
        }})
    );
}

#[tokio::test]
async fn unknown_fields_fail_validation_with_github_messages() {
    let response = run(
        "query IssueProjectItems { node(id: \"I_1\") { ...on Issue { projectItems(first: 10) { totalCount } } } }",
        json!({}),
    )
    .await;
    assert_eq!(
        response["errors"][0]["message"],
        "Field 'projectItems' doesn't exist on type 'Issue'"
    );
    assert_eq!(
        response["errors"][0]["extensions"]["code"],
        "undefinedField"
    );
    assert!(response.get("data").is_none());
    for (query, message) in [
        (
            "{ repository(owner: \"team\") { name } }",
            "Field 'repository' is missing required arguments: name",
        ),
        (
            "{ viewer(first: 1) { login } }",
            "Field 'viewer' doesn't accept argument 'first'",
        ),
        (
            "{ viewer }",
            "Field must have selections (field 'viewer' returns User but has no selections. Did you mean 'viewer { ... }'?)",
        ),
        (
            "query Q { repository(owner: $owner, name: \"x\") { name } }",
            "Variable $owner is used by Q but not declared",
        ),
        (
            "{ viewer { ...missing } }",
            "Fragment missing was used, but not defined",
        ),
    ] {
        let response = run(query, json!({})).await;
        assert_eq!(response["errors"][0]["message"], message, "{query}");
    }
}

#[tokio::test]
async fn introspection_reports_fields_and_enum_values_for_feature_detection() {
    let response = run(
        r#"query PullRequest_fields {
          Repository: __type(name: "Repository") { kind fields(includeDeprecated: true) { name args { name } } }
          IssueState: __type(name: "IssueState") { enumValues(includeDeprecated: true) { name } }
          Missing: __type(name: "Missing") { name }
          __schema { queryType { name } mutationType { name } }
        }"#,
        json!({}),
    )
    .await;
    let data = &response["data"];
    assert_eq!(data["Repository"]["kind"], "OBJECT");
    assert_eq!(
        data["Repository"]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|field| field["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["id", "name", "issues", "missing", "required"]
    );
    assert_eq!(
        data["Repository"]["fields"][2]["args"],
        json!([{"name": "first"}, {"name": "states"}])
    );
    assert_eq!(
        data["IssueState"]["enumValues"],
        json!([{"name": "OPEN"}, {"name": "CLOSED"}])
    );
    assert_eq!(data["Missing"], Value::Null);
    assert_eq!(
        data["__schema"],
        json!({"queryType": {"name": "Query"}, "mutationType": {"name": "Mutation"}})
    );
}

#[tokio::test]
async fn resolver_errors_keep_types_paths_and_propagate_non_null_failures() {
    let response = run(
        r#"{
          repo_000: repository(owner: "team", name: "missing") { name }
          repo_001: repository(owner: "team", name: "private") { name missing }
          repo_002: repository(owner: "team", name: "private") { name required }
        }"#,
        json!({}),
    )
    .await;
    assert_eq!(
        response["data"],
        json!({"repo_000": null, "repo_001": {"name": "private", "missing": null}, "repo_002": null})
    );
    let errors = response["errors"].as_array().unwrap();
    assert_eq!(
        (
            &errors[0]["type"],
            &errors[0]["path"],
            &errors[1]["type"],
            &errors[1]["path"],
            &errors[2]["path"],
        ),
        (
            &json!("NOT_FOUND"),
            &json!(["repo_000"]),
            &json!("FORBIDDEN"),
            &json!(["repo_001", "missing"]),
            &json!(["repo_002", "required"]),
        )
    );
    assert_eq!(
        errors[2]["message"],
        "Cannot return null for non-nullable field required"
    );
}

#[tokio::test]
async fn directives_variables_and_operation_selection_follow_the_specification() {
    let response = run(
        "query($skip: Boolean!) { viewer { login @skip(if: $skip) id @include(if: false) } }",
        json!({"skip": true}),
    )
    .await;
    assert_eq!(response, json!({"data": {"viewer": {}}}));
    let missing = run("query($owner: String!) { viewer { login } }", json!({})).await;
    assert_eq!(
        missing["errors"][0]["message"],
        "Variable $owner of type String! was provided invalid value"
    );
    let document = "query A { viewer { login } } mutation B { closeIssue(number: 4) { number } }";
    assert_eq!(
        run(document, json!({})).await["errors"][0]["message"],
        "An operation name is required"
    );
    assert_eq!(
        run_named(document, Some("B"), Map::new()).await,
        json!({"data": {"closeIssue": {"number": 4}}})
    );
}

#[tokio::test]
async fn lookahead_sees_fields_through_fragments() {
    struct Probe;

    #[async_trait]
    impl Object<Context> for Probe {
        fn typename(&self) -> &str {
            "Query"
        }

        async fn resolve(
            &self,
            _: &Context,
            field: &FieldRequest<'_>,
        ) -> Result<Resolved<Context>, FieldError> {
            assert!(field.selects(&["issues", "totalCount"]));
            assert!(!field.selects(&["issues", "nodes"]));
            Ok(Resolved::object(Repository))
        }
    }

    let response = execute(
        &SCHEMA,
        &Context,
        &Probe,
        &Mutation,
        &Request {
            query: "fragment c on IssueConnection { totalCount } { repository(owner: \"a\", name: \"b\") { issues { ...c } } }".into(),
            operation_name: None,
            variables: Map::new(),
        },
    )
    .await;
    assert_eq!(
        response,
        json!({"data": {"repository": {"issues": {"totalCount": 3}}}})
    );
}
