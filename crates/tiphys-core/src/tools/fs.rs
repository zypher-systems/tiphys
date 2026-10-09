//! Reading the filesystem: `read_file`, `list_dir`, `search_files`.
//!
//! These are reads, so they run without asking. Each is bounded: a read
//! returns a window of lines, a listing a number of entries, a search a number
//! of matches, and each says when there was more. None of them will read the
//! key store, by any path that leads to it.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};
use walkdir::WalkDir;

use super::{Action, Output, Tool, ToolCtx, parse_args};
use crate::keys::KEYS_DIR;
use crate::policy::{Class, read_class};

/// Lines a read returns unless asked for fewer.
const READ_LINES: u64 = 2000;
/// Bytes a read returns at most, whatever the line count.
const READ_BYTES: usize = 30_000;
/// Entries a listing returns at most.
const LIST_ENTRIES: usize = 500;
/// Matches a search returns at most.
const SEARCH_MATCHES: usize = 200;
/// Files larger than this are not searched.
const SEARCH_FILE_BYTES: u64 = 1024 * 1024;
/// Characters of a matching line that are shown.
const SEARCH_LINE_CHARS: usize = 300;
/// Directories a search does not go into: version control and build output.
const SEARCH_SKIPS: &[&str] = &[".git", "node_modules", "target"];

/// Where a path the model gave points: `~` is the Tiphys user's home, and a
/// relative path starts from the working directory.
fn locate(path: &str, ctx: &ToolCtx) -> PathBuf {
    let path = path.trim();
    if path == "~" {
        ctx.home.clone()
    } else if let Some(rest) = path.strip_prefix("~/") {
        ctx.home.join(rest)
    } else {
        ctx.cwd.join(path)
    }
}

fn failed(path: &Path, error: impl std::fmt::Display) -> Output {
    Output::error(format!("{}: {error}", path.display()))
}

/// Runs blocking file work off the async runtime.
async fn blocking(work: impl FnOnce() -> Output + Send + 'static) -> Output {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|e| Output::error(format!("the tool crashed: {e}")))
}

/// Whether the start of a file looks like something other than text.
fn looks_binary(head: &[u8]) -> bool {
    head.contains(&0)
}

pub struct ReadFile;

struct Read {
    path: PathBuf,
    first: u64,
    lines: u64,
    class: Class,
}

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a text file. Returns up to 2000 lines from `offset`; when the file goes on, the \
         result says which line to continue from."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file. Absolute, or ~/…"},
                "offset": {"type": "integer", "description": "The first line to return, from 1."},
                "limit": {"type": "integer", "description": "How many lines to return."},
            },
            "required": ["path"],
        })
    }

    fn plan(&self, args: Value, ctx: &ToolCtx) -> Result<Box<dyn Action>, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            path: String,
            offset: Option<u64>,
            limit: Option<u64>,
        }
        let args: Args = parse_args(args)?;
        let path = locate(&args.path, ctx);
        Ok(Box::new(Read {
            class: read_class(&path, &ctx.state),
            path,
            first: args.offset.unwrap_or(1).max(1),
            lines: args.limit.unwrap_or(READ_LINES).clamp(1, READ_LINES),
        }))
    }
}

#[async_trait]
impl Action for Read {
    fn class(&self) -> Class {
        self.class
    }

    fn summary(&self) -> String {
        format!("read {}", self.path.display())
    }

    async fn run(self: Box<Self>, _: &ToolCtx) -> Output {
        blocking(move || read(&self.path, self.first, self.lines)).await
    }
}

fn read(path: &Path, first: u64, max_lines: u64) -> Output {
    let size = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            return failed(path, "is a directory; use list_dir");
        }
        Ok(metadata) => metadata.len(),
        Err(e) => return failed(path, e),
    };
    let mut reader = match File::open(path) {
        Ok(file) => BufReader::new(file),
        Err(e) => return failed(path, e),
    };
    match reader.fill_buf() {
        Ok(head) if looks_binary(head) => {
            return Output::ok(format!(
                "{} is not text ({size} bytes); not shown",
                path.display()
            ));
        }
        Ok(_) => {}
        Err(e) => return failed(path, e),
    }

    let mut text = String::new();
    let mut line = Vec::new();
    let (mut number, mut shown, mut more) = (0u64, 0u64, false);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return failed(path, e),
        }
        number += 1;
        if number < first {
            continue;
        }
        // The first line is always returned, however long, so that a read
        // makes progress.
        if shown == max_lines || shown > 0 && text.len() + line.len() > READ_BYTES {
            more = true;
            break;
        }
        text.push_str(&String::from_utf8_lossy(&line));
        shown += 1;
    }
    if number == 0 {
        return Output::ok("(empty file)");
    }
    if shown == 0 {
        return Output::error(format!(
            "{} has {number} lines; there is no line {first}",
            path.display()
        ));
    }
    let last_shown = first + shown - 1;
    if more {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!(
            "[lines {first} to {last_shown} shown and the file goes on; continue with offset {}]",
            last_shown + 1
        ));
    }
    Output::ok(text)
}

