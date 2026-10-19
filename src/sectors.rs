//! The user's own sectors: "move to Invoices", "copy to the USB stick", "open with the editor". They live in
//! `%APPDATA%\radial\sectors.txt` and show up as the ring called "mine" for the files they apply to.
#![allow(dead_code)] // the stub is replaced by the real module

use std::path::{Path, PathBuf};

/// What a user sector does with the dropped files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Moves the files into the folder `target`.
    MoveTo,
    /// Copies the files into the folder `target`.
    CopyTo,
    /// Opens the files with the program `target`.
    OpenWith,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sector {
    /// Shown on the sector, a few characters.
    pub label: String,
    pub kind: Kind,
    /// A folder for `MoveTo` and `CopyTo`, a program for `OpenWith`.
    pub target: PathBuf,
    /// Lower-case extensions without the dot the sector applies to; empty means every file (and folders).
    pub exts: Vec<String>,
}

impl Sector {
    /// Does this sector apply to every one of `files`?
    pub fn applies_to(&self, files: &[PathBuf]) -> bool {
        let _ = files;
        true
    }
}

/// The most sectors a user can have.
pub const MAX_SECTORS: usize = 8;

/// Reads `%APPDATA%\radial\sectors.txt`; a missing or damaged file is an empty list, never an error.
pub fn load() -> Vec<Sector> {
    Vec::new()
}

/// Writes the list atomically.
pub fn save(list: &[Sector]) -> std::io::Result<()> {
    let _ = list;
    Ok(())
}

/// Runs `sector` on `files` and returns the text of the notice, or why it could not.
pub fn run(sector: &Sector, files: &[PathBuf]) -> Result<String, String> {
    let _ = (sector, files, Path::new(""));
    Err("not implemented".into())
}
