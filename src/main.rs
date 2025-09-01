//! `cargo run --example download`
extern crate env_logger;
use indicatif::{ProgressBar, ProgressStyle};
use log::info;
use reqwest::Client;
use tokio::io::AsyncWriteExt;

use env_logger::Env;
use futures_util::StreamExt;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio::time::{Duration, sleep};

use tokio::fs::{self, File};
use tokio::io::AsyncBufReadExt;
use tokio::task;

const CACHE_FILE: &str = "cache.json";
const DOWNLOADS_FOLDER: &str = "/media/dev/ssd-linux/tmp/downloads";
const URLS_FILE: &str = "urls.txt";
const MAX_CONCURRENT_DOWNLOADS: usize = 5;
const MAX_DOWNLOAD_RETRIES: u32 = 5;

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
        self.videos
            .iter()
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
    let sanitized_name: String = s
        .chars()
        .map(|c| {
            if invalid_chars.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    if sanitized_name.is_empty() {
        "untitled".to_string()
    } else {
        sanitized_name
    }
}

fn find_first<'a>(selector: &Selector, el: &'a ElementRef) -> anyhow::Result<ElementRef<'a>> {
    el.select(selector).next().ok_or(anyhow::anyhow!(
        "selector: {:?} didn't found a thing at {:?}",
        selector,
        el
    ))
}

fn full_hdxx_parse_video_links(html: Html, origin_url: &str) -> anyhow::Result<VideoScrapResult> {
    let root_el = html.root_element();
    let first_el_selector = Selector::parse(".player-wrap__wrap").unwrap();
    let first_el = find_first(&first_el_selector, &root_el)?;
    let next_selector = Selector::parse("[id^='video-']").unwrap();
    let vid_el = find_first(&next_selector, &first_el)?;

    let sources: HashMap<u32, String> = vid_el
        .child_elements()
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

    let title_selector =
        Selector::parse("#tab_video_info > div:nth-child(1) > h1:nth-child(1)").unwrap();
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

// download_video function with retry logic
pub async fn download_video(
    client: Arc<Client>,
    url: String,
    output_path: PathBuf,
) -> anyhow::Result<()> {
    // Check if the file already exists before starting the retry loop
    if output_path.exists() {
        info!("File already exists: {:?}, skipping download.", output_path);
        return Ok(());
    }

    for attempt in 0..MAX_DOWNLOAD_RETRIES {
        info!(
            "Starting download from: {} (Attempt {} of {})",
            url,
            attempt + 1,
            MAX_DOWNLOAD_RETRIES
        );

        let res = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                log::warn!(
                    "Attempt {} failed to get response from '{}': {}",
                    attempt + 1,
                    &url,
                    e
                );
                sleep(Duration::from_secs(2u64.pow(attempt))).await; // Exponential backoff
                continue;
            }
        };

        let total_size = match res.content_length() {
            Some(size) => size,
            None => {
                log::warn!(
                    "Attempt {} failed to get content length from '{}'",
                    attempt + 1,
                    &url
                );
                sleep(Duration::from_secs(2u64.pow(attempt))).await;
                continue;
            }
        };

        let pb = ProgressBar::new(total_size);
        pb.set_style(ProgressStyle::default_bar()
            .template("{msg}\n{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})").unwrap()
            .progress_chars("#>-"));
        pb.set_message(format!("Downloading file {}", url));

        let mut file = match File::create(&output_path).await {
            Ok(f) => f,
            Err(e) => {
                log::warn!(
                    "Attempt {} failed to create file {:?}: {}",
                    attempt + 1,
                    output_path,
                    e
                );
                sleep(Duration::from_secs(2u64.pow(attempt))).await;
                continue;
            }
        };

        let mut stream = res.bytes_stream();
        let mut downloaded = 0;

        let mut download_succeeded = true;
        while let Some(item) = stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    log::warn!(
                        "Error while downloading chunk on attempt {}: {}",
                        attempt + 1,
                        e
                    );
                    download_succeeded = false;
                    break;
                }
            };

            if let Err(e) = file.write_all(&chunk).await {
                log::warn!(
                    "Error while writing to file on attempt {}: {}",
                    attempt + 1,
                    e
                );
                download_succeeded = false;
                break;
            }

            let new = min(downloaded + (chunk.len() as u64), total_size);
            downloaded = new;
            pb.set_position(new);
        }

        pb.finish_with_message(format!(
            "✅ Download complete for {}",
            output_path.to_string_lossy()
        ));

        if download_succeeded {
            return Ok(());
        } else {
            // Clean up partial file on failure before retrying
            let _ = fs::remove_file(&output_path).await;
            log::warn!("Download failed on attempt {}. Retrying...", attempt + 1);
            sleep(Duration::from_secs(2u64.pow(attempt))).await; // Exponential backoff
        }
    }

    Err(anyhow::anyhow!(
        "Failed to download file from '{}' after {} attempts",
        url,
        MAX_DOWNLOAD_RETRIES
    ))
}

async fn process_url(
    client: Arc<Client>,
    url: String,
    cache: Arc<tokio::sync::Mutex<HashMap<String, VideoScrapResult>>>,
) -> anyhow::Result<()> {
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
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS));

    for url in urls {
        let client = client.clone();
        let cache = cache.clone();
        let semaphore = semaphore.clone();

        let handle = task::spawn(async move {
            let _permit = semaphore.acquire().await.unwrap();
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
