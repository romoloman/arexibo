// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Handling resources such as media and layout files.

use std::collections::HashMap;
use std::{fs, io, io::Write, path::PathBuf, sync::Arc};
use anyhow::{bail, ensure, Context, Result};
use elementtree::Element;
use md5::{Md5, Digest};
use serde::{Serialize, Deserialize};
use ureq::Agent;
use crate::{util, layout, layout::TRANSLATOR_VERSION, xmds};
use crate::config::CmsSettings;


pub type LayoutId = i64;

/// An entry in the "required files" set.
#[derive(Debug, Clone)]
pub enum ReqFile {
    File {
        id: i64,
        typ: &'static str,
        size: u64,
        md5: Vec<u8>,
        http: bool,
        path: String,
        name: String,
        code: Option<String>,
    },
    Resource {
        id: i64,
        layoutid: LayoutId,
        regionid: i64,
        mediaid: i64,
        updated: i64,
    },
}

impl ReqFile {
    pub fn description(&self) -> String {
        match self {
            ReqFile::File { typ, name, .. } => format!("{typ} {name}"),
            ReqFile::Resource { mediaid, .. } => format!("resource {mediaid}")
        }
    }

    pub fn inventory(&self) -> (&'static str, i64) {
        match self {
            ReqFile::File { id, typ, .. } => (typ, *id),
            ReqFile::Resource { id, .. } => ("resource", *id),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct LayoutInfo {
    pub id: LayoutId,
    #[serde(deserialize_with = "util::de_hex", serialize_with = "util::ser_hex")]
    pub md5: Vec<u8>,
    pub size: (i32, i32),
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub translated_version: u32,
    // Needed for Proof of Play (see stats.rs) -- #[serde(default =
    // "default_true")] so existing cached content.json entries (written
    // before this field existed) deserialize as "stats enabled", matching
    // Xibo's own documented default when the XLF attribute is absent.
    #[serde(default = "default_true")]
    pub enable_stat: bool,
}

fn default_true() -> bool { true }

#[derive(Debug, Serialize, Deserialize)]
pub struct MediaInfo {
    pub id: i64,
    pub size: u64,
    #[serde(deserialize_with = "util::de_hex", serialize_with = "util::ser_hex")]
    pub md5: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResourceInfo {
    pub id: i64,
    pub layoutid: LayoutId,
    pub regionid: i64,
    // Needed to reconstruct a ReqFile::Resource for a targeted re-download
    // (see Cache::refresh_resource) -- #[serde(default)] so existing
    // content.json caches on disk (written before this field existed)
    // still deserialize instead of erroring out on upgrade.
    #[serde(default)]
    pub mediaid: i64,
    pub updated: i64,
    pub duration: Option<f64>,
    #[serde(default)]
    pub numitems: Option<i64>,
}

/// A resource in the local cache.
#[derive(Debug, Serialize, Deserialize)]
pub enum Resource {
    Layout(Arc<LayoutInfo>),
    Media(Arc<MediaInfo>),
    Resource(Arc<ResourceInfo>),
}


pub struct Cache {
    dir: PathBuf,
    agent: Agent,
    content: HashMap<String, Resource>,
    code_map: HashMap<String, LayoutId>,
    /// Whether Adspace Exchange (`ssp` widgets) should be resolved
    /// during layout translation -- off by default (fails closed,
    /// matching the C#'s own `IsAdspaceEnabled` default), set by
    /// `mainloop.rs` whenever PlayerSettings refreshes. See
    /// `adspace.rs`'s module-level doc comment for the significant
    /// caveats on this whole feature.
    pub adspace_enabled: bool,
    /// FLAGGED AS UNVERIFIED, see adspace.rs: which SSP/partner to
    /// request from, if the CMS provides one. None = omit from the bid
    /// request entirely.
    pub adspace_partner: Option<String>,
    /// Port the embedded HTTP server (server.rs) is listening on, needed
    /// at translation time so `write_media` can build each
    /// `render="html"` widget's own absolute, sharded iframe URL (see
    /// `server::HTML_SHARD_COUNT`'s own doc comment). Set once by
    /// main.rs right after the server starts, before any layout is ever
    /// translated -- 0 is not a meaningful/valid port and would only
    /// appear here if that wiring were ever skipped by mistake.
    pub html_port: u16,
}

impl Cache {
    pub fn new(cms: &CmsSettings, dir: PathBuf, clear: bool, no_verify: bool) -> Result<Self> {
        let mut content = HashMap::new();

        if !fs::metadata(&dir).is_ok_and(|p| p.is_dir()) {
            // no directory? create it...
            fs::create_dir_all(&dir)?;
        } else if clear {
            // clear it?
            fs::remove_dir_all(&dir)?;
            fs::create_dir_all(&dir)?;
        }

        // check for a cached inventory JSON file
        if let Some(saved) = fs::File::open(dir.join("content.json"))
            .ok().and_then(|fp| serde_json::from_reader(fp).ok())
        {
            // ensure all mentioned files are present, remove missing entries
            content = saved;
            content.retain(|fname, _| dir.join(fname).is_file());

            // remove any layout descriptions if translated version is outdated
            content.retain(|_, res| match res {
                Resource::Layout(layout) => {
                    TRANSLATOR_VERSION != 0 &&   // 0 = development mode
                    layout.translated_version == TRANSLATOR_VERSION
                },
                _ => true
            });
        }

        let code_map = content.values().filter_map(|v| {
            if let Resource::Layout(info) = v {
                if let Some(code) = &info.code {
                    return Some((code.clone(), info.id));
                }
            }
            None
        }).collect();

        let cache = Self { dir, agent: cms.make_agent(no_verify)?, content, code_map,
                           adspace_enabled: false, adspace_partner: None, html_port: 0 };
        cache.install_pdfjs()?;
        Ok(cache)
    }

    /// Install bundled pdf.js files into the cache directory for the local HTTP server.
    fn install_pdfjs(&self) -> Result<()> {
        let pdfjs_dir = self.dir.join("pdfjs");
        if !pdfjs_dir.is_dir() {
            fs::create_dir_all(&pdfjs_dir)?;
        }
        let pdf_lib = pdfjs_dir.join("pdf.min.mjs");
        if !pdf_lib.is_file() {
            fs::write(&pdf_lib, PDFJS_LIB)?;
            log::info!("Installed pdf.js library ({} bytes)", PDFJS_LIB.len());
        }
        let pdf_worker = pdfjs_dir.join("pdf.worker.min.mjs");
        if !pdf_worker.is_file() {
            fs::write(&pdf_worker, PDFJS_WORKER)?;
            log::info!("Installed pdf.js worker ({} bytes)", PDFJS_WORKER.len());
        }
        Ok(())
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    pub fn has(&self, res: &ReqFile) -> bool {
        match *res {
            ReqFile::Resource { id, updated, .. } => {
                self.get_resource(id).is_some_and(|res| res.updated == updated)
            }
            ReqFile::File { ref name, ref md5, typ, id, .. } => {
                if typ == "layout" {
                    self.get_layout(id).is_some_and(|res| &res.md5 == md5)
                } else {
                    self.get_media(name).is_some_and(|res| &res.md5 == md5)
                }
            }
        }
    }

    pub fn download(&mut self, res: ReqFile, cms: &mut xmds::Cms) -> Result<()> {
        match res {
            ReqFile::Resource { id, layoutid, regionid, mediaid, updated } => {
                let data = cms.get_resource(layoutid, &regionid.to_string(),
                                            &mediaid.to_string())?;
                let fname = format!("{id}.html");

                // TODO: re-download after given updateInterval
                let duration = data.find("<!-- DURATION=").and_then(|index| {
                    data[index + 14..].find(" -->").and_then(|endindex| {
                        data[index + 14..][..endindex].parse::<f64>().ok()
                    })
                });
                let numitems = data.find("<!-- NUMITEMS=").and_then(|index| {
                    data[index + 14..].find(" -->").and_then(|endindex| {
                        data[index + 14..][..endindex].parse::<i64>().ok()
                    })
                });
                fs::write(self.dir.join(&fname), data)?;
                self.content.insert(fname, Resource::Resource(Arc::new(
                    ResourceInfo { id, layoutid, regionid, mediaid, updated, duration, numitems }
                )));
                self.save()?;
            }
            ReqFile::File { id, typ, http, size, md5, path, name, code } => {
                let filename = self.dir.join(&name);
                if http {
                    match self.download_http(&path, &filename, &md5) {
                        Ok(()) => {},
                        Err(e) => {
                            log::warn!("failing download of {name} over http, retrying \
                                        xmds: {e:#}");
                            Self::download_xmds(id, typ, size, cms, &filename, &md5)?;
                        }
                    }
                } else {
                    Self::download_xmds(id, typ, size, cms, &filename, &md5)?;
                }

                if typ == "layout" {
                    // translate the layout into HTML
                    let adspace_cfg = self.adspace_enabled.then(|| crate::adspace::AdspaceConfig {
                        agent: self.agent.clone(),
                        cache_dir: self.dir.join("adspace"),
                        partner: self.adspace_partner.clone(),
                    });
                    let xl = layout::Translator::new(
                        id,
                        &self.dir.join(&name),
                        &self.dir.join(format!("{name}.html")),
                        &self.code_map,
                        adspace_cfg,
                        self.html_port,
                    )?;
                    let (w, h, enable_stat) = xl.translate()?;
                    self.content.insert(name, Resource::Layout(Arc::new(
                        LayoutInfo { id, md5, size: (w, h), code, enable_stat,
                                     translated_version: TRANSLATOR_VERSION }
                    )));
                } else {
                    self.content.insert(name, Resource::Media(Arc::new(
                        MediaInfo { id, size, md5 }
                    )));
                }
                self.save()?;
            }
        }
        Ok(())
    }

    fn download_http(&mut self, path: &str, filename: &PathBuf,
                     md5: &[u8]) -> Result<()> {
        let body = self.agent.get(path).call()?.into_body();
        let file = io::BufWriter::new(fs::File::create(filename)?);
        let mut wrapper = HashingWriter::new(file);
        io::copy(&mut body.into_reader(), &mut wrapper)?;
        ensure!(wrapper.hash() == md5, "md5 mismatch");
        Ok(())
    }

    fn download_xmds(id: i64, typ: &str, size: u64, cms: &mut xmds::Cms,
                     filename: &PathBuf, md5: &[u8]) -> Result<()> {
        const CHUNK_SIZE: u64 = 1024 * 1024;
        let mut got_size = 0;
        let file = io::BufWriter::new(fs::File::create(filename)?);
        let mut wrapper = HashingWriter::new(file);
        while got_size < size {
            let next_size = (size - got_size).min(CHUNK_SIZE);
            let chunk = cms.get_file_data(id, typ, got_size, next_size)?;
            got_size += chunk.len() as u64;
            wrapper.write_all(&chunk)?;
        }
        ensure!(wrapper.hash() == md5, "md5 mismatch");
        Ok(())
    }

    pub fn update_code_map(&mut self, files: &[ReqFile]) -> Result<()> {
        for file in files {
            if let ReqFile::File { typ: "layout", id, code: Some(code), .. } = file {
                self.code_map.insert(code.clone(), *id);
            }
        }
        Ok(())
    }

    pub fn get_layout(&self, id: LayoutId) -> Option<Arc<LayoutInfo>> {
        self.content.get(&format!("{id}.xlf")).and_then(|entry| match entry {
            Resource::Layout(layout) => Some(layout.clone()),
            _ => None
        })
    }

    /// Whether Proof of Play should record a "layout" stat for this
    /// layout id -- true (record) if the layout isn't cached at all
    /// (fail open, matching the "default enabled" convention), since a
    /// missing/uncached layout shouldn't silently suppress stats.
    pub fn layout_enable_stat(&self, id: LayoutId) -> bool {
        self.get_layout(id).map(|info| info.enable_stat).unwrap_or(true)
    }

    fn get_media(&self, name: &str) -> Option<Arc<MediaInfo>> {
        self.content.get(name).and_then(|entry| match entry {
            Resource::Media(media) => Some(media.clone()),
            _ => None
        })
    }

    fn get_resource(&self, id: i64) -> Option<Arc<ResourceInfo>> {
        self.content.get(&format!("{id}.html")).and_then(|entry| match entry {
            Resource::Resource(res) => Some(res.clone()),
            _ => None
        })
    }

    /// Force a re-download of a single already-cached resource widget,
    /// bypassing the normal `has()`/`updated`-timestamp check -- used for
    /// the XMR `dataUpdate` notification (see xmr.rs/mainloop.rs), which
    /// tells us a specific widget's server-rendered content changed,
    /// identified only by its id (== the CMS's `widgetId`, which recent
    /// Xibo versions use interchangeably with `mediaId` for this purpose).
    /// Requires the resource to already be in the cache (from a previous
    /// full collection) since layoutid/regionid/mediaid aren't part of
    /// the XMR message itself and have to come from there.
    /// Returns the id of the resource that was actually re-downloaded --
    /// normally just `id` itself, but see the second fallback branch
    /// below for why this can genuinely differ from the requested `id`.
    /// Callers that reload/refresh whatever's currently on screen (see
    /// mainloop.rs's `DataUpdate` handling) need the *returned* id, not
    /// the one they originally asked for: a nested widget has no DOM
    /// element of its own to reload (there's no separate `<iframe
    /// id="m{id}">` for it at all -- see write_media's `Some("html")`
    /// branch, which only ever creates one for a widget that's actually
    /// its own `<media>` element), only its *container*'s does.
    pub fn refresh_resource(&mut self, id: i64, cms: &mut xmds::Cms) -> Result<i64> {
        let (fetch_id, layoutid, regionid, mediaid, updated) =
        if let Some(info) = self.get_resource(id) {
            (id, info.layoutid, info.regionid, info.mediaid, info.updated)
        } else if let Some((layoutid, regionid, mediaid, updated)) =
            self.find_widget_layout_region(id)
        {
            // Dataset-bound (DataSet View) widgets never appear in
            // RequiredFiles at all, even though neighboring resources
            // in the same region/layout do -- a genuine CMS-side
            // omission for this widget type. Fall back to searching
            // cached layout XLFs for the <media id> element, reading
            // its parent region directly.
            (id, layoutid, regionid, mediaid, updated)
        } else if let Some((fetch_id, layoutid, regionid, mediaid, updated)) =
            self.find_nested_widget_resource(id)
        {
            // Some widgets aren't standalone <media> elements at all --
            // the Elements designer can combine several into one
            // resource file (a single {id}.html with a widgetData/
            // elements JSON array covering multiple widget ids).
            // Refreshing the nested widget's own id makes no sense --
            // search cached resources for a "widgetId":{id} JSON entry
            // and refresh the containing resource instead.
            log::info!("widget {id} isn't its own resource, but is nested inside \
                        resource {fetch_id}'s own combined HTML -- refreshing that instead");
            (fetch_id, layoutid, regionid, mediaid, updated)
        } else {
            bail!("resource {id} not in cache, not found in any cached layout's own XLF, \
                   and not nested inside any cached resource's own HTML either (was it \
                   ever downloaded via a full collection?)");
        };
        self.download(ReqFile::Resource { id: fetch_id, layoutid, regionid, mediaid, updated }, cms)?;
        Ok(fetch_id)
    }

    /// See `refresh_resource`'s own doc comment on the second fallback
    /// branch. Returns `(fetch_id, layoutid, regionid, mediaid, updated)`
    /// for whichever *cached resource* contains `widget_id` nested
    /// inside its own combined JSON -- `fetch_id` is that container
    /// resource's own id (what actually needs re-downloading), not
    /// `widget_id` itself (which has no resource of its own to fetch).
    fn find_nested_widget_resource(&self, widget_id: i64) -> Option<(i64, LayoutId, i64, i64, i64)> {
        let needle = format!("\"widgetId\":{widget_id}");
        for (fname, res) in &self.content {
            let Resource::Resource(info) = res else { continue };
            // Deliberately `continue` (not `?`/early-return) on any
            // per-resource read failure -- one unreadable cached file
            // must not stop the search across the *other* resources we
            // do have cached (same lesson as `find_widget_layout_region`
            // just above).
            let Ok(content) = fs::read_to_string(self.dir.join(fname)) else { continue };
            if content.contains(&needle) {
                return Some((info.id, info.layoutid, info.regionid, info.mediaid, info.updated));
            }
        }
        None
    }

    /// Fallback used when a widget's resource was never listed in the
    /// CMS's own RequiredFiles response at all (see `refresh_resource`'s
    /// own doc comment for why this is a real, confirmed gap rather than
    /// speculative). Searches every currently-cached layout's own
    /// `.xlf` file (already on disk, no network access needed) for a
    /// `<media id="{widget_id}">` element and returns
    /// `(layoutid, regionid, mediaid, updated)` if found. `updated` is
    /// set to 0 (unknown, since RequiredFiles never gave us a real
    /// value for this resource) -- harmless, it's only ever compared
    /// against a *future* RequiredFiles value to decide whether a
    /// routine (non-targeted) re-download is needed, which doesn't
    /// apply on this always-unconditional targeted-refresh path.
    fn find_widget_layout_region(&self, widget_id: i64) -> Option<(i64, i64, i64, i64)> {
        for (fname, res) in &self.content {
            let Resource::Layout(info) = res else { continue };
            // Deliberately `continue` (not `?`/early-return) on any
            // per-layout failure here -- one unreadable/unparseable
            // cached XLF must not stop the search across the *other*
            // layouts we do have cached.
            let Ok(file) = fs::File::open(self.dir.join(fname)) else { continue };
            let Ok(tree) = Element::from_reader(file) else { continue };
            // Regions and drawers both have their own `id` and their own
            // `<media>` children with the exact same structure -- a
            // widget could in principle live in either.
            for region in tree.find_all("region").chain(tree.find_all("drawer")) {
                let Some(region_id) = region.get_attr("id").and_then(|s| s.parse::<i64>().ok())
                    else { continue };
                for media in region.find_all("media") {
                    if media.get_attr("id").and_then(|s| s.parse::<i64>().ok())
                        == Some(widget_id) {
                        return Some((info.id, region_id, widget_id, 0));
                    }
                }
            }
        }
        None
    }

    fn save(&self) -> Result<()> {
        let fp = fs::File::create(self.dir.join("content.json")).context("writing cache content")?;
        serde_json::to_writer_pretty(fp, &self.content).context("serializing cache content")?;
        Ok(())
    }

    pub fn purge_some(&mut self, list: &[String]) -> Result<()> {
        let mut changed = false;
        for name in list {
            // Always attempt fs::remove_file regardless of whether
            // self.content tracks this key (a naming mismatch shouldn't
            // block deletion), and independently per-file (one failure
            // no longer aborts the whole batch via `?`).
            match fs::remove_file(self.dir.join(name)) {
                Ok(()) => changed = true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Already gone (or never downloaded) -- the common,
                    // unremarkable case, not worth a log line.
                }
                Err(e) => log::warn!("could not purge {name}: {e:#}"),
            }
            if self.content.remove(name).is_some() {
                changed = true;
            }
        }
        if changed {
            self.save()?;
        }
        Ok(())
    }

    pub fn purge(&mut self) -> Result<()> {
        log::info!("purging cache completely");
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
            }
        }
        self.content.clear();
        self.save()?;
        // Re-install bundled assets after purge
        self.install_pdfjs()?;
        Ok(())
    }
}


pub struct HashingWriter<W> {
    writer: W,
    hasher: Md5,
}

impl<W> HashingWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { writer, hasher: md5::Md5::new() }
    }

