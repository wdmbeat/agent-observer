//! Stage 1 of the Rust port of the agent-observer starter kit: the data layer.
//!
//! Modules mirror the Python `challenge/` package one-to-one so later stages
//! (scorer, workflow, agent) can port line-by-line.

pub mod agent;
pub mod calendar;
pub mod contracts;
pub mod geometry;
pub mod requests;
pub mod scoring;
pub mod transport;
pub mod weather;
pub mod workflow;
