use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::warn;

const MAX_SKILL_FILES: usize = 64;
const MAX_SKILL_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_MATCHES: usize = 3;

#[derive(Clone)]
pub struct SkillLibrary {
    workspace: PathBuf,
    directory: PathBuf,
    index: Arc<RwLock<Option<Vec<SkillIndex>>>>,
    max_matches: usize,
}

impl SkillLibrary {
    pub fn from_env(workspace: &Path) -> Self {
        let configured = env::var_os("SKILLS_DIR").map(PathBuf::from);
        let directory = match configured {
            Some(path) if path.is_absolute() => path,
            Some(path) => workspace.join(path),
            None => workspace.join(".my-agent/skills"),
        };
        let max_matches = env::var("SKILLS_MAX_MATCHES")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_MATCHES);
        Self {
            workspace: workspace.to_path_buf(),
            directory,
            index: Arc::new(RwLock::new(None)),
            max_matches,
        }
    }

    #[cfg(test)]
    fn for_test(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            directory: workspace.join(".my-agent/skills"),
            index: Arc::new(RwLock::new(None)),
            max_matches: DEFAULT_MAX_MATCHES,
        }
    }

    pub async fn context_for(&self, query: &str) -> Option<String> {
        let skills = match self.ensure_index().await {
            Ok(skills) => skills,
            Err(error) => {
                warn!(%error, "加载 skill 索引失败");
                return None;
            }
        };
        if skills.is_empty() {
            return None;
        }

        let mut output = String::from("可用技能索引（仅结构化元数据）：");
        for skill in &skills {
            output.push_str(&format!(
                "\n- {}@{}：{}",
                skill.manifest.name, skill.manifest.version, skill.manifest.description
            ));
        }
        let mut ranked = rank_skills(&skills, query);
        if ranked.is_empty() {
            return Some(output);
        }
        output.push_str("\n\n按当前请求命中的技能正文：");
        for (_, skill) in ranked.drain(..).take(self.max_matches) {
            match read_skill(&skill.path) {
                Ok(parsed) => output.push_str(&format!(
                    "\n\n### {}@{}\n{}",
                    parsed.manifest.name, parsed.manifest.version, parsed.body
                )),
                Err(error) => {
                    warn!(path = %skill.path.display(), %error, "命中后读取 skill 正文失败")
                }
            }
        }
        Some(output)
    }

    pub async fn list(&self) -> Result<Vec<SkillInfo>> {
        Ok(self
            .ensure_index()
            .await?
            .into_iter()
            .map(|skill| SkillInfo {
                name: skill.manifest.name,
                version: skill.manifest.version,
                description: skill.manifest.description,
            })
            .collect())
    }

    pub async fn install(&self, source: &Path, force: bool) -> Result<Vec<InstallOutcome>> {
        let sources = self.checked_sources(source)?;
        let mut outcomes = Vec::with_capacity(sources.len());
        for source in sources {
            let parsed = read_skill(&source)
                .with_context(|| format!("校验 skill 失败：{}", source.display()))?;
            outcomes.push(self.install_parsed(&source, parsed, force, None).await?);
            self.refresh().await?;
        }
        Ok(outcomes)
    }

    pub async fn update(&self, name: &str, source: &Path) -> Result<InstallOutcome> {
        validate_name(name)?;
        let source = self.checked_file(source)?;
        let parsed = read_skill(&source)
            .with_context(|| format!("校验 skill 失败：{}", source.display()))?;
        if parsed.manifest.name != name {
            bail!(
                "update 名称不匹配：期望 {name}，源文件声明 {}",
                parsed.manifest.name
            );
        }
        let outcome = self
            .install_parsed(&source, parsed, false, Some(name))
            .await?;
        self.refresh().await?;
        Ok(outcome)
    }

    pub async fn remove(&self, name: &str, confirmed: bool) -> Result<String> {
        validate_name(name)?;
        if !confirmed {
            bail!("删除 skill 需要显式 --confirm");
        }
        let skill = self
            .ensure_index()
            .await?
            .into_iter()
            .find(|skill| skill.manifest.name == name)
            .with_context(|| format!("未安装 skill：{name}"))?;
        ensure_inside(&skill.path, &self.directory)?;
        tokio::fs::remove_file(&skill.path)
            .await
            .with_context(|| format!("删除 skill 失败：{}", skill.path.display()))?;
        self.refresh().await?;
        Ok(format!("已删除 skill：{name}"))
    }

    pub async fn refresh(&self) -> Result<()> {
        let directory = self.directory.clone();
        let skills = tokio::task::spawn_blocking(move || load_index(&directory))
            .await
            .context("skill 索引任务异常终止")?;
        *self.index.write().await = Some(skills);
        Ok(())
    }

    async fn ensure_index(&self) -> Result<Vec<SkillIndex>> {
        if let Some(skills) = self.index.read().await.as_ref() {
            return Ok(skills.clone());
        }
        self.refresh().await?;
        Ok(self
            .index
            .read()
            .await
            .as_ref()
            .cloned()
            .unwrap_or_default())
    }

    fn checked_sources(&self, source: &Path) -> Result<Vec<PathBuf>> {
        let source = resolve_source(&self.workspace, source)?;
        if source.is_file() {
            return Ok(vec![self.checked_file(&source)?]);
        }
        if !source.is_dir() {
            bail!("skill 安装源不是文件或目录：{}", source.display());
        }
        let mut files = std::fs::read_dir(&source)
            .with_context(|| format!("读取 skill 目录失败：{}", source.display()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| is_markdown(path))
            .collect::<Vec<_>>();
        files.sort();
        files.truncate(MAX_SKILL_FILES);
        if files.is_empty() {
            bail!("skill 安装目录中没有 Markdown 文件：{}", source.display());
        }
        Ok(files)
    }

    fn checked_file(&self, source: &Path) -> Result<PathBuf> {
        let source = resolve_source(&self.workspace, source)?;
        if !source.is_file() || !is_markdown(&source) {
            bail!("skill 安装源必须是 Markdown 文件：{}", source.display());
        }
        Ok(source)
    }

    async fn install_parsed(
        &self,
        source: &Path,
        parsed: ParsedSkill,
        force: bool,
        update_name: Option<&str>,
    ) -> Result<InstallOutcome> {
        tokio::fs::create_dir_all(&self.directory)
            .await
            .with_context(|| format!("创建 skill 目录失败：{}", self.directory.display()))?;
        let current = self
            .ensure_index()
            .await?
            .into_iter()
            .find(|skill| skill.manifest.name == parsed.manifest.name);
        if update_name.is_some() && current.is_none() {
            bail!("无法更新未安装的 skill：{}", parsed.manifest.name);
        }
        if let Some(current) = &current {
            if parsed.manifest.version == current.manifest.version {
                return Ok(InstallOutcome::Skipped {
                    name: parsed.manifest.name,
                    version: parsed.manifest.version,
                    reason: "同版本已安装".to_owned(),
                });
            }
            if parsed.manifest.version < current.manifest.version && !force {
                return Ok(InstallOutcome::Skipped {
                    name: parsed.manifest.name,
                    version: parsed.manifest.version,
                    reason: format!(
                        "已安装更高版本 {}；降级需 --force",
                        current.manifest.version
                    ),
                });
            }
        }
        let target = self.directory.join(format!("{}.md", parsed.manifest.name));
        ensure_inside(&target, &self.directory)?;
        let bytes = tokio::fs::read(source)
            .await
            .with_context(|| format!("读取 skill 源失败：{}", source.display()))?;
        let temporary = self.directory.join(format!(
            ".{}.tmp-{}",
            parsed.manifest.name,
            std::process::id()
        ));
        tokio::fs::write(&temporary, bytes)
            .await
            .with_context(|| format!("写入临时 skill 失败：{}", temporary.display()))?;
        tokio::fs::rename(&temporary, &target)
            .await
            .with_context(|| format!("提交 skill 失败：{}", target.display()))?;
        if let Some(current) = current
            && current.path != target
        {
            ensure_inside(&current.path, &self.directory)?;
            if let Err(error) = tokio::fs::remove_file(&current.path).await {
                warn!(path = %current.path.display(), %error, "清理旧 skill 文件失败");
            }
        }
        Ok(InstallOutcome::Installed {
            name: parsed.manifest.name,
            version: parsed.manifest.version,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SkillInfo {
    pub name: String,
    pub version: Version,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InstallOutcome {
    Installed {
        name: String,
        version: Version,
    },
    Skipped {
        name: String,
        version: Version,
        reason: String,
    },
}

impl std::fmt::Display for InstallOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Installed { name, version } => write!(formatter, "已安装 {name}@{version}"),
            Self::Skipped {
                name,
                version,
                reason,
            } => write!(formatter, "已跳过 {name}@{version}：{reason}"),
        }
    }
}

#[derive(Clone)]
struct SkillIndex {
    manifest: SkillManifest,
    summary: String,
    path: PathBuf,
}

#[derive(Clone, Deserialize)]
struct SkillManifest {
    name: String,
    version: Version,
    description: String,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    scope: Vec<String>,
}

struct ParsedSkill {
    manifest: SkillManifest,
    body: String,
}

fn load_index(directory: &Path) -> Vec<SkillIndex> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_markdown(path))
        .collect::<Vec<_>>();
    paths.sort();
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .take(MAX_SKILL_FILES)
        .filter_map(|path| match read_skill(&path) {
            Ok(parsed) if seen.insert(parsed.manifest.name.clone()) => Some(SkillIndex {
                summary: fallback_summary(&parsed.body),
                manifest: parsed.manifest,
                path,
            }),
            Ok(parsed) => {
                warn!(name = %parsed.manifest.name, path = %path.display(), "跳过重名 skill");
                None
            }
            Err(error) => {
                warn!(path = %path.display(), %error, "跳过 frontmatter 无效的 skill");
                None
            }
        })
        .collect()
}

