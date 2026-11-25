use std::{fmt, io};

/// One variant per Go error. Go's `flate.CorruptInputError(off)` is [`Error::Corrupt`], and so on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `flate.CorruptInputError`: the byte offset in the DEFLATE stream where Go reports it.
    Corrupt(u64),
    /// `io.ErrUnexpectedEOF`.
    UnexpectedEof,
    /// `flate.InternalError`.
    Internal(&'static str),
    /// `zlib.ErrHeader`.
    Header,
    /// `zlib.ErrChecksum`.
    Checksum,
    /// `zlib.ErrDictionary`.
    Dictionary,
    /// An error from the caller's reader, as its kind. Go passes the value through. A reader
    /// returns `Interrupted` as `io::ErrorKind::Other`: the error is final, not to be retried.
    Io(io::ErrorKind),
    /// The output would pass `Limits::max_output` (the predictor: the bytes it processes).
    OutputLimit,
    /// The working memory would pass `Limits::max_alloc`.
    AllocLimit,
    /// The allocator refused a size that is within the limits.
    OutOfMemory,
    /// The cancel flag was set.
    Cancelled,
    /// predictor: a slice is too short, `spp` or `bytes_per_sample` is not supported, or a size
    /// overflows. Go panics.
    InvalidArgument,
}

impl Error {
    /// Errors that Go's stdlib reports about the stream or its reader.
    pub(crate) fn is_stream(&self) -> bool {
        matches!(
            self,
            Self::Corrupt(_) | Self::UnexpectedEof | Self::Internal(_) | Self::Io(_)
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt(off) => write!(f, "flate: corrupt input before offset {off}"),
            Self::UnexpectedEof => f.write_str("unexpected EOF"),
            Self::Internal(s) => write!(f, "flate: internal error: {s}"),
            Self::Header => f.write_str("zlib: invalid header"),
            Self::Checksum => f.write_str("zlib: invalid checksum"),
            Self::Dictionary => f.write_str("zlib: invalid dictionary"),
            Self::Io(kind) => write!(f, "read error: {kind}"),
            Self::OutputLimit => f.write_str("deflate: output limit exceeded"),
            Self::AllocLimit => f.write_str("deflate: memory limit exceeded"),
            Self::OutOfMemory => f.write_str("deflate: out of memory"),
            Self::Cancelled => f.write_str("deflate: cancelled"),
            Self::InvalidArgument => f.write_str("predictor: invalid argument"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::UnexpectedEof => Self::UnexpectedEof,
            kind => Self::Io(kind),
        }
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        let kind = match e {
            Error::UnexpectedEof => io::ErrorKind::UnexpectedEof,
            // Sticky, so a retry (read_to_end retries Interrupted) would loop forever.
            Error::Io(io::ErrorKind::Interrupted) => io::ErrorKind::Other,
            Error::Io(kind) => kind,
            Error::OutOfMemory => io::ErrorKind::OutOfMemory,
            // Not Interrupted: the error is final, and `read_to_end` retries that.
            Error::Cancelled => io::ErrorKind::Other,
            _ => io::ErrorKind::InvalidData,
        };
        io::Error::new(kind, e)
    }
}
