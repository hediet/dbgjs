//! Native capture of the service host's interactive desktop.
//!
//! These blocking functions back `dev.dbgjs.desktop`. They capture what the OS shows on the
//! machine running the service, which is only possible from a process in the logged-in user's
//! interactive session (see `docs/investigations/native-screen-capture.md`).

use crate::api::service_api::{
    DesktopError, DesktopWindowSnapshot, DisplaySnapshot, ScreenshotSnapshot,
};

pub fn list_displays() -> Result<Vec<DisplaySnapshot>, DesktopError> {
    platform::list_displays()
}

/// Visible top-level windows, front to back.
pub fn list_windows() -> Result<Vec<DesktopWindowSnapshot>, DesktopError> {
    platform::list_windows()
}

/// Captures one display as PNG; `None` selects the primary display.
pub fn capture_display(display_id: Option<u32>) -> Result<ScreenshotSnapshot, DesktopError> {
    platform::capture_display(display_id)
}

/// Captures one top-level window as PNG, including the parts other windows cover where the
/// platform supports it.
pub fn capture_window(window_id: u32) -> Result<ScreenshotSnapshot, DesktopError> {
    platform::capture_window(window_id)
}

#[cfg(any(windows, target_os = "macos"))]
mod platform {
    use std::io::Cursor;

    use base64::Engine;
    use xcap::image::{DynamicImage, ImageFormat, RgbaImage};
    use xcap::{Monitor, Window, XCapError};

    use super::*;

    pub fn list_displays() -> Result<Vec<DisplaySnapshot>, DesktopError> {
        host::with_desktop_access(false, || {
            Monitor::all()
                .map_err(capture_failed)?
                .iter()
                .map(display_snapshot)
                .collect()
        })
    }

    pub fn list_windows() -> Result<Vec<DesktopWindowSnapshot>, DesktopError> {
        host::with_desktop_access(false, || {
            Ok(Window::all()
                .map_err(capture_failed)?
                .iter()
                // Windows that close during enumeration are skipped rather than failing the list.
                .filter_map(|window| window_snapshot(window).ok())
                .collect())
        })
    }

    pub fn capture_display(display_id: Option<u32>) -> Result<ScreenshotSnapshot, DesktopError> {
        host::with_desktop_access(true, || {
            let monitors = Monitor::all().map_err(capture_failed)?;
            let monitor = match display_id {
                Some(display_id) => monitors
                    .into_iter()
                    .find(|monitor| monitor.id().ok() == Some(display_id))
                    .ok_or(DesktopError::DisplayNotFound { display_id })?,
                None => {
                    let primary = monitors
                        .iter()
                        .position(|monitor| monitor.is_primary().unwrap_or(false))
                        .unwrap_or(0);
                    monitors.into_iter().nth(primary).ok_or_else(|| {
                        DesktopError::CaptureFailed {
                            reason: "the host reports no displays".to_owned(),
                        }
                    })?
                }
            };
            encode_png(monitor.capture_image().map_err(capture_failed)?, true)
        })
    }

    pub fn capture_window(window_id: u32) -> Result<ScreenshotSnapshot, DesktopError> {
        host::with_desktop_access(true, || {
            let window = Window::all()
                .map_err(capture_failed)?
                .into_iter()
                .find(|window| window.id().ok() == Some(window_id))
                .ok_or(DesktopError::WindowNotFound { window_id })?;
            if window.is_minimized().unwrap_or(false) {
                return Err(DesktopError::WindowMinimized { window_id });
            }
            // macOS window images carry meaningful transparency (rounded corners); Windows'
            // PrintWindow leaves undefined alpha.
            encode_png(
                window.capture_image().map_err(capture_failed)?,
                cfg!(windows),
            )
        })
    }

