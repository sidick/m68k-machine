//! Opt-in blitter register tracing (`--blitter-trace FILE`), the
//! recording half of proposal §12's "differential against Copperline
//! (randomised ops + recorded Workbench traces)" -- see
//! `docs/blitter-differential.md`.
//!
//! **Design: record register combinations, not pixel data.** Capturing
//! and replaying a blit's actual source pixels would mean snapshotting
//! chip RAM per operation -- far more machinery than the value justifies
//! (`docs/blitter-differential.md` explains why). What the differential
//! wants from a real OS is the *combinations* of minterm, shift, mask,
//! modulo, and channel-enable a random generator is unlikely to reach --
//! `BltTemplate` text rendering, area fill, window/gadget borders. So
//! this watches every write that reaches a blitter register, and each
//! time `BLTSIZE` arms a blit (the same write `MachineBus::write_custom_word`
//! reacts to by calling `Blitter::execute`), records the *other*
//! registers that shaped it. Absolute pointers are dropped -- the
//! differential harness remaps every case's pointers into its own
//! scratch arena regardless of where the real OS put them -- except for
//! whether C and D happened to be the same address, which is a
//! structural property of the operation (graphics.library's
//! read-modify-write idiom, `D = A | C` onto existing content) worth
//! preserving.
//!
//! **Deduplicated on the fly.** A HashSet of the recorded signature
//! keeps only the first occurrence of each distinct register
//! combination, so a multi-thousand-blit boot yields a small corpus
//! rather than a firehose.
//!
//! **Line mode is recorded too, but as a distinct, raw shape.**
//! `BLTAPTL`/`BLTAPTH` in line mode hold the Bresenham error accumulator,
//! not a memory address, and `execute_line`'s doc comment in
//! `blitter.rs` records that `BLTCPT`/`BLTDPT` are assumed equal at arm
//! time and `BLTDPT` is simply overwritten from `BLTCPT` every pixel
//! rather than independently stepped. Replaying a captured line-mode
//! signature through this file's `AreaCase` shape -- which treats every
//! channel's pointer as an address to remap -- would poke the recorded
//! error term into an arena-remapped "pointer" and silently corrupt the
//! geometry. So line arms get their own `LineSignature`: every register
//! this task's replay needs is kept *verbatim* (`BLTAPT` most of all --
//! remapping it would corrupt the Bresenham state rather than test it),
//! with only `BLTCPT`/`BLTDPT` reduced to the same `c_eq_d` structural
//! flag `AreaCase` already uses for its own pointer channels, since the
//! replay harness remaps real addresses into its own scratch arena
//! regardless of where the real OS put them. The randomised half of the
//! differential already exercises line mode's octant/`SING`/texture
//! space by construction (`line_cases`); this half's value is real
//! register combinations a generator might under-sample, not novel
//! geometry coverage.
//!
//! **Gating.** Every hook in this file is behind `Option<BlitterTrace>`
//! being `None` when `--blitter-trace` is not passed, so a plain run
//! pays one `if let Some` check per register write and nothing else --
//! no file I/O, no hashing, no allocation.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufWriter, Write as _};
use std::path::Path;

use machine_core::blitter::{reg, BLTCON1_LINE};
use machine_core::CUSTOM_BASE;

/// An area-mode blit's replay-relevant register state, with absolute
/// pointers already dropped -- see the module doc comment. Field order
/// here is exactly the corpus file's `AREA` column order (see
/// `write_area_line`/the parser in `tests/blitter_differential.rs`); keep
/// them in sync if either changes.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Signature {
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
    /// `BLTSIZE`'s raw 6-bit width field (0 means 64 -- left encoded, not
    /// decoded, so replay can reuse `AreaCase::effective_width_words`'s
    /// existing zero-means-max handling).
    width: u16,
    /// `BLTSIZE`'s raw 10-bit height field (0 means 1024, same reasoning).
    height: u16,
    /// Whether `BLTCPT`/`BLTDPT` were equal at the moment `BLTSIZE`
    /// armed this blit -- graphics.library's read-modify-write idiom.
    c_eq_d: bool,
}

