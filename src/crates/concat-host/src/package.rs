// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! Portable project packages: one `.zip` that carries a whole edit from one
//! machine to another - a desktop to a phone and back.
//!
//! A project folder points at its media by absolute path, and a phone
//! cannot read the desktop's paths (nor, without a permission this app does
//! not ask for, the phone's own Download folder). So a package is the
//! project made self-contained:
//!
//! - `concat.json` - the ordinary project document, with every media and
//!   font path rewritten relative to the package (`media/...`, `fonts/...`).
//! - `media/`, `fonts/` - the files behind them, copied in once each.
//! - `preview.jpg` - the project's cached preview, when it has one.
//!
//! Importing unpacks a package into a fresh project folder under the
//! projects location and turns the relative paths back into absolute ones
//! there, so the editor opens it like any other project - the same shape as
//! a template's `assets/` (see templates.rs), minus the slots.
//!
//! `cache/` is never packed: proxies, masks and waveforms are rebuilt from
//! the media on the other side.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use concat_project::{DocumentSettings, Editor};
use serde_json::Value;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::projects;

const DOCUMENT: &str = "concat.json";
const MEDIA: &str = "media";
const FONTS: &str = "fonts";
const PREVIEW: &str = "preview.jpg";

/// The file extension a package is saved with. Plain `.zip`, so a phone's
/// file manager and share sheet know what it is.
pub const EXTENSION: &str = "zip";

/// What a finished export wrote.
#[derive(Clone, Debug)]
pub struct Exported {
    /// The package file.
    pub path: PathBuf,
    /// How many media and font files went in.
    pub files: usize,
    /// The package's size in bytes.
    pub bytes: u64,
}

/// The file name a package of a project called `name` is offered under.
pub fn file_name(name: &str) -> String {
    format!("{}.concat.{EXTENSION}", projects::folder_name(name))
}

/// Packs the given document - the open project's, straight from its session
/// - into a package at `output`.
///
/// `project_path` is the project folder; it donates the preview. Every media
/// file must exist: a package that cannot find its own footage would open
/// on the other side as a project full of holes, so a missing file is an
/// error that names it, not a warning. Placeholder media (an unfilled
/// template slot) has no file and travels as it is.
pub fn export(
    document: &Value,
    settings: &DocumentSettings,
    project_path: &str,
    output: &Path,
) -> Result<Exported, String> {
    // Typed through the engine's reader and writer, as templates are, so a
    // schema change breaks this at compile time rather than in a package.
    let Some(mut project) = concat_project::from_document(document) else {
        return Err("the open project is not a document this build can read".to_owned());
    };

    // Written next to the destination and renamed into place at the end: an
    // export that fails halfway leaves no truncated package behind.
    let temporary = output.with_extension("zip.saving");
    let file = File::create(&temporary)
        .map_err(|error| format!("could not create {}: {error}", temporary.display()))?;
    let mut zip = ZipWriter::new(BufWriter::new(file));

    let result = (|| {
        let mut packed = Packer::default();
        for item in &mut project.media {
            if item.placeholder && item.path.is_empty() {
                continue;
            }
            item.path = packed.add(&mut zip, MEDIA, &item.path)?;
        }
        for font in &mut project.fonts {
            font.path = packed.add(&mut zip, FONTS, &font.path)?;
        }

        let document = concat_project::to_document(settings, &project);
        let encoded = serde_json::to_vec_pretty(&document)
            .map_err(|error| format!("could not encode the project: {error}"))?;
        zip.start_file(DOCUMENT, deflated())
            .and_then(|()| zip.write_all(&encoded).map_err(Into::into))
            .map_err(|error| format!("could not write the project: {error}"))?;

        // Best effort: a package without a preview opens all the same.
        let preview = Path::new(project_path).join("cache").join(PREVIEW);
        if let Ok(bytes) = std::fs::read(&preview)
            && zip.start_file(PREVIEW, stored()).is_ok()
        {
            let _ = zip.write_all(&bytes);
        }
        Ok(packed.count)
    })();

    let finished = match result {
        Ok(count) => zip
            .finish()
            .map_err(|error| format!("could not finish the package: {error}"))
            .and_then(|writer| {
                writer
                    .into_inner()
                    .map_err(|error| format!("could not finish the package: {error}"))
            })
            .map(|_| count),
        Err(error) => Err(error),
    };
    let count = match finished {
        Ok(count) => count,
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
    };

    std::fs::rename(&temporary, output).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("could not write {}: {error}", output.display())
    })?;
    let bytes = std::fs::metadata(output)
        .map(|meta| meta.len())
        .unwrap_or(0);
    Ok(Exported {
        path: output.to_path_buf(),
        files: count,
        bytes,
    })
}

/// Copies each source file into the package once, however many media items
/// share it, and hands back its package-relative path.
#[derive(Default)]
struct Packer {
    /// Source path to package path.
    seen: HashMap<String, String>,
    /// Package paths already taken, so two `clip.mp4`s from two folders do
    /// not overwrite each other.
    taken: HashMap<String, ()>,
    count: usize,
}

