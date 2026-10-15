# Change Log

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](http://keepachangelog.com/)
and this project adheres to [Semantic Versioning](http://semver.org/).

## Unreleased

### Added

- Added support for a persistent firmware identifier to help hosts identify the running binary and select the correct ELF for decoding.
- Provide list of caching-sensitive memory
- Iterate over caching-sensitive memory
- `GrantR::identifier` to tell recovered logs from different firmwares apart
- `Consumer::next_log` to read logs together with their identifier

### Fixed

- doc comment for `Consumer::read`

### Changed

- **Breaking:** `init` takes an `Identifier`
- **Breaking:** `init` returns `Consumer`, `ConsumerAndMetadata` is removed

## v0.1.0

### Added

- Initial release

