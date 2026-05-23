//! toydb REPL.
//!
//! Usage:
//!   toydb                    -- in-memory REPL
//!   toydb --db path/to.db    -- disk-backed REPL (durable)
//!   toydb file.sql           -- run script then exit (in-memory)
//!   toydb --db x.db file.sql -- run script against disk DB
//!
//! Multi-line input ends at the first `;` on a line.

use std::io::Write as _;
use std::path::PathBuf;

use rustyline::DefaultEditor;
use rustyline::config::Configurer;
use rustyline::error::ReadlineError;

use toydb::engine::{DiskEngine, Engine, MemoryEngine};
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

struct Args {
    db: Option<PathBuf>,
    script: Option<PathBuf>,
    help: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        db: None,
        script: None,
        help: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--db" => {
                args.db = iter.next().map(PathBuf::from);
            }
            "--help" | "-h" => args.help = true,
            other if other.starts_with("--") => {
                eprintln!("unknown flag {other}");
                std::process::exit(2);
            }
            other => {
                args.script = Some(PathBuf::from(other));
            }
        }
    }
    args
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();
    if args.help {
        println!("toydb [--db FILE] [SCRIPT.sql]");
        return Ok(());
    }

    let mut engine: Box<dyn Engine> = match args.db.clone() {
        Some(path) => Box::new(DiskEngine::open(&path)?),
        None => Box::new(MemoryEngine::new()),
    };

    if let Some(path) = args.script {
        let sql = std::fs::read_to_string(&path)?;
        run_script(engine.as_mut(), &sql);
        // `engine` is dropped at end of scope; if it's a DiskEngine, the
        // Drop impl flushes pending pages. The WAL is durable already.
        return Ok(());
    }

    let mut rl = DefaultEditor::new()?;
    rl.set_auto_add_history(true);
    let _ = rl.load_history(".toydb_history");
    println!("toydb REPL — type SQL, end with `;`. Ctrl-D to exit.");
    if let Some(p) = &args.db {
        println!("connected to {}", p.display());
    } else {
        println!("(in-memory; data is lost when you exit)");
    }
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
            if let Some(rest) = trimmed.strip_prefix(".schema") {
                let name = rest.trim();
                print_schema(engine.as_ref(), name);
                continue;
            }
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
            continue;
        }
        execute_buffer(engine.as_mut(), &buf);
        buf.clear();
    }
    let _ = rl.save_history(".toydb_history");
    Ok(())
}

fn execute_buffer(engine: &mut dyn Engine, sql: &str) {
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

fn run_script(engine: &mut dyn Engine, sql: &str) {
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
    println!("  .tables          list tables in the catalog");
    println!("  .schema [table]  show schema for a table (or all)");
    println!("  .help            show this help");
    println!("  .exit            leave the REPL");
    println!();
    println!("Statements end with a semicolon. Multi-line input is supported.");
}

fn print_schema(engine: &dyn Engine, name: &str) {
    let names: Vec<String> = if name.is_empty() {
        engine.list_tables()
    } else {
        vec![name.to_string()]
    };
    for n in names {
        match engine.get_table(&n) {
            Ok(t) => {
                println!("CREATE TABLE {} (", t.name);
                for (i, c) in t.columns.iter().enumerate() {
                    let mut line = format!("    {} {}", c.name, c.ty);
                    if c.primary_key {
                        line.push_str(" PRIMARY KEY");
                    }
                    if c.unique && !c.primary_key {
                        line.push_str(" UNIQUE");
                    }
                    if !c.nullable && !c.primary_key {
                        line.push_str(" NOT NULL");
                    }
                    if c.default.is_some() {
                        line.push_str(" DEFAULT ...");
                    }
                    if i + 1 < t.columns.len() {
                        line.push(',');
                    }
                    println!("{line}");
                }
                println!(");");
                for index in &t.indexes {
                    println!(
                        "CREATE INDEX {} ON {}({});",
                        index.name, index.table, index.column
                    );
                }
            }
            Err(e) => eprintln!("error: {e}"),
        }
    }
}
