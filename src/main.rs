use clap::Parser;
use lopdf;
use reqwest::{self, Request};
use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::{fs, io};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use unidecode::unidecode;
use url;

#[derive(Deserialize)]
struct IsDownloadableResponse {
    isDownloadable: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Bookmark {
    title: String,
    #[serde(default)]
    startPage: u32,
    #[serde(default)]
    lessons: Vec<Bookmark>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Part {
    title: String,
}

#[derive(Deserialize, Serialize, Debug, Default, Clone)]
struct Book {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    page_shift: i64,
    #[serde(default)]
    native_downloadable: bool,
    #[serde(default)]
    #[serde(rename = "nothing")]
    title: String,
    #[serde(rename = "title")]
    collection_title: String,
    parts: Vec<Part>,
    #[serde(default)]
    page_urls: Vec<String>,
    #[serde(default)]
    bookmarks: Vec<Bookmark>,
}

#[derive(Debug, Error)]
enum EdukaError {
    #[error("an unknown error occured")]
    Unknown,
    #[error("JSON input was invalid")]
    JSONError(#[from] serde_json::Error),
    #[error("a Reqwest failed")]
    InternetError(#[from] reqwest::Error),
    #[error("the position number returned by eduka for a chapter does not match reality")]
    PositionOffsetError,
    #[error("the data sent by eduka does not match any known technologies")]
    UnexpectedResponse,
    #[error("an error occured when manipulating a pdf")]
    PDFError(#[from] lopdf::Error),
    #[error("an I/O error occured")]
    IOError(#[from] std::io::Error),
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

async fn fill_teaching_tool_metadata(
    client: &reqwest::Client,
    teaching_tool: &mut TeachingTool,
) -> Result<(), EdukaError> {
    let is_downloadable_response: IsDownloadableResponse = client
        .get(&format!(
            "https://klase.eduka.lt/api/authenticated/teaching-tool/is-downloadable/{}",
            &teaching_tool.id
        ))
        .send()
        .await?
        .json()
        .await?;
    let mut book: Book = client
        .get(
            &(String::from("https://klase.eduka.lt/api/authenticated/part/show-by-teaching-tool/")
                + &teaching_tool.id.to_string()),
        )
        .send()
        .await?
        .json()
        .await?;
    book.title = book.collection_title.clone()
        + ": "
        + &book
            .parts
            .get(0)
            .ok_or(EdukaError::UnexpectedResponse)?
            .title;
    book.id = teaching_tool.id;
    book.native_downloadable = is_downloadable_response.isDownloadable;
    teaching_tool.book = book.clone();
    let pages_json: serde_json::Value = serde_json::from_str(
        &client
            .get(
                &(String::from("https://klase.eduka.lt/api/authenticated/teaching-tool/pages/")
                    + &book.id.to_string()),
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
    book.page_shift = pages_json.get("pageShift").unwrap().as_i64().unwrap();
    let bookmarks_array: Vec<Bookmark> = serde_json::from_str(
        &pages_json
            .get("chapters")
            .ok_or(EdukaError::UnexpectedResponse)?
            .to_string(),
    )
    .map_err(|_| EdukaError::UnexpectedResponse)?;
    book.bookmarks = bookmarks_array;
    teaching_tool.book = book.clone();
    Ok(())
}

async fn download_teaching_tool(
    client: &Arc<reqwest::Client>,
    teaching_tool: &TeachingTool,
) -> Result<(), EdukaError> {
    let book = &teaching_tool.book;
    let book_dir = String::from("./") + &book.title + " ;;; " + &book.id.to_string();
    // skip already started to dl books
    if Path::new(&book_dir).is_dir() {
        println!("SKIPPING");
        return Ok(());
    }
    fs::create_dir_all(&book_dir).unwrap();

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
    Ok(())
}

async fn download_package(client: Arc<reqwest::Client>, id: u64) -> Result<Package, EdukaError> {
    let url = reqwest::Url::parse_with_params(
        &(String::from("https://klase.eduka.lt/api/authenticated/teaching-package/")
            + &id.to_string()),
        [("withTeachingTools", "1")],
    )
    .unwrap();
    let mut package: Package = client.get(url).send().await?.json().await?;
    for teaching_tool in &mut package.teaching_tools {
        fill_teaching_tool_metadata(&client, teaching_tool).await?;
    }
    for teaching_tool in &package.teaching_tools {
        download_teaching_tool(&client, teaching_tool).await?;
    }
    Ok(package)
}

fn prepare_teaching_tool(teaching_tool: &TeachingTool) -> Result<(), EdukaError> {
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

    let pdf_path = std::path::Path::new(&book_dir).join(format!("{}.pdf", &teaching_tool.book.id));

    let mut doc = lopdf::Document::load(&pdf_path)?;

    fn add_bookmarks(
        doc: &mut lopdf::Document,
        page_shift: i64,
        bookmarks: &Vec<Bookmark>,
        parent_id: Option<u32>,
    ) -> Result<(), EdukaError> {
        for eduka_bookmark in bookmarks {
            let page_num = ({
                if eduka_bookmark.startPage == 0 {
                    if let Some(child) = eduka_bookmark.lessons.get(0) {
                        child.startPage
                    } else {
                        0
                    }
                } else {
                    eduka_bookmark.startPage
                }
            } as i64
                - page_shift) as u32;
            let page_id = doc
                .get_pages()
                .get(&page_num)
                .ok_or(EdukaError::PositionOffsetError)?
                .to_owned();

            // breaks in table of contents, surely its possible? LOPDF BUG?
            let ascii_title = unidecode(&eduka_bookmark.title);
            let lo_bookmark = lopdf::Bookmark::new(ascii_title, [1.0; 3], 0, page_id);
            let bookmark_id = doc.add_bookmark(lo_bookmark, parent_id);
            add_bookmarks(doc, page_shift, &eduka_bookmark.lessons, Some(bookmark_id))?;
        }
        Ok(())
    }
    add_bookmarks(
        &mut doc,
        teaching_tool.book.page_shift,
        &teaching_tool.book.bookmarks,
        None,
    )?;
    if let Some(n) = doc.build_outline() {
        doc.catalog_mut()?
            .set("Outlines", lopdf::Object::Reference(n));
    }
    doc.save(&pdf_path)?;
    Ok(())
}

fn prepare_package(package: Package) -> Result<(), EdukaError> {
    for teaching_tool in &package.teaching_tools {
        prepare_teaching_tool(teaching_tool)?;
    }
    Ok(())
}

#[derive(Parser)]
struct Cli {
    #[arg(short, long)]
    username: String,
    #[arg(short, long)]
    password: String,
    books: Vec<String>,
    #[arg(long)]
    exploration_start: Option<u64>,
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
                    let mut teaching_tools_to_download = vec![];
                    let mut i = match cli.exploration_start {
                        Some(exploration_start) => exploration_start,
                        None => 0,
                    };
                    loop {
                        let mut teaching_tool = TeachingTool {
                            id: i,
                            book: Default::default(),
                        };
                        println!("trying teaching tool {}", &i);
                        if let Ok(()) =
                            fill_teaching_tool_metadata(&client, &mut teaching_tool).await
                        {
                            let mut input_string = String::new();
                            while !(input_string.trim() == "y"
                                || input_string.trim() == "n"
                                || input_string.trim() == "cancel")
                            {
                                if teaching_tool.book.native_downloadable {
                                    print!("[NATIVE DOWNLOADABLE]");
                                }
                                print!(
                                    "Should {} be downloaded (y/n/cancel): ",
                                    &teaching_tool.book.title
                                );
                                io::stdout().flush();
                                input_string.clear();
                                io::stdin()
                                    .read_line(&mut input_string)
                                    .expect("reading user input failed");
                            }
                            match input_string.trim().as_ref() {
                                "y" => {
                                    teaching_tools_to_download.push(teaching_tool);
                                }
                                "cancel" => {
                                    break;
                                }
                                _ => {}
                            }
                        }
                        i += 1;
                    }
                    for teaching_tool in teaching_tools_to_download {
                        if let Ok(()) = download_teaching_tool(&client, &teaching_tool).await {
                            println!("downloaded {}", &teaching_tool.book.title);
                            if let Ok(()) = prepare_teaching_tool(&teaching_tool) {
                                println!("prepared {}", teaching_tool.book.title);
                            } else {
                                println!("failed to prepare {}", teaching_tool.book.title);
                            }
                        } else {
                            println!("failed to download {}", &teaching_tool.book.title);
                        }
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
                                        match package {
                                            Ok(package) => {
                                                prepare_package(package).unwrap();
                                            }
                                            Err(e) => {
                                                println!(
                                                    "downloading package for url {} failed {}",
                                                    &url, &e
                                                );
                                            }
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