fn read_skill(path: &Path) -> Result<ParsedSkill> {
    let bytes =
        std::fs::read(path).with_context(|| format!("读取 skill 失败：{}", path.display()))?;
    if bytes.len() > MAX_SKILL_BYTES {
        bail!("skill 超过 {MAX_SKILL_BYTES} 字节限制");
    }
    let source = String::from_utf8(bytes).context("skill 必须是 UTF-8")?;
    let normalized = source.replace("\r\n", "\n");
    let Some(rest) = normalized.strip_prefix("---\n") else {
        bail!("skill 缺少 YAML frontmatter 起始分隔符");
    };
    let Some((frontmatter, body)) = rest.split_once("\n---\n") else {
        bail!("skill 缺少 YAML frontmatter 结束分隔符");
    };
    let manifest: SkillManifest =
        serde_yaml_ng::from_str(frontmatter).context("解析 skill frontmatter 失败")?;
    validate_manifest(&manifest)?;
    if body.trim().is_empty() {
        bail!("skill 正文不能为空");
    }
    Ok(ParsedSkill {
        manifest,
        body: body.trim().to_owned(),
    })
}

fn validate_manifest(manifest: &SkillManifest) -> Result<()> {
    validate_name(&manifest.name)?;
    if manifest.description.trim().is_empty() {
        bail!("skill description 不能为空");
    }
    if manifest
        .keywords
        .iter()
        .any(|keyword| keyword.trim().is_empty())
    {
        bail!("skill keywords 不能包含空值");
    }
    if manifest.scope.iter().any(|scope| scope.trim().is_empty()) {
        bail!("skill scope 不能包含空值");
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('.')
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("skill name 只能包含 ASCII 字母、数字、-、_，且不能以点开头");
    }
    Ok(())
}

