//! A user-defined group order for "group by type".
//!
//! The built-in order is one fixed table (`fs::TYPE_GROUP_ORDER`): folders,
//! then programs, archives, media, documents, configuration, and whatever could
//! not be classified. This module lets the user reorder part of that list — and
//! split a category apart — by naming the groups they care about, top to
//! bottom, as `md`, `pdf`, `image`, `folder`…
//!
//! ## How an entry finds its group
//!
//! Rules are consulted **in order and the first match wins**, so a rule that
//! names an extension can pull that extension out of the category rule that
//! would otherwise have claimed it:
//!
//! ```text
//!   kind:folder   → group 0
//!   ext:md        → group 1     (md files leave the "document" category)
//!   kind:document → group 2     (everything else that is a document)
//! ```
//!
//! An entry no rule matches keeps its built-in category, and that category is
//! placed **after every rule** — a user who lists nine extensions still sees
//! the untouched categories below them, in the order they always had.
//!
//! ## Extensions never claim folders
//!
//! `ext:` rules only ever match files. A folder called `notes.md` would
//! otherwise be pulled into the `md` group, where it does not belong: folders
//! are matched by `kind:folder` alone.

use crate::fs::{Entry, FileKind, type_group_rank};

/// Most rules the order can hold.
///
/// A guard against a hand-edited file, not a real limit: nobody orders more
/// than a handful of extensions by hand, and the settings list stays usable.
pub const MAX_RULES: usize = 32;

/// Most extensions one rule may bundle.
///
/// A rule like `doc,docx,docm,rtf,odt` is one group; a runaway list is a
/// hand-editing mistake. 16 covers every family worth grouping.
pub const MAX_EXTS_PER_RULE: usize = 16;

/// One entry of the order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupRule {
    /// A whole [`FileKind`] section, including folders.
    Kind(FileKind),
    /// One or more extensions sharing a single group, lowercase without dots.
    ///
    /// More than one is how a FORMAT family becomes one section: `doc` and
    /// `docx` are both "Word", and giving them two rules would draw a section
    /// break between two halves of the same thing.
    Exts(Vec<String>),
}

impl GroupRule {
    /// `true` when a rendered row belongs to this group.
    ///
    /// The list view lays rows out long after the `Entry` is gone, so matching
    /// works on the three facts a row keeps: is it a folder, its kind, and its
    /// extension.
    fn matches_parts(&self, is_dir: bool, kind: FileKind, ext: &str) -> bool {
        match self {
            // Folders are claimed by their kind only: an `ext:` rule must not
            // capture a directory whose name happens to end in `.md`.
            Self::Kind(rule) => kind == *rule,
            Self::Exts(exts) => {
                !is_dir && exts.iter().any(|candidate| ext.eq_ignore_ascii_case(candidate))
            }
        }
    }

    /// Canonical token: what the settings list shows and what the config stores.
    pub fn token(&self) -> String {
        match self {
            Self::Kind(kind) => format!("kind:{}", kind_code(*kind)),
            Self::Exts(exts) => format!("ext:{}", exts.join(",")),
        }
    }
}

/// Suffix used in `kind:` tokens. Deliberately the same words the `GroupMode`
/// codes and the extension lists use, so one vocabulary covers the settings UI,
/// the config file and the code.
pub fn kind_code(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Folder => "folder",
        FileKind::Application => "application",
        FileKind::Archive => "archive",
        FileKind::Image => "image",
        FileKind::Video => "video",
        FileKind::Audio => "audio",
        FileKind::Document => "document",
        FileKind::Config => "config",
        FileKind::File => "file",
    }
}

/// Inverse of [`kind_code`].
pub fn kind_from_code(code: &str) -> Option<FileKind> {
    Some(match code {
        "folder" => FileKind::Folder,
        "application" => FileKind::Application,
        "archive" => FileKind::Archive,
        "image" => FileKind::Image,
        "video" => FileKind::Video,
        "audio" => FileKind::Audio,
        "document" => FileKind::Document,
        "config" => FileKind::Config,
        "file" => FileKind::File,
        _ => return None,
    })
}

