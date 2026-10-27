//! aniDB (<https://anidb.net>) anime metadata provider extension for
//! LisekMovie.
//!
//! aniDB's HTTP API returns one anime by its id (`aid`) as gzip-compressed
//! XML:
//!
//! ```text
//! GET http://api.anidb.net:9001/httpapi?request=anime&client={client}&clientver={ver}&protover=1&aid={aid}
//! ```
//!
//! Requirements set by aniDB:
//!
//! - a **registered client** (<https://anidb.net/software/add>): its name
//!   and version are this provider's "API key" in Admin → Plugins, written
//!   `client` or `client:version` (version 1 when left out);
//! - **at most one request every 2 seconds** per client, or aniDB bans the
//!   address for a while: every request of this process waits its turn
//!   ([`MIN_INTERVAL`]), and a ban answer is reported as an error;
//! - responses should be cached; the server only asks when matching.
//!
//! What works: Fix match by aniDB id (`69`, `anidb:69`,
//! `https://anidb.net/anime/69`) and the full lookup (titles in several
//! languages, synopsis, rating, tags, poster).
//!
//! TODO: implement endpoint — title search. The HTTP API has none; aniDB
//! publishes a daily dump of every title instead
//! (`https://anidb.net/api/anime-titles.xml.gz`, at most one download a
//! day) that a local index would be built from. Until then
//! `find_movie`/`find_series`/`search` return nothing and log a warning
//! once.

use std::io::Read;
use std::sync::{LazyLock, Once};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lisek_plugin_api::{
    MediaKind, MetadataProvider, MovieQuery, ProviderCandidate, ProviderError, ProviderMatch,
    SeriesQuery,
};
use quick_xml::Reader;
use quick_xml::events::Event;
use regex::Regex;
use tokio::sync::Mutex;

const DEFAULT_BASE_URL: &str = "http://api.anidb.net:9001/httpapi";
const IMAGE_BASE: &str = "https://cdn-eu.anidb.net/images/main/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// aniDB's flood protection: one request every 2 seconds, plus a margin.
pub const MIN_INTERVAL: Duration = Duration::from_millis(2_500);
const MAX_TAGS: usize = 20;

/// When this process last asked aniDB (shared by every provider instance:
/// the server builds one per request).
static LAST_REQUEST: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));

static SEARCH_TODO: Once = Once::new();

fn warn_search_todo() {
    SEARCH_TODO.call_once(|| {
        tracing::warn!(
            provider = "anidb",
            "TODO: implement endpoint — aniDB has no title search in its HTTP API (needs the \
             anime-titles dump); only lookups by aniDB id work for now (Fix match: type the id)"
        );
    });
}

/// `client` or `client:version`.
fn client_and_version(api_key: &str) -> (String, String) {
    match api_key.trim().split_once(':') {
        Some((client, version)) if !version.trim().is_empty() => {
            (client.trim().to_string(), version.trim().to_string())
        }
        Some((client, _)) => (client.trim().to_string(), "1".into()),
        None => (api_key.trim().to_string(), "1".into()),
    }
}

/// One title of an anime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnidbTitle {
    /// `x-jat` (romanized Japanese), `ja`, `en`, `pl`...
    pub lang: String,
    /// `main`, `official`, `synonym`, `short`.
    pub kind: String,
    pub text: String,
}

/// One anime from the HTTP API.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnidbAnime {
    pub aid: u64,
    /// `TV Series`, `Movie`, `OVA`, `Web`, `TV Special`...
    pub kind: String,
    pub episode_count: Option<u32>,
    pub start_date: Option<String>,
    pub titles: Vec<AnidbTitle>,
    pub description: String,
    pub rating: Option<f64>,
    pub picture: Option<String>,
    /// `(name, weight)`.
    pub tags: Vec<(String, u32)>,
    pub restricted: bool,
}

static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://anidb\.net/\S+ \[([^\]]+)\]").unwrap());

/// aniDB descriptions link characters and creators as
/// `http://anidb.net/ch123 [Name]`; the name stays.
pub fn plain_description(text: &str) -> String {
    LINK.replace_all(text, "$1").trim().to_string()
}

impl AnidbAnime {
    /// An official title in `language`, else English, else the main
    /// (usually romanized) title.
    pub fn title_for(&self, language: &str) -> Option<String> {
        let language = language.split(['-', '_']).next().unwrap_or(language);
        let official = |lang: &str| {
            self.titles
                .iter()
                .find(|t| t.kind == "official" && t.lang == lang)
                .map(|t| t.text.clone())
        };
        official(language)
            .or_else(|| official("en"))
            .or_else(|| self.main_title())
    }

