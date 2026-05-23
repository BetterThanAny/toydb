//! Pretty-printing for [`ResultSet`] — used by the REPL.
//!
//! The output style is the standard "+---+---+ / | a | b |" ASCII grid.
//! Width is computed per column from the rendered string of each cell;
//! NULLs render as `NULL`, strings unquoted.

use std::fmt::Write;

use crate::executor::ResultSet;

pub fn render(rs: &ResultSet) -> String {
    match rs {
        ResultSet::Select { columns, rows } => {
            let headers: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
            let mut data: Vec<Vec<String>> = Vec::with_capacity(rows.len());
            for r in rows {
                let mut line = Vec::with_capacity(r.len());
                for v in &r.0 {
                    line.push(v.to_string());
                }
                data.push(line);
            }
            render_table(&headers, &data)
        }
        ResultSet::CreateTable { name } => format!("CREATE TABLE {name}\n"),
        ResultSet::AlterTable { name } => format!("ALTER TABLE {name}\n"),
        ResultSet::CreateIndex { name } => format!("CREATE INDEX {name}\n"),
        ResultSet::DropIndex { name } => format!("DROP INDEX {name}\n"),
        ResultSet::DropTable { name, existed } => {
            if *existed {
                format!("DROP TABLE {name}\n")
            } else {
                format!("DROP TABLE {name} (did not exist)\n")
            }
        }
        ResultSet::Insert { count } => format!("{count} row(s) inserted\n"),
        ResultSet::Update { count } => format!("{count} row(s) updated\n"),
        ResultSet::Delete { count } => format!("{count} row(s) deleted\n"),
        ResultSet::Begin => "BEGIN\n".into(),
        ResultSet::Commit => "COMMIT\n".into(),
        ResultSet::Rollback => "ROLLBACK\n".into(),
        ResultSet::Explain(plan) => format!("{plan}\n"),
    }
}

fn render_table(headers: &[String], rows: &[Vec<String>]) -> String {
    if headers.is_empty() {
        return "(empty)\n".into();
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| visible_width(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i >= widths.len() {
                continue;
            }
            let w = visible_width(cell);
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }

    let mut out = String::new();
    write_separator(&mut out, &widths);
    write_row(&mut out, headers, &widths);
    write_separator(&mut out, &widths);
    if rows.is_empty() {
        // Render an empty body so the result still looks like a table.
        let _ = writeln!(
            out,
            "| {}|",
            (0..widths
                .iter()
                .map(|w| w + 3)
                .sum::<usize>()
                .saturating_sub(2))
                .map(|_| ' ')
                .collect::<String>()
        );
    } else {
        for row in rows {
            write_row(&mut out, row, &widths);
        }
    }
    write_separator(&mut out, &widths);
    let _ = writeln!(
        out,
        "({} row{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    out
}

fn write_separator(out: &mut String, widths: &[usize]) {
    out.push('+');
    for w in widths {
        for _ in 0..(w + 2) {
            out.push('-');
        }
        out.push('+');
    }
    out.push('\n');
}

fn write_row(out: &mut String, cells: &[String], widths: &[usize]) {
    out.push('|');
    for (i, cell) in cells.iter().enumerate() {
        let w = widths.get(i).copied().unwrap_or(0);
        let pad = w.saturating_sub(visible_width(cell));
        out.push(' ');
        out.push_str(cell);
        for _ in 0..pad {
            out.push(' ');
        }
        out.push(' ');
        out.push('|');
    }
    out.push('\n');
}

/// Visible width of a string treating each char as 1 cell. Good enough
/// for ASCII identifier headers; CJK/emoji widths would need a proper
/// `unicode-width` crate, which we skip on purpose.
fn visible_width(s: &str) -> usize {
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::result::Column;
    use crate::types::row::Row;
    use crate::types::value::Value;

    #[test]
    fn renders_simple_select() {
        let rs = ResultSet::Select {
            columns: vec![Column::new("id"), Column::new("name")],
            rows: vec![
                Row(vec![Value::Integer(1), Value::String("alice".into())]),
                Row(vec![Value::Integer(2), Value::String("bob".into())]),
            ],
        };
        let s = render(&rs);
        assert!(s.contains("| id"));
        assert!(s.contains("| alice"));
        assert!(s.contains("(2 rows)"));
    }

    #[test]
    fn renders_empty_select() {
        let rs = ResultSet::Select {
            columns: vec![Column::new("id")],
            rows: vec![],
        };
        let s = render(&rs);
        assert!(s.contains("(0 rows)"));
    }

    #[test]
    fn renders_dml_messages() {
        assert!(render(&ResultSet::Insert { count: 3 }).contains("3 row"));
        assert!(render(&ResultSet::CreateTable { name: "t".into() }).contains("CREATE TABLE t"));
        assert!(render(&ResultSet::Begin).contains("BEGIN"));
    }
}
