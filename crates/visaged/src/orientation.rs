//! Turn camera frames upright when the device is not.
//!
//! A face camera in the display bezel rotates with the device. On a tablet or
//! convertible held upside down or sideways, it sees the face rotated too, and
//! neither SCRFD nor ArcFace is trained for that: detection mostly fails, and
//! the odd detection it does make yields an embedding that matches no one.
//!
//! `VISAGE_ROTATE=auto` follows the device orientation from iio-sensor-proxy
//! (`net.hadess.SensorProxy` on the system bus, the service desktops use to
//! rotate the screen), and the engine rotates every frame by the matching
//! amount before examining it. It reads the physical orientation, not the
//! screen's, so a screen rotation lock does not matter.
//!
//! The daemon claims the accelerometer once and keeps the latest orientation,
//! instead of claiming it for each verify: a claim can take a second or more
//! while the sensor starts (measured 0.8 to 2.2 s on a ThinkPad X12's sensor
//! hub), which would add that to every unlock. Claims are per client, so
//! other programs reading the sensor are not affected, and iio-sensor-proxy
//! drops the claim by itself when the daemon's bus connection closes.
//! Anything unavailable (no sensor, no proxy) means no rotation: the sensor
//! can never make a verify fail.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_lite::StreamExt;

/// How far to turn a frame clockwise before examining it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    None,
    Cw90,
    Half,
    Ccw90,
}

impl Rotation {
    pub fn degrees(self) -> u32 {
        match self {
            Rotation::None => 0,
            Rotation::Cw90 => 90,
            Rotation::Half => 180,
            Rotation::Ccw90 => 270,
        }
    }

    fn from_degrees(degrees: u32) -> Option<Self> {
        match degrees {
            0 => Some(Rotation::None),
            90 => Some(Rotation::Cw90),
            180 => Some(Rotation::Half),
            270 => Some(Rotation::Ccw90),
            _ => None,
        }
    }
}

/// `VISAGE_ROTATE`: what decides the rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RotateMode {
    /// Never rotate (the default).
    #[default]
    Off,
    /// Follow the device orientation from iio-sensor-proxy.
    Auto,
    /// Always rotate by this much, for a camera mounted turned.
    Fixed(Rotation),
}

/// Parse `VISAGE_ROTATE`: `auto`, `0`, `90`, `180`, `270`, or unset/`off`.
///
/// An unknown value means no rotation, with a warning: a typo in a tuning knob
/// must never keep the daemon from starting.
pub fn parse_rotate_mode(value: Option<&str>) -> RotateMode {
    let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return RotateMode::Off;
    };
    if v.eq_ignore_ascii_case("off") || v == "0" {
        return RotateMode::Off;
    }
    if v.eq_ignore_ascii_case("auto") {
        return RotateMode::Auto;
    }
    match v.parse().ok().and_then(Rotation::from_degrees) {
        Some(r) => RotateMode::Fixed(r),
        None => {
            tracing::warn!(
                value = v,
                "VISAGE_ROTATE ignored (use auto, 0, 90, 180 or 270)"
            );
            RotateMode::Off
        }
    }
}

/// The rotation that turns a bezel camera's frame upright, for an
/// iio-sensor-proxy orientation.
///
/// The orientation names the edge of the display that points up: `left-up`
/// means the device was turned 90 degrees clockwise, so the frame is turned
/// back counter-clockwise. Unknown and `undefined` mean no rotation.
pub fn rotation_for_orientation(orientation: &str) -> Rotation {
    match orientation {
        "bottom-up" => Rotation::Half,
        "left-up" => Rotation::Ccw90,
        "right-up" => Rotation::Cw90,
        _ => Rotation::None,
    }
}

