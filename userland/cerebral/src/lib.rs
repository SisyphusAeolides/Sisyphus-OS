#![no_std]

extern crate alloc;

pub mod axon;
pub mod nexus_reactor;

pub use axon::{Axon, Dendrite, NeuromorphicRouter, Synapse};
