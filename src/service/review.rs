//! 原生段评服务：读取 legado `ruleReview`，抓取段评/回复接口并解析为统一 JSON。
//!
//! Android Legado 当前原生段评的规则引擎仍在客户端，因此阅读服务器需要完成两件事：
//! 保存 `ruleReview` 字段，以及为 Web 阅读器提供同一套规则的服务端执行入口。
//! 本模块只做只读请求，不实现点赞、发布或删除。

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::model::book::Book;
use crate::model::book_chapter::BookChapter;
use crate::model::book_source::BookSource;
use crate::parser::js::JsBridge;
use crate::parser::rule::{push_js_context, RuleVars};
use crate::service::{crawler, search};

/// legado `ruleReview`（字段保持与上游 ReviewRule 同名）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ReviewRule {
    // 旧版/基础段评字段
    pub review_url: Option<String>,
    pub avatar_rule: Option<String>,
    pub content_rule: Option<String>,
    pub post_time_rule: Option<String>,
    pub review_quote_url: Option<String>,
    // 原生段评扩展
    pub review_summary_url: Option<String>,
    pub summary_list_rule: Option<String>,
    pub summary_paragraph_index_rule: Option<String>,
    pub summary_paragraph_data_rule: Option<String>,
    pub summary_count_rule: Option<String>,
    pub review_detail_url: Option<String>,
    pub review_detail_next_page_url: Option<String>,
    pub detail_list_rule: Option<String>,
    pub detail_id_rule: Option<String>,
    pub detail_avatar_rule: Option<String>,
    pub detail_name_rule: Option<String>,
    pub detail_badge_rule: Option<String>,
    pub detail_content_rule: Option<String>,
    pub reply_list_rule: Option<String>,
    pub reply_id_rule: Option<String>,
    pub reply_avatar_rule: Option<String>,
    pub reply_name_rule: Option<String>,
    pub reply_badge_rule: Option<String>,
    pub reply_content_rule: Option<String>,
    pub enabled: bool,
}

impl ReviewRule {
    fn detail_url(&self) -> Option<&str> {
        self.review_detail_url
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| self.review_url.as_deref().filter(|v| !v.trim().is_empty()))
    }

    fn detail_list_rule(&self) -> Option<&str> {
        self.detail_list_rule
            .as_deref()
            .filter(|v| !v.trim().is_empty())
    }

    fn detail_content_rule(&self) -> Option<&str> {
        self.detail_content_rule
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| self.content_rule.as_deref().filter(|v| !v.trim().is_empty()))
    }

    fn reply_content_rule(&self) -> Option<&str> {
        self.reply_content_rule
            .as_deref()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| self.content_rule.as_deref().filter(|v| !v.trim().is_empty()))
    }
}

