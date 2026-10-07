//! CSV: reading (RFC 4180 quotes, `,` `;` or tab detected from the first
//! line), the column mapping (guessed from the headers, editable in the
//! preview) and the export writer.
//!
//! Recognized headers (case, spaces and punctuation ignored):
//! label/name/alias/title/session, host/hostname/address/ip/server
//! (also Termius' `Hostname/IP` and ZOC's `Connect to`), port,
//! user/username/login, group/groups/folder/path, tags, notes/description,
//! password, protocol/type/device. A `user@host:port` address is split.

use super::{ImportHost, ImportSet, is_ssh_protocol, split_tags};

/// A CSV file split into rows and cells.
#[derive(Debug, Clone, PartialEq)]
pub struct CsvTable {
    pub delimiter: char,
    pub rows: Vec<Vec<String>>,
}

impl CsvTable {
    /// Number of columns (of the widest row).
    pub fn width(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(0)
    }
}

/// The delimiter of a CSV: the most frequent of `,`, `;` and tab in the
/// first line, outside quotes.
pub fn detect_delimiter(text: &str) -> char {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut counts = [(',', 0usize), (';', 0), ('\t', 0)];
    let mut quoted = false;
    for c in line.chars() {
        if c == '"' {
            quoted = !quoted;
        } else if !quoted && let Some(e) = counts.iter_mut().find(|(d, _)| *d == c) {
            e.1 += 1;
        }
    }
    counts
        .iter()
        .max_by_key(|(_, n)| *n)
        .filter(|(_, n)| *n > 0)
        .map_or(',', |(d, _)| *d)
}

/// Reads a CSV (quoted fields may contain the delimiter, `""` and line
/// breaks). Empty lines are dropped.
pub fn read(text: &str) -> CsvTable {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let delimiter = detect_delimiter(text);
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cell.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
            continue;
        }
        match c {
            '"' if cell.trim().is_empty() => {
                cell.clear();
                quoted = true;
            }
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut cell));
                push_row(&mut rows, std::mem::take(&mut row));
            }
            c if c == delimiter => row.push(std::mem::take(&mut cell)),
            c => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        push_row(&mut rows, row);
    }
    CsvTable { delimiter, rows }
}

fn push_row(rows: &mut Vec<Vec<String>>, row: Vec<String>) {
    if row.iter().any(|c| !c.trim().is_empty()) {
        rows.push(row);
    }
}

/// A field of a host a column can be mapped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    Label,
    Address,
    Port,
    User,
    Group,
    Tags,
    Notes,
    Password,
    Protocol,
}

impl Field {
    pub const ALL: [Field; 9] = [
        Field::Label,
        Field::Address,
        Field::Port,
        Field::User,
        Field::Group,
        Field::Tags,
        Field::Notes,
        Field::Password,
        Field::Protocol,
    ];

    pub fn name(self) -> gpui::SharedString {
        match self {
            Field::Label => t!("import_export.field.label"),
            Field::Address => t!("import_export.field.address"),
            Field::Port => t!("import_export.field.port"),
            Field::User => t!("import_export.field.user"),
            Field::Group => t!("import_export.field.group"),
            Field::Tags => t!("import_export.field.tags"),
            Field::Notes => t!("import_export.field.notes"),
            Field::Password => t!("import_export.field.password"),
            Field::Protocol => t!("import_export.field.protocol"),
        }
    }

    /// The field a header names, if any.
    pub fn from_header(header: &str) -> Option<Field> {
        let h: String = header
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        Some(match h.as_str() {
            "label" | "name" | "alias" | "title" | "session" | "sessionname" | "displayname"
            | "hostalias" | "entry" | "entryname" => Field::Label,
            "host" | "hostname" | "address" | "hostaddress" | "ip" | "ipaddress" | "server"
            | "hostnameip" | "hostip" | "connectto" | "remotehost" | "serveraddress" | "fqdn" => {
                Field::Address
            }
            "port" | "sshport" | "portnumber" => Field::Port,
            "user" | "username" | "login" | "loginname" | "userid" | "sshuser" => Field::User,
            "group" | "groups" | "folder" | "path" | "directory" | "category" | "groupname" => {
                Field::Group
            }
            "tags" | "tag" | "labels" | "keywords" => Field::Tags,
            "notes" | "note" | "description" | "comment" | "comments" | "memo" => Field::Notes,
            "password" | "pass" | "passwd" => Field::Password,
            "protocol" | "type" | "connectiontype" | "device" | "kind" => Field::Protocol,
            _ => return None,
        })
    }
}

