use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolOutput};
use crate::provider::Message;
use crate::safety::{PathIntent, SafetyPolicy};

const MAX_MEDIA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PDF_PAGES: usize = 50;
const MAX_PDF_TEXT_CHARS: usize = 512 * 1024;

pub struct ReadFileTool {
    safety: Arc<SafetyPolicy>,
    multimodal_enabled: bool,
}

impl ReadFileTool {
    #[cfg(test)]
    pub fn new(safety: Arc<SafetyPolicy>) -> Self {
        Self {
            safety,
            multimodal_enabled: false,
        }
    }

    pub fn from_env(safety: Arc<SafetyPolicy>) -> Result<Self> {
        let multimodal_enabled = match env::var("MULTIMODAL_ENABLED") {
            Ok(value) => parse_bool("MULTIMODAL_ENABLED", &value)?,
            Err(env::VarError::NotPresent) => env::var("MODEL_NAME")
                .is_ok_and(|model: String| model.to_ascii_lowercase().contains("vision")),
            Err(error) => return Err(error).context("读取 MULTIMODAL_ENABLED 失败"),
        };
        Ok(Self {
            safety,
            multimodal_enabled,
        })
    }

    #[cfg(test)]
    fn with_multimodal(mut self) -> Self {
        self.multimodal_enabled = true;
        self
    }

    async fn read(&self, args: Value) -> Result<ToolOutput> {
        let args: ReadArgs = serde_json::from_value(args).context("read_file 参数无效")?;
        let path = self
            .safety
            .authorize_path(&args.path, PathIntent::Read)
            .await?;
        match file_kind(&path) {
            FileKind::Image(mime_type) if self.multimodal_enabled => {
                read_image(path, mime_type).await
            }
            FileKind::Image(_) => Ok(ToolOutput::text(format!(
                "当前模型未启用多模态图片输入，无法解析图片：{}。请设置 MULTIMODAL_ENABLED=true 或使用名称含 vision 的 MODEL_NAME。",
                path.display()
            ))),
            FileKind::Pdf => read_pdf(path).await.map(ToolOutput::text),
            FileKind::UnsupportedMedia(kind) => Ok(ToolOutput::text(format!(
                "当前版本不解析{kind}文件：{}。支持的图片格式为 PNG/JPG/JPEG/WebP，PDF 会在本地抽取文字。",
                path.display()
            ))),
            FileKind::Text => tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("读取文件失败: {}", path.display()))
                .map(ToolOutput::text),
        }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "读取文本文件；本地抽取最多 50 页、16MiB 的 PDF；多模态模型启用时可读取 PNG/JPG/JPEG/WebP 图片"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "要读取的文件路径" }
            },
            "required": ["path"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String> {
        self.read(args)
            .await
            .map(|output: ToolOutput| output.content)
    }

    async fn execute_rich(&self, args: Value) -> Result<ToolOutput> {
        self.read(args).await
    }
}

enum FileKind {
    Text,
    Image(&'static str),
    Pdf,
    UnsupportedMedia(&'static str),
}

fn file_kind(path: &Path) -> FileKind {
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => FileKind::Image("image/png"),
        "jpg" | "jpeg" => FileKind::Image("image/jpeg"),
        "webp" => FileKind::Image("image/webp"),
        "pdf" => FileKind::Pdf,
        "gif" | "bmp" | "tiff" | "svg" => FileKind::UnsupportedMedia("图片"),
        _ => FileKind::Text,
    }
}

async fn read_image(path: PathBuf, mime_type: &'static str) -> Result<ToolOutput> {
    ensure_size(&path).await?;
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("读取图片失败: {}", path.display()))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let data_url = format!("data:{mime_type};base64,{encoded}");
    let display = path.display().to_string();
    Ok(ToolOutput {
        content: format!(
            "已读取图片 {display}，图像内容已作为下一次模型请求的临时用户内容块提供。"
        ),
        transient_messages: vec![Message::user_with_images(
            format!("read_file 提供的本地图片：{display}。请结合当前任务分析图片内容。"),
            vec![data_url],
        )],
    })
}