/// 段评/回复条目。字段名与上游 `ReviewRuleParser.DetailItem` 的 Gson 输出一致。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewItem {
    pub id: Option<String>,
    pub avatar: Option<String>,
    pub name: Option<String>,
    pub reply_to_name: Option<String>,
    pub badges: Vec<String>,
    pub content: Option<String>,
    pub image_url: Option<String>,
    pub audio_url: Option<String>,
    pub time: Option<String>,
    pub like_count: Option<i64>,
    pub reply_count: Option<i64>,
    pub replies: Vec<ReviewItem>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryResult {
    /// paragraphIndex → count；JSON 对象键统一使用字符串，兼容 Android/Gson。
    pub counts: HashMap<String, i64>,
    /// paragraphIndex → paraData（点击段评详情时作为规则上下文）。
    pub keys: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetailPage {
    pub items: Vec<ReviewItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page_url: Option<String>,
    pub has_more: bool,
}

/// detail 页的下一页 URL。键中包含用户、书、章节、段落、源，避免跨上下文复用游标。
static DETAIL_NEXT_URLS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const DETAIL_NEXT_URLS_MAX: usize = 512;

fn detail_cache_key(
    ns: &str,
    source: &BookSource,
    book: &Book,
    chapter: &BookChapter,
    para_index: i64,
    para_data: &str,
    page: u32,
) -> String {
    format!(
        "{ns}\0{}\0{}\0{}\0{para_index}\0{para_data}\0{page}",
        source.book_source_url, book.book_url, chapter.url
    )
}

fn get_next_url(key: &str) -> Option<String> {
    DETAIL_NEXT_URLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()
}

fn put_next_url(key: String, url: String) {
    let mut cache = DETAIL_NEXT_URLS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= DETAIL_NEXT_URLS_MAX && !cache.contains_key(&key) {
        if let Some(old) = cache.keys().next().cloned() {
            cache.remove(&old);
        }
    }
    cache.insert(key, url);
}

/// 获取某章段评统计。没有配置统计规则时返回空对象，而不是错误。
pub async fn get_summary(
    ns: &str,
    source: &BookSource,
    book: &Book,
    chapter: &BookChapter,
) -> Result<SummaryResult> {
    let Some(raw_rule) = source.rule_review.as_ref() else {
        return Ok(SummaryResult::default());
    };
    let rule: ReviewRule = serde_json::from_value(raw_rule.clone())?;
    if !rule.enabled {
        return Ok(SummaryResult::default());
    }
    let (Some(url_rule), Some(list_rule), Some(index_rule), Some(count_rule)) = (
        rule.review_summary_url.as_deref().filter(|v| !v.trim().is_empty()),
        rule.summary_list_rule.as_deref().filter(|v| !v.trim().is_empty()),
        rule.summary_paragraph_index_rule
            .as_deref()
            .filter(|v| !v.trim().is_empty()),
        rule.summary_count_rule
            .as_deref()
            .filter(|v| !v.trim().is_empty()),
    ) else {
        return Ok(SummaryResult::default());
    };

    let response = fetch_review_url(
        ns, source, book, chapter, url_rule, -1, "", None, 1,
    )
    .await?;
    let bridge = JsBridge::from_source(source, ns);
    let mut vars = review_rule_vars(book, chapter, -1, "", None, 1, &response.body);
    let raw_items = extract_elements(list_rule, &response.body, &mut vars, &bridge)?;
    let mut result = SummaryResult::default();
    let data_rule = rule
        .summary_paragraph_data_rule
        .as_deref()
        .filter(|v| !v.trim().is_empty());
    for (index, item) in raw_items.iter().enumerate() {
        let mut item_vars = vars.clone();
        let index_text = extract_field(item, Some(index_rule), &bridge, &mut item_vars);
        let paragraph_index = parse_i64(&index_text).unwrap_or(index as i64 + 1);
        if paragraph_index == 0 {
            continue;
        }
        let count = parse_i64(&extract_field(item, Some(count_rule), &bridge, &mut item_vars))
            .unwrap_or(0);
        if count <= 0 {
            continue;
        }
        let key = data_rule
            .map(|r| extract_field(item, Some(r), &bridge, &mut item_vars))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                if index_text.is_empty() {
                    paragraph_index.to_string()
                } else {
                    index_text
                }
            });
        let key = if key.is_empty() {
            paragraph_index.to_string()
        } else {
            key
        };
        result
            .counts
            .insert(paragraph_index.to_string(), count);
        result.keys.insert(paragraph_index.to_string(), key);
    }
    Ok(result)
}