    pub fn hash(self) -> Vec<u8> {
        self.hasher.finalize().as_slice().to_vec()
    }
}

impl<W> Write for HashingWriter<W> where W: Write {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = self.writer.write(buf)?;
        self.hasher.update(&buf[..len]);
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Bundled pdf.js library (Mozilla, Apache 2.0 license).
const PDFJS_LIB: &[u8] = include_bytes!("../assets/pdfjs/pdf.min.mjs");
const PDFJS_WORKER: &[u8] = include_bytes!("../assets/pdfjs/pdf.worker.min.mjs");

#[cfg(test)]
mod purge_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn make_cache() -> (Cache, std::path::PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("arexibo_purge_test_{}_{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cms = CmsSettings {
            address: "https://example.com".into(),
            key: "k".into(),
            display_id: "d".into(),
            display_name: None,
            proxy: None,
        };
        let cache = Cache::new(&cms, dir.clone(), false, true).unwrap();
        (cache, dir)
    }

    #[test]
    fn purges_file_even_if_not_tracked_in_content() {
        // BUG fix regression test: a real file on disk that ISN'T (for
        // whatever reason) present in `self.content` must still get
        // deleted when the CMS says to purge it -- the old code silently
        // skipped this case entirely.
        let (mut cache, dir) = make_cache();
        fs::write(dir.join("orphan.jpg"), b"fake image data").unwrap();
        assert!(dir.join("orphan.jpg").is_file());
        assert!(!cache.content.contains_key("orphan.jpg")); // deliberately untracked

        cache.purge_some(&["orphan.jpg".to_string()]).unwrap();
        assert!(!dir.join("orphan.jpg").is_file(), "untracked file should still be purged");
    }

