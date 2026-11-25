use super::*;
use std::{cell::RefCell, fs, io::Write};
use sunrise_mod_package::{Package, manifest::sha512_of};

/// Modrinth as the check sees it: the jars it lists by SHA-512, and every URL asked for.
#[derive(Default)]
struct Listed {
    jars: Vec<(String, Vec<u8>)>,
    asked: RefCell<Vec<String>>,
}

impl Listed {
    fn url(sha512: &str) -> String {
        format!("https://cdn.modrinth.com/data/{sha512}/original.jar")
    }
}

impl Http for Listed {
    fn get(&self, url: &str) -> Result<Option<Box<dyn Read>>, String> {
        self.asked.borrow_mut().push(url.to_owned());
        Ok(self
            .jars
            .iter()
            .find(|(sha512, _)| Self::url(sha512) == url)
            .map(|(_, bytes)| Box::new(std::io::Cursor::new(bytes.clone())) as Box<dyn Read>))
    }

    fn post_json(&self, url: &str, _: &str) -> Result<Box<dyn Read>, String> {
        self.asked.borrow_mut().push(url.to_owned());
        let listed: serde_json::Map<_, _> = self
            .jars
            .iter()
            .map(|(sha512, _)| {
                let file =
                    serde_json::json!({"hashes": {"sha512": sha512}, "url": Self::url(sha512)});
                (sha512.clone(), serde_json::json!({"files": [file]}))
            })
            .collect();
        Ok(Box::new(std::io::Cursor::new(
            serde_json::Value::Object(listed).to_string(),
        )))
    }
}

/// A jar holding `files`, each empty but the English names.
fn jar(files: &[&str]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for name in files {
        zip.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        if name.ends_with("en_us.json") {
            zip.write_all(br#"{"block.gb.slab":"Slab"}"#).unwrap();
        }
    }
    zip.finish().unwrap().into_inner()
}

/// What the check says of a mod whose one block draws with a model, of which the mod holds
/// nothing but the block, when its manifest lists `original` and Modrinth lists `listed`.
fn checked(original: &[u8], listed: Option<&[u8]>) -> (Result<(), String>, Vec<String>) {
    let sha512 = sha512_of(original);
    let smod = tempfile::tempdir().unwrap();
    fs::write(
        smod.path().join("sunrise-mod.toml"),
        format!(
            "id = \"gb\"\nname = \"gb\"\nversion = \"0.1.0\"\napi = \"0.2\"\nside = \"both\"\n\
             capabilities = [\"registries\"]\n\
             [[jar]]\nid = \"original\"\nversion = \"3.1.2\"\nsha512 = \"{sha512}\"\n"
        ),
    )
    .unwrap();
    let block = smod.path().join("data/gb/sunrise/block");
    fs::create_dir_all(&block).unwrap();
    fs::write(
        block.join("slab.json"),
        r#"{"template":"minecraft:stone","properties":{"render_shape":"MODEL"}}"#,
    )
    .unwrap();
    let package = Package::read(smod.path()).unwrap();
    let modrinth = Listed {
        jars: listed
            .map(|bytes| (sha512_of(bytes), bytes.to_vec()))
            .into_iter()
            .collect(),
        ..Listed::default()
    };
    let work = tempfile::tempdir().unwrap();
    let result = check(smod.path(), &package.manifest, work.path(), &modrinth);
    (result, modrinth.asked.into_inner())
}

const SUPPLIED: [&str; 4] = [
    "assets/gb/blockstates/slab.json",
    "assets/gb/models/block/slab.json",
    "data/gb/loot_table/blocks/slab.json",
    "assets/gb/lang/en_us.json",
];

#[test]
fn a_mod_whose_jar_supplies_what_it_lacks_passes() {
    let original = jar(&SUPPLIED);
    let (result, asked) = checked(&original, Some(&original));
    assert_eq!(result, Ok(()));
    assert_eq!(
        asked,
        [
            "https://api.modrinth.com/v2/version_files".to_owned(),
            Listed::url(&sha512_of(&original)),
        ]
    );
}

#[test]
fn a_jar_modrinth_does_not_list_is_named_and_never_downloaded() {
    let original = jar(&SUPPLIED);
    let (result, asked) = checked(&original, None);
    let error = result.unwrap_err();
    let named = format!(
        "original jar `original 3.1.2` (SHA-512 {})",
        sha512_of(&original)
    );
    assert!(error.starts_with(&named), "{error}");
    assert!(error.contains("not (or no longer) on Modrinth"), "{error}");
    assert_eq!(asked.len(), 1);
}

#[test]
fn a_file_neither_the_mod_nor_its_jar_holds_is_named() {
    let original = jar(&SUPPLIED[1..]);
    let error = checked(&original, Some(&original)).0.unwrap_err();
    assert!(error.contains("assets/gb/blockstates/slab.json"), "{error}");
}
