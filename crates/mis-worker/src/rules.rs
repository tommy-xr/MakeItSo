//! Ask mode: how a tool call the agent wants reads to the user, which rule
//! "always" saves for it, and whether rules saved since the run began cover it.

use std::path::{Path, PathBuf};

use mis_types::{first_line, Permissions, MAX_APPROVAL_DETAIL};
use serde_json::Value;

/// Longest summary shown on the page and in the push.
const MAX_SUMMARY_CHARS: usize = 160;

/// A rule "always" adds to `[permissions]`.
#[derive(Debug, Clone, PartialEq)]
pub enum Rule {
    /// Writes in this canonical directory.
    Folder(String),
    /// Bash commands starting with this program.
    Command(String),
    /// Every tool of this MCP server.
    Mcp(String),
}

/// File tools and the input field naming the file they write.
const WRITES: [(&str, &str); 4] = [
    ("Write", "file_path"),
    ("Edit", "file_path"),
    ("MultiEdit", "file_path"),
    ("NotebookEdit", "notebook_path"),
];

/// `~/x` for paths under `home`.
fn tilde(path: &str, home: Option<&Path>) -> String {
    match home.and_then(|h| Path::new(path).strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.to_string(),
    }
}

/// e.g. "Write ~/assets/crate.py", "Bash: python3 make_crate.py",
/// "blender: execute_blender_code"; one line, clipped.
pub fn summary(tool: &str, input: &Value, home: Option<&Path>) -> String {
    let s = match (
        WRITES.iter().find(|(t, _)| *t == tool),
        tool.strip_prefix("mcp__"),
    ) {
        (Some((_, field)), _) => {
            format!(
                "{tool} {}",
                tilde(input[*field].as_str().unwrap_or("?"), home)
            )
        }
        (_, Some(rest)) => rest.replacen("__", ": ", 1),
        _ if tool == "Bash" => {
            let cmd = input["command"].as_str().unwrap_or("").trim();
            let more = cmd.lines().count().saturating_sub(1);
            match more {
                0 => format!("Bash: {cmd}"),
                n => format!(
                    "Bash: {} (+{n} line{})",
                    first_line(cmd, MAX_SUMMARY_CHARS),
                    if n == 1 { "" } else { "s" }
                ),
            }
        }
        // The argument that says what the call is about, else the first string one.
        _ => {
            let arg = ["url", "query", "path", "pattern", "prompt"]
                .iter()
                .find_map(|k| input[*k].as_str())
                .or_else(|| input.as_object()?.values().find_map(|v| v.as_str()))
                .unwrap_or("");
            format!("{tool} {arg}")
        }
    };
    first_line(s.trim(), MAX_SUMMARY_CHARS)
}

/// The whole call for the page: a Bash command as is, else the input as
/// pretty JSON; clipped to `MAX_APPROVAL_DETAIL` chars, saying how much is cut.
pub fn detail(tool: &str, input: &Value) -> String {
    let full = match (tool, input["command"].as_str()) {
        ("Bash", Some(cmd)) => cmd.to_string(),
        _ => serde_json::to_string_pretty(input).unwrap_or_default(),
    };
    let n = full.chars().count();
    match n > MAX_APPROVAL_DETAIL {
        false => full,
        true => {
            let kept: String = full.chars().take(MAX_APPROVAL_DETAIL).collect();
            format!("{kept}\n… ({} more chars)", n - MAX_APPROVAL_DETAIL)
        }
    }
}

/// The real directory a file tool writes in: the file resolved through any
/// symlinks if it exists, else its parent; None for other tools, a dangling
/// symlink, or a directory that doesn't exist yet.
fn write_dir(tool: &str, input: &Value) -> Option<PathBuf> {
    let (_, field) = WRITES.iter().find(|(t, _)| *t == tool)?;
    let file = Path::new(input[*field].as_str()?);
    let real = match file.symlink_metadata() {
        Ok(_) => file.canonicalize().ok()?,
        Err(_) => file.parent()?.canonicalize().ok()?.join(file.file_name()?),
    };
    real.parent().map(Path::to_path_buf).filter(|d| d.is_dir())
}

/// A Bash command's program when the command is one plain call, e.g.
/// "python3" for "python3 -c 'print(6*7)'"; None for pipes, chains,
/// redirects, substitutions, comments or env assignments outside single
/// quotes, or any newline, which one prefix rule can't cover.
fn program(input: &Value) -> Option<&str> {
    let cmd = input["command"].as_str()?.trim();
    if cmd.contains(['\n', '\r']) {
        return None;
    }
    let mut quote = None;
    for c in cmd.chars() {
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => {}
            // Double quotes still expand `$` and backticks.
            (Some(_), '$' | '`' | '\\') => return None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '&' | '|' | ';' | '<' | '>' | '`' | '$' | '(' | ')' | '#' | '\\') => {
                return None
            }
            (None, _) => {}
        }
    }
    let first = cmd.split_whitespace().next()?;
    (quote.is_none() && !first.contains(['=', '\'', '"'])).then_some(first)
}

