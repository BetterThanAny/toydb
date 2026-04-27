//! toydb REPL.
//!
//! Reads SQL one statement at a time, executes against an in-memory
//! engine, and renders results in an ASCII grid. Multi-line input is
//! supported by detecting whether the buffer ends in a complete statement
//! (we just look for a trailing semicolon — far from bulletproof but it
//! suits a teaching REPL).

use std::io::Write as _;

use rustyline::config::Configurer;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

use toydb::engine::MemoryEngine;
use toydb::executor::Executor;
use toydb::format::render;
use toydb::sql::Parser;

const PROMPT: &str = "toydb> ";
const CONT_PROMPT: &str = "  ...> ";

fn main() {
    if let Err(e) = run() {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = MemoryEngine::new();

    // Scripted mode: `toydb file.sql` runs the file and exits.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(path) = args.first() {
        let sql = std::fs::read_to_string(path)?;
        run_script(&mut engine, &sql);
        return Ok(());
    }

    let mut rl = DefaultEditor::new()?;
    rl.set_auto_add_history(true);
    let _ = rl.load_history(".toydb_history");

    println!("toydb REPL — type SQL, end with `;`. Ctrl-D to exit.");
    let mut buf = String::new();
    loop {
        let prompt = if buf.is_empty() { PROMPT } else { CONT_PROMPT };
        let line = match rl.readline(prompt) {
            Ok(l) => l,
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        };
        let trimmed = line.trim();
        if buf.is_empty() {
            // Meta commands. Only at the start of a fresh line.
            match trimmed {
                ".exit" | ".quit" | "exit" | "quit" => break,
                ".tables" => {
                    for t in engine.list_tables() {
                        println!("  {t}");
                    }
                    continue;
                }
                ".help" => {
                    print_help();
                    continue;
                }
                "" => continue,
                _ => {}
            }
        }
        buf.push_str(&line);
        buf.push('\n');
        if !buf.trim_end().ends_with(';') {
            // Need more input — read another line under continuation prompt.
            continue;
        }
        execute_buffer(&mut engine, &buf);
        buf.clear();
    }
    let _ = rl.save_history(".toydb_history");
    Ok(())
}

use toydb::engine::Engine as _;

fn execute_buffer(engine: &mut MemoryEngine, sql: &str) {
    let stmts = match Parser::parse_all(sql) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("parse error: {e}");
            return;
        }
    };
    for stmt in stmts {
        match Executor::new(engine).execute(&stmt) {
            Ok(rs) => {
                let _ = std::io::stdout().write_all(render(&rs).as_bytes());
            }
            Err(e) => eprintln!("error: {e}"),
        }
    }
}

fn run_script(engine: &mut MemoryEngine, sql: &str) {
    let stmts = match Parser::parse_all(sql) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("parse error: {e}");
            return;
        }
    };
    for stmt in stmts {
        match Executor::new(engine).execute(&stmt) {
            Ok(rs) => print!("{}", render(&rs)),
            Err(e) => {
                eprintln!("error: {e}");
                return;
            }
        }
    }
}

fn print_help() {
    println!("Meta commands:");
    println!("  .tables   list tables in the catalog");
    println!("  .help     show this help");
    println!("  .exit     leave the REPL");
    println!();
    println!("Statements end with a semicolon. Multi-line input is supported.");
}
