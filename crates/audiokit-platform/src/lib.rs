//! Native adapters and the exact callback transports used by virtual port tests.
//! No transport SDK, Tokio runtime, codec, processing backend or UI is required.
#[cfg(feature = "native-cpal")]
pub mod cpal;
pub mod ports;
#[cfg(all(windows, feature = "windows-process-loopback"))]
pub mod wasapi_process;
