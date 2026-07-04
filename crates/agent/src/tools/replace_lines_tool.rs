use agent_client_protocol::schema as acp;
use anyhow::Result;
use gpui::{App, Entity, SharedString, Task};
use language::Point;
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

use crate::tools::tool_permissions::{
    ResolvedProjectPath, canonicalize_worktree_roots, resolve_project_path,
};
use crate::{AgentTool, ToolCallEventStream, ToolInput};

/// Edit a file using content-anchored find & replace (preferred) or line numbers.
///
/// # Find & Replace mode (recommended)
///
/// Use `find` + `replace` to target content exactly. The tool validates:
/// - `find` must match exactly once in the file (use `occurrence` for duplicates,
///   `all: true` for bulk replacements, or `around` to narrow the match).
/// - Returns a unified diff so you can confirm the right change was applied.
/// - Multiple matches without explicit choice → returns a match list (no write).
///
/// The agent never guesses which occurrence to replace — it either gets it on the
/// first try (unique match) or receives a list of matches to disambiguate.
///
/// # Line mode (legacy)
///
/// Use `start_line` / `end_line` / `new_text` for line-numbered replacement.
/// Only use this when you've just read the file and are certain of exact line
/// numbers.
///
/// Example find/replace: `find: "return bar()"`, `replace: "return baz()"`
/// Example line mode: `read_file` shows lines 42-58, then replace them with new text.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReplaceLinesInput {
    /// The full path of the file to edit. Must start with a project root directory name.
    pub path: PathBuf,

    // ── Find & Replace mode (preferred) ──────────────────────────────
    /// Exact text to find in the file. Must match character-for-character.
    /// The tool will list all matches if the text appears multiple times
    /// without `all: true`, `occurrence`, or `around`.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub find: Option<String>,

    /// New text to replace the found text with. Must be present when `find` is set.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace: Option<String>,

    /// Replace *every* occurrence of `find`. Use for global renames, import
    /// rewrites, etc. The response includes the total number of replacements.
    #[serde(default)]
    pub all: bool,

    /// Narrow the match: `around` must appear as a substring within a
    /// few lines of the matching line. Typical use: `find: "pass"`,
    /// `around: "def empty"` — only replaces `pass` near `def empty`.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub around: Option<String>,

    /// Which occurrence to replace (1-based). Only needed when count > 1.
    /// The tool reports the list of matches; the agent picks an index and retries.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<u32>,

    // ── Line mode (legacy) ───────────────────────────────────────────
    /// First line to replace (1-based, inclusive).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,

    /// Last line to replace (1-based, inclusive). Must be ≥ start_line.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,

    /// New text that replaces lines start_line..=end_line.
    /// Include trailing newline if the last replacement line needs one.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
}

pub struct ReplaceLinesTool {
    project: Entity<Project>,
}

impl ReplaceLinesTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for ReplaceLinesTool {
    type Input = ReplaceLinesInput;
    type Output = String;

    /// Internal name. Mapped to `"edit_file"` in `enabled_tools()`.
    const NAME: &'static str = "replace_lines";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Edit
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(i) => {
                let file = i
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_else(|| i.path.to_string_lossy());
                if let (Some(find), Some(replace)) = (&i.find, &i.replace) {
                    let short = {
                        let f = find.trim();
                        if f.len() > 30 {
                            format!("{}…", &f[..28])
                        } else {
                            f.to_string()
                        }
                    };
                    format!("{}: \"{}\" → \"{}\"", file, short, replace).into()
                } else if let (Some(s), Some(_new)) = (i.start_line, &i.new_text) {
                    let e = i.end_line.unwrap_or(s);
                    format!("{}:{}-{}", file, s, e).into()
                } else {
                    "Edit file".into()
                }
            }
            Err(_) => "Edit file".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input.recv().await.map_err(|e| e.to_string())?;

            let fs = project.read_with(cx, |project, _cx| project.fs().clone());
            let canonical_roots = canonicalize_worktree_roots(&project, &fs, cx).await;