/// Turn an 8-bit grayscale image clockwise by `rotation`.
///
/// Returns the pixels and the new width and height. A 640x480 frame takes well
/// under a millisecond.
pub fn rotate_gray(
    data: &[u8],
    width: u32,
    height: u32,
    rotation: Rotation,
) -> (Vec<u8>, u32, u32) {
    let (w, h) = (width as usize, height as usize);
    debug_assert_eq!(data.len(), w * h);
    match rotation {
        Rotation::None => (data.to_vec(), width, height),
        Rotation::Half => (data.iter().rev().copied().collect(), width, height),
        Rotation::Cw90 => {
            // Source (x, y) lands at (h - 1 - y, x) in a h-wide image.
            let mut out = vec![0u8; w * h];
            for y in 0..h {
                for x in 0..w {
                    out[x * h + (h - 1 - y)] = data[y * w + x];
                }
            }
            (out, height, width)
        }
        Rotation::Ccw90 => {
            // Source (x, y) lands at (y, w - 1 - x) in a h-wide image.
            let mut out = vec![0u8; w * h];
            for y in 0..h {
                for x in 0..w {
                    out[(w - 1 - x) * h + y] = data[y * w + x];
                }
            }
            (out, height, width)
        }
    }
}

/// A copy of `frame` turned upright by `rotation`.
pub fn rotate_frame(frame: &visage_hw::Frame, rotation: Rotation) -> visage_hw::Frame {
    let (data, width, height) = rotate_gray(&frame.data, frame.width, frame.height, rotation);
    visage_hw::Frame {
        data,
        width,
        height,
        timestamp: frame.timestamp,
        sequence: frame.sequence,
        is_dark: frame.is_dark,
    }
}

#[zbus::proxy(
    interface = "net.hadess.SensorProxy",
    default_service = "net.hadess.SensorProxy",
    default_path = "/net/hadess/SensorProxy"
)]
trait SensorProxy {
    fn claim_accelerometer(&self) -> zbus::Result<()>;
    fn release_accelerometer(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn has_accelerometer(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn accelerometer_orientation(&self) -> zbus::Result<String>;
}

/// How long to wait before trying again after iio-sensor-proxy went away or
/// could not be reached.
const RETRY: Duration = Duration::from_secs(10);

/// The current rotation for the device orientation, shared between the
/// watcher and the verify handler. Starts as no rotation.
#[derive(Debug, Clone, Default)]
pub struct DeviceRotation(Arc<AtomicU8>);

impl DeviceRotation {
    pub fn get(&self) -> Rotation {
        match self.0.load(Ordering::Relaxed) {
            1 => Rotation::Cw90,
            2 => Rotation::Half,
            3 => Rotation::Ccw90,
            _ => Rotation::None,
        }
    }

    fn set(&self, rotation: Rotation) {
        self.0
            .store((rotation.degrees() / 90) as u8, Ordering::Relaxed);
    }
}

/// Follow the device orientation in the background for `VISAGE_ROTATE=auto`.
pub fn spawn_watcher(conn: zbus::Connection, current: DeviceRotation) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = watch(&conn, &current).await {
                tracing::debug!(error = %e, "device orientation unavailable, not rotating");
            }
            current.set(Rotation::None);
            tokio::time::sleep(RETRY).await;
        }
    });
}

