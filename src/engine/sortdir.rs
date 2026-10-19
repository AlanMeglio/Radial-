//! Sorting the loose files of a folder into subfolders, by type or by month. Pure planning on top of the rename
//! engine's all-or-nothing, never-overwrite `apply` and its undo log.
//!
//! Only the files directly inside the folder are touched: nothing is read recursively, subfolders stay where they
//! are, hidden and system files, shortcuts and half-downloaded files are left alone, and a file whose type is not
//! recognised stays put rather than going into a catch-all folder.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::error::{Error, Result};
use super::rename::{local_parts, Item, Status};

/// How the files are grouped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    ByType,
    ByMonth,
}

/// The kinds of file a folder is sorted into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    Images,
    Documents,
    Spreadsheets,
    Presentations,
    Archives,
    Audio,
    Video,
    Programs,
}

impl Group {
    #[cfg(test)]
    pub const ALL: [Group; 8] = [
        Group::Images,
        Group::Documents,
        Group::Spreadsheets,
        Group::Presentations,
        Group::Archives,
        Group::Audio,
        Group::Video,
        Group::Programs,
    ];

    /// Key of the localized folder name (`config::STRINGS`).
    pub fn key(self) -> &'static str {
        match self {
            Group::Images => "grp-images",
            Group::Documents => "grp-documents",
            Group::Spreadsheets => "grp-spreadsheets",
            Group::Presentations => "grp-presentations",
            Group::Archives => "grp-archives",
            Group::Audio => "grp-audio",
            Group::Video => "grp-video",
            Group::Programs => "grp-programs",
        }
    }

    /// The group an extension belongs to, if it is a common one.
    pub fn of_extension(ext: &str) -> Option<Group> {
        let ext = ext.to_ascii_lowercase();
        Some(match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "heif" | "avif" | "svg"
            | "ico" | "raw" | "cr2" | "nef" | "dng" => Group::Images,
            "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "epub" | "html" | "htm" => Group::Documents,
            "xls" | "xlsx" | "xlsm" | "csv" | "tsv" | "ods" => Group::Spreadsheets,
            "ppt" | "pptx" | "odp" | "key" => Group::Presentations,
            "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "iso" => Group::Archives,
            "mp3" | "wav" | "ogg" | "oga" | "opus" | "flac" | "m4a" | "aac" | "wma" | "mid" | "midi" => Group::Audio,
            "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg" => Group::Video,
            "exe" | "msi" | "msix" | "appx" | "bat" | "cmd" | "ps1" => Group::Programs,
            _ => return None,
        })
    }
}

/// What sorting a folder would do.
#[derive(Debug, Default)]
pub struct Plan {
    /// One `Ok` item per file that moves; a file whose target name is already taken is `Conflict` and is not moved.
    pub moves: Vec<Item>,
    /// How many files go to each destination folder, in folder-name order.
    pub groups: Vec<(String, usize)>,
    /// Files left where they are on purpose (unknown type, hidden, system, shortcuts, partial downloads).
    pub left: usize,
}

/// Extensions of files that are still being written or are not really files of the user's.
fn is_transient(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    name.starts_with('.')
        || [".crdownload", ".part", ".tmp", ".partial", ".lnk", ".url", ".ini", ".lock", ".download"]
            .iter()
            .any(|e| lower.ends_with(e))
}

fn hidden_or_system(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    meta.file_attributes() & (0x2 | 0x4) != 0 // FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM
}

/// The destination folder name of a file, or `None` to leave it where it is.
fn folder_for(
    path: &Path,
    meta: &std::fs::Metadata,
    mode: Mode,
    name_of_group: &dyn Fn(Group) -> String,
) -> Option<String> {
    match mode {
        Mode::ByType => {
            let group = Group::of_extension(path.extension()?.to_str()?)?;
            Some(name_of_group(group))
        }
        Mode::ByMonth => {
            let when: SystemTime = meta.modified().ok()?;
            let p = local_parts(when)?;
            Some(format!("{:04}-{:02}", p.y, p.mo))
        }
    }
}

