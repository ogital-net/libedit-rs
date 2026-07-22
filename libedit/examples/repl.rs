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
//! - Tab completion of commands (`EditLine::set_complete_handler`)
//! - Juniper-style `?` context help (`EditLine::set_help_handler`)
//! - Ctrl-C / Ctrl-D distinction (`Error::Interrupted` vs EOF)
//! - Signal handling for terminal resize (`set_signal_handling`)
//! - Bracketed paste (on by default): pasted text -- even with newlines or
//!   Tabs -- is inserted literally instead of triggering completion/submit.
//!   Try pasting `echo one two three` or a multi-line snippet.
//! - Unified error handling via `libedit::Result`
//!
//! Built-in commands: `echo`, `show`, `set`, `help`, `history`, `clear`, `quit`.

use libedit::editline::Hinter;
use libedit::term::supports_color;
use libedit::{Action, EditLine, Error, EventHandler, History, LineContext, Result, Tokenizer};
use std::path::PathBuf;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Command table
// ---------------------------------------------------------------------------

/// Each command has a name and a one-line description. In a real appliance CLI
/// this would be a hierarchical prefix trie (e.g. the `command-trie` crate)
/// with sub-commands, argument specs, etc.
const COMMANDS: &[(&str, &str)] = &[
    ("echo", "print the rest of the line (try pasting into it)"),
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
// Word helper
// ---------------------------------------------------------------------------

/// Extract the word under the cursor (everything from the last whitespace
/// before the cursor to the cursor position).
fn word_at_cursor(line: &str, cursor: usize) -> &str {
    let before = &line[..cursor];
    let start = before.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    &before[start..]
}

// ---------------------------------------------------------------------------
// Completion handler
// ---------------------------------------------------------------------------

/// Tab-completes the first token against the command table.
struct CommandCompleter;

impl EventHandler for CommandCompleter {
    fn handle(
        &self,
        ctx: &mut LineContext,
        insert_writer: &mut dyn std::fmt::Write,
        output_writer: &mut dyn std::io::Write,
    ) -> Action {
        let word = word_at_cursor(ctx.line(), ctx.cursor());

        // Only complete the command word (before the first space).
        if ctx.line()[..ctx.cursor()].contains(char::is_whitespace) {
            return Action::Norm;
        }

        let matches: Vec<&str> = COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(word))
            .map(|(name, _)| *name)
            .collect();

        match matches.len() {
            0 => Action::RefreshBeep,
            1 => {
                // Single match — insert the remaining suffix plus a trailing space.
                let suffix = &matches[0][word.len()..];
                if write!(insert_writer, "{suffix} ").is_err() {
                    return Action::Error;
                }
                Action::Refresh
            }
            _ => {
                // Multiple matches — list them and redisplay.
                let _ = writeln!(output_writer);
                let max_name = matches.iter().map(|n| n.len()).max().unwrap_or(0);
                for name in &matches {
                    let _ = write!(
                        output_writer,
                        "  \x1b[1;36m{:<width$}\x1b[0m",
                        name,
                        width = max_name
                    );
                }
                let _ = writeln!(output_writer);
                let _ = output_writer.flush();
                Action::Redisplay
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Help handler
// ---------------------------------------------------------------------------

/// Prints Juniper-style context help for `?`.
struct HelpHandler;

impl EventHandler for HelpHandler {
    fn handle(
        &self,
        ctx: &mut LineContext,
        _insert_writer: &mut dyn std::fmt::Write,
        output_writer: &mut dyn std::io::Write,
    ) -> Action {
        let word = word_at_cursor(ctx.line(), ctx.cursor());
        let matches: Vec<_> = COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(word))
            .collect();

        if matches.is_empty() {
            let _ = writeln!(output_writer, "\n  (no completions)");
        } else {
            let _ = writeln!(output_writer, "\nPossible completions:");
            let max_name = matches.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
            for (name, desc) in &matches {
                let _ = writeln!(
                    output_writer,
                    "  \x1b[1;36m{:<width$}\x1b[0m  {desc}",
                    name,
                    width = max_name
                );
            }
        }
        let _ = output_writer.flush();
        Action::Redisplay
    }
}

// ---------------------------------------------------------------------------
// Hinter
// ---------------------------------------------------------------------------
struct CommandHinter;

impl Hinter for CommandHinter {
    fn hint(&self, line_ctx: &mut LineContext, writer: &mut dyn std::fmt::Write) {
        let line = line_ctx.line();
        let mut iter = COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(line) && name.len() > line.len());
        let name = match iter.next() {
            Some((name, _)) => *name,
            None => return,
        };
        if iter.next().is_some() {
            // ambiguous
            return;
        }
        let _ = writer.write_str(&name[line.len()..]);
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

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let mut editor = EditLine::new_with_locale("cli")?;
    editor.set_editor(libedit::Editor::Emacs)?;
    let mut tokenizer = Tokenizer::new(None)?;
    let color = supports_color();

    // -- Completion and help handlers --
    editor.set_complete_handler(CommandCompleter)?;
    editor.set_help_handler(HelpHandler)?;
    editor.set_hinter(CommandHinter)?;

    // -- Editor settings --
    let mut history = History::with_size(1000);
    let _ = history.load(history_path());
    editor.set_signal_handling(true)?;
    editor.set_auto_add_history(true);
    editor.set_history_ignore_space(true);
    editor.set_history(history);

    // -- Idle timeout: disconnect after 60s, warn at 30s remaining --
    editor.set_idle_timeout(Some(Duration::from_secs(60)));
    editor.set_idle_warning(Duration::from_secs(30), |w| {
        let _ = write!(w, "\x1b[1;33m% session will expire in 30 seconds\x1b[0m");
    });

    // -- Banner --
    if color {
        println!(
            "\x1b[1;36mcli\x1b[0m -- type \x1b[1m?\x1b[0m for help, Tab to complete, Ctrl-D to exit"
        );
        println!("      paste is bracketed -- try pasting \x1b[1mecho hello world\x1b[0m");
    } else {
        println!("cli -- type ? for help, Tab to complete, Ctrl-D to exit");
        println!("      paste is bracketed -- try pasting `echo hello world`");
    }

    // -- REPL loop --
    let prompt = if color {
        "\x1b[1;32mcli>\x1b[0m "
    } else {
        "cli> "
    };
    loop {
        let line = match editor.readline(prompt) {
            Ok(line) => line,
            Err(Error::Eof) => {
                println!();
                break;
            }
            Err(Error::Timeout) => {
                println!("\n% session timed out due to inactivity");
                break;
            }
            Err(Error::Interrupted) => {
                println!("^C");
                continue;
            }
            Err(e) => return Err(e),
        };
        if line.is_empty() {
            continue; // empty or whitespace-only
        }

        tokenizer.reset();
        let words = tokenizer.tokenize(line)?;

        match words.first().copied() {
            Some("quit") | Some("exit") => break,
            Some("help") => {
                let max = COMMANDS.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
                for (name, desc) in COMMANDS {
                    println!("  \x1b[1;36m{:<width$}\x1b[0m  {desc}", name, width = max);
                }
            }
            Some("history") => {
                match editor.history_mut() {
                    None => println!("(no history)"),
                    Some(hist) if hist.is_empty() => println!("(no history)"),
                    Some(hist) => {
                        let total = hist.len();
                        let show = total.min(10);
                        println!("{} entries (showing last {show}):", total);

                        // Rewind and print oldest -> newest
                        let _ = hist.newest();
                        let _ = hist.older_n(show.saturating_sub(1));
                        if let Some(e) = hist.curr() {
                            println!("{:4}  {}", e.num, e.value);
                        }
                        let mut count = 1;
                        while let Some(e) = hist.newer() {
                            println!("{:4}  {}", e.num, e.value);
                            count += 1;
                            if count >= show {
                                break;
                            }
                        }
                    }
                }
            }
            Some("clear") => {
                if let Some(hist) = editor.history_mut() {
                    hist.clear();
                    println!("history cleared");
                }
            }
            Some("caf\u{00e9}") => println!("(caf\u{00e9}: not implemented in this demo)"),
            Some("echo") => {
                // Echo the remaining tokens -- handy for seeing exactly what a
                // paste inserted into the line.
                println!("{}", words[1..].join(" "));
            }
            Some("show") => println!("(show: not implemented in this demo)"),
            Some("set") => println!("(set: not implemented in this demo)"),
            Some(cmd) => {
                println!("\x1b[31munknown command:\x1b[0m `{cmd}` -- press ? for help");
            }
            None => {}
        }
    }

    if let Some(hist) = editor.history_mut() {
        let _ = hist.save(history_path());
    }

    println!("bye!");
    Ok(())
}
