// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Xibo Adspace Exchange integration -- VAST (IAB standard) ad requests
//! for the `ssp` widget type and (in principle, see scope note below)
//! Adspace-driven schedule items.
//!
//! ARCHITECTURAL HONESTY NOTE: built without access to
//! exchange.xibo-adspace.com or its API docs -- nothing here has been
//! exercised against a real bid response. Grounded sources: VAST
//! itself (public IAB standard); the custom `xibo*` VAST Extension
//! names (confirmed real, from xibo-dotnetclient GitHub issue #268).
//! FLAGGED AS UNVERIFIED: the bid request endpoint/shape
//! (exchange.xibo-adspace.com/vast/device, query params) -- comes from
//! this project's own earlier porting session notes, not independently
//! verified, the single biggest risk area in this module.
//!
//! SCOPE: only widget-level activation (type="ssp" on a <media>,
//! resolved during layout.rs's translate()) is wired up end-to-end.
//! Schedule-item-level activation (a whole ScheduleItem of type
//! "Adspace Exchange" replacing the layout, like an Interrupt Layout --
//! see ScheduleItem.CreateForAdspaceExchange in the real C# client)
//! is NOT implemented -- would need synthesizing a fake "layout" that
//! is ad content, wired into navigation/cache/Proof of Play as if a
//! real cached .xlf. request_ad/creative-download are written to be
//! reusable for that follow-up regardless.

use std::{collections::HashMap, fs, path::{Path, PathBuf}};
use anyhow::{bail, Context, Result};
use elementtree::Element;
use md5::{Md5, Digest};

/// Shared configuration threaded from `resource::Cache` into
/// `layout::Translator` so `ssp` widgets can be resolved during layout
/// translation -- see this module's top-level doc comment for the
/// overall scope/caveats.
#[derive(Clone)]
pub struct AdspaceConfig {
    pub agent: ureq::Agent,
    pub cache_dir: PathBuf,
    pub partner: Option<String>,
}

/// The result of fully resolving an `ssp` widget's ad: a locally cached,
/// playable file plus enough metadata for `layout.rs::write_media` to
/// emit it exactly like a normal video/image widget.
pub struct ResolvedCreative {
    pub local_path: PathBuf,
    pub mime_type: String,
    /// Seconds, if VAST declared one -- `write_media` falls back to the
    /// XLF widget's own declared duration if this is `None`, same as any
    /// other widget type.
    pub duration: Option<f64>,
}

/// High-level entry point for a `ssp` widget: bid, follow any Wrapper
/// chain, prefetch bulk resources if the ad declares any (best-effort --
/// a prefetch failure is logged by the caller and does not block
/// showing the actual ad creative), then download and cache the chosen
/// MediaFile. `target_width` is the widget's own pixel width (from the
/// XLF region), used only to pick the best-fitting MediaFile when
/// several are offered (see `ResolvedAd::best_media_file`).
pub fn resolve_widget_ad(cfg: &AdspaceConfig, target_width: i32) -> Result<ResolvedCreative> {
    let ad = request_ad(&cfg.agent, target_width, target_width, cfg.partner.as_deref(), None)
        .context("requesting ad from Adspace Exchange")?;

    if let Some(prefetch_spec) = ad.extensions.get(ext_names::PREFETCH) {
        match resolve_prefetch_urls(&cfg.agent, prefetch_spec) {
            Ok(urls) => for url in urls {
                if let Err(e) = download_creative(&cfg.agent, &cfg.cache_dir, &url) {
                    log::warn!("adspace: prefetch of {url} failed (non-fatal): {e:#}");
                }
            },
            Err(e) => log::warn!("adspace: could not resolve prefetch list (non-fatal): {e:#}"),
        }
    }

    let media = ad.best_media_file(target_width)
        .context("ad response has no usable MediaFile")?
        .clone();
    let local_path = download_creative(&cfg.agent, &cfg.cache_dir, &media.url)
        .context("downloading ad creative")?;

    Ok(ResolvedCreative { local_path, mime_type: media.mime_type, duration: ad.duration })
}

