//! THROWAWAY t02 evidence harness — NOT production. See docs/adr/0001-parser-library.md.
//!
//! `cargo run -p parser-spike --example compare` prints the side-by-side
//! winnow-vs-chumsky parse of every corpus input. The committed golden file
//! (`tests/golden/errors.txt`) is the locked subset; this binary is for humans.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use parser_spike::{chumsky_spike, winnow_spike, CORPUS};

fn main() {
    println!("== t02 parser spike: winnow vs chumsky ==\n");
    for case in CORPUS {
        println!(
            "--- {} ({}) ---",
            case.label,
            if case.valid { "valid" } else { "broken" }
        );
        println!("input: {:?}", case.input);
        match winnow_spike::parse(case.input) {
            Ok(ast) => println!("winnow:  OK   {ast:?}"),
            Err(e) => println!("winnow:  ERR  {}", e.render()),
        }
        match chumsky_spike::parse(case.input) {
            Ok(ast) => println!("chumsky: OK   {ast:?}"),
            Err(e) => println!("chumsky: ERR  {}", e.render()),
        }
        if !case.valid {
            println!(
                "chumsky recovery errors: {}",
                chumsky_spike::parse_all_errors(case.input).len()
            );
        }
        println!();
    }
}
