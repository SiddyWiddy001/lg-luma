# Luma `optimize` branch — status report

Built from `custom/glass-ui`, never merged. Each optimisation is its own commit.
Current head: `725087e` ("Collapse deferred batch merge checks for clippy").
The TV currently has the `725087e` build installed; frame timing and visual comparison are
pending because the TV is off.

## Commits

| Commit | Step |
| --- | --- |
| `2a7f807` | 1 — full LTO in the release profile |
| `d79d31a` | 2 — present pacing: no CPU spin while animating when SDL refuses vsync |
| `f5fcbf6` | 3 — pre-size wallpapers and oversized icons once (Lanczos3) instead of GPU downscale every frame |
| `7277e9b` | 4 — cache the nav/header layout, re-measure only when the language changes |
| `d5c5d65` | 5 — measure tile captions once per compose instead of twice |
| `64ccc03` | 6 — stop laying out Apps rows that start past the viewport |
| `1a8d9e8` | 7 — idle backoff: an untouched launcher sleeps up to 1 s between polls |
| `386ec6e` | step 3 compile fix: keep Lanczos tap weights in float until the fixed-point rounding |
| `68c632c` | focus glow: one continuous falloff replacing five flat-alpha rings |
| `e770a07` | eased motion capped to two frame steps, so a wake from idle cannot consume the ramp |
| `64943f5` | keep the frame loop painting while a card grows, shrinks or flies |
| `bfed94c` | dim and tighten the focus bloom (edge alpha `0.49 → 0.42`, falloff `blur * 0.2667 → blur * 0.2`) |
| `8885467` | glide the scrolling list to a revealed row instead of snapping |
| `7ecfb1d` | reflow the nineteen lines rustfmt flagged on the previous commits |
| `a740884` | glide the favourites strip to a revealed card instead of snapping |

Steps 1–7 were meant to be pixel-neutral. `68c632c` deliberately changes the glow, because the
five stepped rings were what banded.

## The banding fix

Old `view::Scene::glow()` pushed five filled rounded shapes (insets 6/12/20/30/42 px, alphas
0.24/0.165/0.105/0.06/0.038). Each ring is a flat alpha, so the stack composited into concentric
plateaus with hard edges — visible bands on a dark wallpaper.

New: `Draw::Glow(Rect, radius, blur, Color)` → `PaintMode::Glow`, one rounded-rect distance field
in `paint.frag` with a hold-then-exponential falloff (`hold = blur * 0.1`, `decay = blur * 0.2667`,
residual at the quad edge subtracted so the halo reaches exactly zero at the geometry edge).
Glow is flagged by negating the half sizes (`v_shape.z < 0`), so the vertex layout and stride 56
are unchanged and it batches like any other paint quad. Inside the card it stays full strength, so
the translucent plate is lit as before; only the outer falloff changed.

Tuning knobs live in `src/shaders/paint.frag` (`hold`, `decay`) and the edge alpha `0.49 * intensity`
in `src/view.rs::glow()`.

### Measured falloff (row across a focused Apps tile, 0 = card edge, 255 levels)

```
distance px      0   1   2   4   6   8  10  12  16  20  25  30  35  40  45  51
old 5 rings    132 101 100 103 100  77  72  74  58  56  50  47  46  51  45  35
new glow       133 131 125 117 109 101  95  93  71  71  57  57  51  52  47  36
```

Structure, which is what the complaint was about:

- hard step edges along the profile: **4 → 0** (old stepped at ~7, ~15, ~25 px outward)
- flat plateau pixels: **15 → 2** — the new profile is monotonic, so there is nothing to band
- starts and ends at the same place: 132 vs 133 at the card edge, ~35 vs ~36 in the background

Honest caveat: the new curve interpolates where the old one stepped, so the mid-band is a bit
brighter than the old plateaus (roughly +20 to +30 levels between 6 and 20 px in the row above).
It reads softer and brighter rather than identical. Two easy dials if you want it closer to the
old brightness: drop the edge alpha from `0.49` toward `0.42`, or raise `decay` from
`blur * 0.2667` to `blur * 0.2` for a faster falloff. Either is a one-line change plus one CI run.

