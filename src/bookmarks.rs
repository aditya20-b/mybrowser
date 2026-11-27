//! Bookmarks: a short list of pages the user saved, kept in a small text file in their own
//! data folder (`~/.mybrowser`, or `$MYBROWSER_DATA`), and shown on the `about:bookmarks` page.
//!
//! The file is `url<TAB>title` per line. Only web and local-file addresses are kept, and a
//! page on the web has no way to add, remove or read bookmarks: only the user's own key press does.
//! The file is private to the user (mode 0600 in a 0700 folder), changes are made under a lock
//! so two browsers can't overwrite each other, and a file we can't read is never replaced.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use url::Url;

/// The most bookmarks kept, the longest title kept for one, and the biggest file we read.
const MAX_BOOKMARKS: usize = 2000;
const MAX_TITLE_CHARS: usize = 200;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Bookmark {
    pub url: Url,
    pub title: String,
}

/// Where the bookmarks live: `$MYBROWSER_DATA` if set and not empty (used by the tests), else
/// `~/.mybrowser`.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MYBROWSER_DATA").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if cfg!(test) {
        return None; // tests must never touch the real home folder
    }
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".mybrowser"))
}

fn file_in(dir: &Path) -> PathBuf {
    dir.join("bookmarks.tsv")
}

/// Can this address be bookmarked? Web pages and local files; not blank tabs, internal pages,
/// `data:` documents or anything else.
pub fn is_bookmarkable(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https" | "file") && url.as_str().len() <= 2000
}

/// The address as it is stored: without a username and password (they would sit in plain
/// text in the file and on the bookmarks page).
fn stored(url: &Url) -> Url {
    let mut u = url.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u
}

fn clean_title(title: &str, url: &Url) -> String {
    let flat: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
    // (no control or direction-changing characters: a title could otherwise disguise a link's text)
    let flat = crate::download::display_safe(&flat, MAX_TITLE_CHARS);
    if flat.is_empty() { url.as_str().chars().take(MAX_TITLE_CHARS).collect() } else { flat }
}

