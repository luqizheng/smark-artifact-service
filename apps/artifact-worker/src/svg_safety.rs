//! 产物静态化守卫（ART-03A A3A-07；上游实现：ai-pm V0-6 安全基座，原样移植）。
//!
//! 安全范式从「隔离不可信代码」改为「**不执行不可信代码**」——产物只以静态 SVG
//! 形式存在，浏览器不执行其中任何代码。
//!
//! 本模块是那道保证的执行点：**渲染产物入库前**必须通过静态化断言。
//!
//! ## 为什么不能朴素地全文搜 `on\w+=`
//!
//! 实测（SeeCode v0.6.3）确认：产物文本节点里会出现**已转义**的注入串，
//! 例如 `&lt;img src=x onerror=1&gt;` ——它在 SVG 源码里字面包含 `onerror=`，
//! 但它位于**文本内容**而非标签属性，浏览器不会把它当事件处理器。
//! 全文正则会把它误判为违规（假阳性），而漏判真违规则更糟。
//!
//! 因此本实现只检查**标签内部**（`<tag ...>` 区段）的属性，
//! 文本节点内容不参与事件属性判定——与浏览器的实际解析行为一致。
//!
//! ## 允许项
//!
//! - `data:` URI（内嵌图片与 base64 字体）——不产生网络请求；
//! - 片段引用（`href="#id"`）——同文档内部引用；
//! - `<style>` 块与 `@font-face`（其中 `url(data:font/woff2;base64,…)`）。

use std::collections::BTreeSet;

/// sanitizer 策略版本（写入 Output manifest，A3A-07 §6.2 可审计要求）。
/// 规则集变更（允许/禁止清单调整）时递增。
pub const SANITIZER_POLICY_VERSION: &str = "svg-allowlist-1";

/// 一条静态化违规。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SafetyViolation {
    /// 违规码（`S_*` 前缀）。
    pub code: String,
    /// 定位（标签序号或上下文片段）。
    pub at: String,
    /// 说明。
    pub message: String,
}

/// 静态化检查报告。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SvgSafetyReport {
    /// 违规列表。
    pub violations: Vec<SafetyViolation>,
}

impl SvgSafetyReport {
    /// 是否通过（无任何违规）。
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }

    /// 违规码集合（便于聚合统计）。
    pub fn codes(&self) -> BTreeSet<&str> {
        self.violations.iter().map(|v| v.code.as_str()).collect()
    }

    /// 人类可读摘要（用于日志与审计）。
    pub fn summary(&self) -> String {
        if self.is_clean() {
            return "产物静态化检查通过".to_string();
        }
        let codes: BTreeSet<&str> = self.codes();
        format!(
            "产物静态化检查失败（{} 项）：{}",
            self.violations.len(),
            codes.into_iter().collect::<Vec<_>>().join(", ")
        )
    }
}

/// 禁止出现的元素（标签名 → 违规码）。
const FORBIDDEN_ELEMENTS: &[(&str, &str, &str)] = &[
    ("script", "S_SVG_SCRIPT", "产物不得包含 script 元素"),
    (
        "foreignobject",
        "S_SVG_FOREIGN_OBJECT",
        "产物不得包含 foreignObject（可嵌入任意 HTML）",
    ),
    ("iframe", "S_SVG_IFRAME", "产物不得包含 iframe"),
    ("object", "S_SVG_OBJECT", "产物不得包含 object"),
    ("embed", "S_SVG_EMBED", "产物不得包含 embed"),
    (
        "audio",
        "S_SVG_MEDIA",
        "产物不得包含媒体元素（可发起网络请求）",
    ),
    (
        "video",
        "S_SVG_MEDIA",
        "产物不得包含媒体元素（可发起网络请求）",
    ),
    ("handler", "S_SVG_HANDLER", "产物不得包含 handler 元素"),
    (
        "set",
        "S_SVG_ANIMATE",
        "产物不得包含 set/animate 元素（可改写属性）",
    ),
    (
        "animate",
        "S_SVG_ANIMATE",
        "产物不得包含 set/animate 元素（可改写属性）",
    ),
    (
        "animatetransform",
        "S_SVG_ANIMATE",
        "产物不得包含 set/animate 元素（可改写属性）",
    ),
];

