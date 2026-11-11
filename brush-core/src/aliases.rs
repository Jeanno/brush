//! Alias expansion as bash does it: on the text of a command, before it is
//! parsed, so an alias's value may hold redirections, pipes, quotes and lists.

use std::collections::{HashMap, HashSet};

/// How deep aliases may expand into one another.
const MAX_DEPTH: usize = 32;

/// Words after which the next word is again in command position.
const RESERVED: &[&str] = &[
    "if", "then", "else", "elif", "do", "while", "until", "!", "{", "time", "fi", "done", "}",
];

/// Operators after which the next word is in command position.
const SEPARATORS: &[&str] = &[
    ";", ";;", ";&", ";;&", "&", "&&", "||", "|", "|&", "(", ")", "\n",
];

/// Whether `word` is an alias name as typed: unquoted, unescaped, no expansions.
fn is_bare(word: &str) -> bool {
    !word.is_empty()
        && word.chars().all(|c| {
            !matches!(
                c,
                '\\' | '\''
                    | '"'
                    | '$'
                    | '`'
                    | '='
                    | '/'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '|'
                    | '&'
                    | ';'
                    | ' '
                    | '\t'
                    | '\n'
            )
        })
}

/// `text` with the aliases in it expanded; `None` when nothing was expanded
/// (or the text cannot be tokenized yet, as an unfinished line).
pub(crate) fn expand(text: &str, aliases: &HashMap<String, String>) -> Option<String> {
    if aliases.is_empty() {
        return None;
    }
    let out = expand_with(text, aliases, &HashSet::new(), 0)?;
    (out != text).then_some(out)
}

fn expand_with(
    text: &str,
    aliases: &HashMap<String, String>,
    active: &HashSet<String>,
    depth: usize,
) -> Option<String> {
    let tokens = brush_parser::tokenize_str(text).ok()?;
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0; // chars of `text` already in `out`
    let mut command_position = true;
    // The word after a here-document operator is its delimiter, not a command.
    let mut after_heredoc = false;
    for token in &tokens {
        let span = token.location();
        let (start, end) = (span.start.index, span.end.index);
        match token {
            brush_parser::Token::Operator(op, _) => {
                after_heredoc = op == "<<" || op == "<<-";
                // A redirection operator keeps the command position for what
                // follows its target; its target is consumed below.
                command_position =
                    SEPARATORS.contains(&op.as_str()) || (command_position && is_redirection(op));
            }
            brush_parser::Token::Word(word, _) => {
                if after_heredoc {
                    after_heredoc = false;
                    continue;
                }
                if !command_position {
                    continue;
                }
                // An assignment before the command leaves the command position.
                if is_assignment(word) {
                    continue;
                }
                let replacement =
                    if is_bare(word) && !active.contains(word.as_str()) && depth < MAX_DEPTH {
                        aliases.get(word.as_str()).cloned()
                    } else {
                        None
                    };
                let Some(value) = replacement else {
                    command_position = RESERVED.contains(&word.as_str());
                    continue;
                };
                // The value's own first word is checked again, but never this
                // alias, so `alias ls='ls -G'` ends.
                let mut inner = active.clone();
                inner.insert(word.clone());
                let mut expanded = expand_with(&value, aliases, &inner, depth + 1)
                    .unwrap_or_else(|| value.clone());
                // Escaped so later expansion (at run time) leaves it be.
                if let Some(first) = first_word(&expanded) {
                    if inner.contains(first.as_str()) && aliases.contains_key(first.as_str()) {
                        let at = expanded.find(first.as_str()).unwrap_or(0);
                        expanded.insert(at, '\\');
                    }
                }
                out.extend(&chars[copied..start.min(chars.len())]);
                out.push_str(&expanded);
                copied = end.min(chars.len());
                // A value ending in a blank checks the next word too, as bash.
                command_position = value.ends_with(' ') || value.ends_with('\t');
            }
        }
    }
    out.extend(&chars[copied.min(chars.len())..]);
    Some(out)
}

/// A command of a parsed program, its aliases expanded and parsed again;
/// `None` when it has none to expand.
pub(crate) fn expand_command<SE: crate::extensions::ShellExtensions>(
    shell: &crate::Shell<SE>,
    command: &brush_parser::ast::CompleteCommand,
) -> Option<brush_parser::ast::Program> {
    if !shell.options().expand_aliases || shell.aliases().is_empty() {
        return None;
    }
    let text = command.to_string();
    let expanded = expand(&text, shell.aliases())?;
    shell.parse_string(expanded).ok()
}

fn is_redirection(op: &str) -> bool {
    op.contains('<') || op.contains('>')
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let name = name.strip_suffix('+').unwrap_or(name);
    let name = name.split('[').next().unwrap_or(name);
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn first_word(text: &str) -> Option<String> {
    brush_parser::tokenize_str(text)
        .ok()?
        .into_iter()
        .find_map(|t| match t {
            brush_parser::Token::Word(w, _) => Some(w),
            brush_parser::Token::Operator(..) => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn an_alias_is_its_text_before_parsing() {
        let a = aliases(&[
            ("e1", "echo hi 2>&1"),
            ("ll", "ls -la"),
            ("l", "ls | less"),
            ("g", "git commit -m \"wip it\""),
        ]);
        assert_eq!(expand("e1 there", &a).unwrap(), "echo hi 2>&1 there");
        assert_eq!(
            expand("ll /tmp; e1", &a).unwrap(),
            "ls -la /tmp; echo hi 2>&1"
        );
        assert_eq!(expand("true && l", &a).unwrap(), "true && ls | less");
        assert_eq!(expand("g -q", &a).unwrap(), "git commit -m \"wip it\" -q");
        assert_eq!(expand("echo ll", &a), None, "only in command position");
        assert_eq!(
            expand("'ll'; \\ll; \"ll\"", &a),
            None,
            "quoted names are not aliases"
        );
        assert_eq!(expand("X=1 ll", &a).unwrap(), "X=1 ls -la");
        assert_eq!(
            expand("if ll; then ll; fi", &a).unwrap(),
            "if ls -la; then ls -la; fi"
        );
        assert_eq!(
            expand("echo $(ll)", &a),
            None,
            "inside a substitution: parsed later"
        );
    }

    #[test]
    fn aliases_expand_into_one_another_and_end() {
        let a = aliases(&[
            ("ls", "ls -G"),
            ("ll", "ls -la"),
            ("loop1", "loop2 x"),
            ("loop2", "loop1 y"),
        ]);
        assert_eq!(expand("ll", &a).unwrap(), "\\ls -G -la");
        assert_eq!(expand("ls", &a).unwrap(), "\\ls -G");
        assert!(expand("loop1", &a).unwrap().contains("\\loop1 y x"));
    }

    #[test]
    fn a_value_ending_in_a_blank_checks_the_next_word() {
        let a = aliases(&[("sudo", "sudo "), ("ll", "ls -la")]);
        assert_eq!(expand("sudo ll", &a).unwrap(), "\\sudo  ls -la");
    }

    #[test]
    fn a_here_document_is_left_alone() {
        let a = aliases(&[("ll", "ls -la")]);
        assert_eq!(expand("cat <<ll\nhello\nll\n", &a), None);
    }
}
