# `pktport` register and wire-format contract

The DosPacket transport card of ADR 0004: the guest's thin handler stub
forwards AmigaDOS packets over this card, and a host-side filesystem
service (built on the published `amiga-rdb` and `amiga-ffs` crates)
answers them. This document is the contract between the three parties —
the `machine-core` card model, the host backend behind it, and the 68k
handler stub — the same role `hostblk-protocol.md` plays for block
storage.

Everything here follows `hostblk`'s conventions unless stated: 4-byte
register slots with the live byte at offset +3, doorbell writes that do
no work themselves (§ deferred completion), INT2 completion, guest
addresses treated as hostile input throughout.

## 1. Identity

Zorro II, 64 KB register window, `MANUFACTURER 0x07DB`, `PRODUCT 5`.
Small on purpose: no data crosses the register window. Names, buffers
and `FileInfoBlock`s are read and written directly in guest RAM through
`GuestMemory`, exactly as `hostblk` moves sector data — the window
carries only registers.

## 2. The request descriptor

One request is one 64-byte descriptor in guest RAM, allocated by the
stub, 4-byte aligned:

| offset | field | who writes | meaning |
|---|---|---|---|
| 0x00 | `ACTION` | guest | the DosPacket `dp_Type` |
| 0x04 | `ARG1` | guest | `dp_Arg1`, verbatim conventions per action (§5) |
| 0x08 | `ARG2` | guest | `dp_Arg2` |
| 0x0C | `ARG3` | guest | `dp_Arg3` |
| 0x10 | `ARG4` | guest | `dp_Arg4` |
| 0x14 | `ARG5` | guest | `dp_Arg5` |
| 0x18 | `ARG6` | guest | `dp_Arg6` |
| 0x1C | `ARG7` | guest | `dp_Arg7` |
| 0x20 | `RES1` | host | `dp_Res1` |
| 0x24 | `RES2` | host | `dp_Res2` (AmigaDOS error code on failure) |
| 0x28 | `STATUS` | host | 0 = pending, 1 = complete. Written **after** `RES1`/`RES2`. |
| 0x2C..0x40 | reserved | — | guest writes 0; host ignores |

All fields big-endian, as the guest naturally writes them. The host
writes `RES1`/`RES2` first, `STATUS` last, and raises INT2 only after
`STATUS`; the stub may therefore trust `RES1`/`RES2` the moment it
observes `STATUS == 1`, with or without the interrupt.

BPTR arguments are BPTRs on the wire — the host resolves `addr << 2`
itself. The descriptor carries `dp_Arg` values *verbatim* so the stub
stays thin; the one deliberate divergence is locks and file handles
(§4).

## 3. Register map

