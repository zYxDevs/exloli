use crate::database::Gallery;
use crate::exhentai::*;
use crate::utils::*;
use crate::{BOT, CONFIG, DB};
use anyhow::Result;
use chrono::{Duration, Timelike, Utc};
use telegraph_rs::{html_to_node, Page, Telegraph};
use teloxide::prelude::*;
use teloxide::types::ChatOrInlineMessage;
use teloxide::ApiErrorKind;
use v_htmlescape::escape;

pub struct ExLoli {
    exhentai: ExHentai,
    telegraph: Telegraph,
}

impl ExLoli {
    pub async fn new() -> Result<Self> {
        let exhentai = CONFIG.init_exhentai().await?;
        let telegraph = CONFIG.init_telegraph().await?;
        Ok(ExLoli {
            exhentai,
            telegraph,
        })
    }

    /// 根据配置文件自动扫描并上传本子
    pub async fn scan_and_upload(&self) -> Result<()> {
        // 筛选最新本子
        let page_limit = CONFIG.exhentai.max_pages;
        let galleries = self.exhentai.search_n_pages(page_limit).await?;

        // 从后往前爬, 保持顺序
        for gallery in galleries.into_iter().rev() {
            info!("检测中：{}", gallery.url);
            match DB.query_gallery_by_url(&gallery.url) {
                Ok(g) => {
                    self.update_gallery(g, gallery).await.log_on_error().await;
                }
                _ => {
                    self.upload_gallery(gallery).await.log_on_error().await;
                }
            }
        }
        Ok(())
    }

    /// 更新画廊信息
    async fn update_gallery<'a>(&'a self, g: Gallery, gallery: BasicGalleryInfo<'a>) -> Result<()> {
        let now = Utc::now();
        let duration = Utc::today().naive_utc() - g.publish_date;
        // 已删除画廊不更新
        // 7 天前发的本子不更新
        // 两天前的本子，逢 4 小时更新
        if (g.score - -1.0).abs() < f32::EPSILON
            || duration.num_days() > 7
            || (duration.num_days() > 2 && now.hour() % 4 != 0)
        {
            return Ok(());
        }

