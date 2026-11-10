//! Keep the face from verifies that saw a face and rejected it.
//!
//! Off unless `VISAGE_REJECTED_DIR` is set. When on, every verify that found a
//! face which matched no enrolled model stores that face, as the camera frame
//! the model judged (grayscale PNG), so the owner can see who tried to
//! authenticate as them. Verifies that saw no face store nothing.
//!
//! Optionally (`VISAGE_REJECTED_PHOTO_CAMERA`) a colour photo from a regular
//! webcam is taken right after, as `<same name>_rgb.jpg`: the IR frame shows
//! who was judged, the colour photo shows the person and the scene as people
//! see them, and whoever tried is most likely still in front of the screen.
//!
//! These are biometric captures of whoever stood in front of the camera, so the
//! directory is created `0700` and every file `0600`, owned by the daemon's
//! user (root for the packaged service), and only the newest `keep` files are
//! retained (a colour photo goes with its IR frame; `keep` 0 keeps all).
//!
//! To let one user see them without copying, `group_readable` writes the files
//! `0640`. The group comes from the directory: create it setgid for that group
//! (`2750 root:<group>`, e.g. with systemd-tmpfiles), since the sandboxed
//! daemon has no capability to change a file's group itself.

use image::ImageEncoder;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Where and how many rejected faces are kept.
#[derive(Debug, Clone)]
pub struct RejectedFaces {
    pub dir: PathBuf,
    /// How many faces to keep, newest first; 0 keeps all.
    pub keep: usize,
    /// Write the files `0640` instead of `0600`, for the directory's group.
    pub group_readable: bool,
    /// A colour photo with each rejected face, or `None`.
    pub photo: Option<ColourPhoto>,
}

/// The webcam for the colour photo and how to take it.
#[derive(Debug, Clone)]
pub struct ColourPhoto {
    pub camera: String,
    pub width: u32,
    pub height: u32,
    /// How long to stream before the photo, so auto-exposure has adjusted.
    pub settle: std::time::Duration,
}

/// The colour photo that belongs to a rejected face's PNG.
pub fn photo_path(png: &Path) -> PathBuf {
    png.with_file_name(format!(
        "{}_rgb.jpg",
        png.file_stem().unwrap_or_default().to_string_lossy()
    ))
}

impl RejectedFaces {
    /// Store `frame` and prune the directory to the newest `keep` files.
    /// Returns the path written.
    pub fn save(
        &self,
        frame: &visage_hw::Frame,
        user: &str,
        similarity: f32,
    ) -> std::io::Result<PathBuf> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;

        let path = self.dir.join(file_name(
            &chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string(),
            user,
            similarity,
        ));

        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                &frame.data,
                frame.width,
                frame.height,
                image::ExtendedColorType::L8,
            )
            .map_err(std::io::Error::other)?;

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(self.file_mode())
            .open(&path)?;
        file.write_all(&png)?;

        prune(&self.dir, self.keep)?;
        Ok(path)
    }

    fn file_mode(&self) -> u32 {
        if self.group_readable {
            0o640
        } else {
            0o600
        }
    }

    /// Take the colour photo for the rejected face saved at `png`, if configured.
    /// Returns the path written, or `None` when no photo is configured.
    pub fn save_photo(&self, png: &Path) -> std::io::Result<Option<PathBuf>> {
        let Some(photo) = &self.photo else {
            return Ok(None);
        };
        let jpeg = visage_hw::capture_jpeg(&photo.camera, photo.width, photo.height, photo.settle)
            .map_err(std::io::Error::other)?;
        let path = photo_path(png);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(self.file_mode())
            .open(&path)?;
        file.write_all(&jpeg)?;
        Ok(Some(path))
    }
}

/// `<UTC time>_<user>_<similarity>.png`. The user name is reduced to
/// characters that are safe in a file name; the time comes first so a plain
/// sort is chronological.
fn file_name(timestamp: &str, user: &str, similarity: f32) -> String {
    let user: String = user
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{timestamp}_{user}_{similarity:.2}.png")
}

/// Delete the oldest `.png` files beyond `keep`, each with its colour photo;
/// `keep` 0 deletes nothing. Names start with the UTC time, so name order is
/// age order.
fn prune(dir: &Path, keep: usize) -> std::io::Result<()> {
    if keep == 0 {
        return Ok(());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "png"))
        .collect();
    files.sort();
    let excess = files.len().saturating_sub(keep);
    for old in &files[..excess] {
        std::fs::remove_file(old)?;
        match std::fs::remove_file(photo_path(old)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn frame() -> visage_hw::Frame {
        visage_hw::Frame {
            data: (0..64u32 * 48).map(|i| (i % 251) as u8).collect(),
            width: 64,
            height: 48,
            timestamp: std::time::Instant::now(),
            sequence: 0,
            is_dark: false,
        }
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "visage-rejected-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn user_names_cannot_escape_the_directory() {
        assert_eq!(
            file_name("T", "../../etc/x", 0.31),
            "T_.._.._etc_x_0.31.png"
        );
        assert_eq!(file_name("T", "monzon", 0.3), "T_monzon_0.30.png");
    }

    /// Biometric captures of strangers: owner-only, always.
    #[test]
    fn saved_faces_are_private_and_readable_pngs() {
        let dir = tmp_dir("perm");
        let r = RejectedFaces {
            dir: dir.clone(),
            keep: 10,
            group_readable: false,
            photo: None,
        };
        let path = r.save(&frame(), "monzon", 0.21).unwrap();

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);

        let img = image::open(&path).unwrap().into_luma8();
        assert_eq!((img.width(), img.height()), (64, 48));
        assert_eq!(img.as_raw(), &frame().data);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_the_newest_files_are_kept_each_with_its_photo() {
        let dir = tmp_dir("prune");
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "2026a.png",
            "2026a_rgb.jpg",
            "2026b.png",
            "2026b_rgb.jpg",
            "2026c.png",
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        prune(&dir, 2).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["2026b.png", "2026b_rgb.jpg", "2026c.png", "notes.txt"]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keep_zero_keeps_everything() {
        let dir = tmp_dir("keepall");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["2026a.png", "2026b.png", "2026c.png"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        prune(&dir, 0).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Group-readable is opt-in: by default nobody but the daemon's user may
    /// read a stranger's face.
    #[test]
    fn group_readable_only_when_asked() {
        let dir = tmp_dir("group");
        let mut r = RejectedFaces {
            dir: dir.clone(),
            keep: 10,
            group_readable: true,
            photo: None,
        };
        let shared = r.save(&frame(), "monzon", 0.2).unwrap();
        r.group_readable = false;
        let private = r.save(&frame(), "monzon", 0.3).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&shared), 0o640);
        assert_eq!(mode(&private), 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_photo_is_named_after_its_face() {
        assert_eq!(
            photo_path(Path::new("/r/20261025T145520.020Z_monzon_0.21.png")),
            Path::new("/r/20261025T145520.020Z_monzon_0.21_rgb.jpg")
        );
    }

    #[test]
    fn without_a_camera_no_photo_is_taken() {
        let r = RejectedFaces {
            dir: PathBuf::from("/nonexistent"),
            keep: 1,
            group_readable: false,
            photo: None,
        };
        assert!(r
            .save_photo(Path::new("/nonexistent/x.png"))
            .unwrap()
            .is_none());
    }
}
