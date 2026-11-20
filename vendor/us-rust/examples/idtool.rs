//! Small CLI to convert between numeric entry/revision ids and their public
//! 13-character representation (see `src/id.rs`).
//!
//! ```sh
//! cargo run --example idtool -- encode 42
//! cargo run --example idtool -- decode 000000000001z
//! ```

use us_rs::id::{decode_id, encode_id};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match (args.first().map(String::as_str), args.get(1)) {
        (Some("encode"), Some(raw)) => match raw.parse::<i64>() {
            Ok(id) => println!("{}", encode_id(id)),
            Err(_) => {
                eprintln!("not a numeric id: {raw}");
                std::process::exit(1);
            }
        },
        (Some("decode"), Some(encoded)) => match decode_id(encoded) {
            Ok(id) => println!("{id}"),
            Err(error) => {
                eprintln!("cannot decode '{encoded}': {error}");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("usage:\n  idtool encode <numeric id>\n  idtool decode <13-char id>");
            std::process::exit(1);
        }
    }
}
