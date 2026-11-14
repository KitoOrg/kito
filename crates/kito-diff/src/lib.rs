//! `kito-diff`: producing the changes.
//!
//! * [`text`]: the text differ, used by the `TextEdit` ops.
//! * [`fileops`]: from a change in the state of the files to `File*` and
//!   `TextEdit` ops.
//! * [`ast`]: the AST differ, which keeps symbol IDs stable across versions
//!   and produces the symbolic ops.

pub mod ast;
pub mod fileops;
pub mod text;
