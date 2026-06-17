//! AGENTS.md discovery and user instruction assembly.
//!
//! Project-level documentation is primarily stored in files named `AGENTS.md`.
//! Additional fallback filenames can be configured via `project_doc_fallback_filenames`.
//! We include the concatenation of all files found along the path from the
//! project root to the current working directory as follows:
//!
//! 1.  Determine the project root by walking upwards from the current working
//!     directory until a configured `project_root_markers` entry is found.
//!     When `project_root_markers` is unset, the default marker list is used
//!     (`.git`). If no marker is found, only the current working directory is
//!     considered. An empty marker list disables parent traversal.
//! 2.  Collect every effective base instructions file found from the project root
//!     down to the current working directory (inclusive), then append
//!     `AGENTS.local.md` from the same directory when present.
//! 3.  We do **not** walk past the project root.

use crate::config::Config;
use crate::context::ContextualUserFragment;
use crate::context::UserInstructions as ContextUserInstructions;
use crate::environment_selection::TurnEnvironmentSnapshot;
use codex_app_server_protocol::ConfigLayerSource;
use codex_config::ConfigLayerStackOrdering;
use codex_config::default_project_root_markers;
use codex_config::merge_toml_values;
use codex_config::project_root_markers_from_config;
use codex_exec_server::ExecutorFileSystem;
use codex_extension_api::UserInstructions;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use toml::Value as TomlValue;
use tracing::error;

/// Default filename scanned for AGENTS.md instructions.
pub const DEFAULT_AGENTS_MD_FILENAME: &str = "AGENTS.md";
/// Preferred replacement for AGENTS.md instructions.
pub const OVERRIDE_AGENTS_MD_FILENAME: &str = "AGENTS.override.md";
/// Local additive overlay for AGENTS.md instructions.
pub const LOCAL_AGENTS_MD_FILENAME: &str = "AGENTS.local.md";

/// When both user and project AGENTS.md docs are present, they will be
/// concatenated with the following separator.
const AGENTS_MD_SEPARATOR: &str = "\n\n--- project-doc ---\n\n";
const AGENTS_MD_REFERENCE_MAX_DEPTH: usize = 5;
const AGENTS_MD_REFERENCE_MAX_FILES: usize = 32;
const AGENTS_MD_REFERENCE_TEXT_EXTENSIONS: &str = concat!(
    "adoc,asciidoc,astro,bash,bat,c,cc,cfg,cjs,clj,cljs,cljc,cmake,cmd,conf,config,cpp,cs,css,",
    "csv,cts,cxx,dart,diff,edn,ejs,elm,env,erb,erl,ex,exs,f,f90,f95,fish,for,go,gql,gradle,",
    "graphql,h,hbs,hpp,hrl,hs,htm,html,hxx,ini,jade,java,js,json,jsx,kt,kts,latex,less,lhs,",
    "lock,log,lua,make,makefile,md,mjs,ml,mli,mts,org,patch,php,pl,pm,properties,proto,ps1,",
    "pug,py,pyi,pyw,r,rake,rb,rs,rst,sass,sbt,scala,scss,sh,sql,svelte,swift,tex,text,toml,",
    "ts,tsx,txt,vue,xml,yaml,yml,zsh",
);

/// Loads project AGENTS.md content and combines it with host-provided user
/// instructions.
pub(crate) async fn load_project_instructions(
    config: &Config,
    user_instructions: Option<UserInstructions>,
    environments: &TurnEnvironmentSnapshot,
) -> Option<LoadedAgentsMd> {
    let mut loaded = LoadedAgentsMd::from_user_instructions(user_instructions);
    for turn_environment in &environments.turn_environments {
        let filesystem = turn_environment.environment.get_filesystem();
        match read_agents_md(
            config,
            filesystem.as_ref(),
            &turn_environment.environment_id,
            turn_environment.cwd(),
        )
        .await
        {
            Ok(Some(docs)) => loaded.entries.extend(docs.entries),
            Ok(None) => {}
            Err(e) => {
                error!(
                    environment_id = turn_environment.environment_id,
                    "error trying to find AGENTS.md docs: {e:#}"
                );
            }
        }
    }

    (!loaded.is_empty()).then_some(loaded)
}

