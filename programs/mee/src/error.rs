use mee_core::EngineError;
use solana_program::program_error::ProgramError;

/// Program-level errors. Engine errors keep their own codes (1..1000); these
/// start at 1000 so the two never collide.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MeeError {
    InvalidInstruction = 1000,
    InvalidAccountOwner = 1001,
    InvalidAccountData = 1002,
    InvalidPda = 1003,
    AlreadyInitialized = 1004,
    Unauthorized = 1005,
    InvalidTokenAccount = 1006,
    InvalidTokenProgram = 1007,
    InvalidSystemProgram = 1008,
    BookMismatch = 1009,
    TooManyGuardians = 1010,
    SlippageExceeded = 1011,
    BridgeLocked = 1012,
    DisputeWindow = 1013,
}

impl From<MeeError> for ProgramError {
    fn from(e: MeeError) -> Self {
        ProgramError::Custom(e as u32)
    }
}

pub fn engine(e: EngineError) -> ProgramError {
    ProgramError::Custom(e.code())
}
