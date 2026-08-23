// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Handling resources such as media and layout files.

use std::collections::HashMap;
use std::{fs, io, io::Write, path::PathBuf, sync::Arc};
use std::time::{Duration, Instant};
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
    /// A player dependency (font, player bundle JS/CSS for Elements-
    /// based widgets, etc.) -- a distinct variant, not reusing `File`,
    /// because its id is a string (e.g. a font/bundle name), unlike
    /// every other file type's integer id, and the CMS provides no
    /// md5 to verify it against (see required_files's own parsing).
    Dependency {
        id: String,
        file_type: String,
        size: u64,
        http: bool,
        path: String,
        name: String,
    },
}

impl ReqFile {
    pub fn description(&self) -> String {
        match self {
            ReqFile::File { typ, name, .. } => format!("{typ} {name}"),
            ReqFile::Resource { mediaid, .. } => format!("resource {mediaid}"),
            ReqFile::Dependency { file_type, name, .. } => format!("dependency ({file_type}) {name}"),
        }
    }

    pub fn inventory(&self) -> (&'static str, i64) {
        match self {
            ReqFile::File { id, typ, .. } => (typ, *id),
            ReqFile::Resource { id, .. } => ("resource", *id),
            // Dependencies have no integer id (see the variant's own
            // doc comment) and aren't part of MediaInventory reporting
            // in the reference client either -- 0 is a harmless
            // placeholder, never actually looked up by this id.
            ReqFile::Dependency { .. } => ("dependency", 0),
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

/// A cached player dependency (see ReqFile::Dependency's own doc
/// comment for why this needs its own type -- a string id, and no
/// md5 given by the CMS to verify against).
#[derive(Debug, Serialize, Deserialize)]
pub struct DependencyInfo {
    pub id: String,
    pub file_type: String,
    pub size: u64,
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
    Dependency(Arc<DependencyInfo>),
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
    /// v7 GetData polling state, one entry per discovered data widget
    /// needing GetData (see parse_data_widgets/DataWidgetInfo). Not
    /// persisted across restarts -- a fresh collection re-discovers
    /// the same widgets from re-downloaded resources anyway, so
    /// serializing this isn't worth the complexity. Cleaned up by
    /// `purge`/`purge_some` alongside their containing resource, so
    /// this never accumulates entries for widgets no longer scheduled.
    data_widgets: HashMap<i64, DataWidgetState>,
}

/// Polling state for one data widget (see `data_widgets`'s own doc
/// comment). `resource_id` is the containing resource's own numeric
/// id (matching its `"{id}.html"` filename) -- lets `purge_some`
/// remove this entry when that specific resource is purged, without
/// needing to re-scan any HTML.
struct DataWidgetState {
    resource_id: i64,
    update_interval: Duration,
    /// None means never refreshed yet -- due immediately.
    last_refreshed: Option<Instant>,
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
                           adspace_enabled: false, adspace_partner: None, html_port: 0,
                           data_widgets: HashMap::new() };
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
            // The CMS gives no md5 to detect a changed dependency (see
            // ReqFile::Dependency's own doc comment) -- once cached
            // under this name, considered up to date until the CMS's
            // own purge mechanism removes it from RequiredFiles/the
            // purge list entirely.
            ReqFile::Dependency { ref name, .. } => self.get_dependency(name).is_some(),
        }
    }