    #[test]
    fn one_failed_purge_does_not_abort_the_rest_of_the_batch() {
        // BUG fix regression test: previously, using `?` inside the loop
        // meant a single failure (here: a name that doesn't exist on
        // disk at all, a very ordinary real-world case -- e.g. already
        // removed by an earlier purge) would abort the whole batch,
        // silently skipping every subsequent file.
        let (mut cache, dir) = make_cache();
        fs::write(dir.join("real1.jpg"), b"data").unwrap();
        fs::write(dir.join("real2.jpg"), b"data").unwrap();
        // "missing.jpg" doesn't exist at all -- must not block real1/real2
        let result = cache.purge_some(&[
            "missing.jpg".to_string(),
            "real1.jpg".to_string(),
            "real2.jpg".to_string(),
        ]);
        assert!(result.is_ok(), "purge_some should not error out on a missing file");
        assert!(!dir.join("real1.jpg").is_file(), "real1.jpg should still be purged");
        assert!(!dir.join("real2.jpg").is_file(), "real2.jpg should still be purged");
    }

    #[test]
    fn purging_missing_file_is_not_an_error() {
        let (mut cache, _dir) = make_cache();
        let result = cache.purge_some(&["never-existed.jpg".to_string()]);
        assert!(result.is_ok());
    }
}