    fn main_title(&self) -> Option<String> {
        self.titles
            .iter()
            .find(|t| t.kind == "main")
            .map(|t| t.text.clone())
    }

    pub fn year(&self) -> Option<i32> {
        self.start_date.as_deref()?.get(..4)?.parse().ok()
    }

    fn poster(&self) -> Option<String> {
        self.picture.as_deref().map(|p| format!("{IMAGE_BASE}{p}"))
    }

    fn to_match(&self, language: &str) -> ProviderMatch {
        let title = self.title_for(language).unwrap_or_default();
        let mut tags = self.tags.clone();
        tags.sort_by_key(|(_, weight)| std::cmp::Reverse(*weight));
        let mut fields = serde_json::json!({
            "anidb_id": self.aid,
            "title": title,
            "year": self.year(),
            "overview": plain_description(&self.description),
            "tags": tags.into_iter().take(MAX_TAGS).map(|(name, _)| name).collect::<Vec<_>>(),
        });
        if let Some(main) = self.main_title().filter(|m| *m != title) {
            fields["original_title"] = serde_json::json!(main);
        }
        if let Some(rating) = self.rating {
            fields["rating"] = serde_json::json!(rating);
        }
        if self.restricted {
            fields["age_rating"] = serde_json::json!(18);
        }
        ProviderMatch {
            poster_url: self.poster(),
            fields,
            ..ProviderMatch::default()
        }
    }
}

/// Parses the `<anime>` document. `Err` carries aniDB's `<error>` message
/// or a parser error.
pub fn parse_anime(xml: &str) -> Result<AnidbAnime, String> {
    let mut reader = Reader::from_str(xml);
    let mut anime = AnidbAnime::default();
    let mut path: Vec<String> = Vec::new();
    let mut title_attrs: Option<(String, String)> = None;
    let mut tag_weight = 0u32;
    let mut tag_name: Option<String> = None;
    let mut error: Option<String> = None;
    let mut buffer = String::new();
    loop {
        match reader.read_event().map_err(|err| err.to_string())? {
            Event::Start(start) => {
                let name = String::from_utf8_lossy(start.name().as_ref()).into_owned();
                let attr = |key: &[u8]| {
                    start
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == key)
                        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
                };
                match (path.last().map(String::as_str), name.as_str()) {
                    (None, "anime") => {
                        anime.aid = attr(b"id").and_then(|v| v.parse().ok()).unwrap_or_default();
                        anime.restricted = attr(b"restricted").as_deref() == Some("true");
                    }
                    (None, "error") => error = Some(String::new()),
                    (Some("titles"), "title") => {
                        title_attrs = Some((
                            attr(b"xml:lang").unwrap_or_default(),
                            attr(b"type").unwrap_or_default(),
                        ));
                    }
                    (Some("tags"), "tag") => {
                        tag_weight = attr(b"weight")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or_default();
                        tag_name = None;
                    }
                    _ => {}
                }
                buffer.clear();
                path.push(name);
            }
            Event::Text(text) => buffer.push_str(&text.decode().map_err(|err| err.to_string())?),
            Event::CData(data) => buffer.push_str(&String::from_utf8_lossy(&data)),
            Event::GeneralRef(entity) => {
                let name = entity.decode().map_err(|err| err.to_string())?;
                match entity.resolve_char_ref().map_err(|err| err.to_string())? {
                    Some(ch) => buffer.push(ch),
                    None => buffer
                        .push_str(quick_xml::escape::resolve_xml_entity(&name).unwrap_or_default()),
                }
            }
            Event::End(_) => {
                let value = std::mem::take(&mut buffer).trim().to_string();
                let parent = path.len().checked_sub(2).map(|i| path[i].as_str());
                match (parent, path.last().map(String::as_str)) {
                    (None, Some("error")) => error = Some(value),
                    (Some("anime"), Some("type")) => anime.kind = value,
                    (Some("anime"), Some("episodecount")) => {
                        anime.episode_count = value.parse().ok()
                    }
                    (Some("anime"), Some("startdate")) => anime.start_date = Some(value),
                    (Some("anime"), Some("description")) => anime.description = value,
                    (Some("anime"), Some("picture")) => anime.picture = Some(value),
                    (Some("ratings"), Some("permanent")) => anime.rating = value.parse().ok(),
                    (Some("titles"), Some("title")) => {
                        let (lang, kind) = title_attrs.take().unwrap_or_default();
                        anime.titles.push(AnidbTitle {
                            lang,
                            kind,
                            text: value,
                        });
                    }
                    (Some("tag"), Some("name")) => tag_name = Some(value),
                    (Some("tags"), Some("tag")) => {
                        if let Some(name) = tag_name.take() {
                            anime.tags.push((name, tag_weight));
                        }
                    }
                    _ => {}
                }
                path.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if let Some(message) = error {
        return Err(message);
    }
    if anime.aid == 0 {
        return Err("no <anime> in the response".into());
    }
    Ok(anime)
}

/// aniDB answers gzip whether asked or not.
fn decode_body(bytes: &[u8]) -> Result<String, ProviderError> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut text = String::new();
        flate2::read::GzDecoder::new(bytes)
            .read_to_string(&mut text)
            .map_err(|err| ProviderError::UnexpectedResponse(err.to_string()))?;
        Ok(text)
    } else {
        String::from_utf8(bytes.to_vec())
            .map_err(|err| ProviderError::UnexpectedResponse(err.to_string()))
    }
}

