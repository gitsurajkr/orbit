//! ORBIT client: QUIC receive → GPU decode → low-latency present, plus input capture.
//! Used by the `orbit-client` CLI and by the ORBIT app's CONNECT mode.

mod cursor;
mod decode;
mod keyhook;
mod net;
mod render;
mod viewer;

pub use viewer::run_viewer;