/// Plans sorting `dir`. `name_of_group` supplies the (localized) folder name of each group.
pub fn plan(dir: &Path, mode: Mode, name_of_group: &dyn Fn(Group) -> String) -> Result<Plan> {
    let listing = std::fs::read_dir(dir).map_err(|e| Error::Io(format!("{}: {e}", dir.display())))?;
    let mut plan = Plan::default();
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for entry in listing.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if !meta.is_file() {
            continue; // subfolders and links stay
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_transient(&name) || hidden_or_system(&meta) {
            plan.left += 1;
            continue;
        }
        let Some(folder) = folder_for(&path, &meta, mode, name_of_group) else {
            plan.left += 1;
            continue;
        };
        let target_dir = dir.join(&folder);
        // A name taken by a file where a folder should be, or a same-named file already in the folder, is a conflict.
        let blocked = std::fs::symlink_metadata(&target_dir).is_ok_and(|m| !m.is_dir());
        let to = target_dir.join(&name);
        let status = if blocked || std::fs::symlink_metadata(&to).is_ok() {
            Status::Conflict("exists".into())
        } else {
            *counts.entry(folder).or_default() += 1;
            Status::Ok
        };
        plan.moves.push(Item { from: path, to, status });
    }
    plan.moves.sort_by(|a, b| a.from.cmp(&b.from));
    plan.groups = counts.into_iter().collect();
    Ok(plan)
}

/// The destination folders `plan` needs, created.
pub fn make_folders(plan: &Plan) -> Result<Vec<PathBuf>> {
    let mut made = Vec::new();
    for item in plan.moves.iter().filter(|i| i.status == Status::Ok) {
        let Some(dir) = item.to.parent() else { continue };
        if !dir.exists() {
            std::fs::create_dir(dir).map_err(|e| Error::Io(format!("{}: {e}", dir.display())))?;
            made.push(dir.to_path_buf());
        }
    }
    Ok(made)
}

