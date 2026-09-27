//! Adapts [`machine_core::MachineBus`] to `m68k`'s [`AddressBus`] trait.
//!
//! Lives here for the same orphan-rule reason `tests/hello_guest.rs`
//! documents in `machine-core`: both `MachineBus` and `AddressBus` are
//! foreign to this crate, so neither side can carry a blanket `impl`.

use std::sync::atomic::{AtomicU32, Ordering};

use m68k::{AddressBus, FastMem};
use machine_core::MachineBus;

use crate::blitter_trace::BlitterTrace;

/// Bus-access region, for the `BUS_COVERAGE` diagnostic (plan step 7.2):
/// which memory region a CPU access lands in, split from whether it is
/// an instruction fetch or a data access at the call site. Order matches
/// [`Coverage`]'s arrays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
    /// Fast RAM's AUTOCONFIG-placed window ([`MachineBus::fast_ram_window`]
    /// -- asked, never a constant, per `docs/device-ledger.md`).
    Fast = 0,
    /// Chip RAM (`machine_core::CHIP_RAM_BASE..CHIP_RAM_END`) -- a fixed
    /// architectural range, not AUTOCONFIG-placed, so a constant is the
    /// right source here (same posture `machine-core`'s own `fast_region`
    /// takes).
    Chip = 1,
    /// The ROM window (`machine_core::ROM_BASE..ROM_END`) -- likewise
    /// fixed, not AUTOCONFIG-placed.
    Rom = 2,
    /// Anything else: custom chips, CIAs, AUTOCONFIG space, and every
    /// AUTOCONFIG-placed device board (hostblk, pktport, rtgboard, input,
    /// pcibridge, graphics) not lumped in above.
    Other = 3,
}

/// Bus-access counters for the `BUS_COVERAGE` diagnostic
/// (`docs/bus-fast-path-plan.md` step 7.2): instruction fetches
/// (`read_immediate_word`/`read_immediate_long`) and data accesses (every
/// other read/write), by [`Region`]. `None` unless `BUS_COVERAGE` is set
/// at [`Bus`] construction -- a plain run pays nothing for this, the same
/// "diagnostic gated at construction, not re-checked per access" posture
/// `docs/bus-fast-path-plan.md` §3.5 established for `SERIAL_REG_TRACE`.
#[derive(Default)]
pub struct Coverage {
    pub fetch: [u64; 4],
    pub data: [u64; 4],
}

impl Coverage {
    fn record(&mut self, region: Region, is_fetch: bool) {
        let slot = if is_fetch {
            &mut self.fetch
        } else {
            &mut self.data
        };
        slot[region as usize] += 1;
    }

    /// Render the percentages `run.rs` prints at the end of a
    /// `BUS_COVERAGE`-enabled run.
    pub fn format(&self) -> String {
        let names = ["fast RAM", "chip RAM", "ROM", "other"];
        let fetch_total: u64 = self.fetch.iter().sum();
        let data_total: u64 = self.data.iter().sum();
        let mut out = String::new();
        out.push_str("bus coverage (instruction fetches):\n");
        for (i, name) in names.iter().enumerate() {
            let pct = if fetch_total > 0 {
                100.0 * self.fetch[i] as f64 / fetch_total as f64
            } else {
                0.0
            };
            out.push_str(&format!("  {name:8}: {:>12} ({pct:5.1}%)\n", self.fetch[i]));
        }
        out.push_str("bus coverage (data accesses):\n");
        for (i, name) in names.iter().enumerate() {
            let pct = if data_total > 0 {
                100.0 * self.data[i] as f64 / data_total as f64
            } else {
                0.0
            };
            out.push_str(&format!("  {name:8}: {:>12} ({pct:5.1}%)\n", self.data[i]));
        }
        out
    }
}

/// PC of the instruction currently executing, stored by `run_guest`'s
/// per-instruction hook so the serial register trace below can attribute
/// accesses. Diagnostic only.
pub static LAST_PC: AtomicU32 = AtomicU32::new(0);

const CUSTOM_BASE: u32 = 0x00DF_F000;
const CUSTOM_END: u32 = 0x00E0_0000;

