#![no_std]

extern crate alloc;

pub mod axon;

pub use axon::{Axon, Dendrite, NeuromorphicRouter, Synapse};