/// Attempt to locate and load AGENTS.md documentation.
///
/// On success returns `Ok(Some(loaded))` where `loaded` contains every
/// discovered doc. If no documentation file is found the function returns
/// `Ok(None)`. Unexpected I/O failures bubble up as `Err` so callers can
/// decide how to handle them.
async fn read_agents_md(
    config: &Config,
    fs: &dyn ExecutorFileSystem,
    environment_id: &str,
    cwd: &PathUri,
) -> io::Result<Option<LoadedAgentsMd>> {
    let max_total = config.project_doc_max_bytes;

    if max_total == 0 {
        return Ok(None);
    }

    let discovery = discover_agents_md(config, cwd, fs).await?;
    if discovery.sources.is_empty() {
        return Ok(None);
    }

    let mut remaining: u64 = max_total as u64;
    let mut loaded = LoadedAgentsMd::default();
    let mut seen_paths = HashSet::new();
    let mut reference_count = 0usize;
    let reference_root = discovery.reference_root;
    let mut canonical_reference_root = None;

    for source in discovery.sources {
        if remaining == 0 {
            break;
        }

        let mut stack = vec![PendingInstructionSource {
            path: source.path,
            source_kind: source.source_kind,
            depth: 0,
        }];
        while let Some(pending) = stack.pop() {
            if remaining == 0 {
                break;
            }
            let require_within_root = pending.source_kind == InstructionSourceKind::Reference;
            if require_within_root && canonical_reference_root.is_none() {
                canonical_reference_root = Some(canonicalize_path_uri(fs, &reference_root).await?);
            }
            let canonical_reference_root = canonical_reference_root.as_ref();
            let Some(read_path) = canonical_instruction_path(
                fs,
                &pending.path,
                canonical_reference_root,
                require_within_root,
            )
            .await?
            else {
                continue;
            };
            if !seen_paths.insert(read_path.clone()) {
                continue;
            }

            let Some((entry, references)) = read_instruction_entry(
                fs,
                &pending.path,
                &read_path,
                pending.source_kind,
                environment_id,
                cwd,
                &mut remaining,
            )
            .await?
            else {
                continue;
            };
            loaded.entries.push(entry);

            if pending.depth >= AGENTS_MD_REFERENCE_MAX_DEPTH {
                continue;
            }

            for reference in references.into_iter().rev() {
                if seen_paths.contains(&reference) {
                    continue;
                }
                if reference_count >= AGENTS_MD_REFERENCE_MAX_FILES {
                    break;
                }
                reference_count += 1;
                stack.push(PendingInstructionSource {
                    path: reference,
                    source_kind: InstructionSourceKind::Reference,
                    depth: pending.depth + 1,
                });
            }
        }
    }

    if loaded.is_empty() {
        Ok(None)
    } else {
        Ok(Some(loaded))
    }
}

