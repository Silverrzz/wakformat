use crate::cli;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::Instant;
use wakformat::board::Board;
use wakformat::common::{Color, Piece};
use wakformat::format::{FILE_HEADER_SIZE, GAME_HEADER_SIZE, RECORD_SIZE, MoveRecord,
    header_offset, invalid, read_file_header, read_game, replay_move, unpack_header};

struct Stats {
    games: u64,
    positions: u64,
    exits: i128,
    outcomes: [u64; 3],
    kings: [u64; 64],
    ducks: [u64; 65],
    scores: Vec<u64>,
    material: [u64; 33],
    phases: [u64; 25],
    pieces: [[[u64; 33]; 6]; 2],
    hashes: HashSet<u64>,
    registers: Vec<u8>,
}

impl Stats {
    fn new(approximate: bool) -> Self {
        Self {
            games: 0, positions: 0, exits: 0, outcomes: [0; 3], kings: [0; 64],
            ducks: [0; 65], scores: vec![0; 65536], material: [0; 33],
            phases: [0; 25], pieces: [[[0; 33]; 6]; 2],
            hashes: HashSet::new(), registers: vec![0; if approximate { 1 << 20 } else { 0 }],
        }
    }

    fn unique_count(&self) -> u64 {
        if self.registers.is_empty() { return self.hashes.len() as u64; }
        let m = self.registers.len() as f64;
        let sum: f64 = self.registers.iter().map(|&rank| 2f64.powi(-(rank as i32))).sum();
        let estimate = (0.7213 / (1.0 + 1.079 / m)) * m * m / sum;
        let empty = self.registers.iter().filter(|&&rank| rank == 0).count();
        if estimate <= 2.5 * m && empty > 0 { (m * (m / empty as f64).ln()).round() as u64 }
        else { estimate.round() as u64 }
    }

    fn position(&mut self, board: &Board, score: i16) {
        self.positions += 1;
        let hash = board.hash();
        if self.registers.is_empty() { self.hashes.insert(hash); }
        else {
            let register = &mut self.registers[(hash >> 44) as usize];
            *register = (*register).max(((hash << 20) | (1 << 19)).leading_zeros() as u8 + 1);
        }
        self.scores[(score as i32 + 32768) as usize] += 1;
        self.kings[board.king(board.stm()).relative_to(board.stm()) as usize] += 1;
        self.ducks[board.duck().map_or(64, |square| square as usize)] += 1;
        let occupied = board.colors(Color::White) | board.colors(Color::Black);
        self.material[occupied.popcnt() as usize] += 1;
        let mut phase = 0;
        for &piece in Piece::ALL {
            phase += board.pieces(piece).popcnt() as usize * [0, 1, 1, 2, 4, 0][piece as usize];
            for &color in Color::ALL {
                let count = board.colored_pieces(color, piece).popcnt() as usize;
                self.pieces[color as usize][piece as usize][count] += 1;
            }
        }
        self.phases[phase.min(24)] += 1;
    }

    fn game(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut board = unpack_header(bytes[..GAME_HEADER_SIZE].try_into().unwrap())?;
        let outcome = bytes[header_offset::OUTCOME] as usize;
        self.games += 1;
        self.outcomes[outcome] += 1;
        let moves = &bytes[GAME_HEADER_SIZE..bytes.len() - RECORD_SIZE];
        let plies = moves.len() / RECORD_SIZE;
        for (index, bytes) in moves.chunks_exact(RECORD_SIZE).enumerate() {
            let result = (|| -> io::Result<()> {
                let record = MoveRecord::from_bytes(bytes.try_into().unwrap()).map_err(invalid)?
                    .ok_or_else(|| invalid("unexpected game terminator"))?;
                if board.try_king(Color::White).is_none() || board.try_king(Color::Black).is_none()
                    || board.hmc() >= 100 || !board.is_legal(record.mv)
                { return Err(invalid("invalid move during analysis")); }
                if index == 0 {
                    self.exits += record.white_score as i128 * board.stm().signum() as i128;
                }
                self.position(&board, record.white_score);
                let king_capture = board.piece_on(record.mv.dest()) == Some(Piece::King)
                    && board.color_on(record.mv.dest()) == Some(!board.stm());
                let winner = if board.stm() == Color::White { 2 } else { 0 };
                if king_capture && (index + 1 != plies || outcome != winner) {
                    return Err(invalid("king capture must end the game with the matching result"));
                }
                if index + 1 < plies { replay_move(&mut board, record.mv)?; }
                Ok(())
            })();
            result.map_err(|e| io::Error::new(e.kind(), format!("ply {}: {e}", index + 1)))?;
        }
        Ok(())
    }

