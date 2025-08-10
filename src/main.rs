//! `cargo run --example download`
extern crate env_logger;
use indicatif::{ProgressBar, ProgressStyle};
use log::info;
use reqwest::Client;
use tokio::io::AsyncWriteExt;

use env_logger::Env;
use scraper::{ElementRef, Html, Selector};
use std::cmp::min;
use std::hash::{Hash, Hasher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use futures_util::StreamExt;
use serde::{Serialize, Deserialize};

use tokio::fs::{self, File};
use tokio::task;
use tokio::io::AsyncBufReadExt;

const CACHE_FILE: &str = "cache.json";
const DOWNLOADS_FOLDER: &str = "/media/dev/ssd-linux/tmp/downloads";
const URLS_FILE: &str = "urls.txt";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VideoScrapResult {
    origin_url: String,
    models: Vec<String>,
    videos: HashMap<u32, String>,
    title: String,
}

impl PartialEq for VideoScrapResult {
    fn eq(&self, other: &Self) -> bool {
        self.origin_url == other.origin_url
    }
}

impl Eq for VideoScrapResult {}

impl Hash for VideoScrapResult {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.origin_url.hash(state);
    }
}

impl VideoScrapResult {
    fn best_quality_source(&self) -> Option<String> {
        self.videos.iter()
            .max_by_key(|(quality, _)| *quality)
            .map(|(_, source)| source.clone())
    }

    fn file_name(&self) -> String {
        format!("{}.mp4", sanitize_filename(&self.title))
    }
}

async fn get_page_html(client: &Client, url: &str) -> anyhow::Result<scraper::Html> {
    client
        .get(url)
        .send()
        .await?
        .text()
        .await
        .map(|text| Html::parse_document(&text))
        .map_err(|e| anyhow::anyhow!(e))
}

fn sanitize_filename(s: &str) -> String {
    let invalid_chars = ['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    let sanitized_name: String = s.chars()
        .map(|c| if invalid_chars.contains(&c) || c.is_control() { '_' } else { c })
        .collect();
    if sanitized_name.is_empty() {
        "untitled".to_string()
    } else {
        sanitized_name
    }
}

fn find_first<'a>(selector: &Selector, el: &'a ElementRef) -> anyhow::Result<ElementRef<'a>> {
    el.select(selector).next().ok_or(anyhow::anyhow!("selector: {:?} didn't found a thing at {:?}", selector, el))
}

fn full_hdxx_parse_video_links(
    html: Html,
    origin_url: &str,
) -> anyhow::Result<VideoScrapResult> {
    let root_el = html.root_element();
    let first_el_selector = Selector::parse(".player-wrap__wrap").unwrap();
    let first_el = find_first(&first_el_selector, &root_el)?;
    let next_selector = Selector::parse("[id^='video-']").unwrap();
    let vid_el = find_first(&next_selector, &first_el)?;

    let sources: HashMap<u32, String> = vid_el.child_elements()
        .filter_map(|source| {
            if let (Some(quality), Some(src)) = (source.attr("label"), source.attr("src")) {
                let quality_str = quality.split('p').next()?;
                let quality_num = quality_str.parse::<u32>().ok()?;
                Some((quality_num, src.to_string()))
            } else {
                None
            }
        })
        .collect();

    let title_selector = Selector::parse("#tab_video_info > div:nth-child(1) > h1:nth-child(1)").unwrap();
    let title = find_first(&title_selector, &html.root_element())?.inner_html();

    let models: Vec<String> = html
        .select(&Selector::parse("a.btn_model").unwrap())
        .map(|el| el.inner_html())
        .collect();

    Ok(VideoScrapResult {
        origin_url: origin_url.to_string(),
        models,
        title,
        videos: sources,
    })
}

async fn load_cache() -> HashMap<String, VideoScrapResult> {
    if let Ok(data) = fs::read(CACHE_FILE).await {
        serde_json::from_slice(&data).unwrap_or_default()
    } else {
        HashMap::new()
    }
}

async fn save_cache(cache: &HashMap<String, VideoScrapResult>) {
    if let Ok(data) = serde_json::to_string_pretty(cache) {
        fs::write(CACHE_FILE, data).await.unwrap();
    }
}

