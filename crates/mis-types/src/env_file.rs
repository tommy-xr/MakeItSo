//! `KEY=value` env files (`~/.config/mis/orchestrator.env`), the subset a shell sourcing the
//! file reads the same: a line the shell would expand or split differently is an error, not a
//! guess.

/// `KEY=value` pairs in file order (a repeated key: the last wins when applied in order).
/// Skips blank lines and `#` comments; allows `export `, `'…'` and `"…"`. Err names the line,
/// never its value (it may be a token).
pub fn parse(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let at = |why: &str| format!("line {}: {why}", n + 1);
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line.split_once('=').ok_or_else(|| at("not KEY=value"))?;
        if !valid_key(key) {
            return Err(at("not a variable name"));
        }
        out.push((key.to_string(), value_of(value).ok_or_else(|| at(RULE))?));
    }
    Ok(out)
}

const RULE: &str = "quote the value; no `$`, backticks, backslashes or inline `#`";

fn valid_key(k: &str) -> bool {
    let mut chars = k.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The value a shell would assign, if it is literal text: `'…'` holds anything but `'`; `"…"`
/// and bare values hold nothing the shell expands; bare values also no spaces or `~`.
fn value_of(v: &str) -> Option<String> {
    let quoted = |q: char| {
        v.strip_prefix(q)?
            .strip_suffix(q)
            .filter(|s| !s.contains(q))
    };
    if let Some(s) = quoted('\'') {
        return Some(s.to_string());
    }
    let expands = |c: char| matches!(c, '$' | '`' | '\\' | '"' | '\'');
    if let Some(s) = quoted('"') {
        return (!s.chars().any(expands)).then(|| s.to_string());
    }
    let special = |c: char| {
        expands(c)
            || c.is_whitespace()
            || matches!(c, '#' | '~' | ';' | '&' | '|' | '<' | '>' | '(' | ')')
    };
    (!v.chars().any(special)).then(|| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn kv(text: &str) -> Vec<(String, String)> {
        parse(text).unwrap()
    }

    #[test]
    fn reads_the_shell_subset() {
        let text = "# c\n\nMIS_TOKEN='a b$c'\nexport MIS_BIND=\"0.0.0.0:7575\"\n  X=1=2\nE=\n";
        let pairs = [
            ("MIS_TOKEN", "a b$c"),
            ("MIS_BIND", "0.0.0.0:7575"),
            ("X", "1=2"),
            ("E", ""),
        ];
        let want: Vec<_> = pairs.map(|(k, v)| (k.into(), v.into())).into();
        assert_eq!(kv(text), want);
        assert_eq!(kv("A=1\nA=2").len(), 2);
    }

    #[test]
    fn rejects_what_a_shell_reads_differently() {
        for line in [
            "A=$HOME/x",
            "A=\"$HOME\"",
            "A=`id`",
            "A=a\\ b",
            "A=a #c",
            "A=a b",
            "A=~/x",
            "A='a'b'",
            "A='a' # c",
            "A=\"a",
            "1A=x",
            "A B=x",
            "just words",
        ] {
            let err = parse(&format!("OK=1\n{line}")).unwrap_err();
            assert!(err.starts_with("line 2: "), "{line}: {err}");
            assert!(!err.contains("HOME") && !err.contains("id"), "{err}");
        }
    }
}
