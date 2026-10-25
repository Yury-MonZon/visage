//! Keep the face from verifies that saw a face and rejected it.
//!
//! Off unless `VISAGE_REJECTED_DIR` is set. When on, every verify that found a
//! face which matched no enrolled model stores that face, as the camera frame
//! the model judged (grayscale PNG), so the owner can see who tried to
//! authenticate as them. Verifies that saw no face store nothing.
//!
//! These are biometric captures of whoever stood in front of the camera, so the
//! directory is created `0700` and every file `0600`, owned by the daemon's
//! user (root for the packaged service), and only the newest `keep` files are
//! retained.

use image::ImageEncoder;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Where and how many rejected faces are kept.
#[derive(Debug, Clone)]
pub struct RejectedFaces {
    pub dir: PathBuf,
    pub keep: usize,
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
            .mode(0o600)
            .open(&path)?;
        file.write_all(&png)?;

        prune(&self.dir, self.keep)?;
        Ok(path)
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

/// Delete the oldest `.png` files beyond `keep`. Names start with the UTC time,
/// so name order is age order.
fn prune(dir: &Path, keep: usize) -> std::io::Result<()> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "png"))
        .collect();
    files.sort();
    let excess = files.len().saturating_sub(keep);
    for old in &files[..excess] {
        std::fs::remove_file(old)?;
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
    fn only_the_newest_files_are_kept() {
        let dir = tmp_dir("prune");
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["2026a.png", "2026b.png", "2026c.png", "notes.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        prune(&dir, 2).unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["2026b.png", "2026c.png", "notes.txt"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