async fn read_instruction_entry(
    fs: &dyn ExecutorFileSystem,
    source_path: &PathUri,
    read_path: &PathUri,
    source_kind: InstructionSourceKind,
    environment_id: &str,
    cwd: &PathUri,
    remaining: &mut u64,
) -> io::Result<Option<(InstructionEntry, Vec<PathUri>)>> {
    match fs.get_metadata(read_path, /*sandbox*/ None).await {
        Ok(metadata) if !metadata.is_file => return Ok(None),
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    }

    let mut data = match fs.read_file(read_path, /*sandbox*/ None).await {
        Ok(data) => data,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let size = data.len() as u64;
    if size > *remaining {
        data.truncate(*remaining as usize);
    }

    if size > *remaining {
        tracing::warn!(
            path = %source_path,
            remaining_bytes = *remaining,
            "project doc exceeds remaining budget; truncating"
        );
    }

    let text = String::from_utf8_lossy(&data).to_string();
    if text.trim().is_empty() {
        return Ok(None);
    }
    *remaining = (*remaining).saturating_sub(data.len() as u64);
    let references = collect_reference_paths(&text, source_path);

    Ok(Some((
        InstructionEntry {
            contents: text,
            provenance: InstructionProvenance::Project {
                source_path: source_path.clone(),
                source_kind,
                environment_id: environment_id.to_string(),
                cwd: cwd.clone(),
            },
        },
        references,
    )))
}

/// Discovers AGENTS.md files from the project root to the current working
/// directory, inclusive. Symlinks are allowed.
#[cfg(test)]
async fn agents_md_paths(
    config: &Config,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
) -> io::Result<Vec<PathUri>> {
    let discovery = discover_agents_md(config, cwd, fs).await?;
    Ok(discovery
        .sources
        .into_iter()
        .map(|source| source.path)
        .collect())
}

async fn discover_agents_md(
    config: &Config,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
) -> io::Result<AgentsMdDiscovery> {
    let dir = cwd.clone();

    let mut merged = TomlValue::Table(toml::map::Map::new());
    for layer in config.config_layer_stack.get_layers(
        ConfigLayerStackOrdering::LowestPrecedenceFirst,
        /*include_disabled*/ false,
    ) {
        if matches!(layer.name, ConfigLayerSource::Project { .. }) {
            continue;
        }
        merge_toml_values(&mut merged, &layer.config);
    }
    let project_root_markers = match project_root_markers_from_config(&merged) {
        Ok(Some(markers)) => markers,
        Ok(None) => default_project_root_markers(),
        Err(err) => {
            tracing::warn!("invalid project_root_markers: {err}");
            default_project_root_markers()
        }
    };
    let mut project_root = None;
    if !project_root_markers.is_empty() {
        for current in dir.ancestors() {
            for marker in &project_root_markers {
                let marker_path = current
                    .join(marker)
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
                let marker_exists = match fs.get_metadata(&marker_path, /*sandbox*/ None).await {
                    Ok(_) => true,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => false,
                    Err(err) => return Err(err),
                };
                if marker_exists {
                    project_root = Some(current.clone());
                    break;
                }
            }
            if project_root.is_some() {
                break;
            }
        }
    }

    let reference_root = project_root.clone().unwrap_or_else(|| dir.clone());
    let search_dirs: Vec<PathUri> = if let Some(root) = project_root {
        let mut dirs = Vec::new();
        let mut cursor = dir.clone();
        loop {
            dirs.push(cursor.clone());
            if cursor == root {
                break;
            }
            let Some(parent) = cursor.parent() else {
                break;
            };
            cursor = parent;
        }
        dirs.reverse();
        dirs
    } else {
        vec![dir]
    };

    let mut found: Vec<AgentsMdSource> = Vec::new();
    let candidate_filenames = candidate_filenames(config);
    for d in search_dirs {
        for name in &candidate_filenames {
            let candidate = d
                .join(name)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
            match is_regular_file(fs, &candidate).await {
                Ok(true) => {
                    found.push(AgentsMdSource {
                        path: candidate,
                        source_kind: InstructionSourceKind::Primary,
                    });
                    break;
                }
                Ok(false) => continue,
                Err(err) => return Err(err),
            }
        }

        let local = d
            .join(LOCAL_AGENTS_MD_FILENAME)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        match is_regular_file(fs, &local).await {
            Ok(true) => found.push(AgentsMdSource {
                path: local,
                source_kind: InstructionSourceKind::LocalAddition,
            }),
            Ok(false) => {}
            Err(err) => return Err(err),
        }
    }

    Ok(AgentsMdDiscovery {
        sources: found,
        reference_root,
    })
}

async fn is_regular_file(fs: &dyn ExecutorFileSystem, path: &PathUri) -> io::Result<bool> {
    match fs.get_metadata(path, /*sandbox*/ None).await {
        Ok(md) => Ok(md.is_file),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

async fn canonical_instruction_path(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    canonical_reference_root: Option<&PathUri>,
    require_within_root: bool,
) -> io::Result<Option<PathUri>> {
    if !is_regular_file(fs, path).await? {
        return Ok(None);
    }
    if !require_within_root {
        return Ok(Some(path.clone()));
    }
    if require_within_root && !is_text_reference_path(path) {
        return Ok(None);
    }
    let Some(canonical_reference_root) = canonical_reference_root else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reference root is required for references",
        ));
    };

    let canonical = match canonicalize_path_uri(fs, path).await {
        Ok(path) => path,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !is_uri_within_root(&canonical, canonical_reference_root) {
        return Ok(None);
    }
    Ok(Some(canonical))
}

async fn canonicalize_path_uri(fs: &dyn ExecutorFileSystem, path: &PathUri) -> io::Result<PathUri> {
    fs.canonicalize(path, /*sandbox*/ None).await
}

fn collect_reference_paths(text: &str, source_path: &PathUri) -> Vec<PathUri> {
    extract_reference_targets(text)
        .into_iter()
        .filter_map(|reference| resolve_reference_path(&reference, source_path))
        .collect()
}

fn extract_reference_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut fence_marker = None;
    let mut in_html_comment = false;

    for line in text.lines() {
        if let Some(marker) = markdown_fence_marker(line) {
            if fence_marker == Some(marker) {
                fence_marker = None;
            } else if fence_marker.is_none() {
                fence_marker = Some(marker);
            }
            continue;
        }
        if fence_marker.is_some() {
            continue;
        }
        for segment in markdown_segments_outside_html_comments(line, &mut in_html_comment) {
            extract_reference_targets_from_line(segment, &mut targets);
        }
    }

    targets
}

