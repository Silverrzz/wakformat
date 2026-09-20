mod analyse;
mod cli;
mod interleave;
mod pgn2wf;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = cli::args().into_iter();
    let mut command = args.next();
    let help = command.as_deref() == Some("help");
    if help { command = args.next(); }
    let mut args: Vec<_> = args.collect();
    if help && !args.is_empty() { return Err("usage: wakformat help [COMMAND]".into()); }
    if help || args.is_empty() { args.push("--help".into()); }
    match command.as_deref() {
        Some("pgn2wf") => pgn2wf::run(args),
        Some("interleave") => interleave::run(args),
        Some("analyse") => analyse::run(args),
        None | Some("--help" | "-h") => {
            println!("Usage: wakformat <COMMAND> <INPUT> [OPTIONS]

Commands:
  pgn2wf      Convert PGNs, compressed PGNs, or tar archives to .wf
  interleave  Mix one or more .wf files into one output
  analyse     Report position, result and distribution statistics

All commands accept positional input and an optional -o/--output path.
Interleave also accepts multiple input files.

Examples:
  wakformat pgn2wf games.pgn.tar -o games.wf
  wakformat interleave games.wf -o mixed.wf
  wakformat interleave a.wf b.wf -o combined.wf
  wakformat analyse games.wf --approximate

Use wakformat <COMMAND> --help or wakformat help <COMMAND> for options.");
            Ok(())
        }
        Some(command) => Err(format!("unknown command {command}; use --help").into()),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("wakformat: {error}");
        std::process::exit(1);
    }
}