/// 获取一级段评详情。分页使用 1-based page；顺序翻页时优先使用上一页返回的 nextUrl。
pub async fn get_detail(
    ns: &str,
    source: &BookSource,
    book: &Book,
    chapter: &BookChapter,
    para_index: i64,
    para_data: &str,
    page: u32,
) -> Result<DetailPage> {
    let raw_rule = source
        .rule_review
        .as_ref()
        .ok_or_else(|| anyhow!("段评规则未配置"))?;
    let rule: ReviewRule = serde_json::from_value(raw_rule.clone())?;
    if !rule.enabled {
        return Err(anyhow!("段评规则未启用"));
    }
    let list_rule = rule
        .detail_list_rule()
        .ok_or_else(|| anyhow!("段评详情列表规则未配置"))?;
    let content_rule = rule
        .detail_content_rule()
        .ok_or_else(|| anyhow!("段评详情内容规则未配置"))?;
    let first_url = rule
        .detail_url()
        .ok_or_else(|| anyhow!("段评详情地址未配置"))?;
    let page = page.max(1);
    let cache_key = detail_cache_key(ns, source, book, chapter, para_index, para_data, page);
    let url_rule = if page == 1 {
        first_url.to_string()
    } else {
        // 上一页的 nextUrl 是最可靠的游标；没有缓存时仍尝试重新执行首 URL，
        // 让 native page-only 书源可以直接按 page 构造 URL。
        get_next_url(&cache_key).unwrap_or_else(|| first_url.to_string())
    };
    let response = fetch_review_url(
        ns,
        source,
        book,
        chapter,
        &url_rule,
        para_index,
        para_data,
        None,
        page,
    )
    .await?;
    let bridge = JsBridge::from_source(source, ns);
    let mut vars = review_rule_vars(
        book,
        chapter,
        para_index,
        para_data,
        None,
        page,
        &response.body,
    );
    let raw_items = extract_elements(list_rule, &response.body, &mut vars, &bridge)?;
    let items = parse_items(
        &raw_items,
        &rule,
        &bridge,
        &mut vars,
        &response.url,
        false,
        content_rule,
    );
    if !raw_items.is_empty() && items.is_empty() {
        return Err(anyhow!("段评详情解析为空"));
    }

    let next_page_url = rule
        .review_detail_next_page_url
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .map(|next_rule| {
            let mut next_vars = review_rule_vars(
                book,
                chapter,
                para_index,
                para_data,
                None,
                page + 1,
                &response.body,
            );
            let next = extract_field(&response.body, Some(next_rule), &bridge, &mut next_vars);
            if next.trim().is_empty() {
                None
            } else {
                Some(search::to_absolute(next.trim(), &response.url))
            }
        })
        .flatten()
        .filter(|url| !url.is_empty() && !items.is_empty());
    let has_more = next_page_url.is_some();
    if let Some(next) = next_page_url.as_ref() {
        let next_key = detail_cache_key(
            ns,
            source,
            book,
            chapter,
            para_index,
            para_data,
            page + 1,
        );
        put_next_url(next_key, next.clone());
    }
    Ok(DetailPage {
        items,
        next_page_url,
        has_more,
    })
}

/// 获取某条段评的回复。回复页规则通常直接使用 page 参数，不输出旧 cursor。
pub async fn get_replies(
    ns: &str,
    source: &BookSource,
    book: &Book,
    chapter: &BookChapter,
    para_index: i64,
    para_data: &str,
    review_id: &str,
    page: u32,
) -> Result<DetailPage> {
    let raw_rule = source
        .rule_review
        .as_ref()
        .ok_or_else(|| anyhow!("段评规则未配置"))?;
    let rule: ReviewRule = serde_json::from_value(raw_rule.clone())?;
    if !rule.enabled {
        return Err(anyhow!("段评规则未启用"));
    }
    let url_rule = rule
        .review_quote_url
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow!("段评回复地址未配置"))?;
    let list_rule = rule
        .reply_list_rule
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow!("段评回复列表规则未配置"))?;
    let content_rule = rule
        .reply_content_rule()
        .ok_or_else(|| anyhow!("段评回复内容规则未配置"))?;
    let page = page.max(1);
    let response = fetch_review_url(
        ns,
        source,
        book,
        chapter,
        url_rule,
        para_index,
        para_data,
        Some(review_id),
        page,
    )
    .await?;
    let bridge = JsBridge::from_source(source, ns);
    let mut vars = review_rule_vars(
        book,
        chapter,
        para_index,
        para_data,
        Some(review_id),
        page,
        &response.body,
    );
    let raw_items = extract_elements(list_rule, &response.body, &mut vars, &bridge)?;
    let items = parse_items(
        &raw_items,
        &rule,
        &bridge,
        &mut vars,
        &response.url,
        true,
        content_rule,
    );
    if !raw_items.is_empty() && items.is_empty() {
        return Err(anyhow!("段评回复解析为空"));
    }
    // 原生 reader-dev API 的回复页带 total；优先按 total + URL 中的 limit
    // 判断是否还有下一页，避免最后一页仍显示“加载下一页”并重复请求。没有
    // total 的普通书源则退回“本页满页”启发式。
    let limit = response_page_limit(&response.url);
    let has_more = response_total(&response.body)
        .map(|total| (page as u64).saturating_mul(limit as u64) < total)
        .unwrap_or(items.len() >= limit as usize)
        && !items.is_empty();
    Ok(DetailPage {
        has_more,
        items,
        next_page_url: None,
    })
}

