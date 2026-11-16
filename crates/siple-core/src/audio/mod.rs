//! Audio device layer: cpal device enumeration, lock-free rings between the
//! device callbacks and the (future) media thread, the device-owning thread,
//! rubato resampling helpers and macOS microphone permission.
//!
//! Everything here is mono `f32` at the *device* rate; conversion to the
//! 48 kHz internal rate is done by the consumer with [`resample::Resampler`].

pub mod device_thread;
pub mod devices;
pub mod permission;
pub mod resample;
pub mod rings;

pub use device_thread::{
    ActiveDevice, AudioDevices, AudioHandle, AudioStatus, CaptureInjector, DeviceList,
    DeviceSelection, NO_DEVICE,
};
pub use devices::{DeviceInfo, Direction, Resolution, resolve_device};
pub use permission::{MicPermission, mic_permission, request_mic_permission};
pub use resample::Resampler;
pub use rings::Notifier;
