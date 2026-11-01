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

Ren'Py Ogg Vorbis and Opus sources are encoded as Opus by default, or Vorbis
when `ogg_target` is `vorbis`; MP3 sources remain MP3. Audio filenames and
extensions are preserved. WAV, FLAC, and other unsupported formats are copied.
Text tags, including Vorbis loop comments, are retained; embedded pictures are
dropped and noted in the report. MP3 encoder delay and padding can shift loop
points, so sample-exact MP3 loops are not guaranteed.

The default profile was run on RENPY_1 (Ren'Py 8.5.0): 1,308,864,554 input
bytes became 435,139,556 bytes, saving 873,724,998 bytes (66.75%). Audio
conversions saved 59,171,172 bytes. The run reported no failures, and all
3,212 files passed `compressor verify` (exit 0).
The user played the converted build and confirmed that music, sound effects,
and a looping track played correctly.
