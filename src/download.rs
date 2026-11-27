//! Saving a file the browser can't show (a zip, a PDF, an installer) to the downloads folder.
//!
//! Nothing here opens or runs what it saves. The name comes from the address, is cleaned of
//! anything that could point outside the folder or disguise the file's type, and never
//! replaces a file that is already there. Whether a download should happen at all is decided
//! by the tab (only for a navigation the user started), not here.
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The longest file name we create, in characters (the extension is kept when it is cut)...
const MAX_NAME_CHARS: usize = 100;
/// ...and in bytes (file systems limit names to 255 bytes, and CJK names are 3 bytes a character).
const MAX_NAME_BYTES: usize = 200;

/// A file waiting to be saved: what a page load found that it could not display.
#[derive(Debug, Clone)]
pub struct PendingDownload {
    pub name: String,
    pub content_type: String,
    pub body: Vec<u8>,
}

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

/// Characters that show nothing or reorder what is shown around them: a file called
/// `evil<RLO>fdp.exe` would display as `evilexe.pdf`.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}' | '\u{180E}'
            | '\u{180B}'..='\u{180D}' | '\u{1D173}'..='\u{1D17A}' | '\u{E0100}'..='\u{E01EF}'
            | '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{2800}'
            | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// Text for a message shown to the user (the address bar): control and invisible characters
/// removed, and cut to `max` characters.
pub fn display_safe(text: &str, max: usize) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control() && !is_invisible(*c)).collect();
    if clean.chars().count() <= max {
        clean
    } else {
        let mut cut: String = clean.chars().take(max).collect();
        cut.push('…');
        cut
    }
}

/// A file name that is safe to create: no separators, control or invisible characters,
/// nothing that starts with a dot (hidden files), no trailing dots or spaces, never empty,
/// and short enough for any file system.
pub fn safe_file_name(wanted: &str) -> String {
    let cleaned: String = wanted
        .chars()
        .filter(|c| !is_invisible(*c))
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim_end_matches(|c: char| c == '.' || c.is_whitespace()).to_string();
    if cleaned.is_empty() {
        return "download".into();
    }
    if cleaned.chars().count() <= MAX_NAME_CHARS && cleaned.len() <= MAX_NAME_BYTES {
        return cleaned;
    }
    // too long: cut the stem, keep a short extension
    let (stem, ext) = match cleaned.rfind('.') {
        Some(i) if cleaned[i..].chars().count() <= 12 => (&cleaned[..i], &cleaned[i..]),
        _ => (cleaned.as_str(), ""),
    };
    let mut out = String::new();
    let (mut chars, mut bytes) = (0, 0);
    for c in stem.chars() {
        if chars + ext.chars().count() >= MAX_NAME_CHARS || bytes + c.len_utf8() + ext.len() > MAX_NAME_BYTES {
            break;
        }
        out.push(c);
        chars += 1;
        bytes += c.len_utf8();
    }
    out.push_str(ext);
    if out.is_empty() { "download".into() } else { out }
}

/// The name a URL's last path segment suggests, percent-decoded but not shortened (so the
/// extension survives; `safe_file_name` does the cutting).
pub fn name_from_url(url: &url::Url) -> String {
    url.path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(&crate::net::percent_decode_bytes(s)).into_owned())
        .or_else(|| url.host_str().map(String::from))
        .unwrap_or_else(|| "download".into())
}

/// `name`, or `name (1)`, `name (2)` ... (before the extension) until a file by that name
/// can be created. The file is created exclusively, so an existing file (or a symlink
/// someone planted) is never written through or replaced. If the write fails, the partial
/// file is removed.
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
                if let Err(e) = file.write_all(body).and_then(|_| file.flush()) {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
                quarantine(&path);
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "too many files with that name"))
}

/// Mark a downloaded file as coming from the internet, so macOS asks before opening an app or
/// script (Gatekeeper). Best effort: the file is already saved either way.
#[cfg(all(target_os = "macos", not(test)))]
fn quarantine(path: &Path) {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let value = format!("0081;{stamp:x};mybrowser;");
    let _ = std::process::Command::new("/usr/bin/xattr").arg("-w").arg("com.apple.quarantine").arg(value).arg(path).output();
}

#[cfg(not(all(target_os = "macos", not(test))))]
fn quarantine(_path: &Path) {}

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
        assert_eq!(safe_file_name("report.pdf."), "report.pdf", "no trailing dots");
        assert_eq!(safe_file_name("report.pdf  "), "report.pdf");
        // names that would display as something else
        assert_eq!(safe_file_name("evil\u{202e}fdp.exe"), "evilfdp.exe");
        assert_eq!(safe_file_name("a\u{200b}b\u{feff}c\u{2066}d.txt"), "abcd.txt");
        assert_eq!(safe_file_name("x\u{61c}.zip"), "x.zip");
        assert_eq!(safe_file_name("a\u{180b}b\u{e0100}c.pdf"), "abc.pdf");
        assert_eq!(display_safe("a\u{180b}b\u{e0100}c\u{1d173}d", 50), "abcd");
        assert_eq!(safe_file_name("a\u{3164}b\u{2800}c\u{e0041}d\u{2028}e.zip"), "abcde.zip");
        assert_eq!(display_safe("ok\u{202e}\n\u{0}text", 100), "oktext");
        assert_eq!(display_safe(&"x".repeat(6000), 100).chars().count(), 101);
        // long names keep their extension, in characters and in bytes
        let long = format!("{}.tar.gz", "x".repeat(500));
        let cut = safe_file_name(&long);
        assert!(cut.chars().count() <= MAX_NAME_CHARS && cut.ends_with(".gz"), "{cut}");
        let no_ext = safe_file_name(&"é".repeat(500));
        assert!(no_ext.chars().count() <= MAX_NAME_CHARS && no_ext.len() <= MAX_NAME_BYTES);
        let cjk = safe_file_name(&format!("{}.zip", "日".repeat(200)));
        assert!(cjk.len() <= MAX_NAME_BYTES && cjk.ends_with(".zip"), "{} bytes", cjk.len());
        assert!(safe_file_name(&format!("a.{}", "y".repeat(300))).chars().count() <= MAX_NAME_CHARS);
    }

    #[test]
    fn a_long_url_name_keeps_its_extension() {
        let url = url::Url::parse(&format!("https://example.org/{}.pdf", "a".repeat(200))).unwrap();
        let name = safe_file_name(&name_from_url(&url));
        assert!(name.ends_with(".pdf") && name.chars().count() <= MAX_NAME_CHARS, "{name}");
        assert_eq!(name_from_url(&url::Url::parse("https://example.org/dir/my%20file.zip?x=1#y").unwrap()), "my file.zip");
        assert_eq!(name_from_url(&url::Url::parse("https://example.org/").unwrap()), "example.org");
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
