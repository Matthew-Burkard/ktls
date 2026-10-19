//! Print the syntax tree of a Kotlin file: `cargo run --example dump_tree -- File.kt`.
fn main() {
    let path = std::env::args().nth(1).expect("usage: dump_tree <file.kt>");
    let text = std::fs::read_to_string(&path).expect("cannot read file");
    let parse = kotlin_lsp::syntax::parse(&text, path.ends_with(".kts"));
    print!("{}", parse.debug_tree());
    for e in parse.errors() {
        eprintln!("error: {e:?}");
    }
}
