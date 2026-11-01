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

/// Build a small Godot 4 pack with two unencrypted members.
pub fn godot_pack(format: u32, flags: u32) -> Vec<u8> {
    fn u32w(v: &mut Vec<u8>, n: u32) {
        v.extend(n.to_le_bytes());
    }
    fn u64w(v: &mut Vec<u8>, n: u64) {
        v.extend(n.to_le_bytes());
    }
    fn directory() -> Vec<u8> {
        let mut d = Vec::new();
        u32w(&mut d, 2);
        for (name, offset, size) in [
            (b"a\0\0\0".as_slice(), 0, 3),
            (b"long\0\0\0\0".as_slice(), 16, 4),
        ] {
            u32w(&mut d, name.len() as u32);
            d.extend(name);
            u64w(&mut d, offset);
            u64w(&mut d, size);
            d.extend([0; 16]);
            u32w(&mut d, 0);
        }
        d
    }
    let d = directory();
    let header_len = if format == 2 { 96 } else { 104 };
    let base = if format == 2 {
        (header_len + d.len() + 15) & !15
    } else {
        112
    };
    let mut p = Vec::new();
    p.extend(b"GDPC");
    for n in [format, 4, 7, 0, flags] {
        u32w(&mut p, n);
    }
    u64w(&mut p, base as u64);
    if format != 2 {
        u64w(&mut p, (base + 32) as u64);
    }
    p.extend([0; 64]);
    if format == 2 {
        p.extend(d.clone());
    }
    p.resize(base, 0);
    p.extend(b"one");
    p.resize(base + 16, 0);
    p.extend(b"two!");
    p.resize(base + 32, 0);
    if format != 2 {
        p.extend(d);
    }
    p
}

/// Build a Godot 4 format-4 pack from named members.
pub fn godot_pack_members(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut data = Vec::new();
    let mut offsets = Vec::new();
    for (_, bytes) in members {
        offsets.push(data.len() as u64);
        data.extend(*bytes);
        data.resize((data.len() + 15) & !15, 0);
    }
    let mut pack = Vec::new();
    pack.extend(b"GDPC");
    for value in [4u32, 4, 7, 0, 2] {
        pack.extend(value.to_le_bytes());
    }
    pack.extend(112u64.to_le_bytes());
    pack.extend((112u64 + data.len() as u64).to_le_bytes());
    pack.extend([0; 64]);
    pack.resize(112, 0);
    pack.extend(data);
    pack.extend((members.len() as u32).to_le_bytes());
    for ((name, bytes), offset) in members.iter().zip(offsets) {
        let n = (name.len() + 3) & !3;
        pack.extend((n as u32).to_le_bytes());
        pack.extend(name.as_bytes());
        pack.resize(pack.len() + n - name.len(), 0);
        pack.extend(offset.to_le_bytes());
        pack.extend((bytes.len() as u64).to_le_bytes());
        pack.extend([0; 16]);
        pack.extend(0u32.to_le_bytes());
    }
    pack
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
