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

/// One file a package would carry, for the summary shown before exporting.
#[derive(Clone, Debug, PartialEq)]
pub struct PackedFile {
    /// The file's name.
    pub name: String,
    /// Its size in bytes; 0 when it is missing.
    pub bytes: u64,
    /// A clip on some timeline uses it (fonts always count as used).
    pub used: bool,
    /// The file is not on disk: the export would fail on it.
    pub missing: bool,
}

/// What a package of a project would hold, before anything is written.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// Every file, once each, in the bin's order, fonts last.
    pub files: Vec<PackedFile>,
}

impl Summary {
    /// The files that would go in, with or without the unused ones.
    pub fn included(&self, only_used: bool) -> impl Iterator<Item = &PackedFile> {
        self.files
            .iter()
            .filter(move |file| file.used || !only_used)
    }

    /// How many files would go in.
    pub fn count(&self, only_used: bool) -> usize {
        self.included(only_used).count()
    }

    /// Their size in bytes - about the package's, since media is stored.
    pub fn bytes(&self, only_used: bool) -> u64 {
        self.included(only_used).map(|file| file.bytes).sum()
    }

    /// The files no timeline uses: how many, and how big.
    pub fn unused(&self) -> (usize, u64) {
        self.files
            .iter()
            .filter(|file| !file.used)
            .fold((0, 0), |(count, bytes), file| {
                (count + 1, bytes + file.bytes)
            })
    }

    /// Names of the files that would go in but are not on disk.
    pub fn missing(&self, only_used: bool) -> Vec<String> {
        self.included(only_used)
            .filter(|file| file.missing)
            .map(|file| file.name.clone())
            .collect()
    }
}

/// Lists what [`export`] would pack, by looking at the files only: quick
/// enough to run before the summary sheet shows.
pub fn summarize(document: &Value) -> Result<Summary, String> {
    let Some(project) = concat_project::from_document(document) else {
        return Err("the open project is not a document this build can read".to_owned());
    };
    let used = used_media(&project);
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut summary = Summary::default();
    let mut add = |path: &str, used: bool| {
        if let Some(&index) = seen.get(path) {
            let file: &mut PackedFile = &mut summary.files[index];
            file.used |= used;
            return;
        }
        let meta = std::fs::metadata(path).ok().filter(|meta| meta.is_file());
        seen.insert(path.to_owned(), summary.files.len());
        summary.files.push(PackedFile {
            name: Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_owned()),
            bytes: meta.as_ref().map_or(0, std::fs::Metadata::len),
            used,
            missing: meta.is_none(),
        });
    };
    for item in &project.media {
        if item.placeholder && item.path.is_empty() {
            continue;
        }
        add(&item.path, used.contains(&item.id));
    }
    for font in &project.fonts {
        add(&font.path, true);
    }
    Ok(summary)
}

/// The ids of the media some clip on some timeline uses.
fn used_media(project: &concat_project::model::Project) -> std::collections::HashSet<String> {
    project
        .timelines
        .iter()
        .flat_map(|timeline| timeline.clips.iter())
        .map(|clip| clip.media_id.clone())
        .filter(|id| !id.is_empty())
        .collect()
}

/// Packs the given document - the open project's, straight from its session
/// - into a package at `output`.
///
/// `project_path` is the project folder; it donates the preview. Every media
/// file must exist: a package that cannot find its own footage would open
/// on the other side as a project full of holes, so a missing file is an
/// error that names it, not a warning. Placeholder media (an unfilled
/// template slot) has no file and travels as it is.
///
/// With `only_used`, media no clip on any timeline uses is left out - of the
/// package and of its document - as Premiere's "Exclude unused clips" and
/// Kdenlive's "Archive only timeline clips" do: the bin's leftovers are
/// often most of a project's size.
pub fn export(
    document: &Value,
    settings: &DocumentSettings,
    project_path: &str,
    output: &Path,
    only_used: bool,
) -> Result<Exported, String> {
    // Typed through the engine's reader and writer, as templates are, so a
    // schema change breaks this at compile time rather than in a package.
    let Some(mut project) = concat_project::from_document(document) else {
        return Err("the open project is not a document this build can read".to_owned());
    };
    if only_used {
        let used = used_media(&project);
        project.media.retain(|item| used.contains(&item.id));
    }

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

/// What an import does when the projects location already holds a
/// project of the package's name - the same edit coming back from another
/// device, most often.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnClash {
    /// Keep both: the import gets a numbered folder of its own.
    Copy,
    /// The import takes the existing project's place. The old folder is
    /// only removed once the new one is complete, so a failed import leaves
    /// it as it was.
    Replace,
}