## Latest renderer pass (`725087e`)

- `bc72376` merges text runs and brand logos into shared dynamic vertex buffers with per-vertex
  colours. Text and logos are deferred to the final layer; UI glyphs keep their original order.
- `7465a48` flushes deferred text/logos before any full-screen overlay, so modal and Recents dim
  layers stay above the parent interface.
- `b29125f` and `725087e` fix the deferred batch buffer types and collapse the merge checks for
  Clippy.
- CI run `42388557896` at `725087e`: formatting, Clippy, unit tests, all four smoke suites,
  ARM IPK and release eligibility are green.
- `custom-ipk.yml` run `42389413686` at `725087e`: IPK sha256
  `155b3a4fa8834a10b207ec5994c6a8c1ba27bab71ab28b875ca6655a51969127`; inner ARM binary
  sha256 `f8fd0ba660255971303c6220dec1514d8a6aa39ed1ef7530a722a8c2ac9c47d2` (4,334,064 bytes).
- `verify_custom.py` PASS against stock / alpha / `_optimize_arm` / `_optimize_prev_arm`.
- Installed on the TV. The installer confirmed the app binary hash and relaunched Home as PID 5563
  running the new binary. The TV was then turned off before real frame timing was measured.
- Pending TV checks: Apps glow, Apps glide and Recents pacing; before/after screenshots for the
  deferred text/logo layer; and a reboot to verify the `home boot` hook with the new binary.

## Verification

- CI run `41953545474` on `fork/optimize`, headSha `68c632c`, conclusion `success`.
- IPK `com.ecklf.luma_2.16.2_optimize_arm.ipk` — sha256 `93af7b13a66b4f679390cbc967441288b5e4ed2bd801c399ce42324e12e84049`;
  inner ARM binary sha256 `000966ca5011aa774ee4feb95dec233a640b5791ee044e866cb15071b7ae8d75`, 4,334,064 bytes, `e_machine=40`.
- `verify_custom.py` PASS against stock / alpha / previous / older builds: package identity, pooled
  gradient alphas, cinematic strings, the `0.49` glow literal, and the old stepped-table literals gone.
- Pixel regression, same screens captured on the pre-glow build and the glow build:
  - Settings and menu: identical apart from the header clock (x 85–178, y 68–86) — 0.06 % of pixels.
  - Recents: only the dynamic right-edge column (x 1833+, max delta 69).
  - Apps: differences spread over the grid because the wallpaper Ken Burns phase differs between runs.
  - Images: `shots/glow_before_after.png` (old left, new right), `shots/glow_crop_final.png`,
    and full screens `shots/glow2_*.png`.
- Texture memory saved by step 3, rough estimate at `w * h * 4`: all four wallpapers on this TV are
  3840×2160 JPEGs, so each wallpaper texture goes from 3840×2160 ≈ 31.6 MB to the cap of
  2048×1080 ≈ 8.4 MB (`largest_display` = quantise(1920·scale), quantise(1080·scale)) — about
  23 MB less resident GPU memory per wallpaper. Artwork is capped at 1024² ≈ 4 MB worst case;
  webOS icons are far below that, so they are untouched.

### Fair head-to-head on the TV (Apps grid, 400 forced presentations)

Both binaries run from the *same* app directory with the same assets, preferences and script
(`apps,up,up,down,down`), two runs each, so artwork resolution and Luna permissions were identical.
Verified equal workload: same `--dump-state` (32 apps, Apps page, same focus; only the
`preferences` block differs, which is a save-time artifact), same glyph counts (125–128) and
icon meshes (11–12), the alternating 13.05 MB / 13.12 MB artwork figure is the wallpaper cycle,
not a build difference.

