//! Guest-memory introspection: answer "is Kickstart healthy?" by reading
//! Exec's own data structures through the bus, since a stock retail
//! Kickstart writes nothing to `SERDAT` and gives this runner no other
//! signal (unlike AROS, which narrates over serial -- see the Phase 1
//! investigation this module was written for).
//!
//! All reads go through [`machine_core::MachineBus::read_byte`]/`read_word`/
//! `read_long`, which never panic (open bus returns `0xFF`-filled values
//! for any unmapped address, `machine-core`'s `lib.rs` doc comment). This
//! module leans on that: every walk here is bounds- and sanity-checked
//! against plausible values rather than trusted blindly, so a guest that
//! never got far enough to build `ExecBase` (or one that's actively
//! corrupt) degrades to "not found" text instead of a panic or garbage.
//!
//! Offsets below are hand-derived from the NDK 3.2 headers
//! (`exec/execbase.h`, `exec/libraries.h`, `exec/nodes.h`, `exec/lists.h`,
//! `exec/tasks.h`, `exec/resident.h`, `exec/alerts.h`), not guessed or
//! taken from a disassembly: every multi-byte field in these structures
//! already lands on a 2-byte boundary given 68k's natural alignment and
//! the way the headers pair `UBYTE`/`BYTE` fields, so there is no compiler
//! padding to account for and a field-by-field byte count from the struct
//! definition is exact. Two independently-known real offsets served as a
//! cross-check while deriving these (`ChkBase` at `$26`, `AttnFlags` at
//! `$128`) and both fall out of the derivation below, which is why the
//! rest are trusted.

use machine_core::chipset::Chipset;
use machine_core::MachineBus;

/// Byte offsets into `struct ExecBase` (`exec/execbase.h`), derived by
/// summing the preceding fields' sizes -- see this module's doc comment.
mod execbase {
    /// `struct Library LibNode` is 34 bytes: `Node` is 14 (two `APTR` for
    /// succ/pred, a `UBYTE` type, a `BYTE` pri, an `APTR` name), plus
    /// `lib_Flags`(1), `lib_pad`(1), `lib_NegSize`(2), `lib_PosSize`(2),
    /// `lib_Version`(2), `lib_Revision`(2), `lib_IdString`(4), `lib_Sum`(4)
    /// and `lib_OpenCnt`(2).
    pub const LIB_NODE_LN_TYPE: u32 = 8;
    pub const LIB_VERSION: u32 = 20;
    pub const LIB_REVISION: u32 = 22;

    pub const SOFT_VER: u32 = 34;
    pub const CHK_BASE: u32 = 38;
    pub const MAX_LOC_MEM: u32 = 62;
    pub const CHK_SUM: u32 = 82;
    // IntVects[16] of struct IntVector (3 APTR = 12 bytes each) = 192
    // bytes, starting at 84, ending at 276.
    pub const THIS_TASK: u32 = 276;
    pub const ATTN_FLAGS: u32 = 296;
    pub const RES_MODULES: u32 = 300;
    // RES_MODULES is the APTR at 300..304; TaskTrapCode/TaskExceptCode/
    // TaskExitCode/TaskSigAlloc (four 4-byte fields) run 304..320, then
    // TaskTrapAlloc (UWORD) runs 320..322 -- MemList starts right after.
    /// `ExecBase->MemList` (`exec/execbase.h`'s "System Lists (private!)"
    /// block): the guest's actual free-memory list -- what `--fast-ram`'s
    /// verification reads to confirm `expansion.library` really linked
    /// the AUTOCONFIG board in, not merely that this bus answers for it.
    /// ResourceList/DeviceList/IntrList/LibList/PortList, each a 14-byte
    /// `struct List`, sit between here and TaskReady.
    pub const MEM_LIST: u32 = 322;
    /// `ExecBase->LibList`. `execbase.h`'s "System Lists (private!)"
    /// block runs `MemList, ResourceList, DeviceList, IntrList, LibList,
    /// PortList, TaskReady, TaskWait` -- six 14-byte `struct List`
    /// headers before `TaskReady` (already known correct at `406`, this
    /// module's original cross-check): `322 + 6*14 = 406`. `LibList` is
    /// the fifth of those six, four list-widths after `MemList`:
    /// `322 + 4*14 = 378`.
    ///
    /// Not walked for its own sake -- used only to find `expansion.
    /// library`'s own `Library` node (a library's base pointer *is* its
    /// node's address), the only way to reach `ExpansionBase` without
    /// hardcoding its address (`device-ledger.md`, "the rule for
    /// addresses" -- the same principle applied one level up, to a
    /// library base rather than a board's).
    pub const LIB_LIST: u32 = 378;
    pub const TASK_READY: u32 = 406;
    pub const TASK_WAIT: u32 = 420;
    // SoftInts[5] of `struct SoftIntList` (14 + 2 = 16 bytes each) = 80
    // bytes, starting at 434, ending at 514.
    pub const LAST_ALERT: u32 = 514;
}

/// Byte offsets into `struct Node` (`exec/nodes.h`): two `APTR` (succ,
/// pred) then `UBYTE ln_Type`, `BYTE ln_Pri`, `APTR ln_Name`.
mod node {
    pub const LN_SUCC: u32 = 0;
    pub const LN_TYPE: u32 = 8;
    pub const LN_PRI: u32 = 9;
    pub const LN_NAME: u32 = 10;
    pub const SIZE: u32 = 14;
}

/// Byte offsets into `struct List` (`exec/lists.h`): `lh_Head`, `lh_Tail`,
/// `lh_TailPred` (each `APTR`), `lh_Type`, `l_pad`.
mod list {
    pub const LH_HEAD: u32 = 0;
}

/// Byte offset of `ExpansionBase->MountList` (`libraries/expansionbase.h`).
/// Unlike every other offset in this module, the fields *before* it are
/// not documented by name -- the header spells them `eb_Private01`
/// through `eb_Private05` with no stated semantics, only sizes -- so this
/// is not the same derivation `execbase.h`'s "private" lists get
/// elsewhere in this file (there, the header still gives every field's
/// real name; here it deliberately withholds them). What *is* documented,
/// and all this offset needs, is each private field's **size**: `struct
/// Library LibNode`(34, `execbase.h`) + `UBYTE Flags`(1) + `UBYTE
/// eb_Private01`(1) + `ULONG eb_Private02`(4) + `ULONG eb_Private03`(4) +
/// `struct CurrentBinding eb_Private04`(16: four `APTR`/`STRPTR` fields --
/// `cb_ConfigDev`, `cb_FileName`, `cb_ProductString`, `cb_ToolTypes`,
/// `libraries/configvars.h`) + `struct List eb_Private05`(14) = 74. Skip
/// past unnamed bytes by their documented width, land on a named,
/// documented field (`MountList`) -- the same trick this module already
/// uses for `execbase.h`'s nominally-"private" system lists, but here
/// applied to fields whose *meaning* stays genuinely unknown, not merely
/// discouraged from use.
const EXPANSIONBASE_MOUNT_LIST: u32 = 74;

/// Byte offsets into `struct ExpansionRom` (`libraries/configregs.h`), 16
/// bytes, embedded as `struct ConfigDev`'s `cd_Rom` field.
mod expansionrom {
    pub const ER_TYPE: u32 = 0;
    pub const ER_PRODUCT: u32 = 1;
    pub const ER_MANUFACTURER: u32 = 4;
    pub const ER_INIT_DIAG_VEC: u32 = 10;
    /// `expansion.library` stashes the RAM address of the copied
    /// `DiagArea` here (RKRM 3rd ed. "Expansion Library", "Events At DIAG
    /// Time": "Expansion stores the ULONG address of that 'image' in the
    /// UBYTES er_ReservedOc, Od, 0e and Of ... stored as a longword"), or
    /// zero if `DiagEntry` returned failure (or was never called).
    pub const ER_RESERVED_0C: u32 = 12;
    pub const SIZE: u32 = 16;
}