fn rank_skills(skills: &[SkillIndex], query: &str) -> Vec<(u64, SkillIndex)> {
    let query_lower = query.to_lowercase();
    let query_terms = terms(query);
    let mut ranked = skills
        .iter()
        .filter_map(|skill| {
            let keyword_hits = skill
                .manifest
                .keywords
                .iter()
                .filter(|keyword| {
                    let keyword = keyword.to_lowercase();
                    query_lower.contains(&keyword) || query_terms.contains(&keyword)
                })
                .count() as u64;
            let metadata = format!(
                "{} {} {} {}",
                skill.manifest.name,
                skill.manifest.description,
                skill.manifest.scope.join(" "),
                skill.summary
            );
            let metadata_terms = terms(&metadata);
            let overlap = metadata_terms.intersection(&query_terms).count() as u64;
            let normalized_overlap = overlap.saturating_mul(100)
                / u64::try_from(metadata_terms.len().max(1)).unwrap_or(1);
            let body_overlap = if keyword_hits > 0 || overlap > 0 {
                read_skill(&skill.path)
                    .ok()
                    .map(|parsed| {
                        let body_terms = terms(&parsed.body);
                        (body_terms.intersection(&query_terms).count() as u64).saturating_mul(10)
                            / u64::try_from(body_terms.len().max(1)).unwrap_or(1)
                    })
                    .unwrap_or(0)
            } else {
                0
            };
            let score = keyword_hits
                .saturating_mul(10_000)
                .saturating_add(normalized_overlap)
                .saturating_add(body_overlap);
            (score > 0).then_some((score, skill.clone()))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.manifest.version.cmp(&left.1.manifest.version))
            .then_with(|| left.1.manifest.name.cmp(&right.1.manifest.name))
    });
    ranked
}