impl Packer {
    fn add<W: Write + std::io::Seek>(
        &mut self,
        zip: &mut ZipWriter<W>,
        folder: &str,
        source: &str,
    ) -> Result<String, String> {
        if let Some(packed) = self.seen.get(source) {
            return Ok(packed.clone());
        }
        if source.is_empty() {
            return Err(
                "a media item has no file behind it; fill or remove it before exporting".to_owned(),
            );
        }
        let mut input = File::open(source)
            .map(BufReader::new)
            .map_err(|error| format!("could not read {source}: {error}"))?;

        let base = Path::new(source)
            .file_name()
            .and_then(|name| name.to_str())
            .map(projects::folder_name)
            .unwrap_or_else(|| "file".to_owned());
        let mut packed = format!("{folder}/{base}");
        let mut n = 2;
        while self.taken.contains_key(&packed) {
            packed = format!("{folder}/{n}-{base}");
            n += 1;
        }

        // Video, audio and pictures are compressed already: storing them is
        // as small and many times faster, which matters on a phone.
        zip.start_file(packed.as_str(), stored())
            .map_err(|error| format!("could not add {source}: {error}"))?;
        std::io::copy(&mut input, zip)
            .map_err(|error| format!("could not add {source}: {error}"))?;

        self.taken.insert(packed.clone(), ());
        self.seen.insert(source.to_owned(), packed.clone());
        self.count += 1;
        Ok(packed)
    }
}

fn stored() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .large_file(true)
}

fn deflated() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)
}

/// Unpacks the package at `package` into a new project folder under
/// `location` and returns it, ready for `Studio::open_project`.
///
/// The folder is named after the project; if that name is taken - the same
/// edit imported twice, or a project of the same name already there - a
/// number is added rather than overwriting anything.
pub fn import(package: &Path, location: &str) -> Result<projects::ProjectInfo, String> {
    let file = File::open(package)
        .map_err(|error| format!("could not open {}: {error}", package.display()))?;
    let mut archive = ZipArchive::new(BufReader::new(file))
        .map_err(|error| format!("{} is not a Concat package: {error}", package.display()))?;

    let mut document: Value = {
        let mut entry = archive.by_name(DOCUMENT).map_err(|_| {
            format!(
                "{} is not a Concat package: it has no {DOCUMENT}",
                package.display()
            )
        })?;
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| format!("could not read the package: {error}"))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("the package's project does not parse: {error}"))?
    };

    let settings = settings_of(&document)?;
    let root = free_folder(Path::new(location), &settings.name);
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&settings.name)
        .to_owned();
    let project = projects::create(
        location,
        &name,
        settings.width,
        settings.height,
        settings.rate_num,
        settings.rate_den,
    )?;
    let root = PathBuf::from(&project.path);

    // Anything that goes wrong from here takes the half-made folder with it.
    let unpacked = (|| {
        unpack(&mut archive, &root)?;

        document["name"] = Value::String(project.name.clone());
        for section in ["media", "fonts"] {
            let Some(items) = document.get_mut(section).and_then(Value::as_array_mut) else {
                continue;
            };
            for item in items {
                let Some(path) = item.get("path").and_then(Value::as_str) else {
                    continue;
                };
                if path.is_empty() {
                    continue; // a placeholder slot
                }
                let Some(relative) = relative_inside(path) else {
                    return Err(format!("the package points outside itself: {path}"));
                };
                let absolute = root.join(relative);
                item["path"] = Value::String(absolute.to_string_lossy().into_owned());
            }
        }

        let mut editor = Editor::from_document(&document).ok_or(
            "this package holds a project this build cannot read (made by a newer Concat?)",
        )?;
        let catalogue = concat_effects::Catalogue::builtin();
        editor.upgrade_links(|link| catalogue.upgrade(link));
        let settings = DocumentSettings {
            name: project.name.clone(),
            ..settings
        };
        projects::save(&project.path, &editor.to_document(&settings))?;

        let preview = root.join(PREVIEW);
        if preview.is_file() {
            let cache = root.join("cache");
            let _ = std::fs::create_dir_all(&cache);
            let _ = std::fs::rename(&preview, cache.join(PREVIEW));
        }
        Ok(())
    })();

    if let Err(error) = unpacked {
        let _ = std::fs::remove_dir_all(&root);
        return Err(error);
    }
    Ok(project)
}

/// The output format a package's document declares.
fn settings_of(document: &Value) -> Result<DocumentSettings, String> {
    let video = document
        .get("video")
        .ok_or("the package's project names no output format")?;
    let number = |field: &str| {
        video
            .get(field)
            .and_then(Value::as_i64)
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("the package's project has no usable {field}"))
    };
    Ok(DocumentSettings {
        name: document
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("Imported project")
            .to_owned(),
        width: u32::try_from(number("width")?).map_err(|_| "the width is out of range")?,
        height: u32::try_from(number("height")?).map_err(|_| "the height is out of range")?,
        rate_num: number("rateNum")?,
        rate_den: number("rateDen")?,
    })
}

