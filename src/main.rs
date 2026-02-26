use std::collections::HashMap;
use std::env;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use epub_builder::{EpubBuilder, EpubContent, ReferenceType, ZipLibrary};
use regex::Regex;
use scraper::{Html, Selector};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

struct Metadata {
    title: String,
    author: String,
}

#[derive(Clone)]
struct Chapter {
    index: usize,
    title: String,
    href: String,
}

struct CrawlResult {
    index: usize,
    content: String,
}

async fn fetch_metadata(client: &reqwest::Client, url: &str) -> Result<Metadata> {
    let resp = client.get(url).send().await?.text().await?;

    let title_selector =
        Selector::parse(".container .focusbox-title").expect("Failed to parse excerpts");
    let author_selector =
        Selector::parse(".container .focusbox-text p").expect("Failed to parse excerpts");

    let document = Html::parse_document(&resp);

    let title = document
        .select(&title_selector)
        .next()
        .and_then(|elem| elem.text().next())
        .unwrap_or("未知标题")
        .to_string();

    static AUTHOR_RE: OnceLock<Regex> = OnceLock::new();
    let author = document
        .select(&author_selector)
        .next()
        .and_then(|elem| elem.text().next())
        .map_or("未知作者".to_string(), |c| {
            let re = AUTHOR_RE.get_or_init(|| {
                Regex::new(r"作者：\s*([^\s\x22,，]+)").expect("Regex init failed")
            });
            re.captures(c)
                .and_then(|caps| caps.get(1))
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| "未知".to_string())
        });

    Ok(Metadata { author, title })
}

async fn prepare_tasks(client: &reqwest::Client, url: &str) -> Result<Vec<Chapter>> {
    let resp = client.get(url).send().await?.text().await?;

    let document = Html::parse_document(&resp);
    let selector = Selector::parse(".excerpts .excerpt a").expect("Failed to parse excerpts");
    static TITLE_INDEX_RE: OnceLock<Regex> = OnceLock::new();
    let re = TITLE_INDEX_RE
        .get_or_init(|| Regex::new(r"^(\d+)[.\s]+(.*)").expect("Failed to initilize Regex"));

    let mut tasks = Vec::new();
    for elem in document.select(&selector) {
        let href = elem.value().attr("href").unwrap_or("").to_string();
        let idx_title = elem.value().attr("title").unwrap_or("").to_string();

        let (index, title) = match re.captures(&idx_title) {
            Some(caps) => (
                caps.get(1).map_or(0, |m| m.as_str().parse().unwrap_or(0)),
                caps.get(2).map_or("", |m| m.as_str()).to_string(),
            ),
            None => {
                debug!("Failed to match: {}", idx_title);
                (9999, idx_title)
            }
        };
        tasks.push(Chapter { index, title, href });
    }
    info!("Total {} chapters found", tasks.len());

    Ok(tasks)
}

async fn fetch_content(
    client: &reqwest::Client,
    chapter: &Chapter,
    selector: &Selector,
) -> Result<String> {
    let mut attempts = 0;
    const MAX_RETRIES: u32 = 3;

    loop {
        attempts += 1;
        match client.get(&chapter.href).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    let body = resp.text().await?;
                    let document = Html::parse_document(&body);
                    let content = document
                        .select(selector)
                        .next()
                        .map(|elem| elem.text().collect::<Vec<_>>().join(""))
                        .unwrap_or_default();

                    return Ok(format!("## {}\n\n{}\n\n\n", chapter.title, content.trim()));
                } else {
                    warn!(
                        "Failed to request chapter '{}' (Attempt {}/{}), StatusCode: {}",
                        chapter.title,
                        attempts,
                        MAX_RETRIES,
                        resp.status()
                    );
                }
            }
            Err(e) => {
                warn!(
                    "Network error for chapter '{}' (Attempt {}/{}): {}",
                    chapter.title, attempts, MAX_RETRIES, e
                );
            }
        }

        if attempts >= MAX_RETRIES {
            return Ok(format!(
                "## {}\n\nError: Failed to download after {} attempts.\n\n",
                chapter.title, MAX_RETRIES
            ));
        }

        sleep(Duration::from_millis(500 * attempts as u64)).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("useage: cargo run -- <URL>");
        std::process::exit(1);
    }
    // "https://www.zhenhunxiaoshuo.com/wozaifeitushijiesaolaji/"
    let base_url = &args[1];
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .build()?;

    let metadata = fetch_metadata(&client, base_url).await?;

    let tasks = prepare_tasks(&client, base_url).await?;
    let num_jobs = tasks.len();

    let (tx, mut rx) = mpsc::channel(num_jobs);
    let client = Arc::new(client);

    let content_selector = Arc::new(
        Selector::parse(".article-content").expect("Failed to parse chapter content selector"),
    );

    let semaphore = Arc::new(Semaphore::new(2));

    for task in tasks.iter().cloned() {
        let tx = tx.clone();
        let client = Arc::clone(&client);
        let selector = Arc::clone(&content_selector);

        let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();

        tokio::spawn(async move {
            info!("Fetching: {}-{}", task.index, task.title);
            sleep(Duration::from_millis(500)).await;

            match fetch_content(&client, &task, &selector).await {
                Ok(content) => {
                    let _ = tx
                        .send(CrawlResult {
                            index: task.index,
                            content,
                        })
                        .await;
                }
                Err(e) => {
                    error!("Task [{}]: {}", task.title, e);
                    let _ = tx
                        .send(CrawlResult {
                            index: task.index,
                            content: format!("## {}\n\nError: {}\n\n", task.title, e),
                        })
                        .await;
                }
            }
            drop(permit);
        });
    }

    drop(tx);

    let mut results_map = HashMap::new();
    while let Some(res) = rx.recv().await {
        results_map.insert(res.index, res.content);
    }

    info!("All chapters have been downloaded. Files are now being merged into EPUB...");
    let mut book_builder = EpubBuilder::new(ZipLibrary::new()?)?;
    book_builder
        .metadata("author", &metadata.author)?
        .metadata("title", &metadata.title)?;

    for task in &tasks {
        if let Some(content) = results_map.get(&task.index) {
            let body_html: String = content
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with("##"))
                .map(|l| format!("<p>{}</p>", l))
                .collect::<Vec<_>>()
                .join("\n");

            let xhtml_content = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
            <html xmlns=\"http://www.w3.org/1999/xhtml\">\n\
            <head>\
                <title>{}</title>\
                <style>p {{ text-indent: 2em; margin: 0.8em 0; line-height: 1.5; }}</style>\
            </head>\
            <body>\
                <h1>{}</h1>\
                {}\
            </body>\
            </html>",
                task.title, task.title, body_html
            );

            let content_name = format!("chapter_{}.xhtml", task.index);
            book_builder.add_content(
                EpubContent::new(content_name, xhtml_content.as_bytes())
                    .title(&task.title)
                    .reftype(ReferenceType::Text),
            )?;
        }
    }

    let filename = format!("{}.epub", metadata.title);
    let mut out_file = std::fs::File::create(&filename)?;
    book_builder.generate(&mut out_file)?;
    info!("SAVED AS: {}", filename);

    Ok(())
}