            let project_path = project
                .read_with(cx, |project, cx| {
                    let resolved =
                        resolve_project_path(project, &input.path, &canonical_roots, cx)?;
                    match resolved {
                        ResolvedProjectPath::Safe(path) => anyhow::Ok(path),
                        ResolvedProjectPath::SymlinkEscape { .. } => {
                            anyhow::bail!("Cannot edit symlink target: {}", input.path.display())
                        }
                    }
                })
                .map_err(|e| e.to_string())?;

            let abs_path = project
                .read_with(cx, |project, cx| project.absolute_path(&project_path, cx))
                .ok_or_else(|| format!("Failed to resolve path: {}", input.path.display()))
                .map_err(|e| e.to_string())?;

            // Extract find/replace before dispatching to avoid borrow conflicts with input move
            let find = input.find.clone();
            let replace = input.replace.clone();
            let is_find_mode = find.is_some() && replace.is_some();

            if is_find_mode {
                return run_find_replace(
                    &project,
                    &project_path,
                    &abs_path,
                    find.as_deref().unwrap(),
                    replace.as_deref().unwrap(),
                    input,
                    event_stream,
                    cx,
                )
                .await;
            }

            // Legacy line mode
            let start_line = input.start_line.ok_or_else(|| {
                "Either 'find' + 'replace' or 'start_line' + 'new_text' must be provided"
                    .to_string()
            })?;
            let end_line = input.end_line.unwrap_or(start_line);
            let new_text = input
                .new_text
                .as_ref()
                .ok_or_else(|| "'new_text' must be provided when using line mode".to_string())?;

            run_line_mode(
                &project,
                &project_path,
                &abs_path,
                start_line,
                end_line,
                new_text,
                input.path,
                event_stream,
                cx,
            )
            .await
        })
    }
}

// ── Find & Replace implementation ────────────────────────────────────────

const AROUND_WINDOW: usize = 1;
struct MatchInfo {
    index: usize,
    line: usize,
    context: String,
}

fn find_matches(haystack: &str, needle: &str, around: Option<&str>) -> Vec<MatchInfo> {
    let mut matches = Vec::new();
    let lines: Vec<&str> = haystack.lines().collect();
    let needle_trimmed = needle.trim();

    for (line_idx, line) in lines.iter().enumerate() {
        if line.contains(needle_trimmed) {
            if let Some(around_needle) = around {
                let ws = line_idx.saturating_sub(AROUND_WINDOW);
                let we = (line_idx + AROUND_WINDOW + 1).min(lines.len());
                if !lines[ws..we].iter().any(|l| l.contains(around_needle)) {
                    continue;
                }
            }
            let cs = line_idx.saturating_sub(1);
            let ce = (line_idx + 2).min(lines.len());
            matches.push(MatchInfo {
                index: matches.len() + 1,
                line: line_idx + 1,
                context: lines[cs..ce].join("\n"),
            });
        }
    }
    matches
}

