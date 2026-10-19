//! `kotlin-lsp`: a Kotlin language server speaking LSP over stdio.

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("kotlin-lsp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("kotlin-lsp {}\n\nA Kotlin language server. Speaks LSP over stdio.\n\nEnvironment:\n  KOTLIN_LSP_LOG  error|warn|info|debug|trace (default: info, written to stderr)", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    kotlin_lsp::logger::init();
    log::info!("kotlin-lsp {} starting", env!("CARGO_PKG_VERSION"));
    let (connection, io_threads) = lsp_server::Connection::stdio();
    kotlin_lsp::server::run(connection)?;
    io_threads.join()?;
    log::info!("kotlin-lsp exited");
    Ok(())
}
