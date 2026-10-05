//! The tensor device the local candle models run on — shared by the local
//! recogniser ([`super::transcribe_local`]) and the local embedder
//! ([`super::embed_local`]), which must agree on how a machine gets picked.

use candle_core::Device;

/// The tensor device for this run. CUDA is tried first — whether the binary
/// carries the CUDA backend is a build-time choice (the `cuda` feature), and
/// a GPU-capable build must not stop working because the driver is missing,
/// the GPU is busy, or memory is short. A build compiled without `cuda`
/// never reaches a real device: candle's dummy backend answers
/// `Device::new_cuda` with an error of its own, and the same fallback keeps
/// the caller on CPU. The returned tag names the choice for the log.
pub(crate) fn select_device() -> (Device, &'static str) {
    match Device::new_cuda(0) {
        Ok(device) => (device, "cuda"),
        Err(error) => {
            tracing::info!(%error, "local: no usable CUDA device; falling back to CPU");
            (Device::Cpu, "cpu")
        }
    }
}