async fn read_pdf(path: PathBuf) -> Result<String> {
    ensure_size(&path).await?;
    let display = path.display().to_string();
    tokio::task::spawn_blocking(move || extract_pdf_text(&path))
        .await
        .context("PDF 文本抽取任务异常终止")?
        .with_context(|| format!("解析 PDF 失败: {display}"))
}

fn extract_pdf_text(path: &Path) -> Result<String> {
    let document = lopdf::Document::load(path)?;
    let page_numbers = document.get_pages().keys().copied().collect::<Vec<u32>>();
    if page_numbers.len() > MAX_PDF_PAGES {
        bail!(
            "PDF 共 {} 页，超过 {} 页限制",
            page_numbers.len(),
            MAX_PDF_PAGES
        );
    }
    let text = document.extract_text(&page_numbers)?;
    Ok(truncate_chars(&text, MAX_PDF_TEXT_CHARS))
}

async fn ensure_size(path: &Path) -> Result<()> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("读取文件元数据失败: {}", path.display()))?;
    if metadata.len() > MAX_MEDIA_BYTES {
        bail!(
            "文件大小 {} 字节，超过 {} MiB 限制: {}",
            metadata.len(),
            MAX_MEDIA_BYTES / 1024 / 1024,
            path.display()
        );
    }
    Ok(())
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let mut output = text.chars().take(limit).collect::<String>();
    if text.chars().count() > limit {
        output.push_str("\n…[PDF 文本已截断]");
    }
    output
}

fn parse_bool(name: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("环境变量 {name} 必须是 true/false、1/0、yes/no 或 on/off"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::Approval;
    use lopdf::content::{Content, Operation};
    use lopdf::{Document, Object, Stream, dictionary};

    struct AllowApproval;

    #[async_trait]
    impl Approval for AllowApproval {
        async fn request(&self, _prompt: &str) -> Result<bool> {
            Ok(true)
        }
    }

    fn tool() -> ReadFileTool {
        let safety = Arc::new(
            SafetyPolicy::new(std::env::current_dir().unwrap(), Arc::new(AllowApproval)).unwrap(),
        );
        ReadFileTool::new(safety)
    }

    #[test]
    fn routes_supported_and_unsupported_media() {
        assert!(matches!(file_kind(Path::new("a.pdf")), FileKind::Pdf));
        assert!(matches!(
            file_kind(Path::new("a.png")),
            FileKind::Image("image/png")
        ));
        assert!(matches!(
            file_kind(Path::new("a.gif")),
            FileKind::UnsupportedMedia("图片")
        ));
    }

    #[tokio::test]
    async fn degrades_image_when_multimodal_is_disabled() {
        let output = tool()
            .execute(json!({"path": "not-created.png"}))
            .await
            .unwrap();
        assert!(output.contains("未启用多模态"));
    }

    #[tokio::test]
    async fn produces_transient_image_message_when_enabled() {
        let path = std::env::current_dir()
            .unwrap()
            .join(format!("multimodal-test-{}.png", std::process::id()));
        std::fs::write(&path, b"fake-png-for-message-shape").unwrap();

        let output = tool()
            .with_multimodal()
            .execute_rich(json!({"path": path}))
            .await
            .unwrap();

        assert_eq!(output.transient_messages.len(), 1);
        assert_eq!(output.transient_messages[0].image_urls.len(), 1);
        assert!(output.transient_messages[0].image_urls[0].starts_with("data:image/png;base64,"));
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn extracts_text_from_a_local_pdf() {
        let path = std::env::current_dir()
            .unwrap()
            .join(format!("pdf-test-{}.pdf", std::process::id()));
        create_test_pdf(&path, "Hello Agent PDF");

        let output = tool().execute(json!({"path": path})).await.unwrap();

        assert!(output.contains("Hello Agent PDF"));
        std::fs::remove_file(path).unwrap();
    }

    fn create_test_pdf(path: &Path, text: &str) {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let font_id = document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = document.add_object(dictionary! {
            "Font" => dictionary! {"F1" => font_id},
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![72.into(), 720.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id =
            document.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        document.save(path).unwrap();
    }
}
