//! Bounded scan of `LEXICON/*/META.TXT` and `LEXICON/LISTS/*.WLS`.

use std::{fs, io::Read, path::Path};

use anyhow::Result;

use super::wordlist::parse_wordlist;

pub const MAX_DICTS: usize = 16;
pub const MAX_LISTS: usize = 64;
const MAX_META_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DictMeta {
    pub id: String,
    pub title: String,
    pub src_lang: String,
    pub dst_lang: String,
    pub entries: String,
    pub source_url: String,
    pub source_date: String,
    pub license: String,
    pub attribution: String,
    pub redistributable: String,
    pub builder_version: String,
    pub lex_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListMeta {
    pub name: String,
    pub dict_id: String,
    pub title: String,
    pub count: u32,
    pub path: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LexiconCatalog {
    pub dicts: Vec<DictMeta>,
    pub lists: Vec<ListMeta>,
}

pub fn scan_catalog(root: &Path) -> Result<LexiconCatalog> {
    let mut catalog = LexiconCatalog::default();
    if !root.is_dir() {
        return Ok(catalog);
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    for dir in dirs {
        if catalog.dicts.len() >= MAX_DICTS {
            break;
        }
        let name = dir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if name.eq_ignore_ascii_case("LISTS") {
            continue;
        }
        let lex = dir.join("DICT.LEX");
        let meta_path = dir.join("META.TXT");
        if !lex.is_file() {
            continue;
        }
        let mut meta = read_meta(&meta_path).unwrap_or_default();
        if meta.id.is_empty() {
            meta.id = name.to_string();
        }
        if meta.title.is_empty() {
            meta.title = meta.id.clone();
        }
        meta.lex_path = lex.display().to_string();
        catalog.dicts.push(meta);
    }
    let lists_dir = root.join("LISTS");
    if lists_dir.is_dir() {
        let mut files = Vec::new();
        for entry in fs::read_dir(&lists_dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|ext| ext.to_str()) == Some("WLS") {
                files.push(entry.path());
            }
        }
        files.sort();
        for path in files {
            if catalog.lists.len() >= MAX_LISTS {
                break;
            }
            let bytes = read_bounded(&path, 1024 * 1024)?;
            let parsed = parse_wordlist(&bytes)?;
            let name = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("LIST")
                .to_string();
            catalog.lists.push(ListMeta {
                name,
                dict_id: parsed.dict_id,
                title: parsed.title,
                count: parsed.entry_ids.len() as u32,
                path: path.display().to_string(),
            });
        }
    }
    Ok(catalog)
}

fn read_meta(path: &Path) -> Result<DictMeta> {
    let bytes = read_bounded(path, MAX_META_BYTES)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut meta = DictMeta::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "id" => meta.id = value.trim().to_string(),
            "title" => meta.title = value.trim().to_string(),
            "src_lang" => meta.src_lang = value.trim().to_string(),
            "dst_lang" => meta.dst_lang = value.trim().to_string(),
            "entries" => meta.entries = value.trim().to_string(),
            "source_url" => meta.source_url = value.trim().to_string(),
            "source_date" => meta.source_date = value.trim().to_string(),
            "license" => meta.license = value.trim().to_string(),
            "attribution" => meta.attribution = value.trim().to_string(),
            "redistributable" => meta.redistributable = value.trim().to_string(),
            "builder_version" => meta.builder_version = value.trim().to_string(),
            _ => {}
        }
    }
    Ok(meta)
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut limited = file.take(limit as u64 + 1);
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut limited, &mut bytes)?;
    if bytes.len() > limit {
        bytes.truncate(limit);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::scan_catalog;

    #[test]
    fn scans_meta_and_lists_with_caps() {
        let root = std::env::temp_dir().join(format!("rmx-cat-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("ECDICT")).unwrap();
        fs::write(root.join("ECDICT/DICT.LEX"), b"unused").unwrap();
        fs::write(
            root.join("ECDICT/META.TXT"),
            "id=ECDICT\ntitle=ECDICT\nlicense=MIT\nattribution=skywind3000/ECDICT\nsource_date=fixture\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("LISTS")).unwrap();
        fs::write(
            root.join("LISTS/MINI.WLS"),
            include_bytes!("../../tests/fixtures/lexicon/MINI.WLS"),
        )
        .unwrap();
        let catalog = scan_catalog(&root).unwrap();
        assert_eq!(catalog.dicts.len(), 1);
        assert_eq!(catalog.dicts[0].license, "MIT");
        assert_eq!(catalog.lists.len(), 1);
        assert_eq!(catalog.lists[0].dict_id, "MINI");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_root_is_empty() {
        let catalog = scan_catalog(std::path::Path::new("/no/such/lexicon")).unwrap();
        assert!(catalog.dicts.is_empty());
    }
}
