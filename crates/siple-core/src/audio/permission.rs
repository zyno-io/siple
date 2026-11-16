//! Microphone permission (macOS TCC). Other platforms report `NotApplicable`.

use std::time::Duration;

use serde::Serialize;

/// Microphone authorization state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MicPermission {
    /// Access granted.
    Granted,
    /// User denied access (capture yields silence).
    Denied,
    /// Blocked by policy (MDM / parental controls).
    Restricted,
    /// User has not been asked yet.
    NotDetermined,
    /// Platform has no queryable permission model.
    NotApplicable,
}

/// Current microphone authorization, without prompting.
pub fn mic_permission() -> MicPermission {
    imp::status()
}

/// Query the status and, if `NotDetermined`, prompt the user and wait up to
/// `timeout` for the answer. Returns the resulting state (still
/// `NotDetermined` on timeout). On macOS the process must carry
/// `NSMicrophoneUsageDescription` (app bundle `Info.plist`) or the OS will
/// terminate it when the prompt is triggered.
pub fn request_mic_permission(timeout: Duration) -> MicPermission {
    imp::request(timeout)
}

#[cfg(target_os = "macos")]
mod imp {
    use super::MicPermission;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Answer delivered to our own `requestAccess` call (0 = none yet, 1 = granted, 2 = denied).
    /// When the prompt was triggered by opening the input device rather than by AVFoundation,
    /// `authorizationStatusForMediaType` keeps returning "not determined" for the rest of the
    /// process even after the user answers; the completion handler's answer is authoritative.
    static ANSWER: AtomicU8 = AtomicU8::new(0);

    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

    pub fn status() -> MicPermission {
        // SAFETY: plain class method taking a valid media type constant.
        let Some(media) = (unsafe { AVMediaTypeAudio }) else {
            return MicPermission::NotApplicable;
        };
        let s = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) };
        match s {
            AVAuthorizationStatus::Authorized => MicPermission::Granted,
            AVAuthorizationStatus::Denied => MicPermission::Denied,
            AVAuthorizationStatus::Restricted => MicPermission::Restricted,
            _ => match ANSWER.load(Ordering::Relaxed) {
                1 => MicPermission::Granted,
                2 => MicPermission::Denied,
                _ => MicPermission::NotDetermined,
            },
        }
    }

    pub fn request(timeout: Duration) -> MicPermission {
        let cur = status();
        if cur != MicPermission::NotDetermined {
            return cur;
        }
        let Some(media) = (unsafe { AVMediaTypeAudio }) else {
            return MicPermission::NotApplicable;
        };
        let (tx, rx) = mpsc::channel::<bool>();
        let block = RcBlock::new(move |granted: Bool| {
            let _ = tx.send(granted.as_bool());
        });
        // SAFETY: valid media type; the block is retained by the callee.
        unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &block) };
        match rx.recv_timeout(timeout) {
            Ok(granted) => {
                ANSWER.store(if granted { 1 } else { 2 }, Ordering::Relaxed);
                status()
            }
            Err(_) => MicPermission::NotDetermined,
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::MicPermission;
    use std::time::Duration;

    // TODO(windows): the microphone privacy toggle lives in the registry at
    // HKCU\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone
    // ("Value" = "Allow"/"Deny", plus per-app subkeys for NonPackaged apps).
    // It is not reliably queryable for unpackaged desktop apps, so report
    // NotApplicable for now.
    pub fn status() -> MicPermission {
        MicPermission::NotApplicable
    }

    pub fn request(_timeout: Duration) -> MicPermission {
        MicPermission::NotApplicable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_does_not_panic() {
        let _ = mic_permission();
        #[cfg(not(target_os = "macos"))]
        assert_eq!(mic_permission(), MicPermission::NotApplicable);
    }
}