fn format_diff(old_text: &str, new_text: &str) -> String {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let new_lines: Vec<&str> = new_text.lines().collect();
    let mut diff = String::from("```diff\n");

    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let pre_ctx = prefix.saturating_sub(1);

    let mut suffix = 0;
    while suffix < old_lines.len().saturating_sub(prefix)
        && suffix < new_lines.len().saturating_sub(prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let old_end = old_lines.len().saturating_sub(suffix);
    let new_end = new_lines.len().saturating_sub(suffix);

    if pre_ctx < prefix {
        diff.push_str(&format!("  {}\n", old_lines[pre_ctx]));
    }
    for line in &old_lines[prefix..old_end] {
        diff.push_str(&format!("- {}\n", line));
    }
    for line in &new_lines[prefix..new_end] {
        diff.push_str(&format!("+ {}\n", line));
    }
    if new_end < new_lines.len() {
        diff.push_str(&format!("  {}\n", new_lines[new_end]));
    }
    diff.push_str("```");
    diff
}

async fn run_find_replace(
    project: &Entity<Project>,
    project_path: &project::ProjectPath,
    _abs_path: &PathBuf,
    find: &str,
    replace: &str,
    input: ReplaceLinesInput,
    _event_stream: ToolCallEventStream,
    cx: &mut gpui::AsyncApp,
) -> Result<String, String> {
    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer(project_path.clone(), cx)
        })
        .await
        .map_err(|e| e.to_string())?;

    buffer
        .update(cx, |buffer, cx| {
            let snapshot = buffer.text_snapshot();
            let full_text: String = snapshot.text().to_string();
            let matches = find_matches(&full_text, find, input.around.as_deref());
            if matches.is_empty() {
                return Err(format!(
                    "No match found for '{}'{} in {}",
                    find,
                    if input.around.is_some() {
                        format!(" with around '{}'", input.around.as_ref().unwrap())
                    } else {
                        String::new()
                    },
                    input.path.display()
                ));
            }

            // `all` mode
            if input.all {
                let replaced = full_text.replace(find, replace);
                let diff = format_diff(&full_text, &replaced);
                buffer.set_text(replaced, cx);
                return Ok(format!(
                    "Replaced {} occurrence{} of '{}' in {}\n\nDiff:\n{}",
                    matches.len(),
                    if matches.len() == 1 { "" } else { "s" },
                    find,
                    input.path.display(),
                    diff
                ));
            }

            // Single match or explicit occurrence
            if matches.len() == 1 || input.occurrence.is_some() {
                let idx = input
                    .occurrence
                    .map(|o| o.saturating_sub(1) as usize)
                    .unwrap_or(0);

                if idx >= matches.len() {
                    return Err(format!(
                        "Occurrence {} is out of range ({} match{})",
                        input.occurrence.unwrap(),
                        matches.len(),
                        if matches.len() == 1 { "" } else { "es" }
                    ));
                }

                let target = matches[idx].line.saturating_sub(1);
                let old = full_text;
                let lines_vec: Vec<&str> = old.lines().collect();
                let mut out = String::new();
                for (i, line) in lines_vec.iter().enumerate() {
                    if i == target && line.contains(find.trim()) {
                        out.push_str(&line.replace(find.trim(), replace));
                    } else {
                        out.push_str(line);
                    }
                    if i + 1 < lines_vec.len() {
                        out.push('\n');
                    }
                }
                if old.ends_with('\n') && !out.ends_with('\n') {
                    out.push('\n');
                }

                let diff = format_diff(&old, &out);
                buffer.set_text(out, cx);

                return Ok(format!(
                    "Replaced occurrence {}/{} of '{}' at line {} in {}\n\nDiff:\n{}",
                    idx + 1,
                    matches.len(),
                    find,
                    matches[idx].line,
                    input.path.display(),
                    diff
                ));
            }

            // Multiple matches → feedback, no write
            let mut report = format!(
                "Found {} matches for '{}' in {}.\n\
                 Options: `all: true` (replace all), `occurrence` (pick one), \
                 or `around` (narrow scope).\n\nMatches:\n",
                matches.len(),
                find,
                input.path.display()
            );
            for m in &matches {
                let indented = m.context.replace('\n', "\n        ");
                report.push_str(&format!(
                    "  [{idx}] line {line}:\n        {ctx}\n\n",
                    idx = m.index,
                    line = m.line,
                    ctx = indented
                ));
            }
            Err(report)
        })
        .map_err(|e| e.to_string())
}

// ── Line mode (legacy) ───────────────────────────────────────────────────

