//! Blitter differential against Copperline (proposal §12, "differential
//! against Copperline (randomised ops + recorded Workbench traces)"; the
//! 21 host-side unit tests in `crates/machine-core/src/blitter.rs` are
//! this proposal's other, non-differential half). Both halves of this
//! file's own brief live here: the randomised case generators below, and
//! `workbench_corpus_cases`, which replays a corpus of distinct blitter
//! register signatures recorded from a real booted planar Workbench
//! desktop (`crates/machine-hosted/src/blitter_trace.rs` records it,
//! `tests/fixtures/blitter_workbench_corpus.txt` is the checked-in
//! result). See `docs/blitter-differential.md` for the full write-up:
//! what this covers, what it deliberately doesn't, and any divergences
//! found.
//!
//! **Mechanism.** Copperline's blitter is cycle-exact and DMA-gated; ours
//! (`machine_core::blitter::Blitter`) is synchronous and runs whenever a
//! size register is written, ignoring `DMACON` entirely. So this can't
//! just poke Copperline's chip RAM and registers over the control
//! protocol (CCP) directly -- `mem.write`/`mem.read` are explicitly
//! side-effect-free and don't reach the custom-register I/O window (only
//! RAM-backed regions are mutated/read; confirmed empirically and in
//! Copperline's own `cpu.rs` doc comments for `debug_write_memory`/
//! `debug_read_memory`). Instead this hand-assembles a tiny 68k program
//! (register pokes via `MOVE.W #imm,ABS.L`, a `BTST`/`BNE` spin on
//! `DMACONR` bit 14 (`BBUSY`), then a settle margin and a self-branch) and
//! runs it on Copperline's own CPU via CCP's `regs.set`/`run_until{"pc":
//! ...}`, the same way real hardware and graphics.library actually drive
//! the blitter. `mem.write`/`mem.read` remain perfectly fine for seeding
//! and reading back chip RAM itself (RAM-backed), just not for touching
//! registers.
//!
//! The injected program is placed at a **headless `--control` session's
//! reset state**, before any ROM code runs: `regs.set pc=...` overrides
//! the CPU's reset-vector fetch outright, so the AROS boot ROM never
//! executes and never disturbs chip RAM. Interrupts stay masked (`SR =
//! $2700`, matching real hardware's own reset state) and nothing else
//! (Copper, bitplane DMA, CIA) is enabled, so chip RAM is touched by
//! nothing but this test's own pokes and the blitter itself.
//!
//! One empirically-discovered gotcha, load-bearing enough to call out
//! here as well as in the doc: the low ~1 MiB of chip RAM aliases the
//! boot ROM overlay at reset (`OVL`), so `mem.write` there silently
//! writes zero bytes and a hijacked PC pointed into it runs stray ROM
//! code instead of this test's program. Every address this file uses is
//! comfortably above that (`ARENA_BASE` below).
//!
//! A second gotcha, and a real finding about Copperline's timing model
//! (documented in `docs/blitter-differential.md`): `DMACONR` bit 14
//! (`BBUSY`) clears a handful of cycles before the blit's *final* word is
//! actually committed to chip RAM. Reading back immediately on `BBUSY`
//! going clear intermittently misses the last word (observed directly
//! while building this harness). `build_program`'s settle margin (32
//! `NOP`s) after the busy-wait loop is not decorative -- removing it
//! reintroduces the miss.
//!
//! **Why a hand-rolled program instead of an assembler dependency:** the
//! instruction set needed is exactly four forms (`MOVE.W #imm,ABS.L`,
//! `MOVE.W ABS.L,Dn`/`Dn,ABS.L` for the one CPU-side register capture
//! `mem.read` can't reach, `BTST #imm,ABS.L`, and short `Bcc`), all with
//! fixed, hand-verified encodings -- less surface than a new dependency.
//!
//! **Why one Rust ignored test, not a script:** follows
//! `crates/machine-hosted/tests/real_rom.rs`'s convention for
//! slow/optional-input tests. `#[ignore]`s cleanly, skips (doesn't fail)
//! when `copperline` isn't on `PATH`, and needs no directly-invoked
//! script for the one documented command in `docs/blitter-differential.md`
//! to work.
//!
//! Run: `cargo test -p machine-hosted --test blitter_differential -- --ignored --nocapture`

use machine_core::blitter::{self, reg, Blitter};
use machine_core::CHIP_RAM_SIZE;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Fixed seed for every randomised case in this file, printed alongside
/// each divergence so a failure is reproducible without rerunning the
/// whole suite blind.
const SEED: u64 = 0xB717_1E5D_1FF7_D1FF;

// ---------------------------------------------------------------------
// A tiny deterministic PRNG (SplitMix64). Not `rand`: the only thing
// this file needs is a handful of small bounded values, and pulling in a
// dependency for that would be more than the problem needs -- unlike
// `serde_json` below, which earns its place parsing Copperline's nested
// JSON-RPC responses.
// ---------------------------------------------------------------------
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn u16(&mut self) -> u16 {
        (self.next_u64() >> 16) as u16
    }

    fn range(&mut self, lo: i32, hi_inclusive: i32) -> i32 {
        let span = (hi_inclusive - lo + 1) as u64;
        lo + (self.next_u64() % span) as i32
    }

    fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

// ---------------------------------------------------------------------
// Hand-rolled 68k assembler: exactly the instruction forms this harness
// needs, each encoding verified against a live Copperline session while
// this file was written (see the module doc comment).
// ---------------------------------------------------------------------
struct Asm {
    base: u32,
    code: Vec<u8>,
}

impl Asm {
    fn new(base: u32) -> Self {
        Self {
            base,
            code: Vec::new(),
        }
    }

    fn addr(&self) -> u32 {
        self.base + self.code.len() as u32
    }

    fn word(&mut self, w: u16) {
        self.code.extend_from_slice(&w.to_be_bytes());
    }

    fn long(&mut self, l: u32) {
        self.code.extend_from_slice(&l.to_be_bytes());
    }

    /// `MOVE.W #imm,ABS.L` -- opcode `$33FC`.
    fn move_w_imm_absl(&mut self, imm: u16, addr: u32) {
        self.word(0x33FC);
        self.word(imm);
        self.long(addr);
    }

    /// `MOVE.W src.L,D0` then `MOVE.W D0,dst.L` -- the only way to read a
    /// live device register back into RAM the CCP can then `mem.read`,
    /// since `mem.read` itself is side-effect-free and does not reach
    /// the custom-register I/O window (see module docs). Used for
    /// `BLTDDAT`, which Copperline's `custom.read`/`custom.dump` (unlike
    /// every other blitter register) reports as "not readable".
    fn capture_absl_to_absl(&mut self, src: u32, dst: u32) {
        self.word(0x3039); // MOVE.W ABS.L,D0
        self.long(src);
        self.word(0x33C0); // MOVE.W D0,ABS.L
        self.long(dst);
    }

    /// `BTST #bit,ABS.L` -- opcode `$0839`.
    fn btst_imm_absl(&mut self, bit: u8, addr: u32) {
        self.word(0x0839);
        self.word(bit as u16);
        self.long(addr);
    }

    /// `BNE.s target` -- branches backward to an already-emitted address.
    fn bne_s_to(&mut self, target: u32) {
        let disp = target as i64 - (self.addr() as i64 + 2);
        assert!((-128..=127).contains(&disp), "BNE.s out of range");
        self.code.push(0x66);
        self.code.push(disp as i8 as u8);
    }

