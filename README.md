# Ren'Py image compressor

A Linux command-line tool for copying a Ren'Py game to a separate directory
while replacing eligible loose PNG and JPEG images with smaller, validated
WebP images. Archives are copied unchanged; their contents are not processed.

## Install

Install the libwebp command-line tools (`cwebp`, `dwebp`, and `webpinfo`). On
Arch Linux and CachyOS the package is `libwebp-utils`; on Debian and Ubuntu it
is `webp`. Install Rust 1.88 or newer, then build with `cargo build --release`.
The binary is `target/release/compressor`.

## Usage

Set `GAME` to the input game directory, `OUTPUT` to a separate output path,
and `PLAN` to a path for a saved plan. With `compressor` on your `PATH`:

```sh
compressor inspect "$GAME"
compressor inspect "$GAME" --json
compressor plan "$GAME" --profile balanced --list
compressor plan "$GAME" --profile small --save "$PLAN"
compressor compress "$GAME" --output "$OUTPUT" --plan "$PLAN" --jobs 2
compressor verify "$OUTPUT"
compressor verify "$OUTPUT" --json
```

`inspect` only reads the game; `plan` writes only the optional saved plan.
`compress` also accepts `--profile balanced`, `quality`, or `small` instead
of `--plan`, and `--allow-unknown-version` to override the engine version
gate. A profile can be a TOML file, for example:

```toml
quality = 90
effort = 6
sharp_yuv = true
reencode_webp = false
```

The default `balanced` profile uses quality 82 and effort 4. `quality` uses
90 and 6; `small` uses 72 and 6 and permits re-encoding still WebP images
under eligible filenames. Animated images, presplash images, and icons are
not converted. Any failed or non-smaller conversion keeps the original.

Converted files retain their original names and extensions, but contain WebP
bytes. This keeps Ren'Py references intact; modding or translation tools
that assume extensions match image contents may not recognize those files.

The output's `.compressor/` directory stores the plan, journal, manifest,
report, and temporary working files. Keep it to resume an interrupted job
and run `verify`. It can be removed from a distribution, but verification
and resumption then require it to be recreated by a new job.

Exit codes are 0 for success, 1 when some conversions failed and kept their
originals, 2 for usage or precondition errors, and 130 for interruption.
