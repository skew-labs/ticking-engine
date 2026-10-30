use core::fmt;

/// Engine errors. The discriminant is stable: the program surfaces it as
/// `ProgramError::Custom(code)`.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EngineError {
    MathOverflow = 1,
    InvalidMarket = 2,
    MarketNotActive = 3,
    MarketNotHalted = 4,
    InvalidPrice = 5,
    InvalidSize = 6,
    TickSize = 7,
    PriceBand = 8,
    OracleStale = 9,
    OracleMoveTooLarge = 10,
    OracleTooFrequent = 11,
    PostOnlyWouldCross = 12,
    BookFull = 13,
    OrderNotFound = 14,
    NotOrderOwner = 15,
    InsufficientMargin = 16,
    InsufficientCollateral = 17,
    OpenInterestCap = 18,
    NotLiquidatable = 19,
    NoPosition = 20,
    SelfLiquidation = 21,
    TraderNotFound = 22,
    TraderExists = 23,
    TradersFull = 24,
    MarketsFull = 25,
    InvalidMarketParams = 26,
    FeeShareTooHigh = 27,
    FundingNotDue = 28,
    StakeTooLow = 29,
    StakeLocked = 30,
    UnstakeNotRequested = 31,
    UnstakeDelay = 32,
    MarketsOpen = 33,
    DexNotActive = 34,
    AlreadyVoted = 35,
    ProposalMismatch = 36,
    NoProposal = 37,
    ThresholdNotMet = 38,
    InvalidGuardian = 39,
    TraderNotEmpty = 40,
    InvalidAmount = 41,
    UnstakePending = 42,
    MarketNotSettled = 43,
}

impl EngineError {
    pub fn code(self) -> u32 {
        self as u32
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

pub type Result<T> = core::result::Result<T, EngineError>;