/// Diagnostic trace of every serial-related custom register access
/// (SERDATR/SERDAT/SERPER, plus INTENA/INTREQ writes touching the RBF or
/// TBE bits), gated on the `SERIAL_REG_TRACE` env var -- read once at
/// [`Bus`] construction into [`Bus`]'s own `.2` field rather than through
/// a `OnceLock` re-checked on every single access
/// (`docs/bus-fast-path-plan.md` §3.5: this ran once per byte/word/long
/// access on every guest instruction that touched the bus, whether or
/// not the trace was ever enabled). `enabled` is that cached bool, so
/// this is `#[inline]`: the common case (tracing off) is one branch and
/// nothing else.
#[inline]
fn trace_serial(enabled: bool, kind: &str, address: u32, value: u16) {
    if !enabled || !(CUSTOM_BASE..CUSTOM_END).contains(&address) {
        return;
    }
    let offset = (address - CUSTOM_BASE) as u16 & 0x1FE;
    let name = match offset {
        0x018 => "SERDATR",
        0x030 => "SERDAT",
        0x032 => "SERPER",
        0x09A if value & 0x0801 != 0 => "INTENA",
        0x09C if value & 0x0801 != 0 => "INTREQ",
        _ => return,
    };
    let pc = LAST_PC.load(Ordering::Relaxed);
    eprintln!("SERTRACE {kind} {name} val={value:#06x} pc={pc:#010x}");
}

/// `.1` is `--blitter-trace`'s recorder, `None` on a plain run (the
/// default) -- every write path below pays exactly one `if let Some`
/// check in that case and nothing else (`blitter_trace.rs`'s module doc
/// comment on gating). `.2` is whether `SERIAL_REG_TRACE` was set at
/// construction time (`docs/bus-fast-path-plan.md` §3.5) -- `trace_serial`
/// takes it as its first, cheapest-to-check argument rather than looking
/// it up itself. `.3` is whether `--cpu-speed max` is active
/// (`docs/adr-0006-cycle-budgeted-and-wall-clock-paced-timing.md`) --
/// [`AddressBus::take_boundary_request`] only ever delegates to
/// [`MachineBus::take_boundary_request`] when this is set, so cycle mode
/// (the default, `.3 == false`) is byte-for-byte unaffected by
/// `machine-core` now tracking that flag at all: `run_for_cycles_with_hook`
/// never calls `take_boundary_request` on cycle mode's own run loop
/// (`run_guest`'s hook drives everything explicitly instead), but a
/// future caller of the plain hook-free `run_for_cycles` on this same
/// `Bus` in cycle mode must still see `false` unconditionally, exactly
/// today's behaviour.
pub struct Bus<'a>(
    pub MachineBus<'a>,
    pub Option<BlitterTrace>,
    pub bool,
    pub bool,
    pub Option<Coverage>,
);

impl Bus<'_> {
    /// Whether `SERIAL_REG_TRACE` was set at construction (`.2`, read
    /// once in `run.rs` rather than on every call) -- exposed as a
    /// method so `run_guest`'s hook can skip storing
    /// [`LAST_PC`] on every retired instruction when nothing is tracing
    /// (`LAST_PC` exists purely to attribute [`trace_serial`]'s output,
    /// so a plain run with no trace enabled has no use for it at all).
    #[inline]
    pub fn serial_trace_on(&self) -> bool {
        self.2
    }

    /// Classify `address` into a [`Region`] for the `BUS_COVERAGE`
    /// diagnostic. Fast RAM is asked of `MachineBus` (its window is
    /// AUTOCONFIG-placed and can move); chip RAM and ROM are fixed
    /// architectural ranges, so the same constants `machine-core`'s own
    /// `fast_region` uses are the right source for those.
    fn classify(&self, address: u32) -> Region {
        if (machine_core::CHIP_RAM_BASE..machine_core::CHIP_RAM_END).contains(&address) {
            return Region::Chip;
        }
        if (machine_core::ROM_BASE..machine_core::ROM_END).contains(&address) {
            return Region::Rom;
        }
        if let Some((base, len)) = self.0.fast_ram_window() {
            if address >= base && address - base < len {
                return Region::Fast;
            }
        }
        Region::Other
    }

    #[inline]
    fn record(&mut self, address: u32, is_fetch: bool) {
        if self.4.is_some() {
            // Classify before taking the mutable borrow below: `classify`
            // reads `self.0`, and `self.4`'s `&mut` would otherwise
            // conflict with `self`'s own `&self` borrow for that call.
            let region = self.classify(address);
            if let Some(coverage) = &mut self.4 {
                coverage.record(region, is_fetch);
            }
        }
    }

    /// Take the accumulated [`Coverage`] counters, if `BUS_COVERAGE` was
    /// set, for `run.rs` to report once the guest stops.
    pub fn take_coverage(&mut self) -> Option<Coverage> {
        self.4.take()
    }
}

