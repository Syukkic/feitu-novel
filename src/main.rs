use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use epub_builder::{EpubBuilder, EpubContent, ReferenceType, ZipLibrary};
use regex::Regex;
use scraper::{Html, Selector};
use tokio::sync::{Semaphore, mpsc};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

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

async fn prepare_tasks(client: &reqwest::Client, url: &str) -> Result<Vec<Chapter>> {
    let resp = client.get(url).send().await?.text().await?;

    let document = Html::parse_document(&resp);
    let selector = Selector::parse(".excerpts .excerpt a").expect("Failed to parse excerpts");
    let re = Regex::new(r"^(\d+)[.\s]+(.*)").expect("Failed to initilize Regex");

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
        info!("Total {} chapters", tasks.len());
        tasks.push(Chapter { index, title, href });
    }

    Ok(tasks)
}

async fn fetch_content(client: &reqwest::Client, chapter: &Chapter) -> Result<String> {
    let resp = client.get(&chapter.href).send().await?;
    if resp.status() != 200 {
        warn!(
            "Failed to request chapter '{}'，StatusCode: {}",
            chapter.title,
            resp.status()
        );
        Ok(format!(
            "{} -> StatusCode: {}",
            &chapter.title,
            resp.status()
        ))
    } else {
        let body = resp.text().await?;
        let document = Html::parse_document(&body);
        let selector =
            Selector::parse(".article-content").expect("Failed to parse chapter content");
        let content = document
            .select(&selector)
            .next()
            .map(|elem| elem.text().collect::<Vec<_>>().join(""))
            .unwrap_or_default();

        Ok(format!("## {}\n\n{}\n\n\n", chapter.title, content.trim()))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    let base_url = "https://www.zhenhunxiaoshuo.com/wozaifeitushijiesaolaji/";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .build()?;

    let tasks = prepare_tasks(&client, base_url).await?;
    let num_jobs = tasks.len();

    let (tx, mut rx) = mpsc::channel(num_jobs);
    let client = Arc::new(client);

    let semaphore = Arc::new(Semaphore::new(2));

    for task in tasks.clone() {
        let tx = tx.clone();
        let client = Arc::clone(&client);
        let permit = Arc::clone(&semaphore);

        tokio::spawn(async move {
            let _permit = permit.acquire_owned().await.unwrap();
            info!("Fetching: {}-{}", task.index, task.title);
            sleep(Duration::from_millis(500)).await;

            match fetch_content(&client, &task).await {
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
        .metadata("author", "有花在野")?
        .metadata("title", "我在废土世界扫垃圾")?;

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

    let filename = "我在废土世界扫垃圾.epub";
    let mut out_file = std::fs::File::create("我在废土世界扫垃圾.epub")?;
    book_builder.generate(&mut out_file)?;
    info!("Saved as: {}", filename);

    Ok(())
}
