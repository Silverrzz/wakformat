// Convert duck chess PGN games to wakformat.
use crate::cli;

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}, mpsc};
use std::time::Instant;
use wakformat::board::{Board, CastlingDirection};
use wakformat::common::{Color, File as ChessFile, Move, Piece, Rank, Square};
use wakformat::format::{FILE_HEADER, GAME_TERMINATOR, GameOutcome, MoveRecord, invalid, pack_header};
use wakformat::util::Abort;

#[derive(Default)]
struct Game {
    fen: Option<String>,
    result: Option<GameOutcome>,
    initial_eval: Option<i16>,
    plies: Vec<Ply>,
}

// Keep each move token in a small fixed buffer.
struct Ply {
    text: [u8; 32],
    len: u8,
    eval: Option<i16>,
}

impl Ply {
    fn new(text: &str) -> io::Result<Self> {
        let mut ply = Self { text: [0; 32], len: 0, eval: None };
        ply.append(text)?;
        Ok(ply)
    }

    fn append(&mut self, text: &str) -> io::Result<()> {
        let start = self.len as usize;
        let end = start + text.len();
        if end > self.text.len() || !text.is_ascii() { return Err(invalid("invalid PGN move token")); }
        self.text[start..end].copy_from_slice(text.as_bytes());
        self.len = end as u8;
        Ok(())
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.text[..self.len as usize]).unwrap()
    }
}

struct Pgn<R> {
    reader: R,
    token: Vec<u8>,
    pending_tag: bool,
}

impl<R: BufRead> Pgn<R> {
    fn peek(&mut self) -> io::Result<Option<u8>> {
        Ok(self.reader.fill_buf()?.first().copied())
    }

    fn byte(&mut self) -> io::Result<Option<u8>> {
        let value = self.peek()?;
        if value.is_some() { self.reader.consume(1); }
        Ok(value)
    }

    // Read a tag, comment, variation marker, or move token.
    fn token(&mut self) -> io::Result<Option<u8>> {
        if self.pending_tag {
            self.pending_tag = false;
            return Ok(Some(b'['));
        }
        self.token.clear();
        let first = loop {
            match self.byte()? {
                None => return Ok(None),
                Some(b) if b.is_ascii_whitespace() => continue,
                Some(b) => break b,
            }
        };
        if first == b'(' || first == b')' { return Ok(Some(first)); }
        if first == b'[' || first == b'{' {
            let end = if first == b'[' { b']' } else { b'}' };
            let mut quoted = false;
            let mut escaped = false;
            loop {
                let b = self.byte()?.ok_or_else(|| invalid("unterminated PGN tag or comment"))?;
                if first == b'[' {
                    if b == b'"' && !escaped { quoted = !quoted; }
                    if b == end && !quoted { break; }
                    escaped = b == b'\\' && !escaped;
                } else if b == end { break; }
                self.token.push(b);
                if self.token.len() > 1024 * 1024 { return Err(invalid("PGN token exceeds 1 MiB")); }
            }
            return Ok(Some(first));
        }
        if first == b';' || first == b'%' {
            while let Some(b) = self.byte()? {
                if b == b'\n' || b == b'\r' { break; }
                self.token.push(b);
                if self.token.len() > 1024 * 1024 { return Err(invalid("PGN comment exceeds 1 MiB")); }
            }
            return Ok(Some(b'{'));
        }
        self.token.push(first);
        while let Some(b) = self.peek()? {
            if b.is_ascii_whitespace() || matches!(b, b'[' | b'{' | b'}' | b'(' | b')' | b';' | b'$') { break; }
            self.reader.consume(1);
            self.token.push(b);
            if self.token.len() > 4096 { return Err(invalid("PGN move token exceeds 4096 bytes")); }
        }
        Ok(Some(b'm'))
    }

