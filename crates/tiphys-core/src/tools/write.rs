//! Changing files: `write_file` and `edit_file`.
//!
//! Both work out the whole new content when they are planned, so what the
//! owner is shown is exactly what will be written. Where a write lands
//! decides whether it runs or asks: inside the agent's own home it runs,
//! anywhere else it asks, and Tiphys's own files are never touched.
//!
//! A file is replaced in one step, so a reader sees the old content or the
//! new, and its permissions are kept.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use similar::TextDiff;

use super::{Action, Output, Tool, ToolCtx, parse_args};
use crate::files::write_atomic;
use crate::policy::{Class, Verdict, paths};

/// The largest file these tools will read to change or to show a diff of.
const EDIT_LIMIT: u64 = 2 * 1024 * 1024;
/// Lines of a diff shown before it is cut.
const PREVIEW_LINES: usize = 60;
/// Permissions a new file gets.
const NEW_FILE: u32 = 0o644;

/// A write that has been worked out and is ready to be judged.
struct Change {
    path: PathBuf,
    /// The text as it was read when the change was planned. `None` for a
    /// file that did not exist.
    before: Option<String>,
    after: String,
    verdict: Verdict,
    /// The file usually holds a secret, so its content is not shown.
    hidden: bool,
    verb: &'static str,
}

impl Change {
    fn plan(
        path: PathBuf,
        before: Option<String>,
        after: String,
        verb: &'static str,
        ctx: &ToolCtx,
    ) -> Self {
        Self {
            verdict: paths::write(&path, ctx.places()),
            hidden: paths::holds_secret(&path, ctx.places()),
            // What the rules judged is where the path really points, so that
            // is what is written: a symlink is written through, not replaced.
            path: paths::resolve(&path),
            before,
            after,
            verb,
        }
    }
}

#[async_trait]
impl Action for Change {
    fn verdict(&self) -> Verdict {
        self.verdict.clone()
    }

    fn summary(&self) -> String {
        let lines = self.after.lines().count();
        let unit = if lines == 1 { "line" } else { "lines" };
        format!("{} {} ({lines} {unit})", self.verb, self.path.display())
    }

    fn preview(&self) -> Option<String> {
        // The preview is shown to the owner and kept with the session. A
        // secret is not put there; the summary says which file it is.
        if self.hidden {
            return Some("(the contents are not shown: this file usually holds a secret)".into());
        }
        let before = self.before.as_deref().unwrap_or_default();
        let diff = TextDiff::from_lines(before, &self.after);
        let text = diff
            .unified_diff()
            .context_radius(3)
            .header(
                if self.before.is_some() {
                    "before"
                } else {
                    "(new file)"
                },
                "after",
            )
            .to_string();
        let mut lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        if total > PREVIEW_LINES {
            lines.truncate(PREVIEW_LINES);
            return Some(format!(
                "{}\n[{} more lines of diff]",
                lines.join("\n"),
                total - PREVIEW_LINES
            ));
        }
        Some(lines.join("\n"))
    }

    async fn run(self: Box<Self>, _: &ToolCtx) -> Output {
        let done = tokio::task::spawn_blocking(move || self.write()).await;
        done.unwrap_or_else(|e| Output::error(format!("the tool crashed: {e}")))
    }
}

impl Change {
    fn write(&self) -> Output {
        let failed =
            |e: &dyn std::fmt::Display| Output::error(format!("{}: {e}", self.path.display()));
        // The owner may have been looking at the diff for minutes. If the
        // file is no longer what the diff was made from, it is not applied.
        let now = match std::fs::read_to_string(&self.path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return failed(&e),
        };
        if now != self.before {
            return Output::error(format!(
                "{} changed while this was waiting, so nothing was written; read it again",
                self.path.display()
            ));
        }
        let mode = std::fs::metadata(&self.path)
            .map(|metadata| metadata.permissions().mode() & 0o7777)
            .unwrap_or(NEW_FILE);
        if let Some(parent) = self.path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return failed(&e);
        }
        match write_atomic(&self.path, self.after.as_bytes(), mode) {
            Ok(()) => Output::ok(format!(
                "{} {} ({} bytes)",
                if self.before.is_some() {
                    "wrote"
                } else {
                    "created"
                },
                self.path.display(),
                self.after.len()
            )),
            Err(e) => failed(&e),
        }
    }
}