| Metric (mean / p95, ms) | base `glass-ui` | new `optimize` |
| --- | --- | --- |
| compose (CPU layout) | 0.80 – 0.83 / 0.95 – 1.11 | **0.59 – 0.65 / 0.74 – 1.01** |
| CPU submission | 17.97 – 18.09 / 32.3 – 32.7 | 18.01 – 18.07 / 32.5 – 32.8 |
| Frame pacing | 20.12 – 20.34 / 34.4 – 34.9 | 19.91 – 20.08 / 34.4 – 34.8 |
| Swap | 1.15 – 1.21 / 1.6 – 1.9 | 1.15 – 1.18 / 1.5 – 1.8 |

Frame pacing is ~1 % better and the part the optimisation steps actually target — compose — is
**~22–26 % cheaper** (0.80 → 0.59 ms mean). CPU submission is unchanged because it is dominated by
the GL submission path; the Apps page runs at ~50 fps on this panel under a TV load average of ~14,
in both builds.

## TV state

- Installed build is `8885467`: IPK `com.ecklf.luma_2.16.2_optimize_scroll_arm.ipk`, sha256
  `a49e4577ac3a57d17e08c41f3b85b5cd20090719d92bb6e1303bd7a454de0824`; inner ARM binary sha256
  `cef10ef91da8264a9c8f3343aa6b1e01dc9a5868b89afe4760be3fa19fcdde19` (4,334,064 bytes,
  `e_machine=40`). Run `42031929644`, headSha `8885467` = local HEAD = fork tip; CI's
  `sha256sums` matches the local file. `verify_custom.py` PASS against stock / alpha /
  previous (`_optimize_arm`) / older (`_optimize_prev_arm`).
- Home takeover re-applied by the installer (a reinstall replaces the inode the mount points
  at): `/usr/bin/com.webos.app.home` reads `cef10ef9…`, pid 30449 runs it (`State: S`,
  RSS ~79 MB, 19 threads), boot hook `64-luma-home` intact, markers
  `/var/run/luma-home` + `-target` present, 0 panic/fault lines, live log shows
  `Luma TV inventory: 32 apps, 4 inputs` and clean minimize/restore around captures. The
  earlier stale deleted-inode artifact cleared with the reinstall.
- Offscreen captures must lift the marker (`/var/run/luma-home` → `.hold`, trap restores), run
  from the app directory with `XDG_RUNTIME_DIR=/tmp/xdg WAYLAND_DISPLAY=wayland-0
  SDL_VIDEODRIVER=wayland HOME=/home/root`, and use BusyBox `timeout -t 90`. Helpers:
  `shot-now.sh`, `one-capture.sh`, `ab-bind-capture.sh`, `who-runs.sh`, `live-health.sh`,
  `tidy-tmp.sh`.
- **A staged extra binary cannot use Luna.** Running `luma-prev` as a file in the app dir gets
  `LS2 registration failed (-1027)` on the artwork service, inventory retries, and
  "Frame budget ended before the input script completed" - no dump, no real screen. The fix
  is `ab-bind-capture.sh`: bind-mount the prev binary over the registered `<app>/luma` path,
  capture as `./luma`, `umount`. The running Home is unaffected because its exe inode lives on
  the separate `/usr/bin/com.webos.app.home` mount.
- `pidof luma` finds nothing because the process is Home; identify it by `/proc/*/exe`
  readlink or `/proc/mounts`. The sustained load average of ~14 is LG's own power-management
  kernel threads in `D` state (`REGUL-PM-TASK`, `LGDTV-PMDRV-TAS`, `CPUREGULTASK`); runnable
  tasks are 1–2 and it was the same before any install.
