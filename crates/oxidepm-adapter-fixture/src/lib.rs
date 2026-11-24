//! A test adapter that speaks the real protocol through
//! `oxidepm-adapter-proto`, behaves as a script says, and writes down what it saw. It is a
//! workspace member and is never shipped. Every host test drives it.
//!
//! - `FIXTURE_SCRIPT` names a JSON file: a list of rules `{on, times?, after?, do}` (see [`script`]).
//! - `FIXTURE_REPORT` names a JSON Lines file the fixture appends to (see [`report`]).
//! - With no script the fixture is a well-behaved adapter.

pub mod child;
pub mod engine;
pub mod report;
pub mod script;
