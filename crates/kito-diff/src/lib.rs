//! `kito-diff`: producing the changes.
//!
//! For now it contains the text differ ([`text`]), used by the `TextEdit` ops.
//! The AST differ with stable-ID matching arrives with phase 2.

pub mod fileops;
pub mod text;
