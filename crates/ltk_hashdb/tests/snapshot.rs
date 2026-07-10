//! Snapshot of the exact bytes of a small fixture, pinning the on-disk layout.
//! A change here is a format change and must be deliberate (version bump / spec update).

use std::io::Cursor;

use ltk_hashdb::{Casing, Compression, HashDbWriter, HashKind, KeyWidth};

fn hex_dump(bytes: &[u8]) -> String {
    bytes
        .chunks(16)
        .enumerate()
        .map(|(i, chunk)| {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            format!("{:04x}: {}", i * 16, hex.join(" "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn fixture_file_bytes() {
    let mut w = HashDbWriter::new(KeyWidth::U64, Compression::None)
        .hash_kind(HashKind::Xxh64)
        .casing(Casing::Insensitive);
    w.insert(0x0123_4567_89ab_cdef, "assets/a.dds");
    w.insert(0x0000_0000_0000_0042, "data/b.bin");
    let mut out = Cursor::new(Vec::new());
    w.build(&mut out).expect("build");

    insta::assert_snapshot!(hex_dump(out.get_ref()));
}

#[test]
fn fixture_u32_header() {
    let mut w = HashDbWriter::new(KeyWidth::U32, Compression::None)
        .hash_kind(HashKind::Fnv1a32)
        .casing(Casing::Insensitive);
    w.insert(0xafd0_71e5, "test");
    let mut out = Cursor::new(Vec::new());
    w.build(&mut out).expect("build");

    insta::assert_snapshot!(hex_dump(&out.get_ref()[..ltk_hashdb::HEADER_SIZE]));
}