    /// `BRA.s *` -- branches to itself forever; the harness's halt marker
    /// (`run_until{"pc": ...}` catches the CPU here).
    fn bra_s_self(&mut self) {
        self.code.push(0x60);
        self.code.push((-2i8) as u8);
    }

    fn nop(&mut self) {
        self.word(0x4E71);
    }
}

/// `$DFF000`-relative register offset plus the 16-bit value to write, in
/// program order. The single source of truth for a test case's register
/// setup: replayed in order against the host `Blitter::write` (which
/// no-ops on non-blitter offsets like `DMACON`, so it's safe to include
/// that here too) *and* assembled into pokes for Copperline, so the two
/// backends can never see a different register sequence.
type RegWrites = Vec<(u16, u16)>;

const DMACON: u16 = 0x096;
const DMACONR: u16 = 0x002;
const BLTDDAT_ADDR: u32 = 0x00DFF000;
const CUSTOM_BASE: u32 = 0x00DFF000;

/// `DMACON` bits: SET/CLR, master DMA enable, blitter DMA enable. Written
/// once per test case (idempotent, and a no-op on the host `Blitter`)
/// because Copperline's blitter -- unlike ours -- refuses to run without
/// it (see `docs/blitter-differential.md`'s DMACON finding).
const DMACON_ENABLE_BLITTER: u16 = 0x8000 | 0x0200 | 0x0040;

/// Assemble the register pokes, a `BBUSY` spin-wait, a settle margin, a
/// `BLTDDAT` capture into `scratch_addr`, and a halt marker. Returns the
/// program bytes and the address of the halt instruction (the
/// `run_until` target).
fn build_program(base: u32, writes: &RegWrites, scratch_addr: u32) -> (Vec<u8>, u32) {
    let mut asm = Asm::new(base);
    asm.move_w_imm_absl(DMACON_ENABLE_BLITTER, CUSTOM_BASE + DMACON as u32);
    for &(off, val) in writes {
        asm.move_w_imm_absl(val, CUSTOM_BASE + off as u32);
    }
    let loop_addr = asm.addr();
    asm.btst_imm_absl(14, CUSTOM_BASE + DMACONR as u32);
    asm.bne_s_to(loop_addr);
    // Settle margin: see the module doc comment's BBUSY-vs-final-write
    // finding. 32 NOPs is far more than the one or two cycles observed
    // needed, kept generous rather than tuned to the edge.
    for _ in 0..64 {
        asm.nop();
    }
    asm.capture_absl_to_absl(BLTDDAT_ADDR, scratch_addr);
    let halt_addr = asm.addr();
    asm.bra_s_self();
    (asm.code, halt_addr)
}

// ---------------------------------------------------------------------
// CCP (Copperline Control Protocol) client: newline-delimited JSON-RPC
// 2.0 over a loopback TCP socket. See
// ~/src/external/Copperline/docs/debugger/control.md.
// ---------------------------------------------------------------------
struct Ccp {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
    next_id: u64,
    child: Child,
}