#[cfg(test)]
mod dataset_fallback_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn make_cache_with_layout(xlf_content: &str, layout_id: LayoutId) -> Cache {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("arexibo_dataset_fallback_test_{}_{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cms = CmsSettings {
            address: "https://example.com".into(), key: "k".into(),
            display_id: "d".into(), display_name: None, proxy: None,
        };
        let mut cache = Cache::new(&cms, dir.clone(), false, true).unwrap();
        let fname = format!("{layout_id}.xlf");
        fs::write(dir.join(&fname), xlf_content).unwrap();
        cache.content.insert(fname, Resource::Layout(Arc::new(LayoutInfo {
            id: layout_id, md5: vec![], size: (1080, 1920), code: None,
            enable_stat: true, translated_version: TRANSLATOR_VERSION,
        })));
        cache
    }

    #[test]
    fn finds_widget_missing_from_required_files_via_cached_xlf() {
        // Regression test for a real report: a DataSet View widget's own
        // resource never appeared in the CMS's RequiredFiles response at
        // all (confirmed via a real required.xml), even though
        // neighboring resources in the same region did. The (layoutid,
        // regionid) must still be derivable from the layout's own
        // already-cached XLF.
        let xlf = r#"<layout width="1080" height="1920">
            <region id="3630"><media id="3261" type="datasetview"/></region>
            <region id="3631"><media id="3262" type="datasetview"/><media id="3263" type="text"/></region>
        </layout>"#;
        let cache = make_cache_with_layout(xlf, 749);
        let found = cache.find_widget_layout_region(3262);
        assert_eq!(found, Some((749, 3631, 3262, 0)));
    }

    #[test]
    fn finds_widget_inside_a_drawer_too() {
        let xlf = r#"<layout width="1080" height="1920">
            <region id="1"><media id="100" type="text"/></region>
            <drawer id="99"><media id="3047" type="shellcommand"/></drawer>
        </layout>"#;
        let cache = make_cache_with_layout(xlf, 555);
        let found = cache.find_widget_layout_region(3047);
        assert_eq!(found, Some((555, 99, 3047, 0)));
    }

    #[test]
    fn returns_none_when_widget_truly_not_found_anywhere() {
        let xlf = r#"<layout width="1080" height="1920">
            <region id="1"><media id="100" type="text"/></region>
        </layout>"#;
        let cache = make_cache_with_layout(xlf, 749);
        assert_eq!(cache.find_widget_layout_region(999999), None);
    }

    #[test]
    fn one_unparseable_cached_layout_does_not_block_finding_it_in_another() {
        // Regression test for the same class of bug just fixed in
        // purge_some: a `?`/early-return on the first layout's own parse
        // failure must not prevent searching the *other* cached layouts.
        let mut cache = make_cache_with_layout(
            r#"<layout width="1080" height="1920">
                <region id="1"><media id="100" type="text"/></region>
            </layout>"#,
            555,
        );
        // Register a SECOND layout whose actual file is missing/corrupt
        // on disk (never written), which must not block the search.
        cache.content.insert("777.xlf".to_string(), Resource::Layout(Arc::new(LayoutInfo {
            id: 777, md5: vec![], size: (1080, 1920), code: None,
            enable_stat: true, translated_version: TRANSLATOR_VERSION,
        })));
        assert_eq!(cache.find_widget_layout_region(100), Some((555, 1, 100, 0)));
    }
}

