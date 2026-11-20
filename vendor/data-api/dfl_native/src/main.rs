//! Small CLI used to sanity-check the native compiler and to measure it without
//! the Python interpreter in the loop.
//!
//!   dfl-cli sql <dialect> "<formula>" [fields]
//!   dfl-cli parse "<formula>"
//!   dfl-cli bench <dialect> <iters> < formulas.txt

use dfl_native::ast::FieldTypes;
use dfl_native::{compile_formula, parse_formula, DataType};
use std::io::{self, BufRead};
use std::time::Instant;

fn default_types() -> FieldTypes {
    [
        ("name", DataType::String),
        ("city", DataType::String),
        ("first_name", DataType::String),
        ("last_name", DataType::String),
        ("region", DataType::String),
        ("country", DataType::String),
        ("sales", DataType::Float),
        ("cost", DataType::Float),
        ("orders", DataType::Integer),
        ("order_id", DataType::Integer),
        ("order_date", DataType::Date),
        ("ship_date", DataType::Date),
    ]
    .iter()
    .map(|(name, dtype)| (name.to_string(), *dtype))
    .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("sql") => {
            let dialect = args.get(1).cloned().unwrap_or_else(|| "postgres".into());
            let formula = args.get(2).cloned().unwrap_or_default();
            // optional schema: `name:TYPE;name:TYPE...`
            let types = match args.get(3) {
                Some(spec) => parse_spec(spec).unwrap_or_else(|err| {
                    eprintln!("error: {err}");
                    std::process::exit(2);
                }),
                None => default_types(),
            };
            compile_formula(&formula, &dialect, &types).map(|compiled| {
                println!("{}", compiled.sql);
                eprintln!("data type: {}", compiled.data_type.name());
            })
        }
        Some("bench") => bench(
            args.get(1).map(String::as_str).unwrap_or("postgres"),
            parse_iters(&args),
        ),
        Some("templates") => {
            println!("{}", dfl_native::registry::len());
            Ok(())
        }
        Some("parse") => {
            let formula = args.get(1).cloned().unwrap_or_default();
            parse_formula(&formula).map(|item| println!("{item:?}"))
        }
        _ => {
            eprintln!(
                "usage: dfl-cli sql <dialect> \"<formula>\" [name:TYPE;...] | dfl-cli parse \"<formula>\" | \
                 dfl-cli bench <dialect> <iters> < formulas.txt | dfl-cli templates"
            );
            std::process::exit(2);
        }
    };
    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn parse_spec(spec: &str) -> Result<FieldTypes, String> {
    let mut types = FieldTypes::new();
    for entry in spec.split(';').filter(|entry| !entry.trim().is_empty()) {
        let mut parts = entry.split(':');
        let name = parts.next().unwrap_or("").trim();
        let type_name = parts.next().unwrap_or("STRING").trim().to_ascii_uppercase();
        let dtype =
            DataType::from_name(&type_name).ok_or_else(|| format!("unknown type: {type_name}"))?;
        types.insert(name.to_string(), dtype);
    }
    Ok(types)
}

fn parse_iters(args: &[String]) -> usize {
    args.get(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(1000)
}

fn bench(dialect: &str, iters: usize) -> Result<(), String> {
    let types = default_types();
    let mut formulas: Vec<String> = Vec::new();
    for line in io::stdin().lock().lines() {
        let line = line.map_err(|err| err.to_string())?;
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            formulas.push(trimmed.to_string());
        }
    }

    let mut sink = 0usize;
    let mut sum_parse = 0f64;
    let mut sum_gen = 0f64;
    let mut sum_full = 0f64;
    println!("formula\tparse_ms\tgen_ms\tfull_ms\tsql");

    for formula in &formulas {
        let ast = parse_formula(formula)?;

        let start = Instant::now();
        for _ in 0..iters {
            sink += parse_formula(formula).is_ok() as usize;
        }
        let parse_ms = start.elapsed().as_secs_f64() * 1e3 / iters as f64;

        let dialect_kind = dfl_native::Dialect::parse(dialect).ok_or("unknown dialect")?;
        let start = Instant::now();
        for _ in 0..iters {
            sink += dfl_native::compile(&ast, dialect_kind, &types)?.sql.len();
        }
        let gen_ms = start.elapsed().as_secs_f64() * 1e3 / iters as f64;

        let start = Instant::now();
        let mut sql = String::new();
        for _ in 0..iters {
            sql = compile_formula(formula, dialect, &types)?.sql;
            sink += sql.len();
        }
        let full_ms = start.elapsed().as_secs_f64() * 1e3 / iters as f64;

        sum_parse += parse_ms;
        sum_gen += gen_ms;
        sum_full += full_ms;
        println!("{formula}\t{parse_ms:.5}\t{gen_ms:.5}\t{full_ms:.5}\t{sql}");
    }

    let count = formulas.len().max(1) as f64;
    println!(
        "MEAN\t{:.5}\t{:.5}\t{:.5}",
        sum_parse / count,
        sum_gen / count,
        sum_full / count
    );
    if sink == usize::MAX {
        eprintln!("unreachable");
    }
    Ok(())
}