/// 请求段评规则 URL，并执行 URL option（method/body/headers/bodyJs）。
async fn fetch_review_url(
    ns: &str,
    source: &BookSource,
    book: &Book,
    chapter: &BookChapter,
    rule_url: &str,
    para_index: i64,
    para_data: &str,
    review_id: Option<&str>,
    page: u32,
) -> Result<crawler::FetchResponse> {
    let headers = source
        .header
        .as_deref()
        .map(crawler::parse_header)
        .unwrap_or_default();
    let bridge = JsBridge::from_source(source, ns);
    bridge.set_headers(headers.clone());
    let (url_rule_value, suffix) = build_review_url(
        rule_url,
        book,
        chapter,
        para_index,
        para_data,
        review_id,
        page,
        &bridge,
    )?;
    let url = search::to_absolute(url_rule_value.trim(), &chapter.url);
    if url.is_empty() {
        return Err(anyhow!("段评请求地址为空"));
    }
    let mut request_headers = bridge.headers();
    if let Some(extra) = suffix.headers.as_ref() {
        request_headers.extend(extra.clone());
    }
    search::concurrent_rate_acquire(ns, source).await;
    let body = suffix.body.as_deref().map(|value| {
        replace_review_templates(
            value,
            para_index,
            para_data,
            review_id.unwrap_or(""),
            page,
        )
    });
    let method = suffix.method.as_deref().unwrap_or("GET");
    tracing::debug!(
        "段评请求 [{}] {} {} page={} reviewId={}",
        source.book_source_name,
        method,
        url,
        page,
        review_id.unwrap_or("")
    );
    let mut response = if method.eq_ignore_ascii_case("POST") {
        crawler::http_post_retry(
            ns,
            &url,
            &request_headers,
            15,
            body.as_deref(),
            suffix.charset.as_deref(),
            source.proxy_url.as_deref(),
            suffix.retry,
        )
        .await?
    } else {
        crawler::http_get_retry(
            ns,
            &url,
            &request_headers,
            15,
            suffix.charset.as_deref(),
            source.proxy_url.as_deref(),
            suffix.retry,
        )
        .await?
    };
    if let Some(body_js) = suffix.body_js.as_deref() {
        let vars = review_rule_vars(
            book,
            chapter,
            para_index,
            para_data,
            review_id,
            page,
            &response.body,
        );
        response.body = crate::parser::js::eval_js_with_bridge(body_js, &vars, &bridge)?;
    }
    Ok(response)
}