/// The name of the project a package would clash with under `location` -
/// a project folder already there under the package's name - or None.
/// Cheap: only the package's document is read.
pub fn clash(package: &Path, location: &str) -> Result<Option<String>, String> {
    let (_, document) = open_package(package)?;
    let settings = settings_of(&document)?;
    let target = Path::new(location).join(projects::folder_name(&settings.name));
    Ok(projects::is_project(&target).then_some(settings.name))
}

/// Unpacks the package at `package` into a new project folder under
/// `location` and returns it, ready for `Studio::open_project`.
///
/// The folder is named after the project; if that name is taken - the same
/// edit imported twice, or a project of the same name already there - a
/// number is added rather than overwriting anything. See [`import_as`] to
/// replace instead.
pub fn import(package: &Path, location: &str) -> Result<projects::ProjectInfo, String> {
    import_as(package, location, OnClash::Copy)
}

/// [`import`], saying what to do when a project of the package's name is
/// already under `location`.
///
/// The caller makes sure a project being replaced is not open anywhere.
pub fn import_as(
    package: &Path,
    location: &str,
    on_clash: OnClash,
) -> Result<projects::ProjectInfo, String> {
    let (mut archive, mut document) = open_package(package)?;
    let settings = settings_of(&document)?;
    let base = Path::new(location);
    let target = base.join(projects::folder_name(&settings.name));
    let replacing = on_clash == OnClash::Replace && projects::is_project(&target);

    // Where the project ends up, and where it is unpacked first: the same
    // folder for a copy; a scratch folder beside it when replacing, so the
    // old project stays whole until the new one is.
    let (destination, name) = if replacing {
        (target.clone(), settings.name.clone())
    } else {
        let free = free_folder(base, &settings.name);
        let name = free
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&settings.name)
            .to_owned();
        (free, name)
    };
    let work_name = if replacing {
        free_folder(base, &format!("{} importing", projects::folder_name(&name)))
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("importing")
            .to_owned()
    } else {
        name.clone()
    };
    let work = projects::create(
        location,
        &work_name,
        settings.width,
        settings.height,
        settings.rate_num,
        settings.rate_den,
    )?;
    let work_root = PathBuf::from(&work.path);

    // Anything that goes wrong from here takes the half-made folder with it.
    let unpacked = (|| {
        unpack(&mut archive, &work_root)?;

        document["name"] = Value::String(name.clone());
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
                // Against where the project will live, not the scratch
                // folder it is unpacked in.
                let absolute = destination.join(relative);
                item["path"] = Value::String(absolute.to_string_lossy().into_owned());
            }
        }

        if replacing {
            keep_unused_media(&mut document, &target, &work_root, &destination)?;
        }

        let mut editor = Editor::from_document(&document).ok_or(
            "this package holds a project this build cannot read (made by a newer Concat?)",
        )?;
        let catalogue = concat_effects::Catalogue::builtin();
        editor.upgrade_links(|link| catalogue.upgrade(link));
        let settings = DocumentSettings {
            name: name.clone(),
            ..settings
        };
        projects::save(&work.path, &editor.to_document(&settings))?;

        let preview = work_root.join(PREVIEW);
        if preview.is_file() {
            let cache = work_root.join("cache");
            let _ = std::fs::create_dir_all(&cache);
            let _ = std::fs::rename(&preview, cache.join(PREVIEW));
        }
        Ok(())
    })();

    if let Err(error) = unpacked {
        let _ = std::fs::remove_dir_all(&work_root);
        return Err(error);
    }
    if !replacing {
        return Ok(work);
    }

    // The swap: the old project steps aside, the new one moves in, and only
    // then is the old one removed. A failed move puts the old one back.
    let aside = free_folder(base, &format!("{} replaced", projects::folder_name(&name)));
    if let Err(error) = std::fs::rename(&target, &aside) {
        let _ = std::fs::remove_dir_all(&work_root);
        return Err(format!("could not replace {}: {error}", target.display()));
    }
    if let Err(error) = std::fs::rename(&work_root, &target) {
        let _ = std::fs::rename(&aside, &target);
        let _ = std::fs::remove_dir_all(&work_root);
        return Err(format!("could not replace {}: {error}", target.display()));
    }
    let _ = std::fs::remove_dir_all(&aside);
    projects::open(&target.to_string_lossy())
}

