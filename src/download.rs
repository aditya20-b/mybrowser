//! Saving a file the browser can't show (a zip, a PDF, an installer) to the downloads folder.
//!
//! Nothing here opens or runs what it saves. The name comes from the address, is cleaned of
//! anything that could point outside the folder, and never replaces a file that is already there.
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The longest file name we create, in characters (the extension is kept when it is cut).
const MAX_NAME_CHARS: usize = 100;

/// Where downloads go: `$MYBROWSER_DOWNLOADS` if set (used by the tests), else `~/Downloads`.
pub fn downloads_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MYBROWSER_DOWNLOADS") {
        return Some(PathBuf::from(dir));
    }
    if cfg!(test) {
        return None; // tests must never write into the real home folder
    }
    std::env::var_os("HOME").map(|h| Path::new(&h).join("Downloads"))
}

/// A file name that is safe to create: no separators, no control characters, nothing
/// that starts with a dot (hidden files) or is only dots, never empty.
pub fn safe_file_name(wanted: &str) -> String {
    let cleaned: String = wanted
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0') { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim().to_string();
    if cleaned.is_empty() {
        return "download".into();
    }
    if cleaned.chars().count() <= MAX_NAME_CHARS {
        return cleaned;
    }
    // too long: cut the stem, keep a short extension
    let (stem, ext) = match cleaned.rfind('.') {
        Some(i) if cleaned[i..].chars().count() <= 12 => (&cleaned[..i], &cleaned[i..]),
        _ => (cleaned.as_str(), ""),
    };
    let keep = MAX_NAME_CHARS.saturating_sub(ext.chars().count());
    format!("{}{}", stem.chars().take(keep).collect::<String>(), ext)
}

/// `name`, or `name (1)`, `name (2)` ... (before the extension) until a file by that name
/// can be created. The file is created exclusively, so an existing file (or a symlink
/// someone planted) is never written through or replaced.
pub fn save(dir: &Path, name: &str, body: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = safe_file_name(name);
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.clone(), String::new()),
    };
    for n in 0..1000u32 {
        let candidate = if n == 0 { name.clone() } else { format!("{stem} ({n}){ext}") };
        let path = dir.join(&candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(body)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "too many files with that name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mybrowser-dl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn file_names_are_cleaned_of_anything_dangerous() {
        assert_eq!(safe_file_name("report.pdf"), "report.pdf");
        assert_eq!(safe_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_file_name("a/b\\c:d"), "a_b_c_d");
        assert_eq!(safe_file_name(".bashrc"), "bashrc");
        assert_eq!(safe_file_name("..."), "download");
        assert_eq!(safe_file_name(""), "download");
        assert_eq!(safe_file_name("   "), "download");
        assert_eq!(safe_file_name("na\u{0}me\n.txt"), "na_me_.txt");
        assert_eq!(safe_file_name("é日本語.zip"), "é日本語.zip");
        let long = format!("{}.tar.gz", "x".repeat(500));
        let cut = safe_file_name(&long);
        assert!(cut.chars().count() <= MAX_NAME_CHARS && cut.ends_with(".gz"), "{cut}");
        let no_ext = safe_file_name(&"é".repeat(500));
        assert_eq!(no_ext.chars().count(), MAX_NAME_CHARS);
        // a long "extension" is just part of the name
        assert!(safe_file_name(&format!("a.{}", "y".repeat(300))).chars().count() <= MAX_NAME_CHARS);
    }

    #[test]
    fn saving_never_overwrites_and_numbers_duplicates() {
        let dir = scratch("save");
        let first = save(&dir, "data.bin", b"one").unwrap();
        let second = save(&dir, "data.bin", b"two").unwrap();
        let third = save(&dir, "data.bin", b"three").unwrap();
        assert_eq!(first.file_name().unwrap(), "data.bin");
        assert_eq!(second.file_name().unwrap(), "data (1).bin");
        assert_eq!(third.file_name().unwrap(), "data (2).bin");
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        // no extension, and a name that tries to leave the folder stays inside it
        let plain = save(&dir, "README", b"x").unwrap();
        let again = save(&dir, "README", b"y").unwrap();
        assert_eq!(again.file_name().unwrap(), "README (1)");
        assert_eq!(plain.parent(), Some(dir.as_path()));
        let sneaky = save(&dir, "../outside.txt", b"z").unwrap();
        assert_eq!(sneaky.parent(), Some(dir.as_path()));
        assert!(!dir.parent().unwrap().join("outside.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_planted_symlink_is_not_written_through() {
        #[cfg(unix)]
        {
            let dir = scratch("symlink");
            std::fs::create_dir_all(&dir).unwrap();
            let victim = dir.join("victim.txt");
            std::fs::write(&victim, b"precious").unwrap();
            std::os::unix::fs::symlink(&victim, dir.join("dl.bin")).unwrap();
            let saved = save(&dir, "dl.bin", b"payload").unwrap();
            assert_eq!(saved.file_name().unwrap(), "dl (1).bin");
            assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn the_downloads_folder_is_created_when_missing() {
        let dir = scratch("mkdir").join("nested").join("Downloads");
        let p = save(&dir, "f.txt", b"hi").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hi");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }
}
