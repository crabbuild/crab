//! Notebook output stripping for staging-only transforms.
//!
//! Implements the `notebook_strip_outputs` RPC verb. When the
//! workspace setting `strip_notebook_outputs` is enabled, the commit
//! composer calls this verb before committing staged `.ipynb` files.
//!
//! The verb reads the notebook from the working tree, strips all
//! `outputs` arrays and `execution_count` fields from code cells,
//! writes the stripped content to a temp file, then stages the
//! stripped version into the git index via `git hash-object -w` +
//! `git update-index --cacheinfo`. The original file on disk is
//! untouched — this is a staging-only transform.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::debug;

use crate::{AgentError, Result};

/// Parameters for the `notebook_strip_outputs` verb.
#[derive(Debug, Deserialize)]
pub struct NotebookStripParams {
    pub repo_id: String,
    /// Path to the `.ipynb` file, relative to the repo root.
    pub path: String,
}

/// Strip outputs from a notebook and stage the stripped version.
///
/// Returns `{ ok: true, stripped_cells: N }` where N is the number
/// of code cells that had non-empty outputs removed.
pub async fn notebook_strip_outputs(work_dir: &Path, p: &NotebookStripParams) -> Result<Value> {
    let file_path = work_dir.join(&p.path);

    // Read the notebook file from the working tree.
    let contents = tokio::fs::read_to_string(&file_path)
        .await
        .map_err(|e| AgentError::Other(format!("cannot read notebook {}: {e}", p.path)))?;

    // Parse as JSON.
    let mut nb: Value = serde_json::from_str(&contents)
        .map_err(|e| AgentError::Other(format!("notebook JSON parse error for {}: {e}", p.path)))?;

    // Strip outputs from code cells.
    let stripped_count = strip_code_cell_outputs(&mut nb)?;

    // Serialize back with 1-space indent to preserve reasonable formatting.
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut buf = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut buf, formatter);
    nb.serialize(&mut serializer)
        .map_err(|e| AgentError::Other(format!("failed to serialize stripped notebook: {e}")))?;
    // Ensure trailing newline (standard for text files).
    buf.push(b'\n');

    // Write to a temp file.
    let temp_dir = std::env::temp_dir();
    let temp_name = format!("crab-nb-strip-{}.ipynb", std::process::id());
    let temp_path = temp_dir.join(&temp_name);

    tokio::fs::write(&temp_path, &buf)
        .await
        .map_err(|e| AgentError::Other(format!("failed to write temp file: {e}")))?;

    // Stage the stripped content: hash-object -w to get the blob SHA.
    let blob_sha = git_hash_object(work_dir, &temp_path).await?;

    // Update the index to point at the stripped blob for this path.
    git_update_index(work_dir, &blob_sha, &p.path).await?;

    // Clean up the temp file (best-effort).
    let _ = tokio::fs::remove_file(&temp_path).await;

    debug!(
        path = %p.path,
        stripped_cells = stripped_count,
        blob_sha = %blob_sha,
        "staged stripped notebook"
    );

    Ok(json!({
        "ok": true,
        "stripped_cells": stripped_count,
    }))
}

/// Walk the cells array and strip `outputs` and `execution_count`
/// from code cells. Returns the number of cells that had non-empty
/// outputs.
fn strip_code_cell_outputs(nb: &mut Value) -> Result<usize> {
    let cells = nb
        .get_mut("cells")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| AgentError::Other("notebook has no 'cells' array".into()))?;

    let mut stripped_count = 0usize;

    for cell in cells.iter_mut() {
        let cell_type = cell.get("cell_type").and_then(Value::as_str).unwrap_or("");

        if cell_type != "code" {
            continue;
        }

        // Check if this cell has non-empty outputs before stripping.
        let had_outputs = cell
            .get("outputs")
            .and_then(Value::as_array)
            .is_some_and(|o| !o.is_empty());

        if had_outputs {
            stripped_count += 1;
        }

        // Strip outputs (set to empty array).
        if let Some(obj) = cell.as_object_mut() {
            obj.insert("outputs".to_owned(), json!([]));
            obj.insert("execution_count".to_owned(), Value::Null);
        }
    }

    Ok(stripped_count)
}

/// Run `git hash-object -w <path>` to write the file as a blob and
/// return its SHA.
async fn git_hash_object(work_dir: &Path, file_path: &Path) -> Result<String> {
    let output = crate::shellout::git_command()
        .args(["hash-object", "-w"])
        .arg(file_path)
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: stderr.into_owned(),
        });
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if sha.is_empty() {
        return Err(AgentError::Other(
            "git hash-object returned empty SHA".into(),
        ));
    }

    Ok(sha)
}

