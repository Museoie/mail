//! Tests of the pure half of the IMAP server.
//!
//! `golden` replays the fixtures in `tests/golden/imap`, which record what the
//! Bun server's own code returned for a large corpus (see `generate.ts`
//! there). The other modules are the Bun server's unit tests for the same
//! code, case for case.

mod fetch;
mod golden;
mod misc;
mod parsers;
mod sequence_resolver;
mod support;
mod util;
