use super::{Game, Pgn, has_extension, initial_board, is_pgn, parse_move};
use crate::cli;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::time::Instant;
use wakformat::board::Board;
use wakformat::common::{Color, Piece};
use wakformat::format::{GameOutcome, invalid};

type Counts = BTreeMap<(char, u16, u32, i32), u64>;

#[derive(Default)]
struct Extractor {
    all: Counts,
    training: Counts,
    validation: Counts,
    bin_width: i32,
    folds: u64,
    engine: Option<String>,
    games: u64,
    validation_games: u64,
    missing: u64,
    mates: u64,
    excluded: u64,
    excluded_games: u64,
    engines: BTreeMap<String, u64>,
}

impl Extractor {
    fn position(&mut self, game: &Game, board: &Board, index: usize, validation: bool) {
        let name = if board.stm() == Color::White { &game.white } else { &game.black };
        if self.engine.as_ref().is_some_and(|engine| name.as_ref() != Some(engine)) {
            self.excluded += 1;
            return;
        }
        let score = match game.plies[index].eval {
            None => { self.missing += 1; return; }
            Some(-32767 | 32767) => { self.mates += 1; return; }
            Some(score) => score,
        };
        let outcome = match game.result.unwrap() {
            GameOutcome::Draw => 'D',
            GameOutcome::WhiteWin if board.stm() == Color::White => 'W',
            GameOutcome::BlackWin if board.stm() == Color::Black => 'W',
            _ => 'L',
        };
        let material = [(Piece::Pawn, 1), (Piece::Knight, 3), (Piece::Bishop, 3),
            (Piece::Rook, 5), (Piece::Queen, 9)].into_iter()
            .map(|(piece, value)| board.pieces(piece).popcnt() as u32 * value).sum();
        let eval = (f64::from(score) / f64::from(self.bin_width)).round() as i32 * self.bin_width;
        let key = (outcome, board.fmc(), material, eval);
        *self.all.entry(key).or_default() += 1;
        if self.folds != 0 {
            let counts = if validation { &mut self.validation } else { &mut self.training };
            *counts.entry(key).or_default() += 1;
        }
        *self.engines.entry(name.as_deref().unwrap_or("unknown").to_owned()).or_default() += 1;
    }

    fn game(&mut self, game: Game) -> io::Result<()> {
        if let Some(variant) = &game.variant {
            if !matches!(variant.to_ascii_lowercase().as_str(), "duck960" | "duck" | "duckchess") {
                return Err(invalid(format!("unsupported variant {variant}")));
            }
        }
        if game.termination.as_ref().is_some_and(|value| matches!(
            value.to_ascii_lowercase().as_str(),
            "time forfeit" | "abandoned" | "stalled connection" | "illegal move" | "unterminated"
        )) {
            self.excluded_games += 1;
            return Ok(());
        }
        let result = game.result.ok_or_else(|| invalid("unfinished game"))?;
        if game.plies.is_empty() { return Err(invalid("game has no moves")); }
        let mut board = initial_board(game.fen.as_deref())?;
        let opening = game.fen.as_deref().unwrap_or("startpos")
            .split_whitespace().take(4).collect::<Vec<_>>().join(" ");
        let hash = opening.bytes().fold(0xcbf29ce484222325u64,
            |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3));
        let validation = self.folds != 0 && hash % self.folds == 0;

        for (index, ply) in game.plies.iter().enumerate() {
            if board.hmc() >= 100 { return Err(invalid("moves after fifty-move draw")); }
            let mv = parse_move(&board, ply.text())
                .map_err(|error| invalid(format!("ply {}: {error}", index + 1)))?;
            self.position(&game, &board, index, validation);
            if board.piece_on(mv.dest()) == Some(Piece::King) && board.color_on(mv.dest()) == Some(!board.stm()) {
                let winner = if board.stm() == Color::White { GameOutcome::WhiteWin } else { GameOutcome::BlackWin };
                if index + 1 != game.plies.len() || result != winner {
                    return Err(invalid("king capture must end the game with the matching result"));
                }
                break;
            }
            if index + 1 < game.plies.len() {
                if board.stm() == Color::Black && board.fmc() == u16::MAX {
                    return Err(invalid("fullmove counter overflow"));
                }
                board.make_move(mv);
            }
        }
        self.games += 1;
        self.validation_games += u64::from(validation);
        Ok(())
    }

    fn pgn(&mut self, reader: impl Read, source: &str) -> io::Result<()> {
        let reader: Box<dyn Read + '_> = if source.to_ascii_lowercase().ends_with(".bz2") {
            Box::new(bzip2::read::MultiBzDecoder::new(reader))
        } else { Box::new(reader) };
        let mut reader = Pgn::new(BufReader::with_capacity(1024 * 1024, reader))?;
        let mut index = 1;
        let result: io::Result<()> = (|| {
            while let Some(game) = reader.game()? {
                self.game(game)?;
                if self.games % 1000 == 0 { println!("processed {} games", self.games); }
                index += 1;
            }
            Ok(())
        })();
        result.map_err(|error| io::Error::new(error.kind(), format!("{source}: game {index}: {error}")))
    }

    fn file(&mut self, path: &Path) -> io::Result<()> {
        let file = File::open(path)?;
        if !has_extension(path, "tar") { return self.pgn(file, &path.to_string_lossy()); }
        let mut archive = tar::Archive::new(BufReader::new(file));
        let mut found = false;
        for entry in archive.entries()? {
            let mut entry = entry?;
            if !entry.header().entry_type().is_file() { continue; }
            let member = entry.path()?.into_owned();
            if !is_pgn(&member) { continue; }
            found = true;
            self.pgn(&mut entry, &format!("{}: {}", path.display(), member.display()))?;
        }
        if !found { return Err(invalid("archive contains no PGNs")); }
        Ok(())
    }

    fn report(&self, seconds: f64) {
        let training = if self.folds == 0 { &self.all } else { &self.training };
        for (label, count) in [
            ("games", self.games),
            ("scored positions", self.all.values().sum()),
            ("missing evaluations", self.missing),
            ("mate evaluations", self.mates),
            ("excluded evaluations", self.excluded),
            ("excluded terminations", self.excluded_games),
            ("bin width", self.bin_width as u64),
            ("training games", self.games - self.validation_games),
            ("validation games", self.validation_games),
            ("training positions", training.values().sum()),
            ("validation positions", self.validation.values().sum()),
        ] { println!("{label}: {count}"); }
        println!("time taken: {seconds:.2}s");
        for (engine, count) in &self.engines { println!("engine {engine}: {count} scored positions"); }
    }
}

