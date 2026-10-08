use candle::Result;
use prost::Message;

pub mod onnx {
    // The prost-generated message code, checked in so no build needs
    // `protoc`. Regenerate from `onnx.proto3` with prost-build, never by hand.
    include!("onnx_generated.rs");
}

pub mod eval;
pub use eval::{dtype, simple_eval};

pub fn read_file<P: AsRef<std::path::Path>>(p: P) -> Result<onnx::ModelProto> {
    let buf = std::fs::read(p)?;
    onnx::ModelProto::decode(buf.as_slice()).map_err(candle::Error::wrap)
}
