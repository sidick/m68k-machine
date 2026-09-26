# Hosted runtime speed: implementation plan

**Status:** plan, 26 September 2026. Supersedes the "MachineBus fast path
— sketch" for sequencing; that sketch's §2 design is adopted as step 3
below. Core-independent: no change to the `m68k` fork.
**Applies to:** `crates/machine-core/src/lib.rs`, `autoconfig.rs`,
`crates/machine-hosted/src/{bus.rs,run.rs}`, root `Cargo.toml`.

## 0. Where the time goes today

Measured on an M3 Pro, release build, Kickstart 3.2.2 (A1200 ROM),
`--cpu 68040` default, `--max-frames 1500 --max-instructions 0`,
profiled with `samply` and symbolized against the binary:

| Config | Instructions | Wall | MIPS |
|---|---|---|---|
| ROM + fast RAM only ("bare") | 136 M | 12.3 s | 11 |
| `--pcibridge --rtgboard 640x480 --hostblk aros.hdf` ("full") | 88 M | 8.7 s | 10 |

Self-time share, bare run: `MachineBus::read_byte` 21.7%,
`dispatch_instruction` 12.9%, `write_byte` 6.2%, hook closure 6.2%,
`AutoConfig::board_at` 4.9%, `Cia::tick` 4.8%, `cycles_040` 4.2%,
`MachineBus::tick` 3.4%, `read_word` 3.2%, `graphics_target` 2.8%,
`trace_serial` 2.3%, `fast_ram_target` 2.2%, `service_host_serial`
2.1%, `drain_serial` 1.7%, `mirage_target` 1.6%.

Full run additions: `board_at` rises to 8.4% (134 of 198 samples from
`read_word` via `pcibridge_aperture_sized`), `MachineBus::tick` is 27.6%
inclusive: memmove from `Option::take()` 6.4%, `intx_levels` 5.5%,
`Cia::tick` 4.8%, virtio-net tick 3.3%, `Hostblk::tick` 1.5%.

Three buckets, in order of size: the bus chain (~45% bare), the
per-instruction tick and hook (~20% bare, ~40% full), the build profile.

## 1. Measurement harness (do first, ships with every step)

Every step below is accepted on numbers, not on tests passing. Silent
failure is this platform's norm; a speedup that isn't measured isn't
one.

1. `scripts/bench-boot.sh`: runs `target/release/machine-hosted` on
   `$M68K_KICKSTART_A1200` for the two configs above at 1500 frames,
   prints wall clock, instruction count and MIPS from the `LIMIT REACHED`
   line. Fails loudly if the ROM is absent (same posture as
   `phase3-gate.sh`). Optional `--profile` flag wraps the run in
   `samply record --save-only`.
2. `scripts/bench-symbolize.py`: the aggregator used for §0 (self and
   inclusive time per function from a samply profile, symbolized with
   `atos`). Committed so before/after profiles are comparable.
3. Record the §0 numbers in this file's "Results" section as the
   baseline. Each subsequent step appends its own row.

Done when: the script reproduces §0 within noise on the untouched tree.

### How to run

```sh
scripts/bench-boot.sh --label baseline
scripts/bench-boot.sh --profile /tmp/bench-profile
```

The first prints the two configs' wall clock, instruction count and
MIPS, plus a Markdown row for the Results table below. The second
additionally records each run with `samply record --save-only` and
symbolizes both profiles with `scripts/bench-symbolize.py` into
`<dir>/{bare,full}.txt`. `--no-build` skips the release rebuild;
`--help` lists every flag. Env vars: `M68K_KICKSTART_A1200` (ROM,
default `nondistribution/A1200.47.115.rom`), `M68K_BENCH_HDF` (hostblk
image, default `nondistribution/aros/aros.hdf`), `FRAMES` (frame count
for both configs, default `1500`).

## 2. Release profile (zero-code, ~10%)

Add to the root `Cargo.toml`:

```toml
[profile.release]
lto = "fat"
codegen-units = 1
```

Measured: bare 12.27 → 10.69 s, full 8.67 → 7.95 s.

Checks: the board crates share the workspace profile. Build both with
their explicit `--target`, run `scripts/run-qemu-virt.sh` and
`run-qemu-q35.sh`, assert markers with `check-serial-markers.sh`. Note
in `docs/ci.md` that release builds are LTO and why. Do **not** add
`panic = "abort"` here (`.cargo/config.toml` explains why).

## 3. Bus fast path (the sketch's §2, with three additions)

### 3.1 Cache placements once, scan never on the hot path

