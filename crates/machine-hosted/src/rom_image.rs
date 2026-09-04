//! Loading ROM images from disk and reporting what `machine_core::rom`
//! could determine about them.

use std::fs;
use std::io;
use std::path::Path;

use machine_core::rom;

use crate::console::Console;

/// Read a ROM image whole. `MachineBus` mirrors undersized images and
/// simply ignores address bits beyond an oversized one, so no padding or
/// truncation is needed here.
pub fn load(path: &Path) -> io::Result<Vec<u8>> {
    fs::read(path)
}

/// Run `machine_core::rom::identify` over an image and log the result.
///
/// `identify` may still be a stub returning `None` while a concurrent
/// worker fills it in (see `crates/machine-core/src/rom.rs`); that is not
/// an error condition here, just a less informative log line, since
/// identification is advisory and never gates booting.
pub fn report_identify(console: &mut Console, label: &str, image: &[u8]) {
    match rom::identify(image) {
        Some(info) => {
            console.diag(&format!(
                "{label} ROM: {:?} rev {}.{}, exec {}.{}, {} bytes, checksum {}, boot_pc {:#010x}",
                info.kind,
                info.rev.0,
                info.rev.1,
                info.exec_rev.0,
                info.exec_rev.1,
                info.size,
                if info.checksum_ok { "ok" } else { "FAILED" },
                info.boot_pc,
            ));
        }
        None => {
            console.diag(&format!(
                "{label} ROM: unidentified ({} bytes) -- rom::identify returned None \
                 (still a stub, or a header this machine doesn't recognise); booting anyway",
                image.len()
            ));
        }
    }
}
