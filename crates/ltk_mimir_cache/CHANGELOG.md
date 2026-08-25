# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [0.1.0](https://github.com/LeagueToolkit/Mimir/releases/tag/ltk_mimir_cache-v0.1.0) - 2026-08-25

### Added

- UpdateObserver API
- *(cache, cli)* name the update lock holder and add a bounded wait
- *(cache, cli)* add a lock-free HashStore::check and mimir check
- *(cache)* [**breaking**] put the version and a parsed timestamp on the manifest
- *(cache)* [**breaking**] make the manifest and format gates survivable
- *(cache)* [**breaking**] make Table non_exhaustive with Display, FromStr, and serde
- *(cache)* [**breaking**] give Table its key config and hash universe
- *(cache)* add HashStore::open_shared backed by a weak table register
- *(cache, cli)* add fetch glue
- *(hashdb, cache)* implement LayeredHashDB
- *(cache)* add fetcher error type
- *(cache)* add update_async
- refactor monolithic error type
- bunch of improvements to cache crate
- add cache updater
- initial commit

### Fixed

- *(cache)* [**breaking**] record provenance per table instead of per manifest
- *(hashdb, cache)* [**breaking**] reject mismatched layered bases at runtime

### Other

- better cache docs
- *(cache)* [**breaking**] stream downloads into the cache instead of buffering and copying
- add NOTICE naming the copyright holder
- license under Apache-2.0 only
- implement release-plz
