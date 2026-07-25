use crate::config::ProviderConfig;
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageFormat};
use reqwest::{Client, Proxy, Url};
use serde_json::{Value, json};
use std::{io::Cursor, time::Duration};

pub async fn recognize_text(
    image: &DynamicImage,
    provider: &ProviderConfig,
    proxy: Option<&str>,
) -> Result<String> {
    let key = provider
        .api_key()?
        .ok_or_else(|| anyhow!("供应商「{}」未配置多模态 API Key", provider.display_name()))?;
    let model = provider
        .models
        .first()
        .ok_or_else(|| anyhow!("供应商「{}」未配置模型", provider.display_name()))?;
    let endpoint = normalize_endpoint(&provider.api_base)?;
    let mut builder = Client::builder().timeout(Duration::from_secs(45));
    if let Some(proxy) = proxy.filter(|value| !value.is_empty()) {
        builder = builder.proxy(Proxy::all(proxy).context("多模态代理地址无效")?);
    }

    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .context("编码多模态裁剪图失败")?;
    let data_url = format!("data:image/png;base64,{}", STANDARD.encode(png));
    let request = json!({
        "model": model,
        "temperature": 0.0,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "只识别图片中的文字，保持原文，不翻译、不解释、不添加标点。"
                },
                {
                    "type": "image_url",
                    "image_url": { "url": data_url }
                }
            ]
        }]
    });
    let response = builder
        .build()?
        .post(endpoint)
        .bearer_auth(key)
        .json(&request)
        .send()
        .await
        .context("多模态 OCR 请求失败")?;
    let status = response.status();
    let body = response.text().await.context("读取多模态响应失败")?;
    if !status.is_success() {
        bail!("多模态 OCR 返回 HTTP {status}: {body}");
    }
    let value: Value = serde_json::from_str(&body).context("多模态响应不是有效 JSON")?;
    value["choices"][0]["message"]["content"]
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("多模态 OCR 未返回文字"))
}

fn normalize_endpoint(api_base: &str) -> Result<Url> {
    let mut url = Url::parse(api_base.trim()).context("多模态 API 地址无效")?;
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/chat/completions") {
        url.set_path(&format!("{path}/chat/completions"));
    }
    Ok(url)
}