    pub fn download(&mut self, res: ReqFile, cms: &mut xmds::Cms) -> Result<()> {
        match res {
            ReqFile::Resource { id, layoutid, regionid, mediaid, updated } => {
                let data = cms.get_resource(layoutid, &regionid.to_string(),
                                            &mediaid.to_string())?;
                let fname = format!("{id}.html");

                // v7 GetData polling groundwork -- explicitly gated
                // even though the trigger (needs_get_data) never fires
                // against a real v5 CMS anyway (confirmed via real
                // comparison): v5's own generated HTML always has data
                // embedded directly. This gate is defense in depth,
                // not the only thing keeping this inert on v5.
                if xmds::xmds_supports_v6_v7_methods() {
                    self.discover_data_widgets(&data, id);
                }

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
                    match self.download_http(&path, &filename, Some(&md5)) {
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
            ReqFile::Dependency { id, file_type, size, http, path, name } => {
                let filename = self.dir.join(&name);
                if http {
                    match self.download_http(&path, &filename, None) {
                        Ok(()) => {},
                        Err(e) => {
                            log::warn!("failing download of dependency {name} over http, \
                                        retrying xmds: {e:#}");
                            Self::download_dependency_xmds(&id, &file_type, size, cms, &filename)?;
                        }
                    }
                } else {
                    Self::download_dependency_xmds(&id, &file_type, size, cms, &filename)?;
                }
                self.content.insert(name, Resource::Dependency(Arc::new(
                    DependencyInfo { id, file_type, size }
                )));
                self.save()?;
            }
        }
        Ok(())
    }

    fn download_http(&mut self, path: &str, filename: &PathBuf,
                     md5: Option<&[u8]>) -> Result<()> {
        let body = self.agent.get(path).call()?.into_body();
        let file = io::BufWriter::new(fs::File::create(filename)?);
        let mut wrapper = HashingWriter::new(file);
        io::copy(&mut body.into_reader(), &mut wrapper)?;
        if let Some(md5) = md5 {
            ensure!(wrapper.hash() == md5, "md5 mismatch");
        }
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

    /// Same chunked-download loop as `download_xmds`, but for a
    /// dependency's own string id (see ReqFile::Dependency's own doc
    /// comment) via GetDependency instead of GetFile -- and no md5 to
    /// verify against, since the CMS doesn't provide one for this file
    /// type (confirmed via the real reference client source).
    fn download_dependency_xmds(id: &str, file_type: &str, size: u64, cms: &mut xmds::Cms,
                                filename: &PathBuf) -> Result<()> {
        const CHUNK_SIZE: u64 = 1024 * 1024;
        let mut got_size = 0;
        let file = io::BufWriter::new(fs::File::create(filename)?);
        let mut writer = file;
        while got_size < size {
            let next_size = (size - got_size).min(CHUNK_SIZE);
            let chunk = cms.get_dependency_data(id, file_type, got_size, next_size)?;
            got_size += chunk.len() as u64;
            writer.write_all(&chunk)?;
        }
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

    fn get_dependency(&self, name: &str) -> Option<Arc<DependencyInfo>> {
        self.content.get(name).and_then(|entry| match entry {
            Resource::Dependency(dep) => Some(dep.clone()),
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

    /// Discovers data widgets (v7 GetData polling) inside a resource's
    /// own rendered HTML and starts/updates tracking for each one that
    /// needs GetData. Separated from its own call site in `download`
    /// (which gates this behind `xmds::xmds_supports_v6_v7_methods()`)
    /// specifically so this logic can be unit-tested directly,
    /// independently of the hardcoded endpoint version -- see this
    /// module's own tests for both halves: this method's own logic,
    /// and `download`'s own confirmed-closed gate today.
    pub(crate) fn discover_data_widgets(&mut self, html: &str, resource_id: i64) {
        for w in parse_data_widgets(html) {
            if !w.needs_get_data { continue; }
            let interval = Duration::from_secs(
                w.update_interval_minutes.filter(|m| *m > 0).unwrap_or(1) as u64 * 60);
            // Update resource_id/interval but preserve last_refreshed
            // if already tracked -- a widget rediscovered because its
            // containing resource was re-downloaded shouldn't have its
            // own refresh timer reset.
            self.data_widgets.entry(w.widget_id)
                .and_modify(|s| { s.resource_id = resource_id; s.update_interval = interval; })
                .or_insert(DataWidgetState { resource_id, update_interval: interval,
                                              last_refreshed: None });
        }
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
            if let Some(removed) = self.content.remove(name) {
                changed = true;
                // A purged resource might have been the container for
                // tracked data widgets (v7 GetData polling, see
                // data_widgets's own doc comment) -- without this,
                // we'd keep polling GetData for a widget belonging to
                // a layout/region that's no longer scheduled at all.
                if let Resource::Resource(info) = &removed {
                    self.data_widgets.retain(|_, s| s.resource_id != info.id);
                }
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
        self.data_widgets.clear();
        self.save()?;
        // Re-install bundled assets after purge
        self.install_pdfjs()?;
        Ok(())
    }

    /// Which tracked data widgets (v7 GetData polling) are due for a
    /// refresh right now -- never refreshed yet, or their own
    /// update_interval has elapsed since the last one.
    pub fn data_widgets_due(&self, now: Instant) -> Vec<i64> {
        self.data_widgets.iter()
            .filter(|(_, s)| s.last_refreshed
                    .map_or(true, |t| now.duration_since(t) >= s.update_interval))
            .map(|(id, _)| *id)
            .collect()
    }

    /// How long until the *soonest* tracked data widget is due -- for
    /// arming a re-check timer. None if no data widgets are currently
    /// tracked at all (timer should stay disarmed).
    pub fn next_data_widget_due_in(&self, now: Instant) -> Option<Duration> {
        self.data_widgets.values().map(|s| match s.last_refreshed {
            None => Duration::ZERO, // due immediately
            Some(t) => s.update_interval.saturating_sub(now.duration_since(t)),
        }).min()
    }

    /// Fetches fresh data for one tracked widget via GetData and writes
    /// it as `<widget_id>.json` in the cache directory -- ready to be
    /// served by the local webserver at the relative `url` the
    /// widget's own rendered HTML references. Updates the widget's own
    /// last-refreshed time on success.
    /// Returns the containing resource's own id on success (matching
    /// `refresh_resource`'s own pattern) -- the caller needs this to
    /// tell the GUI *which* iframe/webview to reload, since that's
    /// identified by resource id, not the raw widget id.
    pub fn refresh_data_widget(&mut self, widget_id: i64, cms: &mut xmds::Cms,
                                now: Instant) -> Result<i64> {
        let json = cms.get_data(widget_id)?;
        // Write to a sibling .tmp file, then rename atomically -- same
        // convention as adspace.rs's own download_creative. The local
        // webserver could be serving this exact file to the widget's
        // own fetch concurrently with this refresh; a direct fs::write
        // on the final path risks a reader observing a truncated/
        // partial file mid-write. rename() on POSIX filesystems
        // guarantees a reader always sees either the complete old file
        // or the complete new one, never a mix.
        let path = self.dir.join(format!("{widget_id}.json"));
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, json)?;
        fs::rename(&tmp_path, &path)?;
        let state = self.data_widgets.get_mut(&widget_id)
            .ok_or_else(|| anyhow::anyhow!("widget {widget_id} is no longer tracked"))?;
        state.last_refreshed = Some(now);
        Ok(state.resource_id)
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

// --- Data Widget recognition (v7 GetData groundwork) ----------------
//
// Not yet wired into the real collection cycle -- collect_once() never
// calls parse_data_widgets/needs_get_data today. Stays inert on v5:
// GetResource's own generated HTML always has data already embedded
// there (confirmed via real CMS comparison), so needs_get_data()
// never returns true against a real v5 CMS. Written now so a future
// v7 branch has this ready rather than starting from scratch.

/// One `widgetData.push({...})` entry found in a widget's own rendered
/// HTML (see xmds::Cms::get_resource) -- only the fields relevant to
/// deciding whether GetData is needed, not the full object (which also
/// carries template/style content irrelevant here).
#[allow(dead_code)] // groundwork for v7, see module-level comment above
#[derive(Debug, Clone, PartialEq)]
struct DataWidgetInfo {
    widget_id: i64,
    /// True when `data` is JSON null *and* `url` is a real path (not
    /// the literal string "null") -- confirmed real v7 shape:
    /// `"url":"4543.json","data":null`. On v5, `data` is the actual
    /// embedded payload and `url` is the literal string "null", so
    /// this is false.
    needs_get_data: bool,
    /// From `properties.updateInterval` (minutes) -- confirmed real
    /// field, e.g. `5` in an actual v7 widget. None if missing/not a
    /// number.
    update_interval_minutes: Option<i64>,
}

/// Finds every `widgetData.push({...})` call in `html` and parses each
/// as JSON, returning the ones that look like real data widgets (have
/// a `widgetId`). Malformed/unparseable entries are skipped rather
/// than failing the whole scan -- one broken widget's markup
/// shouldn't hide the others in the same resource (Elements designer
/// can combine several widgets into one resource file).
#[allow(dead_code)] // groundwork for v7, see module-level comment above
fn parse_data_widgets(html: &str) -> Vec<DataWidgetInfo> {
    const MARKER: &str = "widgetData.push(";
    let mut found = Vec::new();
    let mut pos = 0;
    while let Some(rel) = html[pos..].find(MARKER) {
        let obj_start = pos + rel + MARKER.len();
        let Some(json_str) = extract_balanced_json_object(&html[obj_start..]) else {
            break; // unbalanced/truncated -- nothing more to find reliably
        };
        pos = obj_start + json_str.len();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) else { continue };
        let Some(widget_id) = value.get("widgetId").and_then(|v| v.as_i64()) else { continue };
        let url_is_real_path = matches!(value.get("url"), Some(serde_json::Value::String(s)) if s != "null");
        let data_is_null = matches!(value.get("data"), Some(serde_json::Value::Null) | None);
        let update_interval_minutes = value.get("properties")
            .and_then(|p| p.get("updateInterval")).and_then(|v| v.as_i64());
        found.push(DataWidgetInfo { widget_id, needs_get_data: url_is_real_path && data_is_null,
                                     update_interval_minutes });
    }
    found
}

/// Given `s` starting (after optional leading whitespace) with a JSON
/// object literal `{`, returns the exact matching substring for that
/// object -- correctly tracking string literals (respecting `\"`
/// escapes) so braces *inside* a string value (e.g. a CSS `styleSheet`
/// property containing literal `{`/`}` characters, confirmed present
/// in real widget data) don't get miscounted as structural nesting.
/// A naive "count all braces" scan, or any regex-based approach, would
/// break exactly on this real, observed shape.
#[allow(dead_code)] // groundwork for v7, see module-level comment above
fn extract_balanced_json_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
    if bytes.get(i) != Some(&b'{') { return None; }
    let start = i;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            if escaped { escaped = false; }
            else if c == b'\\' { escaped = true; }
            else if c == b'"' { in_string = false; }
        } else {
            match c {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 { return Some(&s[start..=i]); }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod extract_balanced_json_object_tests {
    use super::*;

    #[test]
    fn extracts_a_simple_object() {
        assert_eq!(extract_balanced_json_object(r#"{"a":1});"#), Some(r#"{"a":1}"#));
    }

    #[test]
    fn handles_braces_inside_a_string_value() {
        // Adversarial case: an UNMATCHED closing brace inside a string
        // value, followed by real content that must still be included.
        // A naive brace-count (or any regex) would hit depth 0 at that
        // stray `}` and truncate the object early, right in the middle
        // of the styleSheet string -- exactly the kind of real,
        // observed shape (CSS containing braces) this guards against.
        let input = r#"{"a":1,"styleSheet":"width: 1080px; }","b":2});"#;
        assert_eq!(extract_balanced_json_object(input),
                   Some(r#"{"a":1,"styleSheet":"width: 1080px; }","b":2}"#));
    }

    #[test]
    fn handles_escaped_quotes_inside_a_string_value() {
        let input = r#"{"template":"<td class=\"cella\">[NOME]</td>"});"#;
        assert_eq!(extract_balanced_json_object(input),
                   Some(r#"{"template":"<td class=\"cella\">[NOME]</td>"}"#));
    }

    #[test]
    fn handles_nested_objects() {
        let input = r#"{"a":1,"properties":{"b":2,"c":{"d":3}}});"#;
        assert_eq!(extract_balanced_json_object(input),
                   Some(r#"{"a":1,"properties":{"b":2,"c":{"d":3}}}"#));
    }

    #[test]
    fn returns_none_for_unbalanced_input() {
        assert_eq!(extract_balanced_json_object(r#"{"a":1"#), None);
    }

    #[test]
    fn returns_none_when_not_starting_with_a_brace() {
        assert_eq!(extract_balanced_json_object("not json"), None);
    }
}

#[cfg(test)]
mod parse_data_widgets_tests {
    use super::*;

    #[test]
    fn a_real_v5_resource_does_not_need_get_data() {
        // Trimmed from a real GetResource response (a real CMS,
        // v5, widget 4543, dataset "numeriutili") -- data is embedded
        // directly, url is the literal string "null".
        let html = r#"<script>
        var widgetData = [];
        widgetData.push({"widgetId":4543,"templateId":"custom_numeriutili","url":"null","data":{"data":[{"NOME":"Mario Rossi"}],"meta":{}}});
        var elements = [];
        </script>"#;
        let widgets = parse_data_widgets(html);
        assert_eq!(widgets, vec![DataWidgetInfo { widget_id: 4543, needs_get_data: false,
                                                    update_interval_minutes: None }]);
    }

    #[test]
    fn a_real_v7_resource_needs_get_data() {
        // Same real widget, same CMS, only the endpoint version
        // differs -- confirmed real v7 shape: url is a real path,
        // data is JSON null, properties.updateInterval is 5 (minutes).
        let html = r#"<script>
        var widgetData = [];
        widgetData.push({"widgetId":4543,"templateId":"custom_numeriutili","properties":{"updateInterval":5},"url":"4543.json","data":null});
        var elements = [];
        </script>"#;
        let widgets = parse_data_widgets(html);
        assert_eq!(widgets, vec![DataWidgetInfo { widget_id: 4543, needs_get_data: true,
                                                    update_interval_minutes: Some(5) }]);
    }

    #[test]
    fn finds_multiple_pushes_in_one_elements_designer_resource() {
        // Elements designer can combine several widgets into one
        // resource file (see find_nested_widget_resource's own doc
        // comment) -- must not stop after the first push().
        let html = r#"<script>
        widgetData.push({"widgetId":100,"url":"null","data":{"x":1}});
        widgetData.push({"widgetId":200,"properties":{"updateInterval":10},"url":"200.json","data":null});
        </script>"#;
        let widgets = parse_data_widgets(html);
        assert_eq!(widgets, vec![
            DataWidgetInfo { widget_id: 100, needs_get_data: false, update_interval_minutes: None },
            DataWidgetInfo { widget_id: 200, needs_get_data: true, update_interval_minutes: Some(10) },
        ]);
    }

    #[test]
    fn ignores_content_with_no_widget_data_at_all() {
        let html = r#"<html><body>plain layout, no data widgets here</body></html>"#;
        assert_eq!(parse_data_widgets(html), vec![]);
    }
}

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

#[cfg(test)]
mod dependency_download_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn make_cache() -> (Cache, std::path::PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("arexibo_dependency_test_{}_{n}", std::process::id()));
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

    /// Real mock XMDS server for the GetDependency chunked-download
    /// path -- same real-response-format approach as xmds.rs's own
    /// tests, not a shortcut that bypasses the real SOAP parsing.
    fn make_cms_with_mock_getdependency(response_bytes: &'static [u8]) -> xmds::Cms {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                use base64::Engine as _;
                let encoded = base64::engine::general_purpose::STANDARD.encode(response_bytes);
                let body = format!(
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><GetDependencyResponse><file>{encoded}</file></GetDependencyResponse></soap:Body>
</soap:Envelope>"#);
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        let cms_settings = CmsSettings {
            address: format!("http://127.0.0.1:{port}"),
            key: "k".into(), display_id: "d".into(), display_name: None, proxy: None,
        };
        let xml_dir = std::env::temp_dir().join(format!(
            "arexibo_dependency_xmds_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&xml_dir).unwrap();
        xmds::Cms::new(&cms_settings, "dummy-pub-key".into(), true, xml_dir).unwrap()
    }

    #[test]
    fn has_is_false_before_download_and_true_after() {
        // The CMS gives no md5 for a dependency (see ReqFile::
        // Dependency's own doc comment) -- once cached under its name,
        // it must be considered up to date, not re-downloaded every
        // cycle.
        let (mut cache, _dir) = make_cache();
        let req = ReqFile::Dependency {
            id: "roboto-regular.ttf".to_string(), file_type: "font".to_string(),
            size: 12, http: false, path: String::new(), name: "roboto-regular.ttf".to_string(),
        };
        assert!(!cache.has(&req), "must not be considered cached before any download");
        let mut cms = make_cms_with_mock_getdependency(b"font bytes!!");
        cache.download(req.clone(), &mut cms).unwrap();
        assert!(cache.has(&req), "must be considered cached after a successful download");
    }

    #[test]
    fn downloads_via_xmds_getdependency_and_writes_the_correct_file() {
        let (mut cache, dir) = make_cache();
        let mut cms = make_cms_with_mock_getdependency(b"actual font file bytes");
        let req = ReqFile::Dependency {
            id: "roboto-regular.ttf".to_string(), file_type: "font".to_string(),
            size: 22, http: false, path: String::new(), name: "roboto-regular.ttf".to_string(),
        };
        cache.download(req, &mut cms).unwrap();
        let saved = fs::read(dir.join("roboto-regular.ttf")).unwrap();
        assert_eq!(saved, b"actual font file bytes");
    }
}

#[cfg(test)]
mod data_widget_polling_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn make_cache() -> (Cache, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("arexibo_data_widget_test_{}_{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cms = CmsSettings {
            address: "https://example.com".into(), key: "k".into(),
            display_id: "d".into(), display_name: None, proxy: None,
        };
        let cache = Cache::new(&cms, dir.clone(), false, true).unwrap();
        (cache, dir)
    }

    fn make_cms_with_mock_getresource(html: &'static str) -> xmds::Cms {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let escaped = html.replace('&', "&amp;").replace('<', "&lt;")
                                   .replace('>', "&gt;").replace('"', "&quot;");
                let body = format!(
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><GetResourceResponse><resource>{escaped}</resource></GetResourceResponse></soap:Body>
</soap:Envelope>"#);
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        test_cms_at(port)
    }

    fn make_cms_with_mock_getdata(json: &'static str) -> xmds::Cms {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let escaped = json.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
                let body = format!(
                    r#"<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
<soap:Body><GetDataResponse><data>{escaped}</data></GetDataResponse></soap:Body>
</soap:Envelope>"#);
                let _ = request.respond(tiny_http::Response::from_string(body));
            }
        });
        test_cms_at(port)
    }

    fn test_cms_at(port: u16) -> xmds::Cms {
        let cms_settings = CmsSettings {
            address: format!("http://127.0.0.1:{port}"),
            key: "k".into(), display_id: "d".into(), display_name: None, proxy: None,
        };
        let xml_dir = std::env::temp_dir().join(format!(
            "arexibo_data_widget_xmds_test_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&xml_dir).unwrap();
        xmds::Cms::new(&cms_settings, "dummy-pub-key".into(), true, xml_dir).unwrap()
    }

    const V7_WIDGET_HTML: &str = r#"<script>
    widgetData.push({"widgetId":4543,"properties":{"updateInterval":5},"url":"4543.json","data":null});
    </script>"#;

    // The following tests exercise discover_data_widgets/purge*
    // directly, independently of the endpoint-version gate in
    // `download` -- that gate is confirmed separately, at the very
    // end of this module, to actually be closed today (hardcoded v5).

    #[test]
    fn discover_starts_tracking_a_widget_that_needs_get_data() {
        let (mut cache, _dir) = make_cache();
        cache.discover_data_widgets(V7_WIDGET_HTML, 1);
        let due = cache.data_widgets_due(Instant::now());
        assert_eq!(due, vec![4543], "a never-refreshed widget must be due immediately");
    }

    #[test]
    fn discover_does_not_track_a_v5_shaped_widget() {
        // Real v5 shape: data already embedded, url is the literal
        // string "null" -- needs_get_data is false.
        let (mut cache, _dir) = make_cache();
        let v5_html = r#"<script>
        widgetData.push({"widgetId":4543,"url":"null","data":{"data":[{"NOME":"Mario Rossi"}]}});
        </script>"#;
        cache.discover_data_widgets(v5_html, 1);
        assert!(cache.data_widgets_due(Instant::now()).is_empty());
        assert_eq!(cache.next_data_widget_due_in(Instant::now()), None);
    }

    #[test]
    fn refresh_writes_the_json_file_and_clears_due_status() {
        let (mut cache, dir) = make_cache();
        cache.discover_data_widgets(V7_WIDGET_HTML, 1); // resource_id 1, widget_id 4543

        let mut data_cms = make_cms_with_mock_getdata(r#"{"data":[{"NOME":"Mario Rossi"}]}"#);
        let now = Instant::now();
        let resource_id = cache.refresh_data_widget(4543, &mut data_cms, now).unwrap();

        assert_eq!(resource_id, 1,
                   "must return the containing resource's own id, not the widget id -- \
                    the GUI needs the resource id to know which iframe to reload");
        let saved = fs::read_to_string(dir.join("4543.json")).unwrap();
        assert_eq!(saved, r#"{"data":[{"NOME":"Mario Rossi"}]}"#);
        assert!(cache.data_widgets_due(now).is_empty(),
                "must not be due again immediately after a successful refresh");
        assert!(!dir.join("4543.tmp").exists(),
                "the temp file used for the atomic rename must not linger afterward");
    }

    #[test]
    fn becomes_due_again_after_its_own_update_interval_elapses() {
        let (mut cache, _dir) = make_cache();
        cache.discover_data_widgets(V7_WIDGET_HTML, 1); // updateInterval: 5 (minutes)

        let mut data_cms = make_cms_with_mock_getdata(r#"{"data":[]}"#);
        let refreshed_at = Instant::now();
        cache.refresh_data_widget(4543, &mut data_cms, refreshed_at).unwrap();

        let just_under_5_min = refreshed_at + Duration::from_secs(5 * 60 - 1);
        assert!(cache.data_widgets_due(just_under_5_min).is_empty());

        let just_over_5_min = refreshed_at + Duration::from_secs(5 * 60 + 1);
        assert_eq!(cache.data_widgets_due(just_over_5_min), vec![4543]);
    }

    #[test]
    fn next_due_in_is_none_when_nothing_is_tracked() {
        let (cache, _dir) = make_cache();
        assert_eq!(cache.next_data_widget_due_in(Instant::now()), None);
    }

    #[test]
    fn next_due_in_reflects_the_remaining_time() {
        let (mut cache, _dir) = make_cache();
        cache.discover_data_widgets(V7_WIDGET_HTML, 1);

        let mut data_cms = make_cms_with_mock_getdata(r#"{"data":[]}"#);
        let refreshed_at = Instant::now();
        cache.refresh_data_widget(4543, &mut data_cms, refreshed_at).unwrap();

        let one_minute_later = refreshed_at + Duration::from_secs(60);
        // 5 minute interval, 1 minute elapsed -- ~4 minutes remaining.
        let remaining = cache.next_data_widget_due_in(one_minute_later).unwrap();
        assert!(remaining <= Duration::from_secs(4 * 60) &&
                remaining > Duration::from_secs(4 * 60 - 5),
                "expected ~4 minutes remaining, got {remaining:?}");
    }

    #[test]
    fn purge_some_stops_tracking_a_widget_whose_resource_was_purged() {
        let (mut cache, dir) = make_cache();
        // purge_some looks up the resource's own id via self.content,
        // so a real cached ResourceInfo entry (not just the tracked
        // data widget) is needed for this specific test.
        fs::write(dir.join("1.html"), V7_WIDGET_HTML).unwrap();
        cache.content.insert("1.html".to_string(), Resource::Resource(Arc::new(
            ResourceInfo { id: 1, layoutid: 940, regionid: 4542, mediaid: 4543,
                           updated: 0, duration: None, numitems: None })));
        cache.discover_data_widgets(V7_WIDGET_HTML, 1);
        assert_eq!(cache.data_widgets_due(Instant::now()), vec![4543]);

        cache.purge_some(&["1.html".to_string()]).unwrap();

        assert!(cache.data_widgets_due(Instant::now()).is_empty(),
                "a widget whose containing resource was purged must stop being tracked");
        assert_eq!(cache.next_data_widget_due_in(Instant::now()), None);
    }

    #[test]
    fn purge_stops_tracking_every_widget() {
        let (mut cache, _dir) = make_cache();
        cache.discover_data_widgets(V7_WIDGET_HTML, 1);
        assert_eq!(cache.data_widgets_due(Instant::now()), vec![4543]);

        cache.purge().unwrap();

        assert!(cache.data_widgets_due(Instant::now()).is_empty());
    }

    // This is the test that actually matters for today's real-world
    // safety: confirms the production code path (download, not
    // discover_data_widgets directly) does NOT start tracking anything
    // right now, because XMDS_ENDPOINT_VERSION is hardcoded to 5. If
    // this test ever starts failing on its own (without the version
    // constant having been deliberately bumped), that's a sign the
    // gate in `download` got removed or broken.
    #[test]
    fn download_does_not_discover_data_widgets_while_the_version_gate_is_closed() {
        let (mut cache, _dir) = make_cache();
        let mut cms = make_cms_with_mock_getresource(V7_WIDGET_HTML);
        let req = ReqFile::Resource { id: 1, layoutid: 940, regionid: 4542, mediaid: 4543,
                                       updated: 0 };
        cache.download(req, &mut cms).unwrap();
        assert!(cache.data_widgets_due(Instant::now()).is_empty(),
                "nothing should be tracked via the real download() path while \
                 xmds_supports_v6_v7_methods() is false");
    }
}
