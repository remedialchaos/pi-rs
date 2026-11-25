use std::collections::{BTreeMap, HashMap, HashSet};

use pi_rs_chord::{Result, context::Context};
use pi_rs_durable::env::{ShellCommand, ShellExecOptions, ShellOutputStream};
use serde::Deserialize;
use serde_json::{Value, json};

use super::environment::{CHUNK, Workspace, invalid, valid_path};

const STATUS_LIMIT: usize = 5000;
const STATUS_BYTES: usize = CHUNK as usize;
const DIFF_BYTES: usize = 512 * 1024;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(super) enum Query {
    List {
        path: String,
    },
    File {
        path: String,
        commit: Option<String>,
    },
    Files,
    Status,
    Search {
        path: String,
        text: String,
        include: Option<String>,
        exclude: Option<String>,
    },
    Diff {
        path: String,
        #[serde(default)]
        full: bool,
        context: Option<u16>,
        commit: Option<String>,
    },
    Log {
        skip: u32,
        limit: usize,
    },
    Commit {
        commit: String,
    },
    Place,
}

#[derive(Default)]
pub(super) struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

impl Run {
    fn checked(self, fallback: &str) -> Result<Self> {
        if self.code == 0 || self.truncated {
            Ok(self)
        } else {
            Err(invalid(
                self.stderr
                    .trim()
                    .lines()
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(fallback)
                    .chars()
                    .take(300)
                    .collect::<String>(),
            ))
        }
    }

    fn records(&self) -> impl Iterator<Item = &str> {
        self.stdout
            .split_terminator('\0')
            .take(self.stdout.bytes().filter(|b| *b == 0).count())
    }
}