fn markdown_fence_marker(line: &str) -> Option<char> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some('`')
    } else if trimmed.starts_with("~~~") {
        Some('~')
    } else {
        None
    }
}

fn markdown_segments_outside_html_comments<'a>(
    line: &'a str,
    in_comment: &mut bool,
) -> Vec<&'a str> {
    let mut segments = Vec::new();
    let mut start = 0;
    while start < line.len() {
        if *in_comment {
            if let Some(end) = line[start..].find("-->") {
                *in_comment = false;
                start += end + "-->".len();
            } else {
                break;
            }
        } else if let Some(comment_start) = line[start..].find("<!--") {
            let comment_start = start + comment_start;
            if start < comment_start {
                segments.push(&line[start..comment_start]);
            }
            start = comment_start + "<!--".len();
            *in_comment = true;
        } else {
            segments.push(&line[start..]);
            break;
        }
    }
    segments
}

fn extract_reference_targets_from_line(line: &str, targets: &mut Vec<String>) {
    let mut index = 0;
    while index < line.len() {
        let Some(ch) = line[index..].chars().next() else {
            break;
        };
        if ch == '`' {
            index = skip_inline_code(line, index);
            continue;
        }
        if ch == '@'
            && is_reference_start_boundary(line, index)
            && let Some((reference, next_index)) =
                consume_reference_target(line, index + ch.len_utf8())
        {
            targets.push(reference);
            index = next_index;
            continue;
        }
        index += ch.len_utf8();
    }
}

fn skip_inline_code(line: &str, start: usize) -> usize {
    let marker_len = line[start..]
        .chars()
        .take_while(|ch| *ch == '`')
        .map(char::len_utf8)
        .sum::<usize>();
    let marker = "`".repeat(marker_len);
    let search_start = start + marker_len;
    line[search_start..]
        .find(&marker)
        .map_or(line.len(), |end| search_start + end + marker_len)
}

fn is_reference_start_boundary(line: &str, at_index: usize) -> bool {
    line[..at_index]
        .chars()
        .next_back()
        .is_none_or(char::is_whitespace)
}

fn consume_reference_target(line: &str, start: usize) -> Option<(String, usize)> {
    let mut target = String::new();
    let mut index = start;
    while index < line.len() {
        let ch = line[index..].chars().next()?;
        if ch == '\\' {
            let escaped_start = index + ch.len_utf8();
            if let Some(escaped) = line[escaped_start..].chars().next()
                && escaped.is_whitespace()
            {
                target.push(escaped);
                index = escaped_start + escaped.len_utf8();
                continue;
            }
        }
        if ch.is_whitespace() {
            break;
        }
        target.push(ch);
        index += ch.len_utf8();
    }

    let trimmed = target.trim_end_matches(|ch| {
        matches!(
            ch,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
        )
    });
    is_reference_target(trimmed).then(|| (trimmed.to_string(), index))
}

fn is_reference_target(target: &str) -> bool {
    if target.is_empty() || target.starts_with('@') || target.starts_with('#') {
        return false;
    }
    target
        .chars()
        .next()
        .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '.' | '/' | '\\' | '~' | '_' | '-'))
}

fn resolve_reference_path(reference: &str, source_path: &PathUri) -> Option<PathUri> {
    let reference = reference
        .split_once('#')
        .map_or(reference, |(path, _)| path);
    if reference.is_empty() {
        return None;
    }
    if reference.starts_with('~') {
        return None;
    }

    source_path.parent()?.join(reference).ok()
}

fn is_uri_within_root(path: &PathUri, root: &PathUri) -> bool {
    path == root || path.ancestors().any(|ancestor| &ancestor == root)
}

fn is_text_reference_path(path: &PathUri) -> bool {
    let Some(basename) = path.basename() else {
        return true;
    };
    let Some((stem, extension)) = basename.rsplit_once('.') else {
        return true;
    };
    if stem.is_empty() || extension.is_empty() {
        return true;
    }
    AGENTS_MD_REFERENCE_TEXT_EXTENSIONS
        .split(',')
        .any(|allowed| extension.eq_ignore_ascii_case(allowed))
}

