//! # Simple example server
//!
//! Run this example with `cargo run --example server --features server`. It will listen on
//! <http://0.0.0.0:3000> for incoming XML-RPC requests.
//!
//! Testing this local server is straightforward, either with the included `client` example, or
//! with three lines of python:
//!
//! ```python3
//! >>> import xmlrpc.client
//! >>> proxy = xmlrpc.client.ServerProxy("http://0.0.0.0:3000/")
//! >>> proxy.hello("DXR")
//! 'Hello, DXR!'
//! ```

use axum::http::HeaderMap;
use dxr::{Fault, FromParams, Handler, ServerBuilder, ToDXR, Value};

struct HelloHandler {}

impl Handler for HelloHandler {
    fn handle(&self, params: &[Value], _headers: &HeaderMap) -> Result<Value, Fault> {
        let name = String::from_params(params)?;
        format!("Hello, {}!", name).to_dxr().map_err(|error| error.into())
    }
}

#[tokio::main]
async fn main() {
    let hello_handler = HelloHandler {};

    let server = ServerBuilder::new("0.0.0.0:3000".parse().unwrap())
        .add_method("hello", Box::new(hello_handler))
        .build();

    server.serve().await.expect("Failed to run server.")
}