/// Byte offsets into `struct ConfigDev` (`libraries/configvars.h`), past
/// the embedded `struct Node cd_Node` (14 bytes, `node::SIZE`).
mod configdev {
    use super::{expansionrom, node};
    /// `cd_Flags`(1) + `cd_Pad`(1) precede `cd_Rom`.
    pub const CD_ROM: u32 = node::SIZE + 2;
    pub const CD_BOARD_ADDR: u32 = CD_ROM + expansionrom::SIZE;
    pub const CD_BOARD_SIZE: u32 = CD_BOARD_ADDR + 4;
}

/// Byte offset of `BootNode->bn_DeviceNode` (`libraries/expansionbase.h`),
/// past the embedded `struct Node bn_Node` (14 bytes) and `UWORD
/// bn_Flags` (2).
const BOOTNODE_BN_DEVICE_NODE: u32 = node::SIZE + 2;

/// Byte offsets into `struct MemHeader` (`exec/memory.h`), past the
/// embedded `struct Node mh_Node` (14 bytes, `node::SIZE`).
mod memheader {
    use super::node;

    pub const MH_ATTRIBUTES: u32 = node::SIZE; // UWORD
    pub const MH_LOWER: u32 = node::SIZE + 6; // + mh_Attributes(2) + mh_First(4)
    pub const MH_UPPER: u32 = MH_LOWER + 4;
    pub const MH_FREE: u32 = MH_UPPER + 4;
}

/// `exec/memory.h`'s `MEMF_*` attribute bits, the ones worth naming in a
/// report: `MEMF_FAST` absent (and `MEMF_CHIP` present) is exactly what
/// distinguishes the 2 MB chip region every boot already has from a
/// `--fast-ram` board `expansion.library` has actually linked in.
const MEMF_PUBLIC: u16 = 1 << 0;
const MEMF_CHIP: u16 = 1 << 1;
const MEMF_FAST: u16 = 1 << 2;
const MEMF_LOCAL: u16 = 1 << 8;

/// Byte offsets into `struct Task` (`exec/tasks.h`), past the embedded
/// `struct Node tc_Node` (14 bytes).
mod task {
    use super::node;

    pub const TC_STATE: u32 = node::SIZE + 1; // tc_Flags(1) precedes it
    pub const TC_SIG_WAIT: u32 = node::SIZE + 8; // +Flags+State+ID+TDNest
}

/// Byte offsets into `struct Resident` (`exec/resident.h`).
mod resident {
    pub const RT_MATCH_WORD: u32 = 0;
    pub const RT_VERSION: u32 = 11;
    pub const RT_TYPE: u32 = 12;
    pub const RT_PRI: u32 = 13;
    pub const RT_NAME: u32 = 14;
    pub const RT_ID_STRING: u32 = 18;
    /// The 68000 ILLEGAL instruction, `exec/resident.h`'s `RTC_MATCHWORD`
    /// -- every genuine `Resident` structure starts with it, so it is the
    /// cheap plausibility check before trusting the rest of an entry.
    pub const MATCH_WORD: u16 = 0x4AFC;
}

const NT_LIBRARY: u8 = 9;
const NT_TASK: u8 = 1;

/// `exec/execbase.h`'s `AFF_*` bits, decoded for the report.
const ATTN_BITS: &[(u16, &str)] = &[
    (1 << 0, "68010"),
    (1 << 1, "68020"),
    (1 << 2, "68030"),
    (1 << 3, "68040"),
    (1 << 4, "68881"),
    (1 << 5, "68882"),
    (1 << 6, "FPU40"),
    (1 << 7, "68060"),
    (1 << 10, "FPGA"),
];

/// A subset of `exec/alerts.h`'s `AN_*`/`AG_*`/`AO_*` codes: enough to
/// name a Guru without reproducing the whole file. Matched by masking out
/// the subsystem-specific low bits when there is no exact match, per
/// alerts.h's documented format (1-bit deadend flag, 7-bit subsystem,
/// 8-bit general error, 16-bit subsystem-specific error).
const KNOWN_ALERTS: &[(u32, &str)] = &[
    (0x01000000, "exec.library"),
    (
        0x81000005,
        "exec: AN_MemCorrupt (corrupt memory list in FreeMem)",
    ),
    (
        0x81000006,
        "exec: AN_IntrMem (no memory for interrupt servers)",
    ),
    (0x02000000, "graphics.library"),
    (0x82010000, "graphics: AN_GfxNoMem (out of memory)"),
    (0x04000000, "intuition.library"),
    (0x84010007, "intuition: AN_OpenScreen (no memory)"),
    (0x84010002, "intuition: AN_CreatePort (no memory)"),
    (0x07000000, "dos.library"),
    (0x07010001, "dos: AN_StartMem (no memory at startup)"),
    (0x0A000000, "expansion.library"),
    (0x30000000, "bootstrap"),
    (
        0x30000001,
        "bootstrap: AN_BootError (boot code returned an error)",
    ),
    (0x31000000, "Workbench"),
];

/// A resident module found on `ExecBase->ResModules`.
pub struct ResidentEntry {
    pub address: u32,
    pub name: String,
    pub id_string: String,
    pub version: u8,
    pub priority: i8,
    pub node_type: u8,
}

/// One `struct MemHeader` found on `ExecBase->MemList` -- one contiguous
/// region `expansion.library`/Kickstart's own memory-sizing code has
/// actually linked into the free-memory pool. This is the direct evidence
/// `--fast-ram`'s verification needs: our own bus and AUTOCONFIG chain
/// answering for a board proves nothing about whether the guest adopted
/// it, but an entry here of the right size and attributes does.
pub struct MemListEntry {
    pub address: u32,
    pub name: String,
    pub attributes: u16,
    pub lower: u32,
    pub upper: u32,
    pub free: u32,
}

/// A task found on `TaskReady` or `TaskWait`.
pub struct TaskEntry {
    pub address: u32,
    pub name: String,
    pub priority: i8,
    pub state: u8,
    pub sig_wait: u32,
}

/// Everything this module could determine about the guest's Exec state.
pub struct Report {
    pub exec_base: Option<u32>,
    pub lib_version: u16,
    pub lib_revision: u16,
    pub soft_ver: u16,
    pub max_loc_mem: u32,
    pub attn_flags: u16,
    pub chk_base_ok: bool,
    pub chk_sum_ok: bool,
    pub resident_modules: Vec<ResidentEntry>,
    pub last_alert: [u32; 4],
    pub task_ready: Vec<TaskEntry>,
    pub task_wait: Vec<TaskEntry>,
    pub this_task: Option<TaskEntry>,
    pub mem_list: Vec<MemListEntry>,
}

/// Cap every walk (resident array, task lists) well above anything a real
/// Kickstart builds, so a corrupt or hostile chain of pointers still
/// terminates this function instead of looping until `max-instructions`
/// governs it from the outside.
const MAX_WALK_ENTRIES: usize = 512;
const MAX_STRING_LEN: usize = 96;