/// Run `git update-index --cacheinfo 100644,<sha>,<path>` to stage
/// the blob at the given path in the index.
async fn git_update_index(work_dir: &Path, sha: &str, path: &str) -> Result<()> {
    let cacheinfo = format!("100644,{sha},{path}");
    let output = crate::shellout::git_command()
        .args(["update-index", "--cacheinfo", &cacheinfo])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: stderr.into_owned(),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn sh(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command failed")
    }

    fn git_init_repo() -> (TempDir, std::path::PathBuf) {
        let td = tempfile::tempdir().expect("tempdir");
        let root = td.path().to_path_buf();
        sh(&root, &["init", "--initial-branch=main"]);
        sh(&root, &["config", "user.email", "test@test.com"]);
        sh(&root, &["config", "user.name", "Test"]);
        // Initial commit so the index exists.
        std::fs::write(root.join("README.md"), "# test\n").expect("write");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "init"]);
        (td, root)
    }

    /// Sample notebook with outputs and execution counts.
    fn sample_notebook() -> &'static str {
        r##"{
 "cells": [
  {
   "cell_type": "code",
   "execution_count": 1,
   "metadata": {},
   "outputs": [
    {
     "output_type": "stream",
     "name": "stdout",
     "text": ["hello\n"]
    }
   ],
   "source": ["print('hello')"]
  },
  {
   "cell_type": "markdown",
   "metadata": {},
   "source": ["# Title"]
  },
  {
   "cell_type": "code",
   "execution_count": null,
   "metadata": {},
   "outputs": [],
   "source": ["x = 1"]
  },
  {
   "cell_type": "code",
   "execution_count": 3,
   "metadata": {},
   "outputs": [
    {
     "output_type": "execute_result",
     "data": {"text/plain": ["42"]},
     "metadata": {},
     "execution_count": 3
    }
   ],
   "source": ["40 + 2"]
  }
 ],
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3",
   "language": "python",
   "name": "python3"
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5
}"##
    }

    #[test]
    fn strip_code_cell_outputs_counts_correctly() {
        let mut nb: Value = serde_json::from_str(sample_notebook()).unwrap();
        let count = strip_code_cell_outputs(&mut nb).unwrap();
        // Two code cells had non-empty outputs (cell 0 and cell 3).
        assert_eq!(count, 2);
    }

    #[test]
    fn strip_code_cell_outputs_clears_outputs_and_execution_count() {
        let mut nb: Value = serde_json::from_str(sample_notebook()).unwrap();
        strip_code_cell_outputs(&mut nb).unwrap();

        let cells = nb["cells"].as_array().unwrap();
        for cell in cells {
            let cell_type = cell["cell_type"].as_str().unwrap();
            if cell_type == "code" {
                assert_eq!(cell["outputs"], json!([]));
                assert_eq!(cell["execution_count"], Value::Null);
            }
        }
    }

    #[test]
    fn strip_preserves_markdown_cells() {
        let mut nb: Value = serde_json::from_str(sample_notebook()).unwrap();
        strip_code_cell_outputs(&mut nb).unwrap();

        let cells = nb["cells"].as_array().unwrap();
        let md_cell = &cells[1];
        assert_eq!(md_cell["cell_type"], "markdown");
        assert_eq!(md_cell["source"], json!(["# Title"]));
    }

    #[tokio::test]
    async fn notebook_strip_outputs_stages_stripped_version() {
        let (_td, root) = git_init_repo();

        // Write a notebook with outputs.
        let nb_path = root.join("analysis.ipynb");
        std::fs::write(&nb_path, sample_notebook()).expect("write notebook");

        // Stage the original first.
        sh(&root, &["add", "analysis.ipynb"]);

        // Now strip and re-stage.
        let params = NotebookStripParams {
            repo_id: "test".to_owned(),
            path: "analysis.ipynb".to_owned(),
        };
        let result = notebook_strip_outputs(&root, &params).await.unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["stripped_cells"], 2);

        // Verify the staged content has no outputs by reading from the index.
        let show_output = Command::new("git")
            .args(["show", ":analysis.ipynb"])
            .current_dir(&root)
            .output()
            .expect("git show");
        let staged_content = String::from_utf8_lossy(&show_output.stdout);
        let staged_nb: Value = serde_json::from_str(&staged_content).unwrap();

        let cells = staged_nb["cells"].as_array().unwrap();
        for cell in cells {
            if cell["cell_type"].as_str() == Some("code") {
                assert_eq!(
                    cell["outputs"],
                    json!([]),
                    "staged code cell should have empty outputs"
                );
                assert_eq!(cell["execution_count"], Value::Null);
            }
        }

        // Verify the working tree file is untouched.
        let disk_content = std::fs::read_to_string(&nb_path).expect("read disk");
        let disk_nb: Value = serde_json::from_str(&disk_content).unwrap();
        let first_cell = &disk_nb["cells"][0];
        assert!(
            !first_cell["outputs"].as_array().unwrap().is_empty(),
            "working tree file should still have outputs"
        );
    }
}