/// URL 规则求值。与 legado AnalyzeUrl 一致，支持标记位于 URL 后的 `url\n@js:`。
fn build_review_url(
    rule_url: &str,
    book: &Book,
    chapter: &BookChapter,
    para_index: i64,
    para_data: &str,
    review_id: Option<&str>,
    page: u32,
    bridge: &JsBridge,
) -> Result<(String, search::UrlSuffix)> {
    let raw = replace_review_templates(
        rule_url,
        para_index,
        para_data,
        review_id.unwrap_or(""),
        page,
    );
    let lower = raw.to_ascii_lowercase();
    let evaluated = if let Some(at) = lower.find("@js:") {
        let prefix = raw[..at].trim();
        let current = if prefix.is_empty() {
            raw.clone()
        } else {
            prefix.to_string()
        };
        let vars = review_rule_vars(
            book,
            chapter,
            para_index,
            para_data,
            review_id,
            page,
            &current,
        );
        crate::parser::js::eval_js_with_bridge(raw[at + 4..].trim(), &vars, bridge)?
    } else if let Some(tag) = lower.find("<js>") {
        let Some(rel_end) = lower[tag + 4..].find("</js>") else {
            let (url_part, suffix) = search::split_url_suffix(&raw);
            return Ok((url_part, suffix));
        };
        let end = tag + 4 + rel_end;
        let prefix = raw[..tag].trim();
        let vars = review_rule_vars(
            book,
            chapter,
            para_index,
            para_data,
            review_id,
            page,
            if prefix.is_empty() { &raw } else { prefix },
        );
        let mut value = crate::parser::js::eval_js_with_bridge(&raw[tag + 4..end], &vars, bridge)?;
        let tail = raw[end + 5..].trim();
        if !tail.is_empty() {
            value = if tail.contains("@result") {
                tail.replace("@result", &value)
            } else {
                format!("{value}{tail}")
            };
        }
        value
    } else if lower.starts_with("js:") {
        let vars = review_rule_vars(
            book,
            chapter,
            para_index,
            para_data,
            review_id,
            page,
            &raw,
        );
        crate::parser::js::eval_js_with_bridge(&raw[3..].trim(), &vars, bridge)?
    } else {
        raw
    };
    // URL option 中的 js 需要以当前 URL 为 result 再执行一次。
    let (url_part, mut suffix) = search::split_url_suffix(&evaluated);
    let final_url = if let Some(js) = suffix.js.take() {
        let vars = review_rule_vars(
            book,
            chapter,
            para_index,
            para_data,
            review_id,
            page,
            url_part.trim(),
        );
        crate::parser::js::eval_js_with_bridge(&js, &vars, bridge)?
    } else {
        url_part
    };
    // URL option 的 js 已消费，method/body/headers 等其余选项继续交给请求层。
    Ok((final_url, suffix))
}

fn review_rule_vars(
    book: &Book,
    chapter: &BookChapter,
    para_index: i64,
    para_data: &str,
    review_id: Option<&str>,
    page: u32,
    result: &str,
) -> RuleVars {
    let mut vars = RuleVars::new();
    vars.insert("result".to_string(), result.to_string());
    vars.insert("baseUrl".to_string(), chapter.url.clone());
    vars.insert("page".to_string(), page.to_string());
    vars.insert("paraIndex".to_string(), para_index.to_string());
    vars.insert("paraData".to_string(), para_data.to_string());
    vars.insert("reviewId".to_string(), review_id.unwrap_or("").to_string());
    vars.insert("key".to_string(), para_data.to_string());
    vars.insert("src".to_string(), result.to_string());

    let mut context = RuleVars::new();
    context.chapter_title = Some(chapter.title.clone());
    context.chapter_url = Some(chapter.url.clone());
    context.book_name = Some(book.name.clone());
    context.insert(
        crate::parser::rule::RK_CHAPTER_INDEX.to_string(),
        chapter.index.to_string(),
    );
    context.insert(
        crate::parser::rule::RK_BOOK_AUTHOR.to_string(),
        book.author.clone(),
    );
    context.insert(
        crate::parser::rule::RK_BOOK_URL.to_string(),
        book.book_url.clone(),
    );
    push_js_context(&mut vars, Some(&context));
    vars
}

fn replace_review_templates(
    value: &str,
    para_index: i64,
    para_data: &str,
    review_id: &str,
    page: u32,
) -> String {
    let mut out = value.to_string();
    for (name, replacement) in [
        ("paraIndex", para_index.to_string()),
        ("paraData", para_data.to_string()),
        ("reviewId", review_id.to_string()),
        ("page", page.to_string()),
    ] {
        out = out
            .replace(&format!("{{{{{name}}}}}"), &replacement)
            .replace(&format!("{{{name}}}"), &replacement);
    }
    out
}

fn extract_elements(
    rule: &str,
    body: &str,
    vars: &mut RuleVars,
    _bridge: &JsBridge,
) -> Result<Vec<String>> {
    let values = crate::parser::rule::apply_with_vars(rule, body, vars);
    Ok(values)
}

