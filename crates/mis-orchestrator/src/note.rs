//! Drafting a worker's purpose note after it gains tools: pure prompt
//! construction and reply cleanup. The shell runs the model.

/// A draft longer than this is cut; notes are a line or two.
const MAX_CHARS: usize = 400;

/// Prompt asking for an updated note from the old one, what is new, and all it has.
pub fn prompt(old: Option<&str>, new: &[String], tools: &str) -> String {
    let old = old.unwrap_or("(none yet)");
    let new = match new {
        [] => "(nothing new)".to_string(),
        n => n.join(", "),
    };
    format!(
        "A worker machine in a fleet of coding agents has a short note saying what it is \
         for; a planner reads it to decide which tasks to send there. It gained new tools. \
         Rewrite the note to cover them: keep what still holds, in the same voice, one or \
         two lines, no more than 300 characters. Reply with the note only: no quotes, no \
         preamble.\n\nCurrent note: {old}\nNew: {new}\nEverything it has: {tools}"
    )
}

/// The model's reply as a note: trimmed, unquoted, first paragraph, capped.
pub fn clean(reply: &str) -> String {
    let first = reply.trim().split("\n\n").next().unwrap_or("").trim();
    let unquoted = first
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim();
    crate::summary::cut(unquoted, MAX_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_carries_old_note_and_what_is_new() {
        let p = prompt(
            Some("Blender; 3D assets"),
            &["Houdini".into(), "houdini-mcp".into()],
            "apps: Blender, Houdini",
        );
        assert!(p.contains("Current note: Blender; 3D assets\nNew: Houdini, houdini-mcp\n"));
        assert!(p.ends_with("Everything it has: apps: Blender, Houdini"));
        assert!(prompt(None, &[], "").contains("Current note: (none yet)\nNew: (nothing new)"));
    }

    #[test]
    fn clean_keeps_the_note_only() {
        assert_eq!(
            clean("  \"Blender + Houdini; 3D assets\"\n"),
            "Blender + Houdini; 3D assets"
        );
        assert_eq!(clean("Note one.\n\nWhy I wrote it."), "Note one.");
        assert_eq!(clean(&"x".repeat(1000)).chars().count(), MAX_CHARS);
    }
}