/// The order shipped as the default, i.e. what a config that never mentioned
/// this setting gets.
///
/// It is the handful of forms that make up most of a working folder, ordered
/// the way the user asked: folders first, then the note and document formats
/// they reach for, then images and archives, and executables last.
///
/// Office formats are bundled by FAMILY, so "Word" is one section rather than
/// one per extension: a `doc` and a `docx` sitting next to each other belong
/// together, and two rules would draw a section break between them.
pub const DEFAULT_TOKENS: &[&str] = &[
    "kind:folder",
    "ext:md",
    "ext:pdf",
    "ext:txt",
    "ext:xls,xlsx",
    "ext:doc,docx",
    "ext:ppt,pptx",
    "kind:image",
    "kind:archive",
    "ext:exe",
];

/// [`DEFAULT_TOKENS`] as a ready-to-use order.
pub fn default_order() -> TypeGroupOrder {
    TypeGroupOrder::from_tokens(&DEFAULT_TOKENS.iter().map(|t| (*t).to_string()).collect::<Vec<_>>())
}

/// The user's group order, already normalized.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypeGroupOrder {
    rules: Vec<GroupRule>,
}

impl TypeGroupOrder {
    /// Normalizes raw tokens from the config file.
    ///
    /// Forgiving on purpose, because this is a file a user may edit by hand:
    ///
    /// * `kind:image` and `ext:md` are the canonical forms;
    /// * a bare token is accepted too — `image` is read as the category, `md`
    ///   as an extension — since no real extension is spelled like a category
    ///   name, and `folder` in a hand-written list means the folder section;
    /// * case and a leading dot are ignored (`ext:.MD` → `ext:md`);
    /// * anything empty, unparseable or repeated is dropped;
    /// * at most [`MAX_RULES`] entries survive.
    ///
    /// Dropping instead of failing keeps one bad line in the file from taking
    /// the whole order (or the whole config) down with it.
    pub fn from_tokens(tokens: &[String]) -> Self {
        let mut rules: Vec<GroupRule> = Vec::new();
        for token in tokens {
            let Some(rule) = parse_token(token) else {
                continue;
            };
            if rules.len() == MAX_RULES {
                break;
            }
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
        Self { rules }
    }

    /// Canonical tokens, ready to be written back to the config file.
    pub fn to_tokens(&self) -> Vec<String> {
        self.rules.iter().map(GroupRule::token).collect()
    }

    pub fn rules(&self) -> &[GroupRule] {
        &self.rules
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Group index of `entry`.
    ///
    /// `0 .. rules.len()` are the user's groups, in their order. Everything
    /// else falls back to `rules.len() + <built-in category rank>`, which puts
    /// the untouched categories below the defined ones while keeping their
    /// familiar relative order.
    ///
    /// The indices are not contiguous — a fallback category nothing belongs to
    /// simply never appears — which is all the renderer needs, since it only
    /// asks whether two neighbouring rows share a group.
    pub fn group_of(&self, entry: &Entry) -> u32 {
        self.group_of_parts(entry.is_dir, entry.kind, &crate::fs::ops::ext_of(&entry.name))
    }

    /// [`Self::group_of`] for the fields a rendered row still carries.
    ///
    /// The list view lays rows out long after the `Entry` is gone, so the group
    /// is recomputed from the three things a row keeps: is it a folder, its
    /// kind, and its extension.
    pub fn group_of_parts(&self, is_dir: bool, kind: FileKind, ext: &str) -> u32 {
        if let Some(index) = self
            .rules
            .iter()
            .position(|rule| rule.matches_parts(is_dir, kind, ext))
        {
            return index as u32;
        }
        self.rules.len() as u32 + u32::from(type_group_rank(kind))
    }
}

fn parse_token(token: &str) -> Option<GroupRule> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    // `split_once` keeps an extension that itself contains a colon, which a
    // Windows alternate data stream would: `ext:file:stream` round-trips.
    if let Some(rest) = token.strip_prefix("kind:") {
        return kind_from_code(&rest.trim().to_ascii_lowercase()).map(GroupRule::Kind);
    }
    if let Some(rest) = token.strip_prefix("ext:") {
        return clean_extensions(rest).map(GroupRule::Exts);
    }
    // Hand-written shorthand.
    let lowered = token.to_ascii_lowercase();
    if let Some(kind) = kind_from_code(&lowered) {
        return Some(GroupRule::Kind(kind));
    }
    clean_extensions(token).map(GroupRule::Exts)
}

/// Splits a rule's payload into the extensions it bundles.
///
/// Accepts commas, semicolons and spaces as separators (`doc,docx` reads the
/// same as `doc docx`), normalizes each extension and drops the repeats.
fn clean_extensions(raw: &str) -> Option<Vec<String>> {
    let mut exts: Vec<String> = Vec::new();
    for piece in raw.split([',', ';', ' ', '\t']) {
        let Some(ext) = clean_extension(piece) else {
            continue;
        };
        if exts.len() == MAX_EXTS_PER_RULE {
            break;
        }
        if !exts.iter().any(|kept| kept.eq_ignore_ascii_case(&ext)) {
            exts.push(ext);
        }
    }
    (!exts.is_empty()).then_some(exts)
}

fn clean_extension(raw: &str) -> Option<String> {
    let ext = raw.trim().trim_start_matches('.').to_ascii_lowercase();
    // A separator or a quote would never match a real extension and suggests a
    // mistaken line rather than a group the user meant to define.
    if ext.is_empty()
        || ext
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '\\' | '"' | '\''))
    {
        return None;
    }
    Some(ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from(name),
            size_bytes: if is_dir { None } else { Some(0) },
            mtime_unix: None,
            created_unix: None,
            is_dir,
            kind: crate::fs::classify_kind(
                std::path::Path::new(name)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .filter(|_| !is_dir),
                is_dir,
            ),
            hidden: false,
            is_symlink: false,
            executable: false,
        }
    }

    fn order(tokens: &[&str]) -> TypeGroupOrder {
        TypeGroupOrder::from_tokens(&tokens.iter().map(|t| t.to_string()).collect::<Vec<_>>())
    }

    /// The list the user asked for, in the order they wrote it.
    #[test]
    fn rules_are_used_in_the_order_they_were_written() {
        let groups = order(&[
            "kind:folder",
            "ext:md",
            "ext:pdf",
            "ext:txt",
            "ext:xlsx",
            "ext:docx",
            "ext:ppt",
            "ext:svg",
            "ext:png",
        ]);
        assert_eq!(groups.group_of(&entry("dir", true)), 0);
        assert_eq!(groups.group_of(&entry("note.md", false)), 1);
        assert_eq!(groups.group_of(&entry("paper.pdf", false)), 2);
        assert_eq!(groups.group_of(&entry("list.txt", false)), 3);
        assert_eq!(groups.group_of(&entry("icon.svg", false)), 7);
        assert_eq!(groups.group_of(&entry("shot.png", false)), 8);
    }

    /// An extension named by a rule leaves the category that used to hold it:
    /// md is no longer in the "document" fallback once it has its own group.
    #[test]
    fn a_named_extension_leaves_its_category() {
        let groups = order(&["ext:md", "kind:document"]);
        let md = groups.group_of(&entry("note.md", false));
        let docx = groups.group_of(&entry("report.docx", false));
        assert_eq!(md, 0);
        assert_eq!(docx, 1);
    }

    /// Everything unmatched keeps its built-in category, and every such
    /// category sits below the rules.
    #[test]
    fn unmatched_entries_fall_back_below_every_rule() {
        let groups = order(&["ext:md", "ext:pdf"]);
        let folder = groups.group_of(&entry("dir", true));
        let program = groups.group_of(&entry("setup.exe", false));
        let image = groups.group_of(&entry("shot.png", false));
        let unknown = groups.group_of(&entry("data.bin", false));

        // Two rules, so fallback starts at 2 and follows the built-in ranks.
        assert_eq!(folder, 2 + u32::from(type_group_rank(FileKind::Folder)));
        assert_eq!(program, 2 + u32::from(type_group_rank(FileKind::Application)));
        assert_eq!(image, 2 + u32::from(type_group_rank(FileKind::Image)));
        assert_eq!(unknown, 2 + u32::from(type_group_rank(FileKind::File)));
        for index in [folder, program, image, unknown] {
            assert!(index >= 2, "fallback always sorts below the rules");
        }
        // …and the built-in order survives inside the fallback.
        assert!(folder < program && program < image && image < unknown);
    }

    /// A folder is claimed by `kind:folder` even when its name ends in an
    /// extension a rule names.
    #[test]
    fn an_extension_rule_never_claims_a_folder() {
        let groups = order(&["ext:md", "kind:folder"]);
        assert_eq!(groups.group_of(&entry("notes.md", true)), 1);
        assert_eq!(groups.group_of(&entry("notes.md", false)), 0);
    }

    /// An empty order is the "feature off" state: every entry keeps the
    /// built-in category it always had.
    #[test]
    fn an_empty_order_reproduces_the_built_in_categories() {
        let groups = TypeGroupOrder::default();
        assert!(groups.is_empty());
        for (name, is_dir) in [
            ("dir", true),
            ("setup.exe", false),
            ("shot.png", false),
            ("note.md", false),
            ("settings.toml", false),
            ("data.bin", false),
        ] {
            let item = entry(name, is_dir);
            assert_eq!(groups.group_of(&item), u32::from(type_group_rank(item.kind)));
        }
    }

    /// Round-tripping through the config keeps the list, the order and the
    /// meaning of every rule.
    #[test]
    fn tokens_round_trip_through_the_config() {
        let tokens = [
            "kind:folder",
            "ext:md",
            "ext:pdf",
            "kind:image",
            "ext:svg",
        ];
        let groups = order(&tokens);
        assert_eq!(groups.to_tokens(), tokens);
        assert_eq!(TypeGroupOrder::from_tokens(&groups.to_tokens()), groups);
    }

    /// Hand-written shorthand, case and a leading dot are all accepted; junk is
    /// dropped rather than failing the whole order.
    #[test]
    fn hand_written_tokens_are_normalized() {
        let groups = order(&[
            "folder",     // bare category
            "ext:.MD",    // leading dot + upper case
            "  pdf  ",    // surrounding space
            "Image",      // bare category, mixed case
            "",           // empty
            "  ",         // blank
            "ext:",       // no extension
            "fix/me",     // a path, not an extension
            "folder",     // duplicate
        ]);
        assert_eq!(
            groups.to_tokens(),
            vec!["kind:folder", "ext:md", "ext:pdf", "kind:image"]
        );
    }

    #[test]
    fn the_order_stops_growing_at_the_limit() {
        let tokens: Vec<String> = (0..MAX_RULES + 10)
            .map(|index| format!("ext:e{index}"))
            .collect();
        let groups = TypeGroupOrder::from_tokens(&tokens);
        assert_eq!(groups.len(), MAX_RULES);
    }

    /// One rule may bundle a whole format family, and every member lands in the
    /// SAME group — no separator between two halves of one thing.
    #[test]
    fn a_format_family_is_a_single_group() {
        let groups = order(&[
            "kind:folder",
            "ext:doc,docx",
            "ext:ppt,pptx",
            "ext:xls,xlsx",
        ]);
        assert_eq!(groups.group_of(&entry("a.doc", false)), 1);
        assert_eq!(groups.group_of(&entry("b.docx", false)), 1);
        assert_eq!(groups.group_of(&entry("c.ppt", false)), 2);
        assert_eq!(groups.group_of(&entry("d.pptx", false)), 2);
        assert_eq!(groups.group_of(&entry("e.xls", false)), 3);
        assert_eq!(groups.group_of(&entry("f.xlsx", false)), 3);
        // Four groups, not six.
        assert_eq!(groups.len(), 4);
    }

    /// The separators between members of a bundle are accepted in any spelling
    /// and settle on the canonical comma form.
    #[test]
    fn bundled_extensions_normalize_to_one_canonical_token() {
        for raw in [
            "ext:doc,docx",
            "ext:doc, docx",
            "ext:DOC; .docx",
            "ext:doc  docx",
            "ext:docx,doc", // order is the author's, kept as written
        ] {
            let groups = order(&[raw]);
            assert_eq!(groups.len(), 1, "{raw} must stay one rule");
        }
        // Repeats collapse, and the first spelling wins.
        let groups = order(&["ext:doc,DOC,docx,.docx"]);
        assert_eq!(groups.to_tokens(), vec!["ext:doc,docx"]);
    }

    /// A bundle still never claims a folder, however its name ends.
    #[test]
    fn a_bundle_never_claims_a_folder() {
        let groups = order(&["ext:doc,docx", "kind:folder"]);
        assert_eq!(groups.group_of(&entry("archive.docx", true)), 1);
        assert_eq!(groups.group_of(&entry("archive.docx", false)), 0);
    }

    /// The shipped default is the order the user asked for, bundled by family.
    #[test]
    fn the_default_order_bundles_the_office_families() {
        let groups = default_order();
        assert_eq!(groups.to_tokens(), DEFAULT_TOKENS);
        assert_eq!(groups.group_of(&entry("note.md", false)), 1);
        assert_eq!(groups.group_of(&entry("paper.pdf", false)), 2);
        assert_eq!(groups.group_of(&entry("list.txt", false)), 3);
        // One Excel group, one Word group, one PowerPoint group.
        assert_eq!(
            groups.group_of(&entry("book.xlsx", false)),
            groups.group_of(&entry("old.xls", false))
        );
        assert_eq!(
            groups.group_of(&entry("spec.docx", false)),
            groups.group_of(&entry("old.doc", false))
        );
        assert_eq!(
            groups.group_of(&entry("talk.pptx", false)),
            groups.group_of(&entry("old.ppt", false))
        );
        // Images and archives are whole categories, executables their own group.
        let image = groups.group_of(&entry("logo.png", false));
        let archive = groups.group_of(&entry("bundle.zip", false));
        let exe = groups.group_of(&entry("setup.exe", false));
        assert!(image < archive && archive < exe);
    }

    /// The row-shaped entry point must agree with the `Entry` one: the list
    /// lays rows out from the row's own fields, and a disagreement would put
    /// the separator somewhere other than where the sort put the boundary.
    #[test]
    fn the_row_view_and_the_entry_view_agree() {
        let groups = default_order();
        for (name, is_dir) in [
            ("dir", true),
            ("a.md", false),
            ("b.pdf", false),
            ("c.doc", false),
            ("d.docx", false),
            ("e.ppt", false),
            ("f.xlsx", false),
            ("g.png", false),
            ("h.zip", false),
            ("i.exe", false),
            ("j.bin", false),
        ] {
            let item = entry(name, is_dir);
            let ext = crate::fs::ops::ext_of(&item.name);
            assert_eq!(
                groups.group_of(&item),
                groups.group_of_parts(is_dir, item.kind, &ext),
                "{name} must land in the same group either way"
            );
        }
    }

    #[test]
    fn kind_codes_round_trip_for_every_kind() {
        for kind in [
            FileKind::Folder,
            FileKind::File,
            FileKind::Application,
            FileKind::Archive,
            FileKind::Audio,
            FileKind::Document,
            FileKind::Image,
            FileKind::Video,
            FileKind::Config,
        ] {
            assert_eq!(kind_from_code(kind_code(kind)), Some(kind));
        }
        assert_eq!(kind_from_code("nonsense"), None);
    }
}