fn extract_field(
    context: &str,
    rule: Option<&str>,
    bridge: &JsBridge,
    vars: &mut RuleVars,
) -> String {
    rule.map(|r| search::field_with_bridge_vars(context, Some(r), "", Some(bridge), vars))
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn parse_items(
    raw_items: &[String],
    rule: &ReviewRule,
    bridge: &JsBridge,
    vars: &mut RuleVars,
    base_url: &str,
    is_reply: bool,
    content_rule: &str,
) -> Vec<ReviewItem> {
    raw_items
        .iter()
        .filter_map(|item| {
            parse_item(
                item,
                rule,
                bridge,
                vars,
                base_url,
                is_reply,
                content_rule,
            )
        })
        .collect()
}

fn parse_item(
    item: &str,
    rule: &ReviewRule,
    bridge: &JsBridge,
    vars: &mut RuleVars,
    base_url: &str,
    is_reply: bool,
    content_rule: &str,
) -> Option<ReviewItem> {
    let mut item_vars = vars.clone();
    let id_rule = if is_reply {
        rule.reply_id_rule.as_deref()
    } else {
        rule.detail_id_rule.as_deref()
    };
    let avatar_rule = if is_reply {
        rule.reply_avatar_rule.as_deref()
    } else {
        rule.detail_avatar_rule.as_deref()
    };
    let name_rule = if is_reply {
        rule.reply_name_rule.as_deref()
    } else {
        rule.detail_name_rule.as_deref()
    };
    let badge_rule = if is_reply {
        rule.reply_badge_rule.as_deref()
    } else {
        rule.detail_badge_rule.as_deref()
    };
    let id = nonempty(extract_field(item, id_rule, bridge, &mut item_vars));
    let avatar = nonempty(extract_field(item, avatar_rule, bridge, &mut item_vars))
        .map(|url| search::to_absolute(&url, base_url));
    let name = nonempty(extract_field(item, name_rule, bridge, &mut item_vars));
    let badges = split_badges(extract_field(item, badge_rule, bridge, &mut item_vars));
    let raw_content = extract_field(item, Some(content_rule), bridge, &mut item_vars);
    let protocol = parse_content_protocol(&raw_content);
    let content = protocol
        .as_ref()
        .and_then(|v| v.text.clone())
        .or_else(|| (!raw_content.is_empty() && protocol.is_none()).then_some(raw_content));
    let image_url = protocol
        .as_ref()
        .and_then(|v| v.image.clone())
        .map(|url| search::to_absolute(&url, base_url));
    let audio_url = protocol
        .as_ref()
        .and_then(|v| v.audio.clone())
        .map(|url| search::to_absolute(&url, base_url));
    let time = protocol
        .as_ref()
        .and_then(|v| v.time.clone())
        .or_else(|| nonempty(extract_field(item, rule.post_time_rule.as_deref(), bridge, &mut item_vars)));
    let like_count = protocol.as_ref().and_then(|v| v.like_count);
    let reply_count = protocol.as_ref().and_then(|v| v.reply_count);
    let reply_to_name = protocol.as_ref().and_then(|v| v.reply_to_name.clone());

    if name.is_none() && content.is_none() && image_url.is_none() && audio_url.is_none() {
        return None;
    }
    let nested = if !is_reply
        && rule
            .review_quote_url
            .as_deref()
            .is_none_or(|v| v.trim().is_empty())
    {
        rule.reply_list_rule.as_deref().map(|reply_rule| {
            let mut nested_vars = item_vars.clone();
            crate::parser::rule::apply_with_vars(reply_rule, item, &mut nested_vars)
                .into_iter()
                .filter_map(|reply| {
                    parse_item(
                        &reply,
                        rule,
                        bridge,
                        &mut nested_vars,
                        base_url,
                        true,
                        rule.reply_content_rule().unwrap_or(content_rule),
                    )
                })
                .collect()
        })
    } else {
        None
    }
    .unwrap_or_default();

    Some(ReviewItem {
        id,
        avatar,
        name,
        reply_to_name,
        badges,
        content,
        image_url,
        audio_url,
        time,
        like_count,
        reply_count: if is_reply { None } else { reply_count },
        replies: nested,
    })
}

#[derive(Debug, Clone, Default)]
struct ContentProtocol {
    text: Option<String>,
    reply_to_name: Option<String>,
    image: Option<String>,
    audio: Option<String>,
    time: Option<String>,
    like_count: Option<i64>,
    reply_count: Option<i64>,
}

fn parse_content_protocol(raw: &str) -> Option<ContentProtocol> {
    let value: Value = serde_json::from_str(raw.trim()).ok()?;
    let object = value.as_object()?;
    let result = ContentProtocol {
        text: value_string(object.get("text")),
        reply_to_name: value_string(object.get("replyToName")),
        image: value_string(object.get("img")),
        audio: value_string(object.get("audio")),
        time: value_string(object.get("time")),
        like_count: value_i64(object.get("likeCount")),
        reply_count: value_i64(object.get("replyCount")),
    };
    if result.text.is_none()
        && result.reply_to_name.is_none()
        && result.image.is_none()
        && result.audio.is_none()
        && result.time.is_none()
        && result.like_count.is_none()
        && result.reply_count.is_none()
    {
        None
    } else {
        Some(result)
    }
}

fn value_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => nonempty(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        _ => None,
    }
}

