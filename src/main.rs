use crate::config::Config;
use crate::exhentai::*;
use crate::telegram::Bot;
use crate::trans::TRANS;

use anyhow::{format_err, Error};
use futures::prelude::*;
use lazy_static::lazy_static;
use log::{debug, error, info};
use reqwest::{Client, Response};
use telegraph_rs::{html_to_node, Page, Telegraph, UploadResult};
use tempfile::NamedTempFile;
use tokio::time::delay_for;
use v_htmlescape::escape;

use std::collections::HashMap;
use std::env;
use std::fs::{create_dir_all, File};
use std::io::Write;
use std::path::Path;
use std::sync::{
    atomic::{AtomicU32, Ordering::SeqCst},
    Arc,
};
use std::time;

mod config;
mod exhentai;
mod telegram;
mod trans;
mod xpath;

lazy_static! {
    static ref CONFIG: Config = Config::new("config.toml").unwrap_or_else(|e| {
        eprintln!("配置文件解析失败:\n{}", e);
        std::process::exit(1);
    });
    static ref DB: sled::Db = sled::open("./db").expect("无法打开数据库");
}

/// 通过 URL 上传图片至 telegraph
async fn upload_by_url(url: &str, path: &str) -> Result<UploadResult, Error> {
    let client = Client::builder()
        .timeout(time::Duration::from_secs(15))
        .build()?;
    // 下载图片
    debug!("下载图片: {}", url);

    let mut tmp = NamedTempFile::new()?;

    let file = if Path::new(path).exists() {
        Path::new(path).to_owned()
    } else {
        let bytes = client.get(url).send().and_then(Response::bytes).await?;

        if CONFIG.exhentai.local_cache {
            File::create(path).and_then(|mut file| file.write_all(bytes.as_ref()))?;
            Path::new(path).to_owned()
        } else {
            tmp.write_all(bytes.as_ref())?;
            tmp.path().to_owned()
        }
    };

    let result = if CONFIG.telegraph.upload {
        debug!("上传图片: {:?}", file);
        Telegraph::upload(&[file])
            .await
            .map_err(|e| format_err!("上传 telegraph 失败: {}", e))?
            .swap_remove(0)
    } else {
        UploadResult { src: "".to_owned() }
    };

    Ok(result)
}