/// Which column feeds each field.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mapping {
    /// The first row has the column names.
    pub has_header: bool,
    pub columns: Vec<(Field, usize)>,
}

impl Mapping {
    pub fn column(&self, field: Field) -> Option<usize> {
        self.columns
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, c)| *c)
    }

    pub fn set(&mut self, field: Field, column: Option<usize>) {
        self.columns.retain(|(f, _)| *f != field);
        if let Some(c) = column {
            self.columns.push((field, c));
            self.columns.sort();
        }
    }

    /// The mapping can import something: there is an address column.
    pub fn is_usable(&self) -> bool {
        self.column(Field::Address).is_some()
    }
}

/// Mapping from the headers of the first row. Without any known header the
/// first row is taken as data and nothing is mapped (the user maps it).
pub fn guess_mapping(table: &CsvTable) -> Mapping {
    let Some(first) = table.rows.first() else {
        return Mapping::default();
    };
    let mut mapping = Mapping {
        has_header: false,
        columns: Vec::new(),
    };
    for (i, h) in first.iter().enumerate() {
        if let Some(f) = Field::from_header(h)
            && mapping.column(f).is_none()
        {
            mapping.columns.push((f, i));
        }
    }
    mapping.columns.sort();
    mapping.has_header = !mapping.columns.is_empty();
    mapping
}

/// Names of the columns for the mapping (the headers, or `Column 3`).
pub fn column_names(table: &CsvTable, has_header: bool) -> Vec<String> {
    (0..table.width())
        .map(|i| {
            has_header
                .then(|| table.rows.first().and_then(|r| r.get(i)))
                .flatten()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| t!("import_export.column_n", n = i + 1).to_string())
        })
        .collect()
}

/// The hosts of a table with a mapping.
pub fn to_set(table: &CsvTable, mapping: &Mapping) -> ImportSet {
    let mut set = ImportSet::default();
    let skip = usize::from(mapping.has_header);
    for (n, row) in table.rows.iter().enumerate().skip(skip) {
        let line = n + 1;
        let get = |f: Field| {
            mapping
                .column(f)
                .and_then(|c| row.get(c))
                .map(|v| v.trim().to_string())
                .unwrap_or_default()
        };
        let protocol = get(Field::Protocol);
        if !is_ssh_protocol(&protocol) {
            set.warnings.push(
                t!(
                    "import_export.warn.not_ssh",
                    name = format!("{} ({line})", get(Field::Label)),
                    protocol = protocol
                )
                .to_string(),
            );
            continue;
        }
        let port_text = get(Field::Port);
        let port = match port_text.parse::<u16>() {
            Ok(p) => Some(p),
            Err(_) if port_text.is_empty() => None,
            Err(_) => {
                set.warnings.push(
                    t!("import_export.warn.bad_port", line = line, port = port_text).to_string(),
                );
                None
            }
        };
        let group_path = get(Field::Group);
        let group = set.group_path(&group_path);
        let address = get(Field::Address);
        if address.is_empty() {
            set.warnings
                .push(t!("import_export.warn.no_address_line", line = line).to_string());
            continue;
        }
        set.push_host(ImportHost {
            label: get(Field::Label),
            address,
            port,
            username: Some(get(Field::User)),
            group,
            tags: split_tags(&get(Field::Tags)),
            notes: get(Field::Notes),
            password: Some(get(Field::Password)).filter(|p| !p.is_empty()),
            ..Default::default()
        });
    }
    // Groups no host ended up in (rows skipped) are left out.
    let used: Vec<String> = set.hosts.iter().filter_map(|h| h.group.clone()).collect();
    set.groups.retain(|g| {
        used.iter()
            .any(|u| u == &g.key || u.starts_with(&format!("{}/", g.key)))
    });
    set
}

/// A cell for the export: quoted when it has the delimiter, quotes or
/// line breaks.
pub fn escape(cell: &str, delimiter: char) -> String {
    if cell.contains([delimiter, '"', '\n', '\r']) || cell.starts_with(' ') || cell.ends_with(' ') {
        format!("\"{}\"", cell.replace('"', "\"\""))
    } else {
        cell.to_string()
    }
}

/// Row of the export.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExportRow {
    pub label: String,
    pub address: String,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub group: String,
    pub tags: Vec<String>,
    pub notes: String,
}

/// Header of the export (also what [`guess_mapping`] reads back).
pub const EXPORT_HEADER: [&str; 7] = ["label", "host", "port", "user", "group", "tags", "notes"];

