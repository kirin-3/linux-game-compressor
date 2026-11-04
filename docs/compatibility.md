# Ren'Py compatibility

## WebP support

Ren'Py 6.99.12 introduced WebP image format support, according to the
[Ren'Py 6.11–6.99 changelog](https://www.renpy.org/doc/html/changelog6.html#ren-py-6-99-12).
This establishes the minimum version for the version check; it does not by
itself establish that every loading path accepts WebP bytes under another
extension.

## Keep-names smoke test (Linux, Ren'Py 8.5.3)

A minimal project using a copy of RENPY_2's 8.5.3 Linux runtime displayed
synthetic opaque PNG background, transparent RGBA PNG sprite, and JPEG CG
images after each was encoded with `cwebp` and replaced under its original
name. The background and sprite were displayed together, then the CG was
shown by advancing the scene. Visual inspection confirmed the shapes and
sprite transparency; `log.txt` showed no image errors. The original game
was not modified.

The same Linux 8.5.3 test also loaded a WebP-encoded `presplash.png` and
`icon.png`: the presplash was visible, and the X11 window's 64×64 icon
matched the test image. These two paths are included in the design's
exclusion list. They are still conservatively excluded because this single
Linux runtime test does not establish compatibility for other Ren'Py
versions, OS loaders, or packaging paths.

## RENPY_2: archive-only build

The Ren'Py 8.5.3 Linux build compressed with exit status 0. Its `game/archive.rpa`
was byte-identical, and the report listed it as `contents not processed`.
All 1,402 recorded files verified. The output reached its main menu under the
native Linux runtime in Xvfb. The example's runtime executable was stored with
mode 0644 on this machine; execute permission was added temporarily to launch
the output and then restored.

## RENPY_1: loose assets and resume

The Ren'Py 8.5.0 Windows build completed compression with exit status 0 and
no conversion failures. Of 1,308,864,554 input bytes, the output used
494,310,728 bytes, saving 814,553,826 bytes (about 62.2%). Verification
passed. The report excluded `game/gui/window_icon.png` and the configured
`game/images/icon.png`, and listed 584 existing WebP files as skipped.

A separate run was interrupted after 42 completed conversions and resumed.
The journal showed each of those conversions only once, and the resumed
output verified. Sorted `find -printf '%p %s %T@ %m'` snapshots of the input
before and after were identical.

The compressed Windows output also launched under Wine in Xvfb. The animated
main menu and gallery screen displayed; the gallery had no unlocked scenes in
this fresh profile. Starting a new game displayed the introductory CG and a
dialogue scene with the character's side sprite. The displayed `z1.jpeg` CG
and `zenochar1.png` sprite were both converted WebP data under their original
names. No image errors appeared in `log.txt`; Wine did report an unrelated
Steam initialization warning because no Steam client was installed.
# Audio compatibility

Ren'Py 6.99.9 added Opus playback to its default distributions, according to the [Ren'Py 6.99.9 changelog](https://www.renpy.org/doc/html/changelog6.html#ren-py-6-99-9). The compressor uses this as its minimum for Opus output; unknown or older versions fall back to Vorbis for `.ogg` files and skip `.opus` files unless the version check is overridden.

Ren'Py Ogg Vorbis, Opus, and Ogg FLAC sources are encoded as Opus by default, or Vorbis
when `ogg_target` is `vorbis`; MP3 sources remain MP3. Audio filenames and
extensions are preserved. WAV, native FLAC, and other unsupported formats are copied.
Text tags, including Vorbis loop comments, are retained; embedded pictures are
dropped and noted in the report. MP3 encoder delay and padding can shift loop
points, so sample-exact MP3 loops are not guaranteed.

The default profile was run on RENPY_1 (Ren'Py 8.5.0): 1,308,864,554 input
bytes became 435,139,556 bytes, saving 873,724,998 bytes (66.75%). Audio
conversions saved 59,171,172 bytes. The run reported no failures, and all
3,212 files passed `compressor verify` (exit 0).
The user played the converted build and confirmed that music, sound effects,
and a looping track played correctly.

# Godot 4

Standalone PCK files and unsigned Windows executables containing embedded PCK
packs are supported for Godot 4 pack formats 2–4. Encrypted directories,
sparse bundles, Godot 3 and unknown formats, signed executables, and embedded
packs in non-PE executables are copied unchanged. Encrypted members are kept.

Single-image PNG and lossless WebP `.ctex` textures at or above
`engines.godot.min_texture_kib` (default 4 KiB) can be converted to lossy WebP.
Mipmaps, raw and VRAM-compressed textures, and unsupported texture types are
left unchanged. Lossy WebP is re-encoded only when `image.reencode_webp` is
true.

`AudioStreamOggVorbis` and `AudioStreamMP3` binary resources can be re-encoded
when their bitrate exceeds the profile target. Loop and beat properties are
preserved. WAV and compressed binary resources are copied unchanged. MP3
encoder delay and padding can shift loop points; sample-exact MP3 loops are
not guaranteed.


# RPG Maker MV and MZ

MV and MZ exports are detected from their core script and `data/System.json`,
at the root or under `www/`. Built-in encrypted images and audio remain encrypted
under their original names. The key comes from `System.json` or is recovered
from encrypted PNG headers; scripts and `System.json` are copied unchanged.
Games packed with Enigma Virtual Box are unsupported, as are RPG Maker XP,
VX Ace and earlier engines, browser-only deployments, and APKs.

Images in `img/` become WebP; `img/parallaxes/` uses lossless WebP. Icons and
Effekseer textures are copied. Ogg Vorbis and Ogg FLAC in `audio/` become
Vorbis, preserving loop tags; MP3 stays MP3. M4A twins and, for MZ, MV-style
leftovers with matching MZ twins are removed unless
`engines.rpgmaker.remove_unused=false` is set.

Videos in `movies/` or `videos/` become VP9/Opus WebM under their original
names, even when those names end in `.mp4` or `.avi`. Transparency is retained.
The bundled NW.js runtimes of the MV and MZ test games played every converted
video to its end, including raw AVI and H.264 MP4 sources that the stock runtime
could not play before conversion. Much older NW.js builds may lack VP9 or Opus
support, so test video playback in such games before distributing the result.

# Unity standalone players

Windows players using Unity 2020.3.25f1, 2021.3.45f2, and 2022.3.7f1
were exercised during the archive-format spike. Linux standalone players are
detected but have not been tested. Unity 2020.1 and later versions in the
supported version table can have loose player data gathered losslessly into
`data.unity3d`; unlisted versions are left unchanged unless a newer version is
explicitly allowed with `--allow-unknown-version`.

Streamed `.resource` files, asset bundles, `Managed/`, `Plugins/`,
`Resources/`, `StreamingAssets/`, `il2cpp_data/`, configuration, and other game
files stay loose. Mods and tools that patch loose `.assets` files may require
`--set engines.unity.archive_data=false`. Already compressed builds, unsupported
versions, and folders with unreadable player data are copied unchanged;
archiving can also be disabled through that option.

These are the results on the Windows test builds. The release build ran on 8 cores, and each time covers planning, copying, writing the archive, and checking it:

| Game | Unity version | Input bytes | Output bytes | Saved | Time (seconds) |
| --- | --- | ---: | ---: | ---: | ---: |
| UNITY_1 | 2022.3.7f1 | 5,482,269,199 | 1,001,982,445 | 81.7% | 63 |
| UNITY_2 (Manor) | 2021.3.45f2 | 1,451,345,367 | 1,383,096,945 | 4.7% | 31 |
| UNITY_3 | 2020.3.25f1 | 5,632,413,603 | 770,886,570 | 86.3% | 57 |

Manor saves little because most of its content is in asset bundles that are already LZ4-compressed.

All three outputs passed `compressor verify`. Under Wine, each player logged that it loaded player data from `data.unity3d` and reached its first screen. Each archive is the same size as the archive from the format spike. The user played the spike's archived builds of UNITY_1 and UNITY_3: both started, played, and had working audio. Manor's archived build was checked headless through a new game, with its music playing.

# Desktop size estimates

Measured with the three built-in profiles and the balanced compression on the example games. Values are MiB; each estimate is the likely value followed by its low–high range. Measurements use 24 image, 48 audio, and 3 video samples, with eight-second video clips.

| Game | Recommended estimate (range) | Best looking estimate (range) | Smallest estimate (range) | Actual balanced |
|---|---:|---:|---:|---:|
| GODOT_1 | 340.84 (309.02–372.66) | 372.68 (340.99–404.36) | 321.02 (292.07–349.97) | 346.33 |
| GODOT_2 | 156.19 (156.05–156.33) | 178.00 (178.00–178.00) | 144.27 (144.16–144.38) | 156.74 |
| MV_1 | 867.48 (805.02–929.94) | 994.11 (936.69–1,051.53) | 701.23 (646.03–756.42) | 848.91 |
| MV_2 | 314.49 (289.06–339.91) | 346.31 (321.75–370.88) | 281.05 (255.53–306.57) | 316.05 |
| MZ_1 | 267.09 (262.19–271.99) | 270.93 (266.16–275.70) | 264.33 (259.82–268.84) | 268.05 |
| MZ_2 | 418.82 (388.32–449.32) | 466.80 (432.88–500.73) | 392.92 (363.51–422.33) | 416.81 |
| RENPY_1 | 410.72 (311.42–510.02) | 466.83 (356.30–577.35) | 329.02 (245.00–413.05) | 414.98 |
| RENPY_2 | 581.04 (581.04–581.04) | 581.04 (581.04–581.04) | 581.04 (581.04–581.04) | 581.04 |
| UNITY_1 | 993.04 (993.04–993.04) | 993.04 (993.04–993.04) | 993.04 (993.04–993.04) | 955.56 |
| UNITY_2 | 1,317.26 (1,317.26–1,317.26) | 1,317.26 (1,317.26–1,317.26) | 1,317.26 (1,317.26–1,317.26) | 1,319.02 |
| UNITY_3 | 783.40 (783.40–783.40) | 783.40 (783.40–783.40) | 783.40 (783.40–783.40) | 735.17 |

GODOT_2 finished 0.41 MiB above the balanced upper bound: sampled media ratios and the rebuilt embedded pack’s overhead did not exactly represent the complete pack. The Unity balanced results fell outside their zero-width ranges because the 32 sampled LZ4HC blocks did not represent the entire player-data archive: UNITY_1 and UNITY_3 compressed better than sampled, while UNITY_2 compressed slightly worse. Archive sampling currently contributes no uncertainty to the range, so these results show that a zero-width range does not imply an exact archive size.

The initial 12-file audio sample on MV_1 overestimated the balanced output; increasing the audio limit to 48 brought its actual result inside the range. Full runs completed with no conversion failures. Estimates are read-only and are not guarantees of the resulting size.
