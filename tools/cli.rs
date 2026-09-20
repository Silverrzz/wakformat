use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub fn args() -> Vec<String> {
    std::env::args().skip(1).flat_map(|arg| {
        if arg.starts_with("--") {
            if let Some((key, value)) = arg.split_once('=') {
                return vec![key.to_owned(), value.to_owned()];
            }
        }
        vec![arg]
    }).collect()
}

pub fn output(path: &str, inputs: &[impl AsRef<Path>], overwrite: bool) -> io::Result<File> {
    if let Ok(target) = Path::new(path).canonicalize() {
        for input in inputs {
            let source = input.as_ref().canonicalize()?;
            let same = if cfg!(windows) {
                target.to_string_lossy().eq_ignore_ascii_case(&source.to_string_lossy())
            } else { target == source };
            if same { return Err(io::Error::other("output must not be an input file")); }
        }
    }
    let mut options = OpenOptions::new();
    options.write(true);
    if overwrite { options.create(true).truncate(true); } else { options.create_new(true); }
    options.open(path)
}


pub fn flag(args: &mut std::iter::Peekable<std::vec::IntoIter<String>>) -> bool {
    match args.peek().map(String::as_str) {
        Some("true") => { args.next(); true }
        Some("false") => { args.next(); false }
        _ => true,
    }
}