    // Find the next game boundary after a parse error.
    fn recover(&mut self) -> io::Result<bool> {
        if self.pending_tag { return Ok(true); }
        while let Some(kind) = self.token()? {
            if kind == b'[' && self.token.starts_with(b"Event ") {
                self.pending_tag = true;
                return Ok(true);
            }
            if kind == b'm' && matches!(self.token.as_slice(), b"1-0" | b"0-1" | b"1/2-1/2" | b"*") {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // Collect the main line, skipping moves and comments inside variations.
    fn game(&mut self) -> io::Result<Option<Game>> {
        let mut game = Game::default();
        let mut depth = 0usize;
        let mut seen = false;
        while let Some(kind) = self.token()? {
            if kind == b'(' { depth += 1; continue; }
            if kind == b')' {
                depth = depth.checked_sub(1).ok_or_else(|| invalid("unmatched PGN variation close"))?;
                continue;
            }
            if depth != 0 { continue; }
            let token = std::str::from_utf8(&self.token).map_err(invalid)?;
            let token = token.trim_start_matches('\u{feff}');
            if token.is_empty() { continue; }
            if kind == b'[' {
                seen = true;
                if !game.plies.is_empty() {
                    self.pending_tag = true;
                    return Err(invalid("new PGN tags before game result"));
                }
                let split = token.find(char::is_whitespace).ok_or_else(|| invalid("invalid PGN tag"))?;
                let (name, value) = token.split_at(split);
                let value = value.trim().strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                    .ok_or_else(|| invalid("invalid PGN tag value"))?;
                match name {
                    "FEN" => game.fen = Some(value.to_owned()),
                    "Result" => game.result = outcome(value)?,
                    _ => {}
                }
            } else if kind == b'{' {
                if let Some(score) = evaluation(token)? {
                    if let Some(ply) = game.plies.last_mut() { ply.eval = Some(score); }
                    else { game.initial_eval = Some(score); }
                }
            } else if matches!(token, "1-0" | "0-1" | "1/2-1/2" | "*") {
                let result = outcome(token)?;
                if game.result.is_some() && game.result != result {
                    return Err(invalid("PGN header and movetext results disagree"));
                }
                game.result = result;
                return Ok(Some(game));
            } else {
                let mut start = 0;
                while token.as_bytes().get(start).is_some_and(u8::is_ascii_digit) { start += 1; }
                if token.as_bytes().get(start) == Some(&b'.') {
                    while token.as_bytes().get(start) == Some(&b'.') { start += 1; }
                } else { start = 0; }
                let text = &token[start..];
                if text.is_empty() || text.starts_with('$') || text == "e.p."
                    || text.bytes().all(|b| matches!(b, b'.' | b'!' | b'?')) { continue; }
                // A separate duck placement belongs to the preceding move.
                if text.starts_with('@') {
                    let ply = game.plies.last_mut().ok_or_else(|| invalid("duck placement without a move"))?;
                    if ply.text().contains('@') { return Err(invalid("duplicate duck placement")); }
                    ply.append(text)?;
                } else {
                    seen = true;
                    game.plies.push(Ply::new(text)?);
                }
                if game.plies.len() > 100_000 { return Err(invalid("PGN game exceeds 100000 plies")); }
            }
        }
        if depth != 0 { return Err(invalid("unterminated PGN variation")); }
        if !seen { Ok(None) }
        else if game.result.is_some() { Ok(Some(game)) }
        else { Err(invalid("PGN game has no completed result")) }
    }
}

fn outcome(text: &str) -> io::Result<Option<GameOutcome>> {
    match text {
        "1-0" => Ok(Some(GameOutcome::WhiteWin)),
        "0-1" => Ok(Some(GameOutcome::BlackWin)),
        "1/2-1/2" => Ok(Some(GameOutcome::Draw)),
        "*" => Ok(None),
        _ => Err(invalid("invalid PGN result")),
    }
}

// Read tagged or plain scores as centipawns; map mate scores to the limits.
fn evaluation(comment: &str) -> io::Result<Option<i16>> {
    let text = if let Some((_, rest)) = comment.split_once("[%eval ") {
        rest.split(']').next().unwrap_or(rest).trim()
    } else {
        comment.split(|c: char| c.is_whitespace() || matches!(c, '/' | '{' | '}'))
            .find(|s| !s.is_empty()).unwrap_or("")
    };
    if text.is_empty() { return Ok(None); }
    if text.contains(['M', '#']) {
        return Ok(Some(if text.starts_with('-') { -32767 } else { 32767 }));
    }
    let Ok(value) = text.parse::<f64>() else { return Ok(None); };
    if !value.is_finite() { return Ok(None); }
    Ok(Some((value.clamp(-327.67, 327.67) * 100.0) as i16))
}

fn square(text: &str) -> Option<Square> {
    let [file, rank] = text.as_bytes() else { return None; };
    if !(b'a'..=b'h').contains(file) || !(b'1'..=b'8').contains(rank) { return None; }
    Some(Square::new(ChessFile::index((file - b'a') as usize), Rank::index((rank - b'1') as usize)))
}

// Use the standard start position or validate the supplied FEN.
fn initial_board(fen: Option<&str>) -> io::Result<Board> {
    let Some(fen) = fen else { return Ok(Board::startpos()); };
    let fields: Vec<_> = fen.split_whitespace().collect();
    if fields.len() != 6 || fields[0].split('/').count() != 8
        || fields[0].bytes().filter(|&b| b == b'K').count() != 1
        || fields[0].bytes().filter(|&b| b == b'k').count() != 1
        || (fields[3] != "-" && square(fields[3]).is_none())
    { return Err(invalid("invalid initial PGN FEN")); }
    for (rank, row) in fields[0].split('/').enumerate() {
        let mut width = 0;
        for b in row.bytes() {
            match b {
                b'1'..=b'8' => width += (b - b'0') as usize,
                b'p' | b'P' if rank == 0 || rank == 7 => return Err(invalid("pawn on FEN back rank")),
                b'p' | b'n' | b'b' | b'r' | b'q' | b'k' | b'P' | b'N' | b'B' | b'R' | b'Q' | b'K' | b'*' => width += 1,
                _ => return Err(invalid("invalid FEN board character")),
            }
        }
        if width != 8 { return Err(invalid("invalid FEN rank width")); }
    }
    let board = Board::from_fen(fen).ok_or_else(|| invalid("invalid initial PGN FEN"))?;
    for color in [Color::White, Color::Black] {
        for dir in CastlingDirection::ALL {
            if let Some(file) = board.castling_rights(color).get(dir) {
                let rook = Square::new(file, Rank::First.relative_to(color));
                if board.piece_on(rook) != Some(Piece::Rook) || board.color_on(rook) != Some(color) {
                    return Err(invalid("FEN castling right has no rook"));
                }
            }
        }
    }
    if let Some(ep) = board.en_passant() {
        let victim = Square::new(ep.file(), Rank::Fifth.relative_to(board.stm()));
        if board.piece_on(victim) != Some(Piece::Pawn) || board.color_on(victim) != Some(!board.stm()) {
            return Err(invalid("FEN en passant has no opposing pawn"));
        }
    }
    Ok(board)
}

// Resolve SAN or coordinate notation, including the duck placement.
fn parse_move(board: &Board, text: &str) -> io::Result<Move> {
    if !text.is_ascii() { return Err(invalid("non-ASCII move")); }
    let text = text.trim_end_matches(['+', '#', '!', '?']);
    let (piece_move, duck) = match text.rsplit_once('@') {
        Some((piece, duck)) => (piece, Some(square(duck).ok_or_else(|| invalid("invalid duck square"))?)),
        None => (text, None),
    };
    let mut piece_move = piece_move.trim_end_matches(['+', '#', '!', '?']);
    let castle = match piece_move {
        "O-O" | "0-0" => Some(CastlingDirection::Short),
        "O-O-O" | "0-0-0" => Some(CastlingDirection::Long),
        _ => None,
    };
    let promotion = if let Some((base, suffix)) = piece_move.rsplit_once('=') {
        piece_move = base;
        match suffix {
            "Q" | "q" => Some(Piece::Queen), "R" | "r" => Some(Piece::Rook),
            "B" | "b" => Some(Piece::Bishop), "N" | "n" => Some(Piece::Knight),
            _ => return Err(invalid("invalid promotion")),
        }
    } else { None };
    let coordinate = if piece_move.len() == 4 && square(&piece_move[..2]).is_some() {
        square(&piece_move[2..]).map(|dest| (square(&piece_move[..2]).unwrap(), dest))
    } else { None };
    let mut piece = Piece::Pawn;
    let mut destination = None;
    let mut source_file = None;
    let mut source_rank = None;
    let mut capture = false;
    if castle.is_none() && coordinate.is_none() {
        if piece_move.len() < 2 { return Err(invalid("invalid SAN move")); }
        destination = Some(square(&piece_move[piece_move.len() - 2..]).ok_or_else(|| invalid("invalid SAN destination"))?);
        let mut prefix = &piece_move[..piece_move.len() - 2];
        if let Some(first) = prefix.as_bytes().first() {
            piece = match first {
                b'K' => Piece::King, b'Q' => Piece::Queen, b'R' => Piece::Rook,
                b'B' => Piece::Bishop, b'N' => Piece::Knight, _ => Piece::Pawn,
            };
            if piece != Piece::Pawn { prefix = &prefix[1..]; }
        }
        for b in prefix.bytes() {
            match b {
                b'x' if !capture => capture = true,
                b'a'..=b'h' if source_file.is_none() => source_file = Some(ChessFile::index((b - b'a') as usize)),
                b'1'..=b'8' if source_rank.is_none() => source_rank = Some(Rank::index((b - b'1') as usize)),
                _ => return Err(invalid("invalid SAN disambiguation")),
            }
        }
    }
    // Match against generated moves and reject ambiguous notation.
    let mut found = None;
    let mut ambiguous = false;
    board.gen_all_moves(|moves| {
        let matches = if let Some(dir) = castle {
            moves.flag.castling_dir() == Some(dir)
        } else if let Some((src, dest)) = coordinate {
            moves.src == src && moves.dest == dest && moves.flag.promotion() == promotion
        } else {
            Some(moves.dest) == destination && board.piece_on(moves.src) == Some(piece)
                && moves.flag.is_capture() == capture && moves.flag.promotion() == promotion
                && !moves.flag.is_castling()
                && source_file.is_none_or(|file| moves.src.file() == file)
                && source_rank.is_none_or(|rank| moves.src.rank() == rank)
        };
        if !matches { return Abort::No; }
        // A final king capture may use the source square as its duck payload.
        let target = match duck {
            Some(sq) => sq,
            None if board.piece_on(moves.dest) == Some(Piece::King)
                && board.color_on(moves.dest) == Some(!board.stm()) => moves.src,
            None => return Abort::No,
        };
        if !moves.duck.has(target) { return Abort::No; }
        if found.is_some() { ambiguous = true; return Abort::Yes; }
        found = Some(Move::new(moves.src, moves.dest, target, moves.flag));
        Abort::No
    });
    if ambiguous { return Err(invalid(format!("ambiguous move {text}"))); }
    found.ok_or_else(|| invalid(format!("illegal move or missing duck placement: {text}")))
}

#[derive(Clone, Copy)]
enum Fill {
    None,
    Value(i16),
    Prev,
    Next,
}

#[derive(Clone, Copy)]
struct Options {
    fill: Fill,
    skip_broken: bool,
}

// Buffer a complete game so conversion errors cannot write a partial game.
fn convert(game: &Game, options: Options, bytes: &mut Vec<u8>, scores: &mut Vec<Option<i16>>) -> io::Result<u64> {
    let result = game.result.ok_or_else(|| invalid("unfinished game"))?;
    let mut board = initial_board(game.fen.as_deref())?;
    bytes.clear();
    bytes.extend_from_slice(&pack_header(&board, result)?);
    scores.clear();
    // Convert pre-move, side-to-move evaluations to White's perspective.
    scores.extend(game.plies.iter().enumerate().map(|(i, ply)| {
        let black = (board.stm() == Color::Black) ^ (i % 2 != 0);
        ply.eval.map(|value| if black { -value } else { value })
    }));
    let mut missing = 0;
    let mut carry = None;
    let mut fill_score = |score: &mut Option<i16>| {
        if score.is_none() {
            *score = match options.fill { Fill::Value(value) => Some(value), Fill::Prev | Fill::Next => carry, Fill::None => None };
            if score.is_some() { missing += 1; }
        }
        if score.is_some() { carry = *score; }
    };
    // Walk backwards when filling gaps from the next available score.
    if matches!(options.fill, Fill::Next) {
        scores.iter_mut().rev().for_each(&mut fill_score);
    } else {
        scores.iter_mut().for_each(&mut fill_score);
    }
    // Replay and validate each move before storing it with its score.
    for (i, ply) in game.plies.iter().enumerate() {
        if board.hmc() >= 100 { return Err(invalid("moves after fifty-move draw")); }
        let mv = parse_move(&board, ply.text()).map_err(|e| invalid(format!("ply {}: {e}", i + 1)))?;
        let score = scores[i].ok_or_else(|| invalid(format!("missing evaluation at ply {} (use --fill-missing-evals)", i + 1)))?;
        bytes.extend_from_slice(&MoveRecord { mv, white_score: score }.to_bytes());
        if board.piece_on(mv.dest()) == Some(Piece::King) && board.color_on(mv.dest()) == Some(!board.stm()) {
            let winner = if board.stm() == Color::White { GameOutcome::WhiteWin } else { GameOutcome::BlackWin };
            if i + 1 != game.plies.len() || result != winner {
                return Err(invalid("king capture must end the game with the matching result"));
            }
            break;
        }
        if i + 1 < game.plies.len() {
            if board.stm() == Color::Black && board.fmc() == u16::MAX {
                return Err(invalid("fullmove counter overflow"));
            }
            board.make_move(mv);
        }
    }
    bytes.extend_from_slice(&GAME_TERMINATOR);
    Ok(missing)
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
}

fn is_pgn(path: &Path) -> bool {
    has_extension(path, "pgn") || (has_extension(path, "bz2")
        && path.file_stem().is_some_and(|stem| has_extension(Path::new(stem), "pgn")))
}

struct GameReader<'a> {
    send: mpsc::SyncSender<(Arc<str>, u64, Game)>,
    failed: &'a AtomicBool,
    skipped: &'a AtomicU64,
    skip_broken: bool,
}

impl GameReader<'_> {
    fn pgn(&self, reader: impl Read, source: Arc<str>) -> io::Result<()> {
        let mut reader = BufReader::with_capacity(8 * 1024 * 1024, reader);
        if reader.fill_buf()?.starts_with(&[0xef, 0xbb, 0xbf]) { reader.consume(3); }
        let mut reader = Pgn { reader, token: Vec::with_capacity(256), pending_tag: false };
        let mut index = 0;
        while !self.failed.load(Ordering::Relaxed) {
            match reader.game() {
                Ok(Some(game)) => {
                    index += 1;
                    self.send.send((Arc::clone(&source), index, game))
                        .map_err(|_| io::Error::other("conversion workers disconnected"))?;
                }
                Ok(None) => break,
                Err(e) if self.skip_broken && e.kind() == io::ErrorKind::InvalidData => {
                    index += 1;
                    self.skipped.fetch_add(1, Ordering::Relaxed);
                    eprintln!("{source}: game {index}: {e}; skipped");
                    if !reader.recover()? { break; }
                }
                Err(e) => return Err(io::Error::new(e.kind(), format!("game {}: {e}", index + 1))),
            }
        }
        Ok(())
    }

    fn source(&self, reader: impl Read, path: &Path, source: Arc<str>) -> io::Result<()> {
        if has_extension(path, "bz2") {
            self.pgn(bzip2::read::MultiBzDecoder::new(reader), source)
        } else {
            self.pgn(reader, source)
        }
    }

    fn file(&self, path: &Path) -> io::Result<()> {
        let file = File::open(path)?;
        if has_extension(path, "tar") {
            let mut archive = tar::Archive::new(BufReader::with_capacity(1024 * 1024, file));
            let mut found = false;
            for entry in archive.entries()? {
                if self.failed.load(Ordering::Relaxed) { return Ok(()); }
                let entry = entry?;
                if !entry.header().entry_type().is_file() { continue; }
                let entry_path = entry.path()?.into_owned();
                if !is_pgn(&entry_path) { continue; }
                found = true;
                let source = format!("{}: {}", path.display(), entry_path.display()).into();
                self.source(entry, &entry_path, source)
                    .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", entry_path.display())))?;
            }
            if !found { return Err(invalid("archive contains no .pgn or .pgn.bz2 files")); }
            Ok(())
        } else {
            self.source(file, path, path.to_string_lossy().into_owned().into())
        }
    }
}

pub fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    // Parse conversion options and input/output paths.
    let mut args = args.into_iter().peekable();
    let mut input = None;
    let mut output = None;
    let mut overwrite = false;
    let mut allow_extension = false;
    let mut threads = std::thread::available_parallelism().map_or(1, usize::from);
    let mut options = Options { fill: Fill::None, skip_broken: false };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--input" | "-i" => input = Some(args.next().ok_or("missing input")?),
            "--output" | "-o" => output = Some(args.next().ok_or("missing output")?),
            "--threads" => threads = args.next().ok_or("missing thread count")?.parse()?,
            "--skip-broken-games" => options.skip_broken = cli::flag(&mut args),
            "--allow-non-pgn-extension" => allow_extension = cli::flag(&mut args),
            "--allow-overwrite" => overwrite = cli::flag(&mut args),
            "--fill-missing-evals" => {
                let value = args.next().ok_or("missing fill value")?;
                options.fill = match value.to_ascii_lowercase().as_str() {
                    "prev" => Fill::Prev, "next" => Fill::Next, _ => Fill::Value(value.parse()?),
                };
            }
            "--help" | "-h" | "help" => {
                println!("Usage: wakformat pgn2wf <INPUT> [OPTIONS]

Convert a PGN, .pgn.bz2, .tar archive, or folder to .wf.

Options:
  -i, --input <PATH>          Alternative to positional input
  -o, --output <PATH>         Output file (default: <INPUT>.wf or <FOLDER>/combined.wf)
      --allow-overwrite      Replace an existing output file
      --skip-broken-games    Skip games that cannot be converted
      --fill-missing-evals <i16|prev|next>
                            Fill missing evaluations
      --threads <N>          Worker count (default: available parallelism)
      --allow-non-pgn-extension
                            Accept other single-file extensions
  -h, --help                 Show this help

Example: wakformat pgn2wf games.pgn.tar -o games.wf");
                return Ok(());
            }
            _ if !arg.starts_with('-') && input.is_none() => input = Some(arg),
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    let input = input.ok_or("missing input; usage: wakformat pgn2wf <INPUT> [OPTIONS]")?;
    let input_path = Path::new(&input);
    let directory = input_path.metadata()?.is_dir();
    let inputs = if directory {
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(input_path)? {
            let entry = entry?;
            let path = entry.path();
            if is_pgn(&path) && path.metadata()?.is_file() { paths.push(path); }
        }
        paths.sort();
        if paths.is_empty() { return Err(format!("no .pgn or .pgn.bz2 files found in {}", input_path.display()).into()); }
        paths
    } else {
        if !allow_extension && !is_pgn(input_path) && !has_extension(input_path, "tar") {
            return Err("input must end in .pgn, .pgn.bz2, or .tar (or use --allow-non-pgn-extension)".into());
        }
        vec![input_path.to_path_buf()]
    };
    if threads == 0 || threads > 1024 { return Err("invalid thread count".into()); }
    let output = output.unwrap_or_else(|| {
        if directory { input_path.join("combined.wf").to_string_lossy().into_owned() }
        else { format!("{input}.wf") }
    });
    let started = Instant::now();
    let mut writer = BufWriter::with_capacity(8 * 1024 * 1024,
        cli::output(&output, &inputs, overwrite)?);
    writer.write_all(&FILE_HEADER)?;
    Board::startpos();
    let writer = Mutex::new(writer);
    // Limit queued games to keep parsing from outrunning the workers.
    let (send, receive) = mpsc::sync_channel::<(Arc<str>, u64, Game)>(threads * 2);
    let receive = Mutex::new(receive);
    let failed = AtomicBool::new(false);
    let error = Mutex::new(None::<String>);
    let games = AtomicU64::new(0);
    let positions = AtomicU64::new(0);
    let missing = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);
    // Workers convert independently and write whole games in completion order.
    std::thread::scope(|scope| {
        for _ in 0..threads {
            let (receive, writer, failed, error, games, positions, missing, skipped) =
                (&receive, &writer, &failed, &error, &games, &positions, &missing, &skipped);
            scope.spawn(move || {
                let mut bytes = Vec::with_capacity(4096);
                let mut scores = Vec::with_capacity(256);
                loop {
                    let job = receive.lock().unwrap().recv();
                    let Ok((source, index, game)) = job else { break; };
                    if failed.load(Ordering::Relaxed) { continue; }
                    let count = match convert(&game, options, &mut bytes, &mut scores) {
                        Ok(count) => count,
                        Err(e) if options.skip_broken => {
                            skipped.fetch_add(1, Ordering::Relaxed);
                            eprintln!("{source}: game {index}: {e}; skipped");
                            continue;
                        }
                        Err(e) => {
                            if !failed.swap(true, Ordering::Relaxed) {
                                *error.lock().unwrap() = Some(format!("{source}: game {index}: {e}"));
                            }
                            continue;
                        }
                    };
                    if let Err(e) = writer.lock().unwrap().write_all(&bytes) {
                        if !failed.swap(true, Ordering::Relaxed) {
                            *error.lock().unwrap() = Some(format!("writing output: {e}"));
                        }
                        continue;
                    }
                    games.fetch_add(1, Ordering::Relaxed);
                    positions.fetch_add(game.plies.len() as u64, Ordering::Relaxed);
                    missing.fetch_add(count, Ordering::Relaxed);
                }
            });
        }
        // Parse games sequentially and pass them to the worker queue.
        let reader = GameReader { send, failed: &failed, skipped: &skipped, skip_broken: options.skip_broken };
        for path in &inputs {
            if failed.load(Ordering::Relaxed) { break; }
            if let Err(e) = reader.file(path) {
                if !failed.swap(true, Ordering::Relaxed) {
                    *error.lock().unwrap() = Some(format!("{}: {e}", path.display()));
                }
                break;
            }
        }
        // Closing the queue lets workers finish and exit.
        drop(reader);
    });
    writer.into_inner().unwrap().flush()?;
    if let Some(error) = error.into_inner().unwrap() { return Err(error.into()); }
    let seconds = started.elapsed().as_secs_f64();
    eprintln!("broken games: {}\nparsed games: {}\ntotal games: {}\ntotal positions: {}\nfilled evals: {}\ntime taken: {seconds:.2}s\npositions/s: {:.0}",
        skipped.load(Ordering::Relaxed), games.load(Ordering::Relaxed),
        skipped.load(Ordering::Relaxed) + games.load(Ordering::Relaxed),
        positions.load(Ordering::Relaxed), missing.load(Ordering::Relaxed),
        positions.load(Ordering::Relaxed) as f64 / seconds.max(0.000001));
    Ok(())
}