fn resolve_source(workspace: &Path, source: &Path) -> Result<PathBuf> {
    let candidate = if source.is_absolute() {
        source.to_path_buf()
    } else {
        workspace.join(source)
    };
    let resolved = std::fs::canonicalize(&candidate)
        .with_context(|| format!("无法解析 skill 源路径：{}", candidate.display()))?;
    let workspace = std::fs::canonicalize(workspace)
        .with_context(|| format!("无法解析工作区：{}", workspace.display()))?;
    if !resolved.starts_with(&workspace) {
        bail!("拒绝读取工作区外 skill 源：{}", resolved.display());
    }
    Ok(resolved)
}

fn ensure_inside(path: &Path, directory: &Path) -> Result<()> {
    let parent = path.parent().context("skill 目标没有父目录")?;
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("无法解析 skill 目标目录：{}", parent.display()))?;
    let directory = std::fs::canonicalize(directory)
        .with_context(|| format!("无法解析 skill 目录：{}", directory.display()))?;
    if parent != directory {
        bail!("skill 目标越过固定目录边界：{}", path.display());
    }
    Ok(())
}

fn is_markdown(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

fn fallback_summary(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .unwrap_or("（未提供摘要）")
        .chars()
        .take(240)
        .collect()
}

fn terms(text: &str) -> HashSet<String> {
    let mut output = HashSet::new();
    for token in text
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token| token.len() >= 2)
    {
        output.insert(token.to_ascii_lowercase());
    }
    let chinese = text
        .chars()
        .filter(|character| is_cjk(*character))
        .collect::<Vec<_>>();
    for pair in chinese.windows(2) {
        output.insert(pair.iter().collect());
    }
    output
}

fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    fn workspace() -> PathBuf {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("my-agent-skills-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn skill(name: &str, version: &str, description: &str, keywords: &str, body: &str) -> String {
        format!(
            "---\nname: {name}\nversion: {version}\ndescription: {description}\nkeywords: [{keywords}]\nscope: [rust]\n---\n# {name}\n{body}\n"
        )
    }

    #[tokio::test]
    async fn installs_high_version_skips_downgrade_and_forces_it() {
        let workspace = workspace();
        let high = workspace.join("high.md");
        let low = workspace.join("low.md");
        std::fs::write(&high, skill("demo", "2.0.0", "高版本", "rust", "正文")).unwrap();
        std::fs::write(&low, skill("demo", "1.0.0", "低版本", "rust", "正文")).unwrap();
        let library = SkillLibrary::for_test(&workspace);

        assert!(matches!(
            library.install(&high, false).await.unwrap()[0],
            InstallOutcome::Installed { .. }
        ));
        assert!(matches!(
            library.install(&low, false).await.unwrap()[0],
            InstallOutcome::Skipped { .. }
        ));
        assert!(matches!(
            library.install(&low, true).await.unwrap()[0],
            InstallOutcome::Installed { .. }
        ));
        assert_eq!(
            library.list().await.unwrap()[0].version,
            Version::new(1, 0, 0)
        );
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn skips_bad_frontmatter_and_ranks_exact_keyword_first() {
        let workspace = workspace();
        let directory = workspace.join(".my-agent/skills");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("bad.md"), "---\nname: [broken\n---\n正文").unwrap();
        std::fs::write(
            directory.join("exact.md"),
            skill("exact", "1.0.0", "一般描述", "数据库", "正文仅供精确关键词"),
        )
        .unwrap();
        std::fs::write(
            directory.join("fuzzy.md"),
            skill("fuzzy", "9.0.0", "数据库迁移辅助", "别的词", "正文"),
        )
        .unwrap();
        let library = SkillLibrary::for_test(&workspace);

        let list = library.list().await.unwrap();
        assert_eq!(list.len(), 2);
        let context = library.context_for("请检查数据库").await.unwrap();
        assert!(
            context.find("### exact@1.0.0").unwrap() < context.find("### fuzzy@9.0.0").unwrap()
        );
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn update_is_monotonic_and_remove_requires_confirmation() {
        let workspace = workspace();
        let first = workspace.join("first.md");
        let second = workspace.join("second.md");
        std::fs::write(&first, skill("demo", "1.0.0", "第一版", "rust", "正文")).unwrap();
        std::fs::write(&second, skill("demo", "1.1.0", "第二版", "rust", "正文")).unwrap();
        let library = SkillLibrary::for_test(&workspace);
        library.install(&first, false).await.unwrap();
        assert!(matches!(
            library.update("demo", &second).await.unwrap(),
            InstallOutcome::Installed { .. }
        ));
        assert!(library.remove("demo", false).await.is_err());
        assert!(
            library
                .remove("demo", true)
                .await
                .unwrap()
                .contains("已删除")
        );
        assert!(library.list().await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(workspace);
    }
}
