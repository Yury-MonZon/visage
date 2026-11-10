//! A single colour photo from a regular webcam, as the camera's own JPEG.
//!
//! Used to keep a colour picture next to a rejected face: the IR frame shows who was judged,
//! the colour photo shows the person and the scene as people see them. The JPEG the camera
//! produces (MJPEG) is returned as it is, never decoded or re-encoded.

use std::path::Path;
use std::time::{Duration, Instant};
use thiserror::Error;
use v4l::buffer::Type as BufType;
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;
use v4l::video::Capture;
use v4l::FourCC;

#[derive(Error, Debug)]
pub enum PhotoError {
    #[error("camera not found: {0}")]
    NotFound(String),
    #[error("camera busy")]
    Busy,
    #[error("camera does not offer MJPEG at {0}x{1} (got {2:?} {3}x{4})")]
    Format(u32, u32, FourCC, u32, u32),
    #[error("capture failed: {0}")]
    Capture(String),
}

/// Take one JPEG photo from `device_path` at `width` x `height`, after streaming for `settle`
/// so auto-exposure has adjusted. Frames during `settle` are discarded.
pub fn capture_jpeg(
    device_path: &str,
    width: u32,
    height: u32,
    settle: Duration,
) -> Result<Vec<u8>, PhotoError> {
    if !Path::new(device_path).exists() {
        return Err(PhotoError::NotFound(device_path.to_string()));
    }
    let device = Device::with_path(device_path).map_err(|e| {
        if e.to_string().contains("busy") || e.to_string().contains("EBUSY") {
            PhotoError::Busy
        } else {
            PhotoError::NotFound(format!("{device_path}: {e}"))
        }
    })?;

    let mut fmt = device
        .format()
        .map_err(|e| PhotoError::Capture(format!("get format: {e}")))?;
    fmt.fourcc = FourCC::new(b"MJPG");
    fmt.width = width;
    fmt.height = height;
    let got = device.set_format(&fmt).map_err(|e| {
        if e.to_string().contains("busy") || e.to_string().contains("EBUSY") {
            PhotoError::Busy
        } else {
            PhotoError::Capture(format!("set format: {e}"))
        }
    })?;
    if got.fourcc != FourCC::new(b"MJPG") || got.width != width || got.height != height {
        return Err(PhotoError::Format(
            width, height, got.fourcc, got.width, got.height,
        ));
    }

    let mut stream = MmapStream::with_buffers(&device, BufType::VideoCapture, 4)
        .map_err(|e| PhotoError::Capture(format!("stream: {e}")))?;
    let until = Instant::now() + settle;
    loop {
        let (buf, meta) = stream
            .next()
            .map_err(|e| PhotoError::Capture(format!("dequeue: {e}")))?;
        if Instant::now() < until {
            continue;
        }
        // MJPEG buffers are padded; the JPEG is what the driver filled in
        let used = (meta.bytesused as usize).min(buf.len());
        let jpeg = &buf[..used];
        // A JPEG starts with SOI (FF D8); anything else is a broken frame, try the next
        if jpeg.len() > 2 && jpeg[0] == 0xFF && jpeg[1] == 0xD8 {
            return Ok(jpeg.to_vec());
        }
    }
}
