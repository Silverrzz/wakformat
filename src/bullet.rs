use std::io;

use bulletformat::ChessBoard;

use crate::{
    board::Board,
    common::{Color, Move, Piece, Square},
    format::{self, GameOutcome, MoveRecord},
};

pub const DUCK_EXTRA_INDEX: usize = 0;

pub fn to_bulletformat(board: &Board, white_score: i16, outcome: GameOutcome) -> io::Result<ChessBoard> {
    let us = board.colors(board.stm()).0;
    let them = board.colors(!board.stm()).0;
    let kings = board.pieces(Piece::King).0;
    let occupied = us | them;
    if occupied.count_ones() > 32
        || (us & kings).count_ones() != 1
        || (them & kings).count_ones() != 1
    {
        return Err(format::invalid("expected at most 32 chess pieces and both kings"));
    }

    let black = board.stm() == Color::Black;
    let flip = if black { 56 } else { 0 };
    let mut position = ChessBoard {
        occ: if black { occupied.swap_bytes() } else { occupied },
        pcs: [0; 16],
        score: if black { white_score.saturating_neg() } else { white_score },
        result: if black { 2 - outcome as u8 } else { outcome as u8 },
        ksq: (us & kings).trailing_zeros() as u8 ^ flip,
        opp_ksq: (them & kings).trailing_zeros() as u8 ^ flip ^ 56,
        extra: [board.duck().map_or(format::NO_SQUARE, |sq| sq as u8 ^ flip), 0, 0],
    };
    let mut remaining = position.occ;
    let mut index = 0;
    while remaining != 0 {
        let square = remaining.trailing_zeros() as u8 ^ flip;
        remaining &= remaining - 1;
        let piece = board.piece_on(Square::index(usize::from(square)))
            .ok_or_else(|| format::invalid("occupied square has no piece"))? as u8;
        let colour = u8::from(them & (1u64 << square) != 0) << 3;
        position.pcs[index / 2] |= (piece | colour) << (4 * (index & 1));
        index += 1;
    }
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
