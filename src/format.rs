use crate::board::{Board, CastlingDirection};
use crate::common::{Color, Move, MoveFlag, Piece, Rank, Square};
use std::io::{self, BufRead, Read};
use std::fmt;

pub const VERSION: u16 = 1;
pub const MAGIC: [u8; 4] = *b"WAKF";
pub const FILE_HEADER_SIZE: usize = 8;
pub const FILE_HEADER: [u8; FILE_HEADER_SIZE] = [b'W', b'A', b'K', b'F', 1, 0, 0, 0];
pub const GAME_HEADER_SIZE: usize = 32;
pub const MOVE_SIZE: usize = 3;
pub const SCORE_SIZE: usize = 2;
pub const RECORD_SIZE: usize = MOVE_SIZE + SCORE_SIZE;
pub const GAME_TERMINATOR: [u8; RECORD_SIZE] = [0; RECORD_SIZE];
pub const NO_SQUARE: u8 = 64;
pub const MAX_PIECES: usize = 32;
pub const MOVE_BITS: u32 = (1 << 22) - 1;
pub type GameHeaderBytes = [u8; GAME_HEADER_SIZE];

pub mod header_offset {
    pub const OCCUPANCY: usize = 0;
    pub const PIECES: usize = 8;
    pub const SIDE_TO_MOVE_AND_EN_PASSANT: usize = 24;
    pub const HALFMOVE_CLOCK: usize = 25;
    pub const FULLMOVE_NUMBER: usize = 26;
    pub const RESERVED_SCORE: usize = 28;
    pub const OUTCOME: usize = 30;
    pub const DUCK: usize = 31;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GameOutcome {
    BlackWin = 0,
    Draw = 1,
    WhiteWin = 2,
}

impl TryFrom<u8> for GameOutcome {
    type Error = FormatError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::BlackWin),
            1 => Ok(Self::Draw),
            2 => Ok(Self::WhiteWin),
            _ => Err(FormatError::InvalidOutcome(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveRecord {
    pub mv: Move,
    pub white_score: i16,
}

impl MoveRecord {
    pub const fn to_bytes(self) -> [u8; RECORD_SIZE] {
        let mv = encode_move(self.mv);
        let score = self.white_score.to_le_bytes();
        [mv[0], mv[1], mv[2], score[0], score[1]]
    }

    pub fn from_bytes(bytes: [u8; RECORD_SIZE]) -> Result<Option<Self>, FormatError> {
        if bytes == GAME_TERMINATOR {
            return Ok(None);
        }
        let mv = decode_move([bytes[0], bytes[1], bytes[2]])?;
        let white_score = i16::from_le_bytes([bytes[3], bytes[4]]);
        Ok(Some(Self { mv, white_score }))
    }
}

pub fn validate_file_header(bytes: [u8; FILE_HEADER_SIZE]) -> Result<(), FormatError> {
    if bytes[..4] != MAGIC {
        return Err(FormatError::InvalidMagic);
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != VERSION {
        return Err(FormatError::UnsupportedVersion(version));
    }
    let flags = u16::from_le_bytes([bytes[6], bytes[7]]);
    if flags != 0 {
        return Err(FormatError::ReservedFileFlags(flags));
    }
    Ok(())
}

pub const fn encode_move(mv: Move) -> [u8; MOVE_SIZE] {
    let bytes = mv.raw().get().to_le_bytes();
    [bytes[0], bytes[1], bytes[2]]
}

pub fn decode_move(bytes: [u8; MOVE_SIZE]) -> Result<Move, FormatError> {
    let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]);
    if raw & !MOVE_BITS != 0 || raw & 63 == (raw >> 6) & 63 {
        return Err(FormatError::InvalidMove(raw));
    }
    let flag = match raw >> 18 {
        0 => MoveFlag::Normal,
        1 => MoveFlag::DoublePush,
        2 => MoveFlag::LongCastling,
        3 => MoveFlag::ShortCastling,
        4 => MoveFlag::PromotionQueen,
        5 => MoveFlag::PromotionRook,
        6 => MoveFlag::PromotionBishop,
        7 => MoveFlag::PromotionKnight,
        8 => MoveFlag::Capture,
        9 => MoveFlag::EnPassant,
        12 => MoveFlag::CapturePromotionQueen,
        13 => MoveFlag::CapturePromotionRook,
        14 => MoveFlag::CapturePromotionBishop,
        15 => MoveFlag::CapturePromotionKnight,
        _ => return Err(FormatError::InvalidMove(raw)),
    };
    Ok(Move::new(
        Square::index((raw & 63) as usize),
        Square::index(((raw >> 6) & 63) as usize),
        Square::index(((raw >> 12) & 63) as usize),
        flag,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    InvalidMagic,
    UnsupportedVersion(u16),
    ReservedFileFlags(u16),
    InvalidMove(u32),
    InvalidOutcome(u8),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMagic => f.write_str("invalid Wakformat magic"),
            Self::UnsupportedVersion(version) => write!(f, "unsupported Wakformat version {version}"),
            Self::ReservedFileFlags(flags) => write!(f, "nonzero Wakformat v1 file flags: {flags:#06x}"),
            Self::InvalidMove(raw) => write!(f, "invalid Wakformat v1 move: {raw:#08x}"),
            Self::InvalidOutcome(value) => write!(f, "invalid Wakformat outcome: {value}"),
        }
    }
}

impl std::error::Error for FormatError {}


pub fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn read_file_header(reader: &mut impl Read) -> io::Result<()> {
    let mut bytes = [0; FILE_HEADER_SIZE];
    reader.read_exact(&mut bytes)?;
    validate_file_header(bytes).map_err(invalid)
}

pub fn pack_header(board: &Board, outcome: GameOutcome) -> io::Result<GameHeaderBytes> {
    let mut bytes = [0; GAME_HEADER_SIZE];
    let occupied = board.colors(Color::White) | board.colors(Color::Black);
    if occupied.popcnt() > MAX_PIECES {
        return Err(invalid("more than 32 chess pieces"));
    }
    bytes[..8].copy_from_slice(&occupied.0.to_le_bytes());
    for (i, sq) in occupied.into_iter().enumerate() {
        let piece = board.piece_on(sq).ok_or_else(|| invalid("inconsistent board occupancy"))?;
        let color = board.color_on(sq).ok_or_else(|| invalid("piece has no color"))?;
        let castling = piece == Piece::Rook && sq.rank() == Rank::First.relative_to(color)
            && CastlingDirection::ALL.into_iter().any(|dir| board.castling_rights(color).get(dir) == Some(sq.file()));
        let code = if castling { 6 } else { piece as u8 } | (color as u8) << 3;
        bytes[8 + i / 2] |= code << ((i & 1) * 4);
    }
    bytes[24] = (board.stm() as u8) << 7 | board.en_passant().map_or(NO_SQUARE, |ep| {
        Square::new(ep.file(), Rank::Sixth.relative_to(board.stm())) as u8
    });
    bytes[25] = board.hmc();
    bytes[26..28].copy_from_slice(&board.fmc().to_le_bytes());
    bytes[30] = outcome as u8;
    bytes[31] = board.duck().map_or(NO_SQUARE, |sq| sq as u8);
    validate_game_header(&bytes)?;
    Ok(bytes)
}

pub fn validate_game_header(bytes: &GameHeaderBytes) -> io::Result<()> {
    let occupied = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let count = occupied.count_ones() as usize;
    if count == 0 || count > MAX_PIECES || bytes[31] > NO_SQUARE
        || bytes[24] & 127 > NO_SQUARE || bytes[30] > 2
        || u16::from_le_bytes([bytes[26], bytes[27]]) == 0
        || bytes[28] != 0 || bytes[29] != 0
    {
        return Err(invalid("invalid Wakformat game header"));
    }
    if bytes[31] < NO_SQUARE && occupied & (1u64 << bytes[31]) != 0 {
        return Err(invalid("duck overlaps a chess piece"));
    }
    let mut kings = [0u8; 2];
    for i in 0..MAX_PIECES {
        let code = (bytes[8 + i / 2] >> ((i & 1) * 4)) & 15;
        if (i >= count && code != 0) || (i < count && code & 7 == 7) {
            return Err(invalid("invalid packed piece"));
        }
        if i < count && code & 7 == 5 {
            kings[(code >> 3) as usize] += 1;
        }
    }
    if kings != [1, 1] {
        return Err(invalid("initial position must contain both kings"));
    }
    Ok(())
}

pub fn read_game(reader: &mut impl BufRead, bytes: &mut Vec<u8>) -> io::Result<bool> {
    bytes.clear();
    if reader.fill_buf()?.is_empty() {
        return Ok(false);
    }
    bytes.resize(GAME_HEADER_SIZE, 0);
    reader.read_exact(bytes)?;
    validate_game_header(bytes.as_slice().try_into().unwrap())?;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unterminated Wakformat game"));
        }
        if available.len() < RECORD_SIZE {
            let mut record = [0; RECORD_SIZE];
            reader.read_exact(&mut record)?;
            bytes.extend_from_slice(&record);
            if record == GAME_TERMINATOR {
                return Ok(true);
            }
        } else {
            let records = available.chunks_exact(RECORD_SIZE);
            let end = records.clone().position(|record| record == GAME_TERMINATOR);
            let take = end.map_or(records.len(), |i| i + 1) * RECORD_SIZE;
            bytes.extend_from_slice(&available[..take]);
            reader.consume(take);
            if end.is_some() {
                return Ok(true);
            }
        }
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(invalid("Wakformat game exceeds 16 MiB"));
        }
    }
}