/// A project folder under `location` that does not exist yet, named after
/// `name`: `name`, then `name 2`, `name 3`...
fn free_folder(location: &Path, name: &str) -> PathBuf {
    let base = projects::folder_name(name);
    let mut candidate = location.join(&base);
    let mut n = 2;
    while candidate.exists() {
        candidate = location.join(format!("{base} {n}"));
        n += 1;
    }
    candidate
}

/// Writes every entry but the document into `root`.
///
/// Each name is checked before anything is written: an entry that climbs out
/// of the folder (`../`, an absolute path) refuses the whole package, so a
/// crafted zip cannot drop files elsewhere on the machine.
fn unpack<R: Read + std::io::Seek>(archive: &mut ZipArchive<R>, root: &Path) -> Result<(), String> {
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("could not read the package: {error}"))?;
        if entry.is_dir() || entry.name() == DOCUMENT {
            continue;
        }
        let Some(relative) = entry.enclosed_name().filter(|path| is_plain(path)) else {
            return Err(format!(
                "the package holds an unsafe file name: {}",
                entry.name()
            ));
        };
        let destination = root.join(relative);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        let mut output = File::create(&destination)
            .map(BufWriter::new)
            .map_err(|error| format!("could not write {}: {error}", destination.display()))?;
        std::io::copy(&mut entry, &mut output)
            .and_then(|_| output.flush())
            .map_err(|error| format!("could not write {}: {error}", destination.display()))?;
    }
    Ok(())
}

/// A package-relative path from the document, if it stays inside the package.
fn relative_inside(path: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(path.replace('\\', "/"));
    is_plain(&candidate).then_some(candidate)
}

/// Only ordinary names: no root, no drive, no `..`.
fn is_plain(path: &Path) -> bool {
    path.components().count() > 0
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "concat-package-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn document(media: &[(&str, &Path)]) -> Value {
        json!({
            "concat": "0.1.0", "version": 1, "name": "Reel",
            "video": { "width": 1080, "height": 1920, "rateNum": 30, "rateDen": 1 },
            "media": media.iter().map(|(id, path)| json!({
                "id": id, "path": path.to_string_lossy(), "name": id, "duration": 5.0,
                "kind": "image", "hasAudio": false
            })).collect::<Vec<_>>(),
            "tracks": [{ "id": "T1", "name": "Track 1", "visible": true, "muted": false }],
            "clips": media.iter().enumerate().map(|(index, (id, _))| json!({
                "id": format!("c{index}"), "trackId": "T1", "mediaId": id, "name": id,
                "kind": "image", "start": index as f64 * 5.0, "duration": 5.0,
                "sourceStart": 0.0, "volume": 1.0, "fadeIn": 0.0, "fadeOut": 0.0,
                "scale": 1.0, "offsetX": 0.0, "offsetY": 0.0, "rotation": 0.0,
                "opacity": 1.0, "speed": 1.0, "preservePitch": true, "filters": [],
                "videoEffects": []
            })).collect::<Vec<_>>()
        })
    }

    fn settings() -> DocumentSettings {
        DocumentSettings {
            name: "Reel".to_owned(),
            width: 1080,
            height: 1920,
            rate_num: 30,
            rate_den: 1,
        }
    }

    #[test]
    fn round_trip_makes_a_self_contained_project() {
        let source = scratch("source");
        let a = source.join("a").join("shot.jpg");
        let b = source.join("b").join("shot.jpg");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&a, b"first").unwrap();
        std::fs::write(&b, b"second").unwrap();
        // m3 shares m1's file: it must be packed once.
        let doc = document(&[("m1", &a), ("m2", &b), ("m3", &a)]);

        let out = scratch("out").join("Reel.concat.zip");
        let exported = export(&doc, &settings(), source.to_str().unwrap(), &out).unwrap();
        assert_eq!(exported.files, 2);
        assert!(out.is_file());

        let location = scratch("projects");
        let project = import(&out, location.to_str().unwrap()).unwrap();
        let opened = projects::read_document(&project.path).unwrap().unwrap();
        let paths: Vec<String> = opened["media"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["path"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0], paths[2]);
        assert_ne!(paths[0], paths[1]);
        for path in &paths {
            assert!(
                path.starts_with(&project.path),
                "{path} is not inside the project"
            );
        }
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"first");
        assert_eq!(std::fs::read(&paths[1]).unwrap(), b"second");

        // A second import never overwrites the first.
        let again = import(&out, location.to_str().unwrap()).unwrap();
        assert_ne!(again.path, project.path);
    }

    #[test]
    fn a_missing_file_refuses_the_export_and_leaves_nothing() {
        let source = scratch("missing");
        let doc = document(&[("m1", &source.join("gone.jpg"))]);
        let out = source.join("Reel.concat.zip");
        assert!(export(&doc, &settings(), source.to_str().unwrap(), &out).is_err());
        assert!(!out.exists());
        assert!(!out.with_extension("zip.saving").exists());
    }

    #[test]
    fn paths_that_climb_out_are_refused() {
        assert!(relative_inside("media/shot.jpg").is_some());
        assert!(relative_inside("../shot.jpg").is_none());
        assert!(relative_inside("/etc/passwd").is_none());
        assert!(relative_inside("media/../../x").is_none());
    }
}