/// Read a NUL-terminated string from the guest, bounded by
/// [`MAX_STRING_LEN`]. Never trusts the guest to terminate it -- a
/// corrupt pointer just yields a truncated or empty string, not a panic
/// or an unbounded read (`read_byte` itself cannot fail: open bus at any
/// address it wanders into just returns `0xFF`, which is not ASCII NUL
/// and so still terminates the loop at the length cap).
fn read_cstr(bus: &mut MachineBus, address: u32) -> String {
    if address == 0 {
        return String::new();
    }
    let mut bytes = Vec::new();
    for i in 0..MAX_STRING_LEN as u32 {
        let byte = bus.read_byte(address.wrapping_add(i));
        if byte == 0 {
            break;
        }
        bytes.push(byte);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Read one node's name and priority (`struct Node`, `exec/nodes.h`).
fn read_node_name(bus: &mut MachineBus, node_addr: u32) -> (String, i8) {
    let name_ptr = bus.read_long(node_addr + node::LN_NAME);
    let name = read_cstr(bus, name_ptr);
    let pri = bus.read_byte(node_addr + node::LN_PRI) as i8;
    (name, pri)
}

/// Walk a `struct List` (`exec/lists.h`) of `struct Task` nodes.
///
/// The termination rule is the standard Exec one: the list's own
/// `lh_Tail` field is always `NULL`, and the last real node's `ln_Succ`
/// points at the address *of* `lh_Tail` -- so reading `ln_Succ` through
/// that fake "node" (whose first field, at the same offset as `ln_Succ`,
/// literally *is* `lh_Tail`) yields 0 and stops the walk without any
/// special-casing of the list header itself.
fn walk_task_list(bus: &mut MachineBus, list_addr: u32) -> Vec<TaskEntry> {
    let mut out = Vec::new();
    let mut node_addr = bus.read_long(list_addr + list::LH_HEAD);
    for _ in 0..MAX_WALK_ENTRIES {
        let succ = bus.read_long(node_addr + node::LN_SUCC);
        if succ == 0 {
            break;
        }
        let node_type = bus.read_byte(node_addr + node::LN_TYPE);
        if node_type == NT_TASK {
            let (name, priority) = read_node_name(bus, node_addr);
            let state = bus.read_byte(node_addr + task::TC_STATE);
            let sig_wait = bus.read_long(node_addr + task::TC_SIG_WAIT);
            out.push(TaskEntry {
                address: node_addr,
                name,
                priority,
                state,
                sig_wait,
            });
        }
        node_addr = succ;
    }
    out
}

/// Read one `TaskEntry` for a standalone task pointer (`ThisTask`),
/// distinct from [`walk_task_list`] since it is a single `Task*`, not a
/// list header. Returns `None` for a null or implausible pointer rather
/// than fabricating a task that was never there.
fn read_task(bus: &mut MachineBus, task_addr: u32) -> Option<TaskEntry> {
    if task_addr == 0 {
        return None;
    }
    let node_type = bus.read_byte(task_addr + node::LN_TYPE);
    if node_type != NT_TASK {
        return None;
    }
    let (name, priority) = read_node_name(bus, task_addr);
    let state = bus.read_byte(task_addr + task::TC_STATE);
    let sig_wait = bus.read_long(task_addr + task::TC_SIG_WAIT);
    Some(TaskEntry {
        address: task_addr,
        name,
        priority,
        state,
        sig_wait,
    })
}

/// Walk `ExecBase->MemList`: a `struct List` of `struct MemHeader` nodes
/// (`exec/memory.h`), one per contiguous region `AddMemList` has linked
/// into the system free-memory pool. Every real Kickstart always has at
/// least one entry (chip RAM); a `--fast-ram` board shows up here, with
/// `MEMF_FAST` set and `mh_Upper - mh_Lower` matching what was declared,
/// only once `expansion.library` has actually adopted it -- which is the
/// entire question this walk exists to answer (module docs). Same
/// termination shape as [`walk_task_list`]: the list header's own
/// `lh_Tail` slot is `NULL`, and the real last node's `ln_Succ` points at
/// that slot, so reading through it yields 0 and stops the walk without
/// special-casing the header.
fn walk_mem_list(bus: &mut MachineBus, list_addr: u32) -> Vec<MemListEntry> {
    let mut out = Vec::new();
    let mut node_addr = bus.read_long(list_addr + list::LH_HEAD);
    for _ in 0..MAX_WALK_ENTRIES {
        let succ = bus.read_long(node_addr + node::LN_SUCC);
        if succ == 0 {
            break;
        }
        let (name, _pri) = read_node_name(bus, node_addr);
        out.push(MemListEntry {
            address: node_addr,
            name,
            attributes: bus.read_word(node_addr + memheader::MH_ATTRIBUTES),
            lower: bus.read_long(node_addr + memheader::MH_LOWER),
            upper: bus.read_long(node_addr + memheader::MH_UPPER),
            free: bus.read_long(node_addr + memheader::MH_FREE),
        });
        node_addr = succ;
    }
    out
}

/// Walk `ExecBase->ResModules`: a `NULL`-terminated array of `struct
/// Resident *`, built by `InitCode()` during boot from every ROMTag it
/// found (`exec/resident.h`). This is the single best artefact for "how
/// far did Kickstart's init chain get" -- each entry it lists is a
/// library, device, or resource whose `RTF_AUTOINIT` (or explicit `rt_Init`)
/// code actually ran.
fn walk_resident_modules(bus: &mut MachineBus, array_addr: u32) -> Vec<ResidentEntry> {
    let mut out = Vec::new();
    if array_addr == 0 {
        return out;
    }
    for i in 0..MAX_WALK_ENTRIES as u32 {
        let entry_addr = bus.read_long(array_addr + i * 4);
        if entry_addr == 0 {
            break;
        }
        let match_word = bus.read_word(entry_addr + resident::RT_MATCH_WORD);
        if match_word != resident::MATCH_WORD {
            // Not a real Resident structure -- stop rather than report
            // garbage; a well-formed array never contains one of these.
            break;
        }
        let name_ptr = bus.read_long(entry_addr + resident::RT_NAME);
        let id_ptr = bus.read_long(entry_addr + resident::RT_ID_STRING);
        out.push(ResidentEntry {
            address: entry_addr,
            name: read_cstr(bus, name_ptr),
            id_string: read_cstr(bus, id_ptr),
            version: bus.read_byte(entry_addr + resident::RT_VERSION),
            priority: bus.read_byte(entry_addr + resident::RT_PRI) as i8,
            node_type: bus.read_byte(entry_addr + resident::RT_TYPE),
        });
    }
    out
}

/// Sum every 16-bit word from `ExecBase` (the `Library` node start) up to
/// and including `ChkSum` itself; `execbase.h`'s doc comment ("ChkSum --
/// for all of the above (minus 2)") describes the standard Amiga rolling
/// checksum trick, where the stored value is chosen so the total,
/// wrapping-summed including the checksum word itself, comes out to
/// `0xFFFF`. Advisory only -- Exec only maintains this during very early
/// boot, so a healthy, long-running Kickstart legitimately fails this
/// check once later init code has touched anything in the summed range;
/// it is reported, not treated as a failure signal on its own.
fn chk_sum_consistent(bus: &mut MachineBus, exec_base: u32) -> bool {
    let mut sum: u16 = 0;
    let mut addr = exec_base;
    let end = exec_base + execbase::CHK_SUM + 2;
    while addr < end {
        sum = sum.wrapping_add(bus.read_word(addr));
        addr += 2;
    }
    sum == 0xFFFF
}

/// Build the full introspection report from the guest's current memory
/// state. Never panics: every step degrades to "absent"/"implausible"
/// rather than trusting an address that doesn't check out, since this is
/// meant to run against a guest that may not have gotten anywhere near
/// building these structures yet (roadmap Phase 1's whole open question).
pub fn inspect(bus: &mut MachineBus) -> Report {
    let ptr = bus.read_long(0x0000_0004);

    // Plausibility gate before trusting anything at `ptr`: must be
    // long-aligned (`AllocMem` never returns an odd address) and its
    // `LIB_NODE.ln_Type` must read `NT_LIBRARY`.
    //
    // `ptr` landing inside chip RAM specifically was this gate's original
    // check, on the reasoning that `InitCode()` allocates `ExecBase`
    // before `expansion.library` has linked in anything else. That
    // reasoning covers the *initial* placement but not the final one: a
    // real Kickstart 3.2.2 boot with a Zorro III `--fast-ram` board
    // attached moves `ExecBase` itself into that fast RAM by the time the
    // guest goes idle -- confirmed against Copperline (this project's
    // oracle) booting the same `nondistribution/A1200.47.115.rom` with a
    // 16 MB Zorro III board of its own: both land `ExecBase` at the
    // identical address, `$4000089c`. So "is this address real, backed
    // RAM at all" (any region [`machine_core::GuestMemory::ram_slice`]
    // resolves) is the actual invariant, not "is this address inside chip
    // RAM specifically" -- the latter would silently reject a perfectly
    // healthy guest the moment `--fast-ram` starts working.
    let exec_base = if ptr != 0
        && ptr.is_multiple_of(4)
        && machine_core::GuestMemory::ram_slice(bus, ptr, execbase::LIB_NODE_LN_TYPE + 1).is_some()
        && bus.read_byte(ptr + execbase::LIB_NODE_LN_TYPE) == NT_LIBRARY
    {
        Some(ptr)
    } else {
        None
    };

    let Some(base) = exec_base else {
        return Report {
            exec_base: None,
            lib_version: 0,
            lib_revision: 0,
            soft_ver: 0,
            max_loc_mem: 0,
            attn_flags: 0,
            chk_base_ok: false,
            chk_sum_ok: false,
            resident_modules: Vec::new(),
            last_alert: [0; 4],
            task_ready: Vec::new(),
            task_wait: Vec::new(),
            this_task: None,
            mem_list: Vec::new(),
        };
    };

    let chk_base = bus.read_long(base + execbase::CHK_BASE);
    // Documented invariant (`execbase.h`: "system base pointer
    // complement"): ChkBase is the bitwise complement of the ExecBase
    // pointer itself.
    let chk_base_ok = chk_base == !base;

    let res_modules_ptr = bus.read_long(base + execbase::RES_MODULES);
    let last_alert = [
        bus.read_long(base + execbase::LAST_ALERT),
        bus.read_long(base + execbase::LAST_ALERT + 4),
        bus.read_long(base + execbase::LAST_ALERT + 8),
        bus.read_long(base + execbase::LAST_ALERT + 12),
    ];
    let this_task_ptr = bus.read_long(base + execbase::THIS_TASK);

    Report {
        exec_base: Some(base),
        lib_version: bus.read_word(base + execbase::LIB_VERSION),
        lib_revision: bus.read_word(base + execbase::LIB_REVISION),
        soft_ver: bus.read_word(base + execbase::SOFT_VER),
        max_loc_mem: bus.read_long(base + execbase::MAX_LOC_MEM),
        attn_flags: bus.read_word(base + execbase::ATTN_FLAGS),
        chk_base_ok,
        chk_sum_ok: chk_sum_consistent(bus, base),
        resident_modules: walk_resident_modules(bus, res_modules_ptr),
        last_alert,
        task_ready: walk_task_list(bus, base + execbase::TASK_READY),
        task_wait: walk_task_list(bus, base + execbase::TASK_WAIT),
        this_task: read_task(bus, this_task_ptr),
        mem_list: walk_mem_list(bus, base + execbase::MEM_LIST),
    }
}

/// Decode a `LastAlert[0]` value against `exec/alerts.h`'s format: bit 31
/// is `AT_DeadEnd`, and the rest is matched against [`KNOWN_ALERTS`] --
/// first exactly, then with the 16-bit subsystem-specific low bits masked
/// off, since that residue is often a per-call detail (e.g. which library
/// failed to open) this module doesn't attempt to enumerate exhaustively.
fn decode_alert(code: u32) -> String {
    if code == 0 {
        return "none".to_string();
    }
    if code == 0xFFFF_FFFF {
        // Not a real alert: `Alert()` never resets `LastAlert` to zero on
        // a clean boot (it exists to survive a warm reboot after a real
        // crash, so nothing proactively clears it), and Kickstart's own
        // chip-RAM sizing probe fills untouched memory with an all-ones
        // test pattern before anything else claims it (AHRM, memory
        // sizing). All-ones also fails the subsystem-plausibility check
        // below (`SubSysId` would be `0x7F`, past every real subsystem
        // `alerts.h` defines, topping out at `AN_Unknown`'s `0x35`) --
        // called out explicitly rather than mis-decoded as "DeadEnd,
        // unknown subsystem", which would read as a real (if
        // unidentified) Guru.
        return "0xffffffff (uninitialised chip-RAM fill, not a real alert -- Alert() was never called)".to_string();
    }
    let dead_end = code & 0x8000_0000 != 0;
    let masked = code & 0x7FFF_FFFF;
    let name = KNOWN_ALERTS
        .iter()
        .find(|(k, _)| *k & 0x7FFF_FFFF == masked)
        .or_else(|| {
            KNOWN_ALERTS
                .iter()
                .find(|(k, _)| *k & 0x7F00_0000 == masked & 0x7F00_0000)
        })
        .map(|(_, name)| *name)
        .unwrap_or("unknown subsystem");
    format!(
        "{code:#010x} ({}, {name})",
        if dead_end { "DeadEnd" } else { "Recovery" }
    )
}

/// Format the full report as `host |`-prefixed diagnostic text, one
/// concern per section, matching this runner's existing console
/// conventions (`console.rs`).
pub fn format_report(report: &Report) -> String {
    let mut out = String::new();
    let Some(base) = report.exec_base else {
        out.push_str(
            "introspect: no plausible ExecBase at $00000004 -- exec never \
             finished initialising (or hasn't yet)",
        );
        return out;
    };

    out.push_str(&format!("introspect: ExecBase at {base:#010x}\n"));
    out.push_str(&format!(
        "  exec.library {}.{}  SoftVer {:#06x}  MaxLocMem {:#010x}\n",
        report.lib_version, report.lib_revision, report.soft_ver, report.max_loc_mem
    ));

    let attn_names: Vec<&str> = ATTN_BITS
        .iter()
        .filter(|(bit, _)| report.attn_flags & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    out.push_str(&format!(
        "  AttnFlags {:#06x} ({})\n",
        report.attn_flags,
        if attn_names.is_empty() {
            "none recognised".to_string()
        } else {
            attn_names.join(", ")
        }
    ));
    out.push_str(&format!(
        "  ChkBase consistent: {}   ChkSum consistent: {} (advisory -- only \
         held during early boot)\n",
        report.chk_base_ok, report.chk_sum_ok
    ));

    let alert_active = report.last_alert.iter().any(|&a| a != 0);
    out.push_str(&format!(
        "  LastAlert: {}{}\n",
        decode_alert(report.last_alert[0]),
        if alert_active {
            format!(
                " [data: {:#010x} {:#010x} {:#010x}]",
                report.last_alert[1], report.last_alert[2], report.last_alert[3]
            )
        } else {
            String::new()
        }
    ));

    out.push_str(&format!(
        "  resident modules initialised: {}\n",
        report.resident_modules.len()
    ));
    for m in &report.resident_modules {
        out.push_str(&format!(
            "    {:#010x}  pri {:>4}  v{:<3}  type {:<3}  {:<20} {}\n",
            m.address, m.priority, m.version, m.node_type, m.name, m.id_string
        ));
    }

    out.push_str(&format!("  MemList ({}):\n", report.mem_list.len()));
    for m in &report.mem_list {
        out.push_str(&format!("    {}\n", mem_list_line(m)));
    }

    out.push_str(&format!(
        "  this task: {}\n",
        task_line(report.this_task.as_ref())
    ));
    out.push_str(&format!("  TaskReady ({}):\n", report.task_ready.len()));
    for t in &report.task_ready {
        out.push_str(&format!("    {}\n", task_line(Some(t))));
    }
    out.push_str(&format!("  TaskWait ({}):\n", report.task_wait.len()));
    for t in &report.task_wait {
        out.push_str(&format!("    {}\n", task_line(Some(t))));
    }

    out
}

/// Summarise the display-relevant chipset registers the Phase 2 renderer
/// reads (`render.rs`'s `CopperState`/`Geometry::decode`) -- the
/// diagnostic for "the screenshot is blank" investigations this crate's
/// `--screenshot` flag motivates: distinguishes "the renderer has
/// nothing to draw because the guest never programmed the display" from
/// "the guest programmed real geometry and the renderer got it wrong",
/// per the Phase 2 task brief's "dig one level" guidance. `chipset`'s
/// registers are latched copies (`chipset.rs`), so this is exactly what
/// the renderer's own copper walk would see if it ran from `cop1lc` --
/// this just reports them without walking the list, since a blank
/// `cop1lc`/`bplcon0` already answers "did the guest program anything at
/// all" without needing chip RAM.
///
/// **`SPR0PT` only, for the sprite**: `SPR0POS`/`SPR0CTL` are deliberately
/// not printed here even though the chipset has them. A real Kickstart
/// 3.2.2 capture during the sprite-height investigation this line was
/// added for showed *why*: `render.rs`'s `draw_sprite0` reads the
/// sprite's real position/control header from chip RAM at `SPR0PT`/
/// `SPR0PT+2` (matching real sprite-DMA fetch semantics — see that
/// function's doc comment), not from these two chipset registers, which
/// this machine has no DMA engine to keep in sync with the sprite list
/// and which for a standard pointer sprite are frequently stale and
/// unrelated to it. Printing them here would invite exactly the
/// mis-diagnosis that investigation started from.
pub fn format_display_state(chipset: &Chipset) -> String {
    format!(
        "display state: COP1LC {:#010x}  COP2LC {:#010x}  BPLCON0 {:#06x}  BPLCON1 {:#06x}  \
         BPL1PT {:#010x}  DIWSTRT/STOP {:#06x}/{:#06x}  DDFSTRT/STOP {:#06x}/{:#06x}  \
         COLOR00 {:#06x}  SPR0PT {:#010x}",
        chipset.cop1lc,
        // COP2LC matters as much as COP1LC for diagnosing a blank
        // screen: Kickstart's boot view keeps COP1LC pointing at a
        // stub list that strobes COPJMP2, and installs the real
        // (bitplane-enabling) list through COP2LC from the VBlank
        // server every frame -- so a machine that "looks blank" on
        // COP1LC alone may in fact have a fully-built screen here.
        chipset.cop2lc,
        chipset.bplcon0,
        chipset.bplcon1,
        chipset.bplpt[0],
        chipset.diwstrt,
        chipset.diwstop,
        chipset.ddfstrt,
        chipset.ddfstop,
        chipset.color[0],
        chipset.spr0pt,
    )
}

/// Summarise the floppy-relevant state (proposal §7.1/§7.2, `cia.rs`'s
/// `FloppyDrive`): whether the disk DMA engine was ever actually armed
/// (`DSKLEN`'s `DMAEN` bit, `DSKPT`), and the raw CIA-A/CIA-B port bytes
/// the floppy model drives and is driven from. This is what distinguishes
/// "trackdisk read our CIA status bits and gave up cleanly" (`DSKPT`
/// stays `0`, `DSKLEN` never gets `DMAEN`) from "trackdisk tried a real
/// transfer and is waiting on a `DSKBLK` completion this machine's sink
/// registers (§7.1) never raise" -- the two very different explanations
/// for a `trackdisk.device` task parked in `TaskWait`.
pub fn format_disk_state(bus: &MachineBus) -> String {
    format!(
        "disk state: DSKLEN {:#06x}  DSKPT {:#010x}  CIA-A PRA {:#04x} DDRA {:#04x}  \
         CIA-B PRB {:#04x} DDRB {:#04x}",
        bus.chipset.dsklen,
        bus.chipset.dskpt,
        bus.cia_a.pra,
        bus.cia_a.ddra,
        bus.cia_b.prb,
        bus.cia_b.ddrb,
    )
}

/// Summarise the attached Graffity card's state (`--graphics`), for
/// diagnosing exactly how far an RTG boot got: whether a driver ever
/// programmed a presentable mode at all (`decoded_mode`), and, when it
/// did, the geometry/depth it chose plus a couple of palette entries —
/// distinguishing "the driver opened the screen but painted nothing"
/// (VRAM's first bytes and the palette both still at their power-on
/// zero) from "painted, but the wrong colours" (palette entry 0 or 1 set
/// to something unexpected) without needing a screenshot at all. `None`
/// when no card is attached.
///
/// Prints every AUTOCONFIG board index the card might have registered
/// rather than naming them "VRAM"/"regs": Zorro II takes two boards,
/// Zorro III just one, and which board is which aperture is
/// `machine_core::graffity`'s business, not this diagnostic's --
/// `machine_core::MachineBus` itself no longer knows either (see that
/// module's doc comment on the board-index seam).
pub fn format_graphics_state(bus: &MachineBus) -> String {
    let Some(card) = bus.graphics() else {
        return "graphics state: no card attached".to_string();
    };
    let fmt_base = |base: Option<u32>| match base {
        Some(b) => format!("{b:#010x}"),
        None => "none".to_string(),
    };
    // Ask the bus which chain slots are actually this card's, rather
    // than probing the front of the chain. Those were the same thing
    // until fast RAM added a third board: the memory board can win the
    // first Zorro III slot, and this then reported its base as the
    // graphics card's while the card sat somewhere else entirely. The
    // card really does move -- AUTOCONFIG assigns dynamically, as on real
    // hardware -- and nothing should assume otherwise.
    let boards: std::vec::Vec<String> = bus
        .graphics_board_bases()
        .iter()
        .enumerate()
        .map(|(i, base)| format!("board[{i}] {}", fmt_base(*base)))
        .collect();
    let boards = boards.join("  ");
    match card.decoded_mode() {
        Some(mode) => {
            let first_bytes: std::vec::Vec<u8> = (0..8)
                .map(|i| card.vram_read(mode.start_offset + i))
                .collect();
            format!(
                "graphics state: {boards}  \
                 decoded_mode {}x{} {:?}  stride {}  start_offset {:#x}  \
                 first VRAM bytes at start_offset {:02x?}  palette[0] {:#010x}  palette[1] {:#010x}",
                mode.width,
                mode.height,
                mode.depth,
                mode.stride_bytes,
                mode.start_offset,
                first_bytes,
                card.palette_argb(0),
                card.palette_argb(1),
            )
        }
        None => format!(
            "graphics state: {boards}  \
             decoded_mode is None (driver has not programmed a presentable mode yet)"
        ),
    }
}

/// One `hostblk` `ConfigDev` located in guest memory -- see
/// [`find_hostblk_config_dev`] for how it's found.
struct HostblkConfigDev {
    address: u32,
    er_type: u8,
    er_init_diag_vec: u16,
    board_addr: u32,
    board_size: u32,
    diag_copy_addr: u32,
}

/// Find a `Library` node on a `struct List` of them (e.g. `ExecBase->
/// LibList`) by name, and return its address -- which, for a `Library`,
/// *is* the base pointer callers use (`exec/libraries.h`: the negative-
/// offset jump table lives before it, not after, so the node's own
/// address is the library base). This is the only sanctioned-by-header
/// way this module has to reach `ExpansionBase`: NDK 3.2 does not publish
/// its address anywhere fixed, unlike `ExecBase` at `$4`.
fn find_library_base(bus: &mut MachineBus, list_addr: u32, name: &str) -> Option<u32> {
    let mut node_addr = bus.read_long(list_addr + list::LH_HEAD);
    for _ in 0..MAX_WALK_ENTRIES {
        let succ = bus.read_long(node_addr + node::LN_SUCC);
        if succ == 0 {
            break;
        }
        if bus.read_byte(node_addr + node::LN_TYPE) == NT_LIBRARY {
            let (lib_name, _pri) = read_node_name(bus, node_addr);
            if lib_name == name {
                return Some(node_addr);
            }
        }
        node_addr = succ;
    }
    None
}

/// Walk `ExpansionBase->MountList`: a priority-sorted `struct List` of
/// `BootNode`s (`libraries/expansionbase.h`), one per board a driver has
/// made bootable via `AddBootNode()`. Returns each node's address and its
/// `bn_DeviceNode` pointer. Same termination shape as
/// [`walk_task_list`]/[`walk_mem_list`].
fn walk_boot_nodes(bus: &mut MachineBus, list_addr: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut node_addr = bus.read_long(list_addr + list::LH_HEAD);
    for _ in 0..MAX_WALK_ENTRIES {
        let succ = bus.read_long(node_addr + node::LN_SUCC);
        if succ == 0 {
            break;
        }
        out.push((
            node_addr,
            bus.read_long(node_addr + BOOTNODE_BN_DEVICE_NODE),
        ));
        node_addr = succ;
    }
    out
}

/// Locate `hostblk`'s own `ConfigDev`, if `expansion.library` created one
/// for it.
///
/// NDK 3.2 does not publish where `ExpansionBase` keeps the head of its
/// `ConfigDev` list: `libraries/expansionbase.h` marks every field before
/// `MountList` `eb_PrivateNN` with no documented semantics beyond size
/// (see [`EXPANSIONBASE_MOUNT_LIST`]), and the autodoc for
/// `expansion.library/FindConfigDev` documents only the call's register
/// convention, never a memory layout -- confirming the RKRM's own
/// "Expansion Library" chapter, which states plainly that "descriptions
/// of all configured boards are kept in a **private** ExpansionBase list
/// of ConfigDev structures" and gives `FindConfigDev()` as the only way
/// to reach it. That is a real 68k call this host-side tool -- which
/// reads guest memory directly rather than driving the emulated CPU --
/// cannot make.
///
/// Guessing that private layout is exactly what this project's brief
/// prohibits, so this takes a different, fully documented route instead:
/// scan chip RAM for a `ConfigDev` (`libraries/configvars.h`, a public
/// struct start to finish) whose `cd_Rom` identity and `cd_BoardAddr`
/// agree with what our own AUTOCONFIG chain independently knows it placed
/// this board at ([`MachineBus::hostblk_board_base`]). Three fields must
/// agree at once (address, manufacturer, product), so this is a targeted
/// check against ground truth this process already trusts, not a blind
/// structural guess across RAM. Scoped to chip RAM only for this
/// increment -- `expansion.library`'s own board-list allocations run very
/// early in boot, well before a `--fast-ram` board (if any) would be
/// linked in, so chip RAM is where a `ConfigDev` actually lands in
/// practice; extending the scan to fast RAM is future work, not a
/// limitation this increment's evidence depends on.
fn find_hostblk_config_dev(bus: &mut MachineBus, expected_base: u32) -> Option<HostblkConfigDev> {
    use machine_core::hostblk;
    let mut addr = 0u32;
    while (addr as usize) < machine_core::CHIP_RAM_SIZE {
        if bus.read_long(addr + configdev::CD_BOARD_ADDR) == expected_base
            && bus.read_word(addr + configdev::CD_ROM + expansionrom::ER_MANUFACTURER)
                == hostblk::MANUFACTURER
            && bus.read_byte(addr + configdev::CD_ROM + expansionrom::ER_PRODUCT)
                == hostblk::PRODUCT
        {
            return Some(HostblkConfigDev {
                address: addr,
                er_type: bus.read_byte(addr + configdev::CD_ROM + expansionrom::ER_TYPE),
                er_init_diag_vec: bus
                    .read_word(addr + configdev::CD_ROM + expansionrom::ER_INIT_DIAG_VEC),
                board_addr: expected_base,
                board_size: bus.read_long(addr + configdev::CD_BOARD_SIZE),
                diag_copy_addr: bus
                    .read_long(addr + configdev::CD_ROM + expansionrom::ER_RESERVED_0C),
            });
        }
        addr += 4;
    }
    None
}

/// Search chip RAM for a byte-for-byte copy of [`machine_core::hostblk::
/// DIAG_ROM`]'s own header, so this check does not depend on
/// `ConfigDev.cd_Rom.er_Reserved0c..0f` actually holding the copy address
/// -- that field is, after all, named `Reserved`, and the RKRM behaviour
/// this module cites for it is Release 2 (V37, 1991); a V47 Kickstart is
/// not contractually bound to keep using it the same way. Finding the raw
/// bytes is a second, independent way to answer "did expansion.library
/// copy this board's DiagArea into RAM at all", not dependent on that one
/// field's meaning having survived unchanged for three decades. If found,
/// also reports the scratch marker cell DiagEntry writes to
/// (`hostblk::DIAG_MARKER_OFFSET`) -- nonzero there is conclusive
/// (DiagEntry's first instruction is the register read that feeds it),
/// independent of both mechanisms above.
fn find_diag_rom_copy_by_signature(bus: &mut MachineBus) -> Option<(u32, u32)> {
    // First 8 bytes of the embedded ROM's own DiagArea header (da_Config,
    // da_Flags, da_Size, da_DiagPoint) -- read from `hostblk::DIAG_ROM`
    // itself so this never drifts from what was actually assembled.
    let needle = &machine_core::hostblk::DIAG_ROM[0..8];
    let mut addr = 0u32;
    while (addr as usize) + needle.len() <= machine_core::CHIP_RAM_SIZE {
        if (0..needle.len() as u32).all(|i| bus.read_byte(addr + i) == needle[i as usize]) {
            let marker = bus.read_long(addr + machine_core::hostblk::DIAG_MARKER_OFFSET);
            return Some((addr, marker));
        }
        addr += 1;
    }
    None
}

/// Summarise what Kickstart did with the `hostblk` board (`--hostblk`):
/// whether AUTOCONFIG placed it, whether `expansion.library` created a
/// `ConfigDev` for it and accepted its DiagArea, whether `DiagEntry`
/// actually ran and reached the board's own registers, and whether a
/// boot node exists yet (it won't, in this increment -- see
/// `docs/hostblk-protocol.md` section 12). `exec_base` should be
/// [`Report::exec_base`] from a prior [`inspect`] call on the same `bus`,
/// so `ExpansionBase` can be found via `ExecBase->LibList`
/// ([`find_library_base`]) without a second full walk.
pub fn format_hostblk_state(bus: &mut MachineBus, exec_base: Option<u32>) -> String {
    let Some(base) = bus.hostblk_board_base() else {
        return "hostblk state: no board attached, or not yet configured by AUTOCONFIG".to_string();
    };
    let mut out = format!("hostblk state: AUTOCONFIG placed the board at {base:#010x}\n");

    match find_hostblk_config_dev(bus, base) {
        Some(cd) => {
            let diagvalid = cd.er_type & machine_core::autoconfig::ERTF_DIAGVALID != 0;
            out.push_str(&format!(
                "  ConfigDev at {:#010x}: er_Type {:#04x} ({}DIAGVALID)  er_InitDiagVec {:#06x}  cd_BoardAddr {:#010x}  cd_BoardSize {:#010x}\n",
                cd.address,
                cd.er_type,
                if diagvalid { "" } else { "no " },
                cd.er_init_diag_vec,
                cd.board_addr,
                cd.board_size,
            ));
            // Read the DiagArea's header directly from the board's own
            // live window (not the RAM copy) -- isolates "is this board's
            // ROM correctly served at the address expansion.library
            // would have read it from" from "did expansion.library go on
            // to run DiagEntry", the two different things a zero
            // diag_copy_addr below could mean.
            let diag_area_addr = cd.board_addr + u32::from(cd.er_init_diag_vec);
            let live_header: Vec<u8> = (0..8).map(|i| bus.read_byte(diag_area_addr + i)).collect();
            out.push_str(&format!(
                "  DiagArea live header at {diag_area_addr:#010x} (board_addr + er_InitDiagVec): {live_header:02x?}\n"
            ));
            match find_diag_rom_copy_by_signature(bus) {
                Some((addr, marker)) => out.push_str(&format!(
                    "  DiagArea RAM copy found by signature scan at {addr:#010x} \
                     (independent of cd_Rom.er_Reserved0c) -- DiagMarker there: {marker:#010x}\n"
                )),
                None => out.push_str(
                    "  no DiagArea RAM copy found anywhere in chip RAM by signature scan \
                     -- expansion.library never copied it at all\n",
                ),
            }
            if cd.diag_copy_addr == 0 {
                out.push_str(
                    "  ConfigDev.cd_Rom.er_Reserved0c..0f: 0 (DiagEntry returned failure, \
                     was never called, or this Kickstart no longer uses this field the way \
                     RKRM 3rd ed. describes -- see the signature-scan line above instead)\n",
                );
            } else {
                let marker =
                    bus.read_long(cd.diag_copy_addr + machine_core::hostblk::DIAG_MARKER_OFFSET);
                out.push_str(&format!(
                    "  DiagArea RAM copy kept at {:#010x} (DiagEntry returned success)\n",
                    cd.diag_copy_addr
                ));
                out.push_str(&format!(
                    "  DiagMarker {marker:#010x}{}\n",
                    if marker == machine_core::hostblk::PROTOCOL_VERSION {
                        " == hostblk::PROTOCOL_VERSION -- DiagEntry read the board's own VERSION register"
                    } else {
                        " -- unexpected value; DiagEntry did not run as written, or something overwrote it"
                    }
                ));
            }
        }
        None => out.push_str(
            "  no ConfigDev found in chip RAM for this board -- expansion.library never \
             configured it (or the search above needs widening; see this function's doc comment)\n",
        ),
    }

    match exec_base
        .and_then(|eb| find_library_base(bus, eb + execbase::LIB_LIST, "expansion.library"))
    {
        Some(expansion_base) => {
            let boot_nodes = walk_boot_nodes(bus, expansion_base + EXPANSIONBASE_MOUNT_LIST);
            out.push_str(&format!(
                "  ExpansionBase at {expansion_base:#010x}  MountList boot nodes: {}\n",
                boot_nodes.len()
            ));
            for (addr, dev_node) in &boot_nodes {
                out.push_str(&format!(
                    "    BootNode {addr:#010x}  bn_DeviceNode {dev_node:#010x}\n"
                ));
            }
        }
        None => out.push_str(
            "  ExpansionBase not found (expansion.library not linked into ExecBase->LibList yet)\n",
        ),
    }

    out
}

/// Format one `MemList` entry, decoding the `MEMF_*` attribute bits that
/// matter for telling chip RAM apart from an adopted fast-RAM board --
/// `--fast-ram`'s whole verification question (module docs on
/// [`walk_mem_list`]).
fn mem_list_line(m: &MemListEntry) -> String {
    let mut kinds = Vec::new();
    if m.attributes & MEMF_CHIP != 0 {
        kinds.push("CHIP");
    }
    if m.attributes & MEMF_FAST != 0 {
        kinds.push("FAST");
    }
    if m.attributes & MEMF_PUBLIC != 0 {
        kinds.push("PUBLIC");
    }
    if m.attributes & MEMF_LOCAL != 0 {
        kinds.push("LOCAL");
    }
    let kinds = if kinds.is_empty() {
        "none recognised".to_string()
    } else {
        kinds.join("|")
    };
    format!(
        "{:#010x}  {:<20} attrs {:#06x} ({kinds})  {:#010x}-{:#010x} ({} bytes)  free {} bytes",
        m.address,
        m.name,
        m.attributes,
        m.lower,
        m.upper,
        m.upper.wrapping_sub(m.lower),
        m.free,
    )
}

fn task_line(task: Option<&TaskEntry>) -> String {
    match task {
        None => "(none)".to_string(),
        Some(t) => format!(
            "{:#010x}  pri {:>4}  state {}  SigWait {:#010x}  {}",
            t.address, t.priority, t.state, t.sig_wait, t.name
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use machine_core::CHIP_RAM_SIZE;

    fn new_bus(ram: &mut Box<[u8; CHIP_RAM_SIZE]>) -> MachineBus<'_> {
        let mut bus = MachineBus::new(ram, &[]);
        // Every fixture below builds a post-boot guest state, i.e. the
        // overlay has already been cleared and chip RAM answers for low
        // memory -- matching how a real ExecBase pointer at $4 is only
        // meaningful once OVL is down.
        bus.write_byte(0x00BF_E001, 0x00);
        bus
    }

    fn boxed_ram() -> Box<[u8; CHIP_RAM_SIZE]> {
        vec![0u8; CHIP_RAM_SIZE]
            .into_boxed_slice()
            .try_into()
            .unwrap_or_else(|_| unreachable!())
    }

    #[test]
    fn absent_exec_base_reports_none_without_panicking() {
        let mut ram = boxed_ram();
        let mut bus = new_bus(&mut ram);
        // $4 is zero (never written) -- the guest never got far enough.
        let report = inspect(&mut bus);
        assert!(report.exec_base.is_none());
        assert!(format_report(&report).contains("no plausible ExecBase"));
    }

    #[test]
    fn garbage_pointer_at_four_is_rejected() {
        let mut ram = boxed_ram();
        let mut bus = new_bus(&mut ram);
        // A plausible-looking but wrong pointer: word-aligned, in range,
        // but nothing at it looks like a Library node (ln_Type stays 0 /
        // NT_UNKNOWN since the RAM is zeroed).
        bus.write_long(0x0000_0004, 0x0001_0000);
        let report = inspect(&mut bus);
        assert!(report.exec_base.is_none());
    }

    /// Build a minimal but well-formed `ExecBase` at `base` and return it,
    /// so each test below only has to poke the one or two fields it cares
    /// about.
    fn plant_exec_base(bus: &mut MachineBus, base: u32) {
        bus.write_long(0x0000_0004, base);
        bus.write_byte(base + execbase::LIB_NODE_LN_TYPE, NT_LIBRARY);
        bus.write_word(base + execbase::LIB_VERSION, 47);
        bus.write_word(base + execbase::LIB_REVISION, 115);
        bus.write_word(base + execbase::SOFT_VER, 47);
        bus.write_long(base + execbase::MAX_LOC_MEM, CHIP_RAM_SIZE as u32);
        bus.write_word(
            base + execbase::ATTN_FLAGS,
            (1 << 3) | (1 << 6), // AFF_68040 | AFF_FPU40
        );
        bus.write_long(base + execbase::CHK_BASE, !base);
    }

    #[test]
    fn well_formed_exec_base_is_accepted_and_decoded() {
        let mut ram = boxed_ram();
        let mut bus = new_bus(&mut ram);
        let base = 0x0004_0000;
        plant_exec_base(&mut bus, base);

        let report = inspect(&mut bus);
        assert_eq!(report.exec_base, Some(base));
        assert_eq!(report.lib_version, 47);
        assert_eq!(report.lib_revision, 115);
        assert_eq!(report.max_loc_mem, CHIP_RAM_SIZE as u32);
        assert!(report.chk_base_ok);
        assert_eq!(
            report.attn_flags & ((1 << 3) | (1 << 6)),
            (1 << 3) | (1 << 6)
        );

        let text = format_report(&report);
        assert!(text.contains("68040"));
        assert!(text.contains("FPU40"));
    }

    #[test]
    fn resident_module_walk_stops_at_null_and_bad_matchword() {
        let mut ram = boxed_ram();
        let mut bus = new_bus(&mut ram);
        let base = 0x0004_0000;
        plant_exec_base(&mut bus, base);

        let array = base + 0x1000;
        let entry1 = base + 0x2000;
        let entry2 = base + 0x2100;
        let name1 = base + 0x3000;
        let name2 = base + 0x3100;

        // entry1: a genuine-looking Resident.
        bus.write_word(entry1 + resident::RT_MATCH_WORD, resident::MATCH_WORD);
        bus.write_byte(entry1 + resident::RT_VERSION, 47);
        bus.write_byte(entry1 + resident::RT_TYPE, NT_LIBRARY);
        bus.write_byte(entry1 + resident::RT_PRI, 106);
        bus.write_long(entry1 + resident::RT_NAME, name1);
        for (i, b) in b"expansion.library".iter().enumerate() {
            bus.write_byte(name1 + i as u32, *b);
        }

        // entry2: wrong match word -- must be treated as end-of-list, not
        // reported as a module.
        bus.write_word(entry2 + resident::RT_MATCH_WORD, 0x1234);
        bus.write_long(entry2 + resident::RT_NAME, name2);

        bus.write_long(array, entry1);
        bus.write_long(array + 4, entry2);
        bus.write_long(array + 8, 0);
        bus.write_long(base + execbase::RES_MODULES, array);

        let report = inspect(&mut bus);
        assert_eq!(report.resident_modules.len(), 1);
        assert_eq!(report.resident_modules[0].name, "expansion.library");
        assert_eq!(report.resident_modules[0].priority, 106);
    }

    #[test]
    fn alert_decoding_names_a_known_subsystem() {
        assert_eq!(decode_alert(0), "none");
        let text = decode_alert(0x8100_0005); // AN_MemCorrupt, DeadEnd
        assert!(text.contains("DeadEnd"));
        assert!(text.contains("AN_MemCorrupt"));
    }

    /// Regression: on a real Kickstart 3.2.2 A1200 boot (this module's
    /// motivating case), `LastAlert[0]` reads back `0xFFFFFFFF` even
    /// though the machine is genuinely idle and healthy -- it is
    /// leftover chip-RAM fill from Kickstart's own memory-sizing probe,
    /// never actually written by `Alert()`. Decoding it as a real "DeadEnd,
    /// unknown subsystem" Guru would be exactly the "confident nonsense"
    /// this module exists to avoid.
    #[test]
    fn all_ones_last_alert_is_reported_as_uninitialised_not_a_guru() {
        let text = decode_alert(0xFFFF_FFFF);
        assert!(text.contains("not a real alert"));
        assert!(!text.contains("DeadEnd"));
    }

    #[test]
    fn task_list_walk_finds_a_task_and_stops_at_the_tail_sentinel() {
        let mut ram = boxed_ram();
        let mut bus = new_bus(&mut ram);
        let base = 0x0004_0000;
        plant_exec_base(&mut bus, base);

        let list_addr = base + execbase::TASK_READY;
        let task_addr = base + 0x4000;
        let name_addr = base + 0x4100;

        // Standard empty-list-then-one-node wiring: lh_Head points at the
        // task, the task's ln_Succ points back at &lh_Tail (which is the
        // list header + 4, immediately after lh_Head), and that slot
        // holds 0 (lh_Tail is always NULL) -- terminating the walk.
        bus.write_long(list_addr + list::LH_HEAD, task_addr);
        bus.write_long(task_addr + node::LN_SUCC, list_addr + 4);
        bus.write_long(list_addr + 4, 0);

        bus.write_byte(task_addr + node::LN_TYPE, NT_TASK);
        bus.write_byte(task_addr + node::LN_PRI, 0u8.wrapping_sub(1)); // -1, common Kickstart idle pri encoding isn't tested here, just round-trip
        bus.write_long(task_addr + node::LN_NAME, name_addr);
        for (i, b) in b"input.device".iter().enumerate() {
            bus.write_byte(name_addr + i as u32, *b);
        }
        bus.write_long(task_addr + task::TC_SIG_WAIT, 0x0000_0100); // SIGF_DOS

        let report = inspect(&mut bus);
        assert_eq!(report.task_ready.len(), 1);
        assert_eq!(report.task_ready[0].name, "input.device");
        assert_eq!(report.task_ready[0].sig_wait, 0x0000_0100);
    }

    /// `--fast-ram`'s whole verification question, at unit-test scale:
    /// two `MemHeader`s on `ExecBase->MemList` (chip, then a stand-in fast
    /// region) must both come back, with `MEMF_FAST` distinguishing the
    /// second from the first -- the same list shape a real Kickstart that
    /// adopted a `--fast-ram` board would leave behind.
    #[test]
    fn mem_list_walk_finds_chip_and_fast_regions() {
        let mut ram = boxed_ram();
        let mut bus = new_bus(&mut ram);
        let base = 0x0004_0000;
        plant_exec_base(&mut bus, base);

        let list_addr = base + execbase::MEM_LIST;
        let chip_hdr = base + 0x5000;
        let fast_hdr = base + 0x5100;
        let chip_name = base + 0x5200;
        let fast_name = base + 0x5210;

        // lh_Head -> chip_hdr -> fast_hdr -> &lh_Tail (NULL), the same
        // two-real-node chain shape `task_list_walk_...` above builds.
        bus.write_long(list_addr + list::LH_HEAD, chip_hdr);
        bus.write_long(chip_hdr + node::LN_SUCC, fast_hdr);
        bus.write_long(fast_hdr + node::LN_SUCC, list_addr + 4);
        bus.write_long(list_addr + 4, 0);

        bus.write_long(chip_hdr + node::LN_NAME, chip_name);
        for (i, b) in b"chip memory".iter().enumerate() {
            bus.write_byte(chip_name + i as u32, *b);
        }
        bus.write_word(chip_hdr + memheader::MH_ATTRIBUTES, MEMF_CHIP | MEMF_PUBLIC);
        bus.write_long(chip_hdr + memheader::MH_LOWER, 0);
        bus.write_long(chip_hdr + memheader::MH_UPPER, CHIP_RAM_SIZE as u32);
        bus.write_long(chip_hdr + memheader::MH_FREE, 0x0010_0000);

        let fast_base = 0x4000_0000u32;
        let fast_size = 0x0800_0000u32; // 128 MB
        bus.write_long(fast_hdr + node::LN_NAME, fast_name);
        for (i, b) in b"fast memory".iter().enumerate() {
            bus.write_byte(fast_name + i as u32, *b);
        }
        bus.write_word(fast_hdr + memheader::MH_ATTRIBUTES, MEMF_FAST | MEMF_PUBLIC);
        bus.write_long(fast_hdr + memheader::MH_LOWER, fast_base);
        bus.write_long(fast_hdr + memheader::MH_UPPER, fast_base + fast_size);
        bus.write_long(fast_hdr + memheader::MH_FREE, fast_size);

        let report = inspect(&mut bus);
        assert_eq!(report.mem_list.len(), 2);
        assert_eq!(report.mem_list[0].name, "chip memory");
        assert_eq!(report.mem_list[0].attributes & MEMF_FAST, 0);
        assert_eq!(report.mem_list[1].name, "fast memory");
        assert_eq!(report.mem_list[1].attributes & MEMF_FAST, MEMF_FAST);
        assert_eq!(
            report.mem_list[1].upper - report.mem_list[1].lower,
            fast_size,
            "adopted size matches what the board declared"
        );

        let text = format_report(&report);
        assert!(text.contains("fast memory"));
        assert!(text.contains("FAST"));
    }
}
