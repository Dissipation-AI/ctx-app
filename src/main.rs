//! CTX.app. `CTX` opens the app; `CTX --render <tab> <file.png> [columns rows]` draws a tab once
//! into a PNG without opening a window (for tests and reference screenshots); `CTX --start-server`
//! registers and starts the ctx server (the `ctx` command runs it when it finds no server).

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--render") => std::process::exit(ctx_app::headless::render_main(&args[2..])),
        Some("--icon") => std::process::exit(ctx_app::headless::icon_main(std::path::Path::new(
            args.get(2).map(String::as_str).unwrap_or("CTX.iconset"),
        ))),
        Some("--start-server") => std::process::exit(ctx_app::app::plat::install::start_server_main()),
        Some("--version") => println!("CTX {}", env!("CARGO_PKG_VERSION")),
        _ => ctx_app::app::run(),
    }
}
