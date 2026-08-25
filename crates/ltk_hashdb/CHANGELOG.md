# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [0.1.0](https://github.com/LeagueToolkit/Mimir/releases/tag/ltk_hashdb-v0.1.0) - 2026-08-25

### Added

- *(hashdb)* add the optional arena-order section, with values and prefix over it
- *(hashdb, cli)* add verify_index, a checksum pass that skips the arena
- *(hashdb)* reserve an optional flags byte in the header
- *(cache)* [**breaking**] give Table its key config and hash universe
- *(hashdb)* [**breaking**] narrow Casing::Insensitive to AsciiInsensitive
- *(cache)* add HashStore::open_shared backed by a weak table register
- *(hashdb)* [**breaking**] add try_get, get_into, for_each_batch, and a health signal
- *(hashdb)* [**breaking**] return PathRef from lookups and cache decompressed frames
- *(hashdb, cache)* implement LayeredHashDB
- refactor monolithic error type
- implement  case sensitivity flag
- initial commit

### Fixed

- *(hashdb, cache)* [**breaking**] reject mismatched layered bases at runtime

### Other

- *(hashdb)* measure the arena-order trade and the path-factoring alternatives
- *(hashdb)* add a frame-cache point-vs-batch example
- *(hashdb)* [**breaking**] retire ExtendedHashDb in favour of LayeredHashDb
- *(hashdb)* change decompressions visibility
- implement release-plz