/// Reads a file to be changed. `Ok(None)` means it does not exist. Anything
/// that cannot be handled as text is an error for the model.
fn read_text(path: &Path) -> Result<Option<String>, String> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if metadata.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }
    if metadata.len() > EDIT_LIMIT {
        return Err(format!(
            "{} is {} bytes, which is too large to change this way",
            path.display(),
            metadata.len()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("{} is not text", path.display()))
}

pub struct WriteFile;

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Create a file, or replace everything in one, with the content given. Missing \
         directories are created. To change part of a file, use edit_file."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file. Absolute, or ~/…"},
                "content": {"type": "string", "description": "The whole content of the file."},
            },
            "required": ["path", "content"],
        })
    }

    fn plan(&self, args: Value, ctx: &ToolCtx) -> Result<Box<dyn Action>, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            path: String,
            content: String,
        }
        let args: Args = parse_args(args)?;
        let path = ctx.locate(&args.path);
        // A write the rules refuse never happens, so there is no reason to
        // read what is there, and inside the key store every reason not to.
        let refused = paths::write(&path, ctx.places()).class == Class::Never;
        let before = if refused { None } else { read_text(&path)? };
        let verb = if before.is_some() { "write" } else { "create" };
        Ok(Box::new(Change::plan(
            path,
            before,
            args.content,
            verb,
            ctx,
        )))
    }
}

pub struct EditFile;

impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Change part of a text file by replacing `old` with `new`. `old` must match the file \
         exactly, whitespace included, and must appear once: include enough of the surrounding \
         text to make it so, or set `all` to replace every occurrence."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file. Absolute, or ~/…"},
                "old": {"type": "string", "description": "The exact text to replace."},
                "new": {"type": "string", "description": "What to put in its place."},
                "all": {"type": "boolean", "description": "Replace every occurrence of `old`."},
            },
            "required": ["path", "old", "new"],
        })
    }

    fn plan(&self, args: Value, ctx: &ToolCtx) -> Result<Box<dyn Action>, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            path: String,
            old: String,
            new: String,
            #[serde(default)]
            all: bool,
        }
        let args: Args = parse_args(args)?;
        let path = ctx.locate(&args.path);
        if paths::write(&path, ctx.places()).class == Class::Never {
            return Ok(Box::new(Change::plan(
                path,
                None,
                String::new(),
                "edit",
                ctx,
            )));
        }
        if args.old.is_empty() {
            return Err("`old` is empty; give the text to replace".into());
        }
        if args.old == args.new {
            return Err("`old` and `new` are the same, so there is nothing to change".into());
        }
        let Some(before) = read_text(&path)? else {
            return Err(format!(
                "{} does not exist; use write_file to create it",
                path.display()
            ));
        };
        let after = match before.matches(&args.old).count() {
            0 => {
                return Err(format!(
                    "`old` was not found in {}; read the file and copy the text exactly",
                    path.display()
                ));
            }
            1 => before.replacen(&args.old, &args.new, 1),
            _ if args.all => before.replace(&args.old, &args.new),
            count => {
                return Err(format!(
                    "`old` appears {count} times in {}; include more of the surrounding text, \
                     or set `all`",
                    path.display()
                ));
            }
        };
        Ok(Box::new(Change::plan(
            path,
            Some(before),
            after,
            "edit",
            ctx,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;
    use crate::tools::{Planned, Registry};

    struct Fixture {
        _dir: tempfile::TempDir,
        ctx: ToolCtx,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let state = home.join(".tiphys");
        std::fs::create_dir_all(state.join("keys")).unwrap();
        std::fs::write(state.join("keys/work"), "sk-secret-123").unwrap();
        std::fs::write(state.join("config.toml"), "# mine\n").unwrap();
        std::fs::write(home.join("notes.txt"), "alpha\nbeta\ngamma\nbeta\n").unwrap();
        Fixture {
            ctx: ToolCtx {
                state,
                home: home.clone(),
                cwd: home,
            },
            _dir: dir,
        }
    }

    fn plan(f: &Fixture, tool: &str, args: Value) -> Result<Planned, String> {
        let call = ToolCall {
            id: "c".into(),
            name: tool.into(),
            arguments: args.to_string(),
        };
        Registry::builtin().plan(&call, &f.ctx)
    }

    async fn run(f: &Fixture, tool: &str, args: Value) -> Output {
        match plan(f, tool, args) {
            Ok(planned) => planned.action.run(&f.ctx).await,
            Err(message) => Output::error(message),
        }
    }

    fn text(f: &Fixture, path: &str) -> String {
        std::fs::read_to_string(f.ctx.home.join(path)).unwrap()
    }

    #[tokio::test]
    async fn a_new_file_is_created_with_its_directories() {
        let f = fixture();
        let planned = plan(
            &f,
            "write_file",
            json!({"path": "work/deep/new.txt", "content": "one\ntwo\n"}),
        )
        .unwrap();
        assert_eq!(planned.action.verdict(), Verdict::change());
        assert!(planned.action.summary().starts_with("create "));
        assert!(planned.action.summary().ends_with("new.txt (2 lines)"));
        let preview = planned.action.preview().unwrap();
        assert!(
            preview.contains("(new file)") && preview.contains("+one\n+two"),
            "{preview}"
        );

        let output = planned.action.run(&f.ctx).await;
        assert!(
            output.ok && output.text.starts_with("created "),
            "{}",
            output.text
        );
        assert_eq!(text(&f, "work/deep/new.txt"), "one\ntwo\n");
    }

    #[tokio::test]
    async fn replacing_a_file_shows_the_difference_and_keeps_its_permissions() {
        let f = fixture();
        let path = f.ctx.home.join("notes.txt");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let planned = plan(
            &f,
            "write_file",
            json!({"path": "~/notes.txt", "content": "alpha\nBETA\ngamma\nbeta\n"}),
        )
        .unwrap();
        let preview = planned.action.preview().unwrap();
        assert!(preview.contains("-beta\n+BETA"), "{preview}");
        assert!(planned.action.run(&f.ctx).await.text.starts_with("wrote "));
        assert_eq!(text(&f, "notes.txt"), "alpha\nBETA\ngamma\nbeta\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn an_edit_replaces_exactly_what_was_named() {
        let f = fixture();
        let once = run(
            &f,
            "edit_file",
            json!({"path": "notes.txt", "old": "gamma", "new": "delta"}),
        )
        .await;
        assert!(once.ok, "{}", once.text);
        assert_eq!(text(&f, "notes.txt"), "alpha\nbeta\ndelta\nbeta\n");

        let every = run(
            &f,
            "edit_file",
            json!({"path": "notes.txt", "old": "beta", "new": "b", "all": true}),
        )
        .await;
        assert!(every.ok, "{}", every.text);
        assert_eq!(text(&f, "notes.txt"), "alpha\nb\ndelta\nb\n");
    }

    #[tokio::test]
    async fn an_edit_that_is_not_exact_is_explained_and_changes_nothing() {
        let f = fixture();
        let cases = [
            (
                json!({"path": "notes.txt", "old": "beta", "new": "b"}),
                "appears 2 times",
            ),
            (
                json!({"path": "notes.txt", "old": "missing", "new": "b"}),
                "was not found",
            ),
            (
                json!({"path": "notes.txt", "old": "", "new": "b"}),
                "`old` is empty",
            ),
            (
                json!({"path": "notes.txt", "old": "alpha", "new": "alpha"}),
                "nothing to change",
            ),
            (
                json!({"path": "nope.txt", "old": "a", "new": "b"}),
                "does not exist",
            ),
            (
                json!({"path": ".", "old": "a", "new": "b"}),
                "is a directory",
            ),
        ];
        for (args, expected) in cases {
            let output = run(&f, "edit_file", args.clone()).await;
            assert!(
                !output.ok && output.text.contains(expected),
                "{args}: {}",
                output.text
            );
        }
        assert_eq!(text(&f, "notes.txt"), "alpha\nbeta\ngamma\nbeta\n");
    }

    #[tokio::test]
    async fn a_file_that_changed_while_waiting_is_left_alone() {
        let f = fixture();
        let planned = plan(
            &f,
            "edit_file",
            json!({"path": "notes.txt", "old": "gamma", "new": "delta"}),
        )
        .unwrap();
        // Someone else writes to it while the owner is reading the diff.
        std::fs::write(f.ctx.home.join("notes.txt"), "something else\n").unwrap();
        let output = planned.action.run(&f.ctx).await;
        assert!(
            !output.ok && output.text.contains("changed while this was waiting"),
            "{}",
            output.text
        );
        assert_eq!(text(&f, "notes.txt"), "something else\n");

        // The same for a file that appeared where a new one was planned.
        let planned = plan(
            &f,
            "write_file",
            json!({"path": "fresh.txt", "content": "mine"}),
        )
        .unwrap();
        std::fs::write(f.ctx.home.join("fresh.txt"), "theirs").unwrap();
        assert!(!planned.action.run(&f.ctx).await.ok);
        assert_eq!(text(&f, "fresh.txt"), "theirs");
    }

    #[tokio::test]
    async fn tiphys_own_files_are_refused_and_never_even_read() {
        let f = fixture();
        let targets = [
            "~/.tiphys/config.toml",
            "~/.tiphys/keys/work",
            "~/.tiphys/keys/new",
            "~/.tiphys/log/2026-10.jsonl",
        ];
        for path in targets {
            for (tool, args) in [
                ("write_file", json!({"path": path, "content": "x"})),
                (
                    "edit_file",
                    json!({"path": path, "old": "sk-secret-123", "new": "x"}),
                ),
            ] {
                let planned = plan(&f, tool, args).unwrap();
                assert_eq!(
                    planned.action.verdict().class,
                    Class::Never,
                    "{tool} {path}"
                );
                // Nothing about the file's content is in what gets shown.
                let shown = format!(
                    "{} {:?}",
                    planned.action.summary(),
                    planned.action.preview()
                );
                assert!(!shown.contains("sk-secret"), "{shown}");
            }
        }
        assert_eq!(
            std::fs::read_to_string(f.ctx.state.join("keys/work")).unwrap(),
            "sk-secret-123"
        );
    }

    #[tokio::test]
    async fn a_write_outside_home_asks_and_a_secret_is_not_previewed() {
        let f = fixture();
        let outside = plan(
            &f,
            "write_file",
            json!({"path": "/etc/tiphys-test-not-real.conf", "content": "x"}),
        )
        .unwrap();
        let verdict = outside.action.verdict();
        assert_eq!(verdict.class, Class::System);
        assert!(
            verdict.why.contains("outside Tiphys's own home"),
            "{}",
            verdict.why
        );

        std::fs::write(f.ctx.home.join(".env"), "TOKEN=old-secret\n").unwrap();
        let secret = plan(
            &f,
            "write_file",
            json!({"path": ".env", "content": "TOKEN=new-secret\n"}),
        )
        .unwrap();
        assert_eq!(secret.action.verdict().class, Class::System);
        let preview = secret.action.preview().unwrap();
        assert!(
            !preview.contains("old-secret") && !preview.contains("new-secret"),
            "{preview}"
        );
    }

    #[tokio::test]
    async fn a_long_diff_is_cut_and_says_how_much_is_left() {
        let f = fixture();
        let content: String = (0..500).map(|n| format!("line {n}\n")).collect();
        let planned = plan(
            &f,
            "write_file",
            json!({"path": "big.txt", "content": content}),
        )
        .unwrap();
        let preview = planned.action.preview().unwrap();
        assert_eq!(preview.lines().count(), PREVIEW_LINES + 1);
        assert!(
            preview.ends_with("more lines of diff]"),
            "{}",
            &preview[preview.len() - 40..]
        );
    }

    #[tokio::test]
    async fn a_symlink_is_written_through_and_judged_by_where_it_points() {
        let f = fixture();
        std::os::unix::fs::symlink(f.ctx.home.join("notes.txt"), f.ctx.home.join("link.txt"))
            .unwrap();
        let output = run(
            &f,
            "write_file",
            json!({"path": "link.txt", "content": "through\n"}),
        )
        .await;
        assert!(output.ok, "{}", output.text);
        assert_eq!(text(&f, "notes.txt"), "through\n");
        assert!(f.ctx.home.join("link.txt").is_symlink());

        std::os::unix::fs::symlink(f.ctx.state.join("config.toml"), f.ctx.home.join("sneaky"))
            .unwrap();
        let planned = plan(&f, "write_file", json!({"path": "sneaky", "content": "x"})).unwrap();
        assert_eq!(planned.action.verdict().class, Class::Never);
    }
}