- Older IPKs are kept in `luma\` for comparison (`_arm`, `_alpha_arm`, `_glass_arm`,
  `_glass_ui_arm`, `_cine_ui_arm`, `_premium_arm`, `_zoom_ui_arm`, `_optimize_prev_arm`,
  `_optimize_arm`, `_optimize_fixes_arm`). `verify_custom.py` runs as `py -3 -X utf8` with
  five arguments. History: the previous build survived an unrequested TV reboot at 22:55 via
  the boot hook, and a ~18-minute SSH outage at 22:33 turned out to be standby, not the app.

## This pass: motion fixes, measured halo, scroll glide

### `e770a07` — why focus moves used to snap

The tile focus eases at a fixed 0.28 factor per frame, but the idle backoff sleeps up to 1 s
between polls. A focus change that arrives after a long idle woke the loop, and the frame's
eased interval was measured from the *previous presented frame* — tens of easing steps folded
into one, so the first frame after the wake landed on the settled value. The interval is now
clamped to `1/30 s`, so a wake consumes at most two frame steps and the ramp stays visible.

### `64943f5` — cards frozen mid-growth

Card grow/shrink/fly easing lives in the frame loop, not in `State::animating()`. With vsync
refused, an idle screen could go to sleep while a tile was still flying and freeze it mid
motion. `tile_motion` is now folded into the wake decision.

### `bfed94c` — dimmer, tighter halo, measured rather than eyeballed

`shots/glow-profile.py` compares the two Apps-grid captures pixel-by-pixel. They are
comparable: `--screenshot` disables the Ken Burns pan, the clock is fixed, and both runs use
the same preferences and the same script, which leaves focus on the first card. The card face
is at a known grid position (`84, 270.4, 270×154.4`), so the measurement averages the
*difference* in distance rings around that rectangle, masking the face, its rim (the rim
strokes were deliberately unchanged) and the caption strip below it, which is text and can
shift sub-pixel between builds:

```
ring (px from card edge)   before   after   delta
    6-15                    140.3   128.7    -11.6   (-8.2 %)
   15-30                     99.7    89.1    -10.6   (-10.6 %)
   30-60                     67.5    64.4     -3.1   (-4.6 %)
   60-110                    59.2    59.2     -0.0
  110-190                    66.8    66.8     +0.0