/// 引用型元素：`use` / `image` 的**引用目标**决定是否安全。
///
/// - `image` 指向 `data:` URI（内嵌位图）→ 允许（不产生网络请求）；
/// - `use` 指向同文档片段（`#id`）→ 允许；
/// - 其余（外部 URL、协议相对 URL）→ 违规。
const REFERENCE_ELEMENTS: &[&str] = &["use", "image"];

/// 外部协议（属性值以此开头即为外链请求）。
const EXTERNAL_SCHEMES: &[&str] = &["http://", "https://", "ftp://", "ws://", "wss://", "//"];

/// 检查 SVG 产物是否可安全地以静态形式呈现。
pub fn inspect_svg(svg: &str) -> SvgSafetyReport {
    let mut report = SvgSafetyReport::default();

    check_doctype(svg, &mut report);
    check_tags(svg, &mut report);
    check_style_blocks(svg, &mut report);

    report
}

/// 扫描 `<style>…</style>` 内容块。
///
/// CSS 的 `url()` 与 `@import` 出现在**元素内容**里而非标签属性中——
/// 只查标签属性会漏掉「样式表引用外部资源」这条路径（实测产物的 `@font-face`
/// 正是这个形态：字体以 base64 内嵌，但外链字体同样是真实风险）。
fn check_style_blocks(svg: &str, report: &mut SvgSafetyReport) {
    let lower = svg.to_ascii_lowercase();
    let mut cursor = 0usize;
    let mut index = 0usize;
    while let Some(start) = lower[cursor..].find("<style") {
        index += 1;
        let content_start = cursor + start + "<style".len();
        let Some(end) = lower[content_start..].find("</style>") else {
            break;
        };
        let content = &svg[content_start..content_start + end];
        cursor = content_start + end;

        for literal in find_css_urls(content) {
            if is_external(&literal) {
                push(
                    report,
                    "S_SVG_EXTERNAL_REFERENCE",
                    format!("style[{index}]"),
                    format!("样式表含外部引用 {literal}"),
                );
            }
        }
        if content.to_ascii_lowercase().contains("@import") {
            push(
                report,
                "S_SVG_EXTERNAL_REFERENCE",
                format!("style[{index}]"),
                "样式表不得使用 @import（可拉取外部样式）".to_string(),
            );
        }
    }
}

/// 提取 CSS 内容中的 `url(...)` 字面量。
fn find_css_urls(css: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut search = css;
    while let Some(pos) = search.find("url(") {
        let rest = &search[pos + 4..];
        let end = rest.find(')').unwrap_or(rest.len());
        let literal = rest[..end].trim().trim_matches('"').trim_matches('\'');
        urls.push(literal.to_string());
        search = &rest[end.max(1)..];
    }
    urls
}

/// DOCTYPE / ENTITY：实体扩展攻击（XXE）与外部 DTD 引用。
fn check_doctype(svg: &str, report: &mut SvgSafetyReport) {
    let lower = svg.to_ascii_lowercase();
    if lower.contains("<!doctype") {
        push(
            report,
            "S_SVG_DOCTYPE",
            "document",
            "产物不得包含 DOCTYPE 声明".to_string(),
        );
    }
    if lower.contains("<!entity") {
        push(
            report,
            "S_SVG_ENTITY",
            "document",
            "产物不得包含实体声明（XXE）".to_string(),
        );
    }
}

