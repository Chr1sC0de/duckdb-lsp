use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::Service;
use tower_lsp::jsonrpc::{Request, Response};
use tower_lsp::{LspService, Server};

// tower-lsp 0.20 stops its service on exit but keeps reading stdin. Editors
// are allowed to keep that pipe open while waiting for the process to exit.
struct ExitService {
    inner: LspService<duckdb_lsp::server::Backend>,
    shutdown: bool,
}
impl Service<Request> for ExitService {
    type Response = Option<Response>;
    type Error = <LspService<duckdb_lsp::server::Backend> as Service<Request>>::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }
    fn call(&mut self, req: Request) -> Self::Future {
        let exit = req.method() == "exit" && req.id().is_none();
        if req.method() == "shutdown" && req.id().is_some() {
            self.shutdown = true;
        }
        let code = if self.shutdown { 0 } else { 1 };
        let future = self.inner.call(req);
        Box::pin(async move {
            let result = future.await;
            if exit {
                std::process::exit(code);
            }
            result
        })
    }
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("duckdb-lsp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let (service, socket) = LspService::build(duckdb_lsp::server::Backend::new)
        .custom_method("duckdb/status", duckdb_lsp::server::Backend::status)
        .custom_method(
            "duckdb/setConnection",
            duckdb_lsp::server::Backend::set_connection,
        )
        .custom_method(
            "duckdb/catalogDocument",
            duckdb_lsp::server::Backend::catalog_document,
        )
        .finish();
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
        .serve(ExitService {
            inner: service,
            shutdown: false,
        })
        .await;
    // Tokio's blocking stdin reader cannot be cancelled on an LSP exit. Do not
    // wait for the editor to close its pipe after the transport has stopped.
    std::process::exit(0);
}