    fn report(&self, writer: &mut impl Write, verbose: bool) -> io::Result<()> {
        let games = self.games.max(1) as f64;
        let positions = self.positions.max(1) as f64;
        let unique = self.unique_count().min(self.positions);
        let mode = if self.registers.is_empty() { "exact hash count" } else { "estimated" };
        writeln!(writer, "games: {}\npositions: {}\npositions/game: {:.2}\naverage exit: {:.2} cp (starting side to move)",
            self.games, self.positions, self.positions as f64 / games, self.exits as f64 / games)?;
        writeln!(writer, "unique positions: {unique}/{} ({:.2}%; {mode})", self.positions, 100.0 * unique as f64 / positions)?;
        for (label, count) in [("wins (White)", self.outcomes[2]), ("draws", self.outcomes[1]), ("losses (White)", self.outcomes[0])] {
            writeln!(writer, "{label}: {count} ({:.2}%)", 100.0 * count as f64 / games)?;
        }
        for (width, label) in [(8, "%; side-to-move perspective, ranks 1 to 8"), (4, "mirrored; a/h, b/g, c/f, d/e")] {
            writeln!(writer, "king bucket distribution ({label}):")?;
            for row in self.kings.chunks_exact(8) {
                for file in 0..width {
                    let count = row[file] + if width == 4 { row[7 - file] } else { 0 };
                    write!(writer, "{:6.2} ", 100.0 * count as f64 / positions)?;
                }
                writeln!(writer)?;
            }
        }
        if verbose {
            writeln!(writer, "piece count distribution (0..32, excluding duck): {:?}", self.material)?;
            writeln!(writer, "phase distribution (0..24; N/B=1, R=2, Q=4): {:?}", self.phases)?;
            for (color, label) in [(Color::White, "white"), (Color::Black, "black")] {
                writeln!(writer, "{label} piece distributions (counts 0..32):")?;
                for &piece in Piece::ALL {
                    writeln!(writer, "  {piece}: {:?}", self.pieces[color as usize][piece as usize])?;
                }
            }
            writeln!(writer, "king square counts (a1..h8): {:?}", self.kings)?;
            writeln!(writer, "duck square counts (a1..h8): {:?}\npositions without duck: {}", &self.ducks[..64], self.ducks[64])?;
        }
        Ok(())
    }
}

pub fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut args = args.into_iter().peekable();
    let mut input = None;
    let mut output = "score_distribution.txt".to_owned();
    let mut overwrite = false;
    let mut approximate = false;
    let mut verbose = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--input" | "-i" => input = Some(args.next().ok_or("missing input")?),
            "--output" | "-o" => output = args.next().ok_or("missing output")?,
            "--allow-overwrite" => overwrite = cli::flag(&mut args),
            "--approximate" => approximate = cli::flag(&mut args),
            "--verbose" | "-v" => verbose = cli::flag(&mut args),
            "--help" | "-h" | "help" => {
                println!("Usage: wakformat analyse <INPUT> [OPTIONS]

Report dataset statistics for a .wf file, like Pawnocchio's analyse tool.

Options:
  -i, --input <PATH>          Alternative to positional input
  -o, --output <PATH>         Score distribution (default: score_distribution.txt)
      --allow-overwrite      Replace an existing output file
      --approximate          Estimate unique positions with 1 MiB HyperLogLog
  -v, --verbose              Show material, phase, piece, king and duck counts
  -h, --help                 Show this help

Example: wakformat analyse games.wf --approximate");
                return Ok(());
            }
            _ if !arg.starts_with('-') && input.is_none() => input = Some(arg),
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    let input = input.ok_or("missing input; usage: wakformat analyse <INPUT> [OPTIONS]")?;
    if !Path::new(&input).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("wf")) {
        return Err("input must end in .wf".into());
    }
    if !overwrite && Path::new(&output).exists() {
        return Err(format!("output already exists: {output}; use -o or --allow-overwrite").into());
    }
    let file = File::open(&input)?;
    let size = file.metadata()?.len();
    let mut reader = BufReader::with_capacity(8 * 1024 * 1024, file);
    read_file_header(&mut reader)?;
    let mut stats = Stats::new(approximate);
    let mut bytes = Vec::new();
    let mut done = FILE_HEADER_SIZE as u64;
    let mut progress = Instant::now();
    loop {
        let index = stats.games + 1;
        if !read_game(&mut reader, &mut bytes)
            .map_err(|e| io::Error::new(e.kind(), format!("{input}: game {index}: {e}")))? { break; }
        stats.game(&bytes).map_err(|e| io::Error::new(e.kind(), format!("{input}: game {index}: {e}")))?;
        done += bytes.len() as u64;
        if progress.elapsed().as_secs() >= 1 {
            eprint!("\rprogress: {:.2}% ({} games, {} positions)", 100.0 * done as f64 / size as f64, stats.games, stats.positions);
            progress = Instant::now();
        }
    }
    eprintln!("\rprogress: 100.00% ({} games, {} positions)", stats.games, stats.positions);
    let mut writer = BufWriter::new(cli::output(&output, std::slice::from_ref(&input), overwrite)?);
    writeln!(writer, "{:?}", stats.scores)?;
    writer.flush()?;
    let mut stdout = io::stdout().lock();
    stats.report(&mut stdout, verbose)?;
    writeln!(stdout, "score distribution: {output}")?;
    Ok(())
}
