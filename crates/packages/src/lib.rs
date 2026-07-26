//! Package home, lockfile, and template gallery (chapter C extraction).
//!
//! Both modules import through the shared [`installer`] crate: a template is
//! package type 0, and an E2 design-data package is the same mechanism pointed
//! at a `.penpot` source tree instead of shipped binfile bytes.

pub mod packages;
pub mod templates;