/// Writes the export CSV (comma, CRLF as RFC 4180 says; groups as
/// `Parent/Child`, tags separated by `, `).
pub fn write(rows: &[ExportRow]) -> String {
    let mut out = EXPORT_HEADER.join(",");
    out.push_str("\r\n");
    for r in rows {
        let cells = [
            r.label.clone(),
            r.address.clone(),
            r.port.map(|p| p.to_string()).unwrap_or_default(),
            r.username.clone().unwrap_or_default(),
            r.group.clone(),
            r.tags.join(", "),
            r.notes.clone(),
        ];
        let line: Vec<String> = cells.iter().map(|c| escape(c, ',')).collect();
        out.push_str(&line.join(","));
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\u{feff}Name;Host;Port;User;Folder;Tags;Description\r\n\
        web-1;10.0.0.11;22;deploy;Production/Web;\"web, prod\";\"Front, nginx\"\r\n\
        db-1;db.internal;2222;postgres;Production/DB;db;\"Primary\n(eu-west)\"\r\n\
        ;;;;;;\r\n\
        legacy;root@old.example.com:2200;;;;;\r\n\
        broken;;22;;;;\r\n";

    #[test]
    fn reads_semicolons_quotes_and_line_breaks() {
        let t = read(SAMPLE);
        assert_eq!(t.delimiter, ';');
        assert_eq!(t.rows.len(), 5);
        assert_eq!(t.rows[1][5], "web, prod");
        assert_eq!(t.rows[2][6], "Primary\n(eu-west)");
        let m = guess_mapping(&t);
        assert!(m.has_header && m.is_usable());
        assert_eq!(m.column(Field::Group), Some(4));
        let set = to_set(&t, &m);
        assert_eq!(set.hosts.len(), 3);
        assert_eq!(set.warnings.len(), 1, "{:?}", set.warnings);
        let web = &set.hosts[0];
        assert_eq!(web.tags, vec!["web", "prod"]);
        assert_eq!(web.notes, "Front, nginx");
        assert_eq!(
            set.group_label(web.group.as_deref().unwrap()),
            "Production / Web"
        );
        let legacy = &set.hosts[2];
        assert_eq!(legacy.address, "old.example.com");
        assert_eq!(legacy.username.as_deref(), Some("root"));
        assert_eq!(legacy.port, Some(2200));
        assert_eq!(set.groups.len(), 3);
    }

    #[test]
    fn unknown_headers_need_a_mapping() {
        let t = read("a,b,c\nweb,10.0.0.1,ubuntu\n");
        let mut m = guess_mapping(&t);
        assert!(!m.has_header && !m.is_usable());
        assert_eq!(column_names(&t, false)[1], "Column 2");
        m.set(Field::Address, Some(1));
        m.set(Field::Label, Some(0));
        m.set(Field::User, Some(2));
        m.has_header = true;
        let set = to_set(&t, &m);
        assert_eq!(set.hosts.len(), 1);
        assert_eq!(set.hosts[0].target(), "ubuntu@10.0.0.1");
        m.has_header = false;
        assert_eq!(to_set(&t, &m).hosts.len(), 2);
    }

    #[test]
    fn tabs_and_protocols() {
        let t = read("label\taddress\tprotocol\nsw\t10.1.1.1\ttelnet\nbox\t10.1.1.2\tssh\n");
        assert_eq!(t.delimiter, '\t');
        let set = to_set(&t, &guess_mapping(&t));
        assert_eq!(set.hosts.len(), 1);
        assert_eq!(set.warnings.len(), 1);
    }

    #[test]
    fn export_round_trip() {
        let rows = vec![ExportRow {
            label: "web \"main\"".into(),
            address: "10.0.0.1".into(),
            port: Some(2222),
            username: Some("ana".into()),
            group: "Prod/Web".into(),
            tags: vec!["a".into(), "b".into()],
            notes: "line 1\nline 2".into(),
        }];
        let text = write(&rows);
        assert!(text.starts_with("label,host,port,user,group,tags,notes\r\n"));
        let t = read(&text);
        let set = to_set(&t, &guess_mapping(&t));
        let h = &set.hosts[0];
        assert_eq!(h.label, "web \"main\"");
        assert_eq!(h.port, Some(2222));
        assert_eq!(h.tags, vec!["a", "b"]);
        assert_eq!(h.notes, "line 1\nline 2");
        assert_eq!(set.group_label(h.group.as_deref().unwrap()), "Prod / Web");
    }
}
