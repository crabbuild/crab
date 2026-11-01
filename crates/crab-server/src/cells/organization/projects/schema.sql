CREATE TABLE organization_projects (
    id BLOB PRIMARY KEY CHECK(length(id)=16),
    archived INTEGER NOT NULL CHECK(archived IN (0,1)),
    name_key TEXT NOT NULL,
    data TEXT NOT NULL CHECK(json_valid(data))
) STRICT, WITHOUT ROWID;
CREATE INDEX organization_projects_collection ON organization_projects(archived,id);
CREATE TABLE organization_project_repositories (
    project BLOB NOT NULL REFERENCES organization_projects(id),
    repository BLOB NOT NULL CHECK(length(repository)=16),
    PRIMARY KEY(project,repository)
) STRICT, WITHOUT ROWID;
CREATE TABLE organization_project_items (
    project BLOB NOT NULL REFERENCES organization_projects(id),
    id BLOB NOT NULL CHECK(length(id)=16),
    repository BLOB,
    kind INTEGER CHECK(kind IN (0,1)),
    number INTEGER,
    status TEXT NOT NULL,
    data TEXT NOT NULL CHECK(json_valid(data)),
    PRIMARY KEY(project,id),
    FOREIGN KEY(project,repository) REFERENCES organization_project_repositories(project,repository),
    UNIQUE(project,repository,kind,number),
    CHECK((repository IS NULL AND kind IS NULL AND number IS NULL) OR
          (repository IS NOT NULL AND kind IS NOT NULL AND number>0))
) STRICT, WITHOUT ROWID;
CREATE INDEX organization_project_item_status ON organization_project_items(project,status,id);
CREATE INDEX organization_project_item_repository ON organization_project_items(project,repository,id);