/// 逐标签检查：元素白名单、事件属性、外部引用。
fn check_tags(svg: &str, report: &mut SvgSafetyReport) {
    let mut index = 0usize;
    for tag in extract_tags(svg) {
        index += 1;
        let name = tag_name(&tag);
        let lower_name = name.to_ascii_lowercase();

        // 禁止元素
        for (forbidden, code, message) in FORBIDDEN_ELEMENTS {
            if lower_name == *forbidden {
                push(report, code, format!("tag[{index}]"), message.to_string());
                break;
            }
        }

        // 引用型元素：`use` / `image` 的引用目标必须安全
        if REFERENCE_ELEMENTS.contains(&lower_name.as_str()) {
            let target =
                attribute_value(&tag, "href").or_else(|| attribute_value(&tag, "xlink:href"));
            match target {
                Some(value) if !is_external(&value) && !value.trim().is_empty() => {
                    // data: URI / 片段引用 —— 安全
                }
                _ => {
                    push(
                        report,
                        "S_SVG_EXTERNAL_REFERENCE",
                        format!("tag[{index}]"),
                        format!(
                            "标签 {lower_name} 的引用目标不安全：{}",
                            target.unwrap_or_else(|| "（缺失）".to_string())
                        ),
                    );
                }
            }
        }

        // 事件属性（只查标签内部——文本节点里的转义串不算）
        if let Some(attr) = find_event_attribute(&tag) {
            push(
                report,
                "S_SVG_EVENT_ATTRIBUTE",
                format!("tag[{index}]"),
                format!("标签 {lower_name} 含事件属性 {attr}"),
            );
        }

        // 外部引用
        if let Some(target) = find_external_reference(&tag) {
            push(
                report,
                "S_SVG_EXTERNAL_REFERENCE",
                format!("tag[{index}]"),
                format!("标签 {lower_name} 含外部引用 {target}"),
            );
        }
    }
}

/// 提取所有标签（`<tag ...>` 区段）。
///
/// 属性值中的未转义 `>` 会提前截断标签——这是**保守**行为：可能漏检该标签
/// 后面的属性，但不会误报。转义后的 `&gt;` 不受影响。
fn extract_tags(svg: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let bytes = svg.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // 注释与声明
            if svg[i..].starts_with("<!--") {
                match svg[i..].find("-->") {
                    Some(end) => {
                        i += end + 3;
                        continue;
                    }
                    None => break,
                }
            }
            match svg[i..].find('>') {
                Some(end) => {
                    let tag = &svg[i + 1..i + end];
                    if !tag.starts_with('!') && !tag.starts_with('?') {
                        tags.push(tag.to_string());
                    }
                    i += end + 1;
                }
                None => break,
            }
        } else {
            i += 1;
        }
    }
    tags
}

/// 取标签名（`<g class="x">` → `g`）。
fn tag_name(tag: &str) -> &str {
    let trimmed = tag.trim_start();
    trimmed
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("")
}

/// 找标签内的事件属性（`onclick=` / `onerror =` …）。
///
/// **必须是真正的属性名解析**：真实产物里出现过
/// `aria-label="&quot;&gt;&lt;img src=x onerror=alert(1)&gt;"`——`onerror=`
/// 出现在**属性值内部**，不是属性名。朴素全文/子串匹配会把它误判为事件属性
/// （实测夹具 `tests/fixtures/seecode_real_artifact.svg` 即含此形态）。
fn find_event_attribute(tag: &str) -> Option<String> {
    attribute_names(tag)
        .into_iter()
        .find(|name| name.len() > 2 && name.starts_with("on"))
}

/// 解析标签的属性名列表（按顺序）。
fn attribute_names(tag: &str) -> Vec<String> {
    let mut names = Vec::new();
    let chars: Vec<char> = tag.chars().collect();
    let mut i = 0usize;
    // 跳过标签名
    while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '/' {
        i += 1;
    }
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '=' && chars[i] != '/' {
            i += 1;
        }
        let name: String = chars[start..i].iter().collect();
        if name.is_empty() {
            i += 1;
            continue;
        }
        // 跳过值
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && chars[i] == '=' {
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
                let quote = chars[i];
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    i += 1;
                }
                if i < chars.len() {
                    i += 1;
                }
            } else {
                while i < chars.len() && !chars[i].is_whitespace() {
                    i += 1;
                }
            }
        }
        names.push(name.to_ascii_lowercase());
    }
    names
}

