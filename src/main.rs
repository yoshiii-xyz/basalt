mod sql;
mod types;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && (args[1] == "--version") {
        println!("basalt {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    eprintln!("basalt: interactive shell arrives with the executor layer");
}