impl Workspace {
    async fn capture(&self, args: &[&str], limit: usize, cwd: &str, ctx: &Context) -> Result<Run> {
        let (context, cancel) = ctx.with_cancel();
        let mut run = Run::default();
        let variables = BTreeMap::from([
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
            ("GIT_NO_LAZY_FETCH".into(), "1".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_PAGER".into(), "cat".into()),
            ("LC_ALL".into(), "C".into()),
        ]);
        let mut output = |text: &str, _: &Context, info: pi_rs_durable::env::ShellOutputInfo| {
            let (output, limit) = if info.stream == ShellOutputStream::Stderr {
                (&mut run.stderr, 16384)
            } else {
                (&mut run.stdout, limit)
            };
            let available = limit.saturating_sub(output.len());
            if text.len() > available {
                output.push_str(&text[..text.floor_char_boundary(available)]);
                if info.stream == ShellOutputStream::Stdout {
                    run.truncated = true;
                    cancel.cancel("Output limit reached");
                }
            } else {
                output.push_str(text);
            }
            Ok(())
        };
        let result = self
            .env
            .exec(
                ShellCommand::Argv(args),
                Some(ShellExecOptions {
                    cwd: Some(cwd),
                    env: Some(&variables),
                    timeout: Some(10.),
                    on_output: Some(&mut output),
                    ..Default::default()
                }),
                &context,
            )
            .await;
        match result {
            Ok(result) => run.code = result.exit_code,
            Err(error) if !run.truncated => return Err(invalid(error)),
            Err(_) => {}
        }
        Ok(run)
    }

    pub(super) async fn git_at(
        &self,
        args: &[&str],
        limit: usize,
        cwd: &str,
        ctx: &Context,
    ) -> Result<Run> {
        let run = self
            .capture(
                &["git", "config", "--name-only", "--get-regexp", r"^filter\."],
                64 * 1024,
                cwd,
                ctx,
            )
            .await?;
        if run.truncated {
            return Err(invalid("Too many Git filters"));
        }
        let mut overrides = Vec::new();
        let mut drivers = HashSet::new();
        for driver in run.stdout.lines().filter_map(|line| {
            line.strip_prefix("filter.")?
                .rsplit_once('.')
                .map(|pair| pair.0)
        }) {
            if drivers.insert(driver) {
                for setting in ["clean=", "smudge=", "process=", "required=false"] {
                    overrides.push(format!("filter.{driver}.{setting}"));
                }
            }
        }
        let mut command = vec![
            "git",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=off",
            "-c",
            "core.pager=cat",
        ];
        if !matches!(args.first(), Some(&("check-ignore" | "grep"))) {
            command.push("--literal-pathspecs");
        }
        for value in &overrides {
            command.extend(["-c", value]);
        }
        command.extend_from_slice(args);
        self.capture(&command, limit, cwd, ctx).await
    }

    pub(super) async fn git(&self, args: &[&str], limit: usize, ctx: &Context) -> Result<Run> {
        self.git_at(args, limit, &self.root, ctx).await
    }

    pub(super) async fn query(&self, query: Query, ctx: &Context) -> Result<Value> {
        match query {
            Query::List { path } => self.list(&path, ctx).await,
            Query::File { path, commit: None } => {
                let (size, text) = self.preview(&path, CHUNK, ctx).await?;
                let mut out = json!({"kind":"file","path":path,"size":size,"binary":text.is_none(),"truncated":size>CHUNK});
                if let Some(text) = text {
                    out["text"] = json!(text);
                }
                Ok(out)
            }
            Query::File {
                path,
                commit: Some(commit),
            } => self.file_at(&path, &commit, ctx).await,
            Query::Files => {
                if let Ok(run) = self
                    .git(
                        &[
                            "ls-files",
                            "-z",
                            "--cached",
                            "--others",
                            "--exclude-standard",
                        ],
                        4 * STATUS_BYTES,
                        ctx,
                    )
                    .await
                    && run.code == 0
                {
                    let mut seen = HashSet::new();
                    let mut paths: Vec<_> = run
                        .records()
                        .filter(|path| valid_path(path) && seen.insert(*path))
                        .take(50_001)
                        .collect();
                    let truncated = run.truncated || paths.len() > 50_000;
                    paths.truncate(50_000);
                    return Ok(json!({"kind":"files","paths":paths,"truncated":truncated}));
                }
                self.walk(ctx).await
            }
            Query::Status => self.status(ctx).await,
            Query::Search {
                path,
                text,
                include,
                exclude,
            } => {
                self.search(
                    &path,
                    &text,
                    include.as_deref().unwrap_or(""),
                    exclude.as_deref().unwrap_or(""),
                    ctx,
                )
                .await
            }
            Query::Diff {
                path,
                full,
                context,
                commit,
            } => {
                let lines = if full {
                    1_000_000_000
                } else {
                    u32::from(context.unwrap_or(3))
                };
                self.diff(&path, full, lines, commit.as_deref(), ctx).await
            }
            Query::Log { skip, limit } => self.log(skip, limit, ctx).await,
            Query::Commit { commit } => self.commit_details(&commit, ctx).await,
            Query::Place => Err(invalid("Place is resolved by the workspace worker")),
        }
    }

    async fn status(&self, ctx: &Context) -> Result<Value> {
        let located = self.git(&["rev-parse", "--show-prefix"], 8192, ctx).await?;
        let prefix = if located.code == 0 {
            located.stdout.trim()
        } else {
            ""
        };
        let below = |path: &str| path.strip_prefix(prefix).unwrap_or(path).to_owned();
        let run = self
            .git(
                &[
                    "status",
                    "--porcelain=v2",
                    "-z",
                    "--branch",
                    "--untracked-files=all",
                    "--ignore-submodules=dirty",
                    "--no-ahead-behind",
                    "--",
                    ".",
                ],
                STATUS_BYTES,
                ctx,
            )
            .await?;
        if run.code != 0 {
            return Ok(
                json!({"kind":"status","repo":false,"reason":run.stderr.trim().lines().next().unwrap_or_default()}),
            );
        }
        let mut out = json!({"kind":"status","repo":true});
        let mut changes = Vec::new();
        let mut records = run.records();
        while let Some(record) = records.next() {
            if let Some(branch) = record.strip_prefix("# branch.head ") {
                if branch != "(detached)" {
                    out["branch"] = json!(branch);
                }
            } else if let Some(head) = record.strip_prefix("# branch.oid ") {
                if head != "(initial)" {
                    out["head"] = json!(head);
                }
            } else if let Some(path) = record.strip_prefix("? ") {
                changes.push(json!({"status":"??","path":below(path)}));
            } else {
                let fields = match record.as_bytes().first() {
                    Some(b'1') => 8,
                    Some(b'2') => 9,
                    Some(b'u') => 10,
                    _ => continue,
                };
                let Some(path) = record.splitn(fields + 1, ' ').nth(fields) else {
                    continue;
                };
                let Some(status) = record.get(2..4) else {
                    continue;
                };
                let mut row = json!({"status":status,"path":below(path)});
                if fields == 9 {
                    let Some(from) = records.next() else { break };
                    row["from"] = json!(below(from));
                }
                changes.push(row);
            }
            if changes.len() == STATUS_LIMIT {
                break;
            }
        }
        out["truncated"] = json!(run.truncated || changes.len() == STATUS_LIMIT);
        out["changes"] = json!(changes);
        Ok(out)
    }

    async fn file_at(&self, path: &str, commit: &str, ctx: &Context) -> Result<Value> {
        check_file_path(path)?;
        check_commit(commit)?;
        if !valid_path(path) {
            return Err(invalid("Invalid workspace path"));
        }
        let object = format!("{commit}:./{path}");
        let size = self.git(&["cat-file", "-s", &object], 64, ctx).await?;
        if size.code != 0 {
            return Err(invalid("File not found in that commit"));
        }
        let size = size.stdout.trim().parse::<u64>().map_err(invalid)?;
        let run = self
            .git(&["cat-file", "blob", &object], CHUNK as usize, ctx)
            .await?
            .checked("Cannot read file")?;
        let binary = run.stdout.bytes().take(8192).any(|byte| byte == 0);
        let mut out = json!({"kind":"file","path":path,"size":size,"binary":binary,"truncated":!binary && run.truncated});
        if !binary {
            out["text"] = json!(run.stdout);
        }
        Ok(out)
    }

    async fn diff(
        &self,
        path: &str,
        full: bool,
        lines: u32,
        commit: Option<&str>,
        ctx: &Context,
    ) -> Result<Value> {
        check_file_path(path)?;
        let base = if let Some(commit) = commit {
            check_commit(commit)?;
            commit.to_owned()
        } else {
            let head = self
                .git(&["rev-parse", "--verify", "-q", "HEAD"], 4096, ctx)
                .await?;
            let tree = if head.code == 0 {
                head
            } else {
                self.git(&["hash-object", "-t", "tree", "--stdin"], 4096, ctx)
                    .await?
                    .checked("Not a git repository")?
            };
            tree.stdout.trim().to_owned()
        };
        let mut args = if commit.is_some() {
            vec!["show", "--format=", "--diff-merges=first-parent"]
        } else {
            vec!["diff", "--ignore-submodules=dirty"]
        };
        let unified = format!("--unified={lines}");
        args.extend([
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "-M",
            &unified,
            &base,
            "--",
            path,
        ]);
        let mut run = self
            .git(&args, DIFF_BYTES, ctx)
            .await?
            .checked("Diff failed")?;
        if commit.is_none() && run.stdout.is_empty() && !run.truncated {
            let cached = self
                .git(&["ls-files", "--error-unmatch", "--", path], 4096, ctx)
                .await?;
            if cached.code != 0
                && let Ok((size, text)) = self.preview(path, 256 * 1024, ctx).await
            {
                let header = format!("diff --git a/{path} b/{path}\n");
                run.stdout = match text {
                    Some(text) => {
                        run.truncated = size > 256 * 1024;
                        let lines: Vec<_> = text
                            .strip_suffix('\n')
                            .unwrap_or(&text)
                            .split('\n')
                            .collect();
                        let added = lines
                            .iter()
                            .map(|line| format!("+{line}\n"))
                            .collect::<String>();
                        format!(
                            "{header}new file (untracked)\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n{added}",
                            lines.len()
                        )
                    }
                    None => format!("{header}new file\nBinary file b/{path}\n"),
                };
            }
        }
        Ok(
            json!({"kind":"diff","path":path,"full":full,"patch":run.stdout,"truncated":run.truncated}),
        )
    }

    async fn log(&self, skip: u32, limit: usize, ctx: &Context) -> Result<Value> {
        let run = self
            .git(
                &[
                    "log",
                    "-z",
                    "--format=%H%x1f%h%x1f%an%x1f%at%x1f%s",
                    &format!("--skip={skip}"),
                    "-n",
                    &(limit + 1).to_string(),
                    "HEAD",
                    "--",
                ],
                DIFF_BYTES,
                ctx,
            )
            .await?;
        if run.code != 0 {
            return Ok(json!({"kind":"log","commits":[],"more":false}));
        }
        let upstream = self
            .git(
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
                4096,
                ctx,
            )
            .await?;
        let ahead = if upstream.code == 0 {
            Some(
                self.git(
                    &[
                        "rev-list",
                        "-n",
                        "5000",
                        "HEAD",
                        "--not",
                        "@{upstream}",
                        "--",
                    ],
                    DIFF_BYTES,
                    ctx,
                )
                .await?,
            )
        } else {
            None
        };
        let hashes = ahead
            .as_ref()
            .filter(|run| run.code == 0)
            .map(|run| run.stdout.lines().collect::<HashSet<_>>());
        let mut records = run.records();
        let commits: Vec<_> = records.by_ref().take(limit).filter_map(|record| {
            let mut fields = record.splitn(5, '\x1f');
            let hash = fields.next()?;
            let mut row = json!({"hash":hash,"short":fields.next()?,"author":fields.next()?,"time":timestamp(fields.next()?),"subject":fields.next()?});
            if let Some(hashes) = &hashes { row["unpushed"] = json!(hashes.contains(hash)); }
            Some(row)
        }).collect();
        let mut out = json!({"kind":"log","commits":commits,"more":records.next().is_some()});
        if upstream.code == 0 {
            out["upstream"] = json!(upstream.stdout.trim());
        }
        Ok(out)
    }

    async fn commit_details(&self, commit: &str, ctx: &Context) -> Result<Value> {
        check_commit(commit)?;
        let meta = self
            .git(
                &[
                    "show",
                    "-s",
                    "--format=%H%x1f%an%x1f%at%x1f%B",
                    commit,
                    "--",
                ],
                65536,
                ctx,
            )
            .await?;
        if meta.code != 0 {
            return Err(invalid("Unknown commit"));
        }
        let mut fields = meta.stdout.splitn(4, '\x1f');
        let hash = fields.next().unwrap_or(commit);
        let author = fields.next().unwrap_or_default();
        let time = timestamp(fields.next().unwrap_or_default());
        let message = fields.next().unwrap_or_default().trim_end();
        let run = self
            .git(
                &[
                    "show",
                    "--format=",
                    "--name-status",
                    "--relative",
                    "-z",
                    "-M",
                    "--diff-merges=first-parent",
                    hash,
                    "--",
                ],
                STATUS_BYTES,
                ctx,
            )
            .await?
            .checked("Cannot read commit")?;
        let mut records = run.records().filter(|record| !record.is_empty());
        let mut changes = Vec::new();
        while let (Some(status), Some(path)) = (
            records.next().and_then(|s| s.chars().next()),
            records.next(),
        ) {
            let mut row = json!({"status":format!("{status}."),"path":path});
            if matches!(status, 'R' | 'C') {
                let Some(target) = records.next() else { break };
                row["path"] = json!(target);
                row["from"] = json!(path);
            }
            changes.push(row);
            if changes.len() == STATUS_LIMIT {
                break;
            }
        }
        Ok(
            json!({"kind":"commit","commit":hash,"author":author,"time":time,"message":message,"truncated":run.truncated || changes.len() == STATUS_LIMIT,"changes":changes}),
        )
    }

    async fn search(
        &self,
        path: &str,
        text: &str,
        include: &str,
        exclude: &str,
        ctx: &Context,
    ) -> Result<Value> {
        if !path.is_empty() {
            self.resolve(path, ctx).await?;
        }
        if text.trim().is_empty()
            || text.encode_utf16().count() > 200
            || text.contains(['\0', '\r', '\n'])
        {
            return Err(invalid("Invalid search text"));
        }
        let include = globs(include)?;
        let exclude = globs(exclude)?;
        let escaped = path.replace(['*', '?', '[', ']'], "\\$0");
        let escaped = if escaped == path {
            path.to_owned()
        } else {
            path.chars()
                .flat_map(|c| {
                    if matches!(c, '*' | '?' | '[' | ']') {
                        vec!['\\', c]
                    } else {
                        vec![c]
                    }
                })
                .collect()
        };
        let base = if path.is_empty() {
            String::new()
        } else {
            format!("{escaped}/")
        };
        let mut specs = Vec::new();
        if include.is_empty() && !path.is_empty() {
            specs.push(format!(":(literal){path}"));
        }
        for (patterns, magic) in [(include, "glob,icase"), (exclude, "glob,icase,exclude")] {
            for pattern in patterns {
                specs.push(format!(":({magic}){base}{pattern}"));
                specs.push(format!(":({magic}){base}{pattern}/**"));
            }
        }
        let mut args = vec![
            "grep",
            "--untracked",
            "-n",
            "-z",
            "-I",
            "-F",
            "--no-color",
            "--no-textconv",
            "--exclude-standard",
        ];
        if text == text.to_lowercase() {
            args.push("-i");
        }
        args.extend(["-e", text, "--"]);
        args.extend(specs.iter().map(String::as_str));
        let mut run = self.git(&args, 256 * 1024, ctx).await?;
        if run.code == 128 {
            args[1] = "--no-index";
            run = self.git(&args, 256 * 1024, ctx).await?;
        }
        if run.code > 1 {
            run = run.checked("Search failed")?;
        }
        let mut matches = Vec::new();
        let mut counts = HashMap::new();
        let mut tail = run.stdout.as_str();
        while let Some((name, rest)) = tail.split_once('\0')
            && let Some((line, rest)) = rest.split_once('\0')
            && let Some((line_text, rest)) = rest.split_once('\n')
        {
            tail = rest;
            let count = counts.entry(name).or_insert(0);
            if *count < 20
                && valid_path(name)
                && let Ok(line) = line.parse::<u64>()
            {
                matches.push(json!({"path":name,"line":line,"text":line_text.chars().take(300).collect::<String>()}));
                *count += 1;
            }
            if matches.len() == 500 {
                break;
            }
        }
        Ok(
            json!({"kind":"search","path":path,"text":text,"matches":matches,"truncated":run.truncated || matches.len() == 500}),
        )
    }
}

fn check_file_path(path: &str) -> Result<()> {
    if path.is_empty() || !valid_path(path) {
        return Err(invalid("Invalid workspace path"));
    }
    Ok(())
}

fn check_commit(commit: &str) -> Result<()> {
    if !(7..=64).contains(&commit.len())
        || !commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("Invalid commit"));
    }
    Ok(())
}