pub fn unpack_header(bytes: &GameHeaderBytes) -> io::Result<Board> {
    use std::fmt::Write;
    validate_game_header(bytes)?;
    let mut squares = [b' '; 64];
    let mut occupancy = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let mut rights = String::with_capacity(4);
    let mut index = 0;
    while occupancy != 0 {
        let sq = occupancy.trailing_zeros() as usize;
        occupancy &= occupancy - 1;
        let code = (bytes[8 + index / 2] >> ((index & 1) * 4)) & 15;
        index += 1;
        let black = code & 8 != 0;
        let kind = code & 7;
        if kind == 0 && (sq / 8 == 0 || sq / 8 == 7) {
            return Err(invalid("pawn on back rank"));
        }
        let piece = b"pnbrqkr"[kind as usize];
        squares[sq] = if black { piece } else { piece.to_ascii_uppercase() };
        if kind == 6 {
            if sq / 8 != if black { 7 } else { 0 } {
                return Err(invalid("castling rook off back rank"));
            }
            let file = b'a' + (sq % 8) as u8;
            rights.push(if black { file as char } else { file.to_ascii_uppercase() as char });
        }
    }
    if bytes[31] < NO_SQUARE { squares[bytes[31] as usize] = b'*'; }
    let mut fen = String::with_capacity(96);
    for rank in (0..8).rev() {
        let mut empty = 0u8;
        for file in 0..8 {
            let piece = squares[rank * 8 + file];
            if piece == b' ' { empty += 1; }
            else {
                if empty != 0 { fen.push((b'0' + empty) as char); empty = 0; }
                fen.push(piece as char);
            }
        }
        if empty != 0 { fen.push((b'0' + empty) as char); }
        if rank != 0 { fen.push('/'); }
    }
    let stm = if bytes[24] & 128 == 0 { 'w' } else { 'b' };
    write!(fen, " {stm} {} ", if rights.is_empty() { "-" } else { &rights }).unwrap();
    if bytes[24] & 127 == NO_SQUARE { fen.push('-'); }
    else { write!(fen, "{}", Square::index((bytes[24] & 127) as usize)).unwrap(); }
    write!(fen, " {} {}", bytes[25], u16::from_le_bytes([bytes[26], bytes[27]])).unwrap();
    let board = Board::from_fen(&fen).ok_or_else(|| invalid("invalid packed board"))?;
    if let Some(ep) = board.en_passant() {
        let victim = Square::new(ep.file(), Rank::Fifth.relative_to(board.stm()));
        if board.piece_on(victim) != Some(Piece::Pawn) || board.color_on(victim) != Some(!board.stm()) {
            return Err(invalid("en passant has no opposing pawn"));
        }
    }
    Ok(board)
}

pub fn replay_move(board: &mut Board, mv: Move) -> io::Result<()> {
    let piece = board.piece_on(mv.src()).ok_or_else(|| invalid("empty move source"))?;
    if board.try_king(board.stm()).is_none() || board.try_king(!board.stm()).is_none()
        || board.hmc() >= 100 || (board.stm() == Color::Black && board.fmc() == u16::MAX)
        || (piece == Piece::Pawn && matches!(mv.src().rank(), Rank::First | Rank::Eighth))
        || (piece == Piece::Pawn && mv.flag().is_promotion() != (mv.dest().rank() == Rank::Eighth.relative_to(board.stm())))
        || !board.is_legal(mv)
    {
        return Err(invalid("invalid move during replay"));
    }
    board.make_move(mv);
    Ok(())
}
