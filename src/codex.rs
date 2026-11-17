use crate::details::sanitize;
use serde::Deserialize;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

const INDEX_READ_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct IndexEntry<'a> {
    id: &'a str,
    thread_name: String,
}

pub(crate) fn session_title(home: &Path, session_id: &str) -> Option<String> {
    let path = home.join("session_index.jsonl");
    if !path.metadata().ok()?.is_file() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    read_title(&mut file, session_id, INDEX_READ_LIMIT).ok()?
}

fn read_title(
    reader: &mut (impl Read + Seek),
    session_id: &str,
    limit: u64,
) -> io::Result<Option<String>> {
    let length = reader.seek(SeekFrom::End(0))?;
    let start = length.saturating_sub(limit);
    reader.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    reader.take(length - start).read_to_end(&mut bytes)?;
    let mut lines = bytes.split(|&byte| byte == b'\n');
    if start > 0 {
        lines.next();
    }
    Ok(lines.rev().find_map(|line| {
        let entry: IndexEntry = serde_json::from_slice(line).ok()?;
        (entry.id == session_id)
            .then(|| sanitize(&entry.thread_name))
            .filter(|name| !name.is_empty())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn newest_usable_name_wins_despite_bad_or_unrelated_entries() {
        let index = concat!(
            "{\"id\":\"s\",\"thread_name\":\"Old\"}\n",
            "{\"id\":\"s\",\"thread_name\":\"  New\\nname\\u200b  \"}\n",
            "{\"id\":\"s\",\"thread_name\":\"  \"}\n",
            "{\"id\":\"s\",\"thread_name\":42}\n",
            "{\"id\":\"other\",\"thread_name\":\"Other\"}\n",
            "{\"id\":\"s\",\"thread_name\":\"unfinished"
        );
        assert_eq!(
            read_title(&mut Cursor::new(index), "s", INDEX_READ_LIMIT).unwrap(),
            Some("Newname".into())
        );
        assert_eq!(
            read_title(&mut Cursor::new(index), "missing", INDEX_READ_LIMIT).unwrap(),
            None
        );
    }

    #[test]
    fn bounded_tail_discards_the_partial_first_line() {
        let latest = "{\"id\":\"s\",\"thread_name\":\"Latest\"}\n";
        let index = format!("{}\n{latest}", "x".repeat(256));
        assert_eq!(
            read_title(&mut Cursor::new(&index), "s", latest.len() as u64 + 5).unwrap(),
            Some("Latest".into())
        );
        assert_eq!(
            read_title(&mut Cursor::new(&index), "s", latest.len() as u64 - 5).unwrap(),
            None
        );
    }
}