/// HTTP API client. `base_url` and the request spacing are injectable for
/// tests.
pub struct AnidbClient {
    http: reqwest::Client,
    client: String,
    client_version: String,
    base_url: String,
    interval: Duration,
}

impl AnidbClient {
    pub fn new(api_key: &str, base_url: impl Into<String>, interval: Duration) -> Self {
        let (client, client_version) = client_and_version(api_key);
        Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
            client,
            client_version,
            base_url: base_url.into(),
            interval,
        }
    }

    /// Waits until [`MIN_INTERVAL`] has passed since this process's last
    /// request.
    async fn wait_turn(&self) {
        let mut last = LAST_REQUEST.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.interval {
                tokio::time::sleep(self.interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    /// One anime by its aniDB id. `Ok(None)`: no such anime.
    pub async fn anime(&self, aid: u64) -> Result<Option<AnidbAnime>, ProviderError> {
        if self.client.is_empty() {
            return Err(ProviderError::Unauthorized);
        }
        self.wait_turn().await;
        let aid = aid.to_string();
        let response = self
            .http
            .get(&self.base_url)
            .query(&[
                ("request", "anime"),
                ("client", self.client.as_str()),
                ("clientver", self.client_version.as_str()),
                ("protover", "1"),
                ("aid", aid.as_str()),
            ])
            .send()
            .await
            .map_err(|err| ProviderError::Request(err.to_string()))?
            .error_for_status()
            .map_err(|err| ProviderError::Request(err.to_string()))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|err| ProviderError::Request(err.to_string()))?;
        match parse_anime(&decode_body(&bytes)?) {
            Ok(anime) => Ok(Some(anime)),
            Err(message) => {
                let lower = message.to_lowercase();
                if lower.contains("not found") {
                    Ok(None)
                } else if lower.contains("client") {
                    Err(ProviderError::Unauthorized)
                } else {
                    Err(ProviderError::UnexpectedResponse(format!(
                        "aniDB: {message}"
                    )))
                }
            }
        }
    }
}

/// The [`MetadataProvider`] for `"anidb"`.
pub struct AnidbProvider {
    api_key: String,
    base_url: String,
    interval: Duration,
}

impl AnidbProvider {
    /// `api_key`: the registered client, `client` or `client:version`.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL, MIN_INTERVAL)
    }

    pub fn with_base_url(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        interval: Duration,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            interval,
        }
    }

    fn client(&self) -> AnidbClient {
        AnidbClient::new(&self.api_key, self.base_url.clone(), self.interval)
    }
}