async fn run_line_mode(
    project: &Entity<Project>,
    project_path: &project::ProjectPath,
    abs_path: &PathBuf,
    start_line: u32,
    end_line: u32,
    new_text: &str,
    display_path: PathBuf,
    event_stream: ToolCallEventStream,
    cx: &mut gpui::AsyncApp,
) -> Result<String, String> {
    cx.update(|_cx| {
        event_stream.update_fields(acp::ToolCallUpdateFields::new().locations(vec![
            acp::ToolCallLocation::new(abs_path).line(Some(start_line.saturating_sub(1))),
        ]));
    });

    let buffer = project
        .update(cx, |project, cx| {
            project.open_buffer(project_path.clone(), cx)
        })
        .await
        .map_err(|e| e.to_string())?;

    let result = buffer.update(cx, |buffer, cx| {
        let snapshot = buffer.text_snapshot();
        let total_lines = snapshot.max_point().row + 1;

        let sl = start_line.max(1).min(total_lines);
        let el = end_line.max(sl).min(total_lines);

        let mut new_content = if sl > 1 {
            let before_end = Point::new(sl - 1, 0);
            snapshot
                .text_for_range(Point::zero()..before_end)
                .collect::<String>()
        } else {
            String::new()
        };

        new_content.push_str(new_text);
        if !new_text.ends_with('\n') {
            new_content.push('\n');
        }

        if el < total_lines {
            let after_start = Point::new(el, 0);
            new_content.push_str(
                &snapshot
                    .text_for_range(after_start..snapshot.max_point())
                    .collect::<String>(),
            );
        }

        buffer.set_text(new_content, cx);
        Ok::<_, String>(format!(
            "Replaced lines {}-{} in {}",
            sl,
            el,
            display_path.display()
        ))
    })?;

    project
        .update(cx, |project, cx| project.save_buffer(buffer, cx))
        .await
        .map_err(|e| e.to_string())?;

    Ok(result)
}

#[cfg(test)]
#[allow(clippy::arc_with_non_send_sync)]
mod tests {
    use super::*;
    use crate::{AgentTool, ToolCallEventStream, ToolInput};
    use fs::FakeFs;
    use gpui::TestAppContext;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::Arc;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    fn line_input(path: &str, sl: u32, el: u32, t: &str) -> ReplaceLinesInput {
        ReplaceLinesInput {
            path: path.into(),
            find: None,
            replace: None,
            all: false,
            around: None,
            occurrence: None,
            start_line: Some(sl),
            end_line: Some(el),
            new_text: Some(t.to_string()),
        }
    }

    fn find_input(
        path: &str,
        f: &str,
        r: &str,
        all: bool,
        around: Option<&str>,
        occ: Option<u32>,
    ) -> ReplaceLinesInput {
        ReplaceLinesInput {
            path: path.into(),
            find: Some(f.to_string()),
            replace: Some(r.to_string()),
            all,
            around: around.map(|s| s.to_string()),
            occurrence: occ,
            start_line: None,
            end_line: None,
            new_text: None,
        }
    }

    // ── Line mode tests ──────────────────────────────────────────────

