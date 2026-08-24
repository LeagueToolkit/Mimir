//! [`PathRef`]: a resolved path, borrowed from wherever its bytes already live.

use std::borrow::Borrow;
use std::fmt;
use std::sync::Arc;

use crate::cache::Frame;

/// A path resolved from a table, without copying its bytes.
///
/// Derefs to [`str`], so it reads like one at the call site - `path.ends_with(".dds")`,
/// `&*path`, `path.to_owned()`, and `Option::as_deref` all work unchanged. What it adds
/// is where the bytes come from: a raw arena lends them straight out of the mmap, and a
/// compressed arena lends them out of a decompressed frame the table is already holding.
/// Neither allocates. Only two cases do: an entry that straddles a frame boundary, and
/// one whose bytes are not valid UTF-8 (replaced lossily, as [`HashDb::get`] has always
/// done).
///
/// A `PathRef` keeps its frame alive, so holding many of them across lookups pins that
/// many frames in memory. Call [`into_owned`](Self::into_owned) to detach one.
///
/// [`HashDb::get`]: crate::HashDb::get
#[derive(Clone)]
pub struct PathRef<'a> {
    repr: Repr<'a>,
}

#[derive(Clone)]
enum Repr<'a> {
    /// Straight out of a raw arena's mmap, or out of a `LayeredHashDb` overlay.
    Borrowed(&'a str),

    /// A range of a shared decompressed frame, kept alive by the `Arc`.
    ///
    /// The range is always valid UTF-8: `PathRef::from_frame` checks it and falls
    /// back to `Owned` when it is not.
    Frame {
        frame: Arc<Frame>,
        start: u32,
        len: u32,
    },

    /// Spliced across a frame boundary, or lossily replaced invalid UTF-8.
    Owned(Box<str>),
}

impl<'a> PathRef<'a> {
    /// Borrow a path that is already a `&str` (raw arena, overlay entry).
    pub(crate) fn borrowed(path: &'a str) -> Self {
        Self {
            repr: Repr::Borrowed(path),
        }
    }

    /// Take ownership of a path that had to be built (spliced or lossy).
    pub(crate) fn owned(path: impl Into<Box<str>>) -> Self {
        Self {
            repr: Repr::Owned(path.into()),
        }
    }

    /// Borrow `frame[start..start + len]`, keeping the frame alive.
    ///
    /// Falls back to an owned, lossily-replaced copy when those bytes are not valid
    /// UTF-8 - which also establishes the invariant [`Deref`](std::ops::Deref) relies on.
    pub(crate) fn from_frame(frame: Arc<Frame>, start: usize, len: usize) -> Self {
        let bytes = &frame.bytes()[start..start + len];
        match std::str::from_utf8(bytes) {
            Ok(_) => Self {
                repr: Repr::Frame {
                    start: start as u32,
                    len: len as u32,
                    frame,
                },
            },
            Err(_) => Self::owned(String::from_utf8_lossy(bytes).into_owned()),
        }
    }

    /// The path as a plain `&str`.
    pub fn as_str(&self) -> &str {
        match &self.repr {
            Repr::Borrowed(path) => path,
            Repr::Frame { frame, start, len } => {
                let bytes = &frame.bytes()[*start as usize..(*start + *len) as usize];
                // SAFETY: `Repr::Frame` is built only by `from_frame`, which validates
                // exactly this range as UTF-8 and takes the `Owned` branch when it fails.
                // The frame is immutable behind its `Arc`, so those bytes cannot change
                // between that check and this read.
                unsafe { std::str::from_utf8_unchecked(bytes) }
            }
            Repr::Owned(path) => path,
        }
    }

    /// Whether these bytes were copied rather than borrowed.
    ///
    /// False for the paths a table lends out directly - from a raw arena's mmap, from
    /// a cached frame, or from an overlay - which is every path in a well-formed table
    /// but the two exceptions: one spliced across a frame boundary, and one whose bytes
    /// were not valid UTF-8 and got lossily replaced. Assert on it to pin down that a
    /// hot path allocates nothing; [`into_owned`](Self::into_owned) is free when it is
    /// true.
    pub fn is_owned(&self) -> bool {
        matches!(self.repr, Repr::Owned(_))
    }

    /// Detach the path from its frame, copying it if it was borrowed.
    ///
    /// Use this to keep a handful of paths out of a large scan without pinning the
    /// frames they came from.
    pub fn into_owned(self) -> String {
        match self.repr {
            Repr::Owned(path) => path.into_string(),
            _ => self.as_str().to_owned(),
        }
    }
}

impl std::ops::Deref for PathRef<'_> {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for PathRef<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for PathRef<'_> {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// Prints the path itself, quoted - a `PathRef` is a string as far as a reader cares.
impl fmt::Debug for PathRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for PathRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq for PathRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for PathRef<'_> {}

impl PartialOrd for PathRef<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PathRef<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl std::hash::Hash for PathRef<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialEq<str> for PathRef<'_> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for PathRef<'_> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for PathRef<'_> {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<PathRef<'_>> for str {
    fn eq(&self, other: &PathRef<'_>) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<PathRef<'_>> for &str {
    fn eq(&self, other: &PathRef<'_>) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<PathRef<'_>> for String {
    fn eq(&self, other: &PathRef<'_>) -> bool {
        self.as_str() == other.as_str()
    }
}

impl From<PathRef<'_>> for String {
    fn from(path: PathRef<'_>) -> Self {
        path.into_owned()
    }
}

impl From<PathRef<'_>> for Box<str> {
    fn from(path: PathRef<'_>) -> Self {
        match path.repr {
            Repr::Owned(path) => path,
            _ => path.as_str().into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowed_and_owned_compare_and_print_alike() {
        let borrowed = PathRef::borrowed("assets/foo.dds");
        let owned = PathRef::owned("assets/foo.dds");

        assert_eq!(borrowed, owned);
        assert_eq!(borrowed, "assets/foo.dds");
        assert_eq!("assets/foo.dds", borrowed);
        assert_eq!(borrowed.to_string(), "assets/foo.dds");
        assert_eq!(format!("{owned:?}"), "\"assets/foo.dds\"");

        // Deref means str's whole API is available without an accessor.
        assert!(borrowed.ends_with(".dds"));
        assert_eq!(Some(owned).as_deref(), Some("assets/foo.dds"));
    }

    /// A frame-backed path borrows its bytes; invalid UTF-8 falls back to a lossy copy.
    #[test]
    fn frame_backed_paths_borrow_or_replace() {
        let frame = Arc::new(Frame::from(b"aa/one.binbb/two.bin".to_vec()));
        let path = PathRef::from_frame(Arc::clone(&frame), 10, 10);
        assert_eq!(path, "bb/two.bin");
        assert!(matches!(path.repr, Repr::Frame { .. }));

        let invalid = Arc::new(Frame::from(vec![b'a', 0xff, b'b']));
        let path = PathRef::from_frame(invalid, 0, 3);
        assert_eq!(path, "a\u{fffd}b");
        assert!(matches!(path.repr, Repr::Owned(_)));
    }
}