#[async_trait]
impl MetadataProvider for AnidbProvider {
    fn provider_key(&self) -> &'static str {
        "anidb"
    }

    async fn find_movie(
        &self,
        _query: &MovieQuery<'_>,
        _lang: &str,
    ) -> Result<Option<ProviderMatch>, ProviderError> {
        warn_search_todo();
        Ok(None)
    }

    async fn find_series(
        &self,
        _query: &SeriesQuery<'_>,
        _lang: &str,
    ) -> Result<Option<ProviderMatch>, ProviderError> {
        warn_search_todo();
        Ok(None)
    }

    async fn search(
        &self,
        _kind: MediaKind,
        _title: &str,
        _year: Option<i32>,
        _lang: &str,
    ) -> Result<Vec<ProviderCandidate>, ProviderError> {
        warn_search_todo();
        Ok(Vec::new())
    }

    async fn lookup(
        &self,
        _kind: MediaKind,
        provider_id: &str,
        lang: &str,
    ) -> Result<Option<ProviderMatch>, ProviderError> {
        let Ok(aid) = provider_id.trim().parse::<u64>() else {
            return Ok(None);
        };
        Ok(self
            .client()
            .anime(aid)
            .await?
            .map(|anime| anime.to_match(lang)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lisek_plugin_api::DirectId;
    use std::io::Write;
    use wiremock::matchers::{method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ANIME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<anime id="69" restricted="false">
  <type>TV Series</type>
  <episodecount>1100</episodecount>
  <startdate>1999-10-20</startdate>
  <titles>
    <title xml:lang="x-jat" type="main">One Piece</title>
    <title xml:lang="ja" type="official">ワンピース</title>
    <title xml:lang="pl" type="official">One Piece: Wielka przygoda</title>
    <title xml:lang="en" type="official">One Piece</title>
  </titles>
  <description>The pirate http://anidb.net/ch123 [Monkey D. Luffy] &amp; his crew.</description>
  <ratings><permanent count="10000">8.17</permanent><temporary count="1">8.2</temporary></ratings>
  <picture>270153.jpg</picture>
  <tags>
    <tag id="1" weight="300"><name>pirates</name></tag>
    <tag id="2" weight="600"><name>shounen</name></tag>
  </tags>
</anime>"#;

    fn gzip(text: &str) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(text.as_bytes()).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn parses_the_anime_document() {
        let anime = parse_anime(ANIME).unwrap();
        assert_eq!(
            (anime.aid, anime.kind.as_str(), anime.episode_count),
            (69, "TV Series", Some(1100))
        );
        assert_eq!(anime.year(), Some(1999));
        assert_eq!(anime.rating, Some(8.17));
        assert_eq!(
            anime.title_for("pl").as_deref(),
            Some("One Piece: Wielka przygoda")
        );
        assert_eq!(anime.title_for("de").as_deref(), Some("One Piece"));
        assert_eq!(
            plain_description(&anime.description),
            "The pirate Monkey D. Luffy & his crew."
        );
        let matched = anime.to_match("pl");
        assert_eq!(
            matched.fields["tags"],
            serde_json::json!(["shounen", "pirates"])
        );
        assert_eq!(matched.fields["original_title"], "One Piece");
        assert_eq!(
            matched.poster_url.as_deref(),
            Some("https://cdn-eu.anidb.net/images/main/270153.jpg")
        );
        assert_eq!(
            parse_anime("<error code=\"302\">Client Values Missing or Invalid</error>")
                .unwrap_err(),
            "Client Values Missing or Invalid"
        );
        assert_eq!(
            client_and_version("lisekmovie:3"),
            ("lisekmovie".into(), "3".into())
        );
        assert_eq!(
            client_and_version("lisekmovie"),
            ("lisekmovie".into(), "1".into())
        );
    }

    #[tokio::test]
    async fn looks_up_an_id_through_the_gzip_xml_api() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("aid", "69"))
            .and(query_param("client", "lisekmovie"))
            .and(query_param("clientver", "2"))
            .and(query_param("protover", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(gzip(ANIME)))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("aid", "1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(gzip("<error>Anime not found</error>")),
            )
            .mount(&server)
            .await;
        let provider = AnidbProvider::with_base_url("lisekmovie:2", server.uri(), Duration::ZERO);

        let candidate = provider
            .lookup_by_id(MediaKind::Series, &DirectId::Numeric("69".into()), "en")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                candidate.provider_id.as_str(),
                candidate.title.as_str(),
                candidate.year
            ),
            ("69", "One Piece", Some(1999))
        );
        assert!(
            provider
                .lookup(MediaKind::Series, "1", "en")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            provider
                .search(MediaKind::Series, "One Piece", None, "en")
                .await
                .unwrap()
                .is_empty()
        );
        let unregistered = AnidbProvider::with_base_url("", server.uri(), Duration::ZERO);
        assert!(matches!(
            unregistered.lookup(MediaKind::Series, "69", "en").await,
            Err(ProviderError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn requests_are_spaced() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ANIME))
            .mount(&server)
            .await;
        let provider =
            AnidbProvider::with_base_url("lisekmovie", server.uri(), Duration::from_millis(300));
        let started = Instant::now();
        provider
            .lookup(MediaKind::Series, "69", "en")
            .await
            .unwrap();
        provider
            .lookup(MediaKind::Series, "69", "en")
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn provider_key_is_anidb() {
        assert_eq!(AnidbProvider::new("c").provider_key(), "anidb");
    }
}
