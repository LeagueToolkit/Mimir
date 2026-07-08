//! Path-slicing helpers shared by the guessers.
//!
//! All splitting happens at ASCII separator bytes, so every returned span is a
//! valid UTF-8 boundary even on non-ASCII paths.

/// `(directory including the trailing '/', basename)`.
pub(crate) fn split_dir(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i + 1], &path[i + 1..]),
        None => ("", path),
    }
}

/// `(stem, extension including the leading '.')`, splitting the basename at its
/// last dot. A leading dot doesn't count as an extension separator.
pub(crate) fn split_ext(basename: &str) -> (&str, &str) {
    match basename.rfind('.') {
        Some(i) if i > 0 => (&basename[..i], &basename[i..]),
        _ => (basename, ""),
    }
}

/// Byte spans of the `_ - .`-separated tokens of `stem`, in order.
pub(crate) fn token_spans(stem: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (i, b) in stem.bytes().enumerate() {
        if matches!(b, b'_' | b'-' | b'.') {
            if i > start {
                spans.push((start, i));
            }
            start = i + 1;
        }
    }
    if stem.len() > start {
        spans.push((start, stem.len()));
    }

    spans
}

/// Byte spans of the ASCII-digit runs of `path`, in order.
pub(crate) fn digit_runs(path: &str) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, b) in path.bytes().enumerate() {
        match (b.is_ascii_digit(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, path.len()));
    }

    runs
}

/// Is byte `b` (or a string end, `None`) a token boundary?
fn is_boundary(b: Option<u8>) -> bool {
    matches!(b, None | Some(b'/' | b'_' | b'-' | b'.' | b' '))
}

/// Replace every boundary-delimited occurrence of `from` in `path` with `to`,
/// into `buf`. Returns whether anything was replaced.
pub(crate) fn replace_token(path: &str, from: &str, to: &str, buf: &mut String) -> bool {
    buf.clear();
    let mut cursor = 0;
    let mut replaced = false;
    while let Some(rel) = path[cursor..].find(from) {
        let start = cursor + rel;
        let end = start + from.len();
        let before = (start > 0).then(|| path.as_bytes()[start - 1]);
        let bounded = is_boundary(before) && is_boundary(path.as_bytes().get(end).copied());
        if bounded {
            buf.push_str(&path[cursor..start]);
            buf.push_str(to);
            replaced = true;
        } else {
            buf.push_str(&path[cursor..end]);
        }
        cursor = end;
    }

    buf.push_str(&path[cursor..]);
    replaced
}

/// The segment following a `/`-bounded `characters/` component, if it looks
/// like a champion directory name.
pub(crate) fn champ_of(path: &str) -> Option<&str> {
    let mut search = 0;
    while let Some(rel) = path[search..].find("characters/") {
        let at = search + rel;
        search = at + "characters/".len();
        if at > 0 && path.as_bytes()[at - 1] != b'/' {
            continue;
        }

        let champ = path[search..].split('/').next().unwrap_or("");
        if !champ.is_empty() && champ.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Some(champ);
        }
    }

    None
}

/// Number of decimal digits of `n`.
pub(crate) fn dec_len(n: u32) -> usize {
    if n == 0 {
        1
    } else {
        n.ilog10() as usize + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits() {
        assert_eq!(split_dir("a/b/c.dds"), ("a/b/", "c.dds"));
        assert_eq!(split_dir("c.dds"), ("", "c.dds"));
        assert_eq!(split_ext("c.dds"), ("c", ".dds"));
        assert_eq!(split_ext("noext"), ("noext", ""));
        assert_eq!(split_ext(".hidden"), (".hidden", ""));
        assert_eq!(split_ext("a.b.c"), ("a.b", ".c"));
    }

    #[test]
    fn tokens_and_digits() {
        assert_eq!(
            token_spans("aatrox_base-cast"),
            vec![(0, 6), (7, 11), (12, 16)]
        );
        assert_eq!(token_spans("__x"), vec![(2, 3)]);
        assert_eq!(digit_runs("skin01/a2b34"), vec![(4, 6), (8, 9), (10, 12)]);
        assert_eq!(dec_len(0), 1);
        assert_eq!(dec_len(9), 1);
        assert_eq!(dec_len(10), 2);
        assert_eq!(dec_len(4200), 4);
    }

    #[test]
    fn token_replacement() {
        let mut buf = String::new();
        assert!(replace_token(
            "a/ahri/ahri_skin.dds",
            "ahri",
            "zed",
            &mut buf
        ));
        assert_eq!(buf, "a/zed/zed_skin.dds");
        // Unbounded occurrences stay untouched.
        assert!(!replace_token("a/kahrix/x.dds", "ahri", "zed", &mut buf));
        assert_eq!(buf, "a/kahrix/x.dds");
        // Occurrence at the very start and end.
        assert!(replace_token("ahri.bin", "ahri", "zed", &mut buf));
        assert_eq!(buf, "zed.bin");
    }
}
