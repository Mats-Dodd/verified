//! The `verified` command-line interface.

use std::io::{self, Write as _};

fn main() -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "verified {}", verified::version())
}