/// Moves the files of `plan` (only the non-conflicting ones). Creates the folders first and removes the ones it
/// made again when the move fails, so a failure leaves the folder as it was.
pub fn apply(plan: &Plan) -> Result<super::rename::Undo> {
    let ok: Vec<Item> = plan.moves.iter().filter(|i| i.status == Status::Ok).cloned().collect();
    if ok.is_empty() {
        return Err(Error::Unsupported("nothing to sort".into()));
    }
    let made = make_folders(plan)?;
    match super::rename::apply(&ok) {
        Ok(mut undo) => {
            undo.created = made;
            Ok(undo)
        }
        Err(e) => {
            for d in made.iter().rev() {
                let _ = std::fs::remove_dir(d);
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("radial-sort-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn english(g: Group) -> String {
        match g {
            Group::Images => "Images",
            Group::Documents => "Documents",
            Group::Spreadsheets => "Spreadsheets",
            Group::Presentations => "Presentations",
            Group::Archives => "Archives",
            Group::Audio => "Audio",
            Group::Video => "Video",
            Group::Programs => "Programs",
        }
        .into()
    }

    fn touch(d: &Path, n: &str) {
        std::fs::write(d.join(n), n).unwrap();
    }

    #[test]
    fn extensions_map_to_their_group_case_insensitively() {
        assert_eq!(Group::of_extension("PNG"), Some(Group::Images));
        assert_eq!(Group::of_extension("xlsx"), Some(Group::Spreadsheets));
        assert_eq!(Group::of_extension("Mp4"), Some(Group::Video));
        assert_eq!(Group::of_extension("xyz"), None);
        for g in Group::ALL {
            assert!(!crate::config::tr(crate::config::Lang::En, g.key()).is_empty(), "{g:?}");
            assert!(!crate::config::tr(crate::config::Lang::Es, g.key()).is_empty(), "{g:?}");
        }
    }

    #[test]
    fn a_folder_is_sorted_by_type_and_what_is_not_a_plain_known_file_stays() {
        let d = temp("type");
        for n in ["a.png", "b.JPG", "c.pdf", "d.xlsx", "e.zip", "f.mp3", "g.mp4", "h.exe", "notes.txt"] {
            touch(&d, n);
        }
        for n in ["mystery.xyz", "movie.mp4.crdownload", "link.lnk", ".hidden.png", "desktop.ini"] {
            touch(&d, n);
        }
        std::fs::create_dir(d.join("Old")).unwrap();
        touch(&d.join("Old"), "inside.png");
        let plan = plan(&d, Mode::ByType, &english).unwrap();
        assert_eq!(plan.moves.len(), 9);
        assert_eq!(plan.left, 5, "unknown, partial download, shortcut, hidden and ini stay");
        assert_eq!(
            plan.groups,
            vec![
                ("Archives".to_string(), 1),
                ("Audio".to_string(), 1),
                ("Documents".to_string(), 2),
                ("Images".to_string(), 2),
                ("Programs".to_string(), 1),
                ("Spreadsheets".to_string(), 1),
                ("Video".to_string(), 1),
            ]
        );
        let undo = apply(&plan).unwrap();
        assert!(d.join("Images").join("a.png").exists() && d.join("Images").join("b.JPG").exists());
        assert!(d.join("Documents").join("notes.txt").exists());
        assert!(d.join("mystery.xyz").exists() && d.join("Old").join("inside.png").exists());
        assert!(!d.join("a.png").exists());
        // Undo puts everything back and the folders sorting made can be removed again.
        super::super::rename::undo(&undo).unwrap();
        assert!(d.join("a.png").exists() && d.join("c.pdf").exists());
        assert!(!d.join("Images").join("a.png").exists());
        assert!(!d.join("Images").exists() && !d.join("Documents").exists(), "the folders sorting made are gone again");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_is_sorted_by_the_month_the_file_was_last_changed() {
        let d = temp("month");
        touch(&d, "old.txt");
        touch(&d, "new.txt");
        let when = |y: i32, m: u32| {
            use std::time::{Duration, UNIX_EPOCH};
            let days = (y - 1970) as u64 * 365 + (m as u64 - 1) * 30 + 15;
            UNIX_EPOCH + Duration::from_secs(days * 86_400 + 43_200)
        };
        std::fs::File::options().write(true).open(d.join("old.txt")).unwrap().set_modified(when(2021, 3)).unwrap();
        std::fs::File::options().write(true).open(d.join("new.txt")).unwrap().set_modified(when(2023, 8)).unwrap();
        let plan = plan(&d, Mode::ByMonth, &english).unwrap();
        let names: Vec<String> = plan.groups.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names[0].starts_with("2021-") && names[1].starts_with("2023-"), "{names:?}");
        apply(&plan).unwrap();
        assert!(d.join(&names[0]).join("old.txt").exists() && d.join(&names[1]).join("new.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_name_already_taken_in_the_target_folder_is_left_alone_and_never_overwritten() {
        let d = temp("clash");
        touch(&d, "a.png");
        touch(&d, "b.png");
        std::fs::create_dir(d.join("Images")).unwrap();
        std::fs::write(d.join("Images").join("a.png"), "the one already there").unwrap();
        let plan = plan(&d, Mode::ByType, &english).unwrap();
        let a = plan.moves.iter().find(|i| i.old_name() == "a.png").unwrap();
        assert_eq!(a.status, Status::Conflict("exists".into()));
        assert_eq!(plan.groups, vec![("Images".to_string(), 1)]);
        apply(&plan).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("Images").join("a.png")).unwrap(), "the one already there");
        assert!(d.join("a.png").exists(), "the clashing file stays");
        assert!(d.join("Images").join("b.png").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_where_a_folder_should_go_blocks_only_that_group() {
        let d = temp("blocked");
        touch(&d, "a.png");
        touch(&d, "b.pdf");
        touch(&d, "Images"); // a plain file named like the folder
        let plan = plan(&d, Mode::ByType, &english).unwrap();
        assert_eq!(
            plan.moves.iter().find(|i| i.old_name() == "a.png").unwrap().status,
            Status::Conflict("exists".into())
        );
        apply(&plan).unwrap();
        assert!(d.join("Documents").join("b.pdf").exists() && d.join("a.png").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn nothing_to_sort_is_an_error_and_makes_no_folders() {
        let d = temp("empty");
        touch(&d, "mystery.xyz");
        let plan = plan(&d, Mode::ByType, &english).unwrap();
        assert!(matches!(apply(&plan), Err(Error::Unsupported(_))));
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        assert!(plan.moves.is_empty() && plan.groups.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failed_move_removes_the_folders_it_made() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = temp("rollback");
        touch(&d, "a.png");
        touch(&d, "b.pdf");
        let plan = plan(&d, Mode::ByType, &english).unwrap();
        // Holding one file open without sharing makes its move fail halfway through.
        let _held = std::fs::OpenOptions::new().read(true).share_mode(0).open(d.join("b.pdf")).unwrap();
        assert!(apply(&plan).is_err());
        assert!(d.join("a.png").exists() && d.join("b.pdf").exists(), "both files are where they were");
        assert!(!d.join("Images").exists() && !d.join("Documents").exists(), "the folders it made are gone");
        drop(_held);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_folder_is_an_io_error() {
        assert!(matches!(plan(Path::new(r"C:\no\such\folder\radial"), Mode::ByType, &english), Err(Error::Io(_))));
    }
}