pub struct ListDir;

struct List {
    path: PathBuf,
    class: Class,
}

impl Tool for ListDir {
    fn name(&self) -> &'static str {
        "list_dir"
    }

    fn description(&self) -> &'static str {
        "List what is in a directory: one entry per line, directories with a trailing slash, \
         files with their size."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The directory. Absolute, or ~/…"},
            },
            "required": ["path"],
        })
    }

    fn plan(&self, args: Value, ctx: &ToolCtx) -> Result<Box<dyn Action>, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            path: String,
        }
        let args: Args = parse_args(args)?;
        let path = locate(&args.path, ctx);
        Ok(Box::new(List {
            class: read_class(&path, &ctx.state),
            path,
        }))
    }
}

#[async_trait]
impl Action for List {
    fn class(&self) -> Class {
        self.class
    }

    fn summary(&self) -> String {
        format!("list {}", self.path.display())
    }

    async fn run(self: Box<Self>, _: &ToolCtx) -> Output {
        blocking(move || list(&self.path)).await
    }
}

fn list(path: &Path) -> Output {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) => return failed(path, e),
    };
    let mut lines: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry.file_type().ok()?;
            Some(if kind.is_dir() {
                format!("{name}/")
            } else if kind.is_symlink() {
                match std::fs::read_link(entry.path()) {
                    Ok(target) => format!("{name} -> {}", target.display()),
                    Err(_) => format!("{name} -> ?"),
                }
            } else {
                match entry.metadata() {
                    Ok(metadata) => format!("{name}  {}", size(metadata.len())),
                    Err(_) => name,
                }
            })
        })
        .collect();
    if lines.is_empty() {
        return Output::ok("(empty directory)");
    }
    lines.sort();
    let total = lines.len();
    if total > LIST_ENTRIES {
        lines.truncate(LIST_ENTRIES);
        lines.push(format!(
            "[{LIST_ENTRIES} of {total} entries shown; use search_files to find one by name]"
        ));
    }
    Output::ok(lines.join("\n"))
}

/// A size a person can read at a glance.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub struct SearchFiles;

struct Search {
    root: PathBuf,
    pattern: Regex,
    files: Option<Regex>,
    keys: PathBuf,
    class: Class,
}

impl Tool for SearchFiles {
    fn name(&self) -> &'static str {
        "search_files"
    }

    fn description(&self) -> &'static str {
        "Search the text files under a directory for a regular expression. Returns matching \
         lines as path:line: text. `files` narrows it to paths that match a second expression. \
         An empty `pattern` with `files` set finds files by name."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "A regular expression. (?i) makes it ignore case."},
                "path": {"type": "string", "description": "The directory or file to search. Absolute, or ~/…"},
                "files": {"type": "string", "description": "Only search files whose path matches this expression, such as \\.conf$"},
            },
            "required": ["pattern", "path"],
        })
    }

    fn plan(&self, args: Value, ctx: &ToolCtx) -> Result<Box<dyn Action>, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            pattern: String,
            path: String,
            files: Option<String>,
        }
        let args: Args = parse_args(args)?;
        let compile = |what: &str, expression: &str| {
            Regex::new(expression).map_err(|e| format!("`{what}` is not a regular expression: {e}"))
        };
        let files = args
            .files
            .as_deref()
            .map(|f| compile("files", f))
            .transpose()?;
        if args.pattern.is_empty() && files.is_none() {
            return Err("give a `pattern` to search for, or `files` to find files by name".into());
        }
        let root = locate(&args.path, ctx);
        Ok(Box::new(Search {
            class: read_class(&root, &ctx.state),
            pattern: compile("pattern", &args.pattern)?,
            files,
            keys: ctx.state.join(KEYS_DIR),
            root,
        }))
    }
}

