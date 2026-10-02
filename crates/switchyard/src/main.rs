//! The `switchyard` command line. See `docs/DESIGN.md` section 13 and the
//! library target of this package, which holds all of the logic.

use std::process::ExitCode;

fn main() -> ExitCode {
    switchyard::run()
}