/// 取标签中某属性的值（不解引号）。
fn attribute_value(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut search = lower.as_str();
    loop {
        let pos = search.find(attr)?;
        let before_ok = pos == 0
            || search[..pos]
                .chars()
                .next_back()
                .is_none_or(|c| c.is_whitespace() || c == '"' || c == '\'' || c == ':');
        if !before_ok {
            search = &search[pos + attr.len()..];
            continue;
        }
        let value_start = pos + attr.len();
        let rest = tag[value_start..].trim_start();
        // 跳过赋值号（`href="…"`）
        let rest = rest.strip_prefix('=')?.trim_start();
        let rest = rest.strip_prefix('"').or_else(|| rest.strip_prefix('\''))?;
        let end = rest
            .find(|c: char| c == '"' || c == '\'' || c.is_whitespace())
            .unwrap_or(rest.len());
        return Some(rest[..end].to_string());
    }
}

/// 找标签内的外部引用（`href="http…"` / `url(http…)` / `src="//host"` …）。
fn find_external_reference(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    // 属性值里的外部 URL
    for attr in ["href=", "src=", "xlink:href=", "data=", "poster="] {
        let mut search = lower.as_str();
        while let Some(pos) = search.find(attr) {
            let value_start = pos + attr.len();
            let value = search[value_start..].trim_start();
            let value = value
                .strip_prefix('"')
                .or_else(|| value.strip_prefix('\''))
                .unwrap_or(value);
            let end = value
                .find(|c: char| c == '"' || c == '\'' || c.is_whitespace() || c == '>')
                .unwrap_or(value.len());
            let literal = &value[..end];
            if is_external(literal) {
                return Some(literal.to_string());
            }
            search = &search[value_start + end.max(1)..];
        }
    }
    // style 属性里的 url(...)
    if lower.contains("url(") {
        let mut search = lower.as_str();
        while let Some(pos) = search.find("url(") {
            let rest = &search[pos + 4..];
            let end = rest.find(')').unwrap_or(rest.len());
            let literal = rest[..end].trim().trim_matches('"').trim_matches('\'');
            if is_external(literal) {
                return Some(literal.to_string());
            }
            search = &rest[end.max(1)..];
        }
    }
    None
}

/// 是否为外部引用（`data:` 与片段引用不算）。
fn is_external(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    if lower.is_empty() || lower.starts_with('#') || lower.starts_with("data:") {
        return false;
    }
    EXTERNAL_SCHEMES
        .iter()
        .any(|scheme| lower.starts_with(scheme))
}

