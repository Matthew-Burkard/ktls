//! `ktls`: a Kotlin language server speaking LSP over stdio.

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("ktls {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("ktls {}\n\nA Kotlin language server. Speaks LSP over stdio.\n\nEnvironment:\n  KTLS_LOG  error|warn|info|debug|trace (default: info, written to stderr)", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    ktls::logger::init();
    log::info!("ktls {} starting", env!("CARGO_PKG_VERSION"));
    let (connection, io_threads) = lsp_server::Connection::stdio();
    ktls::server::run(connection)?;
    io_threads.join()?;
    log::info!("ktls exited");
    Ok(())
}