    fn display_snapshot(monitor: &Monitor) -> Result<DisplaySnapshot, DesktopError> {
        Ok(DisplaySnapshot {
            id: monitor.id().map_err(capture_failed)?,
            name: host::display_name(monitor),
            x: monitor.x().map_err(capture_failed)?,
            y: monitor.y().map_err(capture_failed)?,
            width: monitor.width().map_err(capture_failed)?,
            height: monitor.height().map_err(capture_failed)?,
            scale_factor: monitor.scale_factor().unwrap_or(1.0),
            is_primary: monitor.is_primary().unwrap_or(false),
        })
    }

    fn window_snapshot(window: &Window) -> Result<DesktopWindowSnapshot, XCapError> {
        Ok(DesktopWindowSnapshot {
            id: window.id()?,
            pid: window.pid()?,
            app_name: window.app_name().unwrap_or_default(),
            title: window.title().unwrap_or_default(),
            x: window.x()?,
            y: window.y()?,
            width: window.width()?,
            height: window.height()?,
            is_minimized: window.is_minimized().unwrap_or(false),
            is_focused: window.is_focused().unwrap_or(false),
        })
    }

    fn encode_png(image: RgbaImage, opaque: bool) -> Result<ScreenshotSnapshot, DesktopError> {
        if image.width() == 0 || image.height() == 0 {
            return Err(DesktopError::CaptureFailed {
                reason: "the platform returned an empty image".to_owned(),
            });
        }
        let image = if opaque {
            DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(image).to_rgb8())
        } else {
            DynamicImage::ImageRgba8(image)
        };
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .map_err(|error| DesktopError::CaptureFailed {
                reason: format!("PNG encoding failed: {error}"),
            })?;
        Ok(ScreenshotSnapshot {
            media_type: "image/png".to_owned(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    fn capture_failed(error: XCapError) -> DesktopError {
        DesktopError::CaptureFailed {
            reason: error.to_string(),
        }
    }

    #[cfg(windows)]
    mod host {
        use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows_sys::Win32::System::StationsAndDesktops::{
            CloseDesktop, DESKTOP_SWITCHDESKTOP, OpenInputDesktop,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcessId;
        use windows_sys::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            SetThreadDpiAwarenessContext,
        };

        use super::*;

        /// Runs `operation` with per-monitor DPI awareness, so that coordinates and captures
        /// use physical pixels on scaled and mixed-DPI displays, and checks first that the
        /// process can see the interactive desktop when `capturing`.
        pub fn with_desktop_access<T>(
            capturing: bool,
            operation: impl FnOnce() -> Result<T, DesktopError>,
        ) -> Result<T, DesktopError> {
            if capturing {
                ensure_interactive_desktop()?;
            }
            let _dpi = ThreadDpiAwareness::per_monitor();
            operation()
        }

        pub fn display_name(monitor: &Monitor) -> String {
            monitor
                .friendly_name()
                .ok()
                .filter(|name| !name.is_empty())
                .or_else(|| monitor.name().ok())
                .unwrap_or_default()
        }

        fn ensure_interactive_desktop() -> Result<(), DesktopError> {
            let mut session_id = 0;
            // SAFETY: `session_id` is a valid out pointer.
            if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) } != 0
                && session_id == 0
            {
                return Err(DesktopError::PermissionDenied {
                    reason: "the dbgjs service runs in session 0 (as a Windows service), which has no visible desktop; run the service in the signed-in user's session".to_owned(),
                });
            }
            // SAFETY: plain Win32 call; the returned handle is closed below.
            let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_SWITCHDESKTOP) };
            if desktop.is_null() {
                return Err(DesktopError::PermissionDenied {
                    reason: "the interactive desktop is not accessible: the workstation is locked or a secure desktop (such as a UAC prompt) is active".to_owned(),
                });
            }
            // SAFETY: `desktop` was returned by OpenInputDesktop and is closed once.
            unsafe { CloseDesktop(desktop) };
            Ok(())
        }

        struct ThreadDpiAwareness(DPI_AWARENESS_CONTEXT);

        impl ThreadDpiAwareness {
            fn per_monitor() -> Self {
                // SAFETY: changes only the calling thread's DPI awareness; restored on drop.
                Self(unsafe {
                    SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
                })
            }
        }

