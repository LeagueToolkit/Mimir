# ltk_hashdb

The `.hashdb` binary format: a read-only table that maps integer hashes to strings. League
Toolkit uses it to turn WAD path hashes, bin hashes, and RST keys back into names. League
tables use the `.lhdb` extension.

A file has a fixed 80-byte header, a sorted array of keys, per-entry offsets and lengths,
and a string arena that is either raw or compressed with the zstd seekable format. Strings
are stored in path order, so paths in the same directory compress together.

- A lookup miss is decided by a binary search over the keys and never reads the arena.
- A hit on a compressed table decompresses only the frame that holds the string, and
  caches it for later lookups.
- Files are treated as untrusted: opening validates the header and section bounds, one
  lookup allocates at most a few `MAX_FRAME_SIZE` (1 MiB) frames, and `HashDb::verify`
  runs a full checksum pass.

The byte-level specification is
[`docs/FORMAT.md`](https://github.com/LeagueToolkit/mimir/blob/main/docs/FORMAT.md).

## Example

```rust
use std::io::Cursor;

use ltk_hashdb::{Casing, Compression, HashDb, HashDbWriter, HashKind, KeyConfig, KeyWidth};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = KeyConfig::new(KeyWidth::U64, HashKind::Xxh64, Casing::AsciiInsensitive);
    let path = "assets/characters/aatrox/aatrox.bin";

    let mut writer = HashDbWriter::with_key_config(config, Compression::default());
    writer.insert(config.hash(path), path);
    let mut file = Cursor::new(Vec::new());
    writer.build(&mut file)?;

    let db = HashDb::open_bytes(file.into_inner())?;
    let hash = db.hash_path("ASSETS/Characters/Aatrox/Aatrox.bin");
    assert_eq!(db.get(hash).as_deref(), Some(path));
    Ok(())
}
```

## Opening files

`HashDb::open` memory-maps the file. The mapping is only valid while the file does not
change, so never rewrite or truncate a `.hashdb` in place while it is open: write a new
file and rename it over the old one. `HashDb::open_bytes` takes an in-memory image
instead.

## Layering

`LayeredHashDb` puts a writable in-memory overlay in front of one or more tables that
share a `KeyConfig`, for example to add mod paths at runtime or to search the `game` and
`lcu` tables with one lookup.

## License

Apache-2.0.