fn timestamp(value: &str) -> i64 {
    value
        .parse::<i64>()
        .unwrap_or_default()
        .saturating_mul(1000)
}

fn globs(value: &str) -> Result<Vec<String>> {
    if value.encode_utf16().count() > 500 || value.chars().any(|c| c < ' ' || c == '\\') {
        return Err(invalid("Invalid file pattern"));
    }
    fn split(value: &str) -> Result<Vec<&str>> {
        let mut depth = 0;
        let mut start = 0;
        let mut parts = Vec::new();
        for (at, c) in value.char_indices() {
            match c {
                '{' => depth += 1,
                '}' if depth == 0 => return Err(invalid("Invalid file pattern")),
                '}' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(&value[start..at]);
                    start = at + 1;
                }
                _ => {}
            }
        }
        if depth != 0 {
            return Err(invalid("Invalid file pattern"));
        }
        parts.push(&value[start..]);
        Ok(parts)
    }
    fn expand(pattern: &str, out: &mut Vec<String>) -> Result<()> {
        if let Some(open) = pattern.find('{') {
            let mut depth = 1;
            let close = pattern[open + 1..]
                .char_indices()
                .find_map(|(at, c)| {
                    if c == '{' {
                        depth += 1;
                    }
                    if c == '}' {
                        depth -= 1;
                    }
                    (depth == 0).then_some(open + 1 + at)
                })
                .ok_or_else(|| invalid("Invalid file pattern"))?;
            for part in split(&pattern[open + 1..close])? {
                expand(
                    &format!("{}{part}{}", &pattern[..open], &pattern[close + 1..]),
                    out,
                )?;
            }
        } else {
            if out.len() == 32 {
                return Err(invalid("Invalid file pattern"));
            }
            out.push(pattern.to_owned());
        }
        Ok(())
    }
    let mut patterns = Vec::new();
    for pattern in split(value)?
        .into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        expand(pattern, &mut patterns)?;
    }
    let mut normalized = Vec::new();
    for pattern in patterns {
        let segments: Vec<_> = pattern
            .split('/')
            .filter(|s| !matches!(*s, "" | "."))
            .collect();
        if segments.contains(&"..") {
            return Err(invalid("Invalid file pattern"));
        }
        if segments.is_empty() {
            continue;
        }
        let glob = segments.join("/");
        let glob = if pattern.starts_with('/')
            || pattern.starts_with("./")
            || glob == "**"
            || glob.starts_with("**/")
        {
            glob
        } else {
            format!("**/{glob}")
        };
        if !normalized.contains(&glob) {
            normalized.push(glob);
        }
    }
    Ok(normalized)
}