In `MachineBus`, add a small cached-window table refreshed from
`AutoConfig` whenever a placement can change. There are exactly two
mutation sites: `AutoConfig::configure` (reached only through the
AUTOCONFIG arm of `write_byte`) and the `with_*` builders. Do not
thread a generation counter; call `refresh_windows()` at the end of the
`write_byte` AUTOCONFIG arm and at the end of `with_fast_ram`.

```rust
/// Fast RAM's placed window, `None` until expansion.library places it.
fast_window: Option<(u32, u32)>,          // (base, len)
/// pcibridge's placed window, same shape.
pcibridge_window: Option<(u32, u32)>,
```

Both derive from `autoconfig.placement(idx)`; never from a constant
(`docs/device-ledger.md`, standing rule 2). `debug_assert!` in
`refresh_windows` that neither window overlaps `AUTOCONFIG_BASE..END`.

### 3.2 Width-native fast path in front of the chain

Exactly the sketch's §2.2: `fast_region(&self, addr) -> Option<(&[u8],
usize)>` covering fast RAM (via `fast_window`), chip RAM (except reads
under the overlay), and ROM when `rom.len() == ROM_WINDOW_SIZE`;
`fast_region_mut` for writes covering fast RAM and chip RAM (writes
under the overlay still land in chip RAM, as today). `read_{byte,word,
long}` and `write_{byte,word,long}` try it first with
`get(off..off+N)` so an end-of-region straddle falls through to the
existing byte path unchanged.

Pin with `const _: () = assert!(CIA_BASE >= CHIP_RAM_END &&
CUSTOM_BASE >= CHIP_RAM_END);`.

### 3.3 Move `pcibridge_aperture_sized` behind the fast path and off `board_at`

`read_word`/`read_long`/`write_word`/`write_long` currently call
`pcibridge_aperture_sized` (a full `board_at` scan) before anything
else. Reorder it after the fast path, and reimplement
`pcibridge_target` against `pcibridge_window` (subtract-and-compare, no
scan). This is the single largest bus item in the full config and the
sketch does not address it.

### 3.4 One `board_at` per device access

Replace the `graphics_target → mirage_target → fast_ram_target →
hostblk_target → input_target → pktport_target → rtg_target →
pcibridge_target` ladder (up to seven scans per access, and rtgboard
VRAM writes are Workbench drawing) with one `board_at` call, then a
match of the returned index against the stored `*_board` indices.
Behaviour is identical: the chain order today is only meaningful for
Graffity, whose `graphics_boards` lookup keeps its own position check.

### 3.5 Hosted wrapper

`bus.rs`: replace `serial_trace_enabled()`'s `OnceLock` lookup per
access with a `bool` field on `Bus` set at construction. `trace_serial`
becomes `#[inline]` with the bool as its first check.

### Tests for step 3

- Existing: the 43 `lib.rs` unit tests, in particular
  `overlay_maps_rom_at_zero_until_ovl_is_cleared`,
  `undersized_rom_mirrors_across_the_window`,
  `configured_fast_ram_routes_reads_and_writes_and_clips_past_the_real_buffer`,
  `open_bus_*`, and every `configured_*_routes_*` device test, must pass
  unchanged. They are the spec for what the fast path must not swallow.
- New: a straddle test (word at `CHIP_RAM_END - 1`, long at fast RAM
  end - 2) equal to the byte-composed result; a test that a placement
  change (re-running AUTOCONFIG on a fresh bus) invalidates
  `fast_window`; a test that an undersized ROM (not exact fit) still
  mirrors; a differential test that walks every 4 KB of the address map
  comparing fast-path and byte-path results for byte/word/long with all
  devices attached.
- End to end: `cargo test -p machine-hosted` real-ROM suite, both QEMU
  smoke scripts, and `scripts/phase3-gate.sh` where the media exists.

Done when: bare and full rows in Results, and the post-step profile
shows `read_byte` + `board_at` + `*_target` under 10% combined.

## 4. Per-instruction tick and hook

### 4.1 Batch the device engines per raster line (machine-core)

`MachineBus::tick` runs hostblk, pktport, mirage, pcibridge (virtio-net
ring walks) and the two IRQ polls on every retired instruction. Their
contracts are already "deferred completion, one step per call"; one
raster line of latency (~908 CPU clocks, ~100 instructions) is
invisible to the guest and is exactly the grain the STOP-path resync
already uses (`STOP_TICK_SLICE`).