impl AddressBus for Bus<'_> {
    fn take_boundary_request(&mut self) -> bool {
        // `.3`'s own doc comment: only max mode ever asks `MachineBus`,
        // so cycle mode gets the trait default (`false`) unconditionally
        // regardless of whether a device write set the flag underneath.
        if self.3 {
            self.0.take_boundary_request()
        } else {
            false
        }
    }

    fn read_byte(&mut self, address: u32) -> u8 {
        self.record(address, false);
        let value = self.0.read_byte(address);
        trace_serial(self.2, "Rb", address, value as u16);
        value
    }

    fn read_word(&mut self, address: u32) -> u16 {
        self.record(address, false);
        let value = self.0.read_word(address);
        trace_serial(self.2, "R", address, value);
        value
    }

    fn read_long(&mut self, address: u32) -> u32 {
        self.record(address, false);
        self.0.read_long(address)
    }

    fn write_byte(&mut self, address: u32, value: u8) {
        self.record(address, false);
        trace_serial(self.2, "Wb", address, value as u16);
        self.0.write_byte(address, value);
    }

    fn write_word(&mut self, address: u32, value: u16) {
        self.record(address, false);
        if let Some(trace) = &mut self.1 {
            trace.observe_word(address, value);
        }
        trace_serial(self.2, "W", address, value);
        self.0.write_word(address, value);
    }

    fn write_long(&mut self, address: u32, value: u32) {
        // Observed as the two word writes `MachineBus::write_long` itself
        // decomposes a long write into -- graphics.library commonly pokes
        // register pairs like `BLTCON0`/`BLTCON1` with one `MOVE.L`, so a
        // recorder that only watched `write_word` would miss those.
        self.record(address, false);
        if let Some(trace) = &mut self.1 {
            trace.observe_word(address, (value >> 16) as u16);
            trace.observe_word(address.wrapping_add(2), value as u16);
        }
        trace_serial(self.2, "W", address, (value >> 16) as u16);
        trace_serial(self.2, "W", address.wrapping_add(2), value as u16);
        self.0.write_long(address, value);
    }

    fn read_immediate_word(&mut self, address: u32) -> u16 {
        self.record(address, true);
        let value = self.0.read_word(address);
        trace_serial(self.2, "R", address, value);
        value
    }

    fn read_immediate_long(&mut self, address: u32) -> u32 {
        self.record(address, true);
        self.0.read_long(address)
    }

    /// Direct window into fast RAM's placed AUTOCONFIG window, for
    /// `m68k::CpuCore::run_batch` (`--cpu-backend batch`,
    /// `docs/bus-fast-path-plan.md` step 7.2). Refused whenever anything
    /// intercepts bus accesses -- the register trace and the blitter
    /// trace both watch specific register ranges today, never fast RAM,
    /// but `FastMem`'s contract (`m68k`'s own doc comment) is "no
    /// interception while the window is active" and a future intercepted
    /// range could move, so both are checked unconditionally rather than
    /// reasoned about address-range by address-range here.
    fn fast_mem(&mut self) -> Option<FastMem> {
        if self.2 || self.1.is_some() {
            return None;
        }
        let (base, mem) = self.0.fast_ram_window_mut()?;
        if mem.len() < 4 {
            return None;
        }
        Some(FastMem {
            // SAFETY: `mem` borrows fast RAM's actual backing buffer
            // (caller-owned, `MachineBus::with_fast_ram`'s `&'a mut`),
            // clamped to the shorter of the AUTOCONFIG window and that
            // buffer (`fast_ram_window_mut`'s own doc comment) -- so
            // every address in `[base, base + len)` is backed by real
            // storage that does not move or resize for as long as `self.0`
            // lives, which trivially outlives one `run_batch` call. No
            // side effects: fast RAM is plain memory, never MMIO.
            ptr: mem.as_mut_ptr(),
            base,
            len: mem.len() as u32,
        })
    }
}