/// Confirmed real (xibo-dotnetclient issue #268) `Extension` `type=`
/// values used by Xibo's own VAST wrapper convention. Not all are acted
/// on here (see individual comments) -- listed together so the full set
/// is visible in one place for whoever extends this later.
pub mod ext_names {
    /// Bulk-prefetch other resources referenced by the ad -- see
    /// `resolve_prefetch_urls`.
    pub const PREFETCH: &str = "xiboPrefetch";
    /// Which SSP/partner served this -- informational, not acted on.
    pub const PARTNER: &str = "xiboPartner";
    /// Whether wrapper-chain rate limiting should apply -- see
    /// `MAX_WRAPPER_DEPTH`; not currently made configurable per-response,
    /// a fixed conservative default is used regardless (see that
    /// constant's own doc comment for why).
    pub const IS_WRAPPER_RATE_LIMIT: &str = "xiboIsWrapperRateLimit";
}

/// Hard cap on how many `Wrapper` redirects to follow before giving up,
/// matching this project's own earlier porting notes ("rate-limit per
/// partner (max 5 wrap)"). The real client can apparently vary this via
/// `xiboIsWrapperRateLimit`/related settings (per issue #268) -- not
/// implemented here, a fixed conservative cap is used unconditionally
/// instead, safer than silently trusting a remote response to tell us
/// how many times to keep following its own redirects.
const MAX_WRAPPER_DEPTH: u32 = 5;

/// A single resolved, playable media file from a VAST `<MediaFile>`
/// node.
#[derive(Debug, Clone)]
pub struct MediaFile {
    pub url: String,
    pub mime_type: String,
    pub width: i32,
    pub height: i32,
}

/// A fully resolved ad (after following any Wrapper chain down to a
/// final InLine) -- what `request_ad`/`resolve_vast` return.
#[derive(Debug, Clone, Default)]
pub struct ResolvedAd {
    pub media_files: Vec<MediaFile>,
    /// Seconds, from the Linear Creative's `<Duration>HH:MM:SS(.mmm)</Duration>`.
    pub duration: Option<f64>,
    /// Extension `type=` -> raw text content, merged across the whole
    /// Wrapper chain (an InLine's own extensions take precedence over
    /// ones seen earlier in the chain, on the assumption that the final,
    /// most specific responder is most authoritative -- not verified
    /// against real Xibo/IAB guidance, a reasonable-seeming default).
    pub extensions: HashMap<String, String>,
}

impl ResolvedAd {
    /// Pick the "best" MediaFile for a widget of the given pixel size --
    /// prefers progressive MP4 closest in width to the target, falling
    /// back to just the first available file. Deliberately simple (no
    /// bitrate/codec negotiation) -- a kiosk player showing a fixed-size
    /// widget doesn't need adaptive selection the way a general-purpose
    /// video player would.
    pub fn best_media_file(&self, target_width: i32) -> Option<&MediaFile> {
        self.media_files.iter()
            .filter(|m| m.mime_type.starts_with("video/") || m.mime_type.starts_with("image/"))
            .min_by_key(|m| (m.width - target_width).abs())
            .or_else(|| self.media_files.first())
    }
}

/// Parse one VAST XML response body. Returns `Ok(Err(wrapper_uri))` if
/// this response is a `Wrapper` (caller should follow `wrapper_uri` and
/// try again) or `Ok(Ok(ad))` if it's a final `InLine`. (An
/// `anyhow::Result` around a `std::result::Result` reads a little
/// awkwardly, but keeps "network/parse error" and "this hop redirects
/// further" as clearly distinct outcomes for the caller.)
fn parse_vast_response(body: &str) -> Result<std::result::Result<ResolvedAd, String>> {
    let tree = Element::from_reader(body.as_bytes()).context("parsing VAST XML")?;
    let ad_node = tree.find("Ad").context("VAST response has no <Ad>")?;

    if let Some(wrapper) = ad_node.find("Wrapper") {
        let tag_uri = wrapper.find("VASTAdTagURI")
            .context("Wrapper has no VASTAdTagURI")?
            .text().trim().to_string();
        return Ok(Err(tag_uri));
    }

    let inline = ad_node.find("InLine").context("Ad has neither Wrapper nor InLine")?;

    let mut media_files = Vec::new();
    let mut duration = None;
    if let Some(creatives) = inline.find("Creatives") {
        for creative in creatives.find_all("Creative") {
            if let Some(linear) = creative.find("Linear") {
                if duration.is_none() {
                    if let Some(d) = linear.find("Duration") {
                        duration = parse_vast_duration(d.text().trim());
                    }
                }
                if let Some(mf_container) = linear.find("MediaFiles") {
                    for mf in mf_container.find_all("MediaFile") {
                        media_files.push(MediaFile {
                            url: mf.text().trim().to_string(),
                            mime_type: mf.get_attr("type").unwrap_or("").to_string(),
                            width: mf.get_attr("width").and_then(|s| s.parse().ok()).unwrap_or(0),
                            height: mf.get_attr("height").and_then(|s| s.parse().ok()).unwrap_or(0),
                        });
                    }
                }
            }
        }
    }

    let mut extensions = HashMap::new();
    if let Some(exts) = inline.find("Extensions") {
        for ext in exts.find_all("Extension") {
            if let Some(ty) = ext.get_attr("type") {
                extensions.insert(ty.to_string(), ext.text().trim().to_string());
            }
        }
    }

    Ok(Ok(ResolvedAd { media_files, duration, extensions }))
}