fn value_i64(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|v| v as i64)),
        Some(Value::String(s)) => parse_i64(s),
        _ => None,
    }
}

fn parse_i64(value: &str) -> Option<i64> {
    value
        .trim()
        .parse::<i64>()
        .or_else(|_| value.trim().parse::<f64>().map(|v| v as i64))
        .ok()
}

fn response_total(body: &str) -> Option<u64> {
    let value = serde_json::from_str::<Value>(body).ok()?;
    let data = value.get("data").unwrap_or(&value);
    data.get("total")
        .and_then(|value| value.as_u64().or_else(|| value.as_i64().map(|v| v.max(0) as u64)))
}

fn response_page_limit(url: &str) -> u32 {
    url::Url::parse(url)
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(key, _)| key.eq_ignore_ascii_case("limit"))
                .and_then(|(_, value)| value.parse::<u32>().ok())
        })
        .filter(|limit| *limit > 0)
        .unwrap_or(20)
        .min(50)
}

fn nonempty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn split_badges(value: String) -> Vec<String> {
    let value = value.trim();
    if value.is_empty() {
        return Vec::new();
    }
    if value.starts_with("[") && value.ends_with("]") {
        if let Ok(items) = serde_json::from_str::<Vec<String>>(value) {
            return items.into_iter().filter_map(nonempty).collect();
        }
    }
    value
        .split(['\n', '|', ','])
        .filter_map(|v| nonempty(v.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_protocol() {
        let p = parse_content_protocol(
            r#"{"text":"你好","replyToName":"甲","img":"/a.png","likeCount":3,"replyCount":"4"}"#,
        )
        .unwrap();
        assert_eq!(p.text.as_deref(), Some("你好"));
        assert_eq!(p.reply_to_name.as_deref(), Some("甲"));
        assert_eq!(p.like_count, Some(3));
        assert_eq!(p.reply_count, Some(4));
    }

    #[test]
    fn unknown_json_is_plain_content() {
        assert!(parse_content_protocol(r#"{"other":"value"}"#).is_none());
    }

    #[test]
    fn reply_page_has_more_uses_total_and_limit() {
        assert_eq!(response_total(r#"{"data":{"total":2}}"#), Some(2));
        assert_eq!(response_page_limit("https://a.test/replies?limit=20"), 20);
        assert_eq!(response_page_limit("https://a.test/replies?limit=200"), 50);
    }

    #[test]
    fn review_templates_replace_page_and_ids() {
        assert_eq!(
            replace_review_templates(
                "/r/{{reviewId}}/{{paraIndex}}/{{paraData}}?page={{page}}",
                2,
                "anchor",
                "thread-1",
                3,
            ),
            "/r/thread-1/2/anchor?page=3"
        );
    }
}
