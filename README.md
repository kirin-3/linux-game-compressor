# Game media compressor

A Linux desktop and command-line tool for copying a Ren'Py, Godot 4, Unity, or RPG Maker MV/MZ game to a separate
directory while converting eligible images, audio, and video to smaller, validated
formats. Ren'Py archives are copied unchanged; Godot PCK packs are rebuilt
when eligible members convert.

## Install

Install the libwebp command-line tools (`cwebp`, `dwebp`, and `webpinfo`). On
Arch Linux and CachyOS the package is `libwebp-utils`; on Debian and Ubuntu it
is `webp`. Install `ffmpeg` and `ffprobe` from the `ffmpeg` package with the
`libopus`, `libvorbis`, `libmp3lame`, and `libvpx-vp9` encoders needed by your plan; transparent video also needs the `libvpx` and `libvpx-vp9` decoders. Install
Rust 1.88 or newer, then build with `cargo build --release`.
The binary is `target/release/compressor`.

## Desktop app

Launch the desktop app with `cargo run --release -p compressor-gui`, or run
`target/release/compressor-gui` after building the workspace. Drag game folders
onto the window, or use **Add folder**. Choose an output folder to put all games
there; otherwise each output is created beside its input. Select a game to
change its profile, settings, verification, or output folder, then press **Start**.
The queue processes games one at a time and verifies finished outputs by default.

On Linux the app uses X11 when `DISPLAY` is available, including XWayland on
Wayland desktops, to support folder drops. Without `DISPLAY`, it uses Wayland
and **Add folder** remains available. Set `COMPRESSOR_GUI_BACKEND=wayland` to
choose native Wayland; drag and drop is unavailable there.

Preferences are saved in `~/.config/compressor/gui.toml` (or under
`$XDG_CONFIG_HOME/compressor/gui.toml` when set). The queue is not saved.
Keep each output's `.compressor/` directory to resume or verify it. Files added
by hand inside an interrupted output may be replaced when that job resumes.

## Usage

Set `GAME` to the input game directory, `OUTPUT` to a separate output path,
and `PLAN` to a path for a saved plan. With `compressor` on your `PATH`:

```sh
compressor inspect "$GAME"
compressor inspect "$GAME" --json
compressor settings --engine renpy --profile small --set image.quality=70
compressor settings "$GAME" --json
compressor plan "$GAME" --profile balanced --list
compressor plan "$GAME" --profile small --set audio.vorbis_quality=2 --save "$PLAN"
compressor compress "$GAME" --output "$OUTPUT" --plan "$PLAN" --jobs 2
compressor verify "$OUTPUT"
compressor verify "$OUTPUT" --json
```

`inspect` only reads the game; `plan` writes only the optional saved plan.
`compress` also accepts `--profile balanced`, `quality`, or `small` instead
of `--plan`, and `--allow-unknown-version` to override the engine version
gate. `plan` and `compress` accept repeatable `--set key=value` overrides;
they cannot be combined with a saved `--plan`. Use `settings` to see effective
values, their sources, and engine options. A profile can be a TOML file:

```toml
name = "custom"
base = "quality"

[image]
quality = 88
effort = 6
sharp_yuv = true
reencode_webp = false

[audio]
opus_kbps_per_channel = 48
vorbis_quality = 3
mp3_kbps_per_channel = 64

[video]
crf = 36
effort = 3
opus_kbps_per_channel = 48

[engines.rpgmaker]
remove_unused = true

[engines.godot]
min_texture_kib = 4

[engines.renpy]
ogg_target = "vorbis"

[engines.renpy.image]
quality = 78
```

The default `balanced` profile uses quality 82 and effort 4. `quality` uses
90 and 6; `small` uses 72 and 6 and permits re-encoding still WebP images
under eligible filenames. Animated images, presplash images, and icons are
not converted. Any failed or non-smaller conversion keeps the original.

Converted files retain their original names and extensions. Images contain
WebP bytes; Ogg audio defaults to Opus unless `ogg_target` selects Vorbis,
and MP3 remains MP3. Tools that assume extensions match content may not
recognize converted files. WAV and native FLAC are copied unchanged; Ogg FLAC is supported.

For RPG Maker MV or MZ, the same commands preserve encrypted asset names and
re-encrypt converted assets with the game key. Ogg audio remains Vorbis;
unused M4A twins are removed by default. Disable that with
`--set engines.rpgmaker.remove_unused=false`. Videos become VP9/Opus WebM
under their original names, including `.mp4` or `.avi` names. Video settings
are `video.crf` (0–63), `video.effort` (0–5), and
`video.opus_kbps_per_channel` (6–256). See
[compatibility](docs/compatibility.md#rpg-maker-mv-and-mz) for details.

For a Godot 4 export, use the same `inspect`, `plan`, `compress`, and `verify`
commands. Pack members are shown as `pack.pck::member`. Set the minimum
texture size with `--set engines.godot.min_texture_kib=4` (KiB; default 4).
For example, `compressor plan "$GAME" --set engines.godot.min_texture_kib=4 --list`
shows the affected members. See [compatibility](docs/compatibility.md#godot-4)
for supported packs and resources.

Allow enough disk space for the complete output, staged converted members,
and a temporary rebuilt pack. Staged members are kept in `.compressor/` for
resume and verification.

The output's `.compressor/` directory stores the plan, journal, manifest,
report, and temporary working files. Keep it to resume an interrupted job
and run `verify`. It can be removed from a distribution, but verification
and resumption then require it to be recreated by a new job.

Exit codes are 0 for success, 1 when some conversions failed and kept their
originals, 2 for usage or precondition errors, and 130 for interruption.
