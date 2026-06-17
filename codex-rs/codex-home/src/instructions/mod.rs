use std::io;

use codex_extension_api::LoadUserInstructionsFuture;
use codex_extension_api::LoadedUserInstructions;
use codex_extension_api::UserInstructions;
use codex_extension_api::UserInstructionsProvider;
use codex_utils_absolute_path::AbsolutePathBuf;

const DEFAULT_AGENTS_MD_FILENAME: &str = "AGENTS.md";
const OVERRIDE_AGENTS_MD_FILENAME: &str = "AGENTS.override.md";
const LOCAL_AGENTS_MD_FILENAME: &str = "AGENTS.local.md";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstructionFileKind {
    Primary,
    LocalAddition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InstructionFile {
    source: AbsolutePathBuf,
    kind: InstructionFileKind,
    contents: String,
}

/// Loads user instructions from a Codex home directory.
#[derive(Clone, Debug)]
pub struct CodexHomeUserInstructionsProvider {
    codex_home: AbsolutePathBuf,
}

impl CodexHomeUserInstructionsProvider {
    /// Creates a provider rooted at the supplied absolute Codex home directory.
    pub fn new(codex_home: AbsolutePathBuf) -> Self {
        Self { codex_home }
    }

    async fn load_from_codex_home(&self) -> LoadedUserInstructions {
        let mut warnings = Vec::new();
        let mut selected = Vec::new();
        if let Some(instructions) = self
            .read_first_present(
                [OVERRIDE_AGENTS_MD_FILENAME, DEFAULT_AGENTS_MD_FILENAME],
                InstructionFileKind::Primary,
                &mut warnings,
            )
            .await
        {
            selected.push(instructions);
        }
        if let Some(instructions) = self
            .read_instruction_file(
                LOCAL_AGENTS_MD_FILENAME,
                InstructionFileKind::LocalAddition,
                &mut warnings,
            )
            .await
        {
            selected.push(instructions);
        }

        let Some(source) = selected
            .first()
            .map(|instructions| instructions.source.clone())
        else {
            return LoadedUserInstructions {
                instructions: None,
                warnings,
            };
        };

        LoadedUserInstructions {
            instructions: Some(UserInstructions {
                text: selected
                    .into_iter()
                    .map(render_instruction_file)
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                source,
            }),
            warnings,
        }
    }

    async fn read_first_present<const N: usize>(
        &self,
        candidates: [&str; N],
        kind: InstructionFileKind,
        warnings: &mut Vec<String>,
    ) -> Option<InstructionFile> {
        for candidate in candidates {
            if let Some(instructions) = self.read_instruction_file(candidate, kind, warnings).await
            {
                return Some(instructions);
            }
        }
        None
    }

    async fn read_instruction_file(
        &self,
        filename: &str,
        kind: InstructionFileKind,
        warnings: &mut Vec<String>,
    ) -> Option<InstructionFile> {
        let path = self.codex_home.join(filename);
        match tokio::fs::metadata(path.as_path()).await {
            Ok(metadata) if !metadata.is_file() => return None,
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
            Err(err) => {
                warnings.push(format!(
                    "Failed to read global AGENTS.md instructions from `{}`: {err}",
                    path.display()
                ));
                return None;
            }
        }
        let data = match tokio::fs::read(path.as_path()).await {
            Ok(data) => data,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
            Err(err) => {
                warnings.push(format!(
                    "Failed to read global AGENTS.md instructions from `{}`: {err}",
                    path.display()
                ));
                return None;
            }
        };
        let contents = String::from_utf8_lossy(&data);
        let trimmed = contents.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(InstructionFile {
                source: path,
                kind,
                contents: trimmed.to_string(),
            })
        }
    }
}

fn render_instruction_file(instructions: InstructionFile) -> String {
    let source = instructions.source.as_path().display();
    let heading = match instructions.kind {
        InstructionFileKind::Primary => format!("Instructions from `{source}`"),
        InstructionFileKind::LocalAddition => format!("Local additions from `{source}`"),
    };
    format!("{heading}\n\n{}", instructions.contents)
}

impl UserInstructionsProvider for CodexHomeUserInstructionsProvider {
    fn load_user_instructions(&self) -> LoadUserInstructionsFuture<'_> {
        Box::pin(self.load_from_codex_home())
    }
}

#[cfg(test)]
mod tests;