/// A line-mode blit's replay-relevant register state, kept **verbatim**
/// rather than address-remapped like `Signature` above -- see the module
/// doc comment for why `BLTAPT` in particular must never be touched.
/// Field order is the corpus file's `LINE` column order; keep in sync
/// with the parser in `tests/blitter_differential.rs`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct LineSignature {
    bltcon0: u16,
    bltcon1: u16,
    bltafwm: u16,
    bltalwm: u16,
    /// `BLTAPTH`/`BLTAPTL` -- the Bresenham error accumulator, not an
    /// address. Replayed verbatim, split high/low exactly as observed
    /// (rather than combined into a single value) so nothing is lost if
    /// the high word ever carries anything beyond sign extension.
    blt_apt_h: u16,
    blt_apt_l: u16,
    /// Bresenham corrective increments (`execute_line`'s `amod_step`/
    /// `bmod_step`), not row strides -- see `blitter.rs`'s doc comment.
    amod: i16,
    bmod: i16,
    /// The bitplane's bytes-per-row. `execute_line` only ever reads
    /// `BLTCMOD` for this (`BLTDMOD` is unused in line mode -- captured
    /// anyway for replay fidelity, per the task's instruction to record
    /// registers verbatim rather than pre-judge which ones matter).
    cmod: i16,
    dmod: i16,
    adat: u16,
    bdat: u16,
    cdat: u16,
    /// `BLTSIZE`'s raw fields -- width is conventionally 2 (unread by
    /// `execute_line`) and height is the line's pixel count.
    width: u16,
    height: u16,
    /// Whether `BLTCPT`/`BLTDPT` were equal at arm time. `execute_line`
    /// only ever advances `BLTCPT` and then copies it onto `BLTDPT` every
    /// pixel (`blitter.rs`'s doc comment), so a signature with this false
    /// is outside the convention this differential replays -- recorded
    /// anyway (never silently dropped) so the replay side can count and
    /// report exactly how many, rather than this recorder deciding for
    /// it.
    c_eq_d: bool,
}

/// Live shadow of the blitter registers this recorder cares about,
/// updated on every observed write so a `BLTSIZE` write has everything
/// it needs without re-reading the guest (which the CCP-based
/// differential harness can't do anyway, and which would need reaching
/// back into `machine_core::MachineBus` internals this crate doesn't
/// own).
#[derive(Default)]
struct Shadow {
    bltcon0: u16,
    bltcon1: u16,
    bltafwm: u16,
    bltalwm: u16,
    amod: u16,
    bmod: u16,
    cmod: u16,
    dmod: u16,
    adat: u16,
    bdat: u16,
    cdat: u16,
    cpth: u16,
    cptl: u16,
    dpth: u16,
    dptl: u16,
    /// `BLTAPTH`/`BLTAPTL` -- only meaningful (and only tracked here) for
    /// line-mode arms, where it's the Bresenham error term rather than an
    /// address; area mode drops A's pointer entirely (see `Signature`).
    apth: u16,
    aptl: u16,
}

pub struct BlitterTrace {
    file: BufWriter<File>,
    shadow: Shadow,
    seen: HashSet<Signature>,
    seen_lines: HashSet<LineSignature>,
    /// Every `BLTSIZE` write that armed an area-mode blit.
    total_area_blits: u64,
    /// Every `BLTSIZE` write that armed a line-mode blit.
    total_line_blits: u64,
    /// Of `total_line_blits`, how many had `BLTCPT != BLTDPT` at arm time
    /// -- outside the convention this differential's replay assumes (see
    /// `LineSignature::c_eq_d`'s doc comment). Recorded for the summary
    /// line so a non-zero count is visible, not silently folded in.
    line_blits_with_c_ne_d: u64,
}

