mod app;
mod queue;
mod style;

fn main() -> eframe::Result {
    // With arguments this is the command-line tool; without, the desktop app.
    if std::env::args_os().len() > 1 {
        std::process::exit(compressor_cli::main());
    }
    app::run()
}
