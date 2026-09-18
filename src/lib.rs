//! Read and consolidate Macrium Reflect X backup sets.
//!
//! Macrium publishes the `.mrimgx` format under the MIT license, together with a reference
//! reader in C++. The reference SDK reads. It has no writer, and Macrium's own
//! `consolidate.exe` runs only on Windows. This crate is the missing writer, so a backup
//! set stored on a NAS can be consolidated from the machine that holds it.
//!
//! Logic lives in the library so it can be unit-tested. `main.rs` is a thin shell.
//!
//! Modules follow the read path: [`block`] frames the file, [`index`] decodes the block
//! index, [`json`] handles the metadata document, and [`reader`] ties the three together.

pub mod block;
pub mod cli;
pub mod commit;
pub mod crypto;
pub mod index;
pub mod json;
pub mod plan;
pub mod reader;
pub mod scan;
pub mod set;
pub mod verify;
pub mod write;
