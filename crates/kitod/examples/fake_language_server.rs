//! The language server of the tests (see `kito-semantic`): every place a
//! word is written is a reference to it.

#[path = "../../kito-semantic/examples/fake_lsp.rs"]
mod server;

fn main() {
    server::main()
}
