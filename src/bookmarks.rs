//! Bookmarks: a short list of pages the user saved, kept in a small text file in their own
//! data folder (`~/.mybrowser`, or `$MYBROWSER_DATA`), and shown on the `about:bookmarks` page.
//!
//! The file is `url<TAB>title` per line. Only web and local-file addresses are kept, and a
//! page on the web has no way to add, remove or read bookmarks: only the user's own key press does.
use std::path::{Path, PathBuf};
use url::Url;

/// The most bookmarks kept, and the longest title kept for one.
const MAX_BOOKMARKS: usize = 2000;
const MAX_TITLE_CHARS: usize = 200;

#[derive(Clone, Debug, PartialEq)]
pub struct Bookmark {
    pub url: Url,
    pub title: String,
}

/// Where the bookmarks live: `$MYBROWSER_DATA` if set (used by the tests), else `~/.mybrowser`.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MYBROWSER_DATA") {
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

fn clean_title(title: &str, url: &Url) -> String {
    let flat: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
    // (no control or direction-changing characters: a title could otherwise disguise a link's text)
    let flat = crate::download::display_safe(&flat, MAX_TITLE_CHARS);
    if flat.is_empty() { url.as_str().chars().take(MAX_TITLE_CHARS).collect() } else { flat }
}

/// Read the list (missing or damaged lines are skipped; the newest bookmark is last).
pub fn load(dir: &Path) -> Vec<Bookmark> {
    let Ok(text) = std::fs::read_to_string(file_in(dir)) else { return vec![] };
    let mut out: Vec<Bookmark> = vec![];
    for line in text.lines() {
        let Some((u, t)) = line.split_once('\t') else { continue };
        let Ok(url) = Url::parse(u) else { continue };
        if is_bookmarkable(&url) && !out.iter().any(|b| b.url == url) && out.len() < MAX_BOOKMARKS {
            out.push(Bookmark { title: clean_title(t, &url), url });
        }
    }
    out
}

/// Write the list: to a temporary file first, then renamed over the old one, so a crash
/// can't leave half a file.
pub fn save(dir: &Path, list: &[Bookmark]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut text = String::new();
    for b in list {
        text.push_str(b.url.as_str());
        text.push('\t');
        text.push_str(&clean_title(&b.title, &b.url));
        text.push('\n');
    }
    let tmp = dir.join(format!("bookmarks.tsv.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, file_in(dir)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Bookmark `url` if it isn't yet, remove it if it is. Returns whether it is bookmarked now.
pub fn toggle(dir: &Path, url: &Url, title: &str) -> Result<bool, String> {
    if !is_bookmarkable(url) {
        return Err("this page can't be bookmarked".into());
    }
    let mut list = load(dir);
    let added = match list.iter().position(|b| b.url == *url) {
        Some(i) => {
            list.remove(i);
            false
        }
        None => {
            if list.len() >= MAX_BOOKMARKS {
                return Err(format!("too many bookmarks (the limit is {MAX_BOOKMARKS})"));
            }
            list.push(Bookmark { url: url.clone(), title: clean_title(title, url) });
            true
        }
    };
    save(dir, &list).map_err(|e| format!("could not save bookmarks: {e}"))?;
    Ok(added)
}

/// Is `url` bookmarked?
pub fn contains(dir: &Path, url: &Url) -> bool {
    load(dir).iter().any(|b| b.url == *url)
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
        text.push_str("https://ok.test/\tFine\n");
        text.push_str("no tab on this line\n");
        text.push_str("javascript:alert(1)\tevil\n");
        text.push_str("file:///etc/passwd\tlocal files are allowed as bookmarks, not as anything else\n");
        text.push_str("https://ok.test/\tduplicate\n");
        text.push_str("\t\n");
        text.push_str("not a url\tx\n");
        text.push_str("https://t.test/\t\u{0}\u{7}control\u{202e}chars\n");
        std::fs::write(dir.join("bookmarks.tsv"), text).unwrap();
        let list = load(&dir);
        let urls: Vec<&str> = list.iter().map(|b| b.url.as_str()).collect();
        assert_eq!(urls, ["https://ok.test/", "file:///etc/passwd", "https://t.test/"]);
        assert_eq!(list[2].title, "controlchars", "control and direction-changing characters are dropped");
        // a huge file is capped
        let many: String = (0..MAX_BOOKMARKS + 50).map(|i| format!("https://h.test/{i}\tt\n")).collect();
        std::fs::write(dir.join("bookmarks.tsv"), many).unwrap();
        assert_eq!(load(&dir).len(), MAX_BOOKMARKS);
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
