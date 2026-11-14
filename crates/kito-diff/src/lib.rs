//! `kito-diff`: produzione delle modifiche.
//!
//! Per ora contiene il differ testuale ([`text`]), usato dalle op `TextEdit`.
//! Il differ AST con il matching degli ID stabili arriva con la fase 2.

pub mod fileops;
pub mod text;
