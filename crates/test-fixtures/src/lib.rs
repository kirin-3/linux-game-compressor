//! Synthetic Ren'Py trees for tests; never used by the shipped application.

use std::fs;
use std::io;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

pub struct Fixture {
    _temp: TempDir,
    root: PathBuf,
}

impl Fixture {
    pub fn new() -> io::Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("Example Game");
        fs::create_dir_all(root.join("renpy"))?;
        fs::create_dir_all(root.join("game/images"))?;
        fs::write(
            root.join("renpy/vc_version.py"),
            "version = '8.5.0.25102101'\n",
        )?;
        let game = root.join("game");
        let images = game.join("images");
        for (name, bytes) in [
            ("opaque.png", &include_bytes!("../assets/opaque.png")[..]),
            (
                "sprite transparent.png",
                &include_bytes!("../assets/transparent.png")[..],
            ),
            (
                "rgba-opaque.png",
                &include_bytes!("../assets/rgba-opaque.png")[..],
            ),
            (
                "animated.png",
                &include_bytes!("../assets/animated.png")[..],
            ),
            ("café.jpg", &include_bytes!("../assets/exif.jpg")[..]),
            (
                "mislabelled.png",
                &include_bytes!("../assets/mislabelled.bmp")[..],
            ),
            (
                "existing.webp",
                &include_bytes!("../assets/existing.webp")[..],
            ),
            ("BG.PNG", &include_bytes!("../assets/opaque.png")[..]),
            ("bg.png", &include_bytes!("../assets/opaque.png")[..]),
        ] {
            fs::write(images.join(name), bytes)?;
        }
        fs::write(
            game.join("presplash.png"),
            include_bytes!("../assets/opaque.png"),
        )?;
        fs::write(
            game.join("icon.png"),
            include_bytes!("../assets/opaque.png"),
        )?;
        fs::write(
            game.join("options.rpy"),
            "define config.window_icon = \"icon.png\"\n",
        )?;
        fs::write(game.join("script.rpy"), "label start:\n    return\n")?;
        fs::write(game.join("script.rpyc"), b"synthetic compiled script")?;
        fs::write(game.join("archive.rpa"), b"synthetic archive")?;
        fs::write(root.join("Game.sh"), "#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(root.join("Game.sh"), fs::Permissions::from_mode(0o755))?;
        fs::write(temp.path().join("outside.txt"), b"outside")?;
        symlink("images/opaque.png", game.join("inside-link"))?;
        symlink("../../outside.txt", game.join("outside-link"))?;
        Ok(Self { _temp: temp, root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tree_contains_expected_inputs() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let root = fixture.root();
        for name in [
            "renpy/vc_version.py",
            "Game.sh",
            "game/images/opaque.png",
            "game/images/sprite transparent.png",
            "game/images/rgba-opaque.png",
            "game/images/animated.png",
            "game/images/café.jpg",
            "game/images/mislabelled.png",
            "game/images/existing.webp",
            "game/images/BG.PNG",
            "game/images/bg.png",
            "game/presplash.png",
            "game/icon.png",
            "game/options.rpy",
            "game/script.rpy",
            "game/script.rpyc",
            "game/archive.rpa",
        ] {
            assert!(root.join(name).is_file(), "missing {name}");
        }
        assert_ne!(
            fs::metadata(root.join("Game.sh"))?.permissions().mode() & 0o111,
            0
        );
        assert!(fs::read_link(root.join("game/inside-link")).is_ok());
        assert!(!root
            .join("game/outside-link")
            .canonicalize()?
            .starts_with(root.canonicalize()?));
        assert!(fs::read(root.join("game/images/animated.png"))?
            .windows(4)
            .any(|x| x == b"acTL"));
        assert!(fs::read(root.join("game/images/café.jpg"))?
            .windows(4)
            .any(|x| x == b"Exif"));
        assert!(fs::read(root.join("game/images/mislabelled.png"))?.starts_with(b"BM"));
        assert!(fs::read_to_string(root.join("game/options.rpy"))?.contains("config.window_icon"));
        Ok(())
    }
}
