use crate::cli;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use wakformat::format::{FILE_HEADER, FILE_HEADER_SIZE, invalid, read_file_header, read_game};

pub fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut args = args.into_iter().peekable();
    let mut inputs = Vec::new();
    let mut output = None;
    let mut overwrite = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--input" | "-i" => inputs.push(args.next().ok_or("missing input")?),
            "--output" | "-o" => output = Some(args.next().ok_or("missing output")?),
            "--allow-overwrite" => overwrite = cli::flag(&mut args),
            "--help" | "-h" | "help" => {
                println!("Usage: wakformat interleave <INPUT> [MORE INPUTS ...] [OPTIONS]

Interleave one or more .wf files into one output.

Options:
  -i, --input <PATH>          Alternative to positional inputs; repeat as needed
  -o, --output <PATH>         Output file
      --allow-overwrite      Replace an existing output file
  -h, --help                 Show this help

Examples:
  wakformat interleave games.wf -o mixed.wf
  wakformat interleave a.wf b.wf c.wf -o mixed.wf");
                return Ok(());
            }
            _ if !arg.starts_with('-') => inputs.push(arg),
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    if inputs.is_empty() { return Err("missing input; usage: wakformat interleave <INPUT> [MORE INPUTS ...] [OPTIONS]".into()); }
    let output = output.unwrap_or_else(|| {
        if inputs.len() == 1 { Path::new(&inputs[0]).with_extension("interleaved.wf").to_string_lossy().into_owned() }
        else { "interleaved.wf".to_owned() }
    });
    let mut remaining = 0u64;
    let mut sections = Vec::new();
    let mut buffer = Vec::new();
    for input in &inputs {
        let path = Path::new(input);
        if !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("wf")) {
            return Err(format!("input must end in .wf: {input}").into());
        }
        let file = File::open(path).map_err(|e| io::Error::new(e.kind(), format!("{input}: {e}")))?;
        let file_size = file.metadata()?.len();
        let mut reader = BufReader::with_capacity(8 * 1024 * 1024, file);
        read_file_header(&mut reader).map_err(|e| io::Error::new(e.kind(), format!("{input}: {e}")))?;
        let bytes = file_size.checked_sub(FILE_HEADER_SIZE as u64).ok_or_else(|| invalid("truncated file header"))?;
        remaining = remaining.checked_add(bytes).ok_or_else(|| invalid("combined input size overflows u64"))?;
        if inputs.len() > 1 {
            if bytes != 0 { sections.push((input, FILE_HEADER_SIZE as u64, bytes)); }
            continue;
        }
        let section_size = bytes.div_ceil(32).max(1);
        let mut start = FILE_HEADER_SIZE as u64;
        let mut offset = start;
        eprintln!("Scanning game boundaries in {input}");
        while read_game(&mut reader, &mut buffer)? {
            offset = offset.checked_add(buffer.len() as u64).ok_or_else(|| invalid("file size overflows u64"))?;
            if offset - start >= section_size {
                sections.push((input, start, offset - start));
                start = offset;
            }
        }
        if offset != file_size { return Err(invalid("file size changed while scanning").into()); }
        if offset > start { sections.push((input, start, offset - start)); }
    }
    let expected_size = remaining.checked_add(FILE_HEADER_SIZE as u64).ok_or_else(|| invalid("output size overflows u64"))?;
    let mut streams = Vec::new();
    for &(input, start, bytes) in &sections {
        let mut file = File::open(input)?;
        file.seek(SeekFrom::Start(start))?;
        streams.push((bytes, BufReader::with_capacity(1024 * 1024, file.take(bytes)), input));
    }
    let mut writer = BufWriter::with_capacity(8 * 1024 * 1024,
        cli::output(&output, &inputs, overwrite)?);
    writer.write_all(&FILE_HEADER)?;
    let mut games = 0u64;
    while remaining != 0 {
        let mut choice = fastrand::u64(0..remaining);
        let mut index = 0;
        while choice >= streams[index].0 {
            choice -= streams[index].0;
            index += 1;
        }
        let (bytes, reader, input) = &mut streams[index];
        if !read_game(reader, &mut buffer).map_err(|e| io::Error::new(e.kind(), format!("{input}: {e}")))? {
            return Err(invalid(format!("{input}: unexpected end of section")).into());
        }
        let size = buffer.len() as u64;
        *bytes = bytes.checked_sub(size).ok_or_else(|| invalid("game crosses a section boundary"))?;
        writer.write_all(&buffer)?;
        remaining -= size;
        games += 1;
        if *bytes == 0 { streams.swap_remove(index); }
    }
    writer.flush()?;
    if writer.get_ref().metadata()?.len() != expected_size {
        return Err(invalid("output size does not match input data").into());
    }
    eprintln!("interleaved {games} games from {} files ({} sections) into {output}", inputs.len(), sections.len());
    Ok(())
}
