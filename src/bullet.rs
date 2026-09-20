use std::io;

use bulletformat::ChessBoard;

use crate::{
    board::Board,
    common::{Color, Move, Piece},
    format::{self, GameOutcome, MoveRecord},
};

pub const DUCK_EXTRA_INDEX: usize = 0;

pub fn to_bulletformat(board: &Board, white_score: i16, outcome: GameOutcome) -> io::Result<ChessBoard> {
    let bbs = [
        board.colors(Color::White).0,
        board.colors(Color::Black).0,
        board.pieces(Piece::Pawn).0,
        board.pieces(Piece::Knight).0,
        board.pieces(Piece::Bishop).0,
        board.pieces(Piece::Rook).0,
        board.pieces(Piece::Queen).0,
        board.pieces(Piece::King).0,
    ];
    if (bbs[0] | bbs[1]).count_ones() > 32
        || (bbs[0] & bbs[7]).count_ones() != 1
        || (bbs[1] & bbs[7]).count_ones() != 1
    {
        return Err(format::invalid("expected at most 32 chess pieces and both kings"));
    }

    let black = board.stm() == Color::Black;
    let mut position = ChessBoard::from_raw(bbs, usize::from(black), 0, outcome as u8 as f32 / 2.0)
        .map_err(format::invalid)?;
    position.score = if black { white_score.saturating_neg() } else { white_score };
    position.extra[DUCK_EXTRA_INDEX] = board.duck()
        .map_or(format::NO_SQUARE, |sq| sq as u8 ^ if black { 56 } else { 0 });
    Ok(position)
}

pub fn splat_to_bulletformat(
    game: &[u8],
    mut callback: impl FnMut(ChessBoard) -> io::Result<()>,
    mut filter: impl FnMut(&Board, Move, i16, f32) -> bool,
) -> io::Result<()> {
    if game.len() < format::GAME_HEADER_SIZE + format::RECORD_SIZE
        || !(game.len() - format::GAME_HEADER_SIZE).is_multiple_of(format::RECORD_SIZE)
        || game[game.len() - format::RECORD_SIZE..] != format::GAME_TERMINATOR
    {
        return Err(format::invalid("incomplete Wakformat game"));
    }

    let header = game[..format::GAME_HEADER_SIZE].try_into().unwrap();
    let mut board = format::unpack_header(header)?;
    let outcome = GameOutcome::try_from(game[format::header_offset::OUTCOME]).map_err(format::invalid)?;
    let white_result = outcome as u8 as f32 / 2.0;
    let records = &game[format::GAME_HEADER_SIZE..game.len() - format::RECORD_SIZE];

    for record in records.chunks_exact(format::RECORD_SIZE) {
        let record = MoveRecord::from_bytes(record.try_into().unwrap()).map_err(format::invalid)?
            .ok_or_else(|| format::invalid("unexpected Wakformat game terminator"))?;
        let position = if filter(&board, record.mv, record.white_score, white_result) {
            Some(to_bulletformat(&board, record.white_score, outcome)?)
        } else {
            None
        };
        format::replay_move(&mut board, record.mv)?;
        if let Some(position) = position {
            callback(position)?;
        }
    }
    Ok(())
}