| offset | name | access | meaning |
|---|---|---|---|
| 0x00 | `VERSION` | R | protocol version, 1. Check it, refuse what you don't know. |
| 0x04 | `CAPACITY` | R | outstanding-request capacity. 1 in this version — **read it, do not hardcode it** (`hostblk::SUBMIT_CAPACITY` reasoning). |
| 0x08 | `REQ_PTR` | W (u32, byte-per-slot like `hostblk`'s pointer registers) | guest address of the descriptor |
| 0x0C | `DOORBELL` | W | submit the descriptor at `REQ_PTR`. Latches; work happens at tick time. |
| 0x10 | `INT_STATUS` | R / W1C | bit 0: completion pending. Write-1-to-clear, **gated**: ignored while a completed request has not had its `STATUS` observed-and-cleared semantics honoured — same stranding argument as `input`'s gate. |
| 0x14 | `INT_ENABLE` | R/W | bit 0 enables INT2 delivery |
| 0x18 | `VOL_COUNT` | R | volumes served; 1 in this version |

A `DOORBELL` write while a request is in flight (`CAPACITY` exhausted)
is recorded in an overflow counter and otherwise ignored — the same
"silently ignored is never silent" posture as `hostblk`'s
`SUBMIT_OVERFLOW`; the register can be added to the map when a deeper
queue is.

## 4. Locks and file handles: host-owned opaque handles

Where a DosPacket convention carries a lock (`struct FileLock` BPTR) or
a file handle's `fh_Arg1`, the wire carries a **host-assigned opaque
non-zero u32 handle** instead. Zero is the null lock (root of the
volume), as in DOS itself.

The stub owns the guest-side `FileLock` structures DOS expects to
inspect, storing the host handle in `fl_Key`; the host never sees or
chases guest `FileLock` pointers. This is the one place the descriptor
diverges from raw `dp_Arg` values, and it is what keeps guest pointers
out of the host's lock table entirely — the host validates a handle by
lookup in its own table, not by trusting a guest address.

Handles are per-machine-instance, never reused within a run, and all
invalidated at reset.

## 5. Supported actions, version 1

Arg conventions are the AmigaDOS ones (RKM: `dos/dosextens.h`), with
locks/handles as in §4. Everything else → `RES1 = DOSFALSE`,
`RES2 = ERROR_ACTION_NOT_KNOWN` (209).

| action | value | args (wire) | backend operation |
|---|---|---|---|
| `ACTION_LOCATE_OBJECT` | 8 | lock, name BSTR, mode | resolve path → new lock |
| `ACTION_FREE_LOCK` | 9 | lock | drop handle |
| `ACTION_COPY_DIR` | 19 | lock | duplicate lock |
| `ACTION_PARENT` | 29 | lock | parent dir lock (0 at root) |
| `ACTION_SAME_LOCK` | 40 | lock, lock | compare targets |
| `ACTION_EXAMINE_OBJECT` | 23 | lock, FIB BPTR | fill `FileInfoBlock` in guest RAM |
| `ACTION_EXAMINE_NEXT` | 24 | lock, FIB BPTR | directory iteration via `fib_DiskKey` |
| `ACTION_INFO` | 26 | lock, InfoData BPTR | fill `InfoData` |
| `ACTION_DISK_INFO` | 25 | InfoData BPTR | fill `InfoData` |
| `ACTION_FINDINPUT` | 1005 | handle*, lock, name BSTR | open existing → handle in `RES2`† |
| `ACTION_FINDOUTPUT` | 1006 | handle*, lock, name BSTR | create/truncate → handle in `RES2`† |
| `ACTION_FINDUPDATE` | 1004 | handle*, lock, name BSTR | open or create → handle in `RES2`† |
| `ACTION_END` | 1007 | handle | close |
| `ACTION_READ` | 'R' (82) | handle, buffer APTR, length | read → actual in `RES1` |
| `ACTION_WRITE` | 'W' (87) | handle, buffer APTR, length | write → actual in `RES1` |
| `ACTION_SEEK` | 1008 | handle, position, mode | seek → old position in `RES1` |
| `ACTION_SET_FILE_SIZE` | 1022 | handle, offset, mode | truncate/extend → new size in `RES1` |
| `ACTION_CREATE_DIR` | 22 | lock, name BSTR | create → new lock |
| `ACTION_DELETE_OBJECT` | 16 | lock, name BSTR | delete |
| `ACTION_RENAME_OBJECT` | 17 | lock, name BSTR, lock, name BSTR | rename/move |
| `ACTION_SET_PROTECT` | 21 | –, lock, name BSTR, mask | protection bits |
| `ACTION_SET_COMMENT` | 28 | –, lock, name BSTR, comment BSTR | comment |
| `ACTION_SET_DATE` | 34 | –, lock, name BSTR, DateStamp APTR | dates |
| `ACTION_IS_FILESYSTEM` | 1027 | — | `RES1 = DOSTRUE` |
| `ACTION_FLUSH` | 27 | — | flush backend to durable storage |

† The `ACTION_FIND*` wire convention: the true DosPacket carries a
guest `FileHandle` BPTR in Arg1; the stub keeps that entirely on its
side and the host returns the new handle in `RES2` (which the real
packets don't use), the stub storing it into `fh_Arg1`. `ARG1` on the
wire is written 0. This is §4's principle applied: guest structure
pointers never cross the wire.

`ACTION_EXAMINE_NEXT` continuation state lives in the FIB's
`fib_DiskKey` as DOS intends, so no host-side iterator state can leak
or dangle; the backend re-seeks from the key each call.

## 6. Error mapping

`RES2` carries standard AmigaDOS error codes; the backend maps
`amiga-ffs` errors onto them. The ones version 1 must produce
correctly: 103 (no free store), 202 (object in use), 203 (exists),
204 (dir not found), 205 (object not found), 210 (invalid name),
212 (wrong type), 213 (not validated — from a backend that mounted a
volume `validate()` flagged), 214 (write protected), 216 (dir not
empty), 218 (not mounted), 221 (disk full), 222 (delete protected),
223 (write protected file), 224 (read protected), 225 (not a DOS
disk), 232 (no more entries — `EXAMINE_NEXT`'s end).

## 7. Hostility rules

Everything the guest hands over is hostile, exactly as in `hostblk`:

- Descriptor and every pointer arg resolved through `GuestMemory`
  (`ram_slice`/`ram_slice_mut`): unmapped, straddling, overflowing →
  the *request* fails (`RES2 = 209` for an unreadable descriptor is
  impossible — an unreadable descriptor cannot be completed, so it is
  dropped and counted, `hostblk`'s posture for an unparseable
  descriptor; an unreadable *buffer/name/FIB* fails the request with
  `ERROR_INVALID_COMPONENT_NAME` (210) for names, `ERROR_BAD_NUMBER`
  (115)-class refusal for buffers).
- BSTR length bytes clipped to what the containing guest RAM span can
  actually supply.
- Unknown handles → 205, never a table index.
- The card must never panic on any descriptor content; fuzzing the
  descriptor path is part of acceptance.

## 8. The backend seam

Defined in `machine-core` (no_std, no alloc — the *trait* is, the
implementation lives where allocation does):

```rust
/// One filesystem service behind the pktport card. `machine-hosted`
/// implements this over the published `amiga-rdb` + `amiga-ffs`
/// crates; tests implement it over fixtures.
pub trait PacketBackend {
    /// Execute one request. `args` are the descriptor's ARG1..ARG7,
    /// verbatim; guest memory access goes through `mem` under §7's
    /// rules. Returns `(res1, res2)`.
    fn execute(
        &mut self,
        action: u32,
        args: [u32; 7],
        mem: &mut dyn GuestMemory,
    ) -> (u32, u32);
}
```

The card owns request lifecycle (descriptor read, STATUS/INT2
ordering, overflow accounting); the backend owns semantics. The card
calls `execute` at tick time, never from the doorbell write.

## 9. What version 1 does not include

Multiple volumes per card, a request queue deeper than 1, ExAll,
notification (`ACTION_ADD_NOTIFY`), record locks, `ACTION_MAKE_LINK` /
`ACTION_READ_LINK`, 64-bit packets, the host-directory backend
(ADR 0004 backend 2 — same card, later), and the DiagArea boot path
(mounted-first per ADR 0004; the stub arrives via the boot volume).
Each is an additive extension: new actions, new registers, or a deeper
queue, all discoverable via `VERSION`/`CAPACITY`.