fn candidate_filenames(config: &Config) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::with_capacity(2 + config.project_doc_fallback_filenames.len());
    names.push(OVERRIDE_AGENTS_MD_FILENAME);
    names.push(DEFAULT_AGENTS_MD_FILENAME);
    for candidate in &config.project_doc_fallback_filenames {
        let candidate = candidate.as_str();
        if candidate.is_empty() || candidate == LOCAL_AGENTS_MD_FILENAME {
            continue;
        }
        if !names.contains(&candidate) {
            names.push(candidate);
        }
    }
    names
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentsMdDiscovery {
    sources: Vec<AgentsMdSource>,
    reference_root: PathUri,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentsMdSource {
    path: PathUri,
    source_kind: InstructionSourceKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingInstructionSource {
    path: PathUri,
    source_kind: InstructionSourceKind,
    depth: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstructionSourceKind {
    Primary,
    LocalAddition,
    Reference,
}

/// Model-visible instructions loaded from AGENTS.md files and internal
/// guidance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoadedAgentsMd {
    /// Host-provided user instructions.
    user_instructions: Option<UserInstructions>,

    /// Ordered instructions and their provenance.
    entries: Vec<InstructionEntry>,
}

impl LoadedAgentsMd {
    /// Creates loaded instructions containing one user-level AGENTS.md entry.
    pub fn new_user(contents: String, path: AbsolutePathBuf) -> Self {
        if contents.trim().is_empty() {
            return Self::default();
        }
        Self {
            user_instructions: Some(UserInstructions {
                text: contents,
                source: path,
            }),
            entries: Vec::new(),
        }
    }

    fn from_user_instructions(user_instructions: Option<UserInstructions>) -> Self {
        Self {
            user_instructions: user_instructions
                .filter(|instructions| !instructions.text.trim().is_empty()),
            entries: Vec::new(),
        }
    }

    /// Creates source-less user instructions for tests.
    ///
    /// This cannot be gated with `#[cfg(test)]` because integration tests
    /// compile `codex-core` as a normal dependency without that configuration.
    pub fn from_text_for_testing(contents: impl Into<String>) -> Self {
        let contents = contents.into();
        if contents.trim().is_empty() {
            return Self::default();
        }
        Self {
            user_instructions: None,
            entries: vec![InstructionEntry {
                contents,
                provenance: InstructionProvenance::Internal,
            }],
        }
    }

    fn is_empty(&self) -> bool {
        self.user_instructions.is_none()
            && self
                .entries
                .iter()
                .all(|entry| entry.contents.trim().is_empty())
    }

    /// Returns the concatenated model-visible instruction text.
    pub fn text(&self) -> String {
        if self.has_multiple_project_environments() {
            self.environment_labeled_text()
        } else {
            self.legacy_text()
        }
    }

    fn legacy_text(&self) -> String {
        let mut output = String::new();
        let mut has_previous = false;
        let mut previous_was_project = false;
        if let Some(instructions) = &self.user_instructions {
            output.push_str(&instructions.text);
            has_previous = true;
        }
        for entry in &self.entries {
            let is_project = matches!(&entry.provenance, InstructionProvenance::Project { .. });
            if has_previous {
                // The project-doc marker tells the model where workspace-scoped
                // instructions begin, so it is only needed on the transition
                // from user or internal instructions to project instructions.
                let separator = if is_project && !previous_was_project {
                    AGENTS_MD_SEPARATOR
                } else {
                    "\n\n"
                };
                output.push_str(separator);
            }
            output.push_str(&entry.rendered_contents());
            has_previous = true;
            previous_was_project = is_project;
        }
        output
    }

    fn environment_labeled_text(&self) -> String {
        let mut output = String::new();
        let mut has_previous = false;
        let mut previous_environment: Option<(&str, &PathUri)> = None;
        if let Some(instructions) = &self.user_instructions {
            output.push_str(&instructions.text);
            has_previous = true;
        }
        for entry in &self.entries {
            match &entry.provenance {
                InstructionProvenance::Project {
                    environment_id,
                    cwd,
                    ..
                } => {
                    if has_previous {
                        output.push_str("\n\n");
                    }
                    // One environment can contribute several hierarchical AGENTS.md files from
                    // its project root through its cwd. Label that environment once for the
                    // complete group rather than repeating the label before every file.
                    let environment = (environment_id.as_str(), cwd);
                    if previous_environment != Some(environment) {
                        output.push_str(&format!(
                            "for `{}` with root {}\n\n",
                            environment_id,
                            cwd.inferred_native_path_string()
                        ));
                    }
                    output.push_str(&entry.rendered_contents());
                    previous_environment = Some(environment);
                }
                InstructionProvenance::Internal => {
                    if has_previous {
                        output.push_str("\n\n");
                    }
                    output.push_str(&entry.contents);
                    previous_environment = None;
                }
            }
            has_previous = true;
        }
        output
    }

    /// Returns the complete model-visible contextual user fragment.
    pub(crate) fn render(&self) -> String {
        // One contributing project environment retains the legacy cwd wrapper. With two or more,
        // the body labels every contributing environment itself, so the outer cwd is omitted.
        let directory = if self.has_multiple_project_environments() {
            None
        } else {
            self.single_project_cwd()
                .map(PathUri::inferred_native_path_string)
        };
        ContextUserInstructions {
            directory,
            text: self.text(),
        }
        .render()
    }

    /// Returns the host-provided user instructions.
    pub(crate) fn user_instructions(&self) -> Option<&UserInstructions> {
        self.user_instructions.as_ref()
    }

    /// Returns the AGENTS.md files that supplied instruction entries.
    pub fn sources(&self) -> impl Iterator<Item = PathUri> + '_ {
        self.user_instructions
            .iter()
            .map(|instructions| PathUri::from_abs_path(&instructions.source))
            .chain(
                self.entries
                    .iter()
                    .filter_map(|entry| entry.provenance.path()),
            )
    }

    fn has_multiple_project_environments(&self) -> bool {
        let mut first_environment_id = None;
        self.entries.iter().any(|entry| {
            let InstructionProvenance::Project { environment_id, .. } = &entry.provenance else {
                return false;
            };
            match first_environment_id {
                Some(first_environment_id) => first_environment_id != environment_id,
                None => {
                    first_environment_id = Some(environment_id);
                    false
                }
            }
        })
    }

    fn single_project_cwd(&self) -> Option<&PathUri> {
        self.entries
            .iter()
            .find_map(|entry| match &entry.provenance {
                InstructionProvenance::Project { cwd, .. } => Some(cwd),
                InstructionProvenance::Internal => None,
            })
    }
}

/// One model-visible instruction and its provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InstructionEntry {
    /// Model-visible instruction text.
    contents: String,

    /// Origin of the instruction.
    provenance: InstructionProvenance,
}

