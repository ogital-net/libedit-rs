//! A network-device-style CLI built on the `libedit` safe wrapper.
//!
//! Run it with:
//!
//! ```sh
//! cargo run --example repl
//! ```
//!
//! Features demonstrated:
//! - Reading lines with a colored prompt (`EditLine::readline`)
//! - Command history with up/down recall, dedup, and persistence
//! - Tab completion of commands (`EditLine::set_completer`)
//! - Fish-style inline ghost-text suggestions (`EditLine::set_suggester`)
//! - Styled ambiguous-candidate listing (`EditLine::set_candidate_styler`)
//! - Right-margin help text (`EditLine::set_hinter`)
//! - Juniper-style `?` context help via `add_action` + `bind_key`
//! - Ctrl-C / Ctrl-D distinction (`Error::Interrupted` vs EOF)
//! - Signal handling for terminal resize (`set_signal_handling`)
//! - Unified error handling via `libedit::Result`
//!
//! Built-in commands: `show`, `set`, `help`, `history`, `clear`, `quit`.

use libedit::hint::Hint;
use libedit::suggestion::Suggestion;
use libedit::term::supports_color;
use libedit::{
    Action, ActionContext, Completer, Completion, EditLine, Error, History, LineContext, Result,
    Tokenizer,
};
use std::io::Write;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Command table
// ---------------------------------------------------------------------------

/// Each command has a name and a one-line description. In a real appliance CLI
/// this would be a hierarchical prefix trie (e.g. the `command-trie` crate)
/// with sub-commands, argument specs, etc.
const COMMANDS: &[(&str, &str)] = &[
    ("show", "display operational state"),
    ("set", "configure a parameter"),
    ("help", "show available commands"),
    ("history", "show stored history entries"),
    ("clear", "clear the command history"),
    (
        "caf\u{00e9}",
        "serve a caf\u{00e9} — UTF-8 in commands works",
    ),
    ("quit", "exit the CLI"),
    ("exit", "exit the CLI"),
];

// ---------------------------------------------------------------------------
// Completer
// ---------------------------------------------------------------------------

/// Tab-completes the first token against the command table.
struct CommandCompleter;

impl Completer for CommandCompleter {
    fn complete(&mut self, ctx: &LineContext) -> Completion {
        // Only complete the command word (before the first space).
        if ctx.line()[..ctx.cursor()].contains(char::is_whitespace) {
            return Completion::none();
        }
        let word = ctx.word();
        let matches: Vec<String> = COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(word))
            .map(|(name, _)| name.to_string())
            .collect();
        Completion::new(matches)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the history file path (`$HOME/.repl_history`).
fn history_path() -> PathBuf {
    let mut p = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    p.push(".repl_history");
    p
}

/// Print the commands matching `prefix` in Juniper-style aligned columns.
fn print_context_help(out: &mut impl Write, prefix: &str) {
    let matches: Vec<_> = COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .collect();
    if matches.is_empty() {
        let _ = writeln!(out, "\n  (no completions)");
        return;
    }
    let _ = writeln!(out, "\nPossible completions:");
    let max_name = matches.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, desc) in &matches {
        let _ = writeln!(
            out,
            "  \x1b[1;36m{:<width$}\x1b[0m  {desc}",
            name,
            width = max_name
        );
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let mut editor = EditLine::new_with_locale("cli")?;
    let mut history = History::with_size(1000);
    let mut tokenizer = Tokenizer::new(None)?;
    let color = supports_color();

    // -- History --
    history.set_unique(true);
    let hist_path = history_path();
    let _ = history.load(&hist_path);
    editor.set_history(&mut history)?;

    // -- Tab completion --
    editor.set_completer(CommandCompleter)?;
    if color {
        editor.set_candidate_styler(|cand: &str, out: &mut String| {
            use std::fmt::Write as _;
            let _ = write!(out, "\x1b[1;36m{cand}\x1b[0m");
        });
    }

    // -- Inline ghost-text suggestion (fish-style) --
    // Only enable when the terminal supports color -- without faint/dim styling
    // the ghost text is indistinguishable from real input.
    if color {
        editor.set_suggester(|ctx: &LineContext| {
            let line = ctx.line();
            if line.is_empty() {
                return None;
            }
            let mut iter = COMMANDS
                .iter()
                .filter(|(name, _)| name.starts_with(line) && name.len() > line.len());
            let (name, _) = iter.next()?;
            if iter.next().is_some() {
                return None;
            }
            Some(Suggestion::new(&name[line.len()..]))
        })?;
        editor.set_suggestion_style("\x1b[2m", "\x1b[0m");
    }

    // -- Right-margin hint (command description) --
    editor.set_hinter(|ctx: &LineContext| {
        // After Tab completion adds a trailing space, `word()` is empty -- fall
        // back to the first token of the line so the hint persists.
        let lookup = if ctx.word().is_empty() {
            ctx.line().split_ascii_whitespace().next().unwrap_or("")
        } else {
            ctx.word()
        };
        if lookup.is_empty() {
            return None;
        }
        let mut iter = COMMANDS.iter().filter(|(name, _)| name.starts_with(lookup));
        let first = iter.next()?;
        if iter.next().is_some() {
            return None;
        }
        Some(Hint::new(format!("-- {}", first.1)))
    });

    // -- Juniper-style '?' context help --
    let help_action = editor.add_action("context-help", |ctx: &ActionContext| {
        let mut out = ctx.output();
        print_context_help(&mut out, ctx.word());
        out.flush().unwrap();
        Action::Redisplay
    })?;
    editor.bind_key("?", &help_action)?;

    // -- Editor settings --
    editor.set_signal_handling(true)?;

    // -- Banner --
    if color {
        println!(
            "\x1b[1;36mcli\x1b[0m -- type \x1b[1m?\x1b[0m for help, Tab to complete, Ctrl-D to exit"
        );
    } else {
        println!("cli -- type ? for help, Tab to complete, Ctrl-D to exit");
    }
    println!("(ghost text suggests -- accept with Ctrl-F or ->)\n");

    // -- REPL loop --
    let prompt = if color {
        "\x1b[1;32mcli>\x1b[0m "
    } else {
        "cli> "
    };
    loop {
        let line = match editor.readline(prompt) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(Error::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(e) => return Err(e),
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        history.add(trimmed)?;
        tokenizer.reset();
        let words = tokenizer.tokenize(trimmed)?;

        match words.first().map(String::as_str) {
            Some("quit") | Some("exit") => break,
            Some("help") => {
                let max = COMMANDS.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
                for (name, desc) in COMMANDS {
                    println!("  \x1b[1;36m{:<width$}\x1b[0m  {desc}", name, width = max);
                }
            }
            Some("history") => {
                if history.is_empty() {
                    println!("(no history)");
                } else {
                    println!("{} entries", history.len());
                    if let Ok(latest) = history.first() {
                        println!("most recent: {latest}");
                    }
                }
            }
            Some("clear") => {
                history.clear();
                println!("history cleared");
            }
            Some("caf\u{00e9}") => println!("(caf\u{00e9}: not implemented in this demo)"),
            Some("show") => println!("(show: not implemented in this demo)"),
            Some("set") => println!("(set: not implemented in this demo)"),
            Some(cmd) => {
                println!("\x1b[31munknown command:\x1b[0m `{cmd}` -- press ? for help");
            }
            None => {}
        }
    }

    if let Err(e) = history.save(&hist_path) {
        eprintln!("warning: could not save history: {e}");
    }
    println!("bye!");
    Ok(())
}
