//! Diagnostics command - outputs system information for bug reports.

use crate::error::CliError;
use xearthlayer::diagnostics::SystemReport;

/// Run the diagnostics command.
pub fn run() -> Result<(), CliError> {
    let report = SystemReport::collect();
    println!("{}", report);
    Ok(())
}