impl InstructionEntry {
    fn rendered_contents(&self) -> Cow<'_, str> {
        match &self.provenance {
            InstructionProvenance::Project {
                source_path,
                source_kind,
                ..
            } => {
                let heading = source_heading(source_path, *source_kind);
                let contents = &self.contents;
                Cow::Owned(format!("{heading}\n\n{contents}"))
            }
            InstructionProvenance::Internal => Cow::Borrowed(&self.contents),
        }
    }
}

fn source_heading(source_path: &PathUri, source_kind: InstructionSourceKind) -> String {
    let source_path = source_path
        .to_abs_path()
        .map(|path| path.as_path().display().to_string())
        .unwrap_or_else(|_| source_path.inferred_native_path_string());
    match source_kind {
        InstructionSourceKind::Primary => {
            format!("Instructions from `{source_path}`")
        }
        InstructionSourceKind::LocalAddition => {
            format!("Local additions from `{source_path}`")
        }
        InstructionSourceKind::Reference => {
            format!("Referenced instructions from `{source_path}`")
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
enum InstructionProvenance {
    /// Workspace instructions discovered from project AGENTS.md files.
    Project {
        /// Exact instruction file, distinct from the environment's selected cwd.
        source_path: PathUri,
        source_kind: InstructionSourceKind,
        environment_id: String,
        cwd: PathUri,
    },

    /// Instructions without a file source, including internally defined guidance.
    Internal,
}

impl InstructionProvenance {
    fn path(&self) -> Option<PathUri> {
        match self {
            Self::Project { source_path, .. } => Some(source_path.clone()),
            Self::Internal => None,
        }
    }
}

#[cfg(test)]
#[path = "agents_md_tests.rs"]
mod tests;