/// Replacing a project keeps its bin: media the old project had that the
/// package does not - most often the unused files a package made with
/// "only used media" left behind - is added to the new document, so a
/// round trip through a phone never empties the bin. A file that lived
/// inside the old project folder is copied into the new one first (that
/// folder is about to go); one outside it is referenced where it is. A
/// file that is gone is not kept.
fn keep_unused_media(
    document: &mut Value,
    old_root: &Path,
    work_root: &Path,
    destination: &Path,
) -> Result<(), String> {
    let Ok(Some(old)) = projects::read_document(&old_root.to_string_lossy()) else {
        return Ok(()); // nothing readable to keep
    };
    let Some(old_media) = old.get("media").and_then(Value::as_array) else {
        return Ok(());
    };
    let media = document
        .get_mut("media")
        .and_then(Value::as_array_mut)
        .ok_or("the package's project has no media list")?;
    let paths: std::collections::HashSet<String> = media
        .iter()
        .filter_map(|item| item.get("path").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let mut ids: std::collections::HashSet<String> = media
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();

    for item in old_media {
        let Some(path) = item.get("path").and_then(Value::as_str) else {
            continue;
        };
        if path.is_empty() || paths.contains(path) {
            continue;
        }
        let source = Path::new(path);
        if !source.is_file() {
            continue;
        }
        let kept_path = match source.strip_prefix(old_root) {
            Ok(relative) => {
                let mut relative = relative.to_path_buf();
                if work_root.join(&relative).exists() {
                    relative = Path::new(MEDIA).join("kept").join(relative);
                }
                let copy = work_root.join(&relative);
                if let Some(parent) = copy.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| format!("could not keep {path}: {error}"))?;
                }
                std::fs::copy(source, &copy)
                    .map_err(|error| format!("could not keep {path}: {error}"))?;
                destination.join(relative).to_string_lossy().into_owned()
            }
            Err(_) => path.to_owned(),
        };
        let mut kept = item.clone();
        let id = item.get("id").and_then(Value::as_str).unwrap_or("m");
        let mut fresh = id.to_owned();
        let mut n = 2;
        while ids.contains(&fresh) {
            fresh = format!("{id}k{n}");
            n += 1;
        }
        ids.insert(fresh.clone());
        kept["id"] = Value::String(fresh);
        kept["path"] = Value::String(kept_path);
        media.push(kept);
    }
    Ok(())
}