/// Parse a VAST `<Duration>` value: `HH:MM:SS` or `HH:MM:SS.mmm`.
fn parse_vast_duration(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let s: f64 = parts[2].parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

/// Follow a chain of VAST `Wrapper` responses (each one's
/// `VASTAdTagURI` points at the next hop) down to a final `InLine`,
/// merging `Extensions` seen along the way (see `ResolvedAd`'s own doc
/// comment on merge precedence). Bails out after `MAX_WRAPPER_DEPTH`
/// hops rather than trusting a chain to terminate on its own -- a
/// malicious or misconfigured upstream could otherwise wrap forever.
pub fn resolve_vast(agent: &ureq::Agent, start_url: &str) -> Result<ResolvedAd> {
    let mut url = start_url.to_string();
    let mut merged_extensions = HashMap::new();
    for depth in 0..=MAX_WRAPPER_DEPTH {
        if depth == MAX_WRAPPER_DEPTH {
            bail!("VAST wrapper chain exceeded {MAX_WRAPPER_DEPTH} hops, giving up");
        }
        let body = agent.get(&url).call()
            .with_context(|| format!("requesting VAST from {url}"))?
            .into_body().read_to_string()
            .context("reading VAST response body")?;
        match parse_vast_response(&body)? {
            Err(next_url) => {
                url = next_url;
            }
            Ok(mut ad) => {
                // Wrapper-chain extensions fill in anything the final
                // InLine didn't itself specify -- see doc comment.
                for (k, v) in merged_extensions {
                    ad.extensions.entry(k).or_insert(v);
                }
                return Ok(ad);
            }
        }
        // Stash this hop's own extensions (Wrappers can carry
        // Extensions too, e.g. xiboPrefetch is documented as a wrapper-
        // level setting in some SSP integrations) before moving on.
        // Re-parsing here is a little wasteful but keeps the control
        // flow above simple; these responses are small.
        if let Ok(tree) = Element::from_reader(body.as_bytes()) {
            if let Some(exts) = tree.find("Ad").and_then(|a| a.find("Wrapper")).and_then(|w| w.find("Extensions")) {
                for ext in exts.find_all("Extension") {
                    if let Some(ty) = ext.get_attr("type") {
                        merged_extensions.insert(ty.to_string(), ext.text().trim().to_string());
                    }
                }
            }
        }
    }
    unreachable!("loop always returns or bails before falling through")
}

/// Make a bid request to the Adspace Exchange and resolve the resulting
/// VAST (following any Wrapper chain). FLAGGED AS UNVERIFIED: endpoint
/// and query parameter names/shape come from this project's own earlier
/// porting session notes, not confirmed against real API documentation
/// (see this module's top-level doc comment).
pub fn request_ad(
    agent: &ureq::Agent,
    width: i32,
    height: i32,
    partner: Option<&str>,
    geo: Option<(f64, f64)>,
) -> Result<ResolvedAd> {
    let mut url = format!("https://exchange.xibo-adspace.com/vast/device?w={width}&h={height}");
    if let Some(partner) = partner {
        url.push_str(&format!("&partner={}", urlencode(partner)));
    }
    if let Some((lat, lng)) = geo {
        url.push_str(&format!("&lat={lat}&lng={lng}"));
    }
    resolve_vast(agent, &url)
}

fn urlencode(s: &str) -> String {
    // Minimal, good enough for the simple partner-name-like strings this
    // is used for -- not a general-purpose URL encoder.
    s.chars().map(|c| {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            c.to_string()
        } else {
            format!("%{:02X}", c as u32)
        }
    }).collect()
}