impl BlitterTrace {
    /// Open (truncating) the trace file this run will append deduplicated
    /// signatures to. Returns an error the caller should treat exactly
    /// like any other `--*-log`-style open failure (see `cli.rs`'s
    /// `--blitter-trace` doc comment).
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        writeln!(
            writer,
            "# blitter register trace corpus -- one line per distinct signature\n\
             #\n\
             # AREA lines -- columns:\n\
             #   AREA bltcon0 bltcon1 bltafwm bltalwm amod bmod cmod dmod adat bdat cdat width height c_eq_d\n\
             # (all decimal; amod/bmod/cmod/dmod signed, width/height are BLTSIZE's raw\n\
             # fields with 0 meaning 64/1024, c_eq_d is 0 or 1 -- see\n\
             # crates/machine-hosted/src/blitter_trace.rs and docs/blitter-differential.md)\n\
             #\n\
             # LINE lines -- columns (registers kept verbatim, not remapped -- BLTAPT is\n\
             # the Bresenham error term here, not an address; see LineSignature):\n\
             #   LINE bltcon0 bltcon1 bltafwm bltalwm blt_apt_h blt_apt_l amod bmod cmod dmod adat bdat cdat width height c_eq_d"
        )?;
        Ok(BlitterTrace {
            file: writer,
            shadow: Shadow::default(),
            seen: HashSet::new(),
            seen_lines: HashSet::new(),
            total_area_blits: 0,
            total_line_blits: 0,
            line_blits_with_c_ne_d: 0,
        })
    }

    /// Observe one 16-bit write that reached the custom-register I/O
    /// window (`$DFF000`-`$DFFFFE`), in absolute address form -- called
    /// from `Bus`'s `AddressBus` impl for both `write_word` and (split in
    /// two) `write_long`, mirroring exactly the writes `MachineBus`
    /// itself would decompose a long write into. Byte writes to blitter
    /// registers are not observed: graphics.library and every OS blit
    /// path always poke these as words or longwords (register pairs like
    /// `BLTCON0`/`BLTCON1` are also frequently set with one `MOVE.L`,
    /// hence long-write support here), so this is a documented, not
    /// silent, scope limit.
    pub fn observe_word(&mut self, address: u32, value: u16) {
        if address < CUSTOM_BASE {
            return;
        }
        let offset = (address - CUSTOM_BASE) as u16 & 0x1FE;
        match offset {
            reg::BLTCON0 => self.shadow.bltcon0 = value,
            reg::BLTCON1 => self.shadow.bltcon1 = value,
            reg::BLTAFWM => self.shadow.bltafwm = value,
            reg::BLTALWM => self.shadow.bltalwm = value,
            reg::BLTCPTH => self.shadow.cpth = value,
            reg::BLTCPTL => self.shadow.cptl = value,
            reg::BLTDPTH => self.shadow.dpth = value,
            reg::BLTDPTL => self.shadow.dptl = value,
            reg::BLTAPTH => self.shadow.apth = value,
            reg::BLTAPTL => self.shadow.aptl = value,
            reg::BLTCMOD => self.shadow.cmod = value,
            reg::BLTBMOD => self.shadow.bmod = value,
            reg::BLTAMOD => self.shadow.amod = value,
            reg::BLTDMOD => self.shadow.dmod = value,
            reg::BLTCDAT => self.shadow.cdat = value,
            reg::BLTBDAT => self.shadow.bdat = value,
            reg::BLTADAT => self.shadow.adat = value,
            reg::BLTSIZE => self.on_size_write(value),
            _ => {}
        }
    }

    /// `BLTSIZE` is the classic-format arm write -- the same one
    /// `MachineBus::write_custom_word` reacts to by calling
    /// `Blitter::execute` (`crates/machine-core/src/lib.rs`). The ECS
    /// split-size path (`BLTSIZV`/`BLTSIZH`) is not separately watched
    /// here, matching the differential's own documented scope note that
    /// every case there triggers via classic `BLTSIZE` too.
    fn on_size_write(&mut self, value: u16) {
        let c_eq_d = (self.shadow.cpth, self.shadow.cptl) == (self.shadow.dpth, self.shadow.dptl);
        if self.shadow.bltcon1 & BLTCON1_LINE != 0 {
            self.total_line_blits += 1;
            if !c_eq_d {
                self.line_blits_with_c_ne_d += 1;
            }

            let sig = LineSignature {
                bltcon0: self.shadow.bltcon0,
                bltcon1: self.shadow.bltcon1,
                bltafwm: self.shadow.bltafwm,
                bltalwm: self.shadow.bltalwm,
                blt_apt_h: self.shadow.apth,
                blt_apt_l: self.shadow.aptl,
                amod: self.shadow.amod as i16,
                bmod: self.shadow.bmod as i16,
                cmod: self.shadow.cmod as i16,
                dmod: self.shadow.dmod as i16,
                adat: self.shadow.adat,
                bdat: self.shadow.bdat,
                cdat: self.shadow.cdat,
                width: value & 0x003F,
                height: (value >> 6) & 0x03FF,
                c_eq_d,
            };
            if self.seen_lines.insert(sig.clone()) {
                // Best-effort: see the area-mode branch below for why a
                // write failure here doesn't abort the boot.
                let _ = writeln!(
                    self.file,
                    "LINE {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                    sig.bltcon0,
                    sig.bltcon1,
                    sig.bltafwm,
                    sig.bltalwm,
                    sig.blt_apt_h,
                    sig.blt_apt_l,
                    sig.amod,
                    sig.bmod,
                    sig.cmod,
                    sig.dmod,
                    sig.adat,
                    sig.bdat,
                    sig.cdat,
                    sig.width,
                    sig.height,
                    sig.c_eq_d as u8,
                );
            }
            return;
        }
        self.total_area_blits += 1;

        let sig = Signature {
            bltcon0: self.shadow.bltcon0,
            bltcon1: self.shadow.bltcon1,
            bltafwm: self.shadow.bltafwm,
            bltalwm: self.shadow.bltalwm,
            amod: self.shadow.amod as i16,
            bmod: self.shadow.bmod as i16,
            cmod: self.shadow.cmod as i16,
            dmod: self.shadow.dmod as i16,
            adat: self.shadow.adat,
            bdat: self.shadow.bdat,
            cdat: self.shadow.cdat,
            width: value & 0x003F,
            height: (value >> 6) & 0x03FF,
            c_eq_d,
        };

        if self.seen.insert(sig.clone()) {
            // Best-effort: a trace file is a diagnostic/fixture-building
            // aid, not part of the guest-visible run, so a write failure
            // here (disk full, etc.) is not worth aborting the boot over.
            let _ = writeln!(
                self.file,
                "AREA {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
                sig.bltcon0,
                sig.bltcon1,
                sig.bltafwm,
                sig.bltalwm,
                sig.amod,
                sig.bmod,
                sig.cmod,
                sig.dmod,
                sig.adat,
                sig.bdat,
                sig.cdat,
                sig.width,
                sig.height,
                sig.c_eq_d as u8,
            );
        }
    }

    /// Flush the file and summarise what was captured, for the run
    /// report (`run.rs` prints this via `Console::diag`).
    pub fn finish(mut self) -> String {
        let _ = self.file.flush();
        format!(
            "blitter-trace: {} area-mode blit(s) observed, {} distinct signature(s) recorded; \
             {} line-mode blit(s) observed, {} distinct signature(s) recorded \
             ({} of those with BLTCPT != BLTDPT at arm time -- see blitter_trace.rs)",
            self.total_area_blits,
            self.seen.len(),
            self.total_line_blits,
            self.seen_lines.len(),
            self.line_blits_with_c_ne_d,
        )
    }
}