/// The saved list, newest last, and whether the file held more than we keep. A missing file is
/// an empty list; a file we can't read (too big, a directory, no permission) is an error, so it
/// is never overwritten by a list that merely looks empty.
fn read_list(dir: &Path) -> Result<(Vec<Bookmark>, bool), String> {
    let path = file_in(dir);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((vec![], false)),
        Err(e) => return Err(format!("could not read bookmarks: {e}")),
    };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err("the bookmarks file is not a normal file or is too big".into());
    }
    let mut bytes = vec![];
    std::fs::File::open(&path)
        .and_then(|f| f.take(MAX_FILE_BYTES).read_to_end(&mut bytes))
        .map_err(|e| format!("could not read bookmarks: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start_matches('\u{feff}');
    let (mut out, mut truncated): (Vec<Bookmark>, bool) = (vec![], false);
    for line in text.lines() {
        // `url<TAB>title`; a hand-added line with just an address is fine too
        let (u, t) = line.split_once('\t').unwrap_or((line, ""));
        let Ok(url) = Url::parse(u.trim()) else { continue };
        let url = stored(&url);
        if !is_bookmarkable(&url) || out.iter().any(|b| b.url == url) {
            continue;
        }
        if out.len() >= MAX_BOOKMARKS {
            truncated = true;
            continue;
        }
        out.push(Bookmark { title: clean_title(t, &url), url });
    }
    Ok((out, truncated))
}

/// Read the list for showing (a file we can't read shows as empty).
pub fn load(dir: &Path) -> Vec<Bookmark> {
    read_list(dir).map(|(list, _)| list).unwrap_or_default()
}

/// Write the list: to a temporary file first (private to the user), then renamed over the old
/// one, so a crash can't leave half a file.
pub fn save(dir: &Path, list: &[Bookmark]) -> std::io::Result<()> {
    create_private_dir(dir)?;
    let mut text = String::new();
    for b in list {
        text.push_str(stored(&b.url).as_str());
        text.push('\t');
        text.push_str(&clean_title(&b.title, &b.url));
        text.push('\n');
    }
    let tmp = dir.join(format!("bookmarks.tsv.{}.tmp", std::process::id()));
    write_private_file(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, file_in(dir)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

/// A lock on the bookmarks folder, held while the list is changed (a file whose creation
/// must succeed exclusively; one left behind by a crash is taken over after a while).
struct Lock(PathBuf);

impl Lock {
    fn acquire(dir: &Path) -> Result<Lock, String> {
        create_private_dir(dir).map_err(|e| format!("could not save bookmarks: {e}"))?;
        let path = dir.join("bookmarks.lock");
        for _ in 0..200 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Lock(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| SystemTime::now().duration_since(t).is_ok_and(|age| age > Duration::from_secs(30)));
                    if stale {
                        let _ = std::fs::remove_file(&path);
                    } else {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                Err(e) => return Err(format!("could not save bookmarks: {e}")),
            }
        }
        Err("could not save bookmarks: another window is using them".into())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Bookmark `url` if it isn't yet, remove it if it is. Returns whether it is bookmarked now.
pub fn toggle(dir: &Path, url: &Url, title: &str) -> Result<bool, String> {
    if !is_bookmarkable(url) {
        return Err("this page can't be bookmarked".into());
    }
    let url = stored(url);
    let _lock = Lock::acquire(dir)?;
    let (mut list, truncated) = read_list(dir)?;
    if truncated {
        return Err(format!("too many bookmarks (the limit is {MAX_BOOKMARKS})"));
    }
    let added = match list.iter().position(|b| b.url == url) {
        Some(i) => {
            list.remove(i);
            false
        }
        None => {
            if list.len() >= MAX_BOOKMARKS {
                return Err(format!("too many bookmarks (the limit is {MAX_BOOKMARKS})"));
            }
            list.push(Bookmark { title: clean_title(title, &url), url });
            true
        }
    };
    save(dir, &list).map_err(|e| format!("could not save bookmarks: {e}"))?;
    Ok(added)
}

/// Is `url` bookmarked?
#[cfg(test)]
pub fn contains(dir: &Path, url: &Url) -> bool {
    let url = stored(url);
    load(dir).iter().any(|b| b.url == url)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// The markup of the `about:bookmarks` page, newest first.
pub fn page_html(list: &[Bookmark]) -> String {
    let mut html = String::from("<title>Bookmarks</title><body style='margin:24px'><h1>Bookmarks</h1>");
    if list.is_empty() {
        html.push_str("<p style='color:#666'>Nothing here yet. Press Cmd+D on a page to bookmark it (and again to remove it).</p>");
    } else {
        html.push_str("<ul style='list-style:none;padding:0'>");
        for b in list.iter().rev() {
            html.push_str(&format!(
                "<li style='margin:8px 0'><a href=\"{}\">{}</a><br><span style='color:#666;font-size:13px'>{}</span></li>",
                esc(b.url.as_str()),
                esc(&b.title),
                esc(b.url.as_str())
            ));
        }
        html.push_str("</ul>");
    }
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mybrowser-bm-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn toggling_adds_then_removes_and_persists() {
        let dir = scratch("toggle");
        assert_eq!(load(&dir), vec![], "no file yet");
        assert_eq!(toggle(&dir, &u("https://example.com/a"), "Example A"), Ok(true));
        assert_eq!(toggle(&dir, &u("https://example.org/"), "  Org\n page "), Ok(true));
        assert!(contains(&dir, &u("https://example.com/a")));
        let list = load(&dir);
        assert_eq!(list.iter().map(|b| b.title.as_str()).collect::<Vec<_>>(), ["Example A", "Org page"]);
        assert_eq!(toggle(&dir, &u("https://example.com/a"), "x"), Ok(false), "a second press removes it");
        assert_eq!(load(&dir).len(), 1);
        assert!(!contains(&dir, &u("https://example.com/a")));
        assert!(!dir.join("bookmarks.lock").exists(), "the lock is released");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_web_and_file_pages_can_be_bookmarked() {
        let dir = scratch("kinds");
        for bad in ["about:blank", "about:bookmarks", "data:text/html,hi", "javascript:alert(1)", "mailto:a@b.c", "ftp://x.test/"] {
            assert!(toggle(&dir, &u(bad), "t").is_err(), "{bad}");
        }
        assert!(toggle(&dir, &u("file:///tmp/x.html"), "local").is_ok());
        assert!(toggle(&dir, &u("http://127.0.0.1:8080/"), "dev").is_ok());
        let long = format!("https://example.com/{}", "a".repeat(3000));
        assert!(toggle(&dir, &u(&long), "long").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_or_hostile_file_is_read_safely() {
        let dir = scratch("damaged");
        std::fs::create_dir_all(&dir).unwrap();
        let mut text = String::new();
        text.push_str("\u{feff}https://ok.test/\tFine\n");
        text.push_str("javascript:alert(1)\tevil\n");
        text.push_str("file:///etc/passwd\tlocal files are allowed as bookmarks, not as anything else\n");
        text.push_str("https://ok.test/\tduplicate\n");
        text.push_str("\t\n");
        text.push_str("not a url\tx\n");
        text.push_str("https://hand.test/added\n");
        text.push_str("https://t.test/\t\u{0}\u{7}control\u{202e}chars\n");
        std::fs::write(dir.join("bookmarks.tsv"), text).unwrap();
        let list = load(&dir);
        let urls: Vec<&str> = list.iter().map(|b| b.url.as_str()).collect();
        assert_eq!(urls, ["https://ok.test/", "file:///etc/passwd", "https://hand.test/added", "https://t.test/"]);
        assert_eq!(list[3].title, "controlchars", "control and direction-changing characters are dropped");
        assert_eq!(list[2].title, "https://hand.test/added", "a line with only an address uses it as the title");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_never_overwritten() {
        // a non-UTF-8 byte is read lossily, so the other lines survive a save
        let dir = scratch("nonutf8");
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = b"https://keep.test/\tKeep\n".to_vec();
        bytes.extend_from_slice(b"https://bad.test/\tcaf\xe9\n");
        std::fs::write(dir.join("bookmarks.tsv"), bytes).unwrap();
        toggle(&dir, &u("https://new.test/"), "New").unwrap();
        let urls: Vec<String> = load(&dir).iter().map(|b| b.url.to_string()).collect();
        assert_eq!(urls, ["https://keep.test/", "https://bad.test/", "https://new.test/"]);
        // a directory where the file should be, and a file over the size limit, are errors, not "empty"
        let dir2 = scratch("unreadable");
        std::fs::create_dir_all(dir2.join("bookmarks.tsv")).unwrap();
        assert!(toggle(&dir2, &u("https://a.test/"), "A").is_err());
        assert!(dir2.join("bookmarks.tsv").is_dir(), "left alone");
        let dir3 = scratch("big");
        std::fs::create_dir_all(&dir3).unwrap();
        let big = format!("https://keep.test/\tKeep\n{}", "x".repeat(MAX_FILE_BYTES as usize + 10));
        std::fs::write(dir3.join("bookmarks.tsv"), &big).unwrap();
        assert!(toggle(&dir3, &u("https://a.test/"), "A").is_err());
        assert_eq!(std::fs::read(dir3.join("bookmarks.tsv")).unwrap().len(), big.len(), "the big file is untouched");
        assert!(load(&dir3).is_empty(), "and it shows as empty rather than being read whole");
        for d in [dir, dir2, dir3] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn a_file_holding_more_than_the_limit_is_not_cut_down_by_a_save() {
        let dir = scratch("overcap");
        std::fs::create_dir_all(&dir).unwrap();
        let many: String = (0..MAX_BOOKMARKS + 50).map(|i| format!("https://h.test/{i}\tt\n")).collect();
        std::fs::write(dir.join("bookmarks.tsv"), &many).unwrap();
        assert_eq!(load(&dir).len(), MAX_BOOKMARKS, "only the first ones are shown");
        assert!(toggle(&dir, &u("https://h.test/0"), "x").is_err(), "removing would cut the rest off, so it is refused");
        assert_eq!(std::fs::read_to_string(dir.join("bookmarks.tsv")).unwrap(), many);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn credentials_in_an_address_are_not_stored() {
        let dir = scratch("creds");
        toggle(&dir, &u("https://alice:hunter2@intranet.test/x?y=1"), "Intranet").unwrap();
        let text = std::fs::read_to_string(dir.join("bookmarks.tsv")).unwrap();
        assert!(!text.contains("alice") && !text.contains("hunter2"), "{text}");
        assert!(text.contains("https://intranet.test/x?y=1"));
        assert!(contains(&dir, &u("https://bob:pw@intranet.test/x?y=1")), "found without them too");
        assert!(!page_html(&load(&dir)).contains("hunter2"));
        // a hand-edited file with credentials is cleaned when read
        std::fs::write(dir.join("bookmarks.tsv"), "https://carol:secret@x.test/\tX\n").unwrap();
        assert_eq!(load(&dir)[0].url.as_str(), "https://x.test/");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn the_folder_and_file_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perms").join("data");
        toggle(&dir, &u("https://a.test/"), "A").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("bookmarks.tsv")), 0o600);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn simultaneous_changes_do_not_lose_each_other() {
        let dir = scratch("race");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || toggle(&dir, &u(&format!("https://race.test/{i}")), "t").unwrap())
            })
            .collect();
        for h in handles {
            assert!(h.join().unwrap());
        }
        assert_eq!(load(&dir).len(), 8, "every bookmark made it");
        // a lock left behind by a crash long ago doesn't block saving for ever
        std::fs::write(dir.join("bookmarks.lock"), "").unwrap();
        let old = SystemTime::now() - Duration::from_secs(120);
        std::fs::File::options().write(true).open(dir.join("bookmarks.lock")).unwrap().set_modified(old).unwrap();
        assert!(toggle(&dir, &u("https://after.test/"), "t").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bookmarks_page_escapes_everything_and_lists_newest_first() {
        let list = vec![
            Bookmark { url: u("https://old.test/"), title: "Old".into() },
            Bookmark { url: u("https://new.test/?a=1&b=2"), title: "<script>alert(1)</script> & \"quotes\"".into() },
        ];
        let html = page_html(&list);
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.find("new.test").unwrap() < html.find("old.test").unwrap());
        assert!(html.contains("&amp;b=2") && html.contains("&quot;quotes&quot;"));
        assert!(page_html(&[]).contains("Nothing here yet"));
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = scratch("tmp");
        save(&dir, &[Bookmark { url: u("https://a.test/"), title: "A".into() }]).unwrap();
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["bookmarks.tsv"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