/// Resolve a `xiboPrefetch` Extension value into a list of URLs to
/// download ahead of time. Documented format (this project's own
/// porting notes): either a bare URL, or
/// `url||urlProp||idProp||mimeTypeProp` describing a JSON array of
/// objects to fetch from `url`, where `urlProp` names the field holding
/// each item's own download URL (`idProp`/`mimeTypeProp` describe other
/// fields on those objects -- not needed just to collect URLs to
/// download, so not used here).
pub fn resolve_prefetch_urls(agent: &ureq::Agent, spec: &str) -> Result<Vec<String>> {
    let spec = spec.trim();
    if let Some((url, rest)) = spec.split_once("||") {
        let url_prop = rest.split("||").next().unwrap_or("url");
        let body = agent.get(url).call()
            .with_context(|| format!("requesting prefetch list from {url}"))?
            .into_body().read_to_string()
            .context("reading prefetch list response")?;
        let json: serde_json::Value = serde_json::from_str(&body)
            .context("prefetch list response is not valid JSON")?;
        let arr = json.as_array().context("prefetch list response is not a JSON array")?;
        Ok(arr.iter()
            .filter_map(|item| item.get(url_prop)?.as_str().map(String::from))
            .collect())
    } else if spec.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![spec.to_string()])
    }
}