/// 将 tag 转换为可以直接发送至 tg 的文本格式
fn tags_to_string(tags: &HashMap<String, Vec<String>>) -> String {
    tags.iter()
        .map(|(k, v)| {
            let v = v
                .iter()
                .map(|s| {
                    let trans = vec![
                        (" ", "_"),
                        ("_|_", " #"),
                        ("-", "_"),
                        ("/", "_"),
                        ("·", "_"),
                    ];
                    let mut s = TRANS.trans(k, s).to_owned();
                    for (from, to) in trans {
                        s = s.replace(from, to);
                    }
                    format!("#{}", s)
                })
                .collect::<Vec<_>>()
                .join(" ");
            format!("<code>{:>5}</code>: {}", TRANS.trans("rows", k), v)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 将图片地址格式化为 html
fn img_urls_to_html(img_urls: &[String]) -> String {
    img_urls
        .iter()
        .map(|s| format!(r#"<img src="{}">"#, s))
        .collect::<Vec<_>>()
        .join("")
}

/// 从图片页面地址获取图片原始地址
async fn get_img_urls<'a>(gallery: &BasicGalleryInfo<'a>, img_pages: &[String]) -> Vec<String> {
    let img_cnt = img_pages.len();
    let idx = Arc::new(AtomicU32::new(0));
    let data_path = format!("{}/{}", &CONFIG.exhentai.cache_path, &gallery.title);

    if CONFIG.exhentai.local_cache {
        create_dir_all(data_path).unwrap();
    }

    let update_progress = || {
        let now = idx.load(SeqCst);
        idx.store(now + 1, SeqCst);
        info!("第 {} / {} 张图片", now + 1, img_cnt);
    };

    let get_image_url = |i: usize, url: String| async move {
        let path = format!("{}/{}/{}", &CONFIG.exhentai.cache_path, &gallery.title, i);
        match DB.get(&url) {
            Ok(Some(v)) => {
                debug!("找到缓存!");
                Ok(String::from_utf8(v.to_vec()).expect("无法转为 UTF-8"))
            }
            _ => gallery
                .get_image_url(&url)
                .and_then(|img_url| async move { upload_by_url(&img_url, &path).await })
                .await
                .map(|result| result.src),
        }
    };

    let f = img_pages
        .iter()
        .enumerate()
        .map(|(i, url)| {
            async move {
                update_progress();
                // 最多重试五次
                for _ in 0..5i32 {
                    let img_url = get_image_url(i, url.to_owned()).await;
                    match img_url {
                        Ok(v) => {
                            DB.insert(url, v.as_bytes()).expect("插入图片 URL 失败");
                            return Some(v);
                        }
                        Err(e) => {
                            error!("获取图片地址失败: {}", e);
                            delay_for(time::Duration::from_secs(10)).await;
                        }
                    }
                }
                None
            }
        })
        .collect::<Vec<_>>();

    let ret = futures::stream::iter(f)
        .buffered(CONFIG.threads_num)
        .filter_map(|x| async move { x })
        .collect::<Vec<_>>()
        .await;

    DB.flush_async().await.expect("无法写入数据库");
    ret
}

struct ExLoli {
    config: Config,
    bot: Bot,
    exhentai: ExHentai,
    telegraph: Telegraph,
}

impl ExLoli {
    async fn new() -> Result<Self, Error> {
        let config =
            Config::new("config.toml").map_err(|e| format_err!("配置文件解析失败:\n{}", e))?;
        let bot = config.init_telegram();
        let exhentai = config.init_exhentai().await?;
        let telegraph = config.init_telegraph().await?;
        Ok(ExLoli {
            config,
            bot,
            exhentai,
            telegraph,
        })
    }

    async fn scan_and_upload(&self) -> Result<(), Error> {
        // 筛选最新本子
        let galleries = self
            .exhentai
            .search_n_pages(&self.config.exhentai.keyword, CONFIG.exhentai.max_pages)
            .await?;

        // 从后往前爬, 防止半路失败导致进度记录错误
        for gallery in galleries.into_iter().rev() {
            if DB.contains_key(gallery.url.as_bytes())? {
                continue;
            }
            self.upload_gallery_to_telegram(&gallery).await?;
        }

        Ok(())
    }

    async fn upload_gallery_by_url(&self, url: &str) -> Result<(), Error> {
        let gallery = self.exhentai.get_gallery_by_url(url).await?;
        self.upload_gallery_to_telegram(&gallery).await
    }

    fn cap_img_pages<'a>(&self, img_pages: &'a [String]) -> &'a [String] {
        let actual_img_cnt = img_pages.len();
        let allow_img_cnt = self.config.exhentai.max_img_cnt;
        let final_img_cnt = std::cmp::min(actual_img_cnt, allow_img_cnt);
        info!("保留图片数量: {}", final_img_cnt);
        &img_pages[..final_img_cnt]
    }

    async fn upload_gallery_to_telegram<'a>(
        &'a self,
        gallery: &BasicGalleryInfo<'a>,
    ) -> Result<(), Error> {
        info!("画廊名称: {}", gallery.title);
        info!("画廊地址: {}", gallery.url);

        let gallery_info = gallery.get_full_info().await?;

        let img_pages = self.cap_img_pages(&gallery_info.img_pages);
        if let Some(len) = DB.get(gallery.title.as_bytes())? {
            let bytes = [len[0], len[1], len[2], len[3], len[4], len[5], len[6], len[7]];
            let len = usize::from_le_bytes(bytes);
            if len >= self.config.exhentai.max_img_cnt {
                return Ok(())
            }
        }

        let img_urls = get_img_urls(gallery, img_pages).await;

        if !self.config.telegraph.upload {
            return Ok(());
        }

        let overflow = img_pages.len() != gallery_info.img_pages.len();
        let page = self
            .publish_to_telegraph(&gallery_info, &img_urls, overflow)
            .await?;

        info!("文章地址: {}", page.url);
        // 由于画廊会更新，这个地址不能用于判断是否重复上传了，仅用于后续查询使用
        DB.insert(gallery.url.as_bytes(), page.url.as_bytes())?;
        DB.insert(gallery.title.as_bytes(), &img_pages.len().to_le_bytes())?;

        self.publish_to_telegram(&gallery_info, &page.url).await
    }

    async fn publish_to_telegraph(
        &self,
        gallery: &FullGalleryInfo,
        img_urls: &[String],
        overflow: bool,
    ) -> Result<Page, Error> {
        info!("上传到 Telegraph");
        let mut content = img_urls_to_html(&img_urls);
        if overflow {
            content.push_str(r#"<p>图片数量过多, 只显示部分. 完整版请前往 E 站观看.</p>"#);
        }
        self.telegraph
            .create_page(&gallery.title, &html_to_node(&content), false)
            .await
            .map_err(|e| e.into())
    }

    async fn publish_to_telegram(
        &self,
        gallery: &FullGalleryInfo,
        article: &str,
    ) -> Result<(), Error> {
        info!("发布到 Telegram 频道");
        let tags = tags_to_string(&gallery.tags);
        let text = format!(
            "{0}\n<code>   预览</code>：<a href=\"{1}\">{2}</a>\n<code>原始地址</code>：<a href=\"{3}\">{3}</a>",
            tags,
            article,
            escape(&gallery.title),
            gallery.url,
        );
        self.bot
            .send_message(&self.config.telegram.channel_id, &text)
            .await?;
        Ok(())
    }
}

fn dump_db() -> Result<(), Error> {
    let mut map = HashMap::new();
    for i in DB.iter() {
        let (k, v) = i?;
        let k = String::from_utf8(k.to_vec()).unwrap_or_default();
        let v = String::from_utf8(v.to_vec()).unwrap_or_default();
        map.insert(k, v);
    }
    let string = serde_json::to_string_pretty(&map)?;
    println!("{}", string);
    Ok(())
}

fn load_db(file: &str) -> Result<(), Error> {
    let file = File::open(file)?;
    let map: HashMap<String, String> = serde_json::from_reader(file)?;
    for (k, v) in map.iter() {
        DB.insert(k.as_bytes(), v.as_bytes())?;
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let exloli = ExLoli::new().await.unwrap_or_else(|e| {
        eprintln!("{}", e);
        std::process::exit(1);
    });

    let args = env::args().collect::<Vec<_>>();
    env::set_var("RUST_LOG", format!("exloli={}", exloli.config.log_level));
    env_logger::init();
    // color_backtrace::install();

    for _ in 0..3i32 {
        let result = match (args.len(), args.get(1).map(String::as_str).unwrap_or("")) {
            (3, "upload") => exloli.upload_gallery_by_url(&args[2]).await,
            (2, "dump") => dump_db(),
            (3, "load") => load_db(&args[2]),
            _ => exloli.scan_and_upload().await,
        };

        let b = DB.flush().expect("数据库写入失败");
        debug!("数据库写入 {} 字节", b);

        match result {
            Ok(()) => {
                info!("任务完成!");
                return;
            }
            Err(e) => {
                error!("任务出错: {}", e);
                delay_for(time::Duration::from_secs(60)).await;
            }
        }
    }
}