        impl Drop for ThreadDpiAwareness {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    // SAFETY: restores the context returned by the matching call.
                    unsafe { SetThreadDpiAwarenessContext(self.0) };
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    mod host {
        use super::*;

        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGPreflightScreenCaptureAccess() -> bool;
        }

        /// Checks Screen Recording (TCC) permission before capturing. Without it, macOS
        /// returns no image or one without other applications' windows.
        pub fn with_desktop_access<T>(
            capturing: bool,
            operation: impl FnOnce() -> Result<T, DesktopError>,
        ) -> Result<T, DesktopError> {
            // SAFETY: CGPreflightScreenCaptureAccess takes no arguments and never prompts.
            if capturing && !unsafe { CGPreflightScreenCaptureAccess() } {
                let executable = std::env::current_exe()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| "the dbgjs service".to_owned());
                return Err(DesktopError::PermissionDenied {
                    reason: format!(
                        "Screen Recording permission is not granted to the dbgjs service ({executable}). macOS attributes it to the service's responsible process: the terminal app that started it, or the executable launchd started (for a hub daemon, typically its node binary). Grant that app in System Settings > Privacy & Security > Screen & System Audio Recording, then restart it. launchd services must run as a LaunchAgent in the user's GUI (Aqua) session."
                    ),
                });
            }
            operation()
        }

        pub fn display_name(monitor: &Monitor) -> String {
            // `friendly_name` uses AppKit, which must not be called off the main thread.
            monitor.name().unwrap_or_default()
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;

    pub fn list_displays() -> Result<Vec<DisplaySnapshot>, DesktopError> {
        Err(unsupported())
    }

    pub fn list_windows() -> Result<Vec<DesktopWindowSnapshot>, DesktopError> {
        Err(unsupported())
    }

    pub fn capture_display(_display_id: Option<u32>) -> Result<ScreenshotSnapshot, DesktopError> {
        Err(unsupported())
    }

    pub fn capture_window(_window_id: u32) -> Result<ScreenshotSnapshot, DesktopError> {
        Err(unsupported())
    }

    fn unsupported() -> DesktopError {
        DesktopError::Unsupported {
            platform: std::env::consts::OS.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;

    use super::*;

    #[test]
    #[ignore = "needs an interactive desktop session; run with --ignored"]
    fn captures_the_primary_display_and_its_frontmost_window() {
        let displays = list_displays().unwrap();
        let primary = displays.iter().find(|display| display.is_primary).unwrap();
        let png = decode(&capture_display(None).unwrap());
        assert!(png_dimensions(&png).0 > 0);
        if cfg!(windows) {
            assert_eq!(png_dimensions(&png), (primary.width, primary.height));
        }
        let window = list_windows()
            .unwrap()
            .into_iter()
            .find(|window| !window.is_minimized && window.width > 0 && window.height > 0)
            .unwrap();
        let png = decode(&capture_window(window.id).unwrap());
        assert!(png_dimensions(&png).0 > 0);
    }

    #[test]
    #[cfg(any(windows, target_os = "macos"))]
    fn reports_unknown_displays_and_windows() {
        assert!(matches!(
            capture_display(Some(u32::MAX)),
            Err(DesktopError::DisplayNotFound { display_id: u32::MAX })
                | Err(DesktopError::PermissionDenied { .. })
        ));
        assert!(matches!(
            capture_window(u32::MAX),
            Err(DesktopError::WindowNotFound { window_id: u32::MAX })
                | Err(DesktopError::PermissionDenied { .. })
        ));
    }

    #[test]
    #[cfg(not(any(windows, target_os = "macos")))]
    fn reports_unsupported_platforms() {
        assert!(matches!(
            capture_display(None),
            Err(DesktopError::Unsupported { .. })
        ));
    }

    fn decode(snapshot: &ScreenshotSnapshot) -> Vec<u8> {
        assert_eq!(snapshot.media_type, "image/png");
        base64::engine::general_purpose::STANDARD
            .decode(&snapshot.data_base64)
            .unwrap()
    }

    fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        (
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
        )
    }
}
