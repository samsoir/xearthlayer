use std::path::Path;
use std::process::Command;

use super::types::DsfError;

/// Abstraction over DSF binary ↔ text conversion.
pub trait DsfTool: Send + Sync {
    fn check_available(&self) -> Result<(), DsfError>;
    fn decode(&self, dsf_path: &Path, text_path: &Path) -> Result<(), DsfError>;
    fn encode(&self, text_path: &Path, dsf_path: &Path) -> Result<(), DsfError>;
}

/// Production implementation that shells out to DSFTool CLI.
pub struct DsfToolRunner;

impl DsfTool for DsfToolRunner {
    fn check_available(&self) -> Result<(), DsfError> {
        match Command::new("DSFTool").arg("--version").output() {
            Ok(_) => Ok(()),
            Err(_) => Err(DsfError::ToolNotFound),
        }
    }

    fn decode(&self, dsf_path: &Path, text_path: &Path) -> Result<(), DsfError> {
        let output = Command::new("DSFTool")
            .arg("--dsf2text")
            .arg(dsf_path)
            .arg(text_path)
            .output()
            .map_err(|_| DsfError::ToolNotFound)?;

        if !output.status.success() {
            return Err(DsfError::DecodeFailed {
                path: dsf_path.to_path_buf(),
                reason: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }
        Ok(())
    }

    fn encode(&self, text_path: &Path, dsf_path: &Path) -> Result<(), DsfError> {
        let output = Command::new("DSFTool")
            .arg("--text2dsf")
            .arg(text_path)
            .arg(dsf_path)
            .output()
            .map_err(|_| DsfError::ToolNotFound)?;

        if !output.status.success() {
            return Err(DsfError::EncodeFailed {
                path: dsf_path.to_path_buf(),
                reason: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }
        Ok(())
    }
}