impl Ccp {
    /// Spawn a headless Copperline with a fresh `--control` server,
    /// connect, and authenticate. Returns `None` if `copperline` isn't
    /// on `PATH` -- the caller's cue to skip the whole differential like
    /// `real_rom.rs`'s ROM-dependent tests skip when their inputs are
    /// absent.
    fn launch() -> Option<Self> {
        let info_path = std::env::temp_dir().join(format!(
            "machine-hosted-blitter-differential-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&info_path);

        let child = match Command::new("copperline")
            .args([
                "--factory",
                "--model",
                "A1200",
                "--chipset",
                "ECS",
                "--chip",
                "2M",
                "--noaudio",
                "--control",
                ":0",
                "--control-info",
            ])
            .arg(&info_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => panic!("spawn copperline: {e}"),
        };

        let info = wait_for_control_info(&info_path, Duration::from_secs(10));
        let listen = info["listen"].as_str().expect("listen address").to_string();
        let token = info["token"].as_str().expect("token").to_string();

        let stream = TcpStream::connect(&listen)
            .unwrap_or_else(|e| panic!("connect to Copperline control server {listen}: {e}"));
        let reader = BufReader::new(
            stream
                .try_clone()
                .expect("clone control socket for reading"),
        );
        let mut ccp = Ccp {
            writer: stream,
            reader,
            next_id: 0,
            child,
        };
        let hello = ccp.call("hello", json!({ "token": token }));
        assert_eq!(
            hello["authed"], true,
            "Copperline control handshake did not authenticate: {hello:?}"
        );
        let _ = std::fs::remove_file(&info_path);
        Some(ccp)
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let req = json!({
            "id": self.next_id,
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req).expect("serialise CCP request");
        self.writer
            .write_all(line.as_bytes())
            .and_then(|_| self.writer.write_all(b"\n"))
            .unwrap_or_else(|e| panic!("write CCP request ({method}): {e}"));
        let mut resp_line = String::new();
        self.reader
            .read_line(&mut resp_line)
            .unwrap_or_else(|e| panic!("read CCP response ({method}): {e}"));
        let resp: Value = serde_json::from_str(resp_line.trim())
            .unwrap_or_else(|e| panic!("parse CCP response ({method}): {e}: {resp_line:?}"));
        if let Some(err) = resp.get("error") {
            panic!("CCP {method} returned an error: {err}");
        }
        resp["result"].clone()
    }

    fn write_mem(&mut self, addr: u32, data: &[u8]) {
        let result = self.call(
            "mem.write",
            json!({ "addr": addr, "data": hex_encode(data), "encoding": "hex" }),
        );
        assert_eq!(
            result["written"].as_u64(),
            Some(data.len() as u64),
            "mem.write at {addr:#010X} only wrote {:?} of {} bytes -- outside RAM \
             (e.g. inside the boot ROM overlay)?",
            result["written"],
            data.len()
        );
    }

    fn read_mem(&mut self, addr: u32, len: usize) -> Vec<u8> {
        let result = self.call(
            "mem.read",
            json!({ "addr": addr, "len": len, "encoding": "hex" }),
        );
        hex_decode(result["data"].as_str().expect("mem.read data"))
    }

    fn regs_set(&mut self, reg: &str, value: u32) {
        self.call("regs.set", json!({ "reg": reg, "value": value }));
    }

    /// Set PC/SR to the program's entry point and run to `halt_addr`,
    /// which is only ever reached once the `BBUSY` spin-wait (baked into
    /// every program `build_program` assembles) has actually observed
    /// the blit finish.
    fn run_program(&mut self, entry: u32, halt_addr: u32) {
        self.regs_set("sr", 0x2700); // supervisor, interrupts masked -- matches real reset state
        self.regs_set("pc", entry);
        let stop = self.call("run_until", json!({ "pc": halt_addr, "wait_ms": 15_000 }));
        assert_eq!(
            stop["reason"], "target",
            "Copperline did not reach the halt marker at {halt_addr:#010X} \
             within the time budget -- stop event was {stop:?}"
        );
    }

    fn custom_dump(&mut self) -> Value {
        self.call("custom.dump", json!({}))["regs"].clone()
    }
}

impl Drop for Ccp {
    fn drop(&mut self) {
        let _ = self.call("shutdown", json!({}));
        let _ = self.child.wait();
    }
}

fn wait_for_control_info(path: &std::path::Path, timeout: Duration) -> Value {
    let start = Instant::now();
    loop {
        if let Ok(mut f) = std::fs::File::open(path) {
            let mut s = String::new();
            if f.read_to_string(&mut s).is_ok() {
                if let Ok(v) = serde_json::from_str::<Value>(&s) {
                    return v;
                }
            }
        }
        if start.elapsed() > timeout {
            panic!("Copperline never wrote its --control-info file at {path:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

/// Diagnostic-only: describe the first differing byte between two equal-
/// length buffers, for divergence messages.
fn first_diff(a: &[u8], b: &[u8]) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        None if a.len() == b.len() => "none (lengths equal, no byte differs)".to_string(),
        None => format!(
            "none in common prefix, but lengths differ ({} vs {})",
            a.len(),
            b.len()
        ),
        Some(i) => {
            let lo = i.saturating_sub(4);
            let hi_a = (i + 4).min(a.len());
            let hi_b = (i + 4).min(b.len());
            format!(
                "byte {i}: host {:02X?} vs copperline {:02X?} (window ±4 bytes)",
                &a[lo..hi_a],
                &b[lo..hi_b]
            )
        }
    }
}

fn ptr_from_dump(dump: &Value, hi: &str, lo: &str) -> u32 {
    let h = dump[hi].as_u64().unwrap_or_else(|| panic!("missing {hi}")) as u32;
    let l = dump[lo].as_u64().unwrap_or_else(|| panic!("missing {lo}")) as u32;
    (h << 16) | l
}

// ---------------------------------------------------------------------
// Chip RAM arena: a simple bump allocator over a fixed address range,
// comfortably above the reset-time boot ROM overlay (see module docs).
// Every test case allocates code + data windows sized exactly to its own
// footprint, so nothing is ever reused or shared between cases and there
// is no cross-test state to reason about beyond the blitter's own
// hardware registers (which `build_program` always rewrites in full).
// ---------------------------------------------------------------------
struct Arena {
    next: u32,
    ceiling: u32,
}

impl Arena {
    fn new(base: u32, ceiling: u32) -> Self {
        Self {
            next: base,
            ceiling,
        }
    }

    fn alloc(&mut self, size: u32) -> u32 {
        let addr = self.next;
        self.next = (self.next + size + 1) & !1;
        assert!(
            self.next <= self.ceiling,
            "blitter differential arena exhausted ({:#X} > {:#X}) -- \
             fewer/smaller test cases, or widen the arena",
            self.next,
            self.ceiling
        );
        addr
    }
}

/// Just above the boot-ROM overlay at reset (see module docs). Empirically
/// re-checked while widening this arena for `workbench_corpus_cases`:
/// `mem.write` at `0x00100000` itself already succeeds (`written` == the
/// full request), so the overlay is exactly the first 1 MiB, not "at
/// least" as originally measured less precisely -- `0x0100_0200` keeps a
/// 512-byte margin below that edge rather than sitting exactly on it.
/// This reclaimed the headroom `workbench_corpus_cases`' extra footprint
/// needed; `ARENA_CEILING` stays comfortably below the true `--chip 2M`
/// end (`0x0020_0000`), confirmed a hard limit (ECS's chipset maximum --
/// `copperline` itself refuses `--chip` above 2 MiB for `--chipset ECS`).
const ARENA_BASE: u32 = 0x0010_0200;
const ARENA_CEILING: u32 = 0x001F_FF00;

// ---------------------------------------------------------------------
// Area-mode (non-line) test cases.
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
struct AreaCase {
    name: String,
    bltcon0: u16,
    bltcon1: u16,
    bltafwm: u16,
    bltalwm: u16,
    amod: i16,
    bmod: i16,
    cmod: i16,
    dmod: i16,
    adat: u16,
    bdat: u16,
    cdat: u16,
    width_words: u16,
    height_rows: u16,
    /// When true, channel C and D point at the *same* address (the
    /// read-modify-write idiom graphics.library uses, e.g. `D = A | C`
    /// to draw onto existing content) instead of independent windows.
    c_equals_d: bool,
}

impl AreaCase {
    /// Conservative per-channel byte span: covers the largest address
    /// offset from the start pointer the blit can reach in either
    /// direction, given this case's width/height/modulo.
    /// `BLTSIZE`'s width field, decoded the way the hardware (and
    /// `Blitter::write`) decodes it: 0 means 64. Window sizing below
    /// must use this, not the raw stored field -- using the raw field
    /// undersizes the window for the `bltsize_zero_field_cases` (window
    /// sized for width 0 while the blit actually runs 64 words wide),
    /// which lets the blit's D-channel writes run past the window into
    /// the arena's *next* allocation -- this test's own running program,
    /// corrupting it mid-execution and hanging Copperline in a runaway
    /// loop. Caught empirically while first running this file: see
    /// docs/blitter-differential.md.
    fn effective_width_words(&self) -> u32 {
        if self.width_words == 0 {
            64
        } else {
            self.width_words as u32
        }
    }

    fn effective_height_rows(&self) -> u32 {
        if self.height_rows == 0 {
            1024
        } else {
            self.height_rows as u32
        }
    }

    fn span(&self) -> u32 {
        let w = self.effective_width_words() * 2;
        let row = w + self.amod.unsigned_abs().max(self.dmod.unsigned_abs()) as u32;
        (self.effective_height_rows() + 1) * row.max(w) + 64
    }
}

/// A single scratch-arena allocation used as a channel window: `base` is
/// its start address, `start` is where this test's pointer register is
/// set, chosen with equal slack either side so ascending and descending
/// traversal both stay inside `[base, base+size)`.
struct ChanWindow {
    base: u32,
    start: u32,
    size: u32,
}

fn alloc_window(arena: &mut Arena, span: u32) -> ChanWindow {
    let size = span * 2;
    let base = arena.alloc(size);
    ChanWindow {
        base,
        start: base + span,
        size,
    }
}

fn area_writes(c: &AreaCase, a_pt: u32, b_pt: u32, c_pt: u32, d_pt: u32) -> RegWrites {
    vec![
        (reg::BLTCON0, c.bltcon0),
        (reg::BLTCON1, c.bltcon1),
        (reg::BLTAFWM, c.bltafwm),
        (reg::BLTALWM, c.bltalwm),
        (reg::BLTAPTH, (a_pt >> 16) as u16),
        (reg::BLTAPTL, a_pt as u16),
        (reg::BLTBPTH, (b_pt >> 16) as u16),
        (reg::BLTBPTL, b_pt as u16),
        (reg::BLTCPTH, (c_pt >> 16) as u16),
        (reg::BLTCPTL, c_pt as u16),
        (reg::BLTDPTH, (d_pt >> 16) as u16),
        (reg::BLTDPTL, d_pt as u16),
        (reg::BLTAMOD, c.amod as u16),
        (reg::BLTBMOD, c.bmod as u16),
        (reg::BLTCMOD, c.cmod as u16),
        (reg::BLTDMOD, c.dmod as u16),
        (reg::BLTADAT, c.adat),
        (reg::BLTBDAT, c.bdat),
        (reg::BLTCDAT, c.cdat),
        (
            reg::BLTSIZE,
            ((c.height_rows & 0x03FF) << 6) | (c.width_words & 0x003F),
        ),
    ]
}

/// Outcome of running one case, shaped identically whichever backend
/// produced it so the two are directly comparable.
#[derive(Debug)]
struct Outcome {
    d_window: Vec<u8>,
    zero: bool,
    bltddat: u16,
    pt: [u32; 4],
}

impl Outcome {
    /// Equality used for the actual pass/fail comparison. **Excludes
    /// `bltddat`, captured and reported for diagnostics only.** Found
    /// empirically while building this harness (see
    /// `docs/blitter-differential.md`'s "Copperline oracle limitation"
    /// section): a CPU-driven read of `$DFF000` (`BLTDDAT`) in this
    /// Copperline build does not return the blitter's last-processed
    /// word. It returns `DMACONR`'s live value regardless of which
    /// register address is read -- confirmed by reading the
    /// known-write-only `BLTCON0` the same way and getting the identical
    /// `DMACONR`-shaped value back, ruling out an "echoes the last bus
    /// value" explanation too. That means every `BLTDDAT` comparison
    /// this file could make would fail for a reason that has nothing to
    /// do with `machine-core`'s blitter -- Copperline itself doesn't
    /// expose a working readback path for this register via CCP
    /// (`custom.read` explicitly refuses it: "$000 is not readable") or,
    /// evidently, via the CPU bus. Comparing it would be noise, not
    /// signal, so this relaxes exactly that one field and states why.
    fn matches(&self, other: &Outcome) -> bool {
        self.d_window == other.d_window && self.zero == other.zero && self.pt == other.pt
    }
}

/// A flat, heap-allocated stand-in for chip RAM, boxed like
/// `lib.rs`'s own `boxed_chip_ram` to avoid a 2 MB stack frame.
struct Ram(Box<[u8; CHIP_RAM_SIZE]>);

impl Ram {
    fn zeroed() -> Self {
        let v: Vec<u8> = vec![0u8; CHIP_RAM_SIZE];
        let boxed: Box<[u8]> = v.into_boxed_slice();
        Ram(boxed.try_into().unwrap_or_else(|_| unreachable!()))
    }

    fn clone_boxed(&self) -> Box<[u8; CHIP_RAM_SIZE]> {
        let v: Vec<u8> = self.0.to_vec();
        let boxed: Box<[u8]> = v.into_boxed_slice();
        boxed.try_into().unwrap_or_else(|_| unreachable!())
    }
}

/// Run one area-mode case on both backends and report any divergence.
/// Returns `Some(description)` on mismatch (never panics itself, so the
/// caller can collect every divergence across a whole sweep before
/// failing once).
fn run_area_case(ccp: &mut Ccp, arena: &mut Arena, rng: &mut Rng, c: &AreaCase) -> Option<String> {
    let span = c.span();
    // Only allocate a channel window for a channel the blit actually
    // dereferences: a disabled channel reads its constant `*DAT`
    // register instead of chip RAM (see `execute_area`'s doc comment),
    // so its pointer is never followed and can safely stay 0. This
    // matters for more than tidiness: the `bltsize_zero_field_cases`
    // (width/height decoded as 64/1024) only use A and D, and `span()`
    // scales with the *decoded* size -- allocating full-size, wasted
    // B/C windows for those blew the arena budget (see `Arena::alloc`'s
    // panic) before this was added.
    let use_a = c.bltcon0 & blitter::BLTCON0_USEA != 0;
    let use_b = c.bltcon0 & blitter::BLTCON0_USEB != 0;
    let use_c = c.bltcon0 & blitter::BLTCON0_USEC != 0;
    let a_win = use_a.then(|| alloc_window(arena, span));
    let b_win = use_b.then(|| alloc_window(arena, span));
    let d_win = alloc_window(arena, span); // always compared, so always real
    let c_win = if use_c && !c.c_equals_d {
        Some(alloc_window(arena, span))
    } else {
        None // either unused, or reuses d_win's address below
    };
    let a_pt = a_win.as_ref().map_or(0, |w| w.start);
    let b_pt = b_win.as_ref().map_or(0, |w| w.start);
    let c_pt = if use_c && c.c_equals_d {
        d_win.start
    } else {
        c_win.as_ref().map_or(0, |w| w.start)
    };

    // Random seed data across every allocated window (including a D
    // window on a case that never writes it) -- cheap, and it means an
    // off-by-one in the footprint math shows up as a mismatch against
    // ram nobody meant to touch.
    let mut seed_bytes = |ram: &mut [u8], base: u32, size: u32| {
        for i in 0..size {
            ram[(base + i) as usize] = rng.u16() as u8;
        }
    };
    let mut host_ram = Ram::zeroed();
    if let Some(w) = &a_win {
        seed_bytes(&mut host_ram.0[..], w.base, w.size);
    }
    if let Some(w) = &b_win {
        seed_bytes(&mut host_ram.0[..], w.base, w.size);
    }
    if let Some(w) = &c_win {
        seed_bytes(&mut host_ram.0[..], w.base, w.size);
    }
    seed_bytes(&mut host_ram.0[..], d_win.base, d_win.size);

    let writes = area_writes(c, a_pt, b_pt, c_pt, d_win.start);

    // ---- host ----
    let mut ram = host_ram.clone_boxed();
    let mut hb = Blitter::new();
    for &(off, val) in &writes {
        hb.write(off, val);
    }
    assert!(hb.execute(&mut ram), "{}: host blit did not run", c.name);
    let host = Outcome {
        d_window: ram[d_win.base as usize..(d_win.base + d_win.size) as usize].to_vec(),
        zero: hb.zero,
        bltddat: hb.bltddat,
        pt: hb.pt,
    };

    // ---- Copperline ----
    let code_addr = arena.alloc(512);
    let scratch = arena.alloc(2);
    let (prog, halt_addr) = build_program(code_addr, &writes, scratch);
    ccp.write_mem(code_addr, &prog);
    if let Some(w) = &a_win {
        ccp.write_mem(
            w.base,
            &host_ram.0[w.base as usize..(w.base + w.size) as usize],
        );
    }
    if let Some(w) = &b_win {
        ccp.write_mem(
            w.base,
            &host_ram.0[w.base as usize..(w.base + w.size) as usize],
        );
    }
    if let Some(w) = &c_win {
        ccp.write_mem(
            w.base,
            &host_ram.0[w.base as usize..(w.base + w.size) as usize],
        );
    }
    ccp.write_mem(
        d_win.base,
        &host_ram.0[d_win.base as usize..(d_win.base + d_win.size) as usize],
    );
    ccp.run_program(code_addr, halt_addr);

    let cop_d = ccp.read_mem(d_win.base, d_win.size as usize);
    let bltddat_bytes = ccp.read_mem(scratch, 2);
    let bltddat = u16::from_be_bytes([bltddat_bytes[0], bltddat_bytes[1]]);
    let dump = ccp.custom_dump();
    let zero = (dump["DMACONR"].as_u64().unwrap_or(0) & (blitter::DMACONR_BZERO as u64)) != 0;
    let cop = Outcome {
        d_window: cop_d,
        zero,
        bltddat,
        pt: [
            ptr_from_dump(&dump, "BLTAPTH", "BLTAPTL"),
            ptr_from_dump(&dump, "BLTBPTH", "BLTBPTL"),
            ptr_from_dump(&dump, "BLTCPTH", "BLTCPTL"),
            ptr_from_dump(&dump, "BLTDPTH", "BLTDPTL"),
        ],
    };

    if !host.matches(&cop) {
        Some(format!(
            "AREA CASE DIVERGED: {} (seed={SEED:#X})\n  \
             bltcon0={:#06X} bltcon1={:#06X} afwm={:#06X} alwm={:#06X}\n  \
             width_words={} height_rows={} amod={} bmod={} cmod={} dmod={}\n  \
             adat={:#06X} bdat={:#06X} cdat={:#06X}\n  \
             a_pt={:#010X} b_pt={:#010X} c_pt={:#010X} d_pt={:#010X} (c_equals_d={})\n  \
             host:       zero={} bltddat={:#06X} pt={:X?}\n  \
             copperline: zero={} bltddat={:#06X} pt={:X?}\n  \
             d_window matches: {} ({})",
            c.name,
            c.bltcon0,
            c.bltcon1,
            c.bltafwm,
            c.bltalwm,
            c.width_words,
            c.height_rows,
            c.amod,
            c.bmod,
            c.cmod,
            c.dmod,
            c.adat,
            c.bdat,
            c.cdat,
            a_pt,
            b_pt,
            c_pt,
            d_win.start,
            c.c_equals_d,
            host.zero,
            host.bltddat,
            host.pt,
            cop.zero,
            cop.bltddat,
            cop.pt,
            host.d_window == cop.d_window,
            first_diff(&host.d_window, &cop.d_window),
        ))
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// Line-mode test cases. Geometry math ported directly from
// `crates/machine-core/src/blitter.rs`'s own
// `draw_line_get_pixels`/`independent_bresenham` test helpers (same
// Bresenham-to-BLTCON translation, cross-checked there against an
// independent integer Bresenham -- this file cross-checks the same
// translation against real cycle-exact hardware instead).
//
// Per the task brief: only the `BLTCPT == BLTDPT` case is exercised
// (the universal graphics.library convention, and the case our
// documented C/D micro-cycle-lag simplification is exact for); the
// unequal-pointer case is deliberately out of scope (see
// docs/blitter-differential.md).
// ---------------------------------------------------------------------

#[derive(Clone, Debug)]
struct LineCase {
    name: String,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    sing: bool,
    use_b_texture: bool,
    bdat: u16,
}

struct LineGeometry {
    bltcon0: u16,
    bltcon1: u16,
    amod: i16,
    bmod: i16,
    npixels: u16,
    start_addr: u32,
    err0: i16,
}

const LINE_BPLMOD: i32 = 2; // one word (16px) per row, matching the unit test

fn line_geometry(c: &LineCase) -> LineGeometry {
    let dx = c.x1 - c.x0;
    let dy = c.y1 - c.y0;
    let adx = dx.unsigned_abs() as i32;
    let ady = dy.unsigned_abs() as i32;
    let (sud, err0, amod, bmod, npixels, sul, aul) = if adx >= ady {
        (
            true,
            2 * ady - adx,
            2 * (ady - adx),
            2 * ady,
            adx + 1,
            dy < 0,
            dx < 0,
        )
    } else {
        (
            false,
            2 * adx - ady,
            2 * (adx - ady),
            2 * adx,
            ady + 1,
            dx < 0,
            dy < 0,
        )
    };

    let mut bltcon0 =
        blitter::BLTCON0_USEA | blitter::BLTCON0_USEC | blitter::BLTCON0_USED | 0x00FA;
    if c.use_b_texture {
        bltcon0 |= blitter::BLTCON0_USEB;
    }
    bltcon0 |= (c.x0 as u16 & 0x000F) << 12;

    let mut bltcon1 = blitter::BLTCON1_LINE;
    if sud {
        bltcon1 |= blitter::BLTCON1_SUD;
    }
    if sul {
        bltcon1 |= blitter::BLTCON1_SUL;
    }
    if aul {
        bltcon1 |= blitter::BLTCON1_AUL;
    }
    if err0 < 0 {
        bltcon1 |= blitter::BLTCON1_SIGN;
    }
    if c.sing {
        bltcon1 |= blitter::BLTCON1_SING;
    }

    LineGeometry {
        bltcon0,
        bltcon1,
        amod: amod as i16,
        bmod: bmod as i16,
        npixels: npixels as u16,
        start_addr: (c.y0 as u32) * (LINE_BPLMOD as u32),
        err0: err0 as i16,
    }
}

fn line_writes(c: &LineCase, g: &LineGeometry, cd_pt: u32) -> RegWrites {
    vec![
        (reg::BLTCON0, g.bltcon0),
        (reg::BLTCON1, g.bltcon1),
        (reg::BLTAFWM, 0xFFFF),
        (reg::BLTALWM, 0xFFFF),
        (reg::BLTAPTH, 0),
        (reg::BLTAPTL, g.err0 as u16),
        (reg::BLTBPTH, 0),
        (reg::BLTBPTL, 0),
        (reg::BLTCPTH, (cd_pt >> 16) as u16),
        (reg::BLTCPTL, cd_pt as u16),
        (reg::BLTDPTH, (cd_pt >> 16) as u16),
        (reg::BLTDPTL, cd_pt as u16),
        (reg::BLTAMOD, g.amod as u16),
        (reg::BLTBMOD, g.bmod as u16),
        (reg::BLTCMOD, LINE_BPLMOD as u16),
        (reg::BLTDMOD, 0),
        (reg::BLTADAT, 0x8000),
        (reg::BLTBDAT, c.bdat),
        (reg::BLTCDAT, 0),
        (
            reg::BLTSIZE,
            ((g.npixels & 0x03FF) << 6) | 2, // width field = 2, by convention
        ),
    ]
}

/// A 16x16-pixel single-bitplane canvas, matching `draw_line_get_pixels`
/// exactly: 16 rows of one word each. Sized to comfortably hold every
/// octant this file's `LineCase`s draw within.
const LINE_CANVAS_ROWS: u32 = 16;
const LINE_CANVAS_BYTES: u32 = LINE_CANVAS_ROWS * LINE_BPLMOD as u32;

fn run_line_case(ccp: &mut Ccp, arena: &mut Arena, c: &LineCase) -> Option<String> {
    let g = line_geometry(c);

    let canvas_base = arena.alloc(LINE_CANVAS_BYTES);
    let cd_pt = canvas_base + g.start_addr;
    // start_addr is y0 * BPLMOD, i.e. relative to canvas_base already,
    // and cd_pt above double-adds it -- correct: canvas_base is address
    // 0 of the 16-row canvas, cd_pt is canvas_base + that row's offset.

    let writes = line_writes(c, &g, cd_pt);

    // ---- host ----
    let mut ram = Ram::zeroed().clone_boxed();
    let mut hb = Blitter::new();
    for &(off, val) in &writes {
        hb.write(off, val);
    }
    assert!(
        hb.execute(&mut ram),
        "{}: host line blit did not run",
        c.name
    );
    let host_canvas =
        ram[canvas_base as usize..(canvas_base + LINE_CANVAS_BYTES) as usize].to_vec();
    let host = Outcome {
        d_window: host_canvas,
        zero: hb.zero,
        bltddat: hb.bltddat,
        pt: hb.pt,
    };

    // ---- Copperline ----
    let code_addr = arena.alloc(512);
    let scratch = arena.alloc(2);
    let (prog, halt_addr) = build_program(code_addr, &writes, scratch);
    ccp.write_mem(code_addr, &prog);
    // Canvas starts zeroed like the host's fresh Ram -- no seed needed.
    ccp.write_mem(canvas_base, &vec![0u8; LINE_CANVAS_BYTES as usize]);
    ccp.run_program(code_addr, halt_addr);

    let cop_canvas = ccp.read_mem(canvas_base, LINE_CANVAS_BYTES as usize);
    let bltddat_bytes = ccp.read_mem(scratch, 2);
    let bltddat = u16::from_be_bytes([bltddat_bytes[0], bltddat_bytes[1]]);
    let dump = ccp.custom_dump();
    let zero = (dump["DMACONR"].as_u64().unwrap_or(0) & (blitter::DMACONR_BZERO as u64)) != 0;
    let cop = Outcome {
        d_window: cop_canvas,
        zero,
        bltddat,
        pt: [
            ptr_from_dump(&dump, "BLTAPTH", "BLTAPTL"),
            ptr_from_dump(&dump, "BLTBPTH", "BLTBPTL"),
            ptr_from_dump(&dump, "BLTCPTH", "BLTCPTL"),
            ptr_from_dump(&dump, "BLTDPTH", "BLTDPTL"),
        ],
    };

    if !host.matches(&cop) {
        Some(format!(
            "LINE CASE DIVERGED: {} (seed={SEED:#X})\n  \
             ({},{})-({},{}) sing={} use_b_texture={} bdat={:#06X}\n  \
             bltcon0={:#06X} bltcon1={:#06X} err0={} amod={} bmod={} npixels={}\n  \
             host:       zero={} bltddat={:#06X} pt={:X?}\n  \
             copperline: zero={} bltddat={:#06X} pt={:X?}\n  \
             canvas matches: {} ({})",
            c.name,
            c.x0,
            c.y0,
            c.x1,
            c.y1,
            c.sing,
            c.use_b_texture,
            c.bdat,
            g.bltcon0,
            g.bltcon1,
            g.err0,
            g.amod,
            g.bmod,
            g.npixels,
            host.zero,
            host.bltddat,
            host.pt,
            cop.zero,
            cop.bltddat,
            cop.pt,
            host.d_window == cop.d_window,
            first_diff(&host.d_window, &cop.d_window),
        ))
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// Case generators.
// ---------------------------------------------------------------------

/// All 256 minterms, small fixed geometry, all four channels enabled
/// (mirrors `blitter.rs`'s own `minterms_match_independent_truth_table`
/// unit test, but end to end through real hardware rather than the
/// `minterm()` function in isolation).
fn minterm_sweep_cases() -> Vec<AreaCase> {
    (0u16..=255)
        .map(|lf| AreaCase {
            name: format!("minterm lf={lf:#04X}"),
            bltcon0: blitter::BLTCON0_USEA
                | blitter::BLTCON0_USEB
                | blitter::BLTCON0_USEC
                | blitter::BLTCON0_USED
                | lf,
            bltcon1: 0,
            bltafwm: 0xFFFF,
            bltalwm: 0xFFFF,
            amod: 0,
            bmod: 0,
            cmod: 0,
            dmod: 0,
            adat: 0,
            bdat: 0,
            cdat: 0,
            width_words: 2,
            height_rows: 2,
            c_equals_d: false,
        })
        .collect()
}

/// All 16 `USEA`/`USEB`/`USEC`/`USED` combinations, ascending and
/// descending, with a minterm (`D = A^B^C`-ish, `0x96`) that actually
/// depends on all three inputs so a wrong channel-enable no-op would
/// show up in the output.
fn channel_enable_cases() -> Vec<AreaCase> {
    let mut out = Vec::new();
    for bits in 0u16..16 {
        for &desc in &[false, true] {
            let bltcon0 = (bits << 8) | 0x4D; // arbitrary minterm sensitive to all inputs
            out.push(AreaCase {
                name: format!("channel enables {bits:04b} desc={desc}"),
                bltcon0,
                bltcon1: if desc { blitter::BLTCON1_DESC } else { 0 },
                bltafwm: 0xFFFF,
                bltalwm: 0xFFFF,
                amod: 0,
                bmod: 0,
                cmod: 0,
                dmod: 0,
                adat: 0x5A5A,
                bdat: 0x3C3C,
                cdat: 0x0F0F,
                width_words: 3,
                height_rows: 2,
                c_equals_d: false,
            });
        }
    }
    out
}

/// A and B barrel shifts across the full 0..15 range, ascending and
/// descending, on a multi-word row so a shift's cross-word carry
/// actually exercises the previous word's carried bits.
fn shift_cases(rng: &mut Rng) -> Vec<AreaCase> {
    let mut out = Vec::new();
    for ash in 0u16..16 {
        for &desc in &[false, true] {
            out.push(AreaCase {
                name: format!("A shift={ash} desc={desc}"),
                bltcon0: blitter::BLTCON0_USEA | blitter::BLTCON0_USED | 0xF0 | (ash << 12),
                bltcon1: if desc { blitter::BLTCON1_DESC } else { 0 },
                bltafwm: 0xFFFF,
                bltalwm: 0xFFFF,
                amod: 0,
                bmod: 0,
                cmod: 0,
                dmod: 0,
                adat: 0,
                bdat: 0,
                cdat: 0,
                width_words: 4,
                height_rows: 1 + (rng.range(0, 2) as u16),
                c_equals_d: false,
            });
        }
    }
    for bsh in 0u16..16 {
        for &desc in &[false, true] {
            out.push(AreaCase {
                name: format!("B shift={bsh} desc={desc}"),
                bltcon0: blitter::BLTCON0_USEB | blitter::BLTCON0_USED | 0xCC | (bsh << 12),
                bltcon1: (if desc { blitter::BLTCON1_DESC } else { 0 }) | (bsh << 12),
                bltafwm: 0xFFFF,
                bltalwm: 0xFFFF,
                amod: 0,
                bmod: 0,
                cmod: 0,
                dmod: 0,
                adat: 0,
                bdat: 0,
                cdat: 0,
                width_words: 4,
                height_rows: 1,
                c_equals_d: false,
            });
        }
    }
    out
}

/// First/last-word masks, including the one-word-row case where both
/// masks apply to the same word (already unit-tested at the register
/// level; this confirms real hardware agrees).
fn mask_cases() -> Vec<AreaCase> {
    vec![
        AreaCase {
            name: "AFWM/ALWM multi-word".into(),
            bltcon0: blitter::BLTCON0_USEA | blitter::BLTCON0_USED | 0xF0,
            bltcon1: 0,
            bltafwm: 0xFF00,
            bltalwm: 0x00FF,
            amod: 0,
            bmod: 0,
            cmod: 0,
            dmod: 0,
            adat: 0,
            bdat: 0,
            cdat: 0,
            width_words: 3,
            height_rows: 1,
            c_equals_d: false,
        },
        AreaCase {
            name: "AFWM/ALWM one-word row".into(),
            bltcon0: blitter::BLTCON0_USEA | blitter::BLTCON0_USED | 0xF0,
            bltcon1: 0,
            bltafwm: 0xFF00,
            bltalwm: 0x00FF,
            amod: 0,
            bmod: 0,
            cmod: 0,
            dmod: 0,
            adat: 0,
            bdat: 0,
            cdat: 0,
            width_words: 1,
            height_rows: 1,
            c_equals_d: false,
        },
    ]
}

/// Inclusive/exclusive fill, with and without carry-in -- fill is only
/// well-defined descending (see `blitter.rs::execute_area`'s doc
/// comment), so every case here sets `DESC`.
fn fill_cases() -> Vec<AreaCase> {
    let mut out = Vec::new();
    for &ife in &[false, true] {
        for &fci in &[false, true] {
            let efe = !ife; // one or the other, never both, matching real usage
            let mut bltcon1 = blitter::BLTCON1_DESC;
            if ife {
                bltcon1 |= blitter::BLTCON1_IFE;
            } else if efe {
                bltcon1 |= blitter::BLTCON1_EFE;
            }
            if fci {
                bltcon1 |= blitter::BLTCON1_FCI;
            }
            out.push(AreaCase {
                name: format!("fill ife={ife} efe={efe} fci={fci}"),
                bltcon0: blitter::BLTCON0_USEA | blitter::BLTCON0_USED | 0xF0,
                bltcon1,
                bltafwm: 0xFFFF,
                bltalwm: 0xFFFF,
                amod: 0,
                bmod: 0,
                cmod: 0,
                dmod: 0,
                adat: 0,
                bdat: 0,
                cdat: 0,
                width_words: 3,
                height_rows: 2,
                c_equals_d: false,
            });
        }
    }
    out
}

/// Signed modulos (including negative), a range of widths/heights, and
/// one explicit read-modify-write case (`C == D`) per graphics.library's
/// own idiom.
fn modulo_and_size_cases(rng: &mut Rng) -> Vec<AreaCase> {
    let mut out = Vec::new();
    for i in 0..14 {
        let desc = rng.bool();
        let width_words = rng.range(1, 6) as u16;
        let height_rows = rng.range(1, 6) as u16;
        let amod = rng.range(-16, 16) as i16;
        let dmod = rng.range(-16, 16) as i16;
        out.push(AreaCase {
            name: format!("modulo/size #{i} desc={desc} w={width_words} h={height_rows}"),
            bltcon0: blitter::BLTCON0_USEA | blitter::BLTCON0_USED | 0xF0,
            bltcon1: if desc { blitter::BLTCON1_DESC } else { 0 },
            bltafwm: 0xFFFF,
            bltalwm: 0xFFFF,
            amod,
            bmod: 0,
            cmod: 0,
            dmod,
            adat: 0,
            bdat: 0,
            cdat: 0,
            width_words,
            height_rows,
            c_equals_d: false,
        });
    }
    out.push(AreaCase {
        name: "read-modify-write: D = A | C, C==D".into(),
        bltcon0: blitter::BLTCON0_USEA | blitter::BLTCON0_USEC | blitter::BLTCON0_USED | 0xFA,
        bltcon1: 0,
        bltafwm: 0xFFFF,
        bltalwm: 0xFFFF,
        amod: 0,
        bmod: 0,
        cmod: 0,
        dmod: 0,
        adat: 0,
        bdat: 0,
        cdat: 0,
        width_words: 3,
        height_rows: 3,
        c_equals_d: true,
    });
    out
}

/// `BLTSIZE`'s zero-field-means-maximum quirk: width alone maxed
/// (64 words), height alone maxed (1024 rows), and both maxed at once
/// (the full 1024x64 blit).
fn bltsize_zero_field_cases() -> Vec<AreaCase> {
    vec![
        AreaCase {
            name: "BLTSIZE width field 0 -> 64 words".into(),
            bltcon0: blitter::BLTCON0_USEA | blitter::BLTCON0_USED | 0xF0,
            bltcon1: 0,
            bltafwm: 0xFFFF,
            bltalwm: 0xFFFF,
            amod: 0,
            bmod: 0,
            cmod: 0,
            dmod: 0,
            adat: 0,
            bdat: 0,
            cdat: 0,
            width_words: 0, // encodes as 64
            height_rows: 2,
            c_equals_d: false,
        },
        AreaCase {
            name: "BLTSIZE height field 0 -> 1024 rows".into(),
            bltcon0: blitter::BLTCON0_USEA | blitter::BLTCON0_USED | 0xF0,
            bltcon1: 0,
            bltafwm: 0xFFFF,
            bltalwm: 0xFFFF,
            amod: 0,
            bmod: 0,
            cmod: 0,
            dmod: 0,
            adat: 0,
            bdat: 0,
            cdat: 0,
            width_words: 3,
            height_rows: 0, // encodes as 1024
            c_equals_d: false,
        },
        AreaCase {
            name: "BLTSIZE both fields 0 -> 1024x64 (full max)".into(),
            bltcon0: blitter::BLTCON0_USEA | blitter::BLTCON0_USED | 0xF0,
            bltcon1: 0,
            bltafwm: 0xFFFF,
            bltalwm: 0xFFFF,
            amod: 0,
            bmod: 0,
            cmod: 0,
            dmod: 0,
            adat: 0,
            bdat: 0,
            cdat: 0,
            width_words: 0,
            height_rows: 0,
            c_equals_d: false,
        },
    ]
}

/// Lines across every octant (all eight sign/axis-major combinations),
/// with and without `SING`, with and without the B-channel texture --
/// same endpoints `blitter.rs`'s own
/// `line_mode_matches_independent_bresenham_across_octants` test uses,
/// plus a few more to broaden coverage.
fn line_cases() -> Vec<LineCase> {
    let endpoints = [
        (0, 0, 7, 3),
        (7, 3, 0, 0),
        (0, 0, 3, 7),
        (3, 7, 0, 0),
        (2, 2, 9, 9),
        (9, 9, 2, 2),
        (1, 8, 8, 1),
        (8, 1, 1, 8),
        (0, 5, 10, 5),
        (5, 0, 5, 10),
        (0, 0, 12, 1),
        (0, 0, 1, 12),
    ];
    let mut out = Vec::new();
    for &(x0, y0, x1, y1) in &endpoints {
        for &sing in &[false, true] {
            for &use_b in &[false, true] {
                out.push(LineCase {
                    name: format!("line ({x0},{y0})-({x1},{y1}) sing={sing} use_b={use_b}"),
                    x0,
                    y0,
                    x1,
                    y1,
                    sing,
                    use_b_texture: use_b,
                    bdat: 0xAAAA, // dashed pattern, exercises the per-pixel rotate
                });
            }
        }
    }
    out
}

/// The recorded-Workbench-traces half of proposal §12
/// (`docs/blitter-differential.md`'s "Recorded Workbench traces" section):
/// distinct blitter register signatures captured booting a real planar
/// Workbench desktop with `--blitter-trace` (`crate::blitter_trace` in
/// `machine-hosted`'s own binary), deduplicated at capture time and
/// checked in as a fixture so this test needs neither the ROM nor the HDF
/// to run -- matching every other real-input-dependent test's degrade
/// story here (`tests/real_rom.rs`'s `fixture()`, this file's own
/// `Ccp::launch` skip).
///
/// Replayed through exactly the same `AreaCase`/`run_area_case`/
/// `build_program` machinery as the randomised cases, not a second
/// mechanism: a recorded signature already has the same shape as
/// `AreaCase` minus a name and absolute pointers (which this differential
/// always remaps into its own scratch arena anyway, randomised or
/// recorded). Line-mode arms were recorded and skipped at capture time,
/// not here -- `blitter_trace.rs`'s module doc comment explains why
/// replaying one through `AreaCase` would be actively wrong, not just
/// out of scope.
const WORKBENCH_CORPUS: &str = include_str!("fixtures/blitter_workbench_corpus.txt");

fn workbench_corpus_cases() -> Vec<AreaCase> {
    WORKBENCH_CORPUS
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .enumerate()
        .map(|(i, line)| {
            let fields: Vec<i64> = line
                .split_whitespace()
                .map(|f| {
                    f.parse::<i64>()
                        .unwrap_or_else(|e| panic!("corpus line {i}: bad field {f:?}: {e}"))
                })
                .collect();
            assert_eq!(
                fields.len(),
                14,
                "corpus line {i}: expected 14 fields, got {} ({line:?})",
                fields.len()
            );
            AreaCase {
                name: format!(
                    "workbench trace #{i}: bltcon0={:#06X} bltcon1={:#06X} w={} h={}",
                    fields[0], fields[1], fields[11], fields[12]
                ),
                bltcon0: fields[0] as u16,
                bltcon1: fields[1] as u16,
                bltafwm: fields[2] as u16,
                bltalwm: fields[3] as u16,
                amod: fields[4] as i16,
                bmod: fields[5] as i16,
                cmod: fields[6] as i16,
                dmod: fields[7] as i16,
                adat: fields[8] as u16,
                bdat: fields[9] as u16,
                cdat: fields[10] as u16,
                width_words: fields[11] as u16,
                height_rows: fields[12] as u16,
                c_equals_d: fields[13] != 0,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------
// The differential itself.
// ---------------------------------------------------------------------

#[test]
#[ignore = "spawns a real Copperline emulator over its control protocol; slow and \
            skips cleanly when `copperline` isn't on PATH -- run with --ignored"]
fn blitter_differential_against_copperline() {
    let Some(mut ccp) = Ccp::launch() else {
        eprintln!("SKIP: `copperline` not found on PATH -- see docs/blitter-differential.md");
        return;
    };

    let mut rng = Rng::new(SEED);
    let mut arena = Arena::new(ARENA_BASE, ARENA_CEILING);
    let mut failures: Vec<String> = Vec::new();
    let mut ran = 0usize;

    let mut area_cases = Vec::new();
    area_cases.extend(minterm_sweep_cases());
    area_cases.extend(channel_enable_cases());
    area_cases.extend(shift_cases(&mut rng));
    area_cases.extend(mask_cases());
    area_cases.extend(fill_cases());
    area_cases.extend(modulo_and_size_cases(&mut rng));
    area_cases.extend(bltsize_zero_field_cases());
    area_cases.extend(workbench_corpus_cases());

    for case in &area_cases {
        ran += 1;
        eprintln!("[{ran}/{}] {}", area_cases.len(), case.name);
        if let Some(msg) = run_area_case(&mut ccp, &mut arena, &mut rng, case) {
            failures.push(msg);
        }
    }

    for case in &line_cases() {
        ran += 1;
        if let Some(msg) = run_line_case(&mut ccp, &mut arena, case) {
            failures.push(msg);
        }
    }

    eprintln!(
        "blitter differential: {ran} cases run against Copperline, {} divergence(s), seed={SEED:#X}",
        failures.len()
    );

    assert!(
        failures.is_empty(),
        "{} of {ran} cases diverged from Copperline (seed={SEED:#X}):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// **Documented, expected divergence** (not a bug this differential is
/// meant to catch and not fixed by this file -- see the task's own
/// framing and `docs/blitter-differential.md`): our `Blitter::execute`
/// runs whenever a size register is written, completely ignoring
/// `DMACON`; Copperline (like real hardware) refuses to run the blitter
/// at all without `DMAEN`+`BLTEN` set. This test pins that gap so it's
/// visible and reproducible rather than silently assumed, and so a
/// future fix to `machine-core` (out of scope for this file -- see
/// `crates/machine-hosted/**`/`scripts/**` ownership note in the task
/// brief) has a test here to flip once it lands.
#[test]
#[ignore = "spawns a real Copperline emulator over its control protocol; slow and \
            skips cleanly when `copperline` isn't on PATH -- run with --ignored"]
fn dmacon_gate_is_a_known_divergence_from_hardware() {
    let Some(mut ccp) = Ccp::launch() else {
        eprintln!("SKIP: `copperline` not found on PATH -- see docs/blitter-differential.md");
        return;
    };

    let mut arena = Arena::new(ARENA_BASE, ARENA_CEILING);
    let a_win = alloc_window(&mut arena, 64);
    let d_win = alloc_window(&mut arena, 64);
    let seed = [0xABu8; 8]; // 4 words at the A pointer

    // Host: runs regardless of DMACON, because it never looks at it.
    let mut ram = Ram::zeroed().clone_boxed();
    ram[a_win.start as usize..a_win.start as usize + seed.len()].copy_from_slice(&seed);
    let mut hb = Blitter::new();
    hb.write(
        reg::BLTCON0,
        blitter::BLTCON0_USEA | blitter::BLTCON0_USED | 0xF0,
    );
    hb.write(reg::BLTAFWM, 0xFFFF);
    hb.write(reg::BLTALWM, 0xFFFF);
    hb.write(reg::BLTAPTH, (a_win.start >> 16) as u16);
    hb.write(reg::BLTAPTL, a_win.start as u16);
    hb.write(reg::BLTDPTH, (d_win.start >> 16) as u16);
    hb.write(reg::BLTDPTL, d_win.start as u16);
    hb.write(reg::BLTSIZE, (1 << 6) | 4);
    assert!(
        hb.execute(&mut ram),
        "host always runs a blit on a size write"
    );
    let host_wrote_data = ram[d_win.start as usize..d_win.start as usize + seed.len()] == seed;
    assert!(
        host_wrote_data,
        "host blitter is documented to ignore DMACON entirely -- if this fails, that \
         behaviour changed and this test's premise needs revisiting"
    );

    // Copperline: same registers, but DMACON is left at its post-reset
    // default (all DMA disabled) instead of build_program's usual
    // DMAEN|BLTEN poke -- assembled by hand here rather than through
    // build_program for exactly that reason.
    let code_addr = arena.alloc(256);
    let scratch = arena.alloc(2);
    let mut asm = Asm::new(code_addr);
    let writes: RegWrites = vec![
        (
            reg::BLTCON0,
            blitter::BLTCON0_USEA | blitter::BLTCON0_USED | 0xF0,
        ),
        (reg::BLTAFWM, 0xFFFF),
        (reg::BLTALWM, 0xFFFF),
        (reg::BLTAPTH, (a_win.start >> 16) as u16),
        (reg::BLTAPTL, a_win.start as u16),
        (reg::BLTDPTH, (d_win.start >> 16) as u16),
        (reg::BLTDPTL, d_win.start as u16),
        (reg::BLTSIZE, (1 << 6) | 4),
    ];
    for &(off, val) in &writes {
        asm.move_w_imm_absl(val, CUSTOM_BASE + off as u32);
    }
    // No BBUSY wait: without DMAEN|BLTEN the blit never starts, so
    // BBUSY never sets in the first place. Just a short, fixed
    // instruction budget's worth of settle time, then capture and halt.
    for _ in 0..64 {
        asm.nop();
    }
    asm.capture_absl_to_absl(BLTDDAT_ADDR, scratch);
    let halt_addr = asm.addr();
    asm.bra_s_self();

    ccp.write_mem(code_addr, &asm.code);
    ccp.write_mem(a_win.start, &seed);
    ccp.write_mem(d_win.start, &[0u8; 8]);
    ccp.regs_set("sr", 0x2700);
    ccp.regs_set("pc", code_addr);
    let stop = ccp.call("run_until", json!({ "pc": halt_addr, "wait_ms": 5_000 }));
    assert_eq!(stop["reason"], "target", "unexpected stop: {stop:?}");

    let cop_d = ccp.read_mem(d_win.start, 8);
    assert_eq!(
        cop_d,
        vec![0u8; 8],
        "Copperline ran the blit with DMACON disabled -- if this fails, Copperline's \
         DMA gating changed and this documented divergence may no longer hold"
    );

    eprintln!(
        "confirmed: with DMAEN|BLTEN clear, Copperline's blitter does not run \
         (destination stayed all-zero) while machine-core's Blitter::execute always \
         runs -- see docs/blitter-differential.md"
    );
}
