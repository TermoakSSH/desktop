//! Text helpers of the importers: decoding files written by Windows apps
//! (UTF-16 with BOM from `regedit`, Windows-1252 from older apps) and a
//! tolerant INI reader.

/// Decodes a file: UTF-8 (with or without BOM), UTF-16 LE/BE with BOM, or
/// Windows-1252/Latin-1 when it is not valid UTF-8.
pub fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, u16::from_be_bytes);
    }
    // UTF-16 LE without BOM: every other byte is zero in ASCII text.
    if bytes.len() >= 4 && bytes[1] == 0 && bytes[3] == 0 && bytes[0] != 0 {
        return utf16(bytes, u16::from_le_bytes);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| cp1252(b)).collect(),
    }
}

fn utf16(bytes: &[u8], read: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|c| read([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units)
}

/// Windows-1252 byte (Latin-1 except 0x80–0x9F).
fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž',
        '\u{8F}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}',
        'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

/// Section of an INI file, with its entries in order (`key=value`, the key
/// trimmed, the value as written after the first `=`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IniSection {
    pub name: String,
    pub entries: Vec<(String, String)>,
}

impl IniSection {
    /// Value of a key (case-insensitive).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }
}

/// Reads an INI file: `[section]` headers, `key=value` lines, `;` and `#`
/// comments. Entries before the first header go to a section without name.
pub fn parse_ini(text: &str) -> Vec<IniSection> {
    let mut out = vec![IniSection::default()];
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        if let Some(name) = trimmed.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            out.push(IniSection {
                name: name.trim().to_string(),
                entries: Vec::new(),
            });
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let section = out.last_mut().expect("there is always a section");
            section
                .entries
                .push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    if out[0].entries.is_empty() {
        out.remove(0);
    }
    out
}

/// Decodes `%XX` escapes (PuTTY session names, `file://` URLs). Invalid
/// escapes are kept as they are.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_windows_files() {
        let mut le = vec![0xFF, 0xFE];
        for u in "Año".encode_utf16() {
            le.extend_from_slice(&u.to_le_bytes());
        }
        assert_eq!(decode(&le), "Año");
        assert_eq!(decode(&[0xEF, 0xBB, 0xBF, b'h', b'i']), "hi");
        assert_eq!(decode(&[b'A', 0xF1, b'o', 0x80]), "Año€");
        assert_eq!(decode(b"h\0i\0"), "hi");
    }

    #[test]
    fn reads_ini() {
        let s = parse_ini("top=1\n; c\n[A]\nx = 1=2\n\n[ B ]\ny=\n");
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].get("TOP"), Some("1"));
        assert_eq!(s[1].get("x"), Some("1=2"));
        assert_eq!(s[2].name, "B");
        assert_eq!(s[2].get("y"), Some(""));
    }

    #[test]
    fn percent() {
        assert_eq!(percent_decode("My%20Server%2Fprod"), "My Server/prod");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(
            percent_decode("/home/ana/Mis%20cosas"),
            "/home/ana/Mis cosas"
        );
    }
}