/// The MCP server of an `mcp__<server>__<tool>` name.
fn server(tool: &str) -> Option<&str> {
    tool.strip_prefix("mcp__")?.split("__").next()
}

/// What "always" saves for this call, if a rule fits: the written file's
/// directory (never home or above it), the command's program, or the MCP
/// server. Each must pass `mis_agent::check`.
pub fn always(tool: &str, input: &Value, home: Option<&Path>) -> Option<Rule> {
    let rule = if let Some(dir) = write_dir(tool, input) {
        if home.is_some_and(|h| h.starts_with(&dir)) {
            return None;
        }
        Rule::Folder(dir.display().to_string())
    } else if tool == "Bash" {
        Rule::Command(program(input)?.to_string())
    } else {
        Rule::Mcp(
            server(tool)
                .filter(|s| *s != mis_agent::MCP_SERVER)?
                .to_string(),
        )
    };
    let mut p = Permissions::default();
    add(&mut p, &rule);
    mis_agent::check(&p).ok().map(|()| rule)
}

/// How the page words `rule`, e.g. "writes in ~/assets".
pub fn describe(rule: &Rule, home: Option<&Path>) -> String {
    match rule {
        Rule::Folder(d) => format!("writes in {}", tilde(d, home)),
        Rule::Command(c) => format!("commands starting {c}"),
        Rule::Mcp(s) => format!("every {s} tool"),
    }
}

/// Add `rule` to `p`'s lists, once.
pub fn add(p: &mut Permissions, rule: &Rule) {
    let (list, v) = match rule {
        Rule::Folder(d) => (&mut p.folders, d),
        Rule::Command(c) => (&mut p.commands, c),
        Rule::Mcp(s) => (&mut p.mcp, s),
    };
    if !list.contains(v) {
        list.push(v.clone());
    }
}