```

The bloom is dimmer where it was brightest and vanishes at the same distance as before —
nothing measurable beyond ~60 px. Side-by-side crop: `shots/glow_dim_compare.png`.

### `8885467` — the scrolling list glides

The Apps grid and Settings lists moved by the full reveal distance in one frame when focus
jumped to a row below the viewport. `State::scroll` (what is drawn) is now separate from
`State::scroll_target` (where it settles): `reveal_focus` only moves the target and the frame
loop eases the drawn position toward it with the same smoothing as the pill. Details:

- `reveal_focus` derives the focused control's place in the *unscrolled* list
  (`rect.y + scroll`), so pressing again mid-glide reveals the same row instead of adding the
  offset twice, and a row cannot end up tucked under the header.
- Changing the list itself — page, section, subpage, filter, search, header-pill focus — goes
  to the top at once through `jump_scroll` (replaces 21 `scroll = 0.` resets): sliding the old
  content away would just be noise. The pointer wheel sets both immediately and accumulates
  from the drawn position, so it tracks the wheel.
- `animating()` reports a gliding list, so the idle backoff cannot cut the glide short.
- A hidden window settles instead of animating off screen.
- Reduced Motion pins the drawn position to the target, and because the vertical list no
  longer changes layout at reveal time, `reveal_focus` returns `false` and `compose()` skips
  the second full scene build it used to do for every off-screen row — a CPU win on the C1
  as well as a behaviour fix. `tests/translations-smoke.mjs`, which asserts dumped `scroll`
  values, keeps passing on this property.
- Three new tests: the glide settles onto the row inside the viewport, Reduced Motion lands
  at once, and repeat presses mid-glide reveal the same row. Existing layout assertions now
  call `settle_scroll()` first.

### Formatting catch-up (`7ecfb1d`)

The full CI workflow on `8885467` (run `42038534506`) failed on `Check formatting`, so the new
tests never ran. `cargo fmt --check` printed nineteen hunks across six files, all from the
commits in this pass: method chains and `assert!` argument lists that fit inside 100 columns
but exceeded rustfmt's `chain_width` and `fn_call_width` of 60 wanted to be broken; two other
lines were the reverse case, a one-line `fn` signature and a `weighted.push(..)` tuple that
rustfmt joins. My pre-push review only checked line width, which is why this round of commits
shipped with fmt failures in it.

Each hunk is applied verbatim from the CI log, since there was no `cargo fmt` on this
machine to check against. Two more nits appeared in the new strip tests (rustfmt packs
short arguments onto one line when it breaks a call) and are folded into `a740884`.

That gate is now reproducible locally: rustfmt 1.93 from `static.rust-lang.org` runs
here, paired with the matching `rustc_driver-9c6fabb71b9022ee.dll` out of the 1.93.0
`rustc` tarball (rustfmt links against exactly that driver). `--edition 2024 --check`
over `src/*.rs` reproduces CI's `Check formatting`, and `--emit files` writes it, so
every commit since has been formatted before pushing rather than guessed. Passing the
check needs the default `newline_style`: the working tree is CRLF, and forcing `Unix`
reports spurious "Incorrect newline style" errors.

### `a740884` — the favourites strip glides too

Home had the same complaint as Apps: moving remote focus onto a card that does not fit slid
the strip the full distance in one frame. Home keeps its own horizontal scroll, so it needed
the same split: `State::home_scroll` is what is drawn, `State::home_scroll_target` is where
the reveal wants to end up, and the frame loop eases them with the same smoothing as the nav
pill. Notes:

- `ease_scroll` now drives both axes through a small `ease_axis` helper. The two calls are
  separate statements rather than a `||` chain on purpose: short-circuiting would skip the
  strip on any frame where the list also moved, and `animating()` would then keep asking for
  frames forever.
- `reveal_focus` measures a card against where it will sit once the strip has settled
  (`rect.x + home_scroll` is the place in the unscrolled strip, tested against
  `home_scroll_target`), so a press during a glide reveals the same card instead of
  accumulating the gap, and the card cannot finish half out of the strip.
- The pointer wheel still moves the strip immediately, from the visible position - direct
  manipulation should track the wheel. A no-delta call means the favourites changed
  underneath (reorder, hide, inventory refresh), so it clamps both positions at once rather
  than sliding across new content.
- Reduced Motion pins the drawn position to the target inside `reveal_focus`, and only that
  case - or a clamp - makes `reveal_focus` ask `compose()` for a second scene build. A
  gliding strip already paints the right geometry, so the double build is gone for ordinary
  reveals on Home as well as Apps.
- Three new tests mirror the Apps ones: the strip glides onto the revealed card inside the
  strip bounds, Reduced Motion lands at once, and repeat presses mid-glide reveal the same
  card. `tests/smoke.mjs` needed no change: its scenarios run 90 frames and the glide settles
  in about ten, and its `home_scroll` assertions come from the wheel path, which still moves
  immediately.

### TV verification of the glide

`custom-ipk.yml` only cross-compiles - it never runs `cargo fmt --check` or the tests - so the
full `CI` workflow was also dispatched on the branch (run `42038534506`) to actually check the
new tests and formatting. A/B on the TV via `ab-bind-capture.sh`, script
`apps,down,down,down,down` (focus lands on row 5, which needs 241.76 px of reveal),
prev = `bfed94c` binary staged as `<app>/luma-prev` and bind-mounted over the registered path:

- **Settled, `--frames 200`: both screenshots byte-identical** (`sha256 9ae17998c7cf…`,
  `shots/ab_settled.png`) and both state dumps report `scroll = 241.760009765625` with the
  same focus. The glide changes only *how* the list gets there, not where it ends up.
- **Mid-glide, `--frames 30`: the two screenshots differ** (`shots/abprev.png` settled vs
  `shots/abnew.png` still easing) — proof the intermediate positions really are rendered, and
  proof the bind ran the two different binaries.
- At the C1's actual pacing the `1/30 s` ease clamp means the smoothing factor is ~0.5 per
  frame, so the glide settles in about 8-10 presented frames (~0.25 s) after the reveal.
- Live Home afterwards: pid 30449 still running, `<app>/luma` and `/usr/bin/com.webos.app.home`
  both back at `cef10ef9…`, only the Home bind mount in `/proc/mounts`, 0 panic/fault lines,
  and the window restored cleanly after the capture stole Wayland focus.

## CI gate: the lint round

Run `42049465627` cleared `Check formatting` and then failed `pnpm check`
(`cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`), which
means the unit tests have still never executed. Two target-level failures:

- `bin "luma"`: 6 errors — `collapsible_if` twice (`resample::taps` and the webOS launch
  request in `main.rs`), `needless_range_loop` in `lanczos3_rgba`, `too_many_arguments`
  on `view::tile` (8/7), and `dead_code` for `Draw::AppImage` ("never constructed") and
  `Artwork::dark_badge` ("never used").
- `bin "luma" test`: 8 × `E0610` — `resampling_a_flat_colour_keeps_it_exact` wrote
  `source.0` on a bare integer, because its `(source, target)` pairs were single widths
  rather than sizes.

Fixed as four commits on `cf33eab…015c0e6…0572540…446dcbe`, each one lint fixed rather
than suppressed except where the code is genuinely the fork's own history:

- `446dcbe` gives the resample test nested `((src_w, src_h), (dst_w, dst_h))` pairs, which
  is what the existing `.0`/`.1` reads wanted, and widens the set to down/up/single-axis/
  extreme/identity.
- `0572540` collapses both `if`s into let-chains, walks `columns.iter().enumerate()`, and
  drops `tile`'s `names` bool by folding it into `caption: Option<f32>` — the caption
  vectors are already empty when names are off, so `None` means the same thing and the
  argument count goes 8 → 7 instead of gaining an `allow(too_many_arguments)`.
- `015c0e6` keeps `AppImage` and `dark_badge` with `#[allow(dead_code)]` and a comment
  each. Deleting them would cascade through the shader's backdrop path and about six
  artwork decode tests, so the fork keeps the upstream look restorable.
- `cf33eab` rewrites the two artwork scene tests that still described the surface the
  glass plate replaced: they now assert the inset `AppIcon` with a white tint on a dark
  translucent plate, one white mark over a dark offset for every badge, and the same face
  for plate and rim. These assertions have not run yet anywhere — CI is the only place
  this crate's tests execute.

Run `42064834654` is the CI re-dispatch on `optimize`. Formatting is green locally before
every push now; this run is the first that can reach `cargo test`.

## Questions for you

1. Glow level: the halo now measures ~8-11 % dimmer right beside the card and gone by 60 px.
   Right, or still too bright / now too faint? The dials are edge alpha (`0.42`) and falloff
   (`blur * 0.2`), one commit each.
2. Glide speed: the list and the favourites strip both use the same exponential smoothing as
   the nav pill (about 0.15-0.25 s to settle on the C1). If either feels slow or floaty, it
   can get its own factor - or the two can differ from each other.
3. While a card grows/shrinks/flies or a list or strip glides, the loop repaints continuously
   (vsync-paced). That is what makes motion smooth, but animation runs at full frame rate
   instead of the 30 fps used for the wallpaper pan. Fine on the C1, or cap animations to
   ~30 fps?

---

## 2026-11-10 - Voice orb: real microphone capture is unreachable in this TV state

The voice orb ships with a **simulated** audio envelope and one clearly marked plug point
(`src/state.rs:2789`, `real_voice_level()` returns `None`). Four independent investigations
were run against the TV (192.168.1.126, root SSH) to see whether real remote-mic PCM could
replace it. It cannot, and the reason is service-side state, not code. Recorded here so the
next attempt starts from the actual blocker.

### The real Magic Remote voice path

```
Magic Remote mic -> BLE (SBC/MSBC compressed) -> lginput2 decodes -> voiceinput service
                 -> /tmp/voiceinput_<clientKey>.soc
```

Evidence - `strings /usr/sbin/lginput2` (the Magic Remote service, `com.webos.service.mrcu`,
declared in `/usr/share/luna-service2/services.d/com.webos.service.mrcu.service`):

```
RS_deal_voice_stream_data
interpret_voice_packet
interpret_ble_voice_packet
voice_fd
voice_sbc%d
voice_msbc_origin%d
```

The remote sends **compressed** voice over BLE, decoded locally. Nothing about this path enters
the ALSA subsystem. `strings /usr/sbin/voiceinput` also shows the intended stream paths:

```
/tmp/voiceinput_<key>.soc
/tmp/voiceinput_debug.pcm
/tmp/voiceinput_mic_pre.pcm
/tmp/voiceinput_spk_pre.pcm
/tmp/voiceinput_rlt_pre.pcm
```

### The blocker, in the service's own words

```
voiceinput LunaHandler.cpp {} startStreaming:284] Receive message: {"id":"remote1","subscribe":true}
voiceinput DeviceStorage.cpp {} findDeviceInfo:1150] Can not find Device!
voiceinput LunaHandler.cpp {} startStreaming:448] End. respond: fail to find voice device
```

The service's **device table is empty**. `getDevices` returns `{"returnValue":true,"devices":[]}`.

### Everything tried, with results

| Approach | Result |
| --- | --- |
| `voiceinput/startStreaming {"id":"remote1"}` | `fail to find voice device` |
| startStreaming with 7 IDs (`remote1`, `LGSIC`, `O20 Builtin`, `LGE MR24`, `MRCU`, `mrcu`, `remote`) | all `fail to find voice device` |
| `setSystemSettings speakToTv=on` | accepted `returnValue: true`, `getActivation` still `activationState:false` |
| `voiceinput/setting/setActivation` | `Unknown method "setActivation"` |
| `setting/enable` / `activate` / `start` | all `Unknown method` |
| `voiceconductor/getVoiceUiEvents` | `{"event":"sessionEnd","subscribed":false}` |
| `voiceconductor/setInputEvent {"inputType":"wakeword"}` | `returnValue: true`, but opens no stream |
| `voiceconductor/recordVoice` (app x voiceModel variants) | `precondition not satisfied` |
| `/dev/uinput` inject key 428 (`RF_VOICE`) | injected OK; RMS 3.61 -> 5.0, max 558 -> 578. No stream. |
| direct write to `/dev/input/event3` | write succeeded, no trigger |

`getActivation` reports `systemStatus:true` but `settingStatus:false`, `activationState:false`,
`supportPlatform:false` on both the thinQ and amazon branches. There is no Lua API to flip this.

`mrcu/isPaired` is `true` (`deviceName: "LGE MR24"`), but `mrcu/getConnectionStatus` reports
`connected:false` and `hid/subscribeDevicesStatus` returns an empty device list. So the remote's
BLE voice link is not registering a device with the voice service.

### ALSA is the TV's own mic, not the remote's

`arecord -l`:

```
card 1: LGSIC [LGSIC], device 0: WoV PDM Mic snd-soc-dummy-dai-0 []
```

This is the **built-in far-field PDM mic**, not the Magic Remote. Two independent captures
(48 kHz stereo; the device rejects 16 kHz mono with `Channels count non available`):

```
arecord -D hw:1,0 -f S16_LE -c 2 -r 48000 -d 2 -t raw /tmp/direct_mic.pcm
384,000 bytes, RMS 3.61, max 558
```

RMS 3.61 / 32768 = 0.011%. The samples are DC-biased near-silence: `min=-558`, `max=0`, most
samples `-1`. `hw:1,2` MixerCapture and `hw:1,3` SoundEngineCapture both read RMS `0.00`.
Both captures land at the same value, so it is reproducible and not a fluke.

### Conclusion for the Rust side

Do **not** wire ALSA `hw:1,0` into `real_voice_level()`. It would feed the orb a flat level
from a DC-biased near-silent signal that is the TV's idle built-in mic, not the user's voice.
That is less honest than the current marked simulation.

`real_voice_level()` at `src/state.rs:2789` stays `None`. The comment already names the intended
path so a future attempt knows what to wire.

### What would unblock it

1. `supportPlatform` / `activationState` becoming true for the voiceinput service - the setting
   lives behind the stock Settings UI (Voice recognition) or an LG account login, and may not be
   exposed to a rooted install at all.
2. The remote's BLE voice link registering a device with the voice service, which is what fills
   the empty `DeviceStorage` table.
3. Starting a capture session exactly on the mic-key event from `/dev/input/event3` (key 428),
   rather than proactively via `startStreaming`.

Each of those is verification work on the TV with the physical remote in hand, which is why this
stays parked rather than improvised in code.