Inside `tick`, accumulate `cpu_clocks` into `engine_carry` and run the
four engines and their `irq_pending()` polls only when
`beam.lines_started > 0`. Chipset and CIA ticks stay per call. Because
this lives in `MachineBus::tick`, both bare-metal boards (which have
their own copies of the hook loop, `board-qemu-virt/src/main.rs:724`,
`board-qemu-q35/src/main.rs:623`) get it without change.

Update the "one step per call" sentences in `hostblk.rs`, `pktport.rs`,
`mirage.rs` and `pcibridge.rs` module docs and in `docs/*-protocol.md`
where they promise per-tick servicing, in the same commit.

Tests: hostblk and pktport unit tests that count ticks-to-completion
must be re-expressed in lines (tick with a line's worth of clocks).
`hostblk-soak`/devsoak and `pktport-e2e.sh` are the acceptance gates.

### 4.2 Stop moving devices through `Option::take()`

Split `MachineBus`'s RAM into a `GuestRam<'a> { chip_ram, fast_ram,
fast_window }` field that implements `GuestMemory` by itself. Device
engines then take `&mut self.ram` while `self.hostblk` is borrowed,
removing the take/put-back (a ~400-byte memmove twice per device per
tick, 6.4% of the full run) and the comments explaining it. `ram_slice`
/`ram_slice_mut` move to `GuestRam`; `MachineBus` keeps a delegating
impl so `screenshot.rs`, `introspect.rs` and `pktvol.rs` are untouched.

### 4.3 CIA: skip the E-clock loop when idle

`Cia::tick_eclock` loops once per E-clock. Add an early return when
neither timer is started (`cra & CRA_START == 0 && crb & CRB_START ==
0`) and the keyboard queue is empty; otherwise unchanged. Cheap and
exact. A full next-event computation is not worth it until the profile
says so.

### 4.4 Hook closure (hosted only)

In `run.rs`'s hook:

- test `bus.0.chipset.frames != last_serviced_frame` at the call site
  before calling `service_host_serial`; the function keeps its own
  check for the STOP path.
- `drain_serial`: `#[inline]`, and check `serial_len == 0` via a cheap
  `has_serial_byte()` before the loop.
- `LAST_PC.store` only when the serial trace bool from 3.5 is set.
- `screenshot_job.maybe_capture` only when `frames` changed.

Mirror the first three in the two board `main.rs` loops where they
exist.

Done when: `MachineBus::tick` inclusive is under 5% in the full run and
the hook closure's self time is under 3%.

## 5. Re-profile and decide the CPU-core question

Run the harness, append rows, and produce the after-profile. If
`dispatch_instruction`, `cycles_040`, `resolve_ea`, `read_imm_*` and
the rest of the `m68k` frames are now the majority, the CPU-core
proposal starts. Two facts for that decision, recorded here so they
are not rediscovered:

- The fork already has `run_batch` with a `FastMem` window (single
  side-effect-free RAM region, direct host pointer, no bus call per
  access). It has no per-instruction hook and clobbers cycle
  accounting, which conflicts with this machine's cycles-to-beam
  timebase, and the contract forbids any interception in-window, so
  `--blitter-trace` and `SERIAL_REG_TRACE` must disable it.
- `cycles_040` (4%) is the 68040 timing table cost; a `--cpu 68030`
  run is a one-flag experiment to size it.

## 6. Sequencing and ownership

| Step | Size | Crates touched | Gate |
|---|---|---|---|
| 1 harness | S | scripts | reproduces §0 |
| 2 LTO | XS | Cargo.toml, docs/ci.md | QEMU markers |
| 3 bus fast path | M | machine-core, machine-hosted | unit + real-ROM + QEMU + gate |
| 4.1–4.3 tick | M | machine-core (+ protocol docs) | soak, pktport-e2e, QEMU |
| 4.4 hook | S | machine-hosted, boards | real-ROM |
| 5 re-profile | S | docs | numbers |

Steps 2, 3 and 4 are independent and can go to separate workers, each
on its own branch, each landing with its Results row. Step 3 is the
one that needs the most review: every change to `read_byte`'s chain is
a change to what every device sees, and the unit tests are
self-consistent (they have passed over real bugs before), so the
real-ROM boots and the QEMU markers are the evidence, not `cargo test`.

Commit discipline per `CLAUDE.md`: fmt and clippy (`-D warnings`, both
hosted crates and both board targets) before every commit; ledger,
protocol docs and this file's Results table updated in the commit that
makes them true.

## Results

| Step | Bare 1500f | Full 1500f | Notes |
|---|---|---|---|
| baseline | 12.27 s (11.1 MIPS) | 8.67 s (10.2 MIPS) | §0 profile |
| 2 LTO (measured, not landed) | 10.69 s | 7.95 s | |
