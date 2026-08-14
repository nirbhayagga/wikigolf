//! wiki-parser as a library, so tools other than the parser binary can reuse
//! it.
//!
//! The pathfinder in particular *must* share `titles::normalize_title`: if a
//! user's search normalized differently than the graph was built with, lookups
//! would miss articles that are present, and the two would disagree about
//! which strings name the same article. Duplicating those rules is how they
//! drift apart.

pub mod dump;
pub mod edges;
pub mod game;
pub mod graph;
pub mod ratelimit;
pub mod runs;
pub mod index;
pub mod output;
pub mod titles;
pub mod wikitext;
