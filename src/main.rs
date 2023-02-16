use clap::Parser;
use reqwest;
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use url;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Part {
    title: String,
}

#[derive(Deserialize, Serialize, Debug, Default, Clone)]
struct Book {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    #[serde(rename = "nothing")]
    title: String,
    #[serde(rename = "title")]
    collection_title: String,
    parts: Vec<Part>,
    #[serde(default)]
    page_urls: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct TeachingTool {
    id: u64,
    #[serde(default)]
    book: Book,
}

#[derive(Serialize, Deserialize, Debug)]
struct Package {
    id: u64,
    authors: String,
    publishing_house: String,
    teaching_tools: Vec<TeachingTool>,
}

async fn save_page_to_file(
    client: Arc<reqwest::Client>,
    book_dir: &str,
    page_url: &str,
    page_number: u64,
) {
    let path_str = String::from(book_dir)
        + &String::from("/")
        + &page_number.to_string()
        + &String::from(".png");
    let path = Path::new(&path_str);
    match tokio::fs::File::create(path).await {
        Ok(mut file) => loop {
            if let Ok(r) = client.get(page_url).send().await {
                let file_as_bytes = r.bytes().await.unwrap();
                file.write_all(&file_as_bytes).await.unwrap();
                file.flush().await.unwrap();
                println!("SUCCESSFULLY DOWNLOADED PAGE {}", &page_number);
                break;
            }
        },
        Err(e) => {
            println!("error {}", &e);
        }
    }
}

async fn download_package(client: Arc<reqwest::Client>, id: u64) -> Option<Package> {
    let url = reqwest::Url::parse_with_params(
        &(String::from("https://klase.eduka.lt/api/authenticated/teaching-package/")
            + &id.to_string()),
        [("withTeachingTools", "1")],
    )
    .unwrap();
    let resp = client.get(url).send().await.unwrap();
    let text = resp.text().await.unwrap();
    let package = serde_json::from_str::<Package>(&text);
    if let Ok(mut package) = package {
        let mut books: Vec<Book> = Vec::new();
        for teaching_tool in &mut package.teaching_tools {
            let mut book: Book = serde_json::from_str(
                &client
                    .get(
                        &(String::from(
                            "https://klase.eduka.lt/api/authenticated/part/show-by-teaching-tool/",
                        ) + &teaching_tool.id.to_string()),
                    )
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap(),
            )
            .unwrap();
            book.title = book.collection_title.clone() + ": " + &book.parts.get(0).unwrap().title;
            book.id = teaching_tool.id;
            teaching_tool.book = book.clone();
            let pages_json: serde_json::Value = serde_json::from_str(
                &client
                    .get(
                        &(String::from(
                            "https://klase.eduka.lt/api/authenticated/teaching-tool/pages/",
                        ) + &book.id.to_string()),
                    )
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap(),
            )
            .unwrap();
            let pages_objects_array = pages_json.get("pages").unwrap().as_array().unwrap();
            for page in pages_objects_array {
                let img_url_frag = page["img"]["1140"].as_str();
                if let Some(img_url_frag) = img_url_frag {
                    book.page_urls
                        .push(String::from("https://klase.eduka.lt") + img_url_frag);
                } else {
                    println!("Couldn't get page by {:?}", &page)
                }
            }
            books.push(book);
        }
        for book in books {
            let book_dir = String::from("./") + &book.title + " ;;; " + &book.id.to_string();
            // skip already started to dl books
            if Path::new(&book_dir).is_dir() {
                println!("SKIPPING");
                return None;
            }
            fs::create_dir_all(&book_dir).unwrap();
            let f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open(String::from(&book_dir) + "/info.json")
                .unwrap();
            serde_json::to_writer_pretty(f, &package).unwrap();

            let mut handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
            for (i, page) in book.page_urls.iter().enumerate() {
                let cl_clone = client.clone();
                let book_dir = book_dir.clone();
                let p_clone = page.clone();

                handles.push(tokio::spawn(async move {
                    save_page_to_file(cl_clone, &book_dir, &p_clone, i.try_into().unwrap()).await;
                }));
                if i % 10 == 0 {
                    for handle in &mut handles {
                        handle.await.unwrap();
                    }
                    handles.clear();
                }
            }
            for handle in &mut handles {
                handle.await.unwrap();
            }
            handles.clear();
            println!("SUCCESSFULLY DOWNLOADED BOOK {}", &book.title);
        }
        Some(package)
    } else {
        None
    }
}

fn prepare_package(package: &Package) {
    for teaching_tool in &package.teaching_tools {
        let book_dir = String::from("./")
            + &teaching_tool.book.title
            + " ;;; "
            + &teaching_tool.book.id.to_string();
        assert!(Command::new("bash")
            .arg("-c")
            .arg(
                String::from("img2pdf $(ls *.png | sort -n) | ocrmypdf -l lit - ")
                    + &teaching_tool.book.id.to_string()
                    + ".pdf"
            )
            .current_dir(fs::canonicalize(&book_dir).unwrap())
            .status()
            .expect("failed to execute process")
            .success());
    }
}

#[derive(Parser)]
struct Cli {
    #[arg(short, long)]
    username: String,
    #[arg(short, long)]
    password: String,
    books: Vec<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let client = Arc::new(
        reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .unwrap(),
    );
    let mut login_map = HashMap::new();
    login_map.insert("username", &cli.username);
    login_map.insert("password", &cli.password);
    let login_response = client
        .post("https://klase.eduka.lt/api/anonymously/login")
        .json(&login_map)
        .send()
        .await;
    if let Ok(login_response) = login_response {
        match login_response.status() {
            reqwest::StatusCode::OK => {
                if cli.books.is_empty() {
                    let mut i = 1;
                    loop {
                        let package = download_package(client.clone(), i).await;
                        if let Some(package) = package {
                            prepare_package(&package);
                        }
                        i += 1;
                    }
                } else {
                    for book in cli.books {
                        match url::Url::parse(&book) {
                            Ok(url) => match url.path_segments() {
                                Some(segments) => match segments.last() {
                                    Some(id_str) => {
                                        let id = match id_str.parse::<u64>() {
                                            Ok(id) => id,
                                            Err(e) => {
                                                println!(
                                                    "url {} doesn't contain a book id: {:?}",
                                                    &url, &e
                                                );
                                                return;
                                            }
                                        };
                                        let package = download_package(client.clone(), id).await;
                                        if let Some(package) = package {
                                            prepare_package(&package);
                                        } else {
                                            println!("downloading package for url {} failed", &url);
                                        }
                                    }
                                    None => println!(
                                        "url {} doesn't have final segment, is it a book url?",
                                        &book
                                    ),
                                },
                                None => println!(
                                    "url {} doesn't have segments, is it a book url?",
                                    &book
                                ),
                            },
                            Err(e) => println!(
                                "skipping downloading book {} as url is invalid: {:?}",
                                &book, &e
                            ),
                        }
                    }
                }
            }
            _ => {
                println!("Failed to log in")
            }
        }
    }
}