        // 检测是否需要更新 tag
        // TODO: 将 tags 塞到 BasicInfo 里
        let info = gallery.into_full_info().await?;
        let new_tags = serde_json::to_string(&info.tags)?;
        if new_tags != g.tags {
            info!("tag 有更新，同步中...");
            info!("画廊名称: {}", info.title);
            info!("画廊地址: {}", info.url);
            self.update_gallery_info(g, &info).await?;
        }
        Ok(())
    }

    /// 上传指定 URL 的画廊
    pub async fn upload_gallery_by_url(&self, url: &str, update: bool) -> Result<()> {
        let mut gallery = self.exhentai.get_gallery_by_url(url).await?;
        gallery.limit = false;
        gallery.update = update;
        self.upload_gallery(gallery).await
    }

    /// 将画廊上传到 telegram
    async fn upload_gallery<'a>(&'a self, basic_info: BasicGalleryInfo<'a>) -> Result<()> {
        info!("上传中，画廊名称: {}", basic_info.title);

        let mut gallery = basic_info.clone().into_full_info().await?;

        // 判断是否上传过并且不需要更新
        let (update_in_place, old_gallery) = if basic_info.update {
            (true, Some(DB.query_gallery_by_url(&basic_info.url)?))
        } else {
            match DB.query_gallery_by_title(&gallery.title) {
                Ok(g) => {
                    // 上传量已经达到限制的，不做更新
                    if g.upload_images as usize == CONFIG.exhentai.max_img_cnt && gallery.limit {
                        return Err(anyhow::anyhow!("AlreadyUpload"));
                    }
                    // FIXME: 如果只是修改而不是增加了图片的画廊会被认为重复而不进行更新
                    // 如果已上传所有图片，则不进行更新
                    if gallery.img_pages.len() == g.upload_images as usize {
                        return Err(anyhow::anyhow!(
                            "该画廊已存在：{}",
                            get_message_url(g.message_id)
                        ));
                    }
                    // FIXME: 当前判断方法可能会误判，而且修改最大图片数量以后会失效
                    // 如果曾经更新过完整版，则继续上传完整版
                    if g.upload_images as usize > CONFIG.exhentai.max_img_cnt {
                        gallery.limit = false;
                    }
                    // 七天以内上传过的，不重复发，在原消息的基础上更新
                    if g.publish_date + Duration::days(7) > Utc::today().naive_utc() {
                        info!("找到历史上传：{}", g.message_id);
                        (true, Some(g))
                    } else {
                        info!("历史上传已过期：{}", g.message_id);
                        (false, Some(g))
                    }
                }
                _ => (false, None),
            }
        };

        let img_urls = gallery.upload_images_to_telegraph().await?;

        // 上传到 telegraph
        let title = gallery.title_jp.as_ref().unwrap_or(&gallery.title);
        let content = Self::get_article_string(
            &img_urls,
            gallery.img_pages.len(),
            if !update_in_place {
                old_gallery.as_ref().map(|v| v.upload_images as usize)
            } else {
                None
            },
        );
        let page = self.publish_to_telegraph(title, &content).await?;
        info!("文章地址: {}", page.url);

        match (update_in_place, old_gallery) {
            // 需要原地更新的旧本子，直接编辑原来的消息
            (true, Some(g)) => self.update_message(&g, &gallery, &page.url).await,
            // 不需要原地更新的旧本子，发布新消息
            (false, Some(g)) => {
                let message = self.publish_to_telegram(&gallery, &page.url).await?;
                DB.update_gallery(&g, &gallery, &page.url, message.id)
            }
            // 新本子，直接发布
            (_, None) => {
                let message = self.publish_to_telegram(&gallery, &page.url).await?;
                DB.insert_gallery(&gallery, page.url, message.id)
            }
        }
    }

    /// 更新 tag
    pub async fn update_tag(&self, old_gallery: &Gallery) -> Result<()> {
        let url = old_gallery.get_url();
        let new_gallery = self.exhentai.get_gallery_by_url(&url).await?.into_full_info().await?;

        // 更新 telegraph
        let path = old_gallery.telegraph.split('/').last().unwrap();
        let old_page = Telegraph::get_page(&path, true).await?;
        let new_page = self.telegraph.edit_page(
            &old_page.path,
            new_gallery.title_jp.as_ref().unwrap_or(&new_gallery.title),
            &serde_json::to_string(&old_page.content)?,
            false,
        ).await?;

        self.update_message(&old_gallery, &new_gallery, &new_page.url).await?;
        DB.update_gallery(&old_gallery, &new_gallery, &new_page.url, old_gallery.message_id)
    }

    /// 更新旧消息并同时更新数据库
    async fn update_message<'a>(
        &self,
        old_gallery: &Gallery,
        gallery: &FullGalleryInfo<'a>,
        article: &str,
    ) -> Result<()> {
        info!("更新 Telegram 频道消息");
        let message = ChatOrInlineMessage::Chat {
            chat_id: CONFIG.telegram.channel_id.clone(),
            message_id: old_gallery.message_id,
        };
        let text = Self::get_message_string(gallery, article);
        match BOT.edit_message_text(message, &text).send().await {
            Err(RequestError::ApiError {
                kind: ApiErrorKind::Known(e),
                ..
            }) => {
                error!("{:?}", e);
                DB.update_gallery(
                    &old_gallery,
                    &gallery,
                    article,
                    old_gallery.message_id,
                )
            }
            Ok(mes) => DB.update_gallery(&old_gallery, &gallery, article, mes.id),
            Err(e) => Err(e.into()),
        }
    }

    /// 将画廊内容上传至 telegraph
    async fn publish_to_telegraph<'a>(&self, title: &str, content: &str) -> Result<Page> {
        info!("上传到 Telegraph");
        self.telegraph
            .create_page(title, &html_to_node(&content), false)
            .await
            .map_err(|e| e.into())
    }

    /// 将画廊内容上传至 telegraph
    async fn publish_to_telegram<'a>(
        &self,
        gallery: &FullGalleryInfo<'a>,
        article: &str,
    ) -> Result<Message> {
        info!("发布到 Telegram 频道");
        let text = Self::get_message_string(gallery, article);
        Ok(BOT
            .send_message(CONFIG.telegram.channel_id.clone(), &text)
            .send()
            .await?)
    }

    async fn update_gallery_info<'a>(&self, og: Gallery, ng: &FullGalleryInfo<'a>) -> Result<()> {
        self.update_message(&og, &ng, &og.telegraph).await?;
        Ok(())
    }

    /// 生成用于发送消息的字符串，默认使用日文标题，在有日文标题的情况下会在消息中附上英文标题
    fn get_message_string<'a>(gallery: &FullGalleryInfo<'a>, article: &str) -> String {
        let mut tags = tags_to_string(&gallery.tags);
        tags.push_str(&format!(
            "\n<code>  预览</code>: <a href=\"{}\">{}</a>",
            article,
            escape(&gallery.title)
        ));
        tags.push_str(&format!("\n<code>原始地址</code>: {}", gallery.url));
        tags
    }

    /// 生成 telegraph 文章内容
    fn get_article_string(
        image_urls: &[String],
        total_image: usize,
        last_uploaded: Option<usize>,
    ) -> String {
        let mut content = img_urls_to_html(&image_urls);
        if last_uploaded.is_some() || image_urls.len() != total_image {
            content.push_str("<p>");
            content.push_str(&format!("已上传 {}/{}", image_urls.len(), total_image,));
            if let Some(v) = last_uploaded {
                content.push_str(&format!("，上次上传到 {}", v));
            }
            if image_urls.len() != total_image {
                content.push_str("，完整版请前往 E 站观看");
            }
            content.push_str("</p>");
        }
        content
    }
}