async fn read_urls_from_file(file_path: &Path) -> anyhow::Result<Vec<String>> {
    let mut urls = Vec::new();
    let file = File::open(file_path).await?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut line = String::new();
    while reader.read_line(&mut line).await? > 0 {
        let trimmed_line = line.trim();
        if !trimmed_line.is_empty() {
            urls.push(trimmed_line.to_string());
        }
        line.clear();
    }
    Ok(urls)
}

pub async fn download_video(client: Arc<Client>, url: String, output_path: PathBuf) -> anyhow::Result<()> {
    info!("Starting download from: {}", url);
    
    if output_path.exists() {
        info!("File already exists: {:?}, skipping download.", output_path);
        return Ok(());
    }

    let res = client
        .get(&url)
        .send()
        .await
        .or(Err(anyhow::anyhow!("Failed to GET from '{}'", &url)))?;
    let total_size = res
        .content_length()
        .ok_or(anyhow::anyhow!("Failed to get content length from '{}'", &url))?;

    let pb = ProgressBar::new(total_size);
    pb.set_style(ProgressStyle::default_bar()
        .template("{msg}\n{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})").unwrap()
        .progress_chars("#>-"));
    pb.set_message(format!("Downloading file {}", url));

    let mut file = File::create(&output_path).await?;
    let mut stream = res.bytes_stream();
    let mut downloaded = 0;

    while let Some(item) = stream.next().await {
        let chunk = item.or(Err(anyhow::anyhow!("Error while downloading file")))?;
        file.write_all(&chunk).await
            .or(Err(anyhow::anyhow!("Error while writing to file")))?;
        let new = min(downloaded + (chunk.len() as u64), total_size);
        downloaded = new;
        pb.set_position(new);
    }

    pb.finish_with_message(format!("✅ Download complete for {}", output_path.to_string_lossy()));
    Ok(())
}

async fn process_url(client: Arc<Client>, url: String, cache: Arc<tokio::sync::Mutex<HashMap<String, VideoScrapResult>>>) -> anyhow::Result<()> {
    let mut cache_guard = cache.lock().await;
    let scrap_result = if let Some(cached_result) = cache_guard.get(&url) {
        info!("Found {} in cache. Skipping scraping.", url);
        cached_result.clone()
    } else {
        info!("Scraping {}", url);
        let html = get_page_html(&client, &url).await?;
        let result = full_hdxx_parse_video_links(html, &url)?;
        cache_guard.insert(url.clone(), result.clone());
        result
    };
    drop(cache_guard);

    let output_path = Path::new(DOWNLOADS_FOLDER).join(scrap_result.file_name());

    if let Some(source_url) = scrap_result.best_quality_source() {
        download_video(client.clone(), source_url, output_path).await?;
    } else {
        info!("No video sources found for {}", url);
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    let env = Env::default()
        .filter_or("RUST_LOG", "info")
        .write_style_or("RUST_LOG_STYLE", "always");

    env_logger::init_from_env(env);

    let client = Arc::new(reqwest::Client::new());
    fs::create_dir_all(DOWNLOADS_FOLDER).await.unwrap();

    let urls = match read_urls_from_file(Path::new(URLS_FILE)).await {
        Ok(urls) => urls,
        Err(e) => {
            log::error!("Failed to read URLs from file {}: {}", URLS_FILE, e);
            log::info!("Creating a dummy {} file for demonstration.", URLS_FILE);
            fs::write(URLS_FILE, "https://www.fullhdxx.me/xxx-video-example1\nhttps://www.fullhdxx.me/xxx-video-example2\n").await.unwrap();
            vec![] // Return an empty vector to prevent further execution with no URLs
        }
    };
    if urls.is_empty() {
        log::info!("No URLs found in {}. Exiting.", URLS_FILE);
        return;
    }
    
    let cache = Arc::new(tokio::sync::Mutex::new(load_cache().await));
    let mut handles = Vec::new();

    for url in urls {
        let client = client.clone();
        let cache = cache.clone();
        let handle = task::spawn(async move {
            if let Err(e) = process_url(client, url.clone(), cache).await {
                log::error!("Error processing {}: {}", url, e);
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // Save the cache at the end
    let final_cache = cache.lock().await;
    save_cache(&final_cache).await;
}