CREATE TABLE repository_projects (
 id TEXT PRIMARY KEY,
 archived INTEGER NOT NULL CHECK(archived IN (0,1)),
 is_template INTEGER NOT NULL CHECK(is_template IN (0,1)),
 data TEXT NOT NULL CHECK(json_valid(data))
) STRICT;
CREATE INDEX repository_projects_collection ON repository_projects(archived,is_template,id);
CREATE TABLE repository_project_items (
 project TEXT NOT NULL REFERENCES repository_projects(id),
 id TEXT NOT NULL,
 kind INTEGER CHECK(kind IN (0,1)),
 number INTEGER,
 status TEXT NOT NULL,
 data TEXT NOT NULL CHECK(json_valid(data)),
 PRIMARY KEY(project,id),
 UNIQUE(project,kind,number),
 CHECK((kind IS NULL AND number IS NULL) OR (kind IS NOT NULL AND number>0))
) STRICT;
CREATE INDEX repository_project_items_status ON repository_project_items(project,status,id);
CREATE INDEX repository_project_items_subject ON repository_project_items(kind,number,project);
CREATE TABLE repository_issue_parents (
 child INTEGER PRIMARY KEY REFERENCES repository_issues(number),
 parent INTEGER REFERENCES repository_issues(number),
 version INTEGER NOT NULL CHECK(version>0),
 CHECK(child!=parent)
) STRICT;
CREATE INDEX repository_issue_parents_children ON repository_issue_parents(parent,child);
