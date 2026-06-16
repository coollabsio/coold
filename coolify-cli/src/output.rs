use std::io::{self, Write};

use anyhow::Result;
use serde::Serialize;

use crate::cli::OutputFormat;

pub fn print<T: Serialize>(format: OutputFormat, value: &T) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(value)?),
        OutputFormat::Pretty => println!("{}", serde_json::to_string_pretty(value)?),
        OutputFormat::Table => println!("{}", serde_json::to_string_pretty(value)?),
    }
    Ok(())
}

pub fn table(headers: &[&str], rows: &[Vec<String>]) -> Result<()> {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i >= widths.len() {
                widths.push(0);
            }
            widths[i] = widths[i].max(cell.len());
        }
    }
    let mut out = io::stdout();
    for (i, h) in headers.iter().enumerate() {
        write!(
            out,
            "{:<width$}{}",
            h,
            if i + 1 == headers.len() { "\n" } else { "  " },
            width = widths[i]
        )?;
    }
    for (i, w) in widths.iter().enumerate() {
        write!(
            out,
            "{:-<width$}{}",
            "",
            if i + 1 == widths.len() { "\n" } else { "  " },
            width = *w
        )?;
    }
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            write!(
                out,
                "{:<width$}{}",
                cell,
                if i + 1 == headers.len() { "\n" } else { "  " },
                width = widths[i]
            )?;
        }
    }
    Ok(())
}