/// Download and cache a single creative (or prefetched resource) file
/// into `cache_dir`, keyed by an MD5 hash of its URL (ad creatives don't
/// have a stable Xibo media id the way normal library media does, so
/// there's no natural cache key besides the URL itself). Returns the
/// local file path. Does nothing (just returns the existing path) if
/// already cached -- ad creatives are treated as immutable once fetched,
/// there's no equivalent of Xibo's own md5-based "has this changed"
/// check for third-party ad content.
pub fn download_creative(agent: &ureq::Agent, cache_dir: &Path, url: &str) -> Result<PathBuf> {
    fs::create_dir_all(cache_dir).context("creating adspace cache dir")?;
    let key = hex::encode(Md5::digest(url.as_bytes()));
    let ext = url.rsplit('.').next()
        .filter(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin");
    let path = cache_dir.join(format!("{key}.{ext}"));
    if path.is_file() {
        return Ok(path);
    }
    let body = agent.get(url).call()
        .with_context(|| format!("downloading creative {url}"))?
        .into_body();
    let tmp_path = path.with_extension("tmp");
    let mut file = std::io::BufWriter::new(fs::File::create(&tmp_path)?);
    std::io::copy(&mut body.into_reader(), &mut file)
        .context("writing creative to disk")?;
    drop(file);
    fs::rename(&tmp_path, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_INLINE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<VAST version="4.0">
  <Ad id="123">
    <InLine>
      <AdSystem>TestAdSystem</AdSystem>
      <AdTitle>Test Ad</AdTitle>
      <Creatives>
        <Creative>
          <Linear>
            <Duration>00:00:15</Duration>
            <MediaFiles>
              <MediaFile delivery="progressive" type="video/mp4" width="1920" height="1080"><![CDATA[https://example.com/ad.mp4]]></MediaFile>
              <MediaFile delivery="progressive" type="video/mp4" width="640" height="360"><![CDATA[https://example.com/ad_small.mp4]]></MediaFile>
            </MediaFiles>
          </Linear>
        </Creative>
      </Creatives>
      <Extensions>
        <Extension type="xiboPrefetch"><![CDATA[https://example.com/prefetch.json]]></Extension>
      </Extensions>
    </InLine>
  </Ad>
</VAST>"#;

    const SAMPLE_WRAPPER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<VAST version="4.0">
  <Ad id="456">
    <Wrapper>
      <AdSystem>WrapperSystem</AdSystem>
      <VASTAdTagURI><![CDATA[https://example.com/next_hop.xml]]></VASTAdTagURI>
      <Extensions>
        <Extension type="xiboPartner"><![CDATA[test-partner]]></Extension>
      </Extensions>
    </Wrapper>
  </Ad>
</VAST>"#;

    // Real-world VAST 2.0 sample (trimmed of tracking pixels/clickthrough
    // for brevity), sourced from Wikipedia's Video_Ad_Serving_Template
    // article which cites the official IAB VAST spec -- used here to
    // confirm the parser handles genuine third-party VAST output, not
    // just my own synthetic test fixture above.
    const REAL_WORLD_SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<VAST version="2.0">
<Ad id="229">
<InLine>
<AdSystem version="4.9.0-10">LiveRail</AdSystem>
<AdTitle><![CDATA[LiveRail creative 1]]></AdTitle>
<Creatives>
<Creative sequence="1" id="331">
<Linear>
<Duration>00:00:09</Duration>
<MediaFiles>
<MediaFile delivery="progressive" bitrate="256" width="480" height="352" type="video/x-flv"><![CDATA[http://cdn.liverail.com/adasset4/1331/229/331/lo.flv]]></MediaFile>
</MediaFiles>
</Linear>
</Creative>
</Creatives>
</InLine>
</Ad>
</VAST>"#;

    #[test]
    fn parses_real_world_vast_sample() {
        let ad = parse_vast_response(REAL_WORLD_SAMPLE).unwrap().unwrap();
        assert_eq!(ad.duration, Some(9.0));
        assert_eq!(ad.media_files.len(), 1);
        assert_eq!(ad.media_files[0].url, "http://cdn.liverail.com/adasset4/1331/229/331/lo.flv");
        assert_eq!(ad.media_files[0].mime_type, "video/x-flv");
        assert_eq!(ad.media_files[0].width, 480);
        assert_eq!(ad.media_files[0].height, 352);
    }

    #[test]
    fn parses_inline_media_files_and_duration() {
        let result = parse_vast_response(SAMPLE_INLINE).unwrap();
        let ad = result.expect("should be InLine, not Wrapper");
        assert_eq!(ad.media_files.len(), 2);
        assert_eq!(ad.media_files[0].url, "https://example.com/ad.mp4");
        assert_eq!(ad.media_files[0].width, 1920);
        assert_eq!(ad.duration, Some(15.0));
    }

    #[test]
    fn parses_inline_extensions() {
        let result = parse_vast_response(SAMPLE_INLINE).unwrap();
        let ad = result.unwrap();
        assert_eq!(ad.extensions.get(ext_names::PREFETCH),
                   Some(&"https://example.com/prefetch.json".to_string()));
    }

    #[test]
    fn detects_wrapper_and_extracts_tag_uri() {
        let result = parse_vast_response(SAMPLE_WRAPPER).unwrap();
        let next_uri = result.expect_err("should be a Wrapper, not InLine");
        assert_eq!(next_uri, "https://example.com/next_hop.xml");
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_vast_duration("00:00:15"), Some(15.0));
        assert_eq!(parse_vast_duration("00:01:30"), Some(90.0));
        assert_eq!(parse_vast_duration("01:00:00"), Some(3600.0));
        assert_eq!(parse_vast_duration("00:00:15.500"), Some(15.5));
        assert_eq!(parse_vast_duration("garbage"), None);
        assert_eq!(parse_vast_duration(""), None);
    }

    #[test]
    fn best_media_file_picks_closest_width() {
        let ad = parse_vast_response(SAMPLE_INLINE).unwrap().unwrap();
        let best = ad.best_media_file(600).unwrap();
        assert_eq!(best.url, "https://example.com/ad_small.mp4");
        let best = ad.best_media_file(1800).unwrap();
        assert_eq!(best.url, "https://example.com/ad.mp4");
    }

    #[test]
    fn best_media_file_none_when_no_media_files() {
        let ad = ResolvedAd::default();
        assert!(ad.best_media_file(1920).is_none());
    }

    #[test]
    fn resolve_prefetch_bare_url() {
        // no "||" separators -> treated as a single direct URL, no HTTP
        // call needed to resolve it further
        let agent = ureq::Agent::new_with_defaults();
        let urls = resolve_prefetch_urls(&agent, "https://example.com/single.jpg").unwrap();
        assert_eq!(urls, vec!["https://example.com/single.jpg"]);
    }

    #[test]
    fn resolve_prefetch_empty_spec() {
        let agent = ureq::Agent::new_with_defaults();
        let urls = resolve_prefetch_urls(&agent, "").unwrap();
        assert!(urls.is_empty());
    }

    #[test]
    fn malformed_vast_is_an_error_not_a_panic() {
        assert!(parse_vast_response("<not>vast</not>").is_err());
        assert!(parse_vast_response("not even xml").is_err());
    }
}