#[async_trait]
impl Action for Search {
    fn class(&self) -> Class {
        self.class
    }

    fn summary(&self) -> String {
        let what = match (&self.files, self.pattern.as_str()) {
            (Some(files), "") => format!("find files matching {files}"),
            (Some(files), pattern) => format!("search for {pattern} in files matching {files}"),
            (None, pattern) => format!("search for {pattern}"),
        };
        format!("{what} under {}", self.root.display())
    }

    async fn run(self: Box<Self>, _: &ToolCtx) -> Output {
        blocking(move || search(&self)).await
    }
}

fn search(search: &Search) -> Output {
    // Resolved once, so every path the walk yields is a real one and can be
    // compared with the key store's real location.
    let root = match search.root.canonicalize() {
        Ok(root) => root,
        Err(e) => return failed(&search.root, e),
    };
    let keys = search
        .keys
        .canonicalize()
        .unwrap_or_else(|_| search.keys.clone());
    let names_only = search.pattern.as_str().is_empty();

    let walk = WalkDir::new(&root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            let skipped = entry.depth() > 0
                && entry.file_type().is_dir()
                && SEARCH_SKIPS.iter().any(|skip| entry.file_name() == *skip);
            !skipped && !entry.path().starts_with(&keys)
        });

    let mut found = Vec::new();
    let mut searched = 0usize;
    let mut more = false;
    'files: for entry in walk.filter_map(std::result::Result::ok) {
        // A symlink is not followed: it could lead anywhere, the key store
        // included.
        if !entry.file_type().is_file() {
            continue;
        }
        let shown = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        let shown = if shown.as_os_str().is_empty() {
            entry.path().to_string_lossy()
        } else {
            shown.to_string_lossy()
        };
        if search
            .files
            .as_ref()
            .is_some_and(|files| !files.is_match(&shown))
        {
            continue;
        }
        if names_only {
            if found.len() == SEARCH_MATCHES {
                more = true;
                break;
            }
            found.push(shown.into_owned());
            continue;
        }
        if entry
            .metadata()
            .map_or(true, |m| m.len() > SEARCH_FILE_BYTES)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if looks_binary(&bytes[..bytes.len().min(8192)]) {
            continue;
        }
        searched += 1;
        for (index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            if !search.pattern.is_match(line) {
                continue;
            }
            if found.len() == SEARCH_MATCHES {
                more = true;
                break 'files;
            }
            let text: String = line.trim().chars().take(SEARCH_LINE_CHARS).collect();
            found.push(format!("{shown}:{}: {text}", index + 1));
        }
    }
    if found.is_empty() {
        return Output::ok(if names_only {
            "no files match".to_string()
        } else {
            format!("no matches in {searched} files")
        });
    }
    if more {
        found.push(format!(
            "[stopped at {SEARCH_MATCHES}; narrow the pattern or the path to see the rest]"
        ));
    }
    Output::ok(found.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ToolCall;
    use crate::tools::Registry;

    struct Fixture {
        _dir: tempfile::TempDir,
        ctx: ToolCtx,
    }

    /// A home with a few files in it and a state directory holding a key.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let state = home.join(".tiphys");
        for sub in ["notes", "notes/.git", "notes/target", "empty"] {
            std::fs::create_dir_all(home.join(sub)).unwrap();
        }
        std::fs::create_dir_all(state.join("keys")).unwrap();
        std::fs::write(state.join("keys/work"), "sk-secret-123\n").unwrap();
        std::fs::write(home.join("notes/a.txt"), "alpha\nbeta needle\ngamma\n").unwrap();
        std::fs::write(home.join("notes/b.conf"), "port = 8080\n# needle here\n").unwrap();
        std::fs::write(home.join("notes/.git/config"), "needle in vcs\n").unwrap();
        std::fs::write(home.join("notes/target/out"), "needle in build\n").unwrap();
        std::fs::write(home.join("notes/blob.bin"), b"\x00\x01needle\x02").unwrap();
        std::os::unix::fs::symlink(state.join("keys/work"), home.join("notes/link")).unwrap();
        Fixture {
            ctx: ToolCtx {
                state,
                home: home.clone(),
                cwd: home,
            },
            _dir: dir,
        }
    }

    async fn run(fixture: &Fixture, tool: &str, args: Value) -> (Class, Output) {
        let call = ToolCall {
            id: "c".into(),
            name: tool.into(),
            arguments: args.to_string(),
        };
        let planned = Registry::builtin().plan(&call, &fixture.ctx).unwrap();
        let class = planned.action.class();
        (class, planned.action.run(&fixture.ctx).await)
    }

    async fn text(fixture: &Fixture, tool: &str, args: Value) -> String {
        let (class, output) = run(fixture, tool, args).await;
        assert_eq!(class, Class::Observe);
        assert!(output.ok, "{}", output.text);
        output.text
    }

    #[tokio::test]
    async fn a_file_is_read_by_any_way_of_naming_it() {
        let f = fixture();
        let absolute = f.ctx.home.join("notes/a.txt");
        for path in ["notes/a.txt", "~/notes/a.txt", absolute.to_str().unwrap()] {
            let got = text(&f, "read_file", json!({"path": path})).await;
            assert_eq!(got, "alpha\nbeta needle\ngamma\n", "{path}");
        }
    }

    #[tokio::test]
    async fn a_read_is_a_window_that_says_where_to_go_on() {
        let f = fixture();
        let long: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        std::fs::write(f.ctx.home.join("long.txt"), long).unwrap();

        let window = text(
            &f,
            "read_file",
            json!({"path": "long.txt", "offset": 10, "limit": 3}),
        )
        .await;
        assert_eq!(
            window,
            "line 10\nline 11\nline 12\n[lines 10 to 12 shown and the file goes on; continue with offset 13]"
        );
        let tail = text(&f, "read_file", json!({"path": "long.txt", "offset": 49})).await;
        assert_eq!(tail, "line 49\nline 50\n");

        let (_, past) = run(&f, "read_file", json!({"path": "long.txt", "offset": 99})).await;
        assert!(
            !past.ok && past.text.contains("has 50 lines"),
            "{}",
            past.text
        );
    }

    #[tokio::test]
    async fn a_read_stops_at_its_byte_budget_even_on_one_huge_line() {
        let f = fixture();
        let wide: String = (0..40).map(|_| format!("{}\n", "x".repeat(1999))).collect();
        std::fs::write(f.ctx.home.join("wide.txt"), wide).unwrap();
        let got = text(&f, "read_file", json!({"path": "wide.txt"})).await;
        assert!(got.len() < READ_BYTES + 200, "{}", got.len());
        assert!(
            got.ends_with("continue with offset 16]"),
            "{}",
            &got[got.len() - 80..]
        );

        // A single line longer than the budget is still returned, so a read
        // always makes progress.
        std::fs::write(f.ctx.home.join("one.txt"), "y".repeat(READ_BYTES * 2)).unwrap();
        let one = text(&f, "read_file", json!({"path": "one.txt"})).await;
        assert_eq!(one.len(), READ_BYTES * 2);
    }

    #[tokio::test]
    async fn what_cannot_be_read_as_text_says_so() {
        let f = fixture();
        std::fs::write(f.ctx.home.join("nothing"), "").unwrap();
        assert_eq!(
            text(&f, "read_file", json!({"path": "nothing"})).await,
            "(empty file)"
        );
        let binary = text(&f, "read_file", json!({"path": "notes/blob.bin"})).await;
        assert!(binary.contains("is not text (9 bytes)"), "{binary}");

        let (_, dir) = run(&f, "read_file", json!({"path": "notes"})).await;
        assert!(!dir.ok && dir.text.contains("is a directory"));
        let (_, missing) = run(&f, "read_file", json!({"path": "nope.txt"})).await;
        assert!(!missing.ok && missing.text.contains("nope.txt"));
    }

    #[tokio::test]
    async fn the_key_store_is_refused_before_anything_is_read() {
        let f = fixture();
        let refused = [
            ("read_file", json!({"path": "~/.tiphys/keys/work"})),
            ("read_file", json!({"path": "notes/link"})),
            ("read_file", json!({"path": "notes/../.tiphys/keys/work"})),
            ("list_dir", json!({"path": "~/.tiphys/keys"})),
            (
                "search_files",
                json!({"pattern": "sk-", "path": "~/.tiphys/keys"}),
            ),
        ];
        for (tool, args) in refused {
            let call = ToolCall {
                id: "c".into(),
                name: tool.into(),
                arguments: args.to_string(),
            };
            let planned = Registry::builtin().plan(&call, &f.ctx).unwrap();
            assert_eq!(planned.action.class(), Class::Never, "{tool} {args}");
        }
    }

    #[tokio::test]
    async fn a_search_from_above_the_key_store_never_looks_inside_it() {
        let f = fixture();
        let found = text(
            &f,
            "search_files",
            json!({"pattern": "sk-secret", "path": "~"}),
        )
        .await;
        assert_eq!(found, "no matches in 2 files");
        let names = text(
            &f,
            "search_files",
            json!({"pattern": "", "path": "~", "files": "work|link"}),
        )
        .await;
        assert_eq!(names, "no files match");
    }

    #[tokio::test]
    async fn a_listing_shows_kinds_and_sizes_in_order() {
        let f = fixture();
        let listing = text(&f, "list_dir", json!({"path": "notes"})).await;
        let link_target = f.ctx.state.join("keys/work");
        assert_eq!(
            listing,
            format!(
                ".git/\na.txt  24 B\nb.conf  26 B\nblob.bin  9 B\nlink -> {}\ntarget/",
                link_target.display()
            )
        );
        assert_eq!(
            text(&f, "list_dir", json!({"path": "empty"})).await,
            "(empty directory)"
        );
        let (_, missing) = run(&f, "list_dir", json!({"path": "nope"})).await;
        assert!(!missing.ok);
    }

    #[tokio::test]
    async fn a_search_finds_lines_and_skips_what_is_not_worth_reading() {
        let f = fixture();
        let found = text(
            &f,
            "search_files",
            json!({"pattern": "needle", "path": "notes"}),
        )
        .await;
        // Not the binary, not version control, not build output, not the link.
        assert_eq!(found, "a.txt:2: beta needle\nb.conf:2: # needle here");

        let narrowed = text(
            &f,
            "search_files",
            json!({"pattern": "needle", "path": "notes", "files": r"\.conf$"}),
        )
        .await;
        assert_eq!(narrowed, "b.conf:2: # needle here");

        let by_name = text(
            &f,
            "search_files",
            json!({"pattern": "", "path": "notes", "files": r"\.txt$"}),
        )
        .await;
        assert_eq!(by_name, "a.txt");

        let one_file = text(
            &f,
            "search_files",
            json!({"pattern": "(?i)ALPHA", "path": "notes/a.txt"}),
        )
        .await;
        assert!(one_file.ends_with("a.txt:1: alpha"), "{one_file}");

        let none = text(
            &f,
            "search_files",
            json!({"pattern": "nowhere", "path": "notes"}),
        )
        .await;
        assert_eq!(none, "no matches in 2 files");
    }

    #[tokio::test]
    async fn a_search_stops_at_its_limit_and_says_so() {
        let f = fixture();
        let many: String = (0..SEARCH_MATCHES + 50)
            .map(|n| format!("hit {n}\n"))
            .collect();
        std::fs::write(f.ctx.home.join("empty/many.txt"), many).unwrap();
        let found = text(
            &f,
            "search_files",
            json!({"pattern": "hit", "path": "empty"}),
        )
        .await;
        assert_eq!(found.lines().count(), SEARCH_MATCHES + 1);
        assert!(found.ends_with("to see the rest]"));
    }

    #[test]
    fn bad_arguments_are_explained_to_the_model() {
        let f = fixture();
        let cases = [
            ("read_file", json!({}), "missing field `path`"),
            (
                "read_file",
                json!({"path": "a", "lines": 3}),
                "unknown field `lines`",
            ),
            (
                "search_files",
                json!({"pattern": "(", "path": "."}),
                "`pattern` is not a regular expression",
            ),
            (
                "search_files",
                json!({"pattern": "", "path": "."}),
                "give a `pattern`",
            ),
        ];
        for (tool, args, expected) in cases {
            let call = ToolCall {
                id: "c".into(),
                name: tool.into(),
                arguments: args.to_string(),
            };
            let Err(message) = Registry::builtin().plan(&call, &f.ctx) else {
                panic!("{tool} {args} planned");
            };
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn sizes_read_at_a_glance() {
        let cases = [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (5 * 1024 * 1024, "5.0 MiB"),
            (3 * 1024 * 1024 * 1024, "3.0 GiB"),
        ];
        for (bytes, expected) in cases {
            assert_eq!(size(bytes), expected);
        }
    }
}
