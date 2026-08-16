//! Workflow stage command contracts.

use serde::{Deserialize, Serialize};

/// Command executed by a stage, in either argv or shell form.
///
/// The variants hash differently on purpose: `Argv(["a", "b"])` bypasses the
/// shell while `Shell("a b")` runs through a shell adapter in the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cmd {
    Argv(Vec<String>),
    Shell(String),
    ShellList(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_serde_preserves_distinct_command_forms() {
        let commands = [
            Cmd::Argv(vec!["python".to_owned(), "train.py".to_owned()]),
            Cmd::Shell("python train.py".to_owned()),
            Cmd::ShellList(vec![
                "python prep.py".to_owned(),
                "python train.py".to_owned(),
            ]),
        ];

        for command in commands {
            let encoded = serde_json::to_string(&command).unwrap();
            let decoded: Cmd = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, command);
        }
    }
}
