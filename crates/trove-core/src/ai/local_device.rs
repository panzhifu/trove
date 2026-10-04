//! The tensor device the local candle models run on — shared by the local
//! recogniser ([`super::transcribe_local`]) and the local embedder
//! ([`super::embed_local`]), which must agree on how a machine gets picked.

use candle_core::Device;

/// The tensor device for this build. On Linux the binary carries candle's
/// CUDA backend, so CUDA is tried first and CPU is the fallback when no
/// working driver answers — a GPU-capable build must not stop working
/// because the driver is missing, the GPU is busy, or memory is short.
/// Elsewhere the binary has no CUDA in it at all; CPU is the only device.
#[cfg(target_os = "linux")]
pub(crate) fn select_device() -> (Device, &'static str) {
    match Device::new_cuda(0) {
        Ok(device) => (device, "cuda"),
        Err(error) => {
            tracing::info!(%error, "local: no usable CUDA device; falling back to CPU");
            (Device::Cpu, "cpu")
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn select_device() -> (Device, &'static str) {
    (Device::Cpu, "cpu")
}