/// Claim the accelerometer and keep `current` up to date until
/// iio-sensor-proxy goes away or restarts (which drops the claim).
async fn watch(conn: &zbus::Connection, current: &DeviceRotation) -> zbus::Result<()> {
    let proxy = SensorProxyProxy::new(conn).await?;
    let mut owner_changes = proxy.inner().receive_owner_changed().await?;
    if !proxy.has_accelerometer().await? {
        tracing::info!("no accelerometer, VISAGE_ROTATE=auto will not rotate");
        // A sensor may appear when the proxy restarts.
        owner_changes.next().await;
        return Ok(());
    }
    let mut orientation_changes = proxy.receive_accelerometer_orientation_changed().await;
    proxy.claim_accelerometer().await?;
    tracing::info!("accelerometer claimed for VISAGE_ROTATE=auto");

    let apply = |orientation: &str| {
        let rotation = rotation_for_orientation(orientation);
        current.set(rotation);
        tracing::debug!(
            orientation,
            degrees = rotation.degrees(),
            "device orientation"
        );
    };
    if let Ok(orientation) = proxy.accelerometer_orientation().await {
        apply(&orientation);
    }
    loop {
        tokio::select! {
            change = orientation_changes.next() => match change {
                Some(change) => {
                    if let Ok(orientation) = change.get().await {
                        apply(&orientation);
                    }
                }
                None => return Ok(()),
            },
            _ = owner_changes.next() => {
                tracing::info!("iio-sensor-proxy restarted, claiming the accelerometer again");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 3x2 image:
    //   1 2 3
    //   4 5 6
    const IMG: [u8; 6] = [1, 2, 3, 4, 5, 6];

    #[test]
    fn rotations_move_every_pixel_where_expected() {
        assert_eq!(
            rotate_gray(&IMG, 3, 2, Rotation::None),
            (IMG.to_vec(), 3, 2)
        );
        // 180:  6 5 4 / 3 2 1
        assert_eq!(
            rotate_gray(&IMG, 3, 2, Rotation::Half),
            (vec![6, 5, 4, 3, 2, 1], 3, 2)
        );
        // 90 clockwise, 2 wide and 3 high:  4 1 / 5 2 / 6 3
        assert_eq!(
            rotate_gray(&IMG, 3, 2, Rotation::Cw90),
            (vec![4, 1, 5, 2, 6, 3], 2, 3)
        );
        // 90 counter-clockwise:  3 6 / 2 5 / 1 4
        assert_eq!(
            rotate_gray(&IMG, 3, 2, Rotation::Ccw90),
            (vec![3, 6, 2, 5, 1, 4], 2, 3)
        );
    }

    #[test]
    fn opposite_rotations_undo_each_other() {
        let (cw, w, h) = rotate_gray(&IMG, 3, 2, Rotation::Cw90);
        assert_eq!(
            rotate_gray(&cw, w, h, Rotation::Ccw90),
            (IMG.to_vec(), 3, 2)
        );
        let (half, w, h) = rotate_gray(&IMG, 3, 2, Rotation::Half);
        assert_eq!(
            rotate_gray(&half, w, h, Rotation::Half),
            (IMG.to_vec(), 3, 2)
        );
    }

    #[test]
    fn rotate_mode_parses_and_never_fails_on_a_typo() {
        assert_eq!(parse_rotate_mode(None), RotateMode::Off);
        assert_eq!(parse_rotate_mode(Some("")), RotateMode::Off);
        assert_eq!(parse_rotate_mode(Some("off")), RotateMode::Off);
        assert_eq!(parse_rotate_mode(Some("0")), RotateMode::Off);
        assert_eq!(parse_rotate_mode(Some("Auto")), RotateMode::Auto);
        assert_eq!(
            parse_rotate_mode(Some("180")),
            RotateMode::Fixed(Rotation::Half)
        );
        assert_eq!(
            parse_rotate_mode(Some("270")),
            RotateMode::Fixed(Rotation::Ccw90)
        );
        assert_eq!(parse_rotate_mode(Some("45")), RotateMode::Off);
        assert_eq!(parse_rotate_mode(Some("sideways")), RotateMode::Off);
    }

    #[test]
    fn orientations_map_to_the_rotation_that_turns_the_frame_back() {
        assert_eq!(rotation_for_orientation("normal"), Rotation::None);
        assert_eq!(rotation_for_orientation("bottom-up"), Rotation::Half);
        assert_eq!(rotation_for_orientation("left-up"), Rotation::Ccw90);
        assert_eq!(rotation_for_orientation("right-up"), Rotation::Cw90);
        assert_eq!(rotation_for_orientation("undefined"), Rotation::None);
        assert_eq!(rotation_for_orientation(""), Rotation::None);
    }
}