fn save(path: &str, input: &str, counts: &Counts, overwrite: bool) -> io::Result<()> {
    let mut writer = BufWriter::new(cli::output(path, &[input], overwrite)?);
    writeln!(writer, "{{")?;
    for (index, ((result, fullmove, material, eval), count)) in counts.iter().enumerate() {
        let comma = if index + 1 == counts.len() { "" } else { "," };
        writeln!(writer, "  \"('{result}', {fullmove}, {material}, {eval})\": {count}{comma}")?;
    }
    writeln!(writer, "}}")?;
    writer.flush()
}

pub(crate) fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut args = args.into_iter().peekable();
    let (mut input, mut output) = (None, None);
    let mut overwrite = false;
    let mut extractor = Extractor { bin_width: 5, ..Default::default() };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--input" | "-i" => input = Some(args.next().ok_or("missing input")?),
            "--output" | "-o" => output = Some(args.next().ok_or("missing output")?),
            "--engine" => extractor.engine = Some(args.next().ok_or("missing engine name")?),
            "--bin-width" => extractor.bin_width = args.next().ok_or("missing bin width")?.parse()?,
            "--validation-folds" => extractor.folds = args.next().ok_or("missing fold count")?.parse()?,
            "--allow-overwrite" => overwrite = cli::flag(&mut args),
            "--help" | "-h" | "help" => {
                println!("Usage: wakformat pgnwdl <INPUT> [OPTIONS]

Extract pre-move, side-to-move WDL counts from .pgn, .pgn.bz2 or .tar files.
Both engines are used by default; mate and missing evaluations are omitted.

Options:
  -i, --input <PATH>         Alternative to positional input
  -o, --output <PATH>        Statistics JSON (default: <INPUT>.wdl.json)
      --engine <NAME>       Include only this exact PGN engine name
      --bin-width <CP>      Evaluation bin width (default: 5)
      --validation-folds <N> Write <OUTPUT>.train.json and <OUTPUT>.validation.json
                            Hold out 1/N opening FENs using FNV-1a modulo N
      --allow-overwrite     Replace existing outputs
  -h, --help                Show this help");
                return Ok(());
            }
            _ if !arg.starts_with('-') && input.is_none() => input = Some(arg),
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    let input = input.ok_or("missing input; usage: wakformat pgnwdl <INPUT> [OPTIONS]")?;
    if !(1..=1000).contains(&extractor.bin_width) { return Err("bin width must be between 1 and 1000".into()); }
    if extractor.folds == 1 || extractor.folds > 1000 { return Err("validation folds must be 0 or between 2 and 1000".into()); }
    let output = output.unwrap_or_else(|| format!("{input}.wdl.json"));
    let mut outputs = vec![output.clone()];
    if extractor.folds != 0 {
        outputs.extend([format!("{output}.train.json"), format!("{output}.validation.json")]);
    }
    for path in &outputs {
        if !overwrite && Path::new(path).exists() { return Err(format!("output already exists: {path}").into()); }
    }

    let started = Instant::now();
    extractor.file(Path::new(&input))?;
    if extractor.all.is_empty() { return Err("no numeric evaluations found".into()); }
    for (path, counts) in outputs.iter().zip([&extractor.all, &extractor.training, &extractor.validation]) {
        save(path, &input, counts, overwrite)?;
    }
    extractor.report(started.elapsed().as_secs_f64());
    println!("output: {output}");
    Ok(())
}