/// Whether `p` allows this call: claude's own rules are fixed when it starts,
/// so one saved by "always" mid-run is applied here.
pub fn covered(p: &Permissions, tool: &str, input: &Value) -> bool {
    if let Some(dir) = write_dir(tool, input) {
        return p.folders.iter().any(|f| dir.starts_with(f));
    }
    if tool == "Bash" {
        let cmd = input["command"].as_str().unwrap_or("").trim();
        return program(input).is_some()
            && (p.commands.iter()).any(|c| cmd == c || cmd.starts_with(&format!("{c} ")));
    }
    server(tool).is_some_and(|s| p.mcp.iter().any(|m| m == s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Fresh canonical dir with `assets/`; removed on drop.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Self {
            let d = std::env::temp_dir().join(format!("mis-rules-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(d.join("assets")).unwrap();
            Tmp(d.canonicalize().unwrap())
        }
        fn path(&self, p: &str) -> String {
            self.0.join(p).display().to_string()
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn summaries_are_short_and_name_the_target() {
        let home = Path::new("/Users/me");
        let s = |tool, input| summary(tool, &input, Some(home));
        assert_eq!(
            s(
                "Write",
                json!({"file_path": "/Users/me/assets/crate.py", "content": "x"})
            ),
            "Write ~/assets/crate.py"
        );
        assert_eq!(
            s("Bash", json!({"command": "python3 make_crate.py\nrm x"})),
            "Bash: python3 make_crate.py (+1 line)"
        );
        assert_eq!(
            s(
                "mcp__blender__execute_blender_code",
                json!({"code": "import bpy"})
            ),
            "blender: execute_blender_code"
        );
        assert_eq!(
            s("WebFetch", json!({"url": "https://x.dev"})),
            "WebFetch https://x.dev"
        );
        // More lines than the summary shows: it says so, and the detail has them all.
        let two = json!({"command": "echo safe\nrm -rf ~/x"});
        assert_eq!(s("Bash", two.clone()), "Bash: echo safe (+1 line)");
        assert_eq!(detail("Bash", &two), "echo safe\nrm -rf ~/x");
        let code = json!({"code": "import bpy\nbpy.ops.wm.quit_blender()"});
        assert!(detail("mcp__blender__run", &code).contains("quit_blender"));
        let big = detail("Bash", &json!({"command": "y".repeat(5000)}));
        assert!(big.ends_with("… (1000 more chars)"), "{big}");
        let long = s("Bash", json!({"command": "x".repeat(500)}));
        assert!(long.chars().count() <= MAX_SUMMARY_CHARS, "{long}");
    }

    #[test]
    fn always_saves_the_narrowest_rule_that_fits() {
        let t = Tmp::new();
        let home = Some(t.0.as_path());
        let write = |p: &str| json!({"file_path": p});
        assert_eq!(
            always("Write", &write(&t.path("assets/crate.py")), home),
            Some(Rule::Folder(t.path("assets")))
        );
        // `..` and symlinks resolve to the real directory.
        #[cfg(unix)]
        std::os::unix::fs::symlink(t.0.join("assets"), t.0.join("link")).unwrap();
        #[cfg(unix)]
        assert_eq!(
            always("Edit", &write(&t.path("link/../assets/x")), home),
            Some(Rule::Folder(t.path("assets")))
        );
        // A file that links elsewhere counts where it lands; a dangling link not at all.
        #[cfg(unix)]
        {
            std::fs::create_dir_all(t.0.join("out")).unwrap();
            std::fs::write(t.0.join("out/secret"), "s").unwrap();
            std::os::unix::fs::symlink(t.0.join("out/secret"), t.0.join("assets/s")).unwrap();
            std::os::unix::fs::symlink(t.0.join("nowhere"), t.0.join("assets/d")).unwrap();
            let landed = always("Edit", &write(&t.path("assets/s")), home);
            assert_eq!(landed, Some(Rule::Folder(t.path("out"))));
            assert_eq!(always("Write", &write(&t.path("assets/d")), home), None);
        }
        // Home itself, `/`, or a directory that doesn't exist yet: no rule.
        assert_eq!(always("Write", &write(&t.path("x.txt")), home), None);
        assert_eq!(always("Write", &write("/x.txt"), None), None);
        assert_eq!(always("Write", &write(&t.path("new/x.txt")), home), None);
        let bash = |c: &str| always("Bash", &json!({"command": c}), home);
        assert_eq!(
            bash("python3 make.py"),
            Some(Rule::Command("python3".into()))
        );
        for c in [
            "cd x && rm y",
            "ls | sh",
            "echo $(id)",
            "A=1 python3",
            "x > y",
            "x;y",
            "echo \"$(id)\"",
            "python3 'unclosed",
            "'py'thon3 x",
            "echo safe # '\nprintf ran\n# '",
            "echo safe # x",
            "python3 -c 'a\nb'",
            "",
        ] {
            assert_eq!(bash(c), None, "{c}");
        }
        let quoted = bash("python3 -c 'print(6*7); x = \"a|b\"'");
        assert_eq!(quoted, Some(Rule::Command("python3".into())));
        let mcp = |t: &str| always(t, &json!({}), home);
        assert_eq!(mcp("mcp__blender__run"), Some(Rule::Mcp("blender".into())));
        assert_eq!(mcp("mcp__mis__report_status"), None);
        assert_eq!(mcp("mcp__a.b__run"), None);
        assert_eq!(mcp("WebFetch"), None);
    }

    #[test]
    fn rules_added_mid_run_cover_matching_calls_only() {
        let t = Tmp::new();
        let mut p = Permissions::default();
        for r in [
            Rule::Folder(t.path("assets")),
            Rule::Command("python3".into()),
            Rule::Mcp("blender".into()),
            Rule::Command("python3".into()),
        ] {
            add(&mut p, &r);
        }
        assert_eq!(p.commands, ["python3"]);
        let c = |tool, input| covered(&p, tool, &input);
        assert!(c("Write", json!({"file_path": t.path("assets/a.py")})));
        #[cfg(unix)]
        {
            std::fs::write(t.0.join("secret"), "s").unwrap();
            std::os::unix::fs::symlink(t.0.join("secret"), t.0.join("assets/link")).unwrap();
            assert!(!c("Edit", json!({"file_path": t.path("assets/link")})));
        }
        assert!(!c("Write", json!({"file_path": t.path("b.py")})));
        assert!(!c("Write", json!({"file_path": t.path("assets/../b.py")})));
        assert!(c("Bash", json!({"command": "python3 a.py"})));
        assert!(c("Bash", json!({"command": "python3"})));
        assert!(!c("Bash", json!({"command": "python3x a.py"})));
        assert!(!c("Bash", json!({"command": "python3 a.py && rm -rf /"})));
        assert!(!c("Bash", json!({"command": "python3 'a' ; rm x"})));
        assert!(c("Bash", json!({"command": "python3 -c 'print(7*8)'"})));
        assert!(c("mcp__blender__run", json!({})));
        assert!(!c("mcp__other__run", json!({})));
        assert!(!c("WebFetch", json!({"url": "https://x"})));
    }
}