    #[gpui::test]
    async fn test_replace_single_line(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({"test.rs": "line1\nline2\nline3\n"}))
            .await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let tool = Arc::new(ReplaceLinesTool::new(project));
        let (es, _rx) = ToolCallEventStream::test();
        let res = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(line_input("root/test.rs", 2, 2, "REPLACED")),
                    es,
                    cx,
                )
            })
            .await
            .expect("ok");
        assert!(res.contains("Replaced lines 2-2"));
    }

    #[gpui::test]
    async fn test_replace_multiple_lines(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            json!({"test.rs": "line1\nline2\nline3\nline4\nline5\n"}),
        )
        .await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let tool = Arc::new(ReplaceLinesTool::new(project));
        let (es, _rx) = ToolCallEventStream::test();
        let res = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(line_input("root/test.rs", 2, 4, "A\nB\n")),
                    es,
                    cx,
                )
            })
            .await
            .expect("ok");
        assert!(res.contains("Replaced lines 2-4"));
    }

    #[gpui::test]
    async fn test_replace_first_line(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({"test.rs": "old\nline2\nline3\n"}))
            .await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let tool = Arc::new(ReplaceLinesTool::new(project));
        let (es, _rx) = ToolCallEventStream::test();
        let res = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(line_input("root/test.rs", 1, 1, "NEW FIRST LINE")),
                    es,
                    cx,
                )
            })
            .await
            .expect("ok");
        assert!(res.contains("Replaced lines 1-1"));
    }

    // ── Find & Replace tests ─────────────────────────────────────────

    #[gpui::test]
    async fn test_find_replace_unique(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            json!({"test.rs": "fn foo() {\n    return bar();\n}\n"}),
        )
        .await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let tool = Arc::new(ReplaceLinesTool::new(project));
        let (es, _rx) = ToolCallEventStream::test();
        let res = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(find_input(
                        "root/test.rs",
                        "return bar();",
                        "return baz();",
                        false,
                        None,
                        None,
                    )),
                    es,
                    cx,
                )
            })
            .await
            .expect("ok");
        assert!(res.contains("Replaced occurrence 1/1"));
        assert!(res.contains("- ") && res.contains("return bar();"));
        assert!(res.contains("+ ") && res.contains("return baz();"));
    }

    #[gpui::test]
    async fn test_find_replace_multiple_feedback(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            json!({"test.rs": "fn a() { pass }\nfn b() { pass }\nfn c() { pass }\n"}),
        )
        .await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let tool = Arc::new(ReplaceLinesTool::new(project));
        let (es, _rx) = ToolCallEventStream::test();
        let res = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(find_input(
                        "root/test.rs",
                        "pass",
                        "return None",
                        false,
                        None,
                        None,
                    )),
                    es,
                    cx,
                )
            })
            .await;
        let err = res.unwrap_err();
        assert!(err.contains("Found 3 matches"));
        assert!(err.contains("[1] line 1"));
        assert!(err.contains("[2] line 2"));
        assert!(err.contains("[3] line 3"));
    }

    #[gpui::test]
    async fn test_find_replace_occurrence(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            json!({"test.rs": "fn a() { pass }\nfn b() { pass }\nfn c() { pass }\n"}),
        )
        .await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let tool = Arc::new(ReplaceLinesTool::new(project));
        let (es, _rx) = ToolCallEventStream::test();
        let res = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(find_input(
                        "root/test.rs",
                        "pass",
                        "return None",
                        false,
                        None,
                        Some(2),
                    )),
                    es,
                    cx,
                )
            })
            .await
            .expect("ok");
        assert!(res.contains("Replaced occurrence 2/3"));
    }

    #[gpui::test]
    async fn test_find_replace_all(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({"test.rs": "pass\nkeep\npass\npass\n"}))
            .await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let tool = Arc::new(ReplaceLinesTool::new(project));
        let (es, _rx) = ToolCallEventStream::test();
        let res = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(find_input(
                        "root/test.rs",
                        "pass",
                        "done",
                        true,
                        None,
                        None,
                    )),
                    es,
                    cx,
                )
            })
            .await
            .expect("ok");
        assert!(res.contains("Replaced 3 occurrences"));
    }

    #[gpui::test]
    async fn test_find_replace_with_around(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/root",
            json!({"test.rs": "fn a() { pass }\nfn b() { x() }\nfn c() { pass }\n"}),
        )
        .await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let tool = Arc::new(ReplaceLinesTool::new(project));
        let (es, _rx) = ToolCallEventStream::test();
        let res = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(find_input(
                        "root/test.rs",
                        "pass",
                        "return None",
                        false,
                        Some("fn c"),
                        None,
                    )),
                    es,
                    cx,
                )
            })
            .await
            .expect("ok");
        assert!(res.contains("Replaced occurrence 1/1"));
    }

    #[gpui::test]
    async fn test_find_replace_no_match(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", json!({"test.rs": "hello world\n"}))
            .await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let tool = Arc::new(ReplaceLinesTool::new(project));
        let (es, _rx) = ToolCallEventStream::test();
        let res = cx
            .update(|cx| {
                tool.run(
                    ToolInput::resolved(find_input(
                        "root/test.rs",
                        "nonsense",
                        "x",
                        false,
                        None,
                        None,
                    )),
                    es,
                    cx,
                )
            })
            .await;
        assert!(res.unwrap_err().contains("No match found"));
    }
}