#[cfg(test)]
mod nested_widget_fallback_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn make_cache_with_resource(html_content: &str, resource_id: i64) -> Cache {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("arexibo_nested_widget_test_{}_{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cms = CmsSettings {
            address: "https://example.com".into(), key: "k".into(),
            display_id: "d".into(), display_name: None, proxy: None,
        };
        let mut cache = Cache::new(&cms, dir.clone(), false, true).unwrap();
        let fname = format!("{resource_id}.html");
        fs::write(dir.join(&fname), html_content).unwrap();
        cache.content.insert(fname, Resource::Resource(Arc::new(ResourceInfo {
            id: resource_id, layoutid: 749, regionid: 3630, mediaid: resource_id,
            updated: 12345, duration: Some(10.0), numitems: Some(1),
        })));
        cache
    }

    #[test]
    fn finds_widget_nested_inside_another_resources_combined_html() {
        // Regression test for a real report: widget 3262 lives *inside*
        // resource 3261's own combined "Elements" HTML (its own
        // `widgetData`/`elements` JSON arrays reference both widget ids
        // together), it is not its own separate resource at all.
        let html = r#"<html><script>
            widgetData.push({"widgetId":3261,"templateId":null});
            widgetData.push({"widgetId":3262,"templateId":"elements"});
            elements.push([{"elements":[],"widgetId":3262}]);
        </script></html>"#;
        let cache = make_cache_with_resource(html, 3261);
        let found = cache.find_nested_widget_resource(3262);
        assert_eq!(found, Some((3261, 749, 3630, 3261, 12345)));
    }

    #[test]
    fn returns_none_when_widget_id_truly_not_nested_anywhere() {
        let html = r#"<html><script>widgetData.push({"widgetId":3261});</script></html>"#;
        let cache = make_cache_with_resource(html, 3261);
        assert_eq!(cache.find_nested_widget_resource(9999), None);
    }

    #[test]
    fn does_not_false_positive_on_similar_field_names() {
        // "mediaId" and "widgetId" must not be confused with each other
        // even though they share letters -- exact string match on the
        // full distinctive marker only.
        let html = r#"<html><script>
            elements.push([{"mediaId":"3262","widgetId":3261}]);
        </script></html>"#;
        let cache = make_cache_with_resource(html, 3261);
        assert_eq!(cache.find_nested_widget_resource(3262), None);
        assert_eq!(cache.find_nested_widget_resource(3261), Some((3261, 749, 3630, 3261, 12345)));
    }
}