fn push(report: &mut SvgSafetyReport, code: &str, at: impl Into<String>, message: String) {
    report.violations.push(SafetyViolation {
        code: code.to_string(),
        at: at.into(),
        message,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_svg_passes() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
            <title>标题</title>
            <style>@font-face{font-family:x;src:url(data:font/woff2;base64,AAAA)}</style>
            <g class="node"><text x="1" y="2">节点</text></g>
        </svg>"#;
        let report = inspect_svg(svg);
        assert!(report.is_clean(), "应通过：{:?}", report.violations);
    }

    #[test]
    fn script_element_is_rejected() {
        let svg = r#"<svg><script>alert(1)</script><text>x</text></svg>"#;
        let report = inspect_svg(svg);
        assert!(report.codes().contains("S_SVG_SCRIPT"));
    }

    #[test]
    fn foreign_object_is_rejected() {
        let svg = r#"<svg><foreignObject><body>x</body></foreignObject></svg>"#;
        assert!(inspect_svg(svg).codes().contains("S_SVG_FOREIGN_OBJECT"));
    }

    #[test]
    fn event_attribute_in_tag_is_rejected() {
        let svg = r#"<svg><g onclick="alert(1)"><text>x</text></g></svg>"#;
        assert!(inspect_svg(svg).codes().contains("S_SVG_EVENT_ATTRIBUTE"));
    }

    #[test]
    fn escaped_event_text_is_not_a_violation() {
        // 实测事实：转义后的注入串出现在**文本内容**里，字面含 `onerror=`
        // 但浏览器不会把它当事件处理器——不得误报。
        let svg = r#"<svg><text>&lt;img src=x onerror=alert(1)&gt;</text></svg>"#;
        let report = inspect_svg(svg);
        assert!(
            !report.codes().contains("S_SVG_EVENT_ATTRIBUTE"),
            "文本节点中的转义串不得误判：{:?}",
            report.violations
        );
        assert!(report.is_clean());
    }

    #[test]
    fn escaped_script_text_is_not_a_violation() {
        let svg = r#"<svg><text>&lt;script&gt;alert(1)&lt;/script&gt;</text></svg>"#;
        assert!(inspect_svg(svg).is_clean());
    }

    #[test]
    fn external_reference_is_rejected() {
        let svg = r#"<svg><a href="https://example.com/x">link</a></svg>"#;
        assert!(
            inspect_svg(svg)
                .codes()
                .contains("S_SVG_EXTERNAL_REFERENCE")
        );
    }

    #[test]
    fn protocol_relative_reference_is_rejected() {
        let svg = r##"<svg><image href="//cdn.example.com/a.png"/></svg>"##;
        let report = inspect_svg(svg);
        assert!(report.codes().contains("S_SVG_EXTERNAL_REFERENCE"));
    }

    #[test]
    fn reference_element_with_data_uri_is_allowed() {
        // 实测事实：真实 SeeCode 产物含 `<image href="data:image/png;base64,…">`
        // —— 内嵌位图不产生网络请求，必须放行。
        let svg = r##"<svg><image href="data:image/png;base64,AAAA"/></svg>"##;
        assert!(inspect_svg(svg).is_clean());
    }

    #[test]
    fn use_element_with_fragment_is_allowed() {
        let svg = r##"<svg><defs><linearGradient id="g"/></defs><use href="#g"/></svg>"##;
        assert!(inspect_svg(svg).is_clean());
    }

    #[test]
    fn use_element_with_external_target_is_rejected() {
        let svg = r##"<svg><use href="https://cdn.example.com/a.svg#g"/></svg>"##;
        assert!(
            inspect_svg(svg)
                .codes()
                .contains("S_SVG_EXTERNAL_REFERENCE")
        );
    }

    #[test]
    fn external_url_in_style_is_rejected() {
        let svg = r#"<svg><style>.a{fill:url(https://cdn.example.com/a.svg#g)}</style></svg>"#;
        assert!(
            inspect_svg(svg)
                .codes()
                .contains("S_SVG_EXTERNAL_REFERENCE")
        );
    }

    #[test]
    fn data_uri_and_fragment_are_allowed() {
        let svg = r#"<svg><g fill="url(#grad)"><style>.b{background:url(data:image/png;base64,AAA)}</style></g></svg>"#;
        assert!(inspect_svg(svg).is_clean());
    }

    #[test]
    fn doctype_and_entity_are_rejected() {
        assert!(
            inspect_svg(r#"<!DOCTYPE svg><svg/>"#)
                .codes()
                .contains("S_SVG_DOCTYPE")
        );
        assert!(
            inspect_svg(r#"<!ENTITY x "y"><svg/>"#)
                .codes()
                .contains("S_SVG_ENTITY")
        );
    }

    #[test]
    fn comments_are_ignored() {
        let svg = r#"<svg><!-- <script>alert(1)</script> --><text>x</text></svg>"#;
        assert!(inspect_svg(svg).is_clean(), "注释内的内容不算");
    }

    #[test]
    fn summary_lists_codes() {
        let report = inspect_svg(r#"<svg><script>x</script></svg>"#);
        let summary = report.summary();
        assert!(summary.contains("S_SVG_SCRIPT"));
    }
}