/// The package's archive, and its document parsed.
fn open_package(package: &Path) -> Result<(ZipArchive<BufReader<File>>, Value), String> {
    let file = File::open(package)
        .map_err(|error| format!("could not open {}: {error}", package.display()))?;
    let mut archive = ZipArchive::new(BufReader::new(file))
        .map_err(|error| format!("{} is not a Concat package: {error}", package.display()))?;
    let document = {
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
    Ok((archive, document))
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
        let exported = export(&doc, &settings(), source.to_str().unwrap(), &out, false).unwrap();
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
    fn replace_takes_the_old_projects_place() {
        let source = scratch("replace-source");
        let shot = source.join("shot.jpg");
        std::fs::write(&shot, b"new").unwrap();
        let doc = document(&[("m1", &shot)]);
        let out = scratch("replace-out").join("Reel.concat.zip");
        export(&doc, &settings(), source.to_str().unwrap(), &out, false).unwrap();

        let location = scratch("replace-projects");
        let location = location.to_str().unwrap();
        assert_eq!(clash(&out, location).unwrap(), None);
        let first = import(&out, location).unwrap();
        std::fs::write(Path::new(&first.path).join("old-only.txt"), b"old").unwrap();
        assert_eq!(clash(&out, location).unwrap().as_deref(), Some("Reel"));

        let replaced = import_as(&out, location, OnClash::Replace).unwrap();
        assert_eq!(replaced.path, first.path);
        assert!(!Path::new(&replaced.path).join("old-only.txt").exists());
        let opened = projects::read_document(&replaced.path).unwrap().unwrap();
        let path = opened["media"][0]["path"].as_str().unwrap().to_owned();
        assert!(path.starts_with(&replaced.path), "{path}");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        // Nothing left beside it: no scratch folder, no old copy.
        assert_eq!(std::fs::read_dir(location).unwrap().count(), 1);
    }

    #[test]
    fn summary_counts_and_only_used_leaves_the_rest_out() {
        let source = scratch("summary");
        let used = source.join("used.jpg");
        let spare = source.join("spare.jpg");
        std::fs::write(&used, b"12345").unwrap();
        std::fs::write(&spare, b"1234567890").unwrap();
        let mut doc = document(&[("m1", &used)]);
        doc["media"].as_array_mut().unwrap().push(json!({
            "id": "m2", "path": spare.to_string_lossy(), "name": "m2", "duration": 5.0,
            "kind": "image", "hasAudio": false
        }));

        let summary = summarize(&doc).unwrap();
        assert_eq!(summary.count(false), 2);
        assert_eq!(summary.bytes(false), 15);
        assert_eq!(summary.count(true), 1);
        assert_eq!(summary.bytes(true), 5);
        assert_eq!(summary.unused(), (1, 10));
        assert!(summary.missing(false).is_empty());

        let out = source.join("Reel.concat.zip");
        let exported = export(&doc, &settings(), source.to_str().unwrap(), &out, true).unwrap();
        assert_eq!(exported.files, 1);
        let location = scratch("summary-projects");
        let project = import(&out, location.to_str().unwrap()).unwrap();
        let opened = projects::read_document(&project.path).unwrap().unwrap();
        assert_eq!(opened["media"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn replacing_keeps_the_old_bins_unused_media() {
        let source = scratch("keep-source");
        let used = source.join("used.jpg");
        let spare = source.join("spare.jpg");
        std::fs::write(&used, b"used").unwrap();
        std::fs::write(&spare, b"spare").unwrap();
        let mut doc = document(&[("m1", &used)]);
        doc["media"].as_array_mut().unwrap().push(json!({
            "id": "m2", "path": spare.to_string_lossy(), "name": "m2", "duration": 5.0,
            "kind": "image", "hasAudio": false
        }));
        let location = scratch("keep-projects");
        let location = location.to_str().unwrap();

        // The full project arrives first; the spare file now lives inside it.
        let full = source.join("full.concat.zip");
        export(&doc, &settings(), source.to_str().unwrap(), &full, false).unwrap();
        let first = import(&full, location).unwrap();

        // Then a package of only the used media replaces it.
        let lean = source.join("lean.concat.zip");
        export(&doc, &settings(), source.to_str().unwrap(), &lean, true).unwrap();
        let replaced = import_as(&lean, location, OnClash::Replace).unwrap();
        assert_eq!(replaced.path, first.path);

        let opened = projects::read_document(&replaced.path).unwrap().unwrap();
        let media = opened["media"].as_array().unwrap();
        assert_eq!(media.len(), 2, "the unused file is still in the bin");
        for item in media {
            let path = item["path"].as_str().unwrap();
            assert!(Path::new(path).is_file(), "{path} is gone");
            assert!(path.starts_with(&replaced.path), "{path}");
        }
    }

    #[test]
    fn a_missing_file_refuses_the_export_and_leaves_nothing() {
        let source = scratch("missing");
        let doc = document(&[("m1", &source.join("gone.jpg"))]);
        let out = source.join("Reel.concat.zip");
        assert!(export(&doc, &settings(), source.to_str().unwrap(), &out, false).is_err());
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